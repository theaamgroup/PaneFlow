//! Workspace and pane lifecycle operations for `PaneFlowApp`.
//!
//! Hosts the action handlers and helpers that create, select, split, close,
//! reorder, zoom, and re-layout workspaces and their pane trees. All methods
//! are pure code-motion from `main.rs` (US-023 of the src-app refactor PRD) -
//! behaviour is unchanged.
//!
//! Rendering (sidebar, context menus), IPC plumbing, toasts, settings, and
//! session persistence live in their own siblings under `app/`.
//!
//! Module layout:
//! - [`focus`] - focus-movement handlers (+ swap-on-focus override)
//! - [`tab`] - tab add/close
//! - [`swap`] - swap-mode toggle
//! - [`layout`] - zoom, layout presets, JSON layout application

mod focus;
mod layout;
mod swap;
mod tab;

use gpui::{App, AppContext, ClipboardItem, Context, Entity, Focusable, PathPromptOptions, Window};
use paneflow_config::schema::TerminalSurfaceProfile;
use paneflow_process::spawn_detached;

use crate::layout::{LayoutTree, MAX_PANES, SplitDirection};
use crate::terminal::TerminalView;
use crate::workspace::{MAX_WORKSPACES, Workspace, next_workspace_id};
use crate::{
    ClosePane, CloseWorkspace, ClosedPaneRecord, ClosedSurfaceRecord, CopyWorkspacePath,
    MAX_CLOSED_PANE_SCROLLBACK_BYTES, MAX_CLOSED_PANES, NewWorkspace, NextWorkspace,
    OpenWorkspaceInCursor, OpenWorkspaceInVsCode, OpenWorkspaceInWindsurf, OpenWorkspaceInZed,
    PaneFlowApp, RevealWorkspaceInFileManager, SelectWorkspace1, SelectWorkspace2,
    SelectWorkspace3, SelectWorkspace4, SelectWorkspace5, SelectWorkspace6, SelectWorkspace7,
    SelectWorkspace8, SelectWorkspace9, SplitHorizontally, SplitVertically, UndoClosePane,
};

#[derive(Clone)]
pub(crate) enum WorkspaceFocusTarget {
    FirstPane,
    Pane {
        pane: gpui::Entity<crate::pane::Pane>,
    },
}

fn push_closed_pane_record(records: &mut Vec<ClosedPaneRecord>, mut record: ClosedPaneRecord) {
    if let ClosedSurfaceRecord::Terminal {
        scrollback: Some(scrollback),
        ..
    } = &mut record.surface
    {
        scrollback.shrink_to_fit();
    }
    if records.len() >= MAX_CLOSED_PANES {
        records.remove(0);
    }
    records.push(record);
    enforce_closed_pane_scrollback_budget(records, MAX_CLOSED_PANE_SCROLLBACK_BYTES);
}

fn enforce_closed_pane_scrollback_budget(records: &mut [ClosedPaneRecord], budget: usize) {
    let mut total = closed_pane_scrollback_bytes(records);
    if total <= budget {
        return;
    }
    for record in records.iter_mut() {
        if total <= budget {
            break;
        }
        if let ClosedSurfaceRecord::Terminal { scrollback, .. } = &mut record.surface
            && let Some(scrollback) = scrollback.take()
        {
            total = total.saturating_sub(scrollback.len());
        }
    }
}

fn closed_pane_scrollback_bytes(records: &[ClosedPaneRecord]) -> usize {
    records
        .iter()
        .map(|record| &record.surface)
        .filter_map(|tab| match tab {
            ClosedSurfaceRecord::Terminal { scrollback, .. } => scrollback.as_ref(),
            ClosedSurfaceRecord::Markdown { .. } => None,
        })
        .map(String::len)
        .sum()
}

fn capture_closed_pane_record(
    pane: &gpui::Entity<crate::pane::Pane>,
    workspace_id: u64,
    cx: &App,
) -> Option<ClosedPaneRecord> {
    let pane_ref = pane.read(cx);
    // A diff surface is not restorable (derived state, not a document), so
    // closing one leaves nothing to undo.
    let surface = match &pane_ref.surface {
        crate::pane::PaneSurface::Terminal(tv) => {
            let tv_ref = tv.read(cx);
            ClosedSurfaceRecord::Terminal {
                cwd: tv_ref
                    .terminal
                    .current_cwd
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .or_else(|| tv_ref.terminal.cwd_now()),
                scrollback: tv_ref.terminal.extract_scrollback(),
                custom_name: tv_ref.terminal.custom_name.clone(),
                font_size: tv_ref.terminal.font_size_override,
            }
        }
        crate::pane::PaneSurface::Markdown(markdown) => ClosedSurfaceRecord::Markdown {
            path: markdown.read(cx).path.clone(),
        },
        crate::pane::PaneSurface::Diff(_) => return None,
    };
    Some(ClosedPaneRecord {
        surface,
        workspace_id,
    })
}

/// Locate the workspace a closed-pane record should restore into.
///
/// Indexes shift when workspaces close or reorder; the record stores a
/// stable `Workspace.id`. `None` means that workspace is gone.
fn workspace_index_for_undo(ids: &[u64], record_id: u64) -> Option<usize> {
    ids.iter().position(|&id| id == record_id)
}

/// After `workspaces.remove(removed_idx)`, map the previous `active_idx` onto
/// the remaining `len` slots. Closing a workspace before the active one
/// decrements; closing at or past the new last index clamps; an empty list is 0.
fn active_idx_after_workspace_remove(active_idx: usize, removed_idx: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else if active_idx >= len {
        len - 1
    } else if active_idx > removed_idx {
        active_idx - 1
    } else {
        active_idx
    }
}

