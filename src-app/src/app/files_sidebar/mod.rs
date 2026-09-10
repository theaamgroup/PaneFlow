//! Docked Files right sidebar (PRD `prd-files-tree-sidebar-2026-Q3`, EP-001).
//!
//! Mirrors the agent-sessions sidebar (`sessions_sidebar.rs`): a
//! `flex_shrink_0` child of the root `flex_row`, toggled by the
//! `toggle_files_sidebar` action (`secondary-alt-f`), mutually exclusive with
//! the sessions sidebar (one right column). The pane header carries no Files
//! button: the tree is keyboard/command-driven only. Renders a lazily-expanded,
//! folders-first tree of the active workspace's `cwd`. Every file opens in
//! the diff dock's editor, and
//! markdown is no longer the exception: a `.md` row reads as source there like
//! any other file rather than opening a rendered pane of its own (rendered
//! Markdown panes still come from an OSC path click and from session
//! restore). Only editor-refused files (binary or over `MAX_FILE_BYTES`) stay
//! muted; gitignored/hidden entries are filtered out before rendering. Rows
//! carry no drag: the markdown drag-to-pane is gone, so a click is the
//! sidebar's only gesture and the dock editor its only destination.
//!
//! Wanting the rail belongs to the workspace tab that asked for it
//! (`Tab::files_sidebar_open`); the app-level `files_sidebar_open` is a live
//! mirror of the visible tab's flag, reconciled by `sync_files_sidebar_session`.
//! The rail is hosted by the CLI cockpit only: Review and Settings unmount its
//! *element* (`files_sidebar_host_visible`), so no focus or keyboard work
//! happens there, while the panel entity, its worker thread and its watches
//! stay warm underneath - which is why coming back is instant.
//!
//! The tree follows Zed's project-panel shape (issue #430, upstream
//! `d6a44bfc`): `worker.rs` owns the directory snapshot and the non-recursive
//! `notify` watches on a dedicated OS thread, `projection.rs` turns a snapshot
//! plus fold state plus filter query into the ordered rows on the background
//! executor, and `panel.rs` is the `FilesSidebar` entity that owns the current
//! projection, selection, focus and scroll handle. `view.rs` / `row.rs` render
//! only the rows `uniform_list` asks for. This module holds the app-side
//! placement: open/close, width animation, workspace association, and the
//! per-tab reconciliation; `integration.rs` turns the panel's events into dock
//! and session work.

mod context_menu;
mod filter;
mod integration;
mod keyboard;
mod list;
mod panel;
mod projection;
mod row;
mod view;
mod watch;
mod worker;

use std::path::PathBuf;

use gpui::{Context, Focusable, Pixels, Window, px};

use paneflow_config::schema::{AppMode, FilesTreePlacement};

use crate::{PaneFlowApp, ToggleFilesSidebar};
pub(crate) use panel::{FilesEvent, FilesSidebar};

/// Fixed sidebar width - matches the sessions sidebar (a resizable width is
/// deferred per the PRD non-goals).
pub(crate) const FILES_SIDEBAR_WIDTH: f32 = 300.;
pub(super) const SIDEBAR_WIDTH: Pixels = px(FILES_SIDEBAR_WIDTH);
pub(crate) const DOCK_TREE_WIDTH: f32 = 250.;

/// Hide only the tree when its fixed width would leave less than 200 px for
/// the editor. Neither this calculation nor either mount writes the dock width.
pub(crate) fn dock_tree_width(open: bool, file_active: bool, dock_width: f32) -> f32 {
    if open && file_active && dock_width >= DOCK_TREE_WIDTH + 200. {
        DOCK_TREE_WIDTH
    } else {
        0.
    }
}
/// File placeholders count as file tabs until a source document replaces them.
pub(crate) fn first_file_tab(tabs: &[crate::app::diff_dock::DiffDockTab]) -> Option<usize> {
    tabs.iter().position(|tab| {
        matches!(
            tab,
            crate::app::diff_dock::DiffDockTab::File(_)
                | crate::app::diff_dock::DiffDockTab::PendingFile
        )
    })
}

pub(crate) fn closes_with_last_file(
    placement: FilesTreePlacement,
    tabs: &[crate::app::diff_dock::DiffDockTab],
) -> bool {
    placement == FilesTreePlacement::Dock && first_file_tab(tabs).is_none()
}

