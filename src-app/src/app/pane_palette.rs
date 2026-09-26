//! EP-005 (prd-cli-tab-hierarchy): the « New pane » preset picker.
//!
//! One entry point for the moment the user decides *what* to launch. The
//! picker is a pure view over catalogues that already exist - the default
//! shell and the agents made visible in Settings -> AI Agent
//! ([`TerminalAgent::visible`]). Nothing new is written to
//! `paneflow.json`: US-015 forbids a `presets` table, and every agent command
//! comes back from [`TerminalAgent::launch_command`] so the Claude bypass
//! setting keeps being honored instead of being reimplemented here.
//!
//! Shape: the picker is a *pane-sized card*, never an overlay, holding one
//! centered column of plain buttons - nothing folds open, nothing is filtered.
//! It appears in the slot the new surface is about to occupy: `New tab` and the
//! sidebar's `+` open a `New pane` tab and fill it, while the pane header's
//! split buttons show it in the half the split is about to create, next to the
//! panes that stay visible. Up / Down move the cursor, Enter launches, Escape
//! creates nothing and hands focus back.

use crate::app::overlay_origin::OverlayKind;
use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseUpEvent, ParentElement,
    ScrollHandle, SharedString, Styled, WeakEntity, Window, deferred, div, prelude::*, px, svg,
};
use paneflow_config::schema::{PaneFlowConfig, TerminalSurfaceProfile};

use crate::PaneFlowApp;
use crate::agent_launcher::TerminalAgent;
use crate::layout::SplitDirection;
use crate::pane::Pane;
use crate::settings::components::{select_item, select_menu, with_alpha};
use crate::ui_primitives::squircle::squircle_fill;
use crate::ui_primitives::{ROW_RADIUS, squircle_skin};

/// Title of the surface the picker stands in for, until a preset renames it.
pub(crate) const PALETTE_TAB_TITLE: &str = "New pane";

/// Width of the picker's column: the preset buttons, the branch select above
/// them, and that select's menu all share one edge.
const PICKER_WIDTH: f32 = 260.0;

/// Where the picked preset lands - and therefore where the card is drawn.
pub(crate) enum PalettePlacement {
    /// The picker is the whole content of the empty tab it just created.
    /// Picking fills that tab; Escape closes it again.
    Tab { tab_id: u64 },
    /// The picker stands in the half a split is about to create, beside the
    /// panes that stay on screen. Nothing is split until a preset is picked,
    /// so Escape leaves the tab exactly as it was.
    Split {
        target: WeakEntity<Pane>,
        direction: SplitDirection,
    },
}

/// What one row of the branch select points at (issue #347).
#[derive(Clone)]
enum BranchTarget {
    /// A branch, checked out on demand if nothing holds it yet.
    Branch(String),
    /// A checkout with no branch to name it - bound directly, since there is
    /// no branch to resolve.
    Checkout(std::path::PathBuf),
}

/// One row of the branch select.
struct BranchOption {
    target: BranchTarget,
    label: String,
    selected: bool,
    /// Whether picking it has to create a worktree first.
    needs_checkout: bool,
}

/// The two catalogues the picker projects (US-015). No third source, and
/// no persistence of its own.
#[derive(Debug, Clone)]
pub(crate) enum PresetSource {
    /// The configured default shell, launched as a plain terminal surface.
    Shell,
    Agent(TerminalAgent),
}

/// One picker button.
#[derive(Debug, Clone)]
pub(crate) struct Preset {
    pub(crate) label: String,
    pub(crate) source: PresetSource,
}

/// What identifies a row across rebuilds of the catalogue. The keyboard
/// cursor is kept by key, not by position (issue #518): the cold PATH walk
/// inserts agent rows once it publishes, and a numeric index taken before
/// that frame would then name a different row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PresetKey {
    Shell,
    Agent(TerminalAgent),
}

impl Preset {
    pub(crate) fn key(&self) -> PresetKey {
        match &self.source {
            PresetSource::Shell => PresetKey::Shell,
            PresetSource::Agent(agent) => PresetKey::Agent(*agent),
        }
    }

    fn icon_path(&self) -> SharedString {
        match &self.source {
            PresetSource::Shell => "icons/terminal.svg".into(),
            PresetSource::Agent(agent) => agent.icon_path().into(),
        }
    }

    fn icon_multicolor(&self) -> bool {
        matches!(&self.source, PresetSource::Agent(agent) if agent.icon_multicolor())
    }

    fn accent(&self) -> Option<u32> {
        match &self.source {
            PresetSource::Agent(agent) => agent.accent(),
            _ => None,
        }
    }

    fn profile(&self) -> TerminalSurfaceProfile {
        match &self.source {
            PresetSource::Agent(_) => TerminalSurfaceProfile::Agent,
            _ => TerminalSurfaceProfile::Normal,
        }
    }

    /// The line written to the new terminal, or `None` for a bare shell.
    fn command(&self, config: &PaneFlowConfig) -> Option<String> {
        match &self.source {
            PresetSource::Shell => None,
            PresetSource::Agent(agent) => Some(agent.launch_command(config)),
        }
    }

    /// `Err` carries the readable refusal a not-installed agent must produce
    /// instead of an empty terminal (US-015 AC4).
    /// The render-frame answer: reads the installed-binary snapshot without
    /// waiting on the cold PATH walk, so a row can paint as muted while the
    /// walk is still out. Never a substitute for [`Self::ensure_launchable`].
    fn looks_launchable(&self) -> bool {
        match &self.source {
            PresetSource::Agent(agent) => agent.is_installed(),
            _ => true,
        }
    }

    fn ensure_launchable(&self) -> Result<(), String> {
        match &self.source {
            // Issue #518: the first PATH walk has not published. Never wait
            // for it on the GPUI thread: the launch is queued
            // (`PanePaletteState::launch_queued`) and replayed by the boot
            // warm's completion, so the Enter is not dropped either.
            PresetSource::Agent(_) if self.awaits_scan() => {
                Err(crate::agent_launcher::AGENT_SCAN_PENDING_COPY.to_string())
            }
            PresetSource::Agent(agent) if !agent.is_installed() => Err(format!(
                "{} is not installed - install its CLI, or hide it in Settings > AI Agent",
                agent.display_name()
            )),
            _ => Ok(()),
        }
    }

    /// An agent row whose only blocker is the pending first PATH walk.
    fn awaits_scan(&self) -> bool {
        matches!(
            &self.source,
            PresetSource::Agent(agent)
                if !agent.is_installed() && crate::agent_launcher::installed_binary_scan_pending()
        )
    }
}

