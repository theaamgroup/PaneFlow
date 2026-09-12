//! Workspace-tab handlers (add/close/select/rename/move) for `PaneFlowApp`.
//!
//! EP-002 US-005 (prd-cli-tab-hierarchy): `New tab` / `Close tab` used to add
//! and close a tab *inside* a pane. A pane is now mono-surface, so multiplicity
//! moved one level up: they operate on workspace tabs, and the workspace always
//! keeps at least one (FR-01).
//!
//! EP-003 (US-009 / US-010 / US-011) adds the sidebar-driven half of the same
//! lifecycle - select, create, close, inline rename, reorder, reattach - so the
//! keyboard actions and the sidebar rows share one implementation instead of
//! two drifting ones.

use gpui::{AppContext, Context, Window};
use paneflow_config::schema::TerminalSurfaceProfile;

use crate::layout::LayoutTree;
use crate::terminal::TerminalView;
use crate::workspace::Tab;
use crate::{CloseTab, ClosedRecord, NewTab, NextTab, PaneFlowApp, PreviousTab, TabDrag};

use super::capture_closed_tab_record;

/// Carry the `agent_sessions` rows of the surfaces that just left
/// `src_ws_idx` into `dest_ws_idx`'s registry (PR #410 review). The rows are
/// keyed by the pane's workspace, and a pane move used to leave them behind:
/// closing the source workspace then dropped a live agent's state, and the
/// sidebar kept the badge on the workspace the pane had left. Returns how
/// many rows moved. A key already present in the destination (a synthetic
/// band key, or a recycled PID) keeps the destination's row and drops the
/// mover, which the next hook frame recreates in place.
pub(crate) fn migrate_agent_sessions(
    workspaces: &mut [crate::workspace::Workspace],
    src_ws_idx: usize,
    dest_ws_idx: usize,
    surface_ids: &std::collections::HashSet<u64>,
) -> usize {
    if src_ws_idx == dest_ws_idx
        || surface_ids.is_empty()
        || src_ws_idx >= workspaces.len()
        || dest_ws_idx >= workspaces.len()
    {
        return 0;
    }
    let source = &mut workspaces[src_ws_idx].agent_sessions;
    let keys: Vec<u32> = source
        .iter()
        .filter(|(_, session)| {
            session
                .surface_id
                .is_some_and(|sid| surface_ids.contains(&sid))
        })
        .map(|(key, _)| *key)
        .collect();
    let moved: Vec<_> = keys
        .into_iter()
        .filter_map(|key| source.remove(&key).map(|session| (key, session)))
        .collect();
    let destination = &mut workspaces[dest_ws_idx].agent_sessions;
    let mut count = 0;
    for (key, session) in moved {
        match destination.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(session);
                count += 1;
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                log::warn!(
                    "pane move: agent session key {key} already present in the destination workspace; dropping the moved row"
                );
            }
        }
    }
    count
}

