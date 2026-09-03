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

    /// Issue #299: the single writer of swap state. Disarms whatever was
    /// armed last time and arms every leaf pane's terminals across all
    /// workspaces and tabs, so Escape cancels from any pane the user focuses
    /// meanwhile, including after a tab or workspace switch (the pre-#299
    /// process-global check allowed that too), and `swap_source` and the
    /// per-view flags change together.
    pub(crate) fn set_swap_source(&mut self, source: Option<Entity<Pane>>, cx: &mut Context<Self>) {
        for pane in std::mem::take(&mut self.swap_armed_panes) {
            Self::arm_swap_terminal(&pane, false, cx);
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
            self.swap_armed_panes = armed;
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
