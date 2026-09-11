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
use std::path::Path;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled,
    canvas, div, px,
};

use crate::PaneFlowApp;
use crate::app::diff_dock::{DIFF_DOCK_PANEL_MIN_WIDTH, DiffDockData, DiffDockTab};
use crate::workspace::Workspace;

/// Width the pane grid keeps beside the dock: one minimum pane plus the gutter
/// on each of its sides, so a dock wider than the panel gives ground instead of
/// pushing its own right edge past the clip.
const PANE_GRID_RESERVED_WIDTH: f32 =
    crate::layout::MIN_PANE_SIZE + 2. * crate::layout::PANE_GUTTER_PX;

/// How the dock fits inside `available` px of main panel: `Some((render, max))`,
/// or `None` when the panel cannot hold the dock's floor width beside a
/// minimum pane. Below that the pane grid wins and the dock is not rendered at
/// all - its state stays where it is, so a rail closing or the window growing
/// brings it straight back. Rendering the floor into a panel that small would
/// shrink the `flex_1().min_w_0()` grid to nothing and clip the dock instead.
///
/// The stored width is a *preference*, not a layout fact: opening a right rail
/// (Files, Sessions) or narrowing the window shrinks the panel under a dock
/// that was sized for the wide one. Clamping at render rather than writing the
/// preference back means the dock returns to its full width when the rail
/// closes or the window grows again. The resize drag reads the same ceiling on
/// every move (`PaneFlowApp::drag_diff_dock_resize`), so a drag started under
/// a rail cannot store a width the panel could not show.
///
/// Pure: `preferred` is taken by value and never written anywhere. The only
/// writer of `DiffDockState::width` is the resize drag, which records a user
/// gesture - and only a value the user actually chose
/// (`diff_dock::diff_dock_drag_preference`).
fn diff_dock_fit(preferred: f32, available: f32) -> Option<(f32, f32)> {
    let max = available - PANE_GRID_RESERVED_WIDTH - crate::layout::PANE_GUTTER_PX;
    (max >= DIFF_DOCK_PANEL_MIN_WIDTH).then_some((preferred.min(max), max))
}

/// Width of a maximized dock (upstream e0ff7e21): the whole panel minus the
/// gutter on each side, never below the floor. The pane grid is hidden, so
/// the fit's reservation for it does not apply and the dock renders even in
/// a panel the fit would refuse.
fn diff_dock_maximized_width(available: f32) -> f32 {
    (available - 2. * crate::layout::PANE_GUTTER_PX).max(DIFF_DOCK_PANEL_MIN_WIDTH)
}

/// How the pane grid sits beside the dock this frame: flexing to the
/// remainder, clipped to `visible` of its `full` width while the maximize
/// slide runs, or hidden behind a maximized dock. Clipped and hidden keep the
/// grid mounted at constant bounds, so no pane sees a PTY resize.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PaneGridLayout {
    Flex,
    Clipped { visible: f32, full: f32 },
    Hidden,
}

impl PaneGridLayout {
    /// The left gutter the dock has to paint itself: the grid pads its own
    /// right edge while it is on screen, and the dock takes that over as the
    /// grid is clipped away.
    fn dock_left_gutter(self) -> f32 {
        let gutter = crate::layout::PANE_GUTTER_PX;
        match self {
            Self::Flex => 0.,
            Self::Clipped { visible, full } => gutter * (1. - visible / full.max(1.)).clamp(0., 1.),
            Self::Hidden => gutter,
        }
    }
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

#[cfg(test)]
impl DiffDockSlot {
    /// Test-only: a parked slot holding `tabs`, so a guard that has to see a
    /// parked dock can be exercised without a live `PaneFlowApp`.
    pub(crate) fn parked_with_tabs(tabs: Vec<DiffDockTab>) -> Self {
        Self {
            open: true,
            picker: false,
            picked: true,
            tabs,
            active_tab: 0,
            data: None,
        }
    }
}

impl DiffDockSlot {
    /// Preserve the original folder while forcing a fresh snapshot when this
    /// parked dock is next restored after a background file write.
    fn invalidate_for_cwd(&mut self, cwd: &str) {
        if let Some(data) = self.data.as_mut().filter(|data| data.cwd == cwd) {
            data.loading = true;
        }
    }

    /// Nothing worth keeping: the session never opened the dock (or was left at
    /// its birth state), so parking it would only grow the map with slots
    /// indistinguishable from a fresh one.
    fn is_idle(&self) -> bool {
        !self.open && !self.picked && self.tabs.is_empty()
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

/// Forget a parked slot's snapshot when it was taken at `from` (issue #347):
/// the session's tab now stands on another checkout, and
/// [`PaneFlowApp::restore_diff_dock`] reopens a snapshot-less slot on the
/// active tab's checkout, which by then is the new one. A snapshot of some
/// other folder is left alone - the dock was opened there on purpose.
fn retarget_parked_dock_slot(slot: &mut DiffDockSlot, from: &str) -> bool {
    if slot.data.as_ref().is_some_and(|data| data.cwd == from) {
        slot.data = None;
        return true;
    }
    false
}

/// Drop the slots of sessions that no longer exist in `workspaces`.
fn prune_parked_dock_slots(parked: &mut HashMap<u64, DiffDockSlot>, workspaces: &[Workspace]) {
    parked.retain(|id, _| session_is_live(workspaces, *id));
}

/// Re-key the dock session `from` owns onto tab `to`: its parked slot moves
/// under the new id and, when it is the live owner, the owner is renamed. For
/// a tab whose *content* moved into a freshly minted tab (a lone pane dragged
/// to another folder row closes its tab and reopens as a new one), so the dock
/// follows the content instead of dying with an id nothing resolves any more.
/// `to` is a fresh tab and holds no slot of its own.
fn rehome_dock_session(
    owner: &mut Option<u64>,
    parked: &mut HashMap<u64, DiffDockSlot>,
    from: u64,
    to: u64,
) {
    if from == to {
        return;
    }
    if let Some(slot) = parked.remove(&from) {
        parked.insert(to, slot);
    }
    if *owner == Some(from) {
        *owner = Some(to);
    }
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

/// Every dock tab in the process: the live dock's tabs followed by every
/// parked slot's. A free function over the two collections rather than a
/// `PaneFlowApp` method so the chain itself stays testable (a live
/// `PaneFlowApp` cannot be constructed in a test).
pub(crate) fn all_dock_tabs<'a>(
    live_tabs: &'a [DiffDockTab],
    parked: &'a HashMap<u64, DiffDockSlot>,
) -> impl Iterator<Item = &'a DiffDockTab> {
    live_tabs
        .iter()
        .chain(parked.values().flat_map(|slot| slot.tabs.iter()))
}

/// Whether a `DiffDockTab::File` among `tabs` open on `path` holds unsaved
/// edits (issue #468: the hunk-revert guard must refuse to rewrite a file some
/// dock editor is still holding edits for, parked docks included).
pub(crate) fn file_tab_dirty_at_path<'a>(
    tabs: impl IntoIterator<Item = &'a DiffDockTab>,
    path: &Path,
    cx: &App,
) -> bool {
    tabs.into_iter().any(|tab| match tab {
        DiffDockTab::File(view) => {
            let view = view.read(cx);
            view.path() == path && view.is_dirty()
        }
        _ => false,
    })
}

/// Whether any `DiffDockTab::File` among `tabs` holds unsaved edits
/// (issue #396: quit must see this before it discards the buffer for good).
///
/// A free function rather than a `PaneFlowApp` method so it is directly
/// testable against a real, dirtied `CodeView` - a live `PaneFlowApp` cannot
/// be constructed in a test (its constructor binds a Unix socket and spawns
/// PTYs), the same reason [`dock_terminals`] and its siblings above take
/// their tabs by reference instead of `&self`.
pub(crate) fn any_file_tab_dirty<'a>(
    tabs: impl IntoIterator<Item = &'a DiffDockTab>,
    cx: &App,
) -> bool {
    tabs.into_iter()
        .any(|tab| matches!(tab, DiffDockTab::File(view) if view.read(cx).is_dirty()))
}