/// Tree geometry, measured off the Codex file tree so the two read as the same
/// widget: 28px rows, an 18px indent step, one 14px leading slot (chevron for a
/// directory, language icon for a file) and a 12px gap before the name.
pub(super) const ROW_HEIGHT: Pixels = px(28.);
/// Per-depth indentation added to the row's left padding.
pub(super) const INDENT_STEP: f32 = 18.;
/// Width of the single leading slot. A directory fills it with its chevron, a
/// file with its language icon; both therefore start on the same pixel.
pub(super) const ROW_SLOT: f32 = 14.;
/// Gap between that slot and the name.
pub(super) const ROW_GAP: f32 = 12.;
/// Extra opacity knock-down for gitignored / hidden rows (US-004 second tier).
pub(super) const DIMMED_OPACITY: f32 = 0.55;

/// Whether the surface that hosts the Files rail is on screen: the CLI cockpit
/// with Settings closed. The tree's rows open into the dock's editor, which
/// only exists there, so Review (`AppMode::Diff`) and Settings unmount the
/// rail's element instead of painting a tree whose clicks would land nowhere.
/// Only the element: the panel entity and its worker keep running underneath
/// (see `PaneFlowApp::files_sidebar_host_visible`).
pub(crate) fn files_rail_host_visible(settings_open: bool, mode: AppMode) -> bool {
    !settings_open && matches!(mode, AppMode::Cli)
}

/// What reconciling the live rail with the visible tab has to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilesSidebarSync {
    /// The visible tab wants the rail and it is down: open it. Opening
    /// roots the panel on the active workspace's `cwd`, so no re-root follows.
    Open,
    /// The visible tab does not want the rail and it is up: close it.
    Close,
    /// Tab and rail agree; only the root can be stale (a workspace switch
    /// between two tabs that both want the tree), so re-root if it moved.
    Reroot,
}

/// The one decision `sync_files_sidebar_session` makes, kept pure so the
/// truth table is testable without a window.
pub(crate) fn files_sidebar_sync_step(tab_wants_open: bool, rail_open: bool) -> FilesSidebarSync {
    match (tab_wants_open, rail_open) {
        (true, false) => FilesSidebarSync::Open,
        (false, true) => FilesSidebarSync::Close,
        _ => FilesSidebarSync::Reroot,
    }
}

