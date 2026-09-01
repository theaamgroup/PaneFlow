//! Right-docked git diff for the CLI cockpit.
//!
//! The trailing `layout-sidebar-right` button of a pane header toggles the side
//! dock ([`crate::app::diff_dock`]) on that pane's *workspace folder*. The
//! dock hosts three surfaces - the working-tree diff against `HEAD`, a shell,
//! and open files - and asks which one the first time a session opens it
//! (`diff_dock::surface_picker`). This module owns the CLI plumbing only - the
//! toggle, the per-session attachment, and the dock host; the panel itself is
//! rendered once by [`crate::app::diff_dock`].
//!
//! The dock is *detached per session* (workspace tab): it belongs to the tab it
//! was opened in, so switching tab - or workspace, which switches tab too -
//! parks it and brings up whatever the incoming session last had: nothing at
//! all, until that session opens the dock itself and answers the picker. A
//! sibling tab of the same folder is a different session and starts clean
//! (#184 Phase 4).
//!
//! Slots are keyed by [`crate::workspace::Tab::id`], which comes from one
//! process-global counter, so a tab id is unique across every workspace and a
//! tab dragged to another workspace keeps its dock. The key is never persisted:
//! session restore and undo-close both mint fresh tab ids, so a restored tab
//! starts with the dock closed, which is what the close-confirmation copy
//! already promises ("dock and Review sessions are not restored").

use std::collections::HashMap;

use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled,
    div, px,
};

use crate::PaneFlowApp;
use crate::app::diff_dock::{DIFF_DOCK_PANEL_MIN_WIDTH, DiffDockData, DiffDockTab};
use crate::workspace::Workspace;

/// Width the pane grid keeps beside the dock: one minimum pane plus the gutter
/// on each of its sides, so a dock wider than the panel gives ground instead of
/// pushing its own right edge past the clip.
const PANE_GRID_RESERVED_WIDTH: f32 =
    crate::layout::MIN_PANE_SIZE + 2. * crate::layout::PANE_GUTTER_PX;

/// How the dock fits inside `available` px of main panel: `(render, max)`.
///
/// The stored width is a *preference*, not a layout fact: opening a right rail
/// (Files, Sessions) or narrowing the window shrinks the panel under a dock
/// that was sized for the wide one. Clamping at render rather than writing the
/// preference back means the dock returns to its full width when the rail
/// closes or the window grows again. The ceiling travels with it so a resize
/// drag started under a rail cannot store a width the panel could not show.
///
/// Pure: `preferred` is taken by value and never written anywhere. The only
/// writer of `DiffDockState::width` is the resize drag
/// (`PaneFlowApp::drag_diff_dock_resize`), which records a user gesture.
fn diff_dock_fit(preferred: f32, available: f32) -> (f32, f32) {
    let max = (available - PANE_GRID_RESERVED_WIDTH - crate::layout::PANE_GUTTER_PX)
        .max(DIFF_DOCK_PANEL_MIN_WIDTH);
    (preferred.min(max), max)
}

/// The dock state one session owns, parked while another session is on screen.
///
/// Only what makes the dock *this session's* dock is carried: whether it is
/// open, which surface was picked, the tabs it accumulated and the last diff
/// snapshot (kept so returning repaints from warm rows instead of flashing a
/// loader while git re-runs). Width, split/unified layout and the fold state
/// stay app-global - they are preferences about how a diff reads, not facts
/// about which session is on screen.
pub(crate) struct DiffDockSlot {
    open: bool,
    picker: bool,
    picked: bool,
    tabs: Vec<DiffDockTab>,
    active_tab: usize,
    data: Option<DiffDockData>,
}

impl DiffDockSlot {
    /// Nothing worth keeping: the session never opened the dock (or was left at
    /// its birth state), so parking it would only grow the map with slots
    /// indistinguishable from a fresh one.
    fn is_idle(&self) -> bool {
        !self.open && !self.picked && self.tabs.len() <= 1
    }
}

/// Id of the session (workspace tab) the cockpit is showing: the active tab of
/// the active workspace. `None` with no workspace open.
fn session_id(workspaces: &[Workspace], active_idx: usize) -> Option<u64> {
    workspaces.get(active_idx).map(|ws| ws.active_tab().id)
}