/// Live picker state, owned by `PaneFlowApp`. `None` = closed.
pub(crate) struct PanePaletteState {
    /// Workspace the preset lands in, by stable id (survives reorders).
    pub(crate) ws_id: u64,
    pub(crate) placement: PalettePlacement,
    /// Keyboard cursor into the preset list, as painted last frame. Only a
    /// scroll hint: launches and highlights resolve [`Self::selected_key`].
    pub(crate) selected: usize,
    /// The row the cursor names, by identity, so a catalogue reshaped by the
    /// cold PATH walk (issue #518) still launches the row the user chose.
    pub(crate) selected_key: PresetKey,
    /// A launch confirmed while the first PATH walk was still pending
    /// (issue #518). The boot warm's completion hands it to
    /// `pending_palette_launch`, which the window-bearing drain replays, so
    /// the confirm neither waits on the GPUI thread nor gets dropped.
    pub(crate) launch_queued: Option<Preset>,
    /// Last refusal, shown under the buttons (US-015 AC4).
    pub(crate) error: Option<String>,
    /// Focus to hand back when the picker goes away (US-014 AC5). `None` for
    /// a split placement, which hands focus back to its target pane instead.
    pub(crate) restore_focus: Option<FocusHandle>,
    pub(crate) scroll: ScrollHandle,
    /// Whether the branch menu is up over the presets (issue #347). Closed on
    /// open, so the picker always comes up on its presets.
    pub(crate) branch_picker_open: bool,
}

/// Whether a tab should have a `Tab` picker attached. Same emptiness guard
/// as `open_tab_with_surface` (`root.is_none() && saved_layout.is_none()`),
/// plus whether a picker already owns this tab.
fn tab_needs_palette(
    root_is_none: bool,
    saved_layout_is_none: bool,
    palette_targets_this_tab: bool,
) -> bool {
    root_is_none && saved_layout_is_none && !palette_targets_this_tab
}

/// Whether the `Tab` picker is the only surface its workspace has left
/// (upstream 9aa03d09, issue #522): the picker sits on the workspace's sole
/// tab and that tab has no pane. Closing it would close the tab, push an
/// undo entry for an empty tab, and have the next frame reinstall a fresh
/// picker on a new tab, so the close is a no-op instead.
///
/// `palette_tab` is the picker's tab id for a `Tab` placement (`None` for a
/// split picker or no picker); `tabs` is `(tab id, root_is_none)` for each
/// tab of the picker's workspace.
fn palette_holds_last_surface(palette_tab: Option<u64>, tabs: &[(u64, bool)]) -> bool {
    let Some(tab_id) = palette_tab else {
        return false;
    };
    match tabs {
        [(id, root_is_none)] => *id == tab_id && *root_is_none,
        _ => false,
    }
}

/// Whether creating a picker should also open the Agent sessions sidebar.
pub(crate) fn palette_should_open_sessions(
    setting_on: bool,
    has_enabled_session_agent: bool,
    tab_placement: bool,
) -> bool {
    setting_on && has_enabled_session_agent && tab_placement
}

/// Palette-bound history is window-global. Workspace activation must not
/// close it on an empty New pane tab (no leaf) or retarget it onto a
/// waiting-agent pane: either would hide history while the picker stays
/// up, or steal resume into the live agent.
pub(crate) fn palette_bound_sessions_survives_activation(
    bound_palette: Option<(u64, u64)>,
    open_tab_palette: Option<(u64, u64)>,
) -> bool {
    match (bound_palette, open_tab_palette) {
        (Some(bound), Some(open)) => bound == open,
        _ => false,
    }
}

/// Whether a `prepare_branch_checkout` error means git could not resolve the
/// configured new-tab branch (issue #549). Other failures (path collisions,
/// a blocked worktree add) stay hard errors so a tab is not opened anyway.
fn new_tab_checkout_unresolved(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    (lower.contains("local branch ") && lower.contains(" does not exist"))
        || lower.contains("invalid reference")
        || lower.contains("unknown revision")
        || lower.contains("not a literal branch name")
}

/// Toast for a failed new-tab checkout. Keeps git's own reason; the Settings
/// hint is how the user stops hitting a missing default like `main`.
fn new_tab_checkout_failure_toast(branch: &str, error: &str) -> String {
    format!(
        "Could not check out {branch}: {error}. Choose a new-tab branch in Settings → Workspaces."
    )
}

impl PaneFlowApp {
    /// Build the picker catalogue (US-015): Terminal first, then the visible
    /// agents in `TerminalAgent::ALL` order. `ws_idx` must name a live
    /// workspace; a gone one yields no rows, so a keypress cannot launch
    /// into it. The rows themselves do not vary by workspace.
    pub(crate) fn pane_palette_presets(&self, ws_idx: usize) -> Vec<Preset> {
        if self.workspaces.get(ws_idx).is_none() {
            return Vec::new();
        }
        let mut presets = vec![Preset {
            label: "Terminal".to_string(),
            source: PresetSource::Shell,
        }];
        // Issue #518: while the first PATH walk is pending the snapshot says
        // nothing is installed, which would drop every default-enabled agent
        // from the catalogue and leave the `looking` row unreachable. Treat
        // them as installed until the walk publishes; the rows paint muted
        // with `looking`, and the confirm still waits for the real answer.
        let scan_pending = crate::agent_launcher::installed_binary_scan_pending();
        presets.extend(
            TerminalAgent::visible_with(&self.cached_config, |agent| {
                scan_pending || agent.is_installed()
            })
            .into_iter()
            .map(|agent| Preset {
                label: agent.display_name().to_string(),
                source: PresetSource::Agent(agent),
            }),
        );
        presets
    }

    /// Re-resolve the picker's workspace by id (it may have been reordered or
    /// closed while the picker was open).
    fn pane_palette_ws_idx(&self) -> Option<usize> {
        let ws_id = self.pane_palette.as_ref()?.ws_id;
        self.workspaces.iter().position(|ws| ws.id == ws_id)
    }

    /// The row the keyboard cursor names, resolved by identity against the
    /// catalogue as it is now (issue #518). `None` when that row is gone (an
    /// agent that read `looking` and turned out not to be installed): Enter
    /// is then inert rather than launching whatever now sits at the painted
    /// index, and the next Up/Down re-anchors the cursor.
    fn pane_palette_selected(&self) -> Option<(usize, Preset)> {
        let palette = self.pane_palette.as_ref()?;
        let presets = self.pane_palette_presets(self.pane_palette_ws_idx()?);
        let idx = resolve_selected(&presets, &palette.selected_key)?;
        let preset = presets.into_iter().nth(idx)?;
        Some((idx, preset))
    }

    /// Issue #518: the boot warm has published. A launch confirmed while the
    /// walk was pending is handed to the window-bearing drain, which replays
    /// it through `pane_palette_launch` (the real installed answer applies
    /// now, so an absent agent is refused there).
    pub(crate) fn pane_palette_resume_queued_launch(&mut self, cx: &mut Context<Self>) {
        let queued = self
            .pane_palette
            .as_mut()
            .and_then(|palette| palette.launch_queued.take());
        if let Some(preset) = queued {
            if let Some(palette) = self.pane_palette.as_mut() {
                palette.error = None;
            }
            self.pending_palette_launch = Some(preset);
            cx.notify();
        }
    }