impl PaneFlowApp {
    /// Release keyboard ownership when the entire dock host is unmounted.
    pub(crate) fn blur_unmounted_files_tree(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.files_tree_in_dock()
            && self
                .files_sidebar
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            window.blur();
            if self.files_sidebar_host_visible()
                && let Some(pane) = self.focused_or_first_pane(window, cx)
            {
                pane.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
    }

    pub(crate) fn files_tree_in_dock(&self) -> bool {
        self.cached_config.files_tree_placement == FilesTreePlacement::Dock
    }

    pub(crate) fn diff_file_tab_active(&self) -> bool {
        self.diff_dock
            .diff_tabs
            .get(self.diff_dock.diff_active_tab)
            .is_some_and(|tab| {
                matches!(
                    tab,
                    crate::app::diff_dock::DiffDockTab::File(_)
                        | crate::app::diff_dock::DiffDockTab::PendingFile
                )
            })
    }

    /// Whether the surface that hosts the Files rail is on screen, whatever
    /// `files_sidebar_open` says.
    ///
    /// The open flag alone is not enough, exactly like the diff dock's own: it
    /// survives a mode switch and a trip through Settings. The tree belongs to
    /// the CLI cockpit - its rows open into the dock's editor, which does not
    /// exist on the full-screen Review surface or behind Settings - so `render`
    /// unmounts the rail's element off the cockpit and the surviving flag is
    /// what brings the same tree back on return.
    ///
    /// Unmounting is the element and its focus/keyboard handling, nothing
    /// more: the `FilesSidebar` entity stays active, so its `files-tree`
    /// worker thread, the `notify` watches it registered and any in-flight
    /// scan or projection keep running (`worker.rs`, `watch.rs`), which is
    /// why the return is instant rather than a re-scan. Only
    /// `close_files_sidebar` deactivates them. Also gates the toggle, so the
    /// chord cannot flip a rail the user cannot see.
    pub(crate) fn files_sidebar_host_visible(&self) -> bool {
        files_rail_host_visible(self.settings_section.is_some(), self.mode)
    }

    /// Toggle the Files sidebar. Opening resolves the active workspace's `cwd`
    /// to the tree root, starts the worker on it, and closes the sessions
    /// sidebar (mutual exclusion). Re-clicking closes and releases the tree.
    pub(crate) fn handle_toggle_files_sidebar(
        &mut self,
        _: &ToggleFilesSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Inert off the cockpit: the rail's element is unmounted there, so a
        // toggle would only flip a flag the user cannot see.
        if !self.files_sidebar_host_visible() {
            return;
        }
        self.sync_diff_dock_session(cx);
        self.sync_files_sidebar_session(cx);
        if self.files_tree_in_dock()
            && (!self.files_sidebar_open || !self.diff_file_tab_active() || !self.diff_dock.open)
        {
            if !self.diff_dock.open {
                let Some(ws) = self.active_workspace() else {
                    return;
                };
                self.open_diff_dock_panel(ws.cwd.clone(), cx);
            }
            if !self.diff_file_tab_active() {
                if let Some(index) = first_file_tab(&self.diff_dock.diff_tabs) {
                    self.select_diff_tab(index, cx);
                } else {
                    self.open_diff_file_picker(window, cx);
                    return;
                }
            }
            // The tree may already be wanted but hidden behind another dock
            // tab (or a closed dock). Showing that file must not close it.
            if self.files_sidebar_open {
                self.focus_files_sidebar(window, cx);
                return;
            }
        }
        self.toggle_files_sidebar(cx);
        if self.files_sidebar_open {
            self.focus_files_sidebar(window, cx);
        } else if self.files_tree_in_dock() {
            self.focus_diff_tab(self.diff_dock.diff_active_tab, window, cx);
        }
    }

    pub(crate) fn focus_files_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.files_sidebar
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    pub(crate) fn toggle_files_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.files_sidebar_open {
            self.close_files_sidebar(cx);
            return;
        }
        let Some(ws) = self.workspaces.get(self.active_idx) else {
            return;
        };
        let root = PathBuf::from(&ws.cwd);
        // US-007: restore this workspace's expansion (held on the Workspace,
        // so it survives a previous close within the session and a restart).
        let persisted = ws.files_expanded.clone();
        self.files_sidebar_workspace = Some(ws.id);
        // Mutual exclusion: only one right column is ever visible.
        if !self.files_tree_in_dock()
            && (self.agent_sessions.sessions_sidebar_open
                || self.agent_sessions.sessions_sidebar_animation.is_some())
        {
            self.close_sessions_sidebar_immediate(cx);
        }
        // Floating dropdowns would paint over the docked panel.
        self.dismiss_transient_surfaces();
        self.set_files_sidebar_open(true, cx);
        self.files_sidebar_root = Some(root.clone());
        self.files_sidebar
            .update(cx, |panel, cx| panel.open(root, persisted, cx));
    }

    /// Close the sidebar: deactivate the panel (its worker thread, watches
    /// and pending projections stop) and release the snapshot once the
    /// closing animation has finished with it. The per-workspace expansion
    /// lives on the `Workspace`, so it is NOT reset here (US-007) - reopening
    /// restores it.
    pub(crate) fn close_files_sidebar(&mut self, cx: &mut Context<Self>) {
        self.files_sidebar.update(cx, |panel, _| panel.deactivate());
        // Close any open row context menu so it can't outlive the tree.
        self.files_menu_open = None;
        self.files_sidebar_root = None;
        self.files_sidebar_workspace = None;
        self.set_files_sidebar_open(false, cx);
        if self.files_sidebar_animation.is_none() {
            self.files_sidebar
                .update(cx, |panel, cx| panel.release_snapshot(cx));
        }
    }

    fn files_sidebar_width_at(&self, now: std::time::Instant) -> f32 {
        if let Some(animation) = self.files_sidebar_animation {
            animation.width_at(now)
        } else if self.files_sidebar_open {
            FILES_SIDEBAR_WIDTH
        } else {
            0.
        }
    }

