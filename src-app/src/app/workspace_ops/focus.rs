//! Focus-movement handlers for `PaneFlowApp`.
//!
//! Part of the US-023 workspace_ops decomposition - behaviour identical to
//! the pre-refactor `main.rs` implementation.

use gpui::{Context, Window};
use paneflow_config::schema::AppMode;

use super::WorkspaceFocusTarget;
use crate::PaneFlowApp;
use crate::layout::{FocusDirection, FocusNav, LayoutTree};
use crate::{FocusDown, FocusLeft, FocusRight, FocusUp, JumpNextWaiting};

impl PaneFlowApp {
    pub(crate) fn nav_root(&self) -> Option<&LayoutTree> {
        match self.mode {
            AppMode::Diff => self.review.layout.as_ref(),
            AppMode::Cli => self
                .active_workspace()
                .and_then(|ws| ws.active_tab().root.as_ref()),
        }
    }

    pub(crate) fn take_nav_root(&mut self) -> Option<LayoutTree> {
        match self.mode {
            AppMode::Diff => self.review.layout.take(),
            AppMode::Cli => self
                .active_workspace_mut()
                .and_then(|ws| ws.active_tab_mut().root.take()),
        }
    }

    pub(crate) fn put_nav_root(&mut self, root: Option<LayoutTree>) {
        match self.mode {
            AppMode::Diff => self.review.layout = root,
            AppMode::Cli => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.active_tab_mut().root = root;
                }
            }
        }
    }

    pub(crate) fn exit_nav_zoom(&mut self, cx: &mut Context<Self>) {
        match self.mode {
            AppMode::Diff => {
                self.review_exit_zoom(cx);
            }
            AppMode::Cli => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.exit_zoom(cx);
                }
            }
        }
    }

    pub(crate) fn handle_focus(
        &mut self,
        dir: FocusDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self.nav_root()
            && !matches!(root.focus_in_direction(dir, window, cx), FocusNav::Moved)
        {
            self.show_toast("No pane in that direction", cx);
        }
        cx.notify();
    }

    pub(crate) fn handle_focus_left(
        &mut self,
        _: &FocusLeft,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_focus(FocusDirection::Left, w, cx);
    }
    pub(crate) fn handle_focus_right(
        &mut self,
        _: &FocusRight,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_focus(FocusDirection::Right, w, cx);
    }
    pub(crate) fn handle_focus_up(&mut self, _: &FocusUp, w: &mut Window, cx: &mut Context<Self>) {
        self.handle_focus(FocusDirection::Up, w, cx);
    }
    pub(crate) fn handle_focus_down(
        &mut self,
        _: &FocusDown,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_focus(FocusDirection::Down, w, cx);
    }

    /// US-019 (orchestration-v2): teleport to the next pane whose agent is
    /// `WaitingForInput`, cross-workspace, in a stable order (workspace
    /// index, then layout traversal, then tab order). Repeated presses cycle
    /// through the waiting set via `jump_cursor`; activating a background
    /// tab is part of the jump (the waiting surface may be hidden). No
    /// waiting agent → silent no-op (an empty queue is the good news).
    /// Sessions without a resolved surface are skipped (US-017 fallback -
    /// never jump to a guessed pane).
    pub(crate) fn handle_jump_next_waiting(
        &mut self,
        _: &JumpNextWaiting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.jump_next_session_where(
            |s| *s == crate::ai_types::AgentState::WaitingForInput,
            window,
            cx,
        );
    }

    /// EP-005 US-015: the teleport body of `handle_jump_next_waiting`,
    /// parametrized by the state predicate (its one caller matches
    /// `WaitingForInput`). Matching panes are visited in a stable order with
    /// cursor cycling (`next_in_cycle`); a cursor that no longer appears in
    /// the order simply restarts the cycle at the first match, which is the
    /// existing stale-cursor behavior.
    pub(crate) fn jump_next_session_where(
        &mut self,
        state_matches: impl Fn(&crate::ai_types::AgentState) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order = matching_session_panes(&self.workspaces, state_matches, cx);
        let ids: Vec<u64> = order.iter().map(|(_, _, _, sid)| *sid).collect();
        let Some(next) = next_in_cycle(&ids, self.jump_cursor) else {
            return;
        };
        let Some((ws_idx, tab_idx, pane, sid)) = order.into_iter().find(|(_, _, _, s)| *s == next)
        else {
            return;
        };
        self.workspaces[ws_idx].set_active_tab(tab_idx);
        self.activate_workspace_at(ws_idx, WorkspaceFocusTarget::Pane { pane }, window, cx);
        self.jump_cursor = Some(sid);
    }

    /// Focus the pane hosting `surface_id`, wherever it lives (issue #339).
    ///
    /// Returns `false` when the surface is gone (a pane closed between render
    /// and activation), which every caller treats as a clean no-op.
    ///
    /// The ORDER is load-bearing and is why this is one function rather than
    /// another copy of the waiting-agent navigation / pane search teleport: focus can
    /// only land on a *rendered* pane, so the owning tab has to become
    /// visible before `activate_workspace_at` runs. Indices are re-resolved
    /// from `surface_id` here rather than captured by the caller, so a
    /// workspace or tab reorder between render and click cannot teleport the
    /// user to the wrong pane. The surface may also sit in the owning tab's
    /// zoom-saved tree (Pane Overview lists it); the `WorkspaceFocusTarget::Pane`
    /// arm then leaves zoom before focusing, so the pane is revealed rather
    /// than focused while hidden (issue #1052).
    pub(crate) fn teleport_to_surface(
        &mut self,
        surface_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(loc) =
            crate::app::ipc_handler::find_pane_by_surface_id(&self.workspaces, surface_id, cx)
        else {
            cx.notify();
            return false;
        };
        let (ws_idx, pane) = (loc.workspace_idx, loc.pane);
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.set_active_tab(loc.tab_idx);
        }
        self.activate_workspace_at(ws_idx, WorkspaceFocusTarget::Pane { pane }, window, cx);
        // Keep the jump cycle coherent: a teleport counts as visiting that
        // surface, so the next Cmd+Shift+J continues from here.
        self.jump_cursor = Some(surface_id);
        true
    }
}

