//! Pane-swap mode toggle for `PaneFlowApp`.
//!
//! Entering swap mode arms the source pane's `TerminalView` so it
//! intercepts Escape to cancel (issue #299: per-view flag, no process-global).
//! Focus-direction keys then swap the source pane with the target (see
//! [`super::focus`]). `set_swap_source` is the only writer of `swap_source`,
//! so arming and disarming the terminal can never drift from it.
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
        } else if let Some(ws) = self.active_workspace()
            && let Some(root) = &ws.active_tab().root
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

    /// Issue #299: the single writer of swap state. Disarms the previous
    /// source pane's terminal and arms the new one, so `swap_source` and the
    /// per-view Escape flag change together and clearing here is enough.
    pub(crate) fn set_swap_source(&mut self, source: Option<Entity<Pane>>, cx: &mut Context<Self>) {
        if let Some(previous) = self.swap_source.take() {
            Self::arm_swap_terminal(&previous, false, cx);
        }
        if let Some(pane) = &source {
            Self::arm_swap_terminal(pane, true, cx);
        }
        self.swap_source = source;
    }

    fn arm_swap_terminal(pane: &Entity<Pane>, armed: bool, cx: &mut Context<Self>) {
        let terminals: Vec<_> = pane.read(cx).terminals().cloned().collect();
        for terminal in terminals {
            terminal.update(cx, |view, cx| view.set_swap_mode_armed(armed, cx));
        }
    }
}
