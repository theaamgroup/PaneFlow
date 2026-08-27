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
use paneflow_config::schema::{LayoutNode, TerminalSurfaceProfile};
use paneflow_process::spawn_detached;

use crate::layout::{LayoutTree, MAX_PANES, SplitDirection};
use crate::terminal::TerminalView;
use crate::workspace::{MAX_WORKSPACES, Workspace, next_workspace_id};
use crate::{
    ClosePane, CloseWorkspace, ClosedPaneRecord, ClosedRecord, ClosedSurfaceRecord,
    CopyWorkspacePath, MAX_CLOSED_PANE_SCROLLBACK_BYTES, MAX_CLOSED_PANES, NewWorkspace,
    NextWorkspace, OpenWorkspaceInCursor, OpenWorkspaceInVsCode, OpenWorkspaceInWindsurf,
    OpenWorkspaceInZed, PaneFlowApp, RevealWorkspaceInFileManager, SelectWorkspace1,
    SelectWorkspace2, SelectWorkspace3, SelectWorkspace4, SelectWorkspace5, SelectWorkspace6,
    SelectWorkspace7, SelectWorkspace8, SelectWorkspace9, SplitHorizontally, SplitVertically,
    UndoClosePane,
};

#[derive(Clone)]
pub(crate) enum WorkspaceFocusTarget {
    FirstPane,
    /// Issue #78: focus the pane whose agent is waiting for input, falling
    /// back to [`Self::FirstPane`] when nothing in the workspace is waiting.
    WaitingElseFirst,
    Pane {
        pane: gpui::Entity<crate::pane::Pane>,
    },
}

/// Issue #78: the pane in `ws` whose agent session is
/// [`crate::ai_types::AgentState::WaitingForInput`], as
/// `(tab index, pane, surface id)`.
///
/// Searches **every tab**, not just the visible one. A workspace-level
/// gesture should find the agent wherever it lives, and the sidebar already
/// advertises waiting agents that sit in a background tab - so a walk limited
/// to the visible tab would leave the badge pointing at a pane the gesture
/// refuses to reach. The caller is responsible for making the returned tab
/// visible before focusing the pane.
///
/// Returns the first match in (tab order, then layout order) so repeated
/// selections of the same workspace are stable. Sessions whose `surface_id`
/// never resolved are skipped: never jump to a guessed pane.
///
/// A free function, not a `PaneFlowApp` method, because `PaneFlowApp` cannot
/// be constructed in a test - keeping the rule here is what makes it testable.
fn waiting_pane_in_workspace(
    ws: &Workspace,
    cx: &App,
) -> Option<(usize, gpui::Entity<crate::pane::Pane>, u64)> {
    let waiting: std::collections::HashSet<u64> = ws
        .agent_sessions
        .values()
        .filter(|s| s.state == crate::ai_types::AgentState::WaitingForInput)
        .filter_map(|s| s.surface_id)
        .collect();
    if waiting.is_empty() {
        return None;
    }
    for (tab_idx, tab) in ws.tabs().iter().enumerate() {
        let Some(root) = tab.root.as_ref() else {
            continue;
        };
        for pane in root.collect_leaves() {
            let sid = pane
                .read(cx)
                .active_terminal_opt()
                .map(|t| t.entity_id().as_u64());
            if let Some(sid) = sid
                && waiting.contains(&sid)
            {
                return Some((tab_idx, pane, sid));
            }
        }
    }
    None
}

/// Push one undo-close entry, honouring both caps: at most
/// [`MAX_CLOSED_PANES`] whole records (oldest evicted first), and at most
/// [`MAX_CLOSED_PANE_SCROLLBACK_BYTES`] of captured scrollback across all of
/// them. A tab entry counts every leaf of its captured tree.
pub(crate) fn push_closed_record(records: &mut Vec<ClosedRecord>, mut record: ClosedRecord) {
    if let ClosedRecord::Pane(ClosedPaneRecord {
        surface:
            ClosedSurfaceRecord::Terminal {
                scrollback: Some(scrollback),
                ..
            },
        ..
    }) = &mut record
    {
        scrollback.shrink_to_fit();
    }
    if records.len() >= MAX_CLOSED_PANES {
        records.remove(0);
    }
    records.push(record);
    enforce_closed_pane_scrollback_budget(records, MAX_CLOSED_PANE_SCROLLBACK_BYTES);
}

/// Drop every undo record belonging to a workspace that is going away.
///
/// `NEXT_WORKSPACE_ID` is a monotonic `fetch_add`, so a closed workspace's id
/// is never handed out again and [`workspace_index_for_undo`] can never match
/// one of these records. Left on the stack they are not merely dead weight:
/// `handle_undo_close_pane` pops the NEWEST record, and a refusal that pushed
/// it back would put it straight back on top, so one orphan blocks every
/// restorable record beneath it forever - and, being always newest, is the
/// LAST thing FIFO eviction reaches.
pub(crate) fn drop_closed_records_for_workspace(
    records: &mut Vec<ClosedRecord>,
    workspace_id: u64,
) {
    records.retain(|record| record.workspace_id() != workspace_id);
}

/// Release captured scrollback until the total is back under `budget`.
///
/// Two nested orderings, both oldest-first: records in stack order, and inside
/// a record its leaves in traversal order. Stripping is **per leaf**, not per
/// record, and the budget is re-checked after each leaf. That distinction is
/// what keeps a tab record usable: one tab can hold up to
/// [`crate::layout::MAX_PANES`] leaves of up to [`crate::limits::MAX_CHARS`]
/// each - far past the whole budget - so an all-or-nothing sweep would return
/// a multi-pane tab with no history at all rather than trimming the oldest
/// leaves and keeping the rest.
///
/// Records themselves are always kept: an undo that restores a pane or a tab
/// without its history is still an undo, while dropping the record would lose
/// the layout too. A single-surface pane record is the degenerate one-leaf
/// case of the same walk.
fn enforce_closed_pane_scrollback_budget(records: &mut [ClosedRecord], budget: usize) {
    let mut total = closed_pane_scrollback_bytes(records);
    if total <= budget {
        return;
    }
    for record in records.iter_mut() {
        if total <= budget {
            break;
        }
        release_record_scrollback(record, &mut total, budget);
    }
}

/// Clear one record's leaves, one at a time in traversal order, stopping the
/// moment `total` drops to `budget`.
fn release_record_scrollback(record: &mut ClosedRecord, total: &mut usize, budget: usize) {
    match record {
        ClosedRecord::Pane(pane) => {
            if *total <= budget {
                return;
            }
            if let ClosedSurfaceRecord::Terminal { scrollback, .. } = &mut pane.surface
                && let Some(scrollback) = scrollback.take()
            {
                *total = total.saturating_sub(scrollback.len());
            }
        }
        ClosedRecord::Tab(tab) => release_layout_scrollback(&mut tab.layout, total, budget),
    }
}

/// Leaf-by-leaf half of [`release_record_scrollback`] for a serialized tree.
fn release_layout_scrollback(
    node: &mut paneflow_config::schema::LayoutNode,
    total: &mut usize,
    budget: usize,
) {
    match node {
        paneflow_config::schema::LayoutNode::Pane { surfaces } => {
            for surface in surfaces.iter_mut() {
                if *total <= budget {
                    return;
                }
                if let Some(scrollback) = surface.scrollback.take() {
                    *total = total.saturating_sub(scrollback.len());
                }
            }
        }
        paneflow_config::schema::LayoutNode::Split { children, .. } => {
            for child in children.iter_mut() {
                if *total <= budget {
                    return;
                }
                release_layout_scrollback(child, total, budget);
            }
        }
    }
}