impl PaneFlowApp {
    pub(crate) fn toggle_workspace_muted(&mut self, ws_idx: usize, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.muted = !ws.muted;
            self.save_session(cx);
            cx.notify();
        }
    }

    pub(crate) fn workspace_is_muted(&self, ws_id: u64) -> bool {
        self.workspaces
            .iter()
            .find(|ws| ws.id == ws_id)
            .is_some_and(|ws| ws.muted)
    }

    /// Clear completion marks across all tabs; session attention badges have
    /// their own tab-level Mark as read action.
    pub(crate) fn mark_workspace_read(&mut self, ws_idx: usize, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.agent_completion_notification.clear();
            self.save_session(cx);
            cx.notify();
        }
    }

    /// US-008: toggle the sidebar folder row for `ws_idx`.
    ///
    /// Persisted since issue #349 (`WorkspaceSession::sidebar_collapsed`): a
    /// fold made on purpose has to survive a restart, or closing ten folders
    /// and reopening two is a chore redone every launch.
    pub(crate) fn toggle_workspace_expanded(&mut self, ws_idx: usize, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.sidebar_expanded = !ws.sidebar_expanded;
            self.save_session(cx);
            cx.notify();
        }
    }

    /// The single "open a tab holding a fresh surface" path: build the pane,
    /// hand it to the workspace, focus it, and - when the caller supplied one -
    /// write the launch command once the tab is actually in place. Returns
    /// `false` (after a toast) when the workspace is at
    /// [`crate::workspace::MAX_TABS_PER_WORKSPACE`].
    ///
    /// EP-005: `profile` and `command` are passed in rather than derived from
    /// an agent, so the preset palette opens a shell, an agent, an agent
    /// variant or a custom command through one implementation.
    pub(crate) fn open_tab_with_surface(
        &mut self,
        ws_idx: usize,
        title: String,
        profile: TerminalSurfaceProfile,
        command: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return false;
        };
        let ws_id = ws.id;
        // The active tab is the one this surface lands in when it is empty (the
        // palette's own tab, and any tab whose last pane closed), so its
        // worktree binding decides the cwd (issue #347). A surface that opens a
        // NEW tab instead starts unbound, at the workspace root. Reading
        // `ws.cwd` directly here was the one pane-creation path that bypassed
        // `Tab::confine_cwd`, which meant an agent picked from the palette in a
        // bound tab spawned in the wrong checkout.
        let fills_active_tab =
            ws.active_tab().root.is_none() && ws.active_tab().saved_layout.is_none();
        let cwd = fills_active_tab
            .then(|| ws.active_tab().worktree.clone())
            .flatten()
            .or_else(|| (!ws.cwd.is_empty()).then(|| std::path::PathBuf::from(&ws.cwd)));
        // The same gate every other pane-creation path holds
        // (`split_pane`, `surface.split`, `workspace.up`): a checkout being
        // torn down is not a place to start a shell.
        if let Some(cwd) = cwd.as_deref()
            && self.pending_worktree_teardown_conflicts(cwd)
        {
            self.show_toast("Worktree is still being retired", cx);
            return false;
        }
        // A preset label reaches the sidebar verbatim, and a custom command's
        // name is user input: strip CLI decoration (spinners, zero-width
        // glyphs) the way every other title path does.
        let title = crate::sidebar_title::clean_sidebar_title(&title).unwrap_or_default();
        let terminal =
            cx.new(|cx| TerminalView::with_cwd_and_profile(ws_id, cwd, None, profile, cx));
        // Issue #422: do NOT subscribe here - `create_pane` already wires
        // `handle_terminal_event`. The duplicate subscription fired every
        // terminal event twice (the US-028 class); it was harmless while the
        // view fired its own OSC 9/777 notification, and fires every program
        // notification twice now that the app handler delivers it.
        let pane = self.create_pane(terminal.clone(), ws_id, cx);
        let root = LayoutTree::Leaf(pane);
        // EP-005: the palette is the content of an empty tab, so the preset
        // fills *that* tab rather than opening a second one behind it. Any
        // other paneless tab (last pane closed) is filled the same way.
        let opened = self.workspaces.get_mut(ws_idx).is_some_and(|ws| {
            let active = ws.active_tab_mut();
            if active.root.is_none() && active.saved_layout.is_none() {
                active.title = title;
                active.title_is_automatic = true;
                active.root = Some(root);
                true
            } else {
                ws.open_tab(Tab::new(title, Some(root)).with_automatic_title(true))
            }
        });
        if !opened {
            // Tab cap reached: `open_tab` already logged and mutated nothing,
            // so the freshly built pane is simply dropped - and with it the
            // terminal, which is why the launch command is only written below.
            self.show_toast("Tab limit reached for this workspace", cx);
            return false;
        }
        if let Some(command) = command {
            // Safe before the PTY is live: `send_command` buffers into the
            // display-only terminal's pending input and `TerminalState::promote`
            // flushes it when the real PTY arrives (US-012), the same contract
            // the sidebar relies on.
            terminal.read(cx).send_command(&command);
            // Carry the agent identity from frame zero when the command names
            // one - the sidebar logo no longer waits for the process scan.
            terminal.update(cx, |view, _cx| view.declare_agent_from_command(&command));
        }
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            // A tab created from a collapsed folder row must be visible.
            ws.sidebar_expanded = true;
        }
        let tab_idx = self.workspaces[ws_idx].active_tab_idx();
        self.focus_workspace_tab(ws_idx, tab_idx, window, cx);
        self.save_session(cx);
        cx.notify();
        true
    }

    /// Open a NEW workspace tab of `ws_idx` holding an agent-profile terminal
    /// spawned at `cwd` - the one path that places a surface at an arbitrary
    /// directory rather than the active tab's checkout. Lifted from the
    /// session-drop handler's center band (`event_handlers.rs`) for issue
    /// #334, which needs the same placement for "Continue in ▸".
    ///
    /// In order: the worktree-teardown gate every spawn path holds; the
    /// terminal (with `command` written through `send_command` and
    /// `declared` stamped on it); the pane in a new tab, or `None` after the
    /// tab-cap toast; the issue #347 binding when `cwd` sits in a bound or
    /// managed worktree ([`worktree_binding_for_cwd`]); focus, session save,
    /// notify. Returns the terminal so a caller can schedule a prefill.
    pub(crate) fn open_agent_tab_at_cwd(
        &mut self,
        ws_idx: usize,
        cwd: std::path::PathBuf,
        command: Option<String>,
        declared: Option<crate::agent_launcher::TerminalAgent>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Entity<TerminalView>> {
        let ws_id = self.workspaces.get(ws_idx)?.id;
        if self.pending_worktree_teardown_conflicts(&cwd) {
            self.show_toast("Worktree is still being retired", cx);
            return None;
        }
        let terminal = cx.new(|cx| {
            TerminalView::with_cwd_and_profile(
                ws_id,
                Some(cwd.clone()),
                None,
                TerminalSurfaceProfile::Agent,
                cx,
            )
        });
        if let Some(command) = command.as_deref() {
            // Safe before the PTY is live: `send_command` buffers into the
            // pending input and `TerminalState::promote` flushes it.
            terminal.read(cx).send_command(command);
        }
        if let Some(agent) = declared {
            terminal.update(cx, |view, _cx| view.declare_agent(agent));
        }
        // `create_pane` wires the app-level CWD/port subscription and the
        // pane-event subscription.
        let pane = self.create_pane(terminal.clone(), ws_id, cx);
        if !self.open_pane_in_new_workspace_tab(ws_idx, pane.clone(), cx) {
            return None;
        }
        let binding = {
            let ws = &self.workspaces[ws_idx];
            let managed: Vec<std::path::PathBuf> = ws
                .managed_worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
                .collect();
            worktree_binding_for_cwd(&ws.bound_tab_worktrees(), &managed, &cwd)
        };
        if let Some(worktree) = binding {
            let tab_idx = self.workspaces[ws_idx].active_tab_idx();
            self.set_tab_worktree(ws_idx, tab_idx, Some(worktree), cx);
        }
        self.pending_pane_focus = Some(pane);
        self.save_session(cx);
        cx.notify();
        Some(terminal)
    }

    pub(crate) fn handle_new_tab(
        &mut self,
        _: &NewTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // EP-005: `New tab` is the palette's primary entry point - the user
        // picks *what* to launch before a surface exists, instead of getting a
        // bare shell and reaching for an agent afterwards.
        self.open_pane_palette(self.active_idx, window, cx);
    }

    /// US-020: cycle to the next tab of the active workspace, wrapping around.
    /// A single-tab workspace is a no-op, which keeps the shortcut harmless
    /// when the hierarchy is not in use.
    pub(crate) fn handle_next_tab(
        &mut self,
        _: &NextTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_active_workspace_tab(1, window, cx);
    }

    /// US-020: cycle to the previous tab of the active workspace, wrapping.
    pub(crate) fn handle_previous_tab(
        &mut self,
        _: &PreviousTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_active_workspace_tab(-1, window, cx);
    }

    /// Shared body of the two cycling shortcuts. Routes through
    /// `focus_workspace_tab`, the single path every "this tab is now visible"
    /// caller uses, so focus, zoom restore and persistence behave exactly as
    /// they do when the tab is picked in the sidebar.
    fn cycle_active_workspace_tab(
        &mut self,
        step: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.active_workspace() else {
            return;
        };
        let count = ws.tab_count();
        if count < 2 {
            return;
        }
        let current = ws.active_tab_idx() as isize;
        let next = (current + step).rem_euclid(count as isize) as usize;
        // `focus_workspace_tab` already persists the new active tab.
        self.focus_workspace_tab(self.active_idx, next, window, cx);
        cx.notify();
    }

    /// US-009: make `tab_idx` of `ws_idx` the visible tab, activating the
    /// workspace first when it is not the active one, then focusing the tab's
    /// first pane.
    pub(crate) fn select_workspace_tab(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspaces
            .get(ws_idx)
            .is_none_or(|ws| tab_idx >= ws.tab_count())
        {
            return;
        }
        self.commit_rename(cx);
        self.dismiss_transient_surfaces();
        // Navigating away abandons an armed close: the red X left behind
        // belongs to a tab the user is no longer looking at.
        // `dismiss_transient_surfaces` would be the natural home for this, but
        // it takes no `cx` and all sixteen of its callers would have to grow
        // one.
        self.dismiss_inline_close_arm(cx);
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.agent_completion_notification.clear();
        }
        self.focus_workspace_tab(ws_idx, tab_idx, window, cx);
        cx.notify();
    }

    /// Shared tail of every "this tab is now the one you look at" path: set the
    /// active tab *before* activating the workspace, so the workspace
    /// activation (focus, files tree, sessions sidebar, diff reconcile) already
    /// sees the right tab.
    pub(crate) fn focus_workspace_tab(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Issue #347: a tab bound to a worktree reviews that checkout, so
        // switching tab inside the active workspace can change what Diff mode
        // is looking at - which the workspace-switch reconcile below never
        // covers, because the workspace did not change. The dock needs no help:
        // it already parks and restores per tab id.
        let checkout_before = self.active_checkout();
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.set_active_tab(tab_idx);
        }
        if ws_idx == self.active_idx {
            // Issue #108: an empty tab has no pane to focus.
            if !self.workspaces[ws_idx].focus_first(window, cx) {
                window.focus(&self.empty_workspace_focus, cx);
            }
            self.save_session(cx);
            if self.active_checkout() != checkout_before {
                self.reconcile_diff_after_workspace_change(cx);
            }
        } else {
            self.select_workspace(ws_idx, window, cx);
        }
    }

    /// US-010: close one tab of one workspace. Closing the last tab leaves an
    /// empty tab behind (FR-01) and never closes the workspace. The removed
    /// `Tab` drops here, which drops its pane entities and their terminals -
    /// the same teardown path a pane close uses, so no PTY is orphaned.
    pub(crate) fn close_workspace_tab(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Issue #83: snapshot the tab BEFORE `close_tab` drops it - the drop
        // takes its panes and their terminals with it. One capture here covers
        // every route into this function: Cmd+W, the sidebar rail X, the tab
        // context menu's Close, and the pane picker's escape.
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        let record = ws
            .tabs()
            .get(tab_idx)
            .and_then(|tab| capture_closed_tab_record(tab, tab_idx, ws.id, cx));
        let closed_tab_id = ws.tabs().get(tab_idx).map(|tab| tab.id);

        // Issue #397: a dirty `DiffDockTab::File` holds edits that live only
        // in `CodeView`'s buffer - `session.json` never journals them - and
        // the dock slot is not on the undo record either (a restored tab gets
        // a fresh id), so once the tab is gone nothing can bring the buffer
        // back: the next `sync_diff_dock_session` prunes the slot as an
        // orphan. Refuse the close BEFORE `close_tab` mutates the workspace,
        // leaving the tab and its dock on screen so the user can still save.
        // `close_arms_first` already refuses the same drop from the dock's
        // own close button, and `quit_after_session_save` refuses to quit
        // past one (#396); this is the same guard on the third route.
        if let Some(tab_id) = closed_tab_id
            && self.dock_file_dirty_for_tab(tab_id, cx)
        {
            self.show_toast(unsaved_dock_file_close_tab_toast_message().to_string(), cx);
            return;
        }

        let Some(ws) = self.workspaces.get_mut(ws_idx) else {
            return;
        };
        if ws.close_tab(tab_idx).is_none() {
            return;
        }
        // The dock is parked per tab (#184 Phase 4) and its slot is not on the
        // undo record: a restored tab gets a fresh id. Tear it down here, not
        // at render - a background tab closes without moving the visible
        // session, so the dock's own reconcile never runs and the slot (and
        // the terminals in it) would outlive the session it belonged to.
        // The dirty-file guard above already returned, so this only ever
        // drops a clean dock.
        if let Some(tab_id) = closed_tab_id {
            self.drop_diff_dock_for_tab(tab_id, cx);
        }
        // The closed tab may have been the last reader of a worktree's git
        // state (issue #347). The checkout itself is left alone: tearing it
        // down is a separate, destructive decision.
        self.prune_worktree_states();
        if let Some(record) = record {
            self.push_closed_record(ClosedRecord::Tab(record), cx);
        }
        // Issue #79/#108: this is a focus-tracking clear, not a plain state
        // clear - the renamed sidebar row is the only element that tracks
        // `sidebar_rename_focus`, so dropping the state here without moving
        // focus leaves the window with nothing focused. The re-focus below
        // only runs for the ACTIVE workspace, and closing a BACKGROUND
        // workspace's tab while renaming it is reachable straight from that
        // tab row's right-click menu. `cancel_inline_rename` restores focus.
        if self.renaming_tab.is_some_and(|(w, _)| w == ws_idx) {
            self.cancel_inline_rename(window, cx);
        }
        self.dismiss_transient_surfaces();
        if ws_idx == self.active_idx {
            // Issue #108: closing the last tab leaves the substitute empty tab
            // behind, so there is no pane left to take focus. Park it on the
            // placeholder to keep the global bindings on the dispatch path.
            if !self.workspaces[ws_idx].focus_first(window, cx) {
                window.focus(&self.empty_workspace_focus, cx);
            }
        }
        self.save_session(cx);
        cx.notify();
        // The closed tab's panes may have carried a Composer target, queued
        // prompts, or group memberships - refresh the same way a workspace
        // close does so nothing points at a dropped terminal.
        self.refresh_composer_slot(cx);
        self.sync_broadcast_stripes(cx);
        self.flush_pending_prefill(cx);
        self.sync_pending_chips(cx);
    }

    pub(crate) fn handle_close_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.active_workspace().map(|ws| ws.active_tab_idx()) else {
            return;
        };
        // Issue #83: ask first when this tab holds a live agent.
        self.request_close_workspace_tab(
            self.active_idx,
            tab_idx,
            crate::app::close_guard::ConfirmStyle::Modal,
            window,
            cx,
        );
    }

    /// Start the inline rename of a sidebar tab row. Mirrors
    /// `begin_workspace_rename`: any live rename commits first, and the input
    /// seeds with the label the row currently shows - which since the label
    /// became derived may be a pane's title rather than `Tab::title`. That is
    /// the point: renaming starts from what the user is looking at. Committing
    /// it unedited still writes nothing, because `commit_rename` compares the
    /// proposal against that same derived label.
    ///
    /// Issue #79: takes a `Window` so it can claim `sidebar_rename_focus`.
    /// Drawing the editor is not enough - until the renamed row is on the
    /// dispatch path to the focused node, GPUI hands its `on_key_down`
    /// nothing. Focus is claimed last, after the state that decides which row
    /// tracks the handle, and after the single-click that precedes a
    /// double-click has already focused a terminal pane.
    pub(crate) fn begin_tab_rename(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_rename(cx);
        let Some(title) = self
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs().get(tab_idx))
            .map(|tab| crate::app::sidebar::tab_row_title(tab, tab_idx, cx))
        else {
            return;
        };
        self.rename_text = title;
        // Seeded, so the editor opens with the whole displayed label selected.
        // Set after `commit_rename`, which clears the flag.
        self.rename_seeded = true;
        self.renaming_tab = Some((ws_idx, tab_idx));
        self.sidebar_rename_focus.focus(window, cx);
        cx.notify();
    }

    /// US-011: reorder a dragged tab inside its own workspace. `target_idx` is
    /// the index of the row it was dropped on; the insertion side matches the
    /// workspace-row drop-edge convention (drop below when dragging down).
    pub(crate) fn reorder_workspace_tab(
        &mut self,
        drag: &TabDrag,
        target_ws_idx: usize,
        target_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspaces.get_mut(target_ws_idx) else {
            return;
        };
        if ws.id != drag.workspace_id {
            return;
        }
        // Re-resolve by id: the sidebar may have re-rendered since the drag
        // started, so the captured index can be stale.
        let Some(from) = ws.tabs().iter().position(|tab| tab.id == drag.tab_id) else {
            return;
        };
        if from == target_idx {
            return;
        }
        ws.reorder_tab(from, target_idx);
        self.save_session(cx);
        cx.notify();
    }

    /// US-011: reattach a dragged tab to another workspace, keeping its pane
    /// tree and its live terminals. Refused - with the tab left untouched, so
    /// nothing is killed - when the destination is already at
    /// [`crate::workspace::MAX_TABS_PER_WORKSPACE`].
    ///
    /// `insert_idx` is the gap the sidebar's insertion line pointed at, so the
    /// tab lands where the line showed it rather than at the end of the list.
    pub(crate) fn move_tab_to_workspace(
        &mut self,
        drag: &TabDrag,
        dest_ws_idx: usize,
        insert_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source_ws_idx) = self
            .workspaces
            .iter()
            .position(|ws| ws.id == drag.workspace_id)
        else {
            return;
        };
        if source_ws_idx == dest_ws_idx {
            return;
        }
        let Some(dest) = self.workspaces.get(dest_ws_idx) else {
            return;
        };
        if !dest.can_open_tab() {
            // Cap reached: bail *before* detaching, so the tab stays where it
            // is with every terminal alive.
            self.show_toast("Tab limit reached for this workspace", cx);
            return;
        }
        let dest_id = dest.id;
        let Some(tab_idx) = self
            .workspaces
            .get(source_ws_idx)
            .and_then(|ws| ws.tabs().iter().position(|tab| tab.id == drag.tab_id))
        else {
            return;
        };
        let Some(tab) = self.workspaces[source_ws_idx].close_tab(tab_idx) else {
            return;
        };
        // The panes move with the tab, so their workspace identity must move
        // too: port scans, agent sessions and IPC surface lookups are all keyed
        // by the pane's workspace id. The terminal's own cwd is untouched - a
        // tab dropped on a workspace with a different cwd keeps running where
        // it was started.
        let moved_surfaces = tab.surface_ids(cx);
        for pane in tab.collect_panes() {
            pane.update(cx, |pane, cx| {
                pane.workspace_id = dest_id;
                cx.notify();
            });
        }
        // The registry rows of the moved panes' agents go with them, so the
        // sidebar badge, the attention queue and `whoami` follow the pane and
        // closing the source workspace no longer discards a live session.
        migrate_agent_sessions(
            &mut self.workspaces,
            source_ws_idx,
            dest_ws_idx,
            &moved_surfaces,
        );
        if !self.workspaces[dest_ws_idx].open_tab(tab) {
            // Unreachable: the cap was checked above and nothing else can have
            // opened a tab in between. Kept as a fail-safe rather than a panic.
            log::warn!("tab move: destination refused the tab after the cap check");
            return;
        }
        // `open_tab` appends; slide the newcomer to the gap the line marked.
        // `reorder_tab` re-resolves the active tab by id, so the moved tab
        // stays the visible one.
        let last = self.workspaces[dest_ws_idx].tab_count().saturating_sub(1);
        self.workspaces[dest_ws_idx].reorder_tab(last, insert_idx.min(last));
        self.renaming_tab = None;
        self.rename_text.clear();
        self.rename_seeded = false;
        self.workspaces[dest_ws_idx].sidebar_expanded = true;
        let dest_tab_idx = self.workspaces[dest_ws_idx].active_tab_idx();
        self.focus_workspace_tab(dest_ws_idx, dest_tab_idx, window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Sidebar drop target for a dragged pane: detach it from wherever it sits
    /// and reopen it as a brand-new tab of `dest_ws_idx` (the folder row it was
    /// dropped on).
    ///
    /// The pane entity survives the detach - this handler holds a strong
    /// handle throughout - so its terminal keeps running across the move; only
    /// the tree it hangs from changes. The source is re-resolved by entity id
    /// because the layout re-renders during the drag, which invalidates any
    /// index captured when the gesture started. `insert_idx` is the gap the
    /// sidebar's insertion line pointed at.
    pub(crate) fn move_pane_to_new_tab(
        &mut self,
        pane_id: u64,
        dest_ws_idx: usize,
        mut insert_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((src_ws_idx, src_tab_idx, pane)) =
            self.workspaces.iter().enumerate().find_map(|(ws_idx, ws)| {
                ws.tabs().iter().enumerate().find_map(|(tab_idx, tab)| {
                    tab.collect_panes()
                        .into_iter()
                        .find(|p| p.entity_id().as_u64() == pane_id)
                        .map(|p| (ws_idx, tab_idx, p))
                })
            })
        else {
            return;
        };

        // Already alone in a tab of that same workspace: the move would close
        // one tab only to reopen an identical one, losing the tab's title on
        // the way.
        if src_ws_idx == dest_ws_idx
            && self.workspaces[src_ws_idx]
                .tabs()
                .get(src_tab_idx)
                .is_some_and(|tab| tab.pane_count() <= 1)
        {
            return;
        }

        if !self
            .workspaces
            .get(dest_ws_idx)
            .is_some_and(|ws| ws.can_open_tab())
        {
            // Cap reached: bail *before* detaching, so the pane stays where it
            // is with its terminal alive.
            self.show_toast("Tab limit reached for this workspace", cx);
            return;
        }

        // Zoom parks the real tree in `saved_layout` and leaves the zoomed pane
        // alone in `root`. Detaching from that root would strand the siblings,
        // so leave zoom first and move from the restored tree.
        if self.workspaces[src_ws_idx]
            .tabs()
            .get(src_tab_idx)
            .is_some_and(|tab| tab.is_zoomed())
            && let Some(tab) = self.workspaces[src_ws_idx].tab_mut(src_tab_idx)
        {
            tab.exit_zoom(cx);
        }

        let Some(tree) = self.workspaces[src_ws_idx]
            .tab_mut(src_tab_idx)
            .and_then(|tab| tab.root.take())
        else {
            return;
        };
        let (pruned, removed) = tree.remove_pane(&pane);
        if !removed {
            // Stale drag: `remove_pane` hands the tree back intact, so the tab
            // is restored exactly as it was rather than left rootless.
            if let Some(tab) = self.workspaces[src_ws_idx].tab_mut(src_tab_idx) {
                tab.root = pruned;
            }
            return;
        }
        // The id of the tab the pane vacates, when it was the whole tab: the
        // tab's dock (#184 Phase 4: parked per tab) follows the pane into the
        // tab it lands in, below.
        let mut vacated_tab_id = None;
        match pruned {
            Some(rest) => {
                if let Some(tab) = self.workspaces[src_ws_idx].tab_mut(src_tab_idx) {
                    tab.root = Some(rest);
                }
            }
            // The pane *was* the whole tab, so the now-empty tab leaves with
            // it. `close_tab` keeps the workspace's last-tab placeholder
            // (FR-01), which `open_tab` fills in place when the destination is
            // this same workspace.
            None => {
                vacated_tab_id = self.workspaces[src_ws_idx]
                    .tabs()
                    .get(src_tab_idx)
                    .map(|tab| tab.id);
                self.workspaces[src_ws_idx].close_tab(src_tab_idx);
                // Every tab after the closed one slid up by one, and so did the
                // gap the line pointed at.
                if src_ws_idx == dest_ws_idx && src_tab_idx < insert_idx {
                    insert_idx -= 1;
                }
            }
        }

        // Port scans, agent sessions and IPC surface lookups are all keyed by
        // the pane's workspace id, so a cross-workspace move must carry it.
        let dest_id = self.workspaces[dest_ws_idx].id;
        pane.update(cx, |pane, cx| {
            pane.workspace_id = dest_id;
            cx.notify();
        });
        // Same contract as `move_tab_to_workspace`: the agent's registry row
        // moves with its pane (no-op when the move stays in one workspace).
        let moved_surfaces: std::collections::HashSet<u64> = pane
            .read(cx)
            .terminals()
            .map(|terminal| terminal.entity_id().as_u64())
            .collect();
        migrate_agent_sessions(
            &mut self.workspaces,
            src_ws_idx,
            dest_ws_idx,
            &moved_surfaces,
        );

        if !self.open_pane_in_new_workspace_tab(dest_ws_idx, pane.clone(), cx) {
            // Unreachable: the cap was checked above and the detach can only
            // have freed a slot. Re-attach rather than orphan the pane - its
            // terminal would keep running with no way back to it.
            log::warn!("pane move: destination refused the tab after the cap check");
            let reattached = self.workspaces[src_ws_idx]
                .tab_mut(src_tab_idx)
                .and_then(|tab| tab.root.as_mut())
                .is_some_and(|root| {
                    root.first_leaf().is_some_and(|anchor| {
                        root.split_at_pane(&anchor, crate::layout::SplitDirection::Vertical, pane)
                    })
                });
            if !reattached {
                log::error!("pane move: dropped pane could not be re-attached");
            }
            // The vacated tab is gone for good and there is no tab to hand its
            // dock to: tear it down explicitly rather than leave it for the
            // render-time prune.
            if let Some(tab_id) = vacated_tab_id {
                self.drop_diff_dock_for_tab(tab_id, cx);
            }
            cx.notify();
            return;
        }

        // The pane *was* the tab's content, so the dock that followed that
        // tab follows the pane into its new one - re-keyed before the next
        // paint, or the reconcile would prune the slot (terminals and all)
        // the moment the vacated id stopped resolving.
        if let Some(from) = vacated_tab_id {
            let to = self.workspaces[dest_ws_idx].active_tab().id;
            self.rehome_diff_dock_for_tab(from, to);
        }

        // `open_tab` appends; slide the newcomer to the gap the line marked.
        let last = self.workspaces[dest_ws_idx].tab_count().saturating_sub(1);
        self.workspaces[dest_ws_idx].reorder_tab(last, insert_idx.min(last));
        self.workspaces[dest_ws_idx].sidebar_expanded = true;
        let dest_tab_idx = self.workspaces[dest_ws_idx].active_tab_idx();
        self.focus_workspace_tab(dest_ws_idx, dest_tab_idx, window, cx);
        self.save_session(cx);
        cx.notify();
    }
}