fn restore_closed_surface_record(
    tab: ClosedSurfaceRecord,
    ws_id: u64,
    cx: &mut Context<PaneFlowApp>,
) -> crate::pane::PaneSurface {
    match tab {
        ClosedSurfaceRecord::Terminal {
            cwd,
            scrollback,
            custom_name,
            font_size,
        } => {
            let terminal = cx.new(|cx| TerminalView::with_cwd(ws_id, cwd, None, cx));
            terminal.update(cx, |view, _| {
                view.terminal.custom_name = custom_name;
                view.terminal.font_size_override = font_size;
            });
            if let Some(scrollback) = scrollback {
                terminal.read(cx).restore_scrollback(&scrollback);
            }
            cx.subscribe(&terminal, PaneFlowApp::handle_terminal_event)
                .detach();
            crate::pane::PaneSurface::Terminal(terminal)
        }
        ClosedSurfaceRecord::Markdown { path } => {
            let markdown = cx.new(|cx: &mut Context<crate::markdown::MarkdownView>| {
                crate::markdown::MarkdownView::open(path, cx)
            });
            crate::pane::PaneSurface::Markdown(markdown)
        }
    }
}

impl PaneFlowApp {
    /// Fold a freshly probed git state (branch, repo-ness, diff stats) into
    /// every workspace rooted at `cwd`. Returns whether anything actually
    /// changed, so the caller only repaints on a real delta.
    pub(crate) fn apply_git_state_for_cwd(
        &mut self,
        cwd: &str,
        branch: String,
        is_repo: bool,
        stats: crate::workspace::GitDiffStats,
    ) -> bool {
        let mut changed = false;
        for workspace in &mut self.workspaces {
            if workspace.cwd == cwd {
                if workspace.git_branch != branch {
                    workspace.git_branch = branch.clone();
                    changed = true;
                }
                if workspace.is_git_repo != is_repo {
                    workspace.is_git_repo = is_repo;
                    changed = true;
                }
                if workspace.git_stats != stats {
                    workspace.git_stats = stats.clone();
                    changed = true;
                }
            }
        }
        changed
    }