/// Drop the parts of a serialized tree that cannot be restored, returning
/// `None` when nothing restorable is left.
///
/// A leaf holding a Diff surface serializes to `LayoutNode::Pane { surfaces:
/// [] }` - `serialize_with` filters the diff `SurfaceDefinition` out but still
/// emits the leaf. Restoring that leaf would fall into
/// `spawn_pane_from_surfaces`' fallback and silently hand back a plain shell,
/// contradicting the pane-level rule that a diff surface is derived state and
/// not restorable at all. So the leaf goes, and the tree closes over the gap:
/// a split left with one child collapses into that child, a split left with
/// none disappears, and a tree that prunes away entirely yields `None`.
fn prune_unrestorable(node: LayoutNode) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { surfaces } => {
            if surfaces.is_empty() {
                None
            } else {
                Some(LayoutNode::Pane { surfaces })
            }
        }
        LayoutNode::Split {
            direction,
            ratio,
            ratios,
            children,
        } => {
            let had_ratios = ratios.is_some();
            let original_children = children.len();
            let mut kept_children = Vec::with_capacity(children.len());
            let mut kept_ratios = Vec::with_capacity(children.len());
            for (i, child) in children.into_iter().enumerate() {
                let child_ratio = ratios.as_ref().and_then(|rs| rs.get(i).copied());
                if let Some(kept) = prune_unrestorable(child) {
                    kept_children.push(kept);
                    if let Some(child_ratio) = child_ratio {
                        kept_ratios.push(child_ratio);
                    }
                }
            }
            match kept_children.len() {
                0 => None,
                // A one-child split is not a split; collapsing keeps the tree
                // in the shape the rest of the layout code expects.
                1 => kept_children.pop(),
                _ => {
                    // Ratios are positional. Keep them only when every
                    // surviving child still has its own; a short list would
                    // silently reassign widths, and `resolved_ratios` already
                    // falls back to equal shares for `None`.
                    let ratios = (had_ratios && kept_ratios.len() == kept_children.len())
                        .then_some(kept_ratios);
                    // The legacy scalar `ratio` describes a BINARY split -
                    // `resolved_ratios` reads it as `[ratio, 1 - ratio]`. Carry
                    // it onto a split of a different width and it silently
                    // reassigns widths that used to resolve to equal shares.
                    // `serialize_with` always emits `None` here, so this only
                    // bites a caller handing this pure function a tree read off
                    // a session file - which is the shape `LayoutNode` exists
                    // to carry.
                    let ratio = if kept_children.len() == original_children {
                        ratio
                    } else {
                        None
                    };
                    Some(LayoutNode::Split {
                        direction,
                        ratio,
                        ratios,
                        children: kept_children,
                    })
                }
            }
        }
    }
}

/// Total bytes of captured scrollback held in one serialized layout tree.
///
/// Walks every `LayoutNode::Pane` leaf and sums its surfaces' inline
/// scrollback. A tab-level undo record stores its whole tree, so the byte
/// budget cannot see that text without this walk.
fn layout_node_scrollback_bytes(node: &paneflow_config::schema::LayoutNode) -> usize {
    match node {
        paneflow_config::schema::LayoutNode::Pane { surfaces } => surfaces
            .iter()
            .filter_map(|surface| surface.scrollback.as_ref())
            .map(String::len)
            .sum(),
        paneflow_config::schema::LayoutNode::Split { children, .. } => {
            children.iter().map(layout_node_scrollback_bytes).sum()
        }
    }
}

/// Total captured scrollback across the whole undo stack, counting a tab
/// record's every leaf as well as a pane record's single surface.
fn closed_pane_scrollback_bytes(records: &[ClosedRecord]) -> usize {
    records
        .iter()
        .map(|record| match record {
            ClosedRecord::Pane(pane) => match &pane.surface {
                ClosedSurfaceRecord::Terminal { scrollback, .. } => {
                    scrollback.as_ref().map_or(0, String::len)
                }
                ClosedSurfaceRecord::Markdown { .. } => 0,
            },
            ClosedRecord::Tab(tab) => layout_node_scrollback_bytes(&tab.layout),
        })
        .sum()
}