/// Pure cycle rule (unit-tested): first waiting surface when the cursor is
/// unset or gone from the set; otherwise the one after it, wrapping.
fn next_in_cycle(order: &[u64], last: Option<u64>) -> Option<u64> {
    if order.is_empty() {
        return None;
    }
    match last.and_then(|l| order.iter().position(|&x| x == l)) {
        Some(pos) => Some(order[(pos + 1) % order.len()]),
        None => Some(order[0]),
    }
}

#[cfg(test)]
mod tests {
    use super::next_in_cycle;

    #[test]
    fn empty_set_is_none() {
        assert_eq!(next_in_cycle(&[], None), None);
        assert_eq!(next_in_cycle(&[], Some(7)), None);
    }

    #[test]
    fn unset_or_stale_cursor_starts_at_first() {
        assert_eq!(next_in_cycle(&[10, 20, 30], None), Some(10));
        // Cursor points at a surface that stopped waiting: restart at first.
        assert_eq!(next_in_cycle(&[10, 20, 30], Some(99)), Some(10));
    }

    #[test]
    fn cycles_and_wraps() {
        assert_eq!(next_in_cycle(&[10, 20, 30], Some(10)), Some(20));
        assert_eq!(next_in_cycle(&[10, 20, 30], Some(30)), Some(10));
        // Single waiting pane: jumping again stays on it.
        assert_eq!(next_in_cycle(&[10], Some(10)), Some(10));
    }
}

type SessionPane = (usize, usize, gpui::Entity<crate::pane::Pane>, u64);

/// Every pane whose agent session's presented state matches, as
/// `(workspace index, tab index, pane, surface id)` in (workspace, tab,
/// layout) order. Walks `Tab::collect_panes`, so a pane parked in a zoomed
/// tab's `saved_layout` is included after the rendered ones. Shared with the
/// sidebar's `WaitingElseFirst` lookup (issue #1072) so the two cannot drift.
pub(super) fn matching_session_panes(
    workspaces: &[crate::workspace::Workspace],
    state_matches: impl Fn(&crate::ai_types::AgentState) -> bool,
    cx: &gpui::App,
) -> Vec<SessionPane> {
    let mut order: Vec<SessionPane> = Vec::new();
    for (ws_idx, ws) in workspaces.iter().enumerate() {
        let matching: std::collections::HashSet<u64> = ws
            .agent_sessions
            .values()
            // A session marked read (#408) presents no state to jump to.
            .filter(|s| s.presented_state().is_some_and(&state_matches))
            .filter_map(|s| s.surface_id)
            .collect();
        if matching.is_empty() {
            continue;
        }
        for (tab_idx, tab) in ws.tabs().iter().enumerate() {
            for pane in tab.collect_panes() {
                if let Some(t) = pane.read(cx).active_terminal_opt() {
                    let sid = t.entity_id().as_u64();
                    if matching.contains(&sid) {
                        order.push((ws_idx, tab_idx, pane.clone(), sid));
                    }
                }
            }
        }
    }
    order
}