    /// Narrower sibling of [`Self::apply_git_state_for_cwd`]: refresh only the
    /// diff stats, for probes that never re-read the branch.
    pub(crate) fn apply_git_stats_for_cwd(
        &mut self,
        cwd: &str,
        stats: crate::workspace::GitDiffStats,
    ) -> bool {
        let mut changed = false;
        for workspace in &mut self.workspaces {
            if workspace.cwd == cwd && workspace.git_stats != stats {
                workspace.git_stats = stats.clone();
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn dismiss_transient_surfaces(&mut self) {
        self.title_bar_files_menu_open = None;
        self.title_bar_help_menu_open = None;
        self.workspace_menu_open = None;
        self.tab_menu_open = None;
        self.pane_menu_open = None;
        self.profile_menu_open = None;
        self.files_menu_open = None;
    }

    pub(crate) fn active_workspace(&self) -> Option<&Workspace> {
        debug_assert!(
            self.workspaces.is_empty() || self.active_idx < self.workspaces.len(),
            "active_idx out of bounds"
        );
        self.workspaces.get(self.active_idx)
    }

    pub(crate) fn active_workspace_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.active_idx)
    }

    pub(crate) fn select_workspace(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_workspace_at(idx, WorkspaceFocusTarget::FirstPane, window, cx);
    }

    pub(crate) fn activate_workspace_at(
        &mut self,
        idx: usize,
        focus_target: WorkspaceFocusTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if idx >= self.workspaces.len() {
            return false;
        }

        let changed = idx != self.active_idx;
        self.dismiss_transient_surfaces();
        self.active_idx = idx;

        match focus_target {
            WorkspaceFocusTarget::FirstPane => {
                self.workspaces[idx].focus_first(window, cx);
            }
            WorkspaceFocusTarget::Pane { pane } => {
                pane.update(cx, |_p, cx| cx.notify());
                pane.read(cx).focus_handle(cx).focus(window, cx);
            }
        }

        self.reroot_files_tree(cx);
        if self.agent_sessions.sessions_sidebar_open {
            let keep_sidebar_focus = self.agent_sessions.sessions_focus.is_focused(window);
            match self.workspaces[idx]
                .active_tab()
                .root
                .as_ref()
                .and_then(|root| root.first_leaf())
            {
                Some(pane) => self.open_sessions_sidebar_for_pane(
                    &pane,
                    keep_sidebar_focus.then_some(window),
                    cx,
                ),
                None => self.close_sessions_sidebar(cx),
            }
        }
        self.save_session(cx);
        self.reconcile_diff_after_workspace_change(cx);
        cx.notify();
        changed
    }

    pub(crate) fn activate_workspace_without_window(
        &mut self,
        idx: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if idx >= self.workspaces.len() {
            return false;
        }

        let changed = idx != self.active_idx;
        self.dismiss_transient_surfaces();
        self.active_idx = idx;
        self.reroot_files_tree(cx);
        if self.agent_sessions.sessions_sidebar_open {
            self.close_sessions_sidebar(cx);
        }
        self.save_session(cx);
        self.reconcile_diff_after_workspace_change(cx);
        cx.notify();
        changed
    }

    /// US-009 (orchestration-v2): tear down the worktrees a closing workspace
    /// owns, off the render thread (`git status` + `worktree remove` are
    /// subprocesses). Clean ones are removed, dirty/unverifiable ones kept,
    /// the branch never touched - all enforced by `worktree::teardown_all`.
    pub(crate) fn spawn_worktree_teardown(
        worktrees: Vec<crate::workspace::worktree::ManagedWorktree>,
        cx: &mut Context<Self>,
    ) {
        if worktrees.is_empty() {
            return;
        }
        cx.spawn(async move |_this, _cx: &mut gpui::AsyncApp| {
            smol::unblock(move || crate::workspace::worktree::teardown_all(worktrees)).await;
        })
        .detach();
    }

    /// US-005/US-014: if in Diff mode, rebuild the mounted diff (deferred) so it
    /// follows the current workspace set and active workspace - covers workspace
    /// switch (re-target) and close (Multi-project group reconcile). Deferred so
    /// the rebuild (which mounts a fresh entity) never runs inside a
    /// render/callback. No-op outside Diff mode.
    fn reconcile_diff_after_workspace_change(&self, cx: &mut Context<Self>) {
        if matches!(self.mode, paneflow_config::schema::AppMode::Diff) {
            let weak = cx.weak_entity();
            cx.defer(move |cx| {
                let _ = weak.update(cx, |app, cx| app.rebuild_diff_view(cx));
            });
        }
    }

    /// Add a workspace rooted at the implicit launch directory.
    ///
    /// EP-003: the workspace is born empty - no tab, no pane, no PTY. Opening
    /// a project is a filing gesture, not a request to run a shell; the user
    /// picks what runs in it from the folder's `+` action or the launch pad.
    #[allow(dead_code)]
    pub(crate) fn create_workspace(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.len() >= MAX_WORKSPACES {
            return;
        }
        let n = self.workspaces.len() + 1;
        let ws_id = next_workspace_id();
        let ws = Workspace::empty_with_cwd_and_id(
            ws_id,
            format!("Terminal {n}"),
            crate::launch_cwd::implicit_launch_cwd(),
        );
        // US-013: deferred git-stats probe off the render thread.
        Self::spawn_initial_git_stats(ws_id, ws.cwd.clone(), cx);
        self.watch_git_dir(&ws);
        self.workspaces.push(ws);
        self.active_idx = self.workspaces.len() - 1;
        self.save_session(cx);
        cx.notify();
    }

    /// Open one workspace per directory in `paths`.
    ///
    /// Shared by the folder picker and the sidebar's file-manager drop: both
    /// hand over a list of paths the user pointed at, and both want the same
    /// filing gesture. A path that is not a directory is ignored rather than
    /// guessed at - the gesture names a project root, and a file's parent
    /// directory is not reliably one.
    pub(crate) fn open_workspace_folders(
        &mut self,
        paths: &[std::path::PathBuf],
        cx: &mut Context<Self>,
    ) {
        let mut opened = false;
        for path in paths {
            if self.workspaces.len() >= MAX_WORKSPACES {
                break;
            }
            // A drop carries whatever the file manager had selected, so the
            // directory check is what keeps a stray file out of the rail. The
            // picker is already restricted to directories and passes through.
            if !path.is_dir() {
                continue;
            }
            let cwd = path.display().to_string();
            // Re-opening a folder that is already filed selects its row
            // instead of stacking a second one on the same root.
            if let Some(at) = self.workspaces.iter().position(|ws| ws.cwd == cwd) {
                self.active_idx = at;
                opened = true;
                continue;
            }
            let n = self.workspaces.len() + 1;
            let title = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("Terminal {n}"));
            let ws_id = next_workspace_id();
            // EP-003: an opened folder starts empty - no tab and no PTY until
            // the user asks for one.
            let ws = Workspace::empty_with_cwd_and_id(ws_id, title, path.clone());
            // US-013: deferred git-stats probe off the render thread.
            Self::spawn_initial_git_stats(ws_id, ws.cwd.clone(), cx);
            self.watch_git_dir(&ws);
            self.workspaces.push(ws);
            self.active_idx = self.workspaces.len() - 1;
            opened = true;
        }
        if !opened {
            return;
        }
        self.save_session(cx);
        cx.notify();
        // US-016 (prd-git-diff-mode-2026-Q3.md): a new repo must surface in
        // Multi-project / re-target the diff.
        self.reconcile_diff_after_workspace_change(cx);
    }