/// Shown when closing a tab would otherwise silently drop a dirty dock file
/// tab (issue #397, mirroring #396's quit-time
/// `unsaved_dock_file_quit_toast_message`): `session.json` never journals a
/// `CodeView`'s in-memory edits.
fn unsaved_dock_file_close_tab_toast_message() -> &'static str {
    "Save your changes in this tab's open files before closing it."
}

/// Issue #347 binding for a tab opened at an arbitrary `cwd`: the worktree it
/// belongs to, when `cwd` is (or lies under) a worktree some tab is already
/// bound to, or a worktree the workspace manages. `None` leaves the tab
/// unbound, exactly as a dropped session at the workspace root is.
pub(crate) fn worktree_binding_for_cwd(
    bound: &[String],
    managed: &[std::path::PathBuf],
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    bound
        .iter()
        .map(std::path::PathBuf::from)
        .chain(managed.iter().cloned())
        .find(|root| !root.as_os_str().is_empty() && cwd.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::{migrate_agent_sessions, worktree_binding_for_cwd};
    use std::path::{Path, PathBuf};

    #[test]
    fn a_pane_move_carries_its_agent_session_rows_to_the_destination() {
        use crate::agent_launcher::TerminalAgent;
        use crate::ai_types::{AgentSession, AgentState};
        use crate::workspace::Workspace;
        let row = |sid: Option<u64>| {
            let mut session = AgentSession::new(TerminalAgent::Codex, AgentState::Thinking);
            session.surface_id = sid;
            session
        };
        let mut source = Workspace::empty_with_cwd_and_id(1, "source", PathBuf::new());
        let mut destination = Workspace::empty_with_cwd_and_id(2, "destination", PathBuf::new());
        source.agent_sessions.insert(42, row(Some(7)));
        // A different surface and an unresolved row stay where they are.
        source.agent_sessions.insert(43, row(Some(8)));
        source.agent_sessions.insert(44, row(None));
        destination.agent_sessions.insert(45, row(Some(9)));
        let mut workspaces = vec![source, destination];
        let moved_surfaces: std::collections::HashSet<u64> = [7].into_iter().collect();

        assert_eq!(
            migrate_agent_sessions(&mut workspaces, 0, 1, &moved_surfaces),
            1
        );
        assert!(!workspaces[0].agent_sessions.contains_key(&42));
        assert!(workspaces[0].agent_sessions.contains_key(&43));
        assert!(workspaces[0].agent_sessions.contains_key(&44));
        assert_eq!(workspaces[1].agent_sessions[&42].surface_id, Some(7));
        assert!(workspaces[1].agent_sessions.contains_key(&45));

        // Same workspace, an empty set, or an out-of-range index is a no-op.
        assert_eq!(
            migrate_agent_sessions(&mut workspaces, 1, 1, &moved_surfaces),
            0
        );
        let none = std::collections::HashSet::new();
        assert_eq!(migrate_agent_sessions(&mut workspaces, 1, 0, &none), 0);
        assert_eq!(
            migrate_agent_sessions(&mut workspaces, 1, 5, &moved_surfaces),
            0
        );
        assert!(workspaces[1].agent_sessions.contains_key(&42));

        // A key collision keeps the destination's row rather than overwriting it.
        workspaces[0].agent_sessions.insert(42, row(Some(7)));
        assert_eq!(
            migrate_agent_sessions(&mut workspaces, 0, 1, &moved_surfaces),
            0
        );
        assert!(!workspaces[0].agent_sessions.contains_key(&42));
        assert!(workspaces[1].agent_sessions.contains_key(&42));
    }

    #[test]
    fn worktree_binding_binds_a_bound_or_managed_checkout_and_nothing_else() {
        let bound = vec!["/r.worktrees/a".to_string()];
        let managed = vec![PathBuf::from("/r.worktrees/b")];
        // Equal to a bound worktree: binds to it.
        assert_eq!(
            worktree_binding_for_cwd(&bound, &managed, Path::new("/r.worktrees/a")),
            Some(PathBuf::from("/r.worktrees/a"))
        );
        // Under a managed worktree: binds to the root, not the subdirectory.
        assert_eq!(
            worktree_binding_for_cwd(&bound, &managed, Path::new("/r.worktrees/b/src")),
            Some(PathBuf::from("/r.worktrees/b"))
        );
        // Unrelated: unbound. A sibling whose name merely shares a prefix is
        // unrelated too.
        assert_eq!(
            worktree_binding_for_cwd(&bound, &managed, Path::new("/r")),
            None
        );
        assert_eq!(
            worktree_binding_for_cwd(&bound, &managed, Path::new("/r.worktrees/ab")),
            None
        );
        assert_eq!(
            worktree_binding_for_cwd(&[String::new()], &[], Path::new("/anything")),
            None,
            "an empty root must never bind"
        );
    }

    #[test]
    fn open_agent_tab_at_cwd_holds_the_placement_contract() {
        // Issue #334 / #347: the lifted helper gates on teardown before
        // spawning, opens a NEW workspace tab (never the active one), returns
        // `None` at the tab cap without focusing anything, and binds the tab
        // through `set_tab_worktree` so the dock and git probe follow.
        let src = include_str!("tab.rs");
        let body = src
            .split("pub(crate) fn open_agent_tab_at_cwd(")
            .nth(1)
            .and_then(|rest| rest.split("pub(crate) fn handle_new_tab(").next())
            .expect("open_agent_tab_at_cwd body");
        let gate = body
            .find("pending_worktree_teardown_conflicts(&cwd)")
            .expect("teardown gate");
        let spawn = body
            .find("TerminalView::with_cwd_and_profile(")
            .expect("terminal spawn");
        let opened = body
            .find("self.open_pane_in_new_workspace_tab(ws_idx, pane.clone(), cx)")
            .expect("new workspace tab");
        let bind = body
            .find("self.set_tab_worktree(ws_idx, tab_idx, Some(worktree), cx)")
            .expect("worktree binding");
        let focus = body
            .find("self.pending_pane_focus = Some(pane)")
            .expect("pane focus");
        assert!(
            gate < spawn && spawn < opened && opened < bind && bind < focus,
            "{body}"
        );
        assert!(
            body[opened..bind].contains("return None;"),
            "the tab cap must return None before any focus or binding: {body}"
        );
        assert!(
            body.contains("TerminalSurfaceProfile::Agent"),
            "the surface is an agent surface"
        );
        assert!(
            !body.contains("active_tab_mut()") && !body.contains("active.root = Some("),
            "the helper must never fill the active tab: {body}"
        );
    }

    /// Issue #422: `open_tab_with_surface` used to subscribe
    /// `handle_terminal_event` by hand and then call `create_pane`, which
    /// subscribes it again (the US-028 class). The duplicate was harmless while
    /// the view fired its own OSC 9 / 777 desktop notification; once the app
    /// handler delivers it, every program notification fired twice. Pinned on
    /// the source body because a `PaneFlowApp` cannot be constructed in a test.
    #[test]
    fn open_tab_with_surface_subscribes_terminal_events_exactly_once() {
        let src = include_str!("tab.rs");
        let body = src
            .split("pub(crate) fn open_tab_with_surface(")
            .nth(1)
            .and_then(|rest| rest.split("pub(crate) fn open_agent_tab_at_cwd(").next())
            .expect("open_tab_with_surface body");
        assert!(
            body.contains("self.create_pane(terminal.clone(), ws_id, cx)"),
            "the pane is built through `create_pane`, which wires the app-level \
             terminal subscription: {body}"
        );
        assert!(
            !body.contains("cx.subscribe(&terminal, Self::handle_terminal_event)"),
            "no manual `handle_terminal_event` subscription beside `create_pane`: {body}"
        );
    }

    /// Issue #397: `close_workspace_tab` used to call `drop_diff_dock_for_tab`
    /// unconditionally, which drops the tab's `CodeView` entities and bypasses
    /// `close_arms_first` - the only US-017 gate that refuses to drop a dirty
    /// file tab. A `PaneFlowApp` cannot be constructed in a test (its
    /// constructor binds a Unix socket and spawns PTYs, the same reason
    /// `quit_after_session_save_refuses_to_discard_a_dirty_dock_file` in
    /// `session.rs` asserts on the raw function body for #396), so this pins
    /// the wiring the same way: the dirty check must run before the drop, and
    /// the drop must sit behind it rather than run unconditionally alongside
    /// it. `a_dirty_code_view_is_reported_by_the_dock_file_dirty_check` in
    /// `diff_dock/code/view.rs` exercises the underlying predicate
    /// (`any_file_tab_dirty`, which `dock_file_dirty_for_tab` reuses) against
    /// a real, edited `CodeView`.
    #[test]
    fn close_workspace_tab_refuses_to_drop_a_dirty_dock_before_confirming() {
        let src = include_str!("tab.rs");
        let body = src
            .split("pub(crate) fn close_workspace_tab(")
            .nth(1)
            .and_then(|rest| rest.split("pub(crate) fn handle_close_tab(").next())
            .expect("close_workspace_tab body");

        let dirty_check_at = body
            .find("self.dock_file_dirty_for_tab(tab_id, cx)")
            .expect("close_workspace_tab must consult the dock dirty-file check");
        let close_at = body
            .find("ws.close_tab(tab_idx)")
            .expect("close_workspace_tab must still close a clean tab");
        let drop_at = body
            .find("self.drop_diff_dock_for_tab(tab_id, cx);")
            .expect("close_workspace_tab must still tear a clean dock down");

        // PR #416 review: checking AFTER `close_tab` is too late - the tab is
        // already gone, the toast has nothing left to save through, and the
        // next dock reconcile prunes the orphaned slot anyway. The check must
        // run before the workspace is mutated at all.
        assert!(
            dirty_check_at < close_at,
            "the dirty check must run before close_tab removes the tab: {body}"
        );
        assert!(
            close_at < drop_at,
            "the dock drop belongs after the tab close, on the clean path only: {body}"
        );

        // The dirty branch must toast and bail out without reaching the
        // close, so the tab (and the dock holding the edits) stays on screen.
        let dirty_branch = &body[dirty_check_at..close_at];
        assert!(
            dirty_branch.contains("self.show_toast("),
            "a dirty dock file must be reported instead of silently kept or dropped: {dirty_branch}"
        );
        let after_toast = dirty_branch
            .split("self.show_toast(")
            .nth(1)
            .expect("toast call in the dirty branch");
        assert!(
            after_toast.contains("return;"),
            "the dirty branch must return before close_tab runs: {dirty_branch}"
        );
    }
    #[test]
    fn workspace_notification_actions_persist_without_dismissing_session_badges() {
        // App bootstrap opens real windows and PTYs; inspect these UI command
        // bodies to pin persistence and separation from session badge state.
        let src = include_str!("tab.rs");
        let toggle = src
            .split("pub(crate) fn toggle_workspace_muted(")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn workspace_is_muted(")
            .next()
            .unwrap();
        assert!(toggle.contains("ws.muted = !ws.muted;"));
        assert!(toggle.contains("self.save_session(cx);"));
        assert!(toggle.contains("cx.notify();"));
        assert!(!toggle.contains("agent_completion_notification"));
        let mark = src
            .split("pub(crate) fn mark_workspace_read(")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn toggle_workspace_expanded(")
            .next()
            .unwrap();
        assert!(mark.contains("ws.agent_completion_notification.clear();"));
        assert!(mark.contains("self.save_session(cx);"));
        assert!(mark.contains("cx.notify();"));
        assert!(!mark.contains("agent_sessions"));
    }

    #[test]
    fn workspace_mute_gates_every_notification_source_after_state_updates() {
        // These routes require the live GPUI app and PTYs. This wiring probe
        // protects the shared mute gate at each actual delivery call; lower
        // level notification and completion behavior have executable tests.
        let compact = |src: &str| src.split_whitespace().collect::<String>();
        let ipc = compact(include_str!("../ipc_handler.rs"));
        assert_eq!(
            ipc.matches("||self.workspace_is_muted(workspace_id)")
                .count(),
            3
        );
        assert!(ipc.contains("letseen=self.session_is_seen(workspace_id,session_key,cx)||self.workspace_is_muted(workspace_id);"));
        assert!(ipc.contains(".record_finished(seen,finished_surface)"));
        let observations = compact(include_str!("../agent_status.rs"));
        assert!(observations.contains(
            "completion_was_seen(visible.as_ref(),Some(surface_id))||self.workspace_is_muted(ws_id)"
        ));
        assert!(observations.contains(".record_finished(seen,Some(surface_id))"));
        let events = compact(include_str!("../event_handlers.rs"));
        assert!(events.contains(".workspace_id_for_surface(surface_id,cx).is_some_and(|ws_id|self.workspace_is_muted(ws_id))"));
        assert!(events.contains("self.hosted_surface_is_seen(surface_id,cx)||muted"));
        assert!(events.contains("surface_id,)||self.workspace_is_muted(ws_id);super::ipc_handler::fire_stalled_notification("));
        assert!(events.contains("session.state=ai_types::AgentState::Stalled;"));
        let dock = compact(include_str!("../diff_dock/tabs.rs"));
        assert!(dock.contains("this.workspace_is_muted(ws.id)&&this.diff_dock_terminals_for_workspace(ws.id).contains(&terminal)"));
        assert!(dock.contains("this.dock_terminal_is_seen(&terminal)||muted"));
    }
}