    pub(crate) fn rendered_files_sidebar_width(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> f32 {
        let now = std::time::Instant::now();
        if let Some(animation) = self.files_sidebar_animation {
            if animation.is_finished(now) {
                self.files_sidebar_animation = None;
                if !self.files_sidebar_open {
                    self.files_sidebar
                        .update(cx, |panel, cx| panel.release_snapshot(cx));
                }
                animation.to_width
            } else {
                window.request_animation_frame();
                animation.width_at(now)
            }
        } else if self.files_sidebar_open {
            FILES_SIDEBAR_WIDTH
        } else {
            0.
        }
    }

    fn set_files_sidebar_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        let from_width = self.files_sidebar_width_at(now);
        self.files_sidebar_open = open;
        // The rail is one app-level surface, but wanting it belongs to the
        // session looking at it. Recording that here - the single funnel every
        // open and close goes through - is what keeps a sibling tab from
        // inheriting a tree it never asked for.
        if let Some(ws) = self.active_workspace_mut() {
            ws.active_tab_mut().files_sidebar_open = open;
        }
        let to_width = if open { FILES_SIDEBAR_WIDTH } else { 0. };
        self.files_sidebar_animation =
            if (from_width - to_width).abs() > crate::PRIMARY_SIDEBAR_MIN_ANIMATION_DELTA {
                Some(crate::SidebarWidthAnimation {
                    from_width,
                    to_width,
                    started_at: now,
                })
            } else {
                None
            };
        cx.notify();
    }

    /// Reconcile the live rail with the session (workspace tab) on screen.
    ///
    /// Two things can be stale after a session change: whether the rail should
    /// be up at all (the visible tab's own flag) and, when both sessions want
    /// it, which `cwd` it is rooted on. Idempotent and cheap on the steady
    /// path, so `render` can call it every frame - which is what makes this
    /// correct without every tab mutation (switch, close, reorder, cross-
    /// workspace move) having to remember the rail exists. Inert off the
    /// cockpit, where the rail is unmounted anyway: the live state is left
    /// alone there and reconciled on the way back.
    pub(crate) fn sync_files_sidebar_session(&mut self, cx: &mut Context<Self>) {
        if !self.files_sidebar_host_visible() {
            return;
        }
        self.sync_diff_dock_session(cx);
        // A hot reload into rail mode restores the one-right-column contract.
        if !self.files_tree_in_dock()
            && self.files_sidebar_open
            && self.agent_sessions.sessions_sidebar_open
        {
            self.close_sessions_sidebar_immediate(cx);
        }
        let wanted = self
            .active_workspace()
            .is_some_and(|ws| ws.active_tab().files_sidebar_open);
        match files_sidebar_sync_step(wanted, self.files_sidebar_open) {
            // Opening roots the panel on the active workspace's `cwd`, so the
            // re-root has nothing left to do on this path.
            FilesSidebarSync::Open => self.toggle_files_sidebar(cx),
            FilesSidebarSync::Close => self.close_files_sidebar(cx),
            FilesSidebarSync::Reroot => self.reroot_files_tree(cx),
        }
    }

    /// Re-root the panel on the active workspace's `cwd` when it changed while
    /// the sidebar is open (US-002 workspace-switch). No-op when closed or when
    /// the root and workspace are unchanged. Restores the new workspace's
    /// expansion (US-007) and restarts the worker on the new root (US-005).
    pub(crate) fn reroot_files_tree(&mut self, cx: &mut Context<Self>) {
        if !self.files_sidebar_open {
            return;
        }
        let Some(ws) = self.workspaces.get(self.active_idx) else {
            return;
        };
        let root = PathBuf::from(&ws.cwd);
        if self.files_sidebar_root.as_ref() == Some(&root)
            && self.files_sidebar_workspace == Some(ws.id)
        {
            return;
        }
        let persisted = ws.files_expanded.clone();
        self.files_sidebar_workspace = Some(ws.id);
        self.files_sidebar_root = Some(root.clone());
        self.files_menu_open = None;
        self.files_sidebar
            .update(cx, |panel, cx| panel.open(root, persisted, cx));
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fork_tests {
    use paneflow_config::schema::AppMode;

    use super::{FilesSidebarSync, files_rail_host_visible, files_sidebar_sync_step};
    use crate::source_probe::source_slice;

    /// The production half of a source file: everything before its test
    /// module. Files without one come back whole.
    fn production(src: &str) -> &str {
        src.split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production source")
    }

    /// The text between two unique markers, so an assertion pins one function
    /// body instead of the whole file.
    fn between<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
        source_slice(src, start, end)
    }

    fn app_render() -> &'static str {
        between(
            include_str!("../../main.rs"),
            "impl Render for PaneFlowApp {",
            "fn register_focus_lost_fallback",
        )
    }