#[cfg(test)]
mod waiting_navigation_tests {
    use super::*;
    use gpui::{AppContext, Focusable};

    #[gpui::test]
    fn next_waiting_target_includes_background_tabs(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let front = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        let back = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(2, cx));
        let sid = back.entity_id().as_u64();
        let front = cx.new(|cx| crate::pane::Pane::new(front, 1, cx));
        let back = cx.new(|cx| crate::pane::Pane::new(back, 2, cx));
        let mut ws = crate::workspace::Workspace::with_layout_and_id(
            1,
            "test",
            std::path::PathBuf::new(),
            crate::layout::LayoutTree::Leaf(front),
        );
        assert!(ws.open_tab(crate::workspace::Tab::new(
            "background",
            Some(crate::layout::LayoutTree::Leaf(back))
        )));
        ws.set_active_tab(0);
        let mut session = crate::ai_types::AgentSession::new(
            crate::agent_launcher::TerminalAgent::ClaudeCode,
            crate::ai_types::AgentState::WaitingForInput,
        );
        session.surface_id = Some(sid);
        ws.agent_sessions.insert(1, session);
        let workspaces = vec![ws];
        let order = cx.update(|_, cx| {
            matching_session_panes(
                &workspaces,
                |state| *state == crate::ai_types::AgentState::WaitingForInput,
                cx,
            )
        });
        assert_eq!(order.len(), 1);
        assert_eq!((order[0].0, order[0].1, order[0].3), (0, 1, sid));
        assert_eq!(
            next_in_cycle(&order.iter().map(|entry| entry.3).collect::<Vec<_>>(), None),
            Some(sid)
        );
    }

    type PaneEntity = gpui::Entity<crate::pane::Pane>;

    fn test_pane(id: u64, cx: &mut gpui::VisualTestContext) -> (PaneEntity, u64) {
        let view = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(id, cx));
        let sid = view.entity_id().as_u64();
        (cx.new(|cx| crate::pane::Pane::new(view, id, cx)), sid)
    }

    /// Issue #1052 fixture: two tabs. Tab 0 holds C alone, unzoomed, and is
    /// the active tab with C focused. Tab 1 holds A and B side by side, zoomed
    /// on A exactly as `handle_toggle_zoom` leaves it (B parked in
    /// `saved_layout`, A alone in `root`). Returns `(ws, c, a, b, b_sid)`.
    fn zoomed_background_tab(
        cx: &mut gpui::VisualTestContext,
    ) -> (
        crate::workspace::Workspace,
        PaneEntity,
        PaneEntity,
        PaneEntity,
        u64,
    ) {
        let (c, _) = test_pane(3, cx);
        let (a, _) = test_pane(1, cx);
        let (b, b_sid) = test_pane(2, cx);
        let tree = crate::layout::LayoutTree::from_panes_equal(
            crate::layout::SplitDirection::Vertical,
            vec![a.clone(), b.clone()],
        )
        .expect("two panes make a split");
        let mut ws = crate::workspace::Workspace::with_layout_and_id(
            1,
            "zoomed",
            std::path::PathBuf::new(),
            crate::layout::LayoutTree::Leaf(c.clone()),
        );
        assert!(ws.open_tab(crate::workspace::Tab::new("split", Some(tree))));
        cx.update(|window, cx| {
            a.update(cx, |pane, _| pane.zoomed = true);
            let tab = ws.tab_mut(1).expect("the split tab");
            tab.saved_layout = tab.root.take();
            tab.root = Some(crate::layout::LayoutTree::Leaf(a.clone()));
            ws.set_active_tab(0);
            c.read(cx).focus_handle(cx).focus(window, cx);
        });
        assert_eq!(ws.active_tab_idx(), 0, "tab 0 starts active");
        let split = &ws.tabs()[1];
        let root = split.root.as_ref().expect("zoomed root");
        assert!(
            split.is_zoomed() && !root.contains_leaf(&b),
            "B starts hidden"
        );
        (ws, c, a, b, b_sid)
    }

    /// After activation tab 1 must be the active tab, out of zoom, with a
    /// rendered root holding B (the whole saved layout back, A included) and
    /// B owning the focus. Tab 0 is left as it was.
    fn assert_revealed_and_focused(
        ws: &crate::workspace::Workspace,
        c: &PaneEntity,
        a: &PaneEntity,
        b: &PaneEntity,
        cx: &mut gpui::VisualTestContext,
    ) {
        assert_eq!(ws.active_tab_idx(), 1, "B's tab becomes the active tab");
        let tab = ws.active_tab();
        assert!(
            !tab.is_zoomed(),
            "activating a zoom-hidden pane leaves zoom"
        );
        let root = tab.root.as_ref().expect("a rendered root");
        assert!(root.contains_leaf(b), "the rendered root contains B");
        assert!(root.contains_leaf(a), "the saved layout came back whole");
        let first = &ws.tabs()[0];
        assert!(
            !first.is_zoomed() && first.root.as_ref().is_some_and(|r| r.contains_leaf(c)),
            "the other tab is untouched"
        );
        cx.update(|window, cx| {
            assert!(!a.read(cx).zoomed, "A is no longer flagged zoomed");
            assert!(
                b.read(cx).focus_handle(cx).is_focused(window),
                "B has focus"
            );
            assert_eq!(root.focused_pane(window, cx).as_ref(), Some(b));
        });
    }

    /// Issue #1052, Pane Overview path: the overview lists B from the
    /// zoom-saved tree; selecting its card runs `teleport_to_surface`, whose
    /// body is replayed here (resolve the surface, make its tab active, then
    /// the shared `WorkspaceFocusTarget::Pane` focus).
    #[gpui::test]
    fn pane_overview_selection_reveals_a_zoom_hidden_pane(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let (ws, c, a, b, b_sid) = zoomed_background_tab(cx);
        let mut workspaces = vec![ws];
        let selected = cx
            .update(|window, cx| {
                crate::app::pane_overview::collect_cards(&workspaces, 0, window, cx)
            })
            .into_iter()
            .find(|card| card.surface_id == b_sid)
            .expect("the overview lists the zoom-hidden pane")
            .surface_id;
        cx.update(|window, cx| {
            let loc = crate::app::ipc_handler::find_pane_by_surface_id(&workspaces, selected, cx)
                .expect("the selected surface resolves");
            assert_eq!((loc.tab_idx, &loc.pane), (1, &b));
            let ws = &mut workspaces[loc.workspace_idx];
            ws.set_active_tab(loc.tab_idx);
            super::super::focus_pane_in_active_tab(ws, &loc.pane, window, cx);
        });
        assert_revealed_and_focused(&workspaces[0], &c, &a, &b, cx);
    }

    /// Issue #1052, Jump Next Waiting path: B hosts the waiting agent, so the
    /// cycle picks it; `jump_next_session_where`'s body is replayed here.
    #[gpui::test]
    fn jump_next_waiting_reveals_a_zoom_hidden_pane(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let (mut ws, c, a, b, b_sid) = zoomed_background_tab(cx);
        let mut session = crate::ai_types::AgentSession::new(
            crate::agent_launcher::TerminalAgent::ClaudeCode,
            crate::ai_types::AgentState::WaitingForInput,
        );
        session.surface_id = Some(b_sid);
        ws.agent_sessions.insert(1, session);
        let mut workspaces = vec![ws];
        cx.update(|window, cx| {
            let order = matching_session_panes(
                &workspaces,
                |state| *state == crate::ai_types::AgentState::WaitingForInput,
                cx,
            );
            let ids: Vec<u64> = order.iter().map(|entry| entry.3).collect();
            let next = next_in_cycle(&ids, None).expect("B is waiting");
            let (ws_idx, tab_idx, pane, _) = order
                .into_iter()
                .find(|entry| entry.3 == next)
                .expect("the cycle target is in the order");
            assert_eq!((tab_idx, &pane), (1, &b));
            let ws = &mut workspaces[ws_idx];
            ws.set_active_tab(tab_idx);
            super::super::focus_pane_in_active_tab(ws, &pane, window, cx);
        });
        assert_revealed_and_focused(&workspaces[0], &c, &a, &b, cx);
    }

    /// Activating the pane that is already the zoomed one keeps the zoom: the
    /// reveal only fires for a pane absent from the rendered root.
    #[gpui::test]
    fn activating_the_zoomed_pane_keeps_the_zoom(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let (mut ws, _c, a, _b, _) = zoomed_background_tab(cx);
        ws.set_active_tab(1);
        cx.update(|window, cx| super::super::focus_pane_in_active_tab(&mut ws, &a, window, cx));
        assert_eq!(ws.active_tab_idx(), 1);
        assert!(ws.is_zoomed(), "the zoomed pane is already rendered");
        cx.update(|window, cx| {
            assert!(a.read(cx).zoomed);
            assert!(a.read(cx).focus_handle(cx).is_focused(window));
        });
    }
}