/// Whether a tab with this id still exists in any workspace.
fn session_is_live(workspaces: &[Workspace], session_id: u64) -> bool {
    workspaces
        .iter()
        .flat_map(|ws| ws.tabs())
        .any(|tab| tab.id == session_id)
}

/// Park `slot` under `owner`, or drop it. `None` means the owner is gone (its
/// tab was just closed): dropping the slot here is that session's dock
/// teardown. An idle slot is not worth a map entry either way.
fn park_dock_slot(parked: &mut HashMap<u64, DiffDockSlot>, owner: Option<u64>, slot: DiffDockSlot) {
    match owner {
        None => drop(slot),
        Some(id) if slot.is_idle() => {
            parked.remove(&id);
        }
        Some(id) => {
            parked.insert(id, slot);
        }
    }
}

/// Drop the slots of sessions that no longer exist in `workspaces`.
fn prune_parked_dock_slots(parked: &mut HashMap<u64, DiffDockSlot>, workspaces: &[Workspace]) {
    parked.retain(|id, _| session_is_live(workspaces, *id));
}

/// The dock tabs a session owns, wherever they are: on screen when it is the
/// live `owner`, and in its parked slot otherwise. Both halves are consulted
/// because the live dock is only reconciled at render, so a session can hold
/// a slot *and* be the owner for the frame between a switch and the next paint.
fn dock_tabs_for_session<'a>(
    owner: Option<u64>,
    live_tabs: &'a [DiffDockTab],
    parked: &'a HashMap<u64, DiffDockSlot>,
    session_id: u64,
) -> Vec<&'a DiffDockTab> {
    let mut tabs = Vec::new();
    if owner == Some(session_id) {
        tabs.extend(live_tabs.iter());
    }
    if let Some(slot) = parked.get(&session_id) {
        tabs.extend(slot.tabs.iter());
    }
    tabs
}

/// Every dock tab owned by any tab of `workspace`.
fn dock_tabs_for_workspace<'a>(
    owner: Option<u64>,
    live_tabs: &'a [DiffDockTab],
    parked: &'a HashMap<u64, DiffDockSlot>,
    workspace: &Workspace,
) -> Vec<&'a DiffDockTab> {
    workspace
        .tabs()
        .iter()
        .flat_map(|tab| dock_tabs_for_session(owner, live_tabs, parked, tab.id))
        .collect()
}

/// The terminal surfaces among `tabs`, deduplicated by entity id.
fn dock_terminals<'a>(
    tabs: impl IntoIterator<Item = &'a DiffDockTab>,
) -> Vec<gpui::Entity<crate::terminal::TerminalView>> {
    let mut result: Vec<_> = tabs
        .into_iter()
        .filter_map(|tab| match tab {
            DiffDockTab::Terminal(terminal) => Some(terminal.clone()),
            _ => None,
        })
        .collect();
    result.sort_by_key(|terminal| terminal.entity_id());
    result.dedup();
    result
}

impl PaneFlowApp {
    /// Every dock terminal in the process: the live dock plus every parked
    /// slot. Feeds the worktree-teardown CWD gate, which must see every PTY.
    pub(crate) fn all_diff_dock_terminals(
        &self,
    ) -> Vec<gpui::Entity<crate::terminal::TerminalView>> {
        dock_terminals(
            self.diff_dock.diff_tabs.iter().chain(
                self.diff_dock
                    .parked
                    .values()
                    .flat_map(|slot| slot.tabs.iter()),
            ),
        )
    }

    /// The dock terminals that die when the tab `tab_id` closes: the live dock
    /// when that tab owns it, plus the tab's parked slot.
    pub(crate) fn diff_dock_terminals_for_tab(
        &self,
        tab_id: u64,
    ) -> Vec<gpui::Entity<crate::terminal::TerminalView>> {
        dock_terminals(dock_tabs_for_session(
            self.diff_dock.owner,
            &self.diff_dock.diff_tabs,
            &self.diff_dock.parked,
            tab_id,
        ))
    }

    /// The dock terminals that die when workspace `workspace_id` closes: those
    /// of every tab it holds. Empty for a workspace that is already gone.
    pub(crate) fn diff_dock_terminals_for_workspace(
        &self,
        workspace_id: u64,
    ) -> Vec<gpui::Entity<crate::terminal::TerminalView>> {
        let Some(workspace) = self.workspaces.iter().find(|ws| ws.id == workspace_id) else {
            return Vec::new();
        };
        dock_terminals(dock_tabs_for_workspace(
            self.diff_dock.owner,
            &self.diff_dock.diff_tabs,
            &self.diff_dock.parked,
            workspace,
        ))
    }