impl PaneFlowApp {
    pub(crate) fn invalidate_parked_diff_docks_for_cwd(&mut self, cwd: &str) {
        for slot in self.diff_dock.parked.values_mut() {
            slot.invalidate_for_cwd(cwd);
        }
    }

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

    /// Whether any dock file tab - the live dock plus every parked slot,
    /// across every workspace - holds unsaved edits (issue #396). Unlike a
    /// dropped dock terminal, a `CodeView` buffer that never reaches disk is
    /// gone for good, so quit must see this before it tears the window down.
    pub(crate) fn any_dock_file_dirty(&self, cx: &App) -> bool {
        any_file_tab_dirty(
            self.diff_dock.diff_tabs.iter().chain(
                self.diff_dock
                    .parked
                    .values()
                    .flat_map(|slot| slot.tabs.iter()),
            ),
            cx,
        )
    }

    /// Whether a dock file tab open on `path` - live dock or parked slot -
    /// holds unsaved edits (issue #468). The hunk revert rewrites that file on
    /// disk, so an editor holding edits for it anywhere in the process must
    /// refuse the revert up front rather than let the later save collide.
    pub(crate) fn dock_file_dirty_at_path(&self, path: &Path, cx: &App) -> bool {
        file_tab_dirty_at_path(
            all_dock_tabs(&self.diff_dock.diff_tabs, &self.diff_dock.parked),
            path,
            cx,
        )
    }