    /// #184 Phase 4: the rail lives on the CLI cockpit only. Review and
    /// Settings unmount its element; the panel and its worker stay warm.
    #[test]
    fn files_rail_is_hosted_only_by_the_cli_cockpit_without_settings() {
        assert!(files_rail_host_visible(false, AppMode::Cli));
        assert!(
            !files_rail_host_visible(true, AppMode::Cli),
            "Settings unmounts the rail"
        );
        assert!(
            !files_rail_host_visible(false, AppMode::Diff),
            "Review unmounts the rail"
        );
        assert!(!files_rail_host_visible(true, AppMode::Diff));
    }

    #[test]
    fn session_sync_follows_the_visible_tabs_flag() {
        assert_eq!(files_sidebar_sync_step(true, false), FilesSidebarSync::Open);
        assert_eq!(
            files_sidebar_sync_step(false, true),
            FilesSidebarSync::Close
        );
        assert_eq!(
            files_sidebar_sync_step(true, true),
            FilesSidebarSync::Reroot
        );
        assert_eq!(
            files_sidebar_sync_step(false, false),
            FilesSidebarSync::Reroot,
            "a closed rail nobody wants only needs the (no-op) re-root check"
        );
    }

    /// Two tabs must not share the open state: the ONE funnel every open and
    /// close goes through records the flag on the active tab, and `render`
    /// reconciles the live rail from that flag every frame - which is what
    /// makes a tab switch, close, reorder or cross-workspace move correct
    /// without each of them remembering the rail exists.
    #[test]
    fn open_state_is_recorded_on_the_active_tab_and_reconciled_every_frame() {
        let sidebar = production(include_str!("mod.rs"));
        let set_open = between(
            sidebar,
            "fn set_files_sidebar_open(",
            "pub(crate) fn sync_files_sidebar_session(",
        );
        assert!(
            set_open.contains("ws.active_tab_mut().files_sidebar_open = open;"),
            "the open/close funnel must record the flag on the active tab: {set_open}"
        );
        let sync = between(
            sidebar,
            "pub(crate) fn sync_files_sidebar_session(",
            "pub(crate) fn reroot_files_tree(",
        );
        assert!(
            sync.contains("ws.active_tab().files_sidebar_open"),
            "reconciliation reads the visible tab's own flag: {sync}"
        );
        assert!(
            sync.contains("files_sidebar_sync_step("),
            "reconciliation goes through the tested decision helper: {sync}"
        );
        assert!(
            app_render().contains("self.sync_files_sidebar_session(cx);"),
            "render must reconcile the rail with the visible tab every frame"
        );
    }

    /// The Settings and Review surfaces do not render the Files rail's
    /// element at all - no focus or keyboard work happens there - and the
    /// chord cannot flip a rail the user cannot see. Only the element goes:
    /// the panel entity stays active, so its worker thread and its watches
    /// stay warm (`worker.rs`), which is why the rail is back instantly on
    /// return and why `close_files_sidebar` is the only thing that
    /// deactivates them. This pins the mount gate and the deactivation
    /// chokepoint; it does not claim the watcher stops.
    #[test]
    fn review_and_settings_unmount_the_files_rail() {
        let render = app_render();
        assert!(
            render.contains("let files_sidebar_host_visible = self.files_sidebar_host_visible();"),
            "render must ask whether the rail's host is on screen"
        );
        assert!(
            render.contains(
                "let files_sidebar_mounted = files_sidebar_host_visible\n            \
                 && (self.files_sidebar_open || self.files_sidebar_animation.is_some());"
            ),
            "the rail mounts only on the cockpit, whatever the open flag says"
        );
        assert!(
            render.contains(
                "if files_sidebar_host_visible {\n            self.sync_files_sidebar_session(cx);"
            ),
            "off the cockpit the live state is left alone and reconciled on return"
        );

        let sidebar = production(include_str!("mod.rs"));
        let host = between(
            sidebar,
            "pub(crate) fn files_sidebar_host_visible(",
            "pub(crate) fn handle_toggle_files_sidebar(",
        );
        assert!(
            host.contains("files_rail_host_visible(self.settings_section.is_some(), self.mode)"),
            "the app-level check must be the tested truth table: {host}"
        );
        let toggle = between(
            sidebar,
            "pub(crate) fn handle_toggle_files_sidebar(",
            "pub(crate) fn focus_files_sidebar(",
        );
        assert!(
            toggle.contains("if !self.files_sidebar_host_visible() {\n            return;"),
            "the chord is inert off the cockpit: {toggle}"
        );
        // Warm on unmount: the only production caller of `panel.deactivate()`
        // is the close path, so leaving the cockpit never stops the worker.
        let deactivations = sidebar.matches("panel.deactivate()").count();
        assert_eq!(
            deactivations, 1,
            "mod.rs must deactivate the panel from close_files_sidebar only"
        );
        let close = between(
            sidebar,
            "pub(crate) fn close_files_sidebar(",
            "fn files_sidebar_width_at(",
        );
        assert!(
            close.contains("panel.deactivate()"),
            "closing the rail is what stops the worker: {close}"
        );
    }