    pub(crate) fn create_workspace_with_picker(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspaces.len() >= MAX_WORKSPACES {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: None,
        });
        cx.spawn(
            async |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                if let Ok(Ok(Some(paths))) = receiver.await {
                    let _ = cx.update(|cx| {
                        this.update(cx, |app, cx| {
                            app.open_workspace_folders(&paths, cx);
                        })
                    });
                }
            },
        )
        .detach();
    }

    // --- Split/close/focus handlers (operate on active workspace) ---

    /// Working directory for a terminal spawned into the active workspace.
    /// `source_cwd` is the focused/source pane's live cwd (`cwd_now()`), which
    /// is `None` for a markdown pane (US-020) and on every platform that can't
    /// introspect a child's cwd - notably *always* on Windows, where
    /// `cwd_now()` is a stub. Left unhandled, that `None` lets the PTY spawn
    /// drop to the process `current_dir()` (`C:\Program Files\PaneFlow` for an
    /// installed build), stranding new panes outside the project. Fall back to
    /// the workspace's own root directory so a split / new pane always lands in
    /// the directory the workspace points at.
    pub(crate) fn new_terminal_cwd(
        &self,
        source_cwd: Option<std::path::PathBuf>,
    ) -> Option<std::path::PathBuf> {
        source_cwd.or_else(|| {
            self.active_workspace()
                .map(|ws| ws.cwd.as_str())
                .filter(|cwd| !cwd.is_empty())
                .map(std::path::PathBuf::from)
        })
    }

    pub(crate) fn split(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.active_workspace() else {
            return;
        };
        if ws.is_zoomed() {
            self.show_toast("Unzoom before splitting panes", cx);
            return;
        }
        let Some(root) = &ws.active_tab().root else {
            return;
        };
        if !ws.active_tab().can_add_pane() {
            self.show_toast(format!("Maximum pane count reached ({MAX_PANES})"), cx);
            return;
        }
        let Some(focused) = root.focused_pane(window, cx) else {
            self.show_toast("No focused pane to split", cx);
            return;
        };
        if let Err(message) = self.split_with_target(
            focused,
            direction,
            TerminalSurfaceProfile::Normal,
            None,
            window,
            cx,
        ) {
            self.show_toast(message, cx);
        }
    }

    /// Split `target` and return the refusal as a value instead of a toast.
    ///
    /// EP-005: the preset palette owns the focus while it is open, so it cannot
    /// resolve a target from the focus chain and must surface a refusal (the
    /// `MAX_PANES` cap in particular) inside the palette rather than behind it.
    /// `profile` and `command` let it drop an agent or a custom command
    /// straight into the new pane.
    pub(crate) fn split_with_target(
        &mut self,
        target: Entity<crate::pane::Pane>,
        direction: SplitDirection,
        profile: TerminalSurfaceProfile,
        command: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(ws) = self.active_workspace() else {
            return Err("No active project".to_string());
        };
        if ws.is_zoomed() {
            return Err("Unzoom before splitting panes".to_string());
        }
        if ws.active_tab().root.is_none() {
            return Err("This tab has no pane to split".to_string());
        }
        if !ws.active_tab().can_add_pane() {
            return Err(format!("Maximum pane count reached ({MAX_PANES})"));
        }
        let ws_id = ws.id;

        // Inherit CWD from the target pane's active terminal. `cwd_now()` is
        // best-effort: `None` for a markdown pane (US-020) and on platforms
        // without child-cwd introspection (always on Windows). `new_terminal_cwd`
        // then falls back to the workspace root, so the new pane never drops to
        // the process `current_dir()` (`C:\Program Files\PaneFlow` when installed).
        let source_cwd = target
            .read(cx)
            .active_terminal_opt()
            .and_then(|tv| tv.read(cx).terminal.cwd_now());
        let source_cwd = self.new_terminal_cwd(source_cwd);
        let new_terminal =
            cx.new(|cx| TerminalView::with_cwd_and_profile(ws_id, source_cwd, None, profile, cx));
        let new_pane = self.create_pane(new_terminal.clone(), ws_id, cx);
        let inserted = if let Some(ws) = self.active_workspace_mut()
            && let Some(root) = &mut ws.active_tab_mut().root
        {
            root.split_at_pane(&target, direction, new_pane.clone())
        } else {
            false
        };
        if !inserted {
            return Err("That pane no longer exists".to_string());
        }
        if let Some(command) = command {
            // Buffered until `TerminalState::promote` hands over the live PTY -
            // same contract as the tab path.
            new_terminal.read(cx).send_command(command);
            new_terminal.update(cx, |view, _cx| view.declare_agent_from_command(command));
        }
        new_pane.read(cx).focus_handle(cx).focus(window, cx);
        self.save_session(cx);
        cx.notify();
        Ok(())
    }

    pub(crate) fn handle_split_h(
        &mut self,
        _: &SplitHorizontally,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split(SplitDirection::Horizontal, w, cx);
    }
    pub(crate) fn handle_split_v(
        &mut self,
        _: &SplitVertically,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split(SplitDirection::Vertical, w, cx);
    }

    pub(crate) fn handle_close_pane(
        &mut self,
        _: &ClosePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Capture state of the pane being closed for undo (US-014).
        // Must happen BEFORE the tree mutation that drops the pane entity.
        if let Some(ws) = self.active_workspace()
            && let Some(root) = &ws.active_tab().root
        {
            let workspace_id = ws.id;
            let closing_pane = if ws.is_zoomed() {
                root.first_leaf()
            } else {
                root.focused_pane(window, cx)
            };
            if let Some(pane) = closing_pane
                && let Some(record) = capture_closed_pane_record(&pane, workspace_id, cx)
            {
                push_closed_pane_record(&mut self.closed_panes, record);
            }
        }

        if let Some(ws) = self.active_workspace_mut()
            && ws.is_zoomed()
        {
            if let Some(pane) = ws.exit_zoom(cx)
                && let Some(root) = ws.active_tab_mut().root.take()
            {
                let (new_root, _) = root.remove_pane(&pane);
                ws.active_tab_mut().root = new_root;
            }
            if let Some(ref root) = ws.active_tab().root {
                root.focus_first(window, cx);
            }
        } else if let Some(ws) = self.active_workspace_mut()
            && let Some(root) = ws.active_tab_mut().root.take()
        {
            let (new_root, _closed, focus_target) = root.close_focused(window, cx);
            ws.active_tab_mut().root = new_root;

            if ws.active_tab().root.is_some() {
                if let Some(target) = focus_target {
                    target.read(cx).focus_handle(cx).focus(window, cx);
                } else if let Some(ref root) = ws.active_tab().root {
                    root.focus_first(window, cx);
                }
            }
        }

        // Never destroy a workspace when its last pane closes - respawn a
        // fresh terminal at the workspace's root cwd. Workspaces are only
        // removed via the explicit "Close workspace" action.
        if let Some(ws) = self.active_workspace()
            && ws.active_tab().root.is_none()
        {
            let ws_id = ws.id;
            let cwd = std::path::PathBuf::from(&ws.cwd);
            let terminal = cx.new(|cx| TerminalView::with_cwd(ws_id, Some(cwd), None, cx));
            // US-028: do NOT subscribe here - `create_pane` already wires
            // `handle_terminal_event` (main.rs:539). The duplicate subscription
            // fired every terminal event twice (double toast / port-scan /
            // mutation) and leaked the extra subscription. `split()` and
            // `create_workspace` prove the correct pattern (no manual subscribe).
            let new_pane = self.create_pane(terminal, ws_id, cx);
            if let Some(ws) = self.active_workspace_mut() {
                ws.active_tab_mut().root = Some(LayoutTree::Leaf(new_pane));
            }
            self.workspaces[self.active_idx].focus_first(window, cx);
        }

        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn handle_undo_close_pane(
        &mut self,
        _: &UndoClosePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self.closed_panes.pop() else {
            self.show_toast("No closed pane to restore", cx);
            return; // No closed panes to restore
        };

        // Restore into the workspace the pane was closed in. Indexes shift
        // on close/reorder; the record stores a stable Workspace.id.
        let ids: Vec<u64> = self.workspaces.iter().map(|ws| ws.id).collect();
        let Some(idx) = workspace_index_for_undo(&ids, record.workspace_id) else {
            self.show_toast("Workspace no longer exists", cx);
            return;
        };
        self.active_idx = idx;
        let ws_id = record.workspace_id;
        let surface = restore_closed_surface_record(record.surface, ws_id, cx);
        let new_pane = self.create_pane_with_existing_surface(surface, ws_id, cx);

        // Insert via split from the currently focused pane
        let inserted = if let Some(ws) = self.active_workspace_mut() {
            if let Some(root) = &mut ws.active_tab_mut().root {
                if !root.split_at_focused(SplitDirection::Horizontal, new_pane.clone(), window, cx)
                {
                    root.split_first_leaf(SplitDirection::Horizontal, new_pane.clone());
                }
            } else {
                ws.active_tab_mut().root = Some(LayoutTree::Leaf(new_pane.clone()));
            }
            true
        } else {
            false
        };
        if !inserted {
            return;
        }
        new_pane.read(cx).focus_handle(cx).focus(window, cx);

        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn handle_new_workspace(
        &mut self,
        _: &NewWorkspace,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_workspace_with_picker(w, cx);
    }

    pub(crate) fn handle_close_workspace(
        &mut self,
        _: &CloseWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_workspace_at(self.active_idx, window, cx);
    }

    pub(crate) fn handle_copy_workspace_path(
        &mut self,
        _: &CopyWorkspacePath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_workspace_path(self.active_idx, cx);
    }

    pub(crate) fn handle_reveal_workspace_in_file_manager(
        &mut self,
        _: &RevealWorkspaceInFileManager,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reveal_workspace_in_file_manager(self.active_idx, cx);
    }

    pub(crate) fn handle_open_workspace_in_zed(
        &mut self,
        _: &OpenWorkspaceInZed,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_in_editor(self.active_idx, "zed", "Zed", cx);
    }

    pub(crate) fn handle_open_workspace_in_cursor(
        &mut self,
        _: &OpenWorkspaceInCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_in_editor(self.active_idx, "cursor", "Cursor", cx);
    }

    pub(crate) fn handle_open_workspace_in_vscode(
        &mut self,
        _: &OpenWorkspaceInVsCode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_in_editor(self.active_idx, "code", "VS Code", cx);
    }

    pub(crate) fn handle_open_workspace_in_windsurf(
        &mut self,
        _: &OpenWorkspaceInWindsurf,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_in_editor(self.active_idx, "windsurf", "Windsurf", cx);
    }

    pub(crate) fn close_workspace_at(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_workspace_at_inner(idx, Some(window), cx);
    }

    /// Close workspace `idx` with the same index math and composer/diff
    /// teardown as the UI closer, but skip Window-only pane focus. IPC uses
    /// this so it cannot drift from the UI path. Does not refuse the last
    /// workspace; `workspace.close` checks that itself.
    pub(crate) fn close_workspace_at_without_window(
        &mut self,
        idx: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        self.close_workspace_at_inner(idx, None, cx)
    }

    fn close_workspace_at_inner(
        &mut self,
        idx: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        if idx >= self.workspaces.len() {
            return false;
        }
        self.workspace_menu_open = None;
        if let Some(dir) = self.workspaces[idx].git_dir.clone() {
            self.unwatch_git_dir(&dir);
        }
        // US-009: this workspace's managed worktrees are torn down (clean
        // ones only) in the background once the workspace is gone.
        let worktrees = std::mem::take(&mut self.workspaces[idx].managed_worktrees);
        Self::spawn_worktree_teardown(worktrees, cx);
        self.workspaces.remove(idx);
        self.active_idx =
            active_idx_after_workspace_remove(self.active_idx, idx, self.workspaces.len());
        if let Some(window) = window
            && !self.workspaces.is_empty()
        {
            self.workspaces[self.active_idx].focus_first(window, cx);
        }
        self.save_session(cx);
        cx.notify();
        // EP-001 (cli-cockpit): the closed workspace's panes may have carried
        // a Composer target, queued prompts, or group memberships. Refresh:
        // a dead-target Composer closes itself (refresh_composer_slot),
        // stale group members are pruned, and orphaned buffers drop on the
        // next flush (their terminals no longer resolve).
        self.refresh_composer_slot(cx);
        self.sync_broadcast_stripes(cx);
        self.flush_pending_prefill(cx);
        self.sync_pending_chips(cx);
        // US-014 (prd-git-diff-mode-2026-Q3.md): in Diff mode, closing a
        // workspace reconciles the diff (a Multi-project group / column for the
        // closed workspace must drop). Deferred so the rebuild runs after the
        // close settles, never inside a render/callback.
        self.reconcile_diff_after_workspace_change(cx);
        true
    }

    /// Move a workspace (identified by `from_id`) so it ends up at `to_idx`
    /// in the workspace list. Preserves which workspace is active across the
    /// reorder and persists the new order.
    pub(crate) fn reorder_workspace(
        &mut self,
        from_id: u64,
        to_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(from_idx) = self.workspaces.iter().position(|ws| ws.id == from_id) else {
            return;
        };
        let active_id = self.workspaces.get(self.active_idx).map(|ws| ws.id);
        let ws = self.workspaces.remove(from_idx);
        let insert_at = to_idx.min(self.workspaces.len());
        if from_idx == insert_at {
            self.workspaces.insert(insert_at, ws);
            return;
        }
        self.workspaces.insert(insert_at, ws);
        if let Some(id) = active_id {
            self.active_idx = self
                .workspaces
                .iter()
                .position(|ws| ws.id == id)
                .unwrap_or(0);
        }
        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn copy_workspace_path(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(ws) = self.workspaces.get(idx) else {
            return;
        };

        cx.write_to_clipboard(ClipboardItem::new_string(ws.cwd.clone()));
        self.show_toast("Path copied", cx);
        self.workspace_menu_open = None;
        cx.notify();
    }

    pub(crate) fn reveal_workspace_in_file_manager(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(ws) = self.workspaces.get(idx) else {
            return;
        };

        let cwd = ws.cwd.clone();
        self.workspace_menu_open = None;

        if let Err(msg) = reveal_in_file_manager(std::path::Path::new(&cwd)) {
            log::warn!("failed to reveal workspace path in file manager: {msg}");
            self.show_toast(msg, cx);
        }

        cx.notify();
    }

    pub(crate) fn open_workspace_in_editor(
        &mut self,
        idx: usize,
        command: &str,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspaces.get(idx) else {
            return;
        };
        let cwd = ws.cwd.clone();

        // GUI launchers (.desktop on Linux, Finder on macOS, Start menu on
        // Windows) frequently strip user bin directories from PATH, so editors
        // installed under ~/.local/bin or ~/.cargo/bin can't be found by
        // Command::new alone - even though they resolve fine from a terminal.
        let bin = resolve_editor_binary(command);

        let toast_label = editor_toast_label(label);
        let mut cmd = std::process::Command::new(&bin);
        cmd.current_dir(&cwd).arg(".");
        if let Err(err) = spawn_detached(&mut cmd) {
            log::warn!("failed to open workspace in {toast_label}: {err}");
            self.show_toast(format!("Couldn't open in {toast_label}: {err}"), cx);
        }

        self.workspace_menu_open = None;
        cx.notify();
    }

    pub(crate) fn commit_rename(&mut self, cx: &App) {
        if let Some(idx) = self.renaming_idx.take() {
            let text = std::mem::take(&mut self.rename_text);
            if !text.is_empty()
                && let Some(ws) = self.workspaces.get_mut(idx)
            {
                ws.title = text;
                self.save_session(cx);
            }
        }
        // US-010: the sidebar tab rows share `rename_text` with the workspace
        // rename, so one commit settles whichever inline rename was live.
        if let Some((ws_idx, tab_idx)) = self.renaming_tab.take() {
            let text = std::mem::take(&mut self.rename_text);
            if !text.is_empty()
                && let Some(tab) = self
                    .workspaces
                    .get_mut(ws_idx)
                    .and_then(|ws| ws.tab_mut(tab_idx))
            {
                tab.title = text;
                self.save_session(cx);
            }
        }
    }

    pub(crate) fn handle_next_workspace(
        &mut self,
        _: &NextWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.workspaces.is_empty() {
            let next = (self.active_idx + 1) % self.workspaces.len();
            self.select_workspace(next, window, cx);
        }
    }

    pub(crate) fn handle_select_ws(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_workspace(idx, window, cx);
    }

    // Macro-like handlers for Ctrl+1-9
    pub(crate) fn handle_ws1(
        &mut self,
        _: &SelectWorkspace1,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(0, w, cx);
    }
    pub(crate) fn handle_ws2(
        &mut self,
        _: &SelectWorkspace2,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(1, w, cx);
    }
    pub(crate) fn handle_ws3(
        &mut self,
        _: &SelectWorkspace3,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(2, w, cx);
    }
    pub(crate) fn handle_ws4(
        &mut self,
        _: &SelectWorkspace4,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(3, w, cx);
    }
    pub(crate) fn handle_ws5(
        &mut self,
        _: &SelectWorkspace5,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(4, w, cx);
    }
    pub(crate) fn handle_ws6(
        &mut self,
        _: &SelectWorkspace6,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(5, w, cx);
    }
    pub(crate) fn handle_ws7(
        &mut self,
        _: &SelectWorkspace7,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(6, w, cx);
    }
    pub(crate) fn handle_ws8(
        &mut self,
        _: &SelectWorkspace8,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(7, w, cx);
    }
    pub(crate) fn handle_ws9(
        &mut self,
        _: &SelectWorkspace9,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_select_ws(8, w, cx);
    }
}

/// Spawn Finder with `path` in focus (US-011).
///
/// Uses `open <path>` (Finder dispatches). `open -R <path>` would "reveal"
/// with the file highlighted, but the PRD explicitly mandates `open <path>`;
/// callers that want reveal-with-highlight pass the parent directory.
///
/// Returns `Err(message)` on spawn failure where `message` is already
/// phrased for a user-visible toast (US-011 AC7, AC9).
pub(crate) fn reveal_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    spawn_detached(std::process::Command::new("open").arg(path))
        .map_err(|err| format!("Could not open Finder: {err}"))
}

/// Resolve an editor command (e.g. `"zed"`, `"code"`) to a concrete path.
///
/// `Command::new(command).spawn()` only consults the spawning process's PATH,
/// which on macOS Finder launches frequently lacks the user-bin directories
/// where editors like Zed, Cursor, or Code Insiders install their CLI shim.
/// We extend the search with a small set of well-known fallbacks, then fall
/// back to the bare command so `spawn()` still produces a clean `NotFound`
/// error (now surfaced via toast in `open_workspace_in_editor`).
pub(crate) fn resolve_editor_binary(command: &str) -> std::path::PathBuf {
    resolve_editor_binary_in(command, &editor_search_paths())
}

pub(crate) fn editor_toast_label(label: &str) -> &str {
    label.strip_prefix("Open in ").unwrap_or(label)
}

/// Pure resolver: try the inherited PATH first, then `fallback_paths`, then
/// return the bare `command`. Split out from [`resolve_editor_binary`] so the
/// fallback list can be injected from tests without touching process env.
fn resolve_editor_binary_in(
    command: &str,
    fallback_paths: &[std::path::PathBuf],
) -> std::path::PathBuf {
    if let Ok(path) = which::which(command)
        && let Some(path) = normalize_editor_candidate(path)
    {
        return path;
    }
    if !fallback_paths.is_empty()
        && let Ok(joined) = std::env::join_paths(fallback_paths)
        && let Ok(path) = which::which_in(command, Some(&joined), ".")
        && let Some(path) = normalize_editor_candidate(path)
    {
        return path;
    }
    std::path::PathBuf::from(command)
}

fn normalize_editor_candidate(path: std::path::PathBuf) -> Option<std::path::PathBuf> {
    Some(path)
}

/// Directories to consult when an editor isn't on PATH.
///
/// GUI launches inherit a narrowed PATH, so we also search well-known user
/// and Homebrew bin directories (`~/.local/bin`, `~/.cargo/bin`, `/usr/local/bin`,
/// `/opt/homebrew/bin`).
fn editor_search_paths() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;

    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local").join("bin"));
        paths.push(home.join(".cargo").join("bin"));
        paths.push(home.join("bin"));
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from("/usr/local/bin"));
        paths.push(PathBuf::from("/opt/homebrew/bin"));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-Rust tests only - spawning actual binaries is brittle in CI
    // (macOS runners may not have `open` on PATH under a non-GUI session).
    // We exercise the error-message shape so the toast copy can't drift silently.

    #[test]
    fn reveal_accepts_regular_path() {
        // Smoke-test that the helper is callable with a plausible path
        // and that its return type is `Result<(), String>`. Actual
        // spawn behaviour is OS-dependent and left to CI / manual
        // verification per US-011 AC10.
        let tmp = tempfile::TempDir::new().unwrap();
        // Don't actually spawn - the test would flake on headless CI
        // without a default file-manager registered. We verify the
        // type-shape compiles and the helper is reachable from tests.
        let _callable: fn(&std::path::Path) -> Result<(), String> = reveal_in_file_manager;
        let _ = tmp.path();
    }

    // ════════════════════════════════════════════════════════════════════
    // Editor-binary resolver - regression coverage for the "Open in
    // <Editor>" silent-failure bug.
    //
    // Bug: GUI launchers (macOS Finder) inherit a narrowed PATH that omits
    // user-bin dirs (~/.local/bin etc.), so editors installed there can't be
    // spawned by `Command::new` alone. Cursor at /usr/bin worked; Zed at
    // ~/.local/bin failed silently.
    //
    // The fixture-based tests below run without a real editor installed. The
    // macOS shape test self-validates the fallback list.
    // ════════════════════════════════════════════════════════════════════

    /// Create a stub binary named `<command>` inside `dir` and flip the
    /// executable bit so `which` will accept it. Returns the absolute path
    /// to the stub for canonical comparison.
    fn make_stub_binary(dir: &std::path::Path, command: &str) -> std::path::PathBuf {
        let path = dir.join(command);
        std::fs::write(&path, b"").expect("write stub binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&path).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&path, perm).unwrap();
        }
        path
    }

    #[test]
    fn resolver_picks_up_binary_from_fallback_dir() {
        // High-entropy stub name so `which::which` against the host's real
        // PATH cannot resolve it - forcing the resolver into the fallback
        // branch we want to exercise. Without this, the test could pass on
        // the wrong code path if the host happens to have a similarly-
        // named binary installed.
        let stub = "paneflow_resolver_stub_pflw_42";
        let dir = tempfile::TempDir::new().unwrap();
        let expected = make_stub_binary(dir.path(), stub);

        let resolved = resolve_editor_binary_in(stub, &[dir.path().to_path_buf()]);

        // `which_in` may canonicalize symlinks and `.` components - compare
        // canonical forms so the test is resilient on macOS (/var → /private/var)
        // and on distros where /home is a symlink. Falling back to raw
        // PathBuf comparison would flake on those hosts.
        let canon_resolved = std::fs::canonicalize(&resolved).ok();
        let canon_expected = std::fs::canonicalize(&expected).ok();
        assert_eq!(
            canon_resolved,
            canon_expected,
            "resolver returned {} instead of fallback {}",
            resolved.display(),
            expected.display()
        );
    }

    #[test]
    fn resolver_returns_bare_command_when_nothing_resolves() {
        // Empty fallback list AND a command name designed to be absent
        // from any host PATH. The resolver must hand back the bare command
        // so the caller's spawn() produces a clean NotFound error that our
        // toast surfaces to the user.
        let bare = "paneflow_no_such_editor_zzz_99";
        let resolved = resolve_editor_binary_in(bare, &[]);
        assert_eq!(resolved, std::path::PathBuf::from(bare));
    }

    #[test]
    fn resolver_returns_bare_command_when_fallback_dir_is_empty() {
        // Same contract, exercised through the directory-search branch
        // rather than the fast-skip empty-vec branch.
        let dir = tempfile::TempDir::new().unwrap();
        let bare = "paneflow_no_such_editor_zzz_77";
        let resolved = resolve_editor_binary_in(bare, &[dir.path().to_path_buf()]);
        assert_eq!(resolved, std::path::PathBuf::from(bare));
    }

    fn closed_pane_record_with_scrollback(len: usize) -> ClosedPaneRecord {
        ClosedPaneRecord {
            surface: ClosedSurfaceRecord::Terminal {
                cwd: None,
                scrollback: Some("x".repeat(len)),
                custom_name: None,
                font_size: None,
            },
            workspace_id: 0,
        }
    }

    #[test]
    fn workspace_index_for_undo_matches_after_lower_workspace_closed() {
        // Originally [10, 20, 30]; workspace 10 (index 0) closed → [20, 30].
        // A pane closed in workspace 20 was index 1; it is now index 0.
        assert_eq!(workspace_index_for_undo(&[20, 30], 20), Some(0));
        assert_eq!(workspace_index_for_undo(&[20, 30], 30), Some(1));
    }

    #[test]
    fn workspace_index_for_undo_missing_id_returns_none() {
        assert_eq!(workspace_index_for_undo(&[20, 30], 10), None);
        assert_eq!(workspace_index_for_undo(&[], 1), None);
    }

    #[test]
    fn closed_pane_budget_drops_oldest_scrollback_not_record() {
        let one_mib = 1024 * 1024;
        let mut records = vec![
            closed_pane_record_with_scrollback(one_mib),
            closed_pane_record_with_scrollback(one_mib),
        ];

        push_closed_pane_record(&mut records, closed_pane_record_with_scrollback(one_mib));

        assert_eq!(records.len(), 3, "budget must preserve undo records");
        assert!(
            matches!(
                &records[0].surface,
                ClosedSurfaceRecord::Terminal {
                    scrollback: None,
                    ..
                }
            ),
            "oldest scrollback should be released first"
        );
        assert!(matches!(
            &records[1].surface,
            ClosedSurfaceRecord::Terminal {
                scrollback: Some(_),
                ..
            }
        ));
        assert!(matches!(
            &records[2].surface,
            ClosedSurfaceRecord::Terminal {
                scrollback: Some(_),
                ..
            }
        ));
        assert_eq!(
            closed_pane_scrollback_bytes(&records),
            MAX_CLOSED_PANE_SCROLLBACK_BYTES
        );
    }

    #[test]
    fn active_idx_after_workspace_remove_table() {
        // (active_idx, removed_idx, remaining_len, expected)
        // remaining_len is the count *after* remove, matching the closer.
        let cases = [
            // Issue #21: three workspaces, stay on 1, close 0 → old 1, not old 2.
            (1usize, 0usize, 2usize, 0usize),
            (1, 1, 2, 1), // close the active middle tab; next slides into the slot
            (1, 2, 2, 1), // close after the active tab
            (2, 2, 2, 1), // close the last tab while on it → clamp
            (2, 0, 2, 1), // close before the last-active tab → clamp lands on old last
            (2, 1, 2, 1), // close the middle tab while on last → clamp
            (0, 0, 2, 0), // close first while on first; old 1 slides to 0
            (0, 1, 2, 0), // close after first while on first
            (0, 1, 1, 0), // two workspaces, close the other
            (1, 0, 1, 0), // two workspaces, close the one before active
            (1, 1, 1, 0), // two workspaces, close the active last
            (0, 0, 0, 0), // close the last remaining workspace
        ];
        for (active, removed, remaining, expected) in cases {
            assert_eq!(
                active_idx_after_workspace_remove(active, removed, remaining),
                expected,
                "active={active} removed={removed} remaining={remaining}"
            );
        }
    }

    #[test]
    fn close_workspace_shared_closer_runs_composer_and_diff_teardown() {
        let src = include_str!("mod.rs");
        let closer = src
            .split("fn close_workspace_at_inner")
            .nth(1)
            .and_then(|rest| rest.split("fn reorder_workspace").next())
            .expect("shared closer");
        for helper in [
            "refresh_composer_slot",
            "sync_broadcast_stripes",
            "flush_pending_prefill",
            "sync_pending_chips",
            "reconcile_diff_after_workspace_change",
            "active_idx_after_workspace_remove",
        ] {
            assert!(closer.contains(helper), "shared closer must call {helper}");
        }
    }

    #[test]
    fn closed_pane_budget_preserves_absent_scrollback_for_undo() {
        let mut records = Vec::new();
        push_closed_pane_record(
            &mut records,
            ClosedPaneRecord {
                surface: ClosedSurfaceRecord::Terminal {
                    cwd: None,
                    scrollback: None,
                    custom_name: None,
                    font_size: None,
                },
                workspace_id: 0,
            },
        );

        assert_eq!(records.len(), 1);
        assert!(matches!(
            &records[0].surface,
            ClosedSurfaceRecord::Terminal {
                scrollback: None,
                ..
            }
        ));
        assert_eq!(closed_pane_scrollback_bytes(&records), 0);
    }

    // ─── Per-OS path-list shape ────────────────────────────────────────
    // `editor_search_paths()` returns the OS-specific fallback list. Each
    // test runs only on its target. Assertions are positive (must contain),
    // so adding entries doesn't break old tests; removing an entry trips
    // a clear failure with the missing path named.

    #[cfg(target_os = "macos")]
    #[test]
    fn search_paths_macos_covers_homebrew_and_user_bin() {
        let paths = editor_search_paths();
        let home = dirs::home_dir().expect("test host has $HOME");
        assert!(
            paths.contains(&home.join(".local").join("bin")),
            "missing ~/.local/bin"
        );
        assert!(
            paths.contains(&home.join(".cargo").join("bin")),
            "missing ~/.cargo/bin"
        );
        assert!(paths.contains(&home.join("bin")), "missing ~/bin");
        assert!(
            paths.contains(&std::path::PathBuf::from("/usr/local/bin")),
            "missing /usr/local/bin (Intel Homebrew prefix)"
        );
        assert!(
            paths.contains(&std::path::PathBuf::from("/opt/homebrew/bin")),
            "missing /opt/homebrew/bin (Apple Silicon Homebrew prefix)"
        );
    }
}