    /// Push a refreshed config to every dock terminal, live or parked (#422
    /// review): `Workspace::propagate_config` walks the layout tree, which
    /// never holds a dock terminal, so a hot-reloaded render setting
    /// (`terminal.minimum_contrast`, `integrated_glyphs`, `color_emoji`,
    /// `cursor_color`, the font block) would otherwise stop at the pane grid.
    pub(crate) fn propagate_config_to_dock_terminals(&self, cx: &mut App) {
        for terminal in dock_terminals(all_dock_tabs(
            &self.diff_dock.diff_tabs,
            &self.diff_dock.parked,
        )) {
            terminal.update(cx, |view, cx| {
                view.apply_render_config(&self.cached_config, cx)
            });
        }
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

    /// Whether the dock owned by tab `tab_id` - the live dock when that tab
    /// owns it, plus the tab's parked slot - holds a dirty `DiffDockTab::File`
    /// (issue #397). `close_workspace_tab` must see this before it calls
    /// [`Self::drop_diff_dock_for_tab`], the same way #396 made
    /// `quit_after_session_save` see [`Self::any_dock_file_dirty`] before
    /// `cx.quit()`: `session.json` never journals a `CodeView`'s in-memory
    /// edits, so dropping the slot here would discard them for good.
    pub(crate) fn dock_file_dirty_for_tab(&self, tab_id: u64, cx: &App) -> bool {
        any_file_tab_dirty(
            dock_tabs_for_session(
                self.diff_dock.owner,
                &self.diff_dock.diff_tabs,
                &self.diff_dock.parked,
                tab_id,
            ),
            cx,
        )
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

    /// The dock of tab `from` now belongs to tab `to`. For the one gesture
    /// that moves a tab's content into a new tab rather than moving the tab:
    /// a lone pane dragged to another folder row (`move_pane_to_new_tab`)
    /// closes the tab it *was* and reopens the pane as a fresh tab. The dock
    /// followed that pane's tab, so it follows the pane - the alternative is
    /// the next reconcile pruning a slot whose id nothing resolves, and the
    /// dock's terminals (an agent, as likely as not) dying with it, silently
    /// and un-undoably. Must run before the next paint.
    pub(crate) fn rehome_diff_dock_for_tab(&mut self, from: u64, to: u64) {
        rehome_dock_session(
            &mut self.diff_dock.owner,
            &mut self.diff_dock.parked,
            from,
            to,
        );
    }

    /// Point tab `tab_id`'s dock at `to` when it is showing `from` (issue
    /// #347): the tab's checkout changed under it. The live dock recomputes
    /// in place; a parked slot forgets its snapshot so it reopens on the new
    /// checkout when the tab comes back. A dock showing any other folder is
    /// untouched.
    pub(crate) fn retarget_diff_dock_for_tab(
        &mut self,
        tab_id: u64,
        from: &str,
        to: &str,
        cx: &mut Context<Self>,
    ) {
        if self.diff_dock.owner == Some(tab_id) {
            if self.diff_dock.open
                && self
                    .diff_dock
                    .data
                    .as_ref()
                    .is_some_and(|data| data.cwd == from)
            {
                self.refresh_diff_dock(to.to_string(), cx);
            }
        } else if let Some(slot) = self.diff_dock.parked.get_mut(&tab_id)
            && retarget_parked_dock_slot(slot, from)
        {
            cx.notify();
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
    pub(crate) fn sync_diff_dock_session(&mut self, cx: &mut Context<Self>) {
        // Above the early return, so a slot whose tab vanished is caught on
        // the next paint whether or not the visible session moved. Free in the
        // common case (nothing parked); otherwise a handful of id compares.
        if !self.diff_dock.parked.is_empty() {
            self.prune_parked_diff_docks();
        }
        let active = self.active_session_id();
        if self.diff_dock.owner == active {
            return;
        }
        let previous = self.diff_dock.owner;
        self.diff_dock.owner = active;
        self.park_live_diff_dock(previous, cx);
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
    /// cover the close paths and [`Self::rehome_diff_dock_for_tab`] the
    /// pane-drag one; this catches a tab that vanished some other way, such
    /// as an empty placeholder tab replaced in place by [`Workspace::open_tab`].
    /// Runs at the top of every [`Self::sync_diff_dock_session`] - once per
    /// frame, before its early return - so it does not wait for the visible
    /// session to change.
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
            tabs: std::mem::take(&mut self.diff_dock.diff_tabs),
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
            let cwd = cwd.or_else(|| self.active_checkout()).unwrap_or_default();
            self.open_diff_dock_panel(cwd, cx);
            // A session switch brings a parked dock straight back: the slide
            // is for a dock the user just opened.
            self.diff_dock.reveal_animation = None;
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

    pub(crate) fn handle_toggle_diff_dock_maximize(
        &mut self,
        _: &crate::ToggleDiffDockMaximize,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if !self.diff_dock_visible() {
            return;
        }
        self.toggle_diff_dock_maximize(window, cx);
    }

    /// Maximize the dock over the whole cockpit, or restore the pane grid
    /// (upstream e0ff7e21). Maximizing records the focus that was active and
    /// moves it onto the active dock tab; restoring hands it back (or the
    /// workspace's first pane when nothing was focused). The grid slides
    /// between its measured width and 0 like the primary sidebar, unless
    /// `reduce_motion` is set.
    pub(crate) fn toggle_diff_dock_maximize(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.diff_dock.resize = None;
        let now = std::time::Instant::now();
        let full = self.diff_dock.pane_grid_width.get();
        let from = match self.diff_dock.maximize_animation {
            Some(animation) => animation.width_at(now),
            None if self.diff_dock.maximized.is_some() => 0.,
            None => full,
        };
        let to = match self.diff_dock.maximized.take() {
            Some(previous_focus) => {
                self.restore_pre_maximize_focus(previous_focus, window, cx);
                full
            }
            None => {
                self.diff_dock.maximized = Some(window.focused(cx));
                // The hidden grid must not keep the keyboard: a tab with its
                // own handle (File, Terminal) takes it, and Changes, which has
                // none, blurs the pane instead. Never the `focus_diff_tab`
                // pane fallback, which would focus exactly the pane that is
                // about to disappear.
                match self.dock_tab_focus_handle(self.diff_dock.diff_active_tab, cx) {
                    Some(focus) => window.focus(&focus, cx),
                    None => window.blur(),
                }
                0.
            }
        };
        self.diff_dock.maximize_animation = if !crate::ui_primitives::reduce_motion()
            && (from - to).abs() > crate::PRIMARY_SIDEBAR_MIN_ANIMATION_DELTA
        {
            Some(crate::SidebarWidthAnimation {
                from_width: from,
                to_width: to,
                started_at: now,
            })
        } else {
            None
        };
        cx.notify();
    }

    /// Hand the keyboard back to where it was before the dock was maximized:
    /// the recorded handle, or the workspace's first pane when nothing was
    /// focused. Shared by the restore toggle and the strip's close button, so
    /// closing a maximized dock never strands the focus on a hidden surface.
    pub(crate) fn restore_pre_maximize_focus(
        &mut self,
        previous_focus: Option<gpui::FocusHandle>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        match previous_focus {
            Some(focus) => window.focus(&focus, cx),
            None => {
                // `false` is a zero-pane workspace: nothing to hand the
                // focus back to.
                if let Some(ws) = self.workspaces.get(self.active_idx) {
                    let _ = ws.focus_first(window, cx);
                }
            }
        }
    }

    /// Whether the dock flexes to its container this frame: maximized, or
    /// sliding either way.
    pub(crate) fn diff_dock_fills_panel(&self) -> bool {
        self.diff_dock.maximized.is_some() || self.diff_dock.maximize_animation.is_some()
    }

    /// Whether the pane grid is behind a maximized dock this frame (#422 gate):
    /// a pane that is not painted is not seen, so its agent's completion and
    /// notifications must go out like a zoomed-away split's.
    pub(crate) fn pane_grid_hidden_by_dock(&self) -> bool {
        self.diff_dock.maximized.is_some() && self.diff_dock_visible()
    }

    /// The open slide's progress (0 to 1) this frame, or `None` once settled.
    fn rendered_dock_reveal(&mut self, window: &mut gpui::Window) -> Option<f32> {
        let animation = self.diff_dock.reveal_animation?;
        let now = std::time::Instant::now();
        if animation.is_finished(now) {
            self.diff_dock.reveal_animation = None;
            return None;
        }
        window.request_animation_frame();
        Some(animation.width_at(now))
    }

    /// How the pane grid renders this frame, settling a finished maximize
    /// slide and requesting the next frame of a running one.
    fn rendered_pane_grid_layout(&mut self, window: &mut gpui::Window) -> PaneGridLayout {
        let now = std::time::Instant::now();
        if let Some(animation) = self.diff_dock.maximize_animation {
            if animation.is_finished(now) {
                self.diff_dock.maximize_animation = None;
            } else {
                window.request_animation_frame();
                // `full` is the last Flex measurement, never the slide's
                // endpoints: a toggle during a restore starts from the
                // partly visible width, and laying the grid out at that
                // width would resize every pane behind the dock.
                return PaneGridLayout::Clipped {
                    visible: animation.width_at(now),
                    full: self.diff_dock.pane_grid_width.get().max(1.),
                };
            }
        }
        if self.diff_dock.maximized.is_some() {
            PaneGridLayout::Hidden
        } else {
            PaneGridLayout::Flex
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

    /// Whether a dock terminal is the one under the user's eye (#422): the
    /// window is active, the mounted dock (the active tab's) was painted by
    /// the last frame (`rendered`, which also covers a main panel too narrow
    /// to hold the dock's floor), and `terminal` is its active dock tab. A
    /// dock parked under another tab, a dock squeezed out by the pane grid,
    /// or a dock tab behind the active one is not seen, so its program
    /// notification goes out - the same line
    /// [`Self::surfaces_under_user_eye`] draws for hosted panes.
    pub(crate) fn dock_terminal_is_seen(
        &self,
        terminal: &gpui::Entity<crate::terminal::TerminalView>,
    ) -> bool {
        crate::agents::notifications::window_active()
            && self.diff_dock_visible()
            && self.diff_dock.rendered
            && matches!(
                self.diff_dock.diff_tabs.get(self.diff_dock.diff_active_tab),
                Some(DiffDockTab::Terminal(active)) if active == terminal
            )
    }

    /// Dock the diff panel to the right of the CLI pane grid when it is open.
    /// The resize / horizontal-scrollbar drags are captured on this wrapper (a
    /// full-height surface) so a drag keeps tracking once the cursor outruns its
    /// handle and crosses into the panes beside it.
    ///
    /// `available_width` is the main panel's live width between the rails; the
    /// dock renders at [`diff_dock_fit`] of it and its stored preference, which
    /// this never writes - and not at all when the panel cannot hold the
    /// dock's floor beside a minimum pane. A maximized dock bypasses the fit:
    /// the grid is clipped away (mounted at constant bounds, so no PTY
    /// resize) and the dock flexes to the panel.
    pub(crate) fn wrap_cli_diff_dock(
        &mut self,
        body: AnyElement,
        available_width: f32,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Before the visibility test, not after: a session whose parked dock is
        // open has to be swapped in to *become* visible.
        self.sync_diff_dock_session(cx);
        if !self.diff_dock_visible() {
            // The gutter is unmounted (Settings, Review) and cannot see the
            // button release; a thumb drag held across the switch must not
            // resume from its old anchor when the dock comes back.
            self.diff_dock.vertical_scrollbar.cancel_drag();
            self.diff_dock.rendered = false;
            self.blur_unmounted_files_tree(window, cx);
            return body;
        }
        let grid = self.rendered_pane_grid_layout(window);
        let fills_panel = grid != PaneGridLayout::Flex;
        let (width, max_width) = if fills_panel {
            let width = diff_dock_maximized_width(available_width);
            (width, width)
        } else {
            let Some(fit) = diff_dock_fit(self.diff_dock.width, available_width) else {
                // Below the floor the pane grid wins. The dock keeps its state
                // (open, tabs, snapshot) and is back the moment there is room
                // for both; only a drag anchored on the edge that just left
                // the screen is dropped, or it would resume on the dock's
                // return.
                self.diff_dock.resize = None;
                self.diff_dock.h_scroll_drag = None;
                self.diff_dock.vertical_scrollbar.cancel_drag();
                self.diff_dock.rendered = false;
                self.blur_unmounted_files_tree(window, cx);
                return body;
            };
            fit
        };
        self.diff_dock.rendered = true;
        let ui = crate::theme::ui_colors();
        let measured_grid_width = self.diff_dock.pane_grid_width.clone();
        let reveal = if fills_panel {
            None
        } else {
            self.rendered_dock_reveal(window)
        };
        let dock_column_width = width + crate::layout::PANE_GUTTER_PX;
        div()
            .size_full()
            .flex()
            .flex_row()
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _w, cx| {
                    if this.diff_dock.h_scroll_drag.is_some() {
                        if event.pressed_button == Some(MouseButton::Left) {
                            this.drag_diff_dock_h_scrollbar(event.position.x, cx);
                        } else {
                            this.end_diff_dock_h_scrollbar_drag(cx);
                        }
                    } else if this.diff_dock.resize.is_some() {
                        if event.pressed_button == Some(MouseButton::Left) {
                            // This frame's ceiling, not the one at mouse-down: the
                            // panel can change under a drag (a rail toggled by
                            // its chord), and the drag must not store past it.
                            this.drag_diff_dock_resize(f32::from(event.position.x), max_width, cx);
                        } else {
                            this.end_diff_dock_resize(cx);
                        }
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _e: &gpui::MouseUpEvent, _w, cx| {
                    this.end_diff_dock_h_scrollbar_drag(cx);
                    this.end_diff_dock_resize(cx);
                }),
            )
            .map(|row| match grid {
                // The canvas measures the grid's width for the maximize
                // slide's `full` extent.
                PaneGridLayout::Flex => row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .relative()
                        .child(
                            canvas(
                                move |bounds, _, _| {
                                    measured_grid_width.set(f32::from(bounds.size.width))
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(body),
                ),
                // Clipped, not shrunk: the grid keeps its full width inside
                // the clip so no pane is laid out narrower.
                PaneGridLayout::Clipped { visible, full } => row.child(
                    div()
                        .flex_none()
                        .w(px(visible))
                        .h_full()
                        .overflow_hidden()
                        .child(div().w(px(full)).h_full().child(body)),
                ),
                PaneGridLayout::Hidden => row,
            })
            // The pane grid already pads its own right edge while it is on
            // screen, so the dock only has to reproduce the other three
            // gutters to sit on the same margins as the cards it docks beside
            // - and the left one as the grid is clipped away.
            .child({
                let dock = div()
                    .map(|dock| {
                        if fills_panel {
                            dock.flex_1().min_w_0()
                        } else {
                            dock.flex_none()
                        }
                    })
                    .pl(px(grid.dock_left_gutter()))
                    .h_full()
                    .flex()
                    .flex_col()
                    .pt(px(crate::layout::PANE_GUTTER_PX))
                    .pb(px(crate::layout::PANE_GUTTER_PX))
                    .pr(px(crate::layout::PANE_GUTTER_PX))
                    .child(self.render_diff_dock_panel(width, ui, window, cx));
                match reveal {
                    // Sliding in from the right edge: the column grows while
                    // the dock stays pinned at its full width inside it.
                    Some(progress) => div()
                        .relative()
                        .flex_none()
                        .h_full()
                        .w(px(dock_column_width * progress))
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .right_0()
                                .w(px(dock_column_width))
                                .child(dock),
                        )
                        .into_any_element(),
                    None => dock.into_any_element(),
                }
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::AppContext as _;

    use super::*;
    use crate::app::diff_dock::{DIFF_DOCK_PANEL_MAX_WIDTH, diff_dock_drag_preference};
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

    #[test]
    fn a_background_revert_invalidates_only_matching_parked_snapshots() {
        let mut parked = slot(true, true, 1);
        let mut data = DiffDockData::loading("/repo/subfolder".into());
        data.loading = false;
        parked.data = Some(data);
        parked.invalidate_for_cwd("/other");
        assert!(!parked.data.as_ref().unwrap().loading);
        parked.invalidate_for_cwd("/repo/subfolder");
        let data = parked.data.as_ref().unwrap();
        assert!(
            data.loading,
            "restoring the dock must rebuild this snapshot"
        );
        assert_eq!(data.cwd, "/repo/subfolder");
        assert_eq!(parked.tabs.len(), 1);
        assert!(parked.open);
    }

    #[test]
    fn a_parked_dock_forgets_a_snapshot_of_the_checkout_its_tab_left() {
        // Issue #347 review, finding 11: binding a tab never re-targeted its
        // dock, so the slot came back showing the checkout the tab had left.
        let mut parked = slot(true, true, 1);
        parked.data = Some(DiffDockData::loading("/repo".to_string()));
        assert!(
            !retarget_parked_dock_slot(&mut parked, "/other"),
            "a dock opened on some other folder was opened there on purpose"
        );
        assert!(parked.data.is_some());
        assert!(retarget_parked_dock_slot(&mut parked, "/repo"));
        assert!(
            parked.data.is_none(),
            "a snapshot-less slot reopens on the active tab's checkout, which is the new one"
        );
        assert!(
            parked.open,
            "forgetting the snapshot does not close the dock"
        );
        assert!(!retarget_parked_dock_slot(&mut parked, "/repo"));
    }

    #[test]
    fn retargeting_refreshes_the_live_dock_and_forgets_the_parked_one() {
        let src = include_str!("cli_diff_dock.rs");
        let body = crate::source_probe::source_slice(
            src,
            "pub(crate) fn retarget_diff_dock_for_tab(",
            "/// Live CWDs of terminal tabs in both the mounted and parked diff docks.",
        );
        assert!(
            body.contains("self.diff_dock.owner == Some(tab_id)"),
            "the live dock is only re-targeted when this tab owns it: {body}"
        );
        assert!(
            body.contains("self.refresh_diff_dock(to.to_string(), cx);"),
            "the live dock recomputes on the new checkout: {body}"
        );
        assert!(
            body.contains("retarget_parked_dock_slot(slot, from)"),
            "a parked slot goes through the pure helper: {body}"
        );
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

    /// The rendered width of the dock, or `None` when the panel is too narrow
    /// to show it at all.
    fn rendered(preferred: f32, available: f32) -> Option<f32> {
        diff_dock_fit(preferred, available).map(|(width, _)| width)
    }

    #[test]
    fn a_wide_panel_leaves_the_preferred_dock_width_alone() {
        // 1920px of panel: the dock has no reason to give ground, and the
        // preference is what the user dragged it to.
        assert_eq!(rendered(880., 1920.), Some(880.));
    }

    #[test]
    fn opening_a_right_rail_shrinks_the_dock_instead_of_clipping_it() {
        // A 300px rail over a 1280px window leaves ~970px of panel. The dock
        // must fit inside it with the pane grid's reserve, not overflow the
        // panel's clip by the difference.
        let available = 970.;
        let (width, max) = diff_dock_fit(880., available).expect("970px holds the dock");
        assert!(width < 880., "the dock must give ground: {width}");
        assert_eq!(width, max, "a clamped dock renders at its ceiling");
        assert!(
            width + PANE_GRID_RESERVED_WIDTH + crate::layout::PANE_GUTTER_PX <= available,
            "the dock still overflows the panel: {width}"
        );
    }

    /// S3 (#193 audit): below the floor the grid wins. The dock used to stop
    /// at its 360 px floor with nothing left for the panes - at the app's
    /// 800 px minimum window with the sidebar and a right rail open the panel
    /// is ~196 px, so the grid shrank to zero and the dock was clipped. Now
    /// the dock is simply not rendered until there is room for both.
    #[test]
    fn a_maximized_dock_spans_the_panel_minus_the_gutters() {
        let available = 1920.;
        assert_eq!(
            diff_dock_maximized_width(available),
            available - 2. * crate::layout::PANE_GUTTER_PX
        );
        assert!(
            diff_dock_maximized_width(available)
                > diff_dock_fit(880., available)
                    .expect("1920px holds the dock")
                    .1
        );
    }

    #[test]
    fn the_dock_gutter_grows_as_the_pane_grid_is_clipped_away() {
        let gutter = crate::layout::PANE_GUTTER_PX;
        assert_eq!(PaneGridLayout::Flex.dock_left_gutter(), 0.);
        assert_eq!(PaneGridLayout::Hidden.dock_left_gutter(), gutter);
        let half = PaneGridLayout::Clipped {
            visible: 400.,
            full: 800.,
        };
        assert!((half.dock_left_gutter() - gutter / 2.).abs() < 1e-3);
        let gone = PaneGridLayout::Clipped {
            visible: 0.,
            full: 800.,
        };
        assert_eq!(gone.dock_left_gutter(), gutter);
    }

    /// Review findings on #502: the clipped grid keeps the width the canvas
    /// measured while it was flexing, so a toggle mid-slide never lays the
    /// panes out narrower; maximizing takes the keyboard off the hidden grid
    /// (a dock tab's own handle, or a blur, never the pane fallback); and the
    /// strip's close button hands the saved focus back like the toggle does.
    #[test]
    fn maximize_never_resizes_or_strands_the_hidden_grid() {
        let host = include_str!("cli_diff_dock.rs");
        let layout = host
            .split("fn rendered_pane_grid_layout(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("layout");
        assert!(
            layout.contains("full: self.diff_dock.pane_grid_width.get().max(1.),"),
            "the clipped grid keeps its measured width: {layout}"
        );
        assert!(
            !layout.contains("from_width.max("),
            "the clipped width must not follow the slide's endpoints: {layout}"
        );
        let toggle = host
            .split("pub(crate) fn toggle_diff_dock_maximize(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("toggle");
        assert!(
            toggle.contains("self.dock_tab_focus_handle(self.diff_dock.diff_active_tab, cx)")
                && toggle.contains("None => window.blur(),"),
            "maximizing blurs a pane when the dock tab has no handle: {toggle}"
        );
        assert!(
            !toggle.contains("self.focus_diff_tab("),
            "the pane fallback would focus the pane being hidden: {toggle}"
        );
        assert!(
            toggle.contains("self.restore_pre_maximize_focus(previous_focus, window, cx);"),
            "restore shares the focus handoff with the strip close: {toggle}"
        );
        let panel = include_str!("diff_dock/mod.rs");
        let close = panel
            .split("pub(crate) fn close_diff_dock_panel_from_strip(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("strip close");
        assert!(
            close.contains("self.diff_dock.maximized.take()")
                && close.contains("self.dock_owns_focus(focus, window, cx)")
                && close.contains("self.restore_pre_maximize_focus(")
                && close.contains("self.close_diff_dock_panel(cx);"),
            "the strip close restores the saved focus before closing, falling back \
             to a workspace pane when the dock itself owned it: {close}"
        );
        for (name, header) in [
            ("tab strip", include_str!("diff_dock/render.rs")),
            (
                "surface picker",
                include_str!("diff_dock/surface_picker.rs"),
            ),
        ] {
            assert!(
                header.contains("this.close_diff_dock_panel_from_strip(window, cx);")
                    && !header.contains("this.close_diff_dock_panel(cx);"),
                "the {name}'s close button goes through the focus-restoring closer"
            );
        }
        let eye = include_str!("agent_status.rs");
        let under_eye = eye
            .split("pub(crate) fn surfaces_under_user_eye(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("under eye");
        assert!(
            under_eye.contains("self.pane_grid_hidden_by_dock()"),
            "panes behind a maximized dock are not under the user's eye: {under_eye}"
        );
    }

    #[test]
    fn a_maximized_dock_never_drops_below_the_floor() {
        assert_eq!(diff_dock_maximized_width(200.), DIFF_DOCK_PANEL_MIN_WIDTH);
    }

    /// A tab switch parks the live dock through `close_diff_dock_panel`, and
    /// the closer must drop the maximize state with it: a parked dock that
    /// left `maximized` set would hand the incoming tab a hidden pane grid.
    /// A restored slot comes back without the reveal slide.
    #[test]
    fn parking_the_dock_drops_its_maximize_state() {
        let host = include_str!("cli_diff_dock.rs");
        let park = host
            .split("fn park_live_diff_dock(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("park");
        assert!(
            park.contains("self.close_diff_dock_panel(cx);"),
            "parking goes through the closer: {park}"
        );
        let panel = include_str!("diff_dock/mod.rs");
        let close = panel
            .split("pub(crate) fn close_diff_dock_panel(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("closer");
        for needle in [
            "self.diff_dock.maximized = None;",
            "self.diff_dock.maximize_animation = None;",
            "self.diff_dock.reveal_animation = None;",
        ] {
            assert!(
                close.contains(needle),
                "the closer lost `{needle}`: {close}"
            );
        }
        let restore = host
            .split("fn restore_diff_dock(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("restore");
        assert!(
            restore.contains("self.diff_dock.reveal_animation = None;"),
            "a session switch restores a parked dock without a slide: {restore}"
        );
    }

    #[test]
    fn a_panel_too_narrow_for_the_floor_hides_the_dock_and_keeps_the_grid() {
        let floor =
            DIFF_DOCK_PANEL_MIN_WIDTH + PANE_GRID_RESERVED_WIDTH + crate::layout::PANE_GUTTER_PX;
        assert_eq!(floor, 464.);
        assert_eq!(rendered(880., 196.), None, "the 800px-window case");
        assert_eq!(rendered(880., 200.), None);
        assert_eq!(rendered(880., floor - 1.), None, "one pixel short is short");
        // Exactly at the floor the dock renders at its minimum and the grid
        // keeps exactly its reserve.
        assert_eq!(
            diff_dock_fit(880., floor),
            Some((DIFF_DOCK_PANEL_MIN_WIDTH, DIFF_DOCK_PANEL_MIN_WIDTH))
        );
    }

    /// S3 (#193 audit): the dock's floor must never be paid for by the pane
    /// grid. At the app's 800 px minimum window with the sidebar and a right
    /// rail open the panel is ~196 px; a 360 px dock in it shrinks the
    /// `flex_1().min_w_0()` grid to zero and clips the dock. Whatever fits,
    /// the grid keeps at least one minimum pane plus its gutters.
    #[test]
    fn the_pane_grid_never_renders_below_its_minimum_beside_the_dock() {
        let floor =
            DIFF_DOCK_PANEL_MIN_WIDTH + PANE_GRID_RESERVED_WIDTH + crate::layout::PANE_GUTTER_PX;
        for preferred in [DIFF_DOCK_PANEL_MIN_WIDTH, 880., DIFF_DOCK_PANEL_MAX_WIDTH] {
            for available in (0..=3000).map(|px| px as f32) {
                let fit = diff_dock_fit(preferred, available);
                if available < floor {
                    assert_eq!(
                        fit, None,
                        "{available}px cannot hold the dock floor beside a minimum pane: the \
                         grid wins and the dock is not rendered"
                    );
                    continue;
                }
                let Some((width, max)) = fit else {
                    panic!("{available}px holds the floor, so the dock renders");
                };
                let grid = available - width - crate::layout::PANE_GUTTER_PX;
                assert!(
                    grid >= PANE_GRID_RESERVED_WIDTH,
                    "preferred {preferred} in {available}px: dock {width} leaves the grid {grid}px, \
                     below its {PANE_GRID_RESERVED_WIDTH}px reserve"
                );
                assert!(
                    width >= DIFF_DOCK_PANEL_MIN_WIDTH,
                    "{width} is below the floor"
                );
                assert!(
                    width <= preferred,
                    "{width} exceeds the preference {preferred}"
                );
                assert!(width <= max, "{width} exceeds the ceiling {max}");
            }
        }
    }

    /// #422 review: `open` is not "on screen". The dock host is the one
    /// writer of `rendered`, it clears the flag on both early returns (the
    /// unmounted modes and the below-the-floor squeeze) and sets it only once
    /// the fit succeeded, and the seen predicate reads it, so a dock the pane
    /// grid squeezed out never swallows a program notification.
    #[test]
    fn the_seen_predicate_reads_the_rendered_flag_the_host_writes() {
        let host = include_str!("cli_diff_dock.rs");
        let wrapper = host
            .split("pub(crate) fn wrap_cli_diff_dock(")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("dock host");
        let head = wrapper
            .split("self.diff_dock.rendered = true;")
            .next()
            .expect("the host sets `rendered` once the fit succeeded");
        assert_eq!(
            head.matches("self.diff_dock.rendered = false;").count(),
            2,
            "both early returns (unmounted mode, below the floor) clear `rendered`: {head}"
        );
        assert!(
            head.contains("diff_dock_fit(") && head.contains("return body;"),
            "the clears sit before the fit's early return: {head}"
        );
        let seen = host
            .split("pub(crate) fn dock_terminal_is_seen(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("seen predicate");
        assert!(
            seen.contains("self.diff_dock.rendered"),
            "a dock that was not painted is not under the user's eye: {seen}"
        );
        // The non-test source: everything before this test module. The
        // slice cannot start at the file's first `#[cfg(test)]`, which sits
        // on a helper above the dock host.
        let source = host.split("\nmod tests {").next().expect("non-test source");
        let writers = source.matches("diff_dock.rendered =").count();
        assert_eq!(
            writers, 3,
            "the dock host is the only writer of `rendered` (two clears, one set)"
        );
    }

    /// #184 Phase 4: the rendered width is `min(stored, remainder)` with the
    /// floor honoured, and the stored preference is never the thing that
    /// moves - growing the panel back must restore the width the user chose.
    #[test]
    fn the_rendered_width_is_the_smaller_of_preference_and_remainder() {
        let stored = 880.;
        let remainder = 700.;
        let narrow = rendered(stored, remainder).expect("700px holds the dock");
        assert_eq!(
            narrow,
            remainder - PANE_GRID_RESERVED_WIDTH - crate::layout::PANE_GUTTER_PX,
            "under a narrow panel the dock renders at what the panel can spare"
        );
        assert!(narrow >= DIFF_DOCK_PANEL_MIN_WIDTH);
        // The same preference, once the panel is wide again: nothing was lost.
        assert_eq!(
            rendered(stored, 1920.),
            Some(stored),
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

    // --- resize drag -----------------------------------------------------

    /// S2 (#193 audit), the sequence from issue #184: a wide preference
    /// clamped under a rail must survive a drag that pins at the ceiling.
    /// Stored 880 -> a rail opens (836 px of panel: 732 px ceiling, rendered
    /// 732) -> a drag toward wider pins at 732 -> the rail closes -> 880.
    #[test]
    fn a_drag_pinned_at_the_ceiling_leaves_a_wider_preference_alone() {
        let mut stored = 880.;
        let (on_screen, ceiling) = diff_dock_fit(stored, 836.).expect("836px holds the dock");
        assert_eq!((on_screen, ceiling), (732., 732.));
        // Anchored on the rendered edge, the cursor moves 40 px further left.
        stored = diff_dock_drag_preference(stored, on_screen + 40., ceiling);
        assert_eq!(
            stored, 880.,
            "a drag pinned at the ceiling asks for 'at least this wide', which 880 already is"
        );
        // A 1 px nudge wider is the same gesture.
        stored = diff_dock_drag_preference(stored, on_screen + 1., ceiling);
        assert_eq!(stored, 880.);
        assert_eq!(
            rendered(stored, 1920.),
            Some(880.),
            "closing the rail restores the width the user chose"
        );
    }

    #[test]
    fn a_genuine_narrow_drag_stores_the_narrower_width() {
        let (on_screen, ceiling) = diff_dock_fit(880., 836.).expect("836px holds the dock");
        let stored = diff_dock_drag_preference(880., on_screen - 132., ceiling);
        assert_eq!(
            stored, 600.,
            "600 is below the ceiling, so the user chose it"
        );
        assert_eq!(rendered(stored, 1920.), Some(600.));
    }

    #[test]
    fn a_drag_pinned_at_the_ceiling_raises_a_narrower_preference_to_it() {
        // Stored 600 under a 732 px ceiling: dragging past the ceiling means
        // "at least 732", which 600 does not satisfy.
        assert_eq!(diff_dock_drag_preference(600., 800., 732.), 732.);
        // The drag never stores below the floor or above the cap.
        assert_eq!(
            diff_dock_drag_preference(600., 100., 732.),
            DIFF_DOCK_PANEL_MIN_WIDTH
        );
        assert_eq!(
            diff_dock_drag_preference(600., 5000., 5000.),
            DIFF_DOCK_PANEL_MAX_WIDTH
        );
        assert_eq!(
            diff_dock_drag_preference(DIFF_DOCK_PANEL_MAX_WIDTH, 5000., 5000.),
            DIFF_DOCK_PANEL_MAX_WIDTH
        );
    }

    // --- parking ---------------------------------------------------------

    #[test]
    fn a_session_that_never_opened_the_dock_parks_nothing() {
        // The birth state. Parking it would make "this session has a slot"
        // stop meaning "this session has a dock", and every tab merely visited
        // once would grow the map.
        assert!(slot(false, false, 0).is_idle());
        let mut parked = HashMap::new();
        park_dock_slot(&mut parked, Some(7), slot(false, false, 0));
        assert!(parked.is_empty(), "an idle slot must not take a map entry");
    }

    #[test]
    fn a_dock_worth_restoring_is_parked() {
        // Open, or answered, or carrying tabs: each on its own is state the
        // session must find again when it comes back.
        assert!(!slot(true, false, 0).is_idle(), "an open dock must survive");
        assert!(
            !slot(false, true, 0).is_idle(),
            "an answered picker must not ask again"
        );
        assert!(
            !slot(false, false, 1).is_idle(),
            "a Changes / terminal / file tab must not be dropped"
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

    /// B1 (#193 audit): a pane dragged to another folder row when it *is* its
    /// tab closes that tab and reopens the pane as a freshly minted tab of the
    /// destination. The dock followed the pane's tab, so it follows the pane:
    /// re-keyed to the new id, it is still reachable (and still counted by
    /// the close guard) through the new tab, rather than pruned - terminal
    /// and all - the moment the old id stops resolving. Modelled on the slot
    /// map like the test above, because a `PaneFlowApp` cannot be
    /// constructed in a test.
    #[gpui::test]
    fn dragging_a_lone_pane_to_another_folder_takes_its_dock_along(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let mut source = workspace_with_tabs(1, "/tmp/a", &["lone"]);
        let mut dest = workspace_with_tabs(2, "/tmp/b", &["home"]);
        let old = tab_ids(&source)[0];
        let bystander = tab_ids(&dest)[0];

        // The lone tab is the visible session, so it owns the live dock (with
        // an agent in a terminal tab); the destination's own tab has a parked
        // dock of its own.
        let live_tabs = vec![DiffDockTab::Changes, terminal_tab(cx)];
        let agent = dock_terminals(live_tabs.iter());
        let mut owner = Some(old);
        let mut parked = HashMap::new();
        parked.insert(bystander, terminal_slot(cx));
        let bystander_terminal = dock_terminals(parked[&bystander].tabs.iter());

        // The gesture on the model: the pane was the whole tab, so the tab
        // goes (the FR-01 placeholder takes its place) and the pane lands in
        // a new tab of the destination.
        let vacated = source.close_tab(0).expect("the lone tab detaches");
        assert_eq!(vacated.id, old);
        assert!(dest.open_tab(Tab::new("", None)));
        let new = dest.active_tab().id;
        assert_ne!(new, old, "the pane lands in a freshly minted tab");
        let workspaces = [source, dest];
        assert!(
            !session_is_live(&workspaces, old),
            "the vacated id resolves nothing: without a re-home the next \
             reconcile prunes its dock"
        );

        rehome_dock_session(&mut owner, &mut parked, old, new);
        assert_eq!(owner, Some(new), "the live dock now belongs to the new tab");
        prune_parked_dock_slots(&mut parked, &workspaces);

        // The close guard sees the agent through the new tab, nothing through
        // the old id, and the bystander's dock is untouched.
        let for_new = dock_terminals(dock_tabs_for_session(owner, &live_tabs, &parked, new));
        assert_eq!(
            ids(&for_new),
            ids(&agent),
            "the dock terminal survives the move"
        );
        assert!(dock_terminals(dock_tabs_for_session(owner, &live_tabs, &parked, old)).is_empty());
        let for_bystander =
            dock_terminals(dock_tabs_for_session(owner, &live_tabs, &parked, bystander));
        assert_eq!(ids(&for_bystander), ids(&bystander_terminal));
    }

    /// The parked half of the same re-home: a dock parked under the vacated
    /// id (the frame between a switch and the next paint) moves under the
    /// new id, and the prune keeps it. Nothing else in the map moves.
    #[test]
    fn rehoming_a_parked_dock_moves_its_slot_under_the_new_tab_id() {
        let mut source = workspace_with_tabs(1, "/tmp/a", &["lone", "staying"]);
        let mut dest = workspace_with_tabs(2, "/tmp/b", &["home"]);
        let (old, staying) = (tab_ids(&source)[0], tab_ids(&source)[1]);
        let mut owner = Some(staying);
        let mut parked = HashMap::new();
        park_dock_slot(&mut parked, Some(old), slot(true, true, 3));

        source.close_tab(0).expect("the lone tab detaches");
        assert!(dest.open_tab(Tab::new("", None)));
        let new = dest.active_tab().id;
        rehome_dock_session(&mut owner, &mut parked, old, new);
        prune_parked_dock_slots(&mut parked, &[source, dest]);

        assert_eq!(owner, Some(staying), "a different owner is left alone");
        assert!(
            !parked.contains_key(&old),
            "nothing is left under the dead id"
        );
        let moved = parked.get(&new).expect("the slot moved under the new id");
        assert!(moved.open && moved.picked && moved.tabs.len() == 3);
        assert_eq!(parked.len(), 1);

        // A no-op re-home (same id) changes nothing.
        rehome_dock_session(&mut owner, &mut parked, new, new);
        assert!(parked.contains_key(&new));
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

        // Every `close_tab` site in the tab ops is paired, inside the same
        // method, with one of: the explicit teardown, the re-home that hands
        // the dock to the tab the content moved into, or re-opening the very
        // same `Tab` (`open_tab(tab)`: same id, so the dock keys stay live).
        // An unpaired site is the #193 audit's B1: the tab id stops resolving,
        // the next reconcile prunes the slot, and its terminals die silently.
        let sites: Vec<usize> = ops.match_indices(".close_tab(").map(|(at, _)| at).collect();
        assert!(
            sites.len() >= 3,
            "expected the three tab-removal sites in workspace_ops/tab.rs, found {sites:?}"
        );
        for at in sites {
            let fn_start = ops[..at]
                .rfind("\n    pub(crate) fn ")
                .into_iter()
                .chain(ops[..at].rfind("\n    fn "))
                .max()
                .expect("a `.close_tab(` site inside a method");
            let name = ops[fn_start..]
                .split("fn ")
                .nth(1)
                .and_then(|rest| rest.split('(').next())
                .unwrap_or_default();
            let fn_end = at + ops[at..].find("\n    }\n").expect("end of the method");
            let tail = &ops[at..fn_end];
            let paired = tail.contains("drop_diff_dock_for_tab(")
                || tail.contains("rehome_diff_dock_for_tab(")
                || tail.contains(".open_tab(tab)");
            assert!(
                paired,
                "`{name}` removes a tab for good without a dock teardown or re-home: {tail}"
            );
        }
    }

    /// N5 (#193 audit): the prune is only a safety net if it runs whether or
    /// not the visible session changed. It must sit above the early return.
    #[test]
    fn the_parked_dock_prune_runs_every_frame_before_the_session_early_return() {
        let src = include_str!("cli_diff_dock.rs");
        let sync = src
            .split("fn sync_diff_dock_session(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("session reconcile");
        let prune_at = sync
            .find("prune_parked_diff_docks(")
            .expect("the reconcile must prune dead slots");
        let early_return_at = sync
            .find("if self.diff_dock.owner == active")
            .expect("the reconcile's early return");
        assert!(
            prune_at < early_return_at,
            "the prune must run before the `owner == active` early return, or a slot whose \
             tab vanished waits for the next session switch: {sync}"
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