    /// A `.md` row is a file like any other: a click (or Enter) opens it as
    /// source in the dock editor, and rows carry no drag. The markdown
    /// drag-to-pane path is gone from the row, the payload, the pane's drop
    /// targets and the app's event handler. The panel emits
    /// `FilesEvent::OpenFile` for every file and `integration.rs` turns that
    /// into `open_diff_file_tab`, so there is exactly one destination.
    #[test]
    fn markdown_rows_open_as_source_in_the_dock_editor_and_carry_no_drag() {
        let row = production(include_str!("row.rs"));
        for forbidden in [
            "MarkdownFileDrag",
            ".on_drag(",
            "is_markdown",
            "open_markdown_in_active_pane",
        ] {
            assert!(
                !row.contains(forbidden),
                "row.rs must not contain {forbidden:?}"
            );
        }
        assert!(
            row.contains("this.activate_path(&click_path, is_dir, window, cx);"),
            "a row click goes through the one activation path"
        );

        let keyboard = production(include_str!("keyboard.rs"));
        for forbidden in ["is_markdown", "open_markdown_in_active_pane"] {
            assert!(
                !keyboard.contains(forbidden),
                "keyboard.rs must not contain {forbidden:?}"
            );
        }
        let activate = between(
            keyboard,
            "pub(super) fn activate_path(",
            "pub(super) fn clear_files_filter(",
        );
        assert!(
            activate.contains("cx.emit(FilesEvent::OpenFile {"),
            "every file row (Enter or click) emits OpenFile: {activate}"
        );
        assert!(
            keyboard.contains("self.activate_path(&row.node.path, row.node.is_dir, window, cx)"),
            "Enter on a row goes through the same activation path as a click"
        );

        let integration = production(include_str!("integration.rs"));
        let open = between(
            integration,
            "fn open_file_in_diff_dock(",
            "pub(crate) fn render_files_sidebar(",
        );
        assert!(
            open.contains("self.open_diff_file_tab(path, window, cx);"),
            "OpenFile lands in the dock editor as source: {open}"
        );
        assert!(
            !integration.contains("open_markdown_in_active_pane"),
            "the sidebar no longer opens rendered markdown panes"
        );

        let pane_drag = production(include_str!("../../pane_drag.rs"));
        assert!(
            !pane_drag.contains("MarkdownFileDrag"),
            "the markdown drag payload is gone"
        );
        let pane = production(include_str!("../../pane.rs"));
        for forbidden in ["MarkdownFileDrag", "DropMarkdownSplit"] {
            assert!(
                !pane.contains(forbidden),
                "pane.rs must not contain {forbidden:?}"
            );
        }
        let handlers = production(include_str!("../event_handlers.rs"));
        assert!(
            !handlers.contains("DropMarkdownSplit"),
            "the app no longer handles a markdown drop"
        );
    }

    /// Rendered Markdown panes are not gone: an OSC path click in a terminal
    /// and a session restore still build a `PaneSurface::Markdown`.
    #[test]
    fn rendered_markdown_panes_still_come_from_osc_click_and_session_restore() {
        let handlers = production(include_str!("../event_handlers.rs"));
        let osc = between(
            handlers,
            "fn open_markdown_in_pane(",
            "fn workspace_idx_for_terminal(",
        );
        assert!(osc.contains("crate::markdown::MarkdownView::open(path, cx)"));
        assert!(osc.contains("crate::pane::PaneSurface::Markdown(markdown)"));

        let session = include_str!("../session.rs");
        assert!(session.contains("surface.surface_type.as_deref() == Some(\"markdown\")"));
        assert!(session.contains("return Some(crate::pane::PaneSurface::Markdown(markdown));"));
    }