pub(crate) fn capture_closed_pane_record(
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

/// Snapshot a whole tab for undo. `None` when the tab holds nothing worth
/// restoring - no layout at all, or a layout that prunes away entirely.
///
/// Prefers `saved_layout` over `root`: while a tab is zoomed, `saved_layout`
/// holds the FULL tree and `root` holds only the zoomed leaf, so reading
/// `root` there would silently drop every other pane from the record.
///
/// Scrollback is captured INLINE, so undo replays each pane's history as inert
/// text. No process is captured and none is resumed: restore spawns brand-new
/// PTYs at the recorded cwds.
///
/// A leaf holding a Diff surface serializes with an empty surface list and is
/// pruned out here, matching `capture_closed_pane_record`'s refusal to record
/// a diff pane at all. Nothing in this record round-trips a Diff view.
fn capture_closed_tab_record(
    tab: &crate::workspace::Tab,
    index: usize,
    workspace_id: u64,
    cx: &App,
) -> Option<crate::ClosedTabRecord> {
    let tree = tab.saved_layout.as_ref().or(tab.root.as_ref())?;
    let layout = prune_unrestorable(tree.serialize(cx))?;
    Some(crate::ClosedTabRecord {
        workspace_id,
        title: tab.title.clone(),
        index,
        layout,
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

/// Rebuild one surface from its undo record.
///
/// Borrows the record rather than consuming it so the caller still owns an
/// intact `ClosedPaneRecord` if a later step refuses:
/// `handle_undo_close_pane` has already POPPED, so a refusal that could not
/// hand the record back would destroy the only copy of the thing undo exists
/// to protect. The scrollback - the one big field - is only read here, never
/// moved, so borrowing costs nothing.
fn restore_closed_surface_record(
    tab: &ClosedSurfaceRecord,
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
            let terminal = cx.new(|cx| TerminalView::with_cwd(ws_id, cwd.clone(), None, cx));
            terminal.update(cx, |view, _| {
                view.terminal.custom_name = custom_name.clone();
                view.terminal.font_size_override = *font_size;
            });
            if let Some(scrollback) = scrollback {
                terminal.read(cx).restore_scrollback(scrollback);
            }
            cx.subscribe(&terminal, PaneFlowApp::handle_terminal_event)
                .detach();
            crate::pane::PaneSurface::Terminal(terminal)
        }
        ClosedSurfaceRecord::Markdown { path } => {
            let markdown = cx.new(|cx: &mut Context<crate::markdown::MarkdownView>| {
                crate::markdown::MarkdownView::open(path.clone(), cx)
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

    /// Close every open popover. Menus and only menus, deliberately: not one
    /// of these seven fields tracks focus, which is why this needs no `Window`
    /// and why its ~17 call sites - several of which have no `Window` to give
    /// it - are safe by construction.
    ///
    /// Issue #79 briefly parked the inline rename's three fields here, and
    /// that turned every one of those call sites into a focus-stranding path.
    /// The renamed sidebar row is the ONLY element that tracks
    /// `sidebar_rename_focus`, and it stops tracking it the instant that state
    /// clears; clearing it from a caller with no `Window` (the title-bar
    /// sidebar-collapse and menu toggles, the IPC/CLI `workspace.select` path)
    /// leaves the window with nothing focused at all. That is exactly the
    /// issue #108 state - the dispatch path collapses to the tree root and
    /// every global `context: None` binding matches but finds no handler - and
    /// nothing recovers from it, because this app registers no focus-lost
    /// listeners. Ending a rename therefore goes through
    /// [`Self::cancel_inline_rename`] or `commit_inline_rename`, both of which
    /// hand focus back. A caller with no `Window` leaves the editor drawn
    /// instead: a stale editor is a far smaller defect than dead keybindings.
    ///
    /// One route this does NOT close, because it is not this method's to
    /// close: hiding the primary sidebar unmounts the renamed row itself, so
    /// the handle it tracks leaves the dispatch tree even though no state was
    /// cleared. Fixing that needs a `Window` where the title-bar event is
    /// handled (a `subscribe_in` at the subscription site), or a
    /// `Window::on_focus_lost` fallback parking focus on
    /// `empty_workspace_focus` - which would cover every stranding route at
    /// once. Neither belongs inside a menu dismisser.
    pub(crate) fn dismiss_transient_surfaces(&mut self) {
        self.title_bar_files_menu_open = None;
        self.title_bar_help_menu_open = None;
        self.workspace_menu_open = None;
        self.tab_menu_open = None;
        self.pane_menu_open = None;
        self.profile_menu_open = None;
        self.files_menu_open = None;
    }

    /// End a live inline rename WITHOUT keeping the typed name, and hand focus
    /// back to the active pane (or to the empty-workspace placeholder).
    ///
    /// The `Window` is the entire point - see
    /// [`Self::dismiss_transient_surfaces`] for what happens without one. A
    /// no-op when no rename is live, so a caller that dismisses on every click
    /// does not also yank focus out from under whatever the user is doing.
    pub(crate) fn cancel_inline_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming_idx.is_none() && self.renaming_tab.is_none() {
            return;
        }
        self.renaming_idx = None;
        self.renaming_tab = None;
        self.rename_text.clear();
        self.restore_focus_after_rename(window, cx);
        cx.notify();
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
                self.focus_first_pane_or_placeholder(idx, window, cx)
            }
            WorkspaceFocusTarget::WaitingElseFirst => {
                // Bind before the `match` so the immutable borrow of
                // `self.workspaces` ends here - the `Some` arm needs `&mut`.
                let waiting = waiting_pane_in_workspace(&self.workspaces[idx], cx);
                match waiting {
                    Some((tab_idx, pane, sid)) => {
                        // Focus can only land on a rendered pane (the
                        // invariant `Workspace::focus_first` documents), so the
                        // owning tab has to become visible BEFORE the focus
                        // call. Same recipe as `attention_queue_activate`.
                        self.workspaces[idx].set_active_tab(tab_idx);
                        pane.update(cx, |_p, cx| cx.notify());
                        pane.read(cx).focus_handle(cx).focus(window, cx);
                        // Keep the jump cycle coherent: landing on a waiting
                        // pane counts as visiting it, so the next
                        // Cmd+Shift+J continues from here instead of
                        // restarting at the first waiting surface.
                        self.jump_cursor = Some(sid);
                    }
                    None => self.focus_first_pane_or_placeholder(idx, window, cx),
                }
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

    /// Issue #108: a workspace whose visible tab is empty has no pane to
    /// focus. Park focus on the placeholder instead, or the window ends up
    /// with nothing focused and every global `context: None` binding stops
    /// reaching its handler.
    ///
    /// Shared by [`WorkspaceFocusTarget::FirstPane`] and the "nothing is
    /// waiting" branch of [`WorkspaceFocusTarget::WaitingElseFirst`] so the
    /// two can never drift apart.
    fn focus_first_pane_or_placeholder(&self, idx: usize, window: &mut Window, cx: &mut App) {
        if !self.workspaces[idx].focus_first(window, cx) {
            window.focus(&self.empty_workspace_focus, cx);
        }
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
        // Issue #83: the pane this gesture would close, resolved once - the
        // guard below and the undo capture must agree on it.
        let closing_pane = self.active_workspace().and_then(|ws| {
            let root = ws.active_tab().root.as_ref()?;
            if ws.is_zoomed() {
                root.first_leaf()
            } else {
                root.focused_pane(window, cx)
            }
        });
        // Issue #83: ask first when that pane holds a live agent. Modal, like
        // the other three user close gestures: two adjacent gestures with
        // opposite safety behaviour is worse than either alone. The confirm
        // path removes the pane by identity instead of re-running the
        // focus-propagating close below, and re-focuses from the modal.
        if let Some(pane) = &closing_pane
            && self.arm_pending_close_pane(pane, crate::app::close_guard::ConfirmStyle::Modal, cx)
        {
            return;
        }

        // Capture state of the pane being closed for undo (US-014).
        // Must happen BEFORE the tree mutation that drops the pane entity.
        if let Some(ws) = self.active_workspace()
            && let Some(pane) = &closing_pane
        {
            let workspace_id = ws.id;
            if let Some(record) = capture_closed_pane_record(pane, workspace_id, cx) {
                push_closed_record(&mut self.closed_items, ClosedRecord::Pane(record));
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
            // Cannot report false here: the lines above just installed a leaf
            // root on this workspace's visible tab, so there is always a pane
            // to focus. No placeholder fallback is reachable.
            let _ = self.workspaces[self.active_idx].focus_first(window, cx);
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
        let Some(record) = self.closed_items.pop() else {
            self.show_toast("Nothing to restore", cx);
            return; // The undo stack is empty
        };
        match record {
            ClosedRecord::Pane(record) => self.restore_closed_pane(record, window, cx),
            ClosedRecord::Tab(record) => self.restore_closed_tab_record(record, window, cx),
        }
    }

    /// Rebuild a closed pane by splitting it back in beside the focused one.
    /// Unchanged behaviour: this is the original `UndoClosePane` body, lifted
    /// out so the action handler can branch on the record kind.
    fn restore_closed_pane(
        &mut self,
        record: ClosedPaneRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Restore into the workspace the pane was closed in. Indexes shift
        // on close/reorder; the record stores a stable Workspace.id.
        let ids: Vec<u64> = self.workspaces.iter().map(|ws| ws.id).collect();
        let Some(idx) = workspace_index_for_undo(&ids, record.workspace_id) else {
            // The ONE refusal that drops the record instead of putting it
            // back. `NEXT_WORKSPACE_ID` is a monotonic `fetch_add`, so a
            // recreated workspace never reuses this id and the lookup above
            // can never match again: re-pushing would re-promote a record
            // nothing can ever restore to the top of the stack on every
            // `Cmd+Shift+T`, blocking the restorable ones beneath it and
            // outlasting them under FIFO eviction. The refusals below are
            // genuinely transient and still push.
            self.show_toast("Workspace no longer exists", cx);
            return;
        };
        self.active_idx = idx;
        let ws_id = record.workspace_id;
        let surface = restore_closed_surface_record(&record.surface, ws_id, cx);
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
            // Unreachable: `idx` indexes `self.workspaces` and the entity lease
            // stops it changing under this body. Kept as a fail-safe rather
            // than a panic - and it re-pushes, because the pop already
            // happened. Returning silently here would satisfy
            // `every_refused_restore_pushes_the_record_back` (which only
            // proves `toasts == pushes`) while losing the pane for good, which
            // is the hole the tab path next door had.
            log::warn!("undo close pane: the workspace vanished mid-restore");
            push_closed_record(&mut self.closed_items, ClosedRecord::Pane(record));
            self.show_toast("Could not restore the pane", cx);
            return;
        }
        new_pane.read(cx).focus_handle(cx).focus(window, cx);

        self.save_session(cx);
        cx.notify();
    }

    /// Rebuild a closed tab in place, spawning a fresh pane per recorded leaf,
    /// and focus it.
    ///
    /// Returns nothing on purpose: the restored index has no consumer outside
    /// this body - the focus move it existed for happens here - and an
    /// `Option<usize>` nobody reads invites a future caller to mistake it for
    /// "did the restore succeed?".
    ///
    /// Restore is a *new* tab holding *new* PTYs: recorded scrollback replays
    /// as inert text and no agent process is resumed. Focus lands on the
    /// restored tab's first leaf - `serialize_with` stamps `focus: true` on
    /// every leaf and `from_layout_node` never reads it, so there is no
    /// recorded per-pane focus to honour.
    ///
    /// Every TRANSIENT refusal pushes `record` back onto the undo stack and
    /// toasts: [`Self::handle_undo_close_pane`] pops before it dispatches, so
    /// simply returning would consume the record and lose the tab for good.
    /// Two of those three are unreachable today (the entity lease stops
    /// `self.workspaces` mutating mid-body, and the cap was checked above),
    /// but the guard beside this only proves `toasts == pushes + drops`, so a
    /// refusal that returned silently would satisfy it while destroying the
    /// record.
    ///
    /// The fourth refusal - the workspace is gone - deliberately DROPS the
    /// record: its id is never reissued, so nothing could ever restore it.
    fn restore_closed_tab_record(
        &mut self,
        record: crate::ClosedTabRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Indexes shift on close/reorder; the record stores a stable
        // `Workspace.id`.
        let ids: Vec<u64> = self.workspaces.iter().map(|ws| ws.id).collect();
        let Some(ws_idx) = workspace_index_for_undo(&ids, record.workspace_id) else {
            // Dropped, not pushed back - see the matching arm in
            // `restore_closed_pane`: the workspace id is never reissued, so
            // this record is unrestorable and re-pushing it would wedge the
            // top of the undo stack permanently.
            self.show_toast("Workspace no longer exists", cx);
            return;
        };
        // `open_tab` fills an empty workspace's placeholder instead of pushing
        // past it, so a placeholder-only workspace always has room even though
        // `can_open_tab` reasons about the raw count.
        if !self.workspaces[ws_idx].can_open_tab() && !self.workspaces[ws_idx].is_empty_shell() {
            push_closed_record(&mut self.closed_items, ClosedRecord::Tab(record));
            self.show_toast("Tab limit reached", cx);
            return;
        }

        self.active_idx = ws_idx;
        let crate::ClosedTabRecord {
            workspace_id,
            title,
            index,
            layout,
        } = record;

        // Copy the workspace's identity out and own its cwd before building
        // the spawn closure: `ws`'s borrow of `self` has to end before the
        // closure captures `cx`, and `self` is re-acquired afterwards.
        let ws = &self.workspaces[ws_idx];
        let ws_id = ws.id;
        let fallback_cwd = std::path::PathBuf::from(&ws.cwd);
        // The deque starts EMPTY on purpose: `from_layout_node` reuses handed-in
        // panes verbatim and ignores their `SurfaceDefinition`, so any reuse
        // here would silently drop a leaf's cwd, custom name and scrollback.
        let mut pane_deque = std::collections::VecDeque::new();
        let root = LayoutTree::from_layout_node(&layout, &mut pane_deque, &mut |node| {
            let surfaces = match node {
                LayoutNode::Pane { surfaces } => surfaces.as_slice(),
                _ => &[],
            };
            Self::spawn_pane_from_surfaces(ws_id, surfaces, &fallback_cwd, cx)
        });

        // Rebuildable because `title` is cloned rather than moved and `layout`
        // was only borrowed above: the two refusals below hand the record back
        // intact instead of dropping it on the floor.
        let put_back = |this: &mut Self, cx: &mut Context<Self>| {
            push_closed_record(
                &mut this.closed_items,
                ClosedRecord::Tab(crate::ClosedTabRecord {
                    workspace_id,
                    title: title.clone(),
                    index,
                    layout: layout.clone(),
                }),
            );
            this.show_toast("Could not restore the tab", cx);
        };

        let tab = crate::workspace::Tab::new(title.clone(), Some(root));
        let Some(ws) = self.workspaces.get_mut(ws_idx) else {
            // Unreachable: `ws_idx` was resolved above and the entity lease
            // stops `self.workspaces` changing under this body. Kept as a
            // fail-safe rather than a panic.
            log::warn!("undo close tab: the workspace vanished mid-restore");
            put_back(self, cx);
            return;
        };
        if !ws.open_tab(tab) {
            // Unreachable: the cap was checked above and nothing can have
            // opened a tab in between. Kept as a fail-safe rather than a panic.
            log::warn!("undo close tab: workspace refused the tab after the cap check");
            put_back(self, cx);
            return;
        }
        // `open_tab` appends (or fills the placeholder); slide the newcomer
        // back to the slot it held. `reorder_tab` re-resolves the active tab
        // by id, so the restored tab stays the visible one.
        let last = ws.tab_count().saturating_sub(1);
        ws.reorder_tab(last, index.min(last));
        let tab_idx = ws.active_tab_idx();
        self.focus_workspace_tab(ws_idx, tab_idx, window, cx);
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
        // Issue #83: this close is deliberately NOT guarded - it drops every
        // tab, pane and terminal in the workspace with no confirmation and no
        // undo, reached from `Cmd+Shift+Q`, the sidebar folder x, the
        // workspace context menu and IPC `workspace.close`. Guarding it needs
        // a whole-workspace undo record, which is a separate feature; the
        // scope call was to leave it out of #83 rather than to overlook it.
        //
        // What it MUST do is take the state that dies with the workspace with
        // it. Undo records for this id can never resolve again
        // (`NEXT_WORKSPACE_ID` never reissues one), and a pending
        // confirmation whose target lives here is asking about something that
        // is about to stop existing.
        let closed_id = self.workspaces[idx].id;
        drop_closed_records_for_workspace(&mut self.closed_items, closed_id);
        // The MODAL half is left to the render stand-down when there is no
        // `Window`: `cancel_pending_close` is what hands focus back, and
        // dropping a focused modal without one strands the window (issue
        // #108). An inline arm never took focus, so it can always go here.
        let has_window = window.is_some();
        let stand_down_pending = self.pending_close.as_ref().is_some_and(|pending| {
            (has_window || pending.style == crate::app::close_guard::ConfirmStyle::Inline)
                && self.pending_close_targets_workspace(pending, idx)
        });
        if stand_down_pending {
            self.set_pending_close(None, cx);
        }
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
        if let Some(window) = window {
            // Issue #108: the workspace we land on may have an empty visible
            // tab, and closing the last workspace leaves none at all. Both
            // render a placeholder with no pane to focus, so park focus there
            // rather than leaving the window with nothing focused.
            let focused = match self.workspaces.get(self.active_idx) {
                Some(ws) => ws.focus_first(window, cx),
                None => false,
            };
            if !focused {
                window.focus(&self.empty_workspace_focus, cx);
            }
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

    /// The single chokepoint for Cmd+1..9 (`handle_ws1`..`handle_ws9` all
    /// funnel through here). Issue #78: an explicit "go to workspace N"
    /// gesture lands on the pane whose agent is waiting for input when there
    /// is one, so clicking through an "Input" badge reaches the agent instead
    /// of whatever pane happens to sort first. Tab-selection gestures keep
    /// plain `select_workspace`: routing there would undo the tab the user
    /// just picked.
    pub(crate) fn handle_select_ws(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_workspace_at(idx, WorkspaceFocusTarget::WaitingElseFirst, window, cx);
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
    validate_reveal_path(path)?;
    spawn_detached(std::process::Command::new("open").arg(path))
        .map_err(|err| format!("Could not open Finder: {err}"))
}

/// Reject a path Finder could not show before spawning `open`: a missing
/// workspace directory (deleted worktree) or a file where a folder was
/// expected. `open <file>` would launch the file's default app rather than
/// Finder, and `open <missing>` fails after the spawn, where nothing reports
/// it. Split out of [`reveal_in_file_manager`] so the toast copy is testable
/// without launching Finder.
fn validate_reveal_path(path: &std::path::Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!(
            "Could not open Finder: {} does not exist",
            path.display()
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "Could not open Finder: {} is not a folder",
            path.display()
        ));
    }
    Ok(())
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

    // ═══════════════════════════════════════════════════════════════════
    // Issue #78: workspace-level selection targets the waiting pane.
    //
    // `PaneFlowApp` is not constructible in a test, so the routing rule
    // lives in the free `waiting_pane_in_workspace` and is exercised
    // directly. Panes are real (`display_only_for_test` - no PTY).
    // ═══════════════════════════════════════════════════════════════════

    fn waiting_test_pane(
        cx: &mut gpui::VisualTestContext,
    ) -> (gpui::Entity<crate::pane::Pane>, u64) {
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        let surface_id = terminal.entity_id().as_u64();
        let pane = cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx));
        (pane, surface_id)
    }

    fn session_waiting_on(surface_id: Option<u64>) -> crate::ai_types::AgentSession {
        let mut session = crate::ai_types::AgentSession::new(
            crate::agent_launcher::TerminalAgent::ClaudeCode,
            crate::ai_types::AgentState::WaitingForInput,
        );
        session.surface_id = surface_id;
        session
    }

    #[gpui::test]
    fn workspace_with_a_waiting_pane_targets_that_pane(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let (first_pane, first_sid) = waiting_test_pane(cx);
        let (waiting_pane, waiting_sid) = waiting_test_pane(cx);
        assert_ne!(first_sid, waiting_sid, "two distinct surfaces");

        let root = LayoutTree::from_panes_equal(
            SplitDirection::Vertical,
            vec![first_pane.clone(), waiting_pane.clone()],
        )
        .expect("two panes build a container");
        let mut ws = Workspace::with_layout_and_id(1, "ws", std::path::PathBuf::new(), root);
        ws.agent_sessions
            .insert(4321u32, session_waiting_on(Some(waiting_sid)));

        let found = cx.update(|_, cx| waiting_pane_in_workspace(&ws, cx));
        let (tab_idx, pane, sid) = found.expect("the waiting pane must be found");
        assert_eq!(tab_idx, 0, "the only tab");
        assert_eq!(sid, waiting_sid, "reports the waiting surface id");
        assert!(
            pane == waiting_pane,
            "must target the waiting pane, not the first leaf"
        );
        assert!(pane != first_pane, "the first leaf is not the target");
    }

    #[gpui::test]
    fn workspace_without_a_waiting_pane_yields_none(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let (pane, sid) = waiting_test_pane(cx);
        let mut ws = Workspace::with_layout_and_id(
            1,
            "ws",
            std::path::PathBuf::new(),
            LayoutTree::Leaf(pane),
        );
        // A live session that is NOT waiting must not match, or every
        // selection would hijack the caller's first-pane fallback.
        let mut thinking = crate::ai_types::AgentSession::new(
            crate::agent_launcher::TerminalAgent::ClaudeCode,
            crate::ai_types::AgentState::Thinking,
        );
        thinking.surface_id = Some(sid);
        ws.agent_sessions.insert(4321u32, thinking);

        assert!(
            cx.update(|_, cx| waiting_pane_in_workspace(&ws, cx))
                .is_none(),
            "nothing is waiting, so the caller must fall back to the first pane"
        );
    }

    #[gpui::test]
    fn waiting_pane_in_a_background_tab_reports_its_tab(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let (visible_pane, _visible_sid) = waiting_test_pane(cx);
        let (hidden_pane, hidden_sid) = waiting_test_pane(cx);

        let mut ws = Workspace::with_layout_and_id(
            1,
            "ws",
            std::path::PathBuf::new(),
            LayoutTree::Leaf(visible_pane),
        );
        assert!(ws.open_tab(crate::workspace::Tab::new(
            "background",
            Some(LayoutTree::Leaf(hidden_pane.clone())),
        )));
        // Make tab 0 visible so the waiting pane genuinely sits out of sight.
        ws.set_active_tab(0);
        ws.agent_sessions
            .insert(4321u32, session_waiting_on(Some(hidden_sid)));

        let (tab_idx, pane, sid) = cx
            .update(|_, cx| waiting_pane_in_workspace(&ws, cx))
            .expect("a waiting agent in a background tab must still be found");
        assert_eq!(tab_idx, 1, "reports the owning tab, not the visible one");
        assert_eq!(sid, hidden_sid);
        assert!(pane == hidden_pane);
    }

    #[gpui::test]
    fn waiting_session_without_a_resolved_surface_is_skipped(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let (pane, _sid) = waiting_test_pane(cx);
        let mut ws = Workspace::with_layout_and_id(
            1,
            "ws",
            std::path::PathBuf::new(),
            LayoutTree::Leaf(pane),
        );
        // The hook PID never resolved to a pane.
        ws.agent_sessions.insert(4321u32, session_waiting_on(None));

        assert!(
            cx.update(|_, cx| waiting_pane_in_workspace(&ws, cx))
                .is_none(),
            "an unresolved waiting session must never target a guessed pane"
        );
    }

    // Pure-Rust tests only - spawning actual binaries is brittle in CI
    // (macOS runners may not have `open` on PATH under a non-GUI session).
    // The validation step in front of the spawn is exercised directly so
    // the toast copy can't drift silently.

    #[test]
    fn reveal_accepts_regular_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(validate_reveal_path(tmp.path()), Ok(()));
    }

    #[test]
    fn reveal_rejects_missing_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("paneflow-no-such-dir");
        let err = validate_reveal_path(&missing).unwrap_err();
        assert!(
            err.starts_with("Could not open Finder: "),
            "toast copy drifted: {err}"
        );
        assert!(err.ends_with("does not exist"), "toast copy drifted: {err}");
        assert!(
            err.contains(&missing.display().to_string()),
            "toast must name the path: {err}"
        );
    }

    #[test]
    fn reveal_rejects_file_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("plain.txt");
        std::fs::write(&file, b"x").unwrap();
        let err = validate_reveal_path(&file).unwrap_err();
        assert!(
            err.starts_with("Could not open Finder: "),
            "toast copy drifted: {err}"
        );
        assert!(
            err.ends_with("is not a folder"),
            "toast copy drifted: {err}"
        );
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

    fn closed_pane_record_with_scrollback(len: usize) -> ClosedRecord {
        ClosedRecord::Pane(ClosedPaneRecord {
            surface: ClosedSurfaceRecord::Terminal {
                cwd: None,
                scrollback: Some("x".repeat(len)),
                custom_name: None,
                font_size: None,
            },
            workspace_id: 0,
        })
    }

    /// A pane record carrying no scrollback, tagged so eviction order is
    /// observable.
    fn closed_pane_record(workspace_id: u64) -> ClosedRecord {
        ClosedRecord::Pane(ClosedPaneRecord {
            surface: ClosedSurfaceRecord::Terminal {
                cwd: None,
                scrollback: None,
                custom_name: None,
                font_size: None,
            },
            workspace_id,
        })
    }

    /// A tab record whose layout is a one-level split of `sizes.len()` leaves,
    /// leaf `i` holding `sizes[i]` bytes of scrollback.
    fn closed_tab_record(workspace_id: u64, sizes: &[usize]) -> ClosedRecord {
        use paneflow_config::schema::{LayoutNode, SurfaceDefinition};
        ClosedRecord::Tab(crate::ClosedTabRecord {
            workspace_id,
            title: "tab".to_string(),
            index: 0,
            layout: LayoutNode::Split {
                direction: "vertical".to_string(),
                ratio: None,
                ratios: None,
                children: sizes
                    .iter()
                    .map(|len| LayoutNode::Pane {
                        surfaces: vec![SurfaceDefinition {
                            scrollback: Some("x".repeat(*len)),
                            ..Default::default()
                        }],
                    })
                    .collect(),
            },
        })
    }

    /// Every leaf's scrollback length in traversal order; `None` for a leaf
    /// whose history the budget released.
    fn leaf_scrollback_lens(node: &paneflow_config::schema::LayoutNode) -> Vec<Option<usize>> {
        use paneflow_config::schema::LayoutNode;
        match node {
            LayoutNode::Pane { surfaces } => surfaces
                .iter()
                .map(|surface| surface.scrollback.as_ref().map(String::len))
                .collect(),
            LayoutNode::Split { children, .. } => {
                children.iter().flat_map(leaf_scrollback_lens).collect()
            }
        }
    }

    fn record_workspace_ids(records: &[ClosedRecord]) -> Vec<u64> {
        records
            .iter()
            .map(|record| match record {
                ClosedRecord::Pane(pane) => pane.workspace_id,
                ClosedRecord::Tab(tab) => tab.workspace_id,
            })
            .collect()
    }

    /// Nested-split scrollback must be summed leaf by leaf: a tab record
    /// stores a whole tree, so a walk that stopped at the root would report
    /// zero for every layout that is not a bare single pane.
    #[test]
    fn layout_node_scrollback_bytes_sums_nested_split_leaves() {
        use paneflow_config::schema::{LayoutNode, SurfaceDefinition};

        fn leaf(scrollback: Option<&str>) -> LayoutNode {
            LayoutNode::Pane {
                surfaces: vec![SurfaceDefinition {
                    scrollback: scrollback.map(str::to_string),
                    ..Default::default()
                }],
            }
        }
        fn split(children: Vec<LayoutNode>) -> LayoutNode {
            LayoutNode::Split {
                direction: "vertical".to_string(),
                ratio: None,
                ratios: None,
                children,
            }
        }

        assert_eq!(layout_node_scrollback_bytes(&leaf(None)), 0);
        assert_eq!(layout_node_scrollback_bytes(&leaf(Some("abcd"))), 4);
        // A surface-less leaf (the shape a Diff pane serializes to) is zero.
        assert_eq!(
            layout_node_scrollback_bytes(&LayoutNode::Pane { surfaces: vec![] }),
            0
        );

        let nested = split(vec![
            leaf(Some("aa")),
            split(vec![leaf(Some("bbb")), leaf(None), leaf(Some("cccc"))]),
        ]);
        assert_eq!(
            layout_node_scrollback_bytes(&nested),
            9,
            "every leaf of every depth contributes"
        );
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

        push_closed_record(&mut records, closed_pane_record_with_scrollback(one_mib));

        assert_eq!(records.len(), 3, "budget must preserve undo records");
        assert!(
            matches!(
                &records[0],
                ClosedRecord::Pane(ClosedPaneRecord {
                    surface: ClosedSurfaceRecord::Terminal {
                        scrollback: None,
                        ..
                    },
                    ..
                })
            ),
            "oldest scrollback should be released first"
        );
        assert!(matches!(
            &records[1],
            ClosedRecord::Pane(ClosedPaneRecord {
                surface: ClosedSurfaceRecord::Terminal {
                    scrollback: Some(_),
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            &records[2],
            ClosedRecord::Pane(ClosedPaneRecord {
                surface: ClosedSurfaceRecord::Terminal {
                    scrollback: Some(_),
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            closed_pane_scrollback_bytes(&records),
            MAX_CLOSED_PANE_SCROLLBACK_BYTES
        );
    }

    /// A terminal pane at a known cwd with a known custom name.
    fn named_test_pane(
        cx: &mut gpui::VisualTestContext,
        custom_name: &str,
        cwd: &str,
    ) -> gpui::Entity<crate::pane::Pane> {
        let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
        terminal.update(cx, |view, _| {
            view.terminal.custom_name = Some(custom_name.to_string());
            view.terminal.current_cwd = Some(cwd.to_string());
        });
        cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx))
    }

    /// Rehydrate a captured record the way `restore_closed_tab_record` does -
    /// an EMPTY deque, so every leaf goes through `spawn` - and report the
    /// `SurfaceDefinition` each leaf would have been rebuilt from.
    fn respawned_surfaces(
        cx: &mut gpui::VisualTestContext,
        layout: &LayoutNode,
    ) -> Vec<paneflow_config::schema::SurfaceDefinition> {
        let mut seen = Vec::new();
        let tree = LayoutTree::from_layout_node(
            layout,
            &mut std::collections::VecDeque::new(),
            &mut |node| {
                if let LayoutNode::Pane { surfaces } = node {
                    seen.push(surfaces.first().cloned().unwrap_or_default());
                }
                let terminal = cx.new(|cx| TerminalView::display_only_for_test(1, cx));
                cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx))
            },
        );
        assert_eq!(
            tree.leaf_count(),
            seen.len(),
            "an empty deque must respawn every leaf"
        );
        seen
    }

    /// Capture is a whole-tree snapshot: two panes go in, two leaves come out,
    /// and each leaf carries the cwd and custom name of the pane it came from
    /// so the restore spawns them back where they were.
    #[gpui::test]
    fn capture_closed_tab_record_round_trips_a_two_pane_tab(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let left = named_test_pane(cx, "agent", "/tmp/agent");
        let right = named_test_pane(cx, "logs", "/tmp/logs");
        let tree = LayoutTree::from_panes_equal(SplitDirection::Vertical, vec![left, right])
            .expect("two panes make a tree");
        let tab = crate::workspace::Tab::new("Agents", Some(tree));

        let record = cx
            .update(|_, cx| capture_closed_tab_record(&tab, 3, 42, cx))
            .expect("a two-pane tab is worth recording");

        assert_eq!(record.title, "Agents");
        assert_eq!(record.index, 3, "the tab remembers the slot it held");
        assert_eq!(record.workspace_id, 42);
        assert_eq!(record.layout.leaf_count(), 2);

        let surfaces = respawned_surfaces(cx, &record.layout);
        assert_eq!(surfaces.len(), 2);
        assert_eq!(surfaces[0].custom_name.as_deref(), Some("agent"));
        assert_eq!(surfaces[0].cwd.as_deref(), Some("/tmp/agent"));
        assert_eq!(surfaces[1].custom_name.as_deref(), Some("logs"));
        assert_eq!(surfaces[1].cwd.as_deref(), Some("/tmp/logs"));
    }

    /// While a tab is zoomed, `root` holds ONLY the zoomed leaf and
    /// `saved_layout` holds the full tree. Capturing `root` there would throw
    /// away every other pane, so capture has to prefer `saved_layout`.
    #[gpui::test]
    fn capture_closed_tab_record_captures_the_full_tree_while_zoomed(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let left = named_test_pane(cx, "agent", "/tmp/agent");
        let right = named_test_pane(cx, "logs", "/tmp/logs");
        let full =
            LayoutTree::from_panes_equal(SplitDirection::Vertical, vec![left.clone(), right])
                .expect("two panes make a tree");
        let mut tab = crate::workspace::Tab::new("Zoomed", Some(full));
        // Exactly the shape `toggle_zoom` leaves behind.
        tab.saved_layout = tab.root.take();
        tab.root = Some(LayoutTree::Leaf(left));
        assert!(tab.is_zoomed());

        let record = cx
            .update(|_, cx| capture_closed_tab_record(&tab, 0, 42, cx))
            .expect("a zoomed tab is still worth recording");

        assert_eq!(
            record.layout.leaf_count(),
            2,
            "the zoom-saved tree is the whole tab, not just the zoomed leaf"
        );
    }

    /// A Diff pane is derived state, not a document: `capture_closed_pane_record`
    /// already refuses to record one, and a tab holding a diff must not smuggle
    /// it back in. The chain has three links - `serialize_with` empties the diff
    /// surface but still emits the leaf, `prune_unrestorable` drops that leaf,
    /// and capture runs the two in that order.
    ///
    /// Links two and three are covered by the `prune_unrestorable_*` tests
    /// above; this pins links one and three. It is deliberately NOT a
    /// `#[gpui::test]`: constructing a real `DiffView` starts a filesystem
    /// watcher through `smol::unblock`, which the GPUI test scheduler rejects
    /// as non-deterministic and which aborts the whole test binary.
    #[test]
    fn capture_closed_tab_record_prunes_the_leaf_a_diff_pane_serializes_to() {
        let serde_src = include_str!("../../layout/serde.rs");
        assert!(
            serde_src
                .contains(r#".filter(|surface| surface.surface_type.as_deref() != Some("diff"))"#),
            "a diff surface is filtered out of the serialized leaf"
        );
        assert!(
            serde_src.contains("LayoutNode::Pane { surfaces }"),
            "the leaf itself is still emitted, which is why pruning is needed"
        );

        let src = include_str!("mod.rs");
        let body = src
            .split("fn capture_closed_tab_record(")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("capture_closed_tab_record body");
        assert!(
            body.contains("prune_unrestorable(tree.serialize(cx))"),
            "capture must prune the serialized tree before storing it"
        );
    }

    /// A tab holding nothing restorable records nothing, so `Cmd+Shift+T`
    /// never resurrects it as a bare shell.
    #[gpui::test]
    fn capture_closed_tab_record_declines_an_empty_tab(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let tab = crate::workspace::Tab::empty();

        let record = cx.update(|_, cx| capture_closed_tab_record(&tab, 0, 42, cx));

        assert!(record.is_none(), "an empty tab has nothing to restore");
    }

    fn pane_leaf(names: &[&str]) -> LayoutNode {
        LayoutNode::Pane {
            surfaces: names
                .iter()
                .map(|name| paneflow_config::schema::SurfaceDefinition {
                    custom_name: Some((*name).to_string()),
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn split_node(ratios: Option<Vec<f64>>, children: Vec<LayoutNode>) -> LayoutNode {
        LayoutNode::Split {
            direction: "vertical".to_string(),
            ratio: None,
            ratios,
            children,
        }
    }

    /// A Diff pane serializes to a leaf with an EMPTY surface list: the diff
    /// `SurfaceDefinition` is filtered out but the leaf itself survives. Left
    /// in the record it would restore as a plain shell, which contradicts the
    /// pane-level rule that a diff surface is not restorable at all. Pruning
    /// drops the leaf, and a split left holding one child collapses into it.
    #[test]
    fn prune_unrestorable_drops_surfaceless_leaves_and_collapses_splits() {
        let pruned = prune_unrestorable(split_node(
            Some(vec![0.25, 0.75]),
            vec![LayoutNode::Pane { surfaces: vec![] }, pane_leaf(&["shell"])],
        ));

        assert_eq!(
            pruned,
            Some(pane_leaf(&["shell"])),
            "a two-child split with one unrestorable leaf collapses to the survivor"
        );

        // Nested: the inner split collapses to one leaf, then the outer split
        // collapses onto it too.
        let nested = split_node(
            Some(vec![0.5, 0.5]),
            vec![
                LayoutNode::Pane { surfaces: vec![] },
                split_node(
                    Some(vec![0.5, 0.5]),
                    vec![LayoutNode::Pane { surfaces: vec![] }, pane_leaf(&["deep"])],
                ),
            ],
        );
        assert_eq!(prune_unrestorable(nested), Some(pane_leaf(&["deep"])));

        // Three children, one unrestorable: the split survives with its two
        // remaining children and their matching ratios.
        let three = split_node(
            Some(vec![0.2, 0.3, 0.5]),
            vec![
                pane_leaf(&["a"]),
                LayoutNode::Pane { surfaces: vec![] },
                pane_leaf(&["c"]),
            ],
        );
        assert_eq!(
            prune_unrestorable(three),
            Some(split_node(
                Some(vec![0.2, 0.5]),
                vec![pane_leaf(&["a"]), pane_leaf(&["c"])]
            )),
            "surviving children keep the ratios that belonged to them"
        );
    }

    /// A tab holding nothing but diff panes records nothing at all - the same
    /// answer `capture_closed_pane_record` already gives for a single one.
    #[test]
    fn prune_unrestorable_returns_none_for_a_wholly_unrestorable_tree() {
        assert_eq!(
            prune_unrestorable(LayoutNode::Pane { surfaces: vec![] }),
            None
        );
        assert_eq!(
            prune_unrestorable(split_node(
                Some(vec![0.5, 0.5]),
                vec![
                    LayoutNode::Pane { surfaces: vec![] },
                    LayoutNode::Pane { surfaces: vec![] },
                ],
            )),
            None
        );
        assert_eq!(prune_unrestorable(split_node(None, vec![])), None);
    }

    /// The LEGACY scalar `ratio` is binary-split-only: `resolved_ratios` reads
    /// it as `[ratio, 1 - ratio]`. Carrying it through a prune that changed the
    /// child count silently reassigns widths - a 4-child split with
    /// `ratio: Some(0.8)` pruned to 2 children would resolve to `[0.8, 0.2]`
    /// where it previously resolved to equal shares.
    ///
    /// `serialize_with` always emits `ratio: None`, so this is unreachable from
    /// the capture path today. It is reachable the moment anything hands this
    /// PURE function a tree read off a session file, which is exactly the shape
    /// `LayoutNode` exists to carry.
    #[test]
    fn prune_unrestorable_drops_a_legacy_ratio_when_the_child_count_changes() {
        let legacy = |ratio: Option<f64>, children: Vec<LayoutNode>| LayoutNode::Split {
            direction: "vertical".to_string(),
            ratio,
            ratios: None,
            children,
        };

        let pruned = prune_unrestorable(legacy(
            Some(0.8),
            vec![
                pane_leaf(&["a"]),
                LayoutNode::Pane { surfaces: vec![] },
                pane_leaf(&["c"]),
                pane_leaf(&["d"]),
            ],
        ));
        assert_eq!(
            pruned,
            Some(legacy(
                None,
                vec![pane_leaf(&["a"]), pane_leaf(&["c"]), pane_leaf(&["d"])]
            )),
            "a legacy binary ratio must not survive onto a split of a different width"
        );

        // A split that lost nothing keeps its legacy ratio: it still describes
        // the same two children.
        let untouched = legacy(Some(0.8), vec![pane_leaf(&["a"]), pane_leaf(&["b"])]);
        assert_eq!(
            prune_unrestorable(untouched.clone()),
            Some(untouched),
            "an unchanged split still means what it said"
        );
    }

    /// Pruning is a no-op on a tree with nothing to prune: direction, ratios
    /// and surfaces all come back untouched.
    #[test]
    fn prune_unrestorable_passes_a_healthy_tree_through_unchanged() {
        let healthy = split_node(
            Some(vec![0.3, 0.7]),
            vec![
                pane_leaf(&["left"]),
                split_node(None, vec![pane_leaf(&["top"]), pane_leaf(&["bottom"])]),
            ],
        );

        assert_eq!(prune_unrestorable(healthy.clone()), Some(healthy));
    }

    /// Issue #83: `handle_undo_close_pane` POPS before it dispatches, so a
    /// restore that then refuses has already consumed the record - and the
    /// pane or tab it described is gone for good. Every TRANSIENT refusal
    /// therefore has to push the record back before it toasts.
    ///
    /// The single exception is the workspace-is-gone arm. That id is never
    /// reissued, so the record is unrestorable; pushing it back would put an
    /// entry nothing can ever use straight back on top of the stack, where it
    /// blocks the restorable records beneath it and outlives them under FIFO
    /// eviction. Each body therefore gets exactly one such arm, and its toast
    /// is the one that is NOT paired with a push.
    ///
    /// `PaneFlowApp` is not constructible in a test, so this pins the shape
    /// rather than the behaviour.
    #[test]
    fn every_transient_refusal_pushes_the_record_back_and_the_orphan_drops() {
        let src = include_str!("mod.rs");
        for (start, end) in [
            ("fn restore_closed_pane(", "fn restore_closed_tab_record("),
            (
                "fn restore_closed_tab_record(",
                "pub(crate) fn handle_new_workspace(",
            ),
        ] {
            let body = src
                .split(start)
                .nth(1)
                .and_then(|rest| rest.split(end).next())
                .unwrap_or_else(|| panic!("{start} body"));
            // `.show_toast(` rather than `self.show_toast(`: the shared
            // re-push helper inside `restore_closed_tab_record` toasts through
            // its own `&mut Self` binding, and a needle that missed it would
            // read the pair as unbalanced.
            let toasts = body.matches(".show_toast(").count();
            let pushes = body.matches("push_closed_record(").count();
            // The one deliberate drop: the workspace lookup that failed.
            let orphan_drops = body.matches("workspace_index_for_undo(").count();
            assert_eq!(
                orphan_drops, 1,
                "{start}: exactly one refusal may drop the record - the workspace-is-gone arm"
            );
            assert!(toasts > 0, "{start}: expected at least one refusal path");
            assert_eq!(
                pushes + orphan_drops,
                toasts,
                "{start}: every refusal toast must be paired with a re-push, except the one                  orphan drop"
            );
        }
    }

    /// Issue #83: closing a workspace has to take the undo records and the
    /// pending confirmation that die with it.
    ///
    /// Both are behavioural on a `PaneFlowApp` that cannot be constructed in a
    /// test, so the ORDER is what is pinned here: the id has to be read - and
    /// the pending close resolved against the live workspace - before
    /// `workspaces.remove` drops it.
    #[test]
    fn closing_a_workspace_prunes_the_undo_stack_and_stands_down_its_pending_close() {
        let src = include_str!("mod.rs");
        let body = src
            .split("fn close_workspace_at_inner(")
            .nth(1)
            .and_then(|rest| rest.split("\n    /// Move a workspace").next())
            .expect("close_workspace_at_inner body");
        let prune_at = body
            .find("drop_closed_records_for_workspace(")
            .expect("the close must prune the undo stack");
        let stand_down_at = body
            .find("pending_close_targets_workspace(")
            .expect("the close must stand a pending confirmation down");
        assert!(
            body.contains("set_pending_close(None"),
            "the stand-down must go through the single writer: {body}"
        );
        let remove_at = body
            .find("self.workspaces.remove(idx)")
            .expect("the close must remove the workspace");
        assert!(
            prune_at < remove_at && stand_down_at < remove_at,
            "both read the workspace being destroyed, so both have to run before it is              dropped: {body}"
        );
    }

    /// The other half of the guard above, for the pane path only: a balanced
    /// toast/push count is still satisfied by a refusal that returns
    /// SILENTLY, which is exactly the hole `restore_closed_pane` had. Its
    /// `return;`s are therefore counted too - every one has to be either a
    /// re-push or the single deliberate orphan drop.
    ///
    /// Asserted only for the pane path: `restore_closed_tab_record` funnels
    /// TWO of its four refusals through one shared `put_back` closure, so its
    /// literal push count is one short of its return count by construction.
    #[test]
    fn restore_closed_pane_has_no_silent_early_return() {
        let src = include_str!("mod.rs");
        let body = src
            .split("fn restore_closed_pane(")
            .nth(1)
            .and_then(|rest| rest.split("fn restore_closed_tab_record(").next())
            .expect("restore_closed_pane body");
        assert_eq!(
            body.matches("return;").count(),
            body.matches("push_closed_record(").count()
                + body.matches("workspace_index_for_undo(").count(),
            "`handle_undo_close_pane` pops before it dispatches, so a bare `return` here \
             destroys the only copy of the pane undo exists to protect: {body}"
        );
    }

    /// Issue #83: a workspace that closes takes its undo records with it.
    ///
    /// The id is never reissued, so those records can never be restored; a
    /// refusal that pushed one back would re-promote it to newest on every
    /// `Cmd+Shift+T`, blocking the restorable records beneath it for good.
    #[test]
    fn closing_a_workspace_drops_only_its_own_undo_records() {
        let mut records = vec![
            closed_pane_record(1),
            closed_tab_record(2, &[]),
            closed_pane_record(2),
            closed_tab_record(3, &[]),
        ];

        drop_closed_records_for_workspace(&mut records, 2);

        assert_eq!(
            record_workspace_ids(&records),
            vec![1, 3],
            "every record for the destroyed workspace goes, and only those - the survivors keep              their stack order, so undo still walks newest-first"
        );

        // A workspace with nothing on the stack is a clean no-op.
        drop_closed_records_for_workspace(&mut records, 99);
        assert_eq!(record_workspace_ids(&records), vec![1, 3]);
    }

    /// The record cap counts whole entries regardless of kind: a stack of
    /// five mixed pane/tab records drops its oldest entry to admit a sixth.
    #[test]
    fn push_closed_record_evicts_the_oldest_of_a_mixed_stack() {
        let mut records = Vec::new();
        for i in 0..MAX_CLOSED_PANES as u64 {
            let record = if i % 2 == 0 {
                closed_pane_record(i)
            } else {
                closed_tab_record(i, &[])
            };
            push_closed_record(&mut records, record);
        }
        assert_eq!(records.len(), MAX_CLOSED_PANES);
        assert_eq!(record_workspace_ids(&records), vec![0, 1, 2, 3, 4]);

        push_closed_record(&mut records, closed_tab_record(99, &[]));

        assert_eq!(records.len(), MAX_CLOSED_PANES, "the cap is a hard ceiling");
        assert_eq!(
            record_workspace_ids(&records),
            vec![1, 2, 3, 4, 99],
            "the oldest whole record is evicted, whatever kind it is"
        );
    }

    /// A tab record can hold far more scrollback than the whole budget, so the
    /// sweep has to release its leaves one at a time and stop as soon as it is
    /// back under. Clearing the record wholesale would hand the user back a
    /// six-pane tab with no history anywhere in it.
    #[test]
    fn closed_tab_budget_strips_leaves_oldest_first_and_keeps_the_rest() {
        let mut records = vec![closed_tab_record(7, &[100; 6])];
        assert_eq!(closed_pane_scrollback_bytes(&records), 600);

        enforce_closed_pane_scrollback_budget(&mut records, 350);

        let ClosedRecord::Tab(tab) = &records[0] else {
            panic!("the record itself must survive the budget");
        };
        assert_eq!(
            leaf_scrollback_lens(&tab.layout),
            vec![None, None, None, Some(100), Some(100), Some(100)],
            "only enough leading leaves are released to get under budget"
        );
        assert_eq!(closed_pane_scrollback_bytes(&records), 300);
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
        push_closed_record(&mut records, closed_pane_record(0));

        assert_eq!(records.len(), 1);
        assert!(matches!(
            &records[0],
            ClosedRecord::Pane(ClosedPaneRecord {
                surface: ClosedSurfaceRecord::Terminal {
                    scrollback: None,
                    ..
                },
                ..
            })
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