    /// Open a `New pane` tab in `ws_idx` and make the preset picker its
    /// content. Entry points: the `New tab` action and the sidebar folder
    /// row's hover `+` (US-010 AC).
    ///
    /// The picker is the tab's surface, not an overlay: creating a tab and
    /// choosing what runs in it are one gesture, so the choice is made in the
    /// place the result will appear rather than over the panes it hides.
    pub(crate) fn open_pane_palette(
        &mut self,
        ws_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        if !ws.can_open_tab() {
            self.show_toast("Tab limit reached for this workspace", cx);
            return;
        }
        let ws_id = ws.id;
        let repo_root = ws.repo_root.clone();
        let branch = self
            .cached_config
            .new_tab_branch_for_workspace(&ws.cwd)
            .map(str::to_string);
        if branch.is_none() || repo_root.is_none() {
            self.open_pane_palette_at_checkout(ws_idx, None, window, cx);
            return;
        }
        if let Some(branch) = self.branch_checkout_pending.clone() {
            self.show_toast(format!("Still checking out {branch}"), cx);
            return;
        }
        let (Some(repo_root), Some(branch)) = (repo_root, branch) else {
            return;
        };
        let handle = window.window_handle();
        self.branch_checkout_pending = Some(branch.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let requested_branch = branch.clone();
            let prepared = smol::unblock(move || {
                crate::workspace::worktree::prepare_branch_checkout(&repo_root, &requested_branch)
            })
            .await;
            let _ = handle.update(cx, |_, window, cx| {
                this.update(cx, |app, cx| {
                    app.branch_checkout_pending = None;
                    cx.notify();
                    let Some(ws_idx) = app.workspaces.iter().position(|ws| ws.id == ws_id) else {
                        return;
                    };
                    match prepared {
                        Ok(path) => {
                            let Some(path) = crate::workspace::existing_worktree_dir(Some(path))
                            else {
                                app.show_toast(
                                    format!("The {branch} checkout no longer exists; run `git worktree prune`"),
                                    cx,
                                );
                                return;
                            };
                            app.open_pane_palette_at_checkout(ws_idx, Some(path), window, cx);
                        }
                        Err(error) => {
                            log::warn!("new tab on {branch}: {error}");
                            app.show_toast(
                                new_tab_checkout_failure_toast(&branch, &error),
                                cx,
                            );
                            if new_tab_checkout_unresolved(&error) {
                                app.open_pane_palette_at_checkout(ws_idx, None, window, cx);
                            }
                        }
                    }
                })
            });
        })
        .detach();
    }

    fn open_pane_palette_at_checkout(
        &mut self,
        ws_idx: usize,
        checkout: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        let ws_id = ws.id;
        // The workspace's own checkout needs no binding. This also preserves
        // a workspace opened at a subdirectory when it is already on the selected branch.
        let checkout = checkout.filter(|path| path != &ws.worktree_root);
        self.commit_rename(cx);
        self.dismiss_transient_surfaces();
        let restore_focus = window.focused(cx);
        // Issue #584: closing the picker hands the focus back to the pane it
        // was opened from.
        self.remember_overlay_origin(OverlayKind::PanePalette, window, cx);

        let mut tab = crate::workspace::Tab::new(PALETTE_TAB_TITLE, None);
        tab.worktree = checkout.clone();
        let tab_id = tab.id;
        let opened = self
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.open_tab(tab));
        if !opened {
            self.show_toast("Tab limit reached for this workspace", cx);
            return;
        }
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            // A tab created from a collapsed folder row must be visible.
            ws.sidebar_expanded = true;
        }

        if let Some(checkout) = checkout {
            Self::spawn_initial_git_stats(ws_id, checkout.to_string_lossy().into_owned(), cx);
        }
        // The tab exists before any preset is picked, which is what lets the
        // worktree be chosen BEFORE the agent's process spawns (issue #347).
        // Refresh the repository's branch and worktree lists now so the header
        // row is populated by the time the eye reaches it.
        self.spawn_worktree_listing(ws_idx, cx);
        self.pane_palette = Some(PanePaletteState {
            ws_id,
            placement: PalettePlacement::Tab { tab_id },
            selected: 0,
            selected_key: PresetKey::Shell,
            launch_queued: None,
            error: None,
            restore_focus,
            scroll: ScrollHandle::new(),
            branch_picker_open: false,
        });
        let tab_idx = self.workspaces[ws_idx].active_tab_idx();
        self.focus_workspace_tab(ws_idx, tab_idx, window, cx);
        // The card owns the keyboard: there is no text field to hand focus to.
        window.focus(&self.pane_palette_focus, cx);
        self.maybe_open_sessions_for_tab_palette(ws_id, tab_id, None, cx);
        cx.notify();
    }

    /// Show the picker in the slot a split would create, instead of dropping a
    /// bare shell there. Entry point: the pane header's split buttons
    /// (`PaneEvent::Split`).
    ///
    /// No `Window` here - the pane-event subscriber has none - so focus is
    /// claimed by `drain_pending_window_actions` through
    /// `pending_palette_focus`, the same deferral `pending_pane_focus` uses
    /// for a drop-to-split.
    pub(crate) fn open_split_palette(
        &mut self,
        target: Entity<Pane>,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        let ws_id = target.read(cx).workspace_id;
        self.commit_rename(cx);
        self.dismiss_transient_surfaces();
        // Issue #584: the split's target pane is the origin by construction
        // (no `Window` here to resolve the focused pane with).
        self.remember_overlay_origin_pane(OverlayKind::PanePalette, &target);
        self.pane_palette = Some(PanePaletteState {
            ws_id,
            placement: PalettePlacement::Split {
                target: target.downgrade(),
                direction,
            },
            selected: 0,
            selected_key: PresetKey::Shell,
            launch_queued: None,
            error: None,
            restore_focus: None,
            scroll: ScrollHandle::new(),
            // A split fills an existing tab, whose binding already governs
            // where its panes start: no branch row, nothing to unfold.
            branch_picker_open: false,
        });
        self.pending_palette_focus = true;
        cx.notify();
    }

    /// Drop a split picker whose slot is no longer on screen - the target pane
    /// was closed, or the user switched tab or project. Without this the state
    /// would survive invisibly and re-appear on the way back, holding focus in
    /// the meantime.
    pub(crate) fn prune_stale_split_palette(&mut self, cx: &mut Context<Self>) {
        let Some(palette) = self.pane_palette.as_ref() else {
            return;
        };
        let PalettePlacement::Split { target, .. } = &palette.placement else {
            return;
        };
        let ws_id = palette.ws_id;
        let visible = target.upgrade().is_some_and(|target| {
            self.active_workspace().is_some_and(|ws| {
                ws.id == ws_id
                    && ws
                        .active_tab()
                        .root
                        .as_ref()
                        .is_some_and(|root| root.contains_leaf(&target))
            })
        });
        if !visible {
            self.pane_palette = None;
            self.forget_overlay_origin(OverlayKind::PanePalette);
            cx.notify();
        }
    }

    /// Attach a `Tab` picker to the active workspace's paneless tab when
    /// nothing already owns it. Restored `"New pane"` tabs, a folder that
    /// opened empty, and the substitute left by closing the last pane all
    /// land here with no in-memory palette.
    pub(crate) fn ensure_empty_tab_palette(&mut self, cx: &mut Context<Self>) {
        let (ws_id, tab_id, root_is_none, saved_layout_is_none) = {
            let Some(ws) = self.workspaces.get_mut(self.active_idx) else {
                return;
            };
            if ws.tab_count() == 0 {
                let _ = ws.open_tab(crate::workspace::Tab::new(PALETTE_TAB_TITLE, None));
            }
            let tab = ws.active_tab();
            (
                ws.id,
                tab.id,
                tab.root.is_none(),
                tab.saved_layout.is_none(),
            )
        };
        let palette_targets_this_tab = self.pane_palette.as_ref().is_some_and(|palette| {
            palette.ws_id == ws_id
                && matches!(
                    palette.placement,
                    PalettePlacement::Tab { tab_id: id } if id == tab_id
                )
        });
        if !tab_needs_palette(root_is_none, saved_layout_is_none, palette_targets_this_tab) {
            if palette_targets_this_tab {
                self.maybe_open_sessions_for_tab_palette(ws_id, tab_id, None, cx);
            }
            return;
        }
        // Restored `New pane` tabs and last-pane-closed tabs reach the picker
        // here, so they need the branch list as much as a freshly opened one.
        if let Some(ws_idx) = self.workspaces.iter().position(|ws| ws.id == ws_id) {
            self.spawn_worktree_listing(ws_idx, cx);
        }
        self.pane_palette = Some(PanePaletteState {
            ws_id,
            placement: PalettePlacement::Tab { tab_id },
            selected: 0,
            selected_key: PresetKey::Shell,
            launch_queued: None,
            error: None,
            restore_focus: None,
            scroll: ScrollHandle::new(),
            branch_picker_open: false,
        });
        self.pending_palette_focus = true;
        self.maybe_open_sessions_for_tab_palette(ws_id, tab_id, None, cx);
        cx.notify();
    }

    /// `palette_holds_last_surface` over the live picker and its workspace.
    fn pane_palette_holds_last_surface(&self) -> bool {
        let Some(palette) = self.pane_palette.as_ref() else {
            return false;
        };
        let PalettePlacement::Tab { tab_id } = &palette.placement else {
            return false;
        };
        let Some(ws) = self.workspaces.iter().find(|ws| ws.id == palette.ws_id) else {
            return false;
        };
        let tabs: Vec<(u64, bool)> = ws
            .tabs()
            .iter()
            .map(|tab| (tab.id, tab.root.is_none()))
            .collect();
        palette_holds_last_surface(Some(*tab_id), &tabs)
    }

    pub(crate) fn open_tab_palette_ids(&self) -> Option<(u64, u64)> {
        let palette = self.pane_palette.as_ref()?;
        match palette.placement {
            PalettePlacement::Tab { tab_id } => Some((palette.ws_id, tab_id)),
            PalettePlacement::Split { .. } => None,
        }
    }

    /// Target pane and direction of a pending split picker in the *active*
    /// tab, so the layout tree can draw the picker at that pane's slot. `None`
    /// when the picker is closed or owns a whole tab.
    pub(crate) fn pending_split_palette(&self) -> Option<(Entity<Pane>, SplitDirection)> {
        let palette = self.pane_palette.as_ref()?;
        let PalettePlacement::Split { target, direction } = &palette.placement else {
            return None;
        };
        let target = target.upgrade()?;
        (self.active_workspace()?.id == palette.ws_id).then_some((target, *direction))
    }

    /// Drop the picker state without touching its tab. Used by the launch
    /// path, which is about to fill that very tab.
    pub(crate) fn discard_pane_palette(&mut self, cx: &mut Context<Self>) {
        self.close_palette_bound_sessions_sidebar(cx);
        self.pane_palette = None;
        self.forget_overlay_origin(OverlayKind::PanePalette);
        cx.notify();
    }

    /// Escape path: nothing is created, and focus goes back to the element
    /// that had it before the picker opened (US-014 AC5). A tab picker closes
    /// its own tab; a split picker only disappears, since it never split
    /// anything.
    pub(crate) fn close_pane_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The workspace's only surface: closing it would just reinstall a
        // fresh picker on a new tab next frame (issue #522).
        if self.pane_palette_holds_last_surface() {
            return;
        }
        let Some(palette) = self.pane_palette.take() else {
            return;
        };
        // Whether a pane held the focus when the picker opened (#584). The
        // saved handle below is restored only while that pane is still a
        // leaf of the tab the focus returns to, checked AFTER the tab close
        // so the check reads the tab that becomes visible: a pane closed
        // from the sidebar while the picker tab was up would otherwise get
        // the focus back through a handle no element renders. A handle
        // outside every pane (sidebar, placeholder) recorded
        // nothing and is restored as before.
        let origin_recorded = self.overlay_origin_recorded(OverlayKind::PanePalette);
        self.close_palette_bound_sessions_sidebar(cx);
        match &palette.placement {
            PalettePlacement::Tab { tab_id } => {
                let position = self
                    .workspaces
                    .iter()
                    .position(|ws| ws.id == palette.ws_id)
                    .and_then(|ws_idx| {
                        self.workspaces[ws_idx]
                            .tabs()
                            .iter()
                            .position(|tab| tab.id == *tab_id)
                            .map(|tab_idx| (ws_idx, tab_idx))
                    });
                if let Some((ws_idx, tab_idx)) = position {
                    self.close_workspace_tab(ws_idx, tab_idx, window, cx);
                }
            }
            PalettePlacement::Split { target, .. } => {
                if let Some(target) = target.upgrade() {
                    target.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
        }
        // Taken on every close so the entry never outlives the picker; the
        // tab close above already focused the first pane (or the placeholder)
        // for the case where the saved handle is skipped.
        let origin_live = self
            .take_live_overlay_origin(OverlayKind::PanePalette)
            .is_some();
        if let Some(handle) = palette.restore_focus
            && (origin_live || !origin_recorded)
        {
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn close_palette_bound_sessions_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.agent_sessions.sessions_bound_palette.take().is_some()
            && self.agent_sessions.sessions_sidebar_open
        {
            self.close_sessions_sidebar(cx);
        }
    }

    /// Open the sessions sidebar for a Tab-placement picker when the setting
    /// is on and at least one session-capable agent is visible. Split
    /// pickers never take this path. Bound to this `(ws_id, tab_id)` so a
    /// later click resumes into the picker tab.
    ///
    /// Already-bound is a user-dismiss latch: the empty-tab normalizer must
    /// not reopen history for the same picker. Safe only because workspace
    /// activation leaves palette-bound history alone.
    fn maybe_open_sessions_for_tab_palette(
        &mut self,
        ws_id: u64,
        tab_id: u64,
        focus_window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.agent_sessions.sessions_bound_palette == Some((ws_id, tab_id)) {
            return;
        }
        let has_agents =
            !crate::agent_sessions::enabled_session_agents_from_config(&self.cached_config)
                .is_empty();
        if !palette_should_open_sessions(
            self.cached_config.new_pane_shows_sessions(),
            has_agents,
            true,
        ) {
            return;
        }
        let Some(ws_idx) = self.workspaces.iter().position(|ws| ws.id == ws_id) else {
            return;
        };
        let cwd = {
            let cwd = &self.workspaces[ws_idx].cwd;
            (!cwd.is_empty()).then(|| cwd.clone())
        };
        self.open_sessions_sidebar_at(cwd, None, focus_window, cx);
        self.agent_sessions.sessions_bound_palette = Some((ws_id, tab_id));
    }

    fn pane_palette_set_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(palette) = self.pane_palette.as_mut() {
            palette.error = Some(message.into());
            cx.notify();
        }
    }

    /// Launch `preset` where the picker stands. The row that was clicked or
    /// confirmed is passed by value, never re-resolved from its painted index:
    /// the cold PATH walk (issue #518) inserts agent rows once it publishes,
    /// so an index captured from the pre-scan frame would launch a newly
    /// inserted agent instead of the shell or agent row the user chose.
    pub(crate) fn pane_palette_launch(
        &mut self,
        preset: Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws_idx) = self.pane_palette_ws_idx() else {
            self.pane_palette_set_error("This project is no longer open", cx);
            return;
        };
        // A pane started now would spawn in the checkout being left behind: the
        // binding is what decides its cwd, and it is not settled yet.
        if let Some(branch) = self.branch_checkout_pending.clone() {
            self.pane_palette_set_error(format!("Checking out {branch}..."), cx);
            return;
        }
        if let Err(message) = preset.ensure_launchable() {
            if let Some(palette) = self.pane_palette.as_mut().filter(|_| preset.awaits_scan()) {
                palette.launch_queued = Some(preset.clone());
            }
            self.pane_palette_set_error(message, cx);
            return;
        }
        let command = preset.command(&self.cached_config);
        let profile = preset.profile();
        let title = preset.label.clone();
        let placement = match self.pane_palette.as_ref() {
            Some(palette) => match &palette.placement {
                PalettePlacement::Tab { tab_id } => Err(*tab_id),
                PalettePlacement::Split { target, direction } => Ok((target.clone(), *direction)),
            },
            None => return,
        };

        match placement {
            Err(tab_id) => {
                // The picker *is* this tab, so the preset fills it in place.
                // `open_tab_with_surface` fills the workspace's active tab,
                // so the placement's own tab has to be that tab and still
                // paneless: a launch replayed after the cold PATH walk
                // (issue #518) may arrive after the user switched tabs or
                // closed the `New pane` tab, and must not land elsewhere.
                let owns_active_tab = ws_idx == self.active_idx
                    && self.workspaces.get(ws_idx).is_some_and(|ws| {
                        let tab = ws.active_tab();
                        tab.id == tab_id && tab.root.is_none() && tab.saved_layout.is_none()
                    });
                if !owns_active_tab {
                    self.pane_palette_set_error("That New pane tab is no longer active", cx);
                    return;
                }
                // Drop the state first, otherwise closing the picker would
                // close the tab that is about to receive the pane.
                self.discard_pane_palette(cx);
                self.open_tab_with_surface(ws_idx, title, profile, command, window, cx);
            }
            Ok((target, direction)) => {
                let Some(target) = target.upgrade() else {
                    self.pane_palette_set_error("That pane no longer exists", cx);
                    return;
                };
                match self.split_with_target(
                    target,
                    direction,
                    profile,
                    command.as_deref(),
                    window,
                    cx,
                ) {
                    // The refusal (the `MAX_PANES` cap in particular) stays
                    // inside the picker, and the tab is left untouched.
                    Err(message) => self.pane_palette_set_error(message, cx),
                    Ok(()) => self.discard_pane_palette(cx),
                }
            }
        }
    }

    pub(crate) fn handle_pane_palette_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = self
            .pane_palette_ws_idx()
            .map_or(0, |ws_idx| self.pane_palette_presets(ws_idx).len());
        // Resolve the cursor by identity first: the catalogue may have been
        // reshaped by the cold PATH walk since the frame that painted it.
        let selected = self.pane_palette_selected().map_or(0, |(idx, _)| idx);
        let picker_open = self
            .pane_palette
            .as_ref()
            .is_some_and(|palette| palette.branch_picker_open);
        match event.keystroke.key.as_str() {
            // The branch menu is a layer over the list, so Escape folds it
            // first and only discards the palette once nothing sits above it.
            "escape" if picker_open => {
                if let Some(palette) = self.pane_palette.as_mut() {
                    palette.branch_picker_open = false;
                }
                cx.notify();
            }
            "escape" => self.close_pane_palette(window, cx),
            "enter" => {
                // Launch the row the cursor names by identity, never the
                // rebuilt list at the painted index (issue #518).
                if let Some((_, preset)) = self.pane_palette_selected() {
                    self.pane_palette_launch(preset, window, cx);
                }
            }
            "up" if selected > 0 && selected < len => {
                self.pane_palette_select(selected - 1, cx);
            }
            "down" if selected + 1 < len => {
                self.pane_palette_select(selected + 1, cx);
            }
            _ => {}
        }
    }

    fn pane_palette_select(&mut self, idx: usize, cx: &mut Context<Self>) {
        let key = self
            .pane_palette_ws_idx()
            .and_then(|ws_idx| self.pane_palette_presets(ws_idx).into_iter().nth(idx))
            .map(|preset| preset.key());
        if let Some(palette) = self.pane_palette.as_mut() {
            palette.selected = idx;
            if let Some(key) = key {
                palette.selected_key = key;
            }
            // Keep the keyboard cursor inside the viewport: the column is
            // taller than its `max_h` as soon as a few agents are visible.
            palette.scroll.scroll_to_item(idx);
            cx.notify();
        }
    }

    pub(crate) fn render_pane_palette(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(palette) = self.pane_palette.as_ref() else {
            return div().into_any_element();
        };
        let ui = crate::theme::ui_colors();
        let presets = self
            .pane_palette_ws_idx()
            .map(|ws_idx| self.pane_palette_presets(ws_idx))
            .unwrap_or_default();

        let title = div()
            .flex_none()
            .pb(px(14.))
            .text_size(px(13.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(ui.text)
            .child(PALETTE_TAB_TITLE);

        let mut buttons = div()
            .id("pane-palette-list")
            .flex()
            .flex_col()
            .gap(px(2.))
            .w(px(PICKER_WIDTH))
            .max_h(px(420.))
            .overflow_y_scroll()
            .track_scroll(&palette.scroll);
        let selected_idx = self.pane_palette_selected().map(|(idx, _)| idx);
        for (idx, preset) in presets.iter().enumerate() {
            buttons = buttons.child(self.render_pane_palette_button(
                idx,
                preset,
                Some(idx) == selected_idx,
                ui,
                cx,
            ));
        }

        let mut column = div().flex().flex_col().items_center().child(title);
        if let Some(row) = self.render_palette_branch_row(palette, ui, cx) {
            column = column.child(row);
        }
        column = column.child(buttons);
        if let Some(error) = &palette.error {
            column = column.child(
                div()
                    .pt(px(10.))
                    .max_w(px(PICKER_WIDTH))
                    .text_size(px(11.))
                    .text_color(ui.vc_deleted)
                    .child(error.clone()),
            );
        }

        // A full-size pane card, filled the way `Pane::render` fills one (a
        // superellipse under the subtree). No hairline: the card is a chooser,
        // not a live surface, so it stays flat until a preset turns it into a
        // real pane.
        div()
            .id("pane-palette")
            .size_full()
            .relative()
            .overflow_hidden()
            .track_focus(&self.pane_palette_focus)
            .on_key_down(cx.listener(Self::handle_pane_palette_key_down))
            .child(squircle_fill(
                crate::app::constants::PANE_CARD_RADIUS,
                crate::theme::active_theme().background,
            ))
            .child(
                div()
                    .relative()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .child(column),
            )
            .into_any_element()
    }

    /// The palette's tab, as `(ws_idx, tab_idx)`. `None` for a split placement,
    /// which fills an existing pane rather than a tab of its own.
    fn pane_palette_tab(&self, palette: &PanePaletteState) -> Option<(usize, usize)> {
        let PalettePlacement::Tab { tab_id } = &palette.placement else {
            return None;
        };
        let ws_idx = self.pane_palette_ws_idx()?;
        let tab_idx = self.workspaces[ws_idx]
            .tabs()
            .iter()
            .position(|tab| tab.id == *tab_id)?;
        Some((ws_idx, tab_idx))
    }

    /// The tab's branch, as a compact select above the presets (issue #347).
    ///
    /// Branches, not worktrees: a worktree is only how git gives a second
    /// branch a directory of its own, and what the user is choosing between is
    /// the branch. Picking one that has no checkout yet makes it - see
    /// [`PaneFlowApp::bind_tab_to_branch`] - under `<workspace>.worktrees/`,
    /// and picking one that has a checkout reuses it.
    ///
    /// A control, not a second list: the options live in a floating menu
    /// anchored under the trigger, so opening it never pushes the presets down
    /// and its rows never read as another column of launch buttons.
    ///
    /// Here rather than only in the tab's context menu because of ordering:
    /// the branch is chosen BEFORE the agent's process starts. A PTY cannot be
    /// moved between checkouts afterwards, so the choice is made while the tab
    /// is still empty and running panes are left alone.
    ///
    /// Only for a tab placement: a split lands in an existing tab, whose own
    /// binding already governs where its panes start. And only inside a
    /// repository - outside one there is nothing to switch between.
    fn render_palette_branch_row(
        &self,
        palette: &PanePaletteState,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (ws_idx, tab_idx) = self.pane_palette_tab(palette)?;
        let ws = self.workspaces.get(ws_idx)?;
        let root = ws.repo_root.clone()?;
        let bound = ws.tabs().get(tab_idx)?.worktree.clone();
        let options = self.branch_options(ws_idx, bound.as_deref(), &root);
        let on_branch = self.tab_branch_label(ws_idx, bound.as_deref());

        // What the trigger names: the branch being checked out while git works,
        // then whatever the tab settled on.
        let current = self
            .branch_checkout_pending
            .clone()
            .or_else(|| {
                options
                    .iter()
                    .find(|option| option.selected)
                    .map(|option| option.label.clone())
            })
            .or(on_branch)
            .unwrap_or_else(|| self.workspace_checkout_label(ws_idx));
        let open = palette.branch_picker_open;
        let fill = with_alpha(ui.text, 0.05);

        let trigger = squircle_skin(
            div()
                .id("palette-branch")
                .flex_none()
                .h(px(28.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .w(px(PICKER_WIDTH)),
            "palette-branch-group",
            ROW_RADIUS,
            open.then_some(fill),
            Some(fill),
        )
        .cursor(CursorStyle::PointingHand)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        // Toggle the render-time snapshot, not the live flag: the menu's
        // `on_mouse_up_out` fires on this same release and has already cleared
        // it, so a live toggle would reopen what the press was closing.
        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
            if let Some(palette) = this.pane_palette.as_mut() {
                palette.branch_picker_open = !open;
                cx.notify();
            }
        }))
        .child(
            svg()
                .size(px(11.))
                .flex_none()
                .path("icons/git-branch-sidebar.svg")
                .text_color(ui.muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(11.))
                .text_color(ui.text)
                .child(current),
        )
        .child(
            svg()
                .size(px(11.))
                .flex_none()
                .path("icons/chevron-down.svg")
                .text_color(ui.muted),
        )
        .when(open, |trigger| {
            let mut menu = select_menu("palette-branch-menu", ui)
                .absolute()
                .top(px(32.))
                .left(px(0.))
                .w(px(PICKER_WIDTH))
                .occlude()
                // Dismiss on release for the reason the sidebar's popover does:
                // the capture-phase `on_mouse_down_out` would close the menu
                // before a row's own click could bubble.
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _w, cx| {
                        if let Some(palette) = this.pane_palette.as_mut() {
                            palette.branch_picker_open = false;
                            cx.notify();
                        }
                    }),
                );
            if options.is_empty() {
                menu = menu.child(
                    div()
                        .h(px(28.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .text_size(px(11.))
                        .text_color(ui.muted)
                        .child("Listing branches..."),
                );
            }
            for option in options {
                menu =
                    menu.child(self.render_palette_branch_option(option, ws_idx, tab_idx, ui, cx));
            }
            trigger.child(
                deferred(crate::ui_primitives::menu_reveal(
                    "pane-palette-branch-menu-reveal",
                    menu,
                ))
                .with_priority(3),
            )
        });

        Some(
            div()
                .flex_none()
                .pb(px(10.))
                .child(trigger)
                .into_any_element(),
        )
    }

    /// The branch a tab works on today: the one its worktree holds, or the
    /// repository's own when it is unbound. `None` for a bound tab whose
    /// worktree the listing has not placed yet (or that is detached).
    fn tab_branch_label(&self, ws_idx: usize, bound: Option<&std::path::Path>) -> Option<String> {
        match bound {
            Some(path) => self
                .workspace_worktree_listing(ws_idx)
                .iter()
                .find(|entry| entry.path == path)
                .and_then(|entry| entry.branch.clone()),
            None => Some(self.workspace_checkout_label(ws_idx)),
        }
    }

    /// Every row the branch select offers for a tab: the repository's local
    /// branches, then its detached checkouts (which have no branch to be
    /// listed under, and dropping them would strand a tab bound to one).
    fn branch_options(
        &self,
        ws_idx: usize,
        bound: Option<&std::path::Path>,
        root: &std::path::Path,
    ) -> Vec<BranchOption> {
        let listing = self.workspace_worktree_listing(ws_idx);
        let on_branch = self.tab_branch_label(ws_idx, bound);
        let mut options: Vec<BranchOption> = self
            .workspace_branches(ws_idx)
            .iter()
            .map(|branch| BranchOption {
                target: BranchTarget::Branch(branch.clone()),
                label: branch.clone(),
                selected: on_branch.as_deref() == Some(branch.as_str()),
                // A branch git already has a directory for costs nothing to
                // switch to; the others are checked out on the spot.
                needs_checkout: !listing
                    .iter()
                    .any(|entry| entry.branch.as_deref() == Some(branch.as_str())),
            })
            .collect();
        options.extend(
            listing
                .iter()
                .filter(|entry| entry.branch.is_none() && !entry.is_bare && entry.path != root)
                .map(|entry| BranchOption {
                    label: crate::workspace::worktree::checkout_label(None, &entry.path, root),
                    target: BranchTarget::Checkout(entry.path.clone()),
                    selected: bound == Some(entry.path.as_path()),
                    needs_checkout: false,
                }),
        );
        options
    }

    /// One branch in the select's menu: its name, a check when the tab is on
    /// it, and otherwise a hint when picking it will have to make a checkout.
    fn render_palette_branch_option(
        &self,
        option: BranchOption,
        ws_idx: usize,
        tab_idx: usize,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let BranchOption {
            target,
            label,
            selected,
            needs_checkout,
        } = option;
        let id = SharedString::from(format!("palette-branch-{label}"));
        select_item(id, selected, ui)
            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                match target.clone() {
                    BranchTarget::Branch(branch) => {
                        this.bind_tab_to_branch(ws_idx, tab_idx, branch, cx)
                    }
                    BranchTarget::Checkout(path) => {
                        this.bind_tab_to_checkout(ws_idx, tab_idx, path, cx);
                    }
                }
                if let Some(palette) = this.pane_palette.as_mut() {
                    palette.branch_picker_open = false;
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(ui.text)
                    .child(label),
            )
            // One trailing slot, reserved either way so the names stay on one
            // left edge: the check for where the tab is, and for a branch with
            // no directory yet the folder this will make.
            .child(div().w(px(13.)).flex_none().child(if selected {
                svg()
                    .size(px(13.))
                    .path("icons/check.svg")
                    .text_color(ui.text)
                    .into_any_element()
            } else if needs_checkout {
                svg()
                    .size(px(13.))
                    .path("icons/folder.svg")
                    .text_color(with_alpha(ui.muted, 0.7))
                    .into_any_element()
            } else {
                div().size(px(13.)).into_any_element()
            }))
            .into_any_element()
    }

    /// One plain preset button: icon, label, and nothing that folds open.
    fn render_pane_palette_button(
        &self,
        idx: usize,
        preset: &Preset,
        is_selected: bool,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A render frame reads the non-blocking snapshot; only the confirm
        // (`ensure_launchable`) waits for the cold PATH walk (issue #518).
        let launchable = preset.looks_launchable();
        let icon_path = preset.icon_path();
        let icon = if preset.icon_multicolor() {
            gpui::img(icon_path)
                .size(px(14.))
                .flex_none()
                .into_any_element()
        } else {
            svg()
                .size(px(14.))
                .flex_none()
                .path(icon_path)
                .text_color(preset.accent().map_or(ui.text, |c| gpui::rgb(c).into()))
                .into_any_element()
        };

        let mut button = select_item(
            SharedString::from(format!("pane-palette-row-{idx}")),
            is_selected,
            ui,
        )
        .cursor(CursorStyle::PointingHand)
        .gap(px(8.))
        .h(px(34.))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener({
            // Issue #518: launch the row the user saw, not whatever sits at
            // this index after the cold scan reshapes the catalogue.
            let preset = preset.clone();
            move |this, _: &ClickEvent, window, cx| {
                this.pane_palette_launch(preset.clone(), window, cx);
                cx.stop_propagation();
            }
        }))
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(if launchable { ui.text } else { ui.muted })
                .child(preset.label.clone()),
        );

        if !launchable {
            button = button.child(
                div()
                    .flex_none()
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    // Issue #518: the first PATH walk has not published yet.
                    .child(if crate::agent_launcher::installed_binary_scan_pending() {
                        "looking"
                    } else {
                        "not installed"
                    }),
            );
        }

        button.into_any_element()
    }
}