    /// Issue #317 (deep-review S2): the filter's clear control must stay the
    /// labelled 24 px button `filter_pill_with_clear_cursor` builds, not a
    /// bare 16 px icon. Upstream's `d6a44bfc` rewrote the filter around the
    /// projection's `query`; that is what the filter computes, not how the
    /// clear control is presented, so the rail keeps going through the shared
    /// `filter_pill` primitive whose body
    /// `filter_pill_clear_control_is_a_labeled_24px_button` pins.
    #[test]
    fn files_filter_keeps_the_labelled_clear_control() {
        let view = production(include_str!("view.rs"));
        let filter_row = between(view, "fn files_filter_row(", "fn files_sidebar_body(");
        assert!(
            filter_row.contains("crate::ui_primitives::filter_pill("),
            "the filter field is the shared pill: {filter_row}"
        );
        assert!(
            filter_row.contains("\"files-sidebar-filter-clear\""),
            "the pill gets its clear control id: {filter_row}"
        );
        assert!(
            filter_row.contains("!is_empty,"),
            "the clear control shows whenever there is a needle to clear: {filter_row}"
        );
        assert!(
            filter_row.contains("this.clear_files_filter(window, cx);"),
            "the clear control clears the filter and returns focus to the tree: {filter_row}"
        );

        let primitives = include_str!("../../ui_primitives.rs");
        let pill = between(
            primitives,
            "pub(crate) fn filter_pill(",
            "fn filter_pill_with_clear_cursor(",
        );
        assert!(
            pill.contains("filter_pill_with_clear_cursor("),
            "filter_pill must build its clear control through the labelled variant: {pill}"
        );
    }

    /// Issue #238: the worker's scan must read directories through the
    /// capped `read_dir_listing` (the authority-carrying form of
    /// `read_dir_sorted`), which bounds the raw `read_dir` walk before
    /// the ignore filter. A scan that reads on its own reopens the
    /// pathological-directory hang the cap closed.
    #[test]
    fn files_worker_scans_through_the_capped_read_dir() {
        let worker = production(include_str!("worker.rs"));
        let scan = between(worker, "fn scan(&mut self", "fn watcher_available(");
        assert!(
            scan.contains("read_dir_listing(root, &dir)"),
            "the scan reads every directory through read_dir_listing: {scan}"
        );
        assert!(
            !worker.contains("std::fs::read_dir("),
            "worker.rs must not walk directories on its own"
        );
        let tree = include_str!("../files_tree.rs");
        assert!(
            tree.contains(".take(MAX_DIRECTORY_ENTRIES)"),
            "read_dir_sorted keeps the #238 raw-entry cap"
        );
    }
}

#[cfg(test)]
mod dock_tests {
    use super::*;

    #[gpui::test]
    fn file_picker_selection_and_last_file_close_cover_both_placements(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::app::diff_dock::{DiffDockTab, code::view::CodeView};
        use gpui::AppContext;
        let file = cx.new(|cx| CodeView::new(PathBuf::from("/tmp/paneflow-file-test.md"), cx));
        let mut tabs = vec![
            DiffDockTab::Changes,
            DiffDockTab::PendingFile,
            DiffDockTab::File(file),
        ];
        assert_eq!(first_file_tab(&tabs), Some(1));
        for placement in [FilesTreePlacement::Rail, FilesTreePlacement::Dock] {
            assert!(!closes_with_last_file(placement, &tabs));
        }
        tabs.remove(1);
        assert_eq!(first_file_tab(&tabs), Some(1));
        assert!(!closes_with_last_file(FilesTreePlacement::Dock, &tabs));
        tabs.remove(1);
        assert_eq!(first_file_tab(&tabs), None);
        assert!(closes_with_last_file(FilesTreePlacement::Dock, &tabs));
        assert!(!closes_with_last_file(FilesTreePlacement::Rail, &tabs));
    }

    #[test]
    fn dock_tree_hides_before_the_dock_floor_without_changing_preference() {
        for (width, expected) in [(360., 0.), (449., 0.), (450., 250.), (880., 250.)] {
            assert_eq!(dock_tree_width(true, true, width), expected);
            if expected > 0. {
                assert!(width - expected >= 200.);
            }
            assert_eq!(dock_tree_width(false, true, width), 0.);
            assert_eq!(dock_tree_width(true, false, width), 0.);
        }
        // Widening restores the tree without any intervening state mutation.
        assert_eq!(dock_tree_width(true, true, 449.), 0.);
        assert_eq!(dock_tree_width(true, true, 450.), 250.);
    }
}
