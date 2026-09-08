//! Pane-swap mode toggle for `PaneFlowApp`.
//!
//! Entering swap mode arms every `TerminalView` in every tab of every
//! workspace, so Escape from whichever pane ends up focused, after a click, a
//! tab switch, or a workspace switch, still cancels (issue #299: per-view flags
//! with one writer, no process-global). Focus-direction keys then swap the source pane
//! with the target (see [`super::focus`]). `set_swap_source` is the only
//! writer of `swap_source`, so arming and disarming can never drift from it.
//!
//! Part of the US-023 workspace_ops decomposition.

use gpui::{Context, Entity, Window};

use crate::pane::Pane;
use crate::{PaneFlowApp, SwapPane};

impl PaneFlowApp {
    pub(crate) fn handle_swap_pane(
        &mut self,
        _: &SwapPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.swap_source.is_some() {
            // Already in swap mode - toggle off (cancel)
            self.set_swap_source(None, cx);
        } else if let Some(root) = self.nav_root()
            && root.leaf_count() > 1
        {
            // Enter swap mode: record the currently focused pane
            if let Some(pane) = root.focused_pane(window, cx) {
                self.set_swap_source(Some(pane), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn cancel_swap_mode(&mut self, cx: &mut Context<Self>) {
        if self.swap_source.is_some() {
            self.set_swap_source(None, cx);
            cx.notify();
        }
    }

    /// Issue #299: the single writer of swap state. Disarms whatever was
    /// armed last time and arms every leaf pane's terminals across all
    /// workspaces and tabs, so Escape cancels from any pane the user focuses
    /// meanwhile, including after a tab or workspace switch (the pre-#299
    /// process-global check allowed that too), and `swap_source` and the
    /// per-view flags change together.
    ///
    /// Issue #471: every handle this stores is weak. A pane closed while the
    /// swap is armed must still drop, or its kill ladder never runs; a pane
    /// that dropped meanwhile simply needs no disarming, since the per-view
    /// flag went with it.
    pub(crate) fn set_swap_source(&mut self, source: Option<Entity<Pane>>, cx: &mut Context<Self>) {
        for pane in std::mem::take(&mut self.swap_armed_panes) {
            if let Some(pane) = pane.upgrade() {
                Self::arm_swap_terminal(&pane, false, cx);
            }
        }
        if let Some(pane) = &source {
            let mut armed: Vec<Entity<Pane>> = self
                .workspaces
                .iter()
                .flat_map(|ws| ws.tabs().iter())
                .filter_map(|tab| tab.root.as_ref())
                .flat_map(|root| root.collect_leaves())
                .collect();
            if !armed.contains(pane) {
                armed.push(pane.clone());
            }
            for pane in &armed {
                Self::arm_swap_terminal(pane, true, cx);
            }
            self.swap_armed_panes = armed.iter().map(Entity::downgrade).collect();
        }
        self.swap_source = source.as_ref().map(Entity::downgrade);
    }

    fn arm_swap_terminal(pane: &Entity<Pane>, armed: bool, cx: &mut Context<Self>) {
        let terminals: Vec<_> = pane.read(cx).terminals().cloned().collect();
        for terminal in terminals {
            terminal.update(cx, |view, cx| view.set_swap_mode_armed(armed, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::source_probe::source_slice;

    /// Issue #471: `set_swap_source` arms EVERY leaf pane of every tab of
    /// every workspace, and nothing in any close path disarms it - not
    /// `remove_pane_from_tree`, not `close_workspace_tab`, not
    /// `close_workspace_at_inner`, which stands down the workspace and tab
    /// menus and never mentions the swap. Only Escape in an armed terminal, a
    /// second `SwapPane`, or a focus-direction key disarm.
    ///
    /// Held strongly, that made an armed swap a second owner of every pane in
    /// the app: close a pane, a tab, or a whole workspace while armed and its
    /// `Pane` never dropped, so `TerminalState::Drop` never ran its kill
    /// ladder and the child was never signalled - invisible to
    /// `live_terminal_session_ids`, which walks the tree the pane just left.
    /// Weak handles are what make the disarm paths a convenience rather than
    /// a correctness requirement. The disarm loop may then find a pane
    /// already gone, which needs no disarming: `Pane::surface` is the sole
    /// owner of a pane's `Entity<TerminalView>`, so the per-view
    /// `swap_mode_armed` flag cannot outlive the pane that carried it.
    /// `pane.rs`'s `closing_a_tab_releases_its_panes_unless_something_else_holds_one`
    /// pins the release a close depends on - not the kill ladder itself,
    /// which needs a live PTY and belongs to `terminal::pty_session`.
    #[test]
    fn swap_state_references_panes_and_never_owns_them() {
        let fields = source_slice(
            include_str!("../../main.rs"),
            "/// Source pane for swap mode",
            "impl PaneFlowApp {",
        );
        for required in [
            "swap_source: Option<WeakEntity<crate::pane::Pane>>",
            "swap_armed_panes: Vec<WeakEntity<crate::pane::Pane>>",
        ] {
            assert!(
                fields.contains(required),
                "an armed swap must not own a pane (issue #471); expected \
                 `{required}` in: {fields}"
            );
        }

        // The single writer must store downgrades, and must tolerate a pane
        // that has already gone rather than resurrecting it to disarm.
        let writer = source_slice(
            include_str!("swap.rs"),
            "pub(crate) fn set_swap_source(",
            "fn arm_swap_terminal(",
        );
        for required in [
            "self.swap_armed_panes = armed.iter().map(Entity::downgrade).collect();",
            "self.swap_source = source.as_ref().map(Entity::downgrade);",
            "if let Some(pane) = pane.upgrade() {",
        ] {
            assert!(writer.contains(required), "missing `{required}`: {writer}");
        }

        // The consumer upgrades before swapping, so a source pane closed while
        // armed is refused rather than dereferenced.
        let consume = source_slice(
            include_str!("focus.rs"),
            "if let Some(source) = self.swap_source.clone() {",
            "if let Some(root) = self.nav_root() {",
        );
        assert!(
            consume.contains("let source = source.upgrade();"),
            "the swap consumer must upgrade rather than hold: {consume}"
        );
        // A departed source must take the SAME path as one that left the
        // tree - focus still moves, the swap is refused with one toast - so
        // the fix cannot quietly change what a direction key does.
        let refuse = source_slice(
            include_str!("focus.rs"),
            "let source = source.upgrade();",
            "} else if !moved {",
        );
        assert!(
            refuse.contains("root.focus_in_direction(dir, window, cx)")
                && refuse.contains(".zip(self.nav_root_mut())")
                && refuse.contains("Swap source pane is no longer available"),
            "a departed source must still move focus and refuse once: {refuse}"
        );
    }
}
