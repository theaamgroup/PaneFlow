//! Tab handlers (add/close) for `PaneFlowApp`.
//!
//! Part of the US-023 workspace_ops decomposition.

use gpui::{AppContext, Context, Focusable, Window};

use crate::PaneFlowApp;
use crate::limits::MAX_PANE_SURFACES;
use crate::pane::can_add_tab;
use crate::terminal::TerminalView;
use crate::{CloseTab, NewTab};

impl PaneFlowApp {
    pub(crate) fn handle_new_tab(
        &mut self,
        _: &NewTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.active_workspace().and_then(|ws| {
            ws.root
                .as_ref()
                .and_then(|root| root.focused_pane(window, cx))
        }) else {
            return;
        };
        if !can_add_tab(pane.read(cx).tabs.len(), MAX_PANE_SURFACES) {
            self.show_toast(
                format!("Maximum tab count reached ({MAX_PANE_SURFACES})"),
                cx,
            );
            return;
        }
        let Some((ws_id, cwd)) = self.active_workspace().map(|ws| {
            let cwd = (!ws.cwd.is_empty()).then(|| std::path::PathBuf::from(&ws.cwd));
            (ws.id, cwd)
        }) else {
            return;
        };
        let terminal = cx.new(|cx| TerminalView::with_cwd(ws_id, cwd, None, cx));
        cx.subscribe(&terminal, Self::handle_terminal_event)
            .detach();
        pane.update(cx, |p, cx| {
            p.add_tab(terminal, cx);
        });
        pane.read(cx).focus_handle(cx).focus(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn handle_close_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ws) = self.active_workspace()
            && let Some(root) = &ws.root
            && let Some(pane) = root.focused_pane(window, cx)
        {
            // close_selected_tab emits PaneEvent::Remove if last tab,
            // which is handled by handle_pane_event via cx.subscribe.
            pane.update(cx, |p, cx| {
                p.close_selected_tab(cx);
            });
            // If pane still has tabs, refocus
            if !pane.read(cx).tabs.is_empty() {
                pane.read(cx).focus_handle(cx).focus(window, cx);
            } else if let Some(ws) = self.active_workspace()
                && let Some(root) = &ws.root
            {
                root.focus_first(window, cx);
            }
            self.save_session(cx);
            cx.notify();
        }
    }
}