/// Where `key` sits in `presets`, or `None` when that row is gone. Never a
/// positional fallback: a row that vanished must not resolve to whatever
/// took its place. Pure so the reshaped-catalogue case is testable without
/// a `PaneFlowApp`.
fn resolve_selected(presets: &[Preset], key: &PresetKey) -> Option<usize> {
    presets.iter().position(|preset| preset.key() == *key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #518: a row highlighted before the cold PATH walk publishes must
    /// still be the row Enter launches once the walk inserts agent rows, and
    /// a row that vanished resolves to nothing rather than to the row that
    /// took its index.
    #[test]
    fn keyboard_cursor_keeps_the_row_when_the_cold_scan_inserts_agents() {
        let shell = Preset {
            label: "Terminal".into(),
            source: PresetSource::Shell,
        };
        let claude = Preset {
            label: "Claude Code".into(),
            source: PresetSource::Agent(TerminalAgent::ClaudeCode),
        };
        let codex = Preset {
            label: "Codex".into(),
            source: PresetSource::Agent(TerminalAgent::Codex),
        };
        let before = vec![shell.clone(), claude.clone()];
        let key = claude.key();
        assert_eq!(resolve_selected(&before, &key), Some(1));

        let after = vec![shell.clone(), codex.clone(), claude.clone()];
        assert_eq!(
            resolve_selected(&after, &key),
            Some(2),
            "the inserted agent row must not steal the cursor"
        );
        // An agent highlighted while it read `looking` and then dropped by
        // the walk: Enter must be inert, not launch the row at its index.
        let vanished = codex.key();
        assert_eq!(
            resolve_selected(&[shell.clone(), claude.clone()], &vanished),
            None
        );
        assert_eq!(resolve_selected(&[], &key), None);
    }

    #[test]
    fn tab_needs_palette_matches_the_open_tab_with_surface_guard() {
        // Paneless, no picker: restore, new workspace, last pane closed.
        assert!(tab_needs_palette(true, true, false));
        // Live tree.
        assert!(!tab_needs_palette(false, true, false));
        // Zoomed: the real tree sits in `saved_layout`.
        assert!(!tab_needs_palette(true, false, false));
        // Picker already owns this tab.
        assert!(!tab_needs_palette(true, true, true));
        assert!(!tab_needs_palette(false, false, false));
        assert!(!tab_needs_palette(false, false, true));
    }

    #[test]
    fn palette_opens_history_only_for_tab_placement_when_setting_and_agents_enabled() {
        assert!(
            !palette_should_open_sessions(false, true, true),
            "setting off: identical to today"
        );
        assert!(
            !palette_should_open_sessions(true, false, true),
            "no session-capable agent: nothing to list"
        );
        assert!(
            !palette_should_open_sessions(true, true, false),
            "split-placement pickers leave the sidebar alone"
        );
        assert!(palette_should_open_sessions(true, true, true));
    }

    #[test]
    fn palette_bound_history_survives_workspace_activation() {
        let bound = Some((1u64, 2u64));
        let picker = Some((1u64, 2u64));
        // Empty New pane tab has no leaf: Cmd+1 / already-selected row must
        // not close history, or the dismiss latch would refuse to reopen it.
        // WaitingElseFirst may then switch to a live agent tab: retargeting
        // would clear the binding so a later click resumes into that agent.
        assert!(
            palette_bound_sessions_survives_activation(bound, picker),
            "activation must not close or retarget while the picker is bound"
        );
        assert!(!palette_bound_sessions_survives_activation(None, picker));
        assert!(!palette_bound_sessions_survives_activation(bound, None));
        assert!(!palette_bound_sessions_survives_activation(
            bound,
            Some((1, 99))
        ));
    }

    #[test]
    fn palette_holds_last_surface_only_on_the_sole_paneless_tab() {
        // The picker's tab is the workspace's only tab and has no pane.
        assert!(palette_holds_last_surface(Some(7), &[(7, true)]));
        // A second tab survives the close.
        assert!(!palette_holds_last_surface(
            Some(7),
            &[(7, true), (8, false)]
        ));
        assert!(!palette_holds_last_surface(
            Some(7),
            &[(8, false), (7, true)]
        ));
        // The picker's tab has a live tree.
        assert!(!palette_holds_last_surface(Some(7), &[(7, false)]));
        // The sole tab is not the picker's.
        assert!(!palette_holds_last_surface(Some(7), &[(9, true)]));
        // Split placement or no picker.
        assert!(!palette_holds_last_surface(None, &[(7, true)]));
        assert!(!palette_holds_last_surface(None, &[]));
    }

    /// Issue #518: the cold PATH walk inserts agent rows once it publishes,
    /// so a launch must carry the `Preset` the user saw rather than
    /// re-resolve a painted index, and the row painter must read the
    /// non-blocking snapshot, never the confirm's blocking `ensure_launchable`.
    #[test]
    fn click_keeps_the_row_when_the_cold_scan_inserts_agents() {
        let src = include_str!("pane_palette.rs");
        assert!(
            src.contains("fn pane_palette_launch(&mut self, preset: Preset,"),
            "pane_palette_launch takes the preset by value, not an index"
        );
        let row = src
            .split("fn render_pane_palette_button(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("render_pane_palette_button exists");
        assert!(
            row.contains("this.pane_palette_launch(preset.clone(), window, cx)"),
            "the click launches the captured preset: {row}"
        );
        assert!(
            !row.contains("pane_palette_launch(idx"),
            "the click must not re-resolve the painted index: {row}"
        );
        assert!(
            row.contains("preset.looks_launchable()") && !row.contains("ensure_launchable()"),
            "a render frame reads the non-blocking snapshot: {row}"
        );
        let launch = src
            .split("pub(crate) fn pane_palette_launch(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("pane_palette_launch exists");
        let bound = launch
            .find("tab.id == tab_id && tab.root.is_none() && tab.saved_layout.is_none()")
            .expect("the Tab arm binds the launch to the placement's own paneless tab");
        let fill = launch
            .find("self.open_tab_with_surface(ws_idx")
            .expect("the Tab arm fills the tab");
        assert!(
            bound < fill,
            "the tab check runs before the fill, so a replayed launch cannot land in another tab: {launch}"
        );
        let keys = src
            .split("pub(crate) fn handle_pane_palette_key_down(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("handle_pane_palette_key_down exists");
        assert!(
            keys.contains("self.pane_palette_selected()") && !keys.contains(".nth(selected)"),
            "Enter resolves the cursor by identity, never by the painted index: {keys}"
        );
        let looks = src
            .split("fn looks_launchable(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("looks_launchable exists");
        assert!(
            looks.contains("agent.is_installed()") && !looks.contains("is_installed_now"),
            "looks_launchable never blocks: {looks}"
        );
    }

    /// Issue #518: confirm must not wait for the first PATH walk on the GPUI
    /// thread, and an Enter during the walk is queued for the boot warm.
    #[test]
    fn confirm_does_not_block_on_the_cold_walk_and_is_replayed_when_it_lands() {
        let src = include_str!("pane_palette.rs");
        let launchable = src
            .split("fn ensure_launchable(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("ensure_launchable exists");
        assert!(
            !launchable.contains("is_installed_now()"),
            "ensure_launchable must not wait on the GPUI thread: {launchable}"
        );
        let launch = src
            .split("pub(crate) fn pane_palette_launch(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("pane_palette_launch exists");
        assert!(
            launch.contains("palette.launch_queued = Some(preset.clone());"),
            "pane_palette_launch queues a launch made during the walk: {launch}"
        );

        let boot = include_str!("bootstrap.rs");
        let warm = boot
            .split("smol::unblock(crate::agent_launcher::refresh_installed_binaries).await;")
            .nth(1)
            .and_then(|rest| rest.split(".detach();").next())
            .expect("the boot warm awaits the first walk");
        assert!(
            warm.contains("app.pane_palette_resume_queued_launch(cx);"),
            "the boot warm's completion must replay queued launches: {warm}"
        );
    }

    /// `self.pane_palette.take()`, or the picker is dropped even when the
    /// close is refused.
    #[test]
    fn close_pane_palette_checks_the_last_surface_guard_before_taking_the_picker() {
        let src = include_str!("pane_palette.rs");
        let body_start = src
            .find("fn close_pane_palette(")
            .expect("close_pane_palette exists");
        let body = &src[body_start..];
        let guard = body
            .find("self.pane_palette_holds_last_surface()")
            .expect("close_pane_palette calls the last-surface guard");
        let take = body
            .find("self.pane_palette.take()")
            .expect("close_pane_palette takes the picker");
        assert!(
            guard < take,
            "the last-surface guard must run before the picker is taken"
        );
    }

    #[test]
    fn main_rs_does_not_render_no_terminal_panes_open() {
        let src = include_str!("../main.rs");
        let forbidden = ["No terminal panes", "open"].join(" ");
        assert!(
            !src.contains(&forbidden),
            "a paneless tab must render the picker, not a dead-end message"
        );
    }

    /// Issue #549: a repo without `main` must still open a New pane tab on the
    /// workspace checkout, and the toast must keep git's reason.
    #[test]
    fn missing_new_tab_branch_opens_on_the_workspace_checkout_and_keeps_gits_reason() {
        assert!(
            new_tab_checkout_unresolved("Local branch main does not exist"),
            "prepare_branch_checkout's missing-branch error"
        );
        assert!(new_tab_checkout_unresolved(
            "git worktree add /tmp/repo.worktrees/main main failed: fatal: invalid reference: main"
        ));
        assert!(new_tab_checkout_unresolved(
            "git rev-parse --verify refs/heads/main failed: fatal: ambiguous argument 'main': unknown revision or path not in the working tree."
        ));
        assert!(new_tab_checkout_unresolved(
            "Not a literal branch name: HEAD"
        ));
        assert!(
            !new_tab_checkout_unresolved(
                "/tmp/repo.worktrees/main exists but is not a registered worktree; remove it first"
            ),
            "a real checkout collision must not silently fall back"
        );
        assert!(!new_tab_checkout_unresolved(
            "/tmp/repo.worktrees/main exists but holds another branch (feat)"
        ));
        let toast = new_tab_checkout_failure_toast("main", "Local branch main does not exist");
        assert!(
            toast.contains("Local branch main does not exist"),
            "toast must include git's reason: {toast}"
        );
        assert!(toast.contains("Settings → Workspaces"), "{toast}");

        let src = include_str!("pane_palette.rs");
        let start = src
            .find("pub(crate) fn open_pane_palette(")
            .expect("open_pane_palette");
        let rest = &src[start..];
        let end = rest
            .find("\n    fn open_pane_palette_at_checkout(")
            .expect("open_pane_palette_at_checkout follows");
        let err = rest[..end]
            .split("Err(error) => {")
            .nth(1)
            .expect("Err arm");
        assert!(
            err.contains("new_tab_checkout_failure_toast"),
            "the Err arm must surface git's reason"
        );
        assert!(
            err.contains("new_tab_checkout_unresolved"),
            "the Err arm must classify a missing branch"
        );
        assert!(
            err.contains("open_pane_palette_at_checkout(ws_idx, None, window, cx)"),
            "an unresolved branch must open the picker on the workspace checkout"
        );
    }
}