    /// Drop both mounted and parked dock state owned by a tab before that tab
    /// leaves the model. Render-time reconciliation is not sufficient for a
    /// background tab: the visible session does not change, so
    /// `sync_diff_dock_session` can legitimately return early and the slot
    /// (and the terminals in it) would outlive the session it belonged to.
    pub(crate) fn drop_diff_dock_for_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.diff_dock.parked.remove(&tab_id);
        if self.diff_dock.owner == Some(tab_id) {
            self.diff_dock.owner = None;
            self.park_live_diff_dock(None, cx);
        }
    }

    /// Workspace-close teardown: every tab of the workspace goes, so every
    /// tab's dock goes with it. Must run while the workspace is still in
    /// `self.workspaces`, or there are no tab ids left to resolve.
    pub(crate) fn drop_diff_dock_for_workspace(
        &mut self,
        workspace_id: u64,
        cx: &mut Context<Self>,
    ) {
        let tab_ids: Vec<u64> = self
            .workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .map(|ws| ws.tabs().iter().map(|tab| tab.id).collect())
            .unwrap_or_default();
        for tab_id in tab_ids {
            self.drop_diff_dock_for_tab(tab_id, cx);
        }
    }

    /// Live CWDs of terminal tabs in both the mounted and parked diff docks.
    /// These terminals are not represented by workspace panes.
    pub(crate) fn diff_dock_terminal_cwds(&self, cx: &App) -> Vec<std::path::PathBuf> {
        fn append(tabs: &[DiffDockTab], cx: &App, cwds: &mut Vec<std::path::PathBuf>) {
            for tab in tabs {
                let DiffDockTab::Terminal(terminal) = tab else {
                    continue;
                };
                let terminal = terminal.read(cx);
                if let Some(cwd) = terminal.terminal.cwd_now() {
                    cwds.push(cwd);
                }
                if let Some(cwd) = terminal.terminal.current_cwd.as_deref() {
                    cwds.push(std::path::PathBuf::from(cwd));
                }
            }
        }

        let mut cwds = Vec::new();
        append(&self.diff_dock.diff_tabs, cx, &mut cwds);
        for slot in self.diff_dock.parked.values() {
            append(&slot.tabs, cx, &mut cwds);
        }
        cwds
    }

    /// Keep the live dock attached to the session on screen.
    ///
    /// Called once per frame from [`Self::wrap_cli_diff_dock`] rather than from
    /// each of the places the visible tab moves (sidebar click, `Cmd+1..9`,
    /// tab switch / create / close / reorder, cross-workspace tab move,
    /// workspace create / close / restore, IPC `workspace.select`, Settings):
    /// the dock follows one fact - which session is visible - so it reconciles
    /// against that fact instead of asking every caller to remember it.
    fn sync_diff_dock_session(&mut self, cx: &mut Context<Self>) {
        let active = self.active_session_id();
        if self.diff_dock.owner == active {
            return;
        }
        let previous = self.diff_dock.owner;
        self.diff_dock.owner = active;
        self.park_live_diff_dock(previous, cx);
        self.prune_parked_diff_docks();
        self.restore_diff_dock(active, cx);
    }

    /// Id of the session (workspace tab) the cockpit is showing. Tab ids come
    /// from one process-global counter, so a tab id is unique across every
    /// workspace and needs no workspace half to key on.
    pub(crate) fn active_session_id(&self) -> Option<u64> {
        session_id(&self.workspaces, self.active_idx)
    }

    /// Drop the slots of sessions that no longer exist. The explicit teardowns
    /// ([`Self::drop_diff_dock_for_tab`], [`Self::drop_diff_dock_for_workspace`])
    /// cover the close paths; this catches a tab that vanished some other way,
    /// such as an empty placeholder tab replaced in place by
    /// [`Workspace::open_tab`].
    pub(crate) fn prune_parked_diff_docks(&mut self) {
        prune_parked_dock_slots(&mut self.diff_dock.parked, &self.workspaces);
    }

    /// Move the live dock fields into `owner`'s slot and reset them to the
    /// state a session that has never opened the dock sees.
    fn park_live_diff_dock(&mut self, owner: Option<u64>, cx: &mut Context<Self>) {
        let slot = DiffDockSlot {
            open: self.diff_dock.open,
            picker: self.diff_dock.picker,
            picked: self.diff_dock.picked,
            tabs: std::mem::replace(&mut self.diff_dock.diff_tabs, vec![DiffDockTab::Changes]),
            active_tab: std::mem::replace(&mut self.diff_dock.diff_active_tab, 0),
            data: self.diff_dock.data.take(),
        };
        // Everything the parked dock left behind: the closer already drops the
        // snapshot state (folds, scroll, horizontal offsets) and the live
        // drags, and the menus below describe a strip that is no longer here.
        self.close_diff_dock_panel(cx);
        self.diff_dock.picker = false;
        self.diff_dock.picked = false;
        self.diff_dock.diff_tab_close_armed = None;
        self.diff_dock.diff_options_menu_open = false;
        self.diff_dock.diff_layout_submenu_open = false;
        self.diff_dock.diff_new_tab_menu_open = false;
        self.diff_dock.diff_branch_menu = None;

        // The owner is gone (its tab was just closed): dropping the slot here
        // is that session's dock teardown.
        let owner = owner.filter(|id| session_is_live(&self.workspaces, *id));
        park_dock_slot(&mut self.diff_dock.parked, owner, slot);
    }

    /// Bring `session_id`'s parked dock back. A session with no slot keeps the
    /// closed dock the parking reset left, which is the whole point: opening a
    /// dock in one session must not open one in the next.
    fn restore_diff_dock(&mut self, session_id: Option<u64>, cx: &mut Context<Self>) {
        let Some(slot) = session_id.and_then(|id| self.diff_dock.parked.remove(&id)) else {
            return;
        };
        self.diff_dock.picker = slot.picker;
        self.diff_dock.picked = slot.picked;
        self.diff_dock.diff_tabs = slot.tabs;
        self.diff_dock.diff_active_tab = slot.active_tab;
        let cwd = slot
            .data
            .as_ref()
            .map(|data| data.cwd.clone())
            .filter(|cwd| !cwd.is_empty());
        self.diff_dock.data = slot.data;
        if slot.open {
            // Reopening on the snapshot's own folder, not the workspace root:
            // the two are the same today, and asking the data keeps the warm
            // snapshot valid if a dock is ever opened on a subfolder.
            let cwd = cwd
                .or_else(|| self.active_workspace().map(|ws| ws.cwd.clone()))
                .unwrap_or_default();
            self.open_diff_dock_panel(cwd, cx);
        }
    }

    /// Pane-header button handler: close the dock when it already shows this
    /// folder, otherwise (re)open it there.
    pub(crate) fn toggle_cli_diff_dock(&mut self, cwd: String, cx: &mut Context<Self>) {
        let cwd = cwd.trim().to_string();
        let showing = self.diff_dock.open
            && self
                .diff_dock
                .data
                .as_ref()
                .is_some_and(|data| data.cwd == cwd);
        if showing {
            self.close_diff_dock_panel(cx);
        } else {
            // The button opens the dock, not the diff: until this session has
            // said once what it wants in it, the dock comes up on its surface
            // picker. Afterwards it restores whatever tab was last active there.
            self.diff_dock.picker = !self.diff_dock.picked;
            self.open_diff_dock_panel(cwd, cx);
        }
    }

    /// Whether the dock is actually on screen.
    ///
    /// `open` alone is not enough: the flag survives a mode switch
    /// and a trip through Settings, both of which unmount the dock in
    /// [`Self::wrap_cli_diff_dock`]. Anything that acts *on* the dock without
    /// putting it back on screen has to ask this instead, or it mutates a strip
    /// nobody can see.
    pub(crate) fn diff_dock_visible(&self) -> bool {
        self.diff_dock.open
            && self.settings_section.is_none()
            && matches!(self.mode, paneflow_config::schema::AppMode::Cli)
    }

    /// Dock the diff panel to the right of the CLI pane grid when it is open.
    /// The resize / horizontal-scrollbar drags are captured on this wrapper (a
    /// full-height surface) so a drag keeps tracking once the cursor outruns its
    /// handle and crosses into the panes beside it.
    ///
    /// `available_width` is the main panel's live width between the rails; the
    /// dock renders at [`diff_dock_fit`] of it and its stored preference, which
    /// this never writes.
    pub(crate) fn wrap_cli_diff_dock(
        &mut self,
        body: AnyElement,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Before the visibility test, not after: a session whose parked dock is
        // open has to be swapped in to *become* visible.
        self.sync_diff_dock_session(cx);
        if !self.diff_dock_visible() {
            return body;
        }
        let ui = crate::theme::ui_colors();
        let (width, max_width) = diff_dock_fit(self.diff_dock.width, available_width);
        div()
            .size_full()
            .flex()
            .flex_row()
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _w, cx| {
                if this.diff_dock.h_scroll_drag.is_some() {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.drag_diff_dock_h_scrollbar(event.position.x, cx);
                    } else {
                        this.end_diff_dock_h_scrollbar_drag(cx);
                    }
                } else if this.diff_dock.resize.is_some() {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.drag_diff_dock_resize(f32::from(event.position.x), cx);
                    } else {
                        this.end_diff_dock_resize(cx);
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _e: &gpui::MouseUpEvent, _w, cx| {
                    this.end_diff_dock_h_scrollbar_drag(cx);
                    this.end_diff_dock_resize(cx);
                }),
            )
            .child(div().flex_1().min_w_0().h_full().child(body))
            // The pane grid already pads its own right edge, so the dock only
            // has to reproduce the other three gutters to sit on the same
            // margins as the cards it docks beside.
            .child(
                div()
                    .flex_none()
                    .h_full()
                    .flex()
                    .flex_col()
                    .pt(px(crate::layout::PANE_GUTTER_PX))
                    .pb(px(crate::layout::PANE_GUTTER_PX))
                    .pr(px(crate::layout::PANE_GUTTER_PX))
                    .child(self.render_diff_dock_panel(width, max_width, ui, cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::AppContext as _;

    use super::*;
    use crate::workspace::Tab;

    fn slot(open: bool, picked: bool, tabs: usize) -> DiffDockSlot {
        DiffDockSlot {
            open,
            picker: false,
            picked,
            tabs: (0..tabs).map(|_| DiffDockTab::Changes).collect(),
            active_tab: 0,
            data: None,
        }
    }

    /// A workspace on `cwd` holding `titles.len()` named tabs, in order, with
    /// the last one active (the way `open_tab` leaves it).
    fn workspace_with_tabs(id: u64, cwd: &str, titles: &[&str]) -> Workspace {
        let mut ws = Workspace::empty_with_cwd_and_id(id, "project", std::path::PathBuf::from(cwd));
        for title in titles {
            assert!(ws.open_tab(Tab::new(*title, None)));
        }
        ws
    }

    fn tab_ids(ws: &Workspace) -> Vec<u64> {
        ws.tabs().iter().map(|tab| tab.id).collect()
    }

    // --- width fit -------------------------------------------------------

    #[test]
    fn a_wide_panel_leaves_the_preferred_dock_width_alone() {
        // 1920px of panel: the dock has no reason to give ground, and the
        // preference is what the user dragged it to.
        assert_eq!(diff_dock_fit(880., 1920.).0, 880.);
    }

    #[test]
    fn opening_a_right_rail_shrinks_the_dock_instead_of_clipping_it() {
        // A 300px rail over a 1280px window leaves ~970px of panel. The dock
        // must fit inside it with the pane grid's reserve, not overflow the
        // panel's clip by the difference.
        let available = 970.;
        let (width, max) = diff_dock_fit(880., available);
        assert!(width < 880., "the dock must give ground: {width}");
        assert_eq!(width, max, "a clamped dock renders at its ceiling");
        assert!(
            width + PANE_GRID_RESERVED_WIDTH + crate::layout::PANE_GUTTER_PX <= available,
            "the dock still overflows the panel: {width}"
        );
    }

    #[test]
    fn a_panel_too_narrow_for_the_floor_stops_at_the_floor() {
        // Past this point something has to be clipped; the dock stays readable
        // rather than collapsing to a sliver.
        assert_eq!(
            diff_dock_fit(880., 200.),
            (DIFF_DOCK_PANEL_MIN_WIDTH, DIFF_DOCK_PANEL_MIN_WIDTH)
        );
    }

    /// #184 Phase 4: the rendered width is `min(stored, remainder)` with the
    /// floor honoured, and the stored preference is never the thing that
    /// moves - growing the panel back must restore the width the user chose.
    #[test]
    fn the_rendered_width_is_the_smaller_of_preference_and_remainder() {
        let stored = 880.;
        let remainder = 700.;
        let (narrow, _) = diff_dock_fit(stored, remainder);
        assert_eq!(
            narrow,
            remainder - PANE_GRID_RESERVED_WIDTH - crate::layout::PANE_GUTTER_PX,
            "under a narrow panel the dock renders at what the panel can spare"
        );
        assert!(narrow >= DIFF_DOCK_PANEL_MIN_WIDTH);
        // The same preference, once the panel is wide again: nothing was lost.
        assert_eq!(
            diff_dock_fit(stored, 1920.).0,
            stored,
            "the preference survives a narrow spell untouched"
        );
    }

    /// The render-time clamp must stay a read: the only writer of the stored
    /// width is the resize drag, which records a user gesture. A fit that
    /// wrote its result back would make a narrow spell permanent.
    #[test]
    fn the_render_fit_never_writes_the_preference_back() {
        let host = include_str!("cli_diff_dock.rs");
        let wrapper = host
            .split("pub(crate) fn wrap_cli_diff_dock(")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("dock host");
        assert!(
            !wrapper.contains("diff_dock.width ="),
            "the dock host must not store the clamped width: {wrapper}"
        );
        let fit = host
            .split("fn diff_dock_fit(")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("fit function");
        assert!(
            !fit.contains("self") && !fit.contains("&mut"),
            "diff_dock_fit must stay a pure function of its two arguments: {fit}"
        );

        let panel = include_str!("diff_dock/mod.rs");
        let writes = panel.matches("self.diff_dock.width =").count();
        assert_eq!(
            writes, 1,
            "expected exactly one writer of the dock width in the panel"
        );
        let drag = panel
            .split("pub(crate) fn drag_diff_dock_resize(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("resize drag");
        assert!(
            drag.contains("self.diff_dock.width ="),
            "the one writer must be the resize drag: {drag}"
        );
        assert!(
            !include_str!("diff_dock/render.rs").contains("diff_dock.width ="),
            "the resize handle only anchors; it must not write the width"
        );
    }

    // --- parking ---------------------------------------------------------

    #[test]
    fn a_session_that_never_opened_the_dock_parks_nothing() {
        // The birth state. Parking it would make "this session has a slot"
        // stop meaning "this session has a dock", and every tab merely visited
        // once would grow the map.
        assert!(slot(false, false, 1).is_idle());
        let mut parked = HashMap::new();
        park_dock_slot(&mut parked, Some(7), slot(false, false, 1));
        assert!(parked.is_empty(), "an idle slot must not take a map entry");
    }

    #[test]
    fn a_dock_worth_restoring_is_parked() {
        // Open, or answered, or carrying tabs: each on its own is state the
        // session must find again when it comes back.
        assert!(!slot(true, false, 1).is_idle(), "an open dock must survive");
        assert!(
            !slot(false, true, 1).is_idle(),
            "an answered picker must not ask again"
        );
        assert!(
            !slot(false, false, 2).is_idle(),
            "a terminal / file tab must not be dropped"
        );
    }

    /// #184 Phase 4 "done when": two tabs of the same folder no longer share
    /// a dock. The key is the tab id, so two sessions in one workspace on one
    /// cwd park under different keys, and switching the active tab changes
    /// which slot the reconcile asks for.
    #[test]
    fn two_tabs_of_the_same_folder_get_their_own_dock_slots() {
        let mut ws = workspace_with_tabs(1, "/tmp/project", &["first", "second"]);
        let ids = tab_ids(&ws);
        let (first, second) = (ids[0], ids[1]);
        assert_ne!(first, second, "sibling tabs must not share an id");

        // The reconcile keys on the visible tab, not the workspace.
        ws.set_active_tab(0);
        assert_eq!(session_id(std::slice::from_ref(&ws), 0), Some(first));
        ws.set_active_tab(1);
        assert_eq!(session_id(std::slice::from_ref(&ws), 0), Some(second));

        let mut parked = HashMap::new();
        park_dock_slot(&mut parked, Some(first), slot(true, true, 2));
        park_dock_slot(&mut parked, Some(second), slot(false, true, 3));
        assert_eq!(parked.len(), 2, "each session parks under its own key");

        // Switching to `first` takes exactly its dock back and leaves the
        // sibling's where it was.
        let restored = parked.remove(&first).expect("first tab's dock");
        assert!(restored.open);
        assert_eq!(restored.tabs.len(), 2);
        let sibling = parked.get(&second).expect("second tab's dock untouched");
        assert!(!sibling.open);
        assert_eq!(sibling.tabs.len(), 3);
    }

    /// A workspace id is not a session key: a parked slot keyed by the
    /// workspace would be handed to whichever tab of it came up next.
    #[test]
    fn a_workspace_id_never_resolves_a_dock_slot() {
        let ws = workspace_with_tabs(42, "/tmp/project", &["only"]);
        let tab = tab_ids(&ws)[0];
        assert_ne!(
            tab, 42,
            "the test needs a tab id that differs from the workspace id"
        );
        let mut parked = HashMap::new();
        park_dock_slot(&mut parked, Some(tab), slot(true, true, 1));
        assert!(!parked.contains_key(&42));
        assert!(parked.contains_key(&tab));
    }

    #[test]
    fn closing_a_tab_drops_only_that_tabs_parked_dock() {
        let mut ws = workspace_with_tabs(1, "/tmp/project", &["first", "second"]);
        let ids = tab_ids(&ws);
        let mut parked = HashMap::new();
        park_dock_slot(&mut parked, Some(ids[0]), slot(true, true, 1));
        park_dock_slot(&mut parked, Some(ids[1]), slot(true, true, 1));

        assert!(ws.close_tab(0).is_some());
        prune_parked_dock_slots(&mut parked, std::slice::from_ref(&ws));
        assert!(
            !parked.contains_key(&ids[0]),
            "the closed tab's dock must go with it"
        );
        assert!(
            parked.contains_key(&ids[1]),
            "the surviving tab keeps its dock"
        );

        // Parking under an id that is no longer live is a drop, not an insert.
        park_dock_slot(&mut parked, None, slot(true, true, 1));
        assert_eq!(parked.len(), 1);
    }

    /// A tab dragged to another workspace keeps its id, so its dock follows
    /// it instead of dying with the workspace it left.
    #[test]
    fn moving_a_tab_between_workspaces_keeps_its_parked_dock() {
        let mut source = workspace_with_tabs(1, "/tmp/a", &["moving", "staying"]);
        let mut dest = workspace_with_tabs(2, "/tmp/b", &["home"]);
        let moving = tab_ids(&source)[0];
        let mut parked = HashMap::new();
        park_dock_slot(&mut parked, Some(moving), slot(true, true, 2));

        let tab = source.close_tab(0).expect("detach the tab");
        assert_eq!(tab.id, moving);
        assert!(dest.open_tab(tab));
        prune_parked_dock_slots(&mut parked, &[source, dest]);
        assert!(
            parked.contains_key(&moving),
            "a moved tab is still live and keeps its dock"
        );
    }

    // --- close guard -----------------------------------------------------

    fn terminal_tab(cx: &mut gpui::VisualTestContext) -> DiffDockTab {
        DiffDockTab::Terminal(
            cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx)),
        )
    }

    fn terminal_slot(cx: &mut gpui::VisualTestContext) -> DiffDockSlot {
        DiffDockSlot {
            open: true,
            picker: false,
            picked: true,
            tabs: vec![DiffDockTab::Changes, terminal_tab(cx)],
            active_tab: 1,
            data: None,
        }
    }

    fn ids(terminals: &[gpui::Entity<crate::terminal::TerminalView>]) -> Vec<gpui::EntityId> {
        terminals.iter().map(|t| t.entity_id()).collect()
    }

    /// The re-keyed close guard: a workspace close still reaches every dock
    /// terminal of every tab in that workspace - the live dock and the parked
    /// ones - while a tab close reaches only that tab's, and a sibling
    /// workspace's docks are never counted.
    #[gpui::test]
    fn workspace_close_reaches_every_tabs_dock_and_tab_close_only_its_own(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let ws = workspace_with_tabs(1, "/tmp/project", &["live", "parked", "no-dock"]);
        let other = workspace_with_tabs(2, "/tmp/elsewhere", &["foreign"]);
        let ws_ids = tab_ids(&ws);
        let (live_tab, parked_tab, bare_tab) = (ws_ids[0], ws_ids[1], ws_ids[2]);
        let foreign_tab = tab_ids(&other)[0];

        // `live_tab` owns the mounted dock; `parked_tab` and `foreign_tab`
        // hold parked slots; `bare_tab` never opened one.
        let live_tabs = vec![DiffDockTab::Changes, terminal_tab(cx), terminal_tab(cx)];
        let mut parked = HashMap::new();
        parked.insert(parked_tab, terminal_slot(cx));
        parked.insert(foreign_tab, terminal_slot(cx));
        let owner = Some(live_tab);

        let live_terminals = dock_terminals(live_tabs.iter());
        assert_eq!(live_terminals.len(), 2);
        let parked_terminal = dock_terminals(parked[&parked_tab].tabs.iter());
        let foreign_terminal = dock_terminals(parked[&foreign_tab].tabs.iter());

        // Tab close: exactly that tab's terminals.
        let for_live = dock_terminals(dock_tabs_for_session(owner, &live_tabs, &parked, live_tab));
        assert_eq!(ids(&for_live), ids(&live_terminals));
        let for_parked = dock_terminals(dock_tabs_for_session(
            owner, &live_tabs, &parked, parked_tab,
        ));
        assert_eq!(ids(&for_parked), ids(&parked_terminal));
        assert!(
            dock_terminals(dock_tabs_for_session(owner, &live_tabs, &parked, bare_tab)).is_empty(),
            "a tab without a dock has nothing to lose"
        );

        // Workspace close: the union over its tabs, and nothing foreign.
        let for_ws = dock_terminals(dock_tabs_for_workspace(owner, &live_tabs, &parked, &ws));
        let mut expected = live_terminals.clone();
        expected.extend(parked_terminal.iter().cloned());
        expected.sort_by_key(|t| t.entity_id());
        assert_eq!(ids(&for_ws), ids(&expected));
        assert!(
            !ids(&for_ws).contains(&foreign_terminal[0].entity_id()),
            "another workspace's dock must not be on this workspace's kill list"
        );
        let for_other = dock_terminals(dock_tabs_for_workspace(owner, &live_tabs, &parked, &other));
        assert_eq!(ids(&for_other), ids(&foreign_terminal));
    }

    #[test]
    fn tab_close_has_an_explicit_parked_dock_teardown() {
        let src = include_str!("cli_diff_dock.rs");
        let helper = src
            .split("pub(crate) fn drop_diff_dock_for_tab(")
            .nth(1)
            .and_then(|rest| rest.split("/// Workspace-close teardown").next())
            .expect("tab dock teardown helper");
        assert!(
            helper.contains("parked.remove(&tab_id)"),
            "a background tab's parked terminal tabs must be dropped immediately: {helper}"
        );
        assert!(
            helper.contains("self.diff_dock.owner == Some(tab_id)")
                && helper.contains("park_live_diff_dock(None, cx)"),
            "the same helper must tear down a mounted owner without re-parking it: {helper}"
        );

        // And the tab closer calls it, so the teardown is not render-gated.
        let ops = include_str!("workspace_ops/tab.rs");
        let closer = ops
            .split("pub(crate) fn close_workspace_tab(")
            .nth(1)
            .and_then(|rest| rest.split("pub(crate) fn handle_close_tab(").next())
            .expect("tab closer");
        assert!(
            closer.contains("drop_diff_dock_for_tab("),
            "close_workspace_tab must tear the closed tab's dock down explicitly: {closer}"
        );
    }

    #[test]
    fn workspace_close_has_an_explicit_parked_dock_teardown() {
        let src = include_str!("cli_diff_dock.rs");
        let helper = src
            .split("pub(crate) fn drop_diff_dock_for_workspace(")
            .nth(1)
            .and_then(|rest| rest.split("/// Live CWDs").next())
            .expect("workspace dock teardown helper");
        assert!(
            helper.contains("ws.id == workspace_id") && helper.contains("ws.tabs()"),
            "the workspace teardown must resolve every tab of the closing workspace: {helper}"
        );
        assert!(
            helper.contains("self.drop_diff_dock_for_tab(tab_id, cx)"),
            "and hand each one to the per-tab teardown, so no tab's dock outlives the folder: {helper}"
        );
    }
}
