//! Issue #83: the confirmation that stands between a user close gesture and
//! `kill(-pid, SIGTERM)` on a live coding agent's whole process group.
//!
//! [`crate::app::close_guard`] answers *whether* to ask; this module owns the
//! asking: the request/confirm/cancel entry points every modal close path
//! funnels through, and the modal itself.
//!
//! Split out of `close_guard.rs` so the predicate stays free of GPUI and
//! `close_guard.rs` stays near the repo's ~280-LOC module convention (see
//! `layout/close.rs`).

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, StatefulInteractiveElement, Styled, Window, deferred,
    div, hsla, px,
};

use crate::agent_launcher::TerminalAgent;
use crate::app::close_guard::{
    CloseTarget, ConfirmStyle, PendingClose, SurfaceCloseState, agent_needing_confirmation,
    agents_needing_confirmation_count,
};
use crate::app::workspace_ops::{capture_closed_pane_record, push_closed_record};
use crate::pane::Pane;
use crate::ui_primitives::AnimatedHoverExt;
use crate::{ClosedRecord, PaneFlowApp};

/// Longest label the modal will echo back. The tab title is user-typed and
/// already bounded, but a pane label can come straight from an OSC title,
/// which is untrusted terminal-controlled text.
const MAX_CONFIRM_LABEL_CHARS: usize = 48;

/// Scrub and clamp a label before it lands in the modal copy.
///
/// The pane label can be an OSC title: a bidi override there could visually
/// reverse the `Close "…"?` around it and make the modal name a different
/// surface than the one about to die. Same scrub the port-conflict tooltip
/// applies to the same source.
fn confirm_label(raw: &str) -> String {
    crate::markdown::strip_bidi_zero_width(raw.chars().take(MAX_CONFIRM_LABEL_CHARS).collect())
}

/// Title line for the close confirmation.
pub(crate) fn close_confirm_title(target_is_tab: bool, label: &str) -> String {
    let noun = if target_is_tab { "tab" } else { "pane" };
    let label = label.trim();
    if label.is_empty() {
        format!("Close this {noun}?")
    } else {
        format!("Close {noun} \u{201c}{label}\u{201d}?")
    }
}

/// Body copy for the close confirmation.
///
/// Pure so the two facts issue #83 insists on can be asserted without a
/// window: closing stops the agent **and everything it started** (the kill is
/// `kill(-pid, …)` on the whole process group), and undo brings the surface
/// back but does **not** resume the agent (restore spawns a brand-new PTY and
/// replays scrollback as inert text).
///
/// `undo_shortcut` is `None` when the user has unassigned the undo binding;
/// the phrase then names the action instead of printing a key that is not
/// bound.
pub(crate) fn close_confirm_body(
    agent: TerminalAgent,
    extra_agents: usize,
    undo_shortcut: Option<&str>,
) -> String {
    let name = agent.display_name();
    let subject = match extra_agents {
        0 => format!("{name} is still running here"),
        1 => format!("{name} and 1 other agent are still running here"),
        n => format!("{name} and {n} other agents are still running here"),
    };
    // Singular vs plural: the guard names one agent but the close kills every
    // qualifying surface in the target, so the consequence clauses have to
    // agree with the count, not with the name.
    let (object, doer, back, their, agents) = if extra_agents == 0 {
        ("it", "it", "it", "its", "the agent")
    } else {
        ("them", "they", "them", "their", "the agents")
    };
    let opener = match undo_shortcut {
        Some(key) => key.to_string(),
        // Undo is unassigned: name the action rather than print a keystroke
        // the user cannot press.
        None => "Undo close".to_string(),
    };
    format!(
        "{subject}. Closing stops {object} and everything {doer} started. \
         {opener} brings {back} back with {their} scrollback, but does not resume {agents}."
    )
}

/// Resolve a pending close's stable `(workspace_id, tab_id)` back to live
/// indices.
///
/// `workspaces` is `(workspace id, that workspace's tab ids in order)`.
/// A tab can be dragged, closed, or moved while the modal is up, so the
/// confirm path re-resolves instead of trusting the indices it armed with;
/// `None` means "close nothing".
pub(crate) fn pending_close_tab_indices(
    workspaces: &[(u64, Vec<u64>)],
    workspace_id: u64,
    tab_id: u64,
) -> Option<(usize, usize)> {
    let ws_idx = workspaces.iter().position(|(id, _)| *id == workspace_id)?;
    let tab_idx = workspaces[ws_idx].1.iter().position(|id| *id == tab_id)?;
    Some((ws_idx, tab_idx))
}

impl PaneFlowApp {
    /// The single writer of `pending_close` (R9): every arm, disarm, and
    /// confirm goes through here, so Task 4 only has to add the
    /// "disarm the OUTGOING target" body in one place.
    pub(crate) fn set_pending_close(&mut self, next: Option<PendingClose>, cx: &mut Context<Self>) {
        // A modal can be armed from a `Window`-less path (`handle_pane_event`
        // subscribes with plain `cx.subscribe`), so the next render claims
        // focus for it - the `fleet_search_pending_focus` pattern.
        self.pending_close_focus_claim = next
            .as_ref()
            .is_some_and(|p| p.style == ConfirmStyle::Modal);
        self.pending_close = next;
        cx.notify();
    }

    /// Copy the guard state out of every terminal surface in `panes`, on the
    /// GPUI thread, so the predicate itself stays pure.
    fn surface_close_states(panes: &[Entity<Pane>], cx: &App) -> Vec<SurfaceCloseState> {
        let mut states = Vec::new();
        for pane in panes {
            let terminals: Vec<Entity<crate::terminal::TerminalView>> =
                pane.read(cx).terminals().cloned().collect();
            for tv in terminals {
                let t = &tv.read(cx).terminal;
                states.push(SurfaceCloseState {
                    detected_agent: t.detected_agent,
                    agent_confirmed: t.agent_confirmed,
                    agent_declared_until: t.agent_declared_until,
                    // Reuse the scan's own admission test rather than invent a
                    // third liveness rule: anything it rejects is never
                    // deposited and never re-confirmed, so `!exited` alone
                    // would report a live agent for a dead one forever.
                    child_is_live: crate::app::event_handlers::terminal_identity_is_scannable(t),
                });
            }
        }
        states
    }

    fn tab_close_states(&self, ws_idx: usize, tab_idx: usize, cx: &App) -> Vec<SurfaceCloseState> {
        let Some(tab) = self
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs().get(tab_idx))
        else {
            return Vec::new();
        };
        Self::surface_close_states(&tab.collect_panes(), cx)
    }

    /// `(workspace id, that workspace's tab ids in order)`, the input
    /// [`pending_close_tab_indices`] resolves against.
    fn workspace_tab_ids(&self) -> Vec<(u64, Vec<u64>)> {
        self.workspaces
            .iter()
            .map(|ws| (ws.id, ws.tabs().iter().map(|tab| tab.id).collect()))
            .collect()
    }

    /// The one entry point every user-initiated tab close goes through.
    ///
    /// No live agent means today's behaviour, unchanged: close immediately.
    /// Otherwise the target is remembered by stable id (never by index - rows
    /// reorder under a drag) and the confirmation is armed.
    pub(crate) fn request_close_workspace_tab(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        style: ConfirmStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let states = self.tab_close_states(ws_idx, tab_idx, cx);
        let now = std::time::Instant::now();
        let Some(agent) = agent_needing_confirmation(&states, now) else {
            self.close_workspace_tab(ws_idx, tab_idx, window, cx);
            return;
        };
        let Some((workspace_id, tab_id, label)) = self.workspaces.get(ws_idx).and_then(|ws| {
            ws.tabs().get(tab_idx).map(|tab| {
                (
                    ws.id,
                    tab.id,
                    crate::app::sidebar::tab_display_title(tab, tab_idx),
                )
            })
        }) else {
            return;
        };
        self.set_pending_close(
            Some(PendingClose {
                target: CloseTarget::Tab {
                    workspace_id,
                    tab_id,
                },
                style,
                agent,
                extra_agents: agents_needing_confirmation_count(&states, now).saturating_sub(1),
                label: confirm_label(&label),
            }),
            cx,
        );
    }

    /// Arm a confirmation for `pane` when closing it would kill a live agent.
    /// `true` means one is now pending and the caller must NOT close.
    ///
    /// Deliberately `Window`-free: the inline half of this (Task 4) is reached
    /// from `handle_pane_event`, which has no `&mut Window` - all five pane
    /// subscriptions are plain `cx.subscribe`, and converting them is a
    /// five-site blast radius for no gain.
    pub(crate) fn arm_pending_close_pane(
        &mut self,
        pane: &Entity<Pane>,
        style: ConfirmStyle,
        cx: &mut Context<Self>,
    ) -> bool {
        let states = Self::surface_close_states(std::slice::from_ref(pane), cx);
        let now = std::time::Instant::now();
        let Some(agent) = agent_needing_confirmation(&states, now) else {
            return false;
        };
        let label = Pane::surface_title(&pane.read(cx).surface, cx);
        self.set_pending_close(
            Some(PendingClose {
                target: CloseTarget::Pane { pane: pane.clone() },
                style,
                agent,
                extra_agents: agents_needing_confirmation_count(&states, now).saturating_sub(1),
                label: confirm_label(&label),
            }),
            cx,
        );
        true
    }

    /// Confirm-or-close for a pane close that has no window-dependent close of
    /// its own (the sidebar pane context menu; Task 4's header `x`).
    /// `Window`-free for the same reason [`Self::arm_pending_close_pane`] is.
    pub(crate) fn request_close_pane(
        &mut self,
        pane: Entity<Pane>,
        style: ConfirmStyle,
        cx: &mut Context<Self>,
    ) {
        if self.arm_pending_close_pane(&pane, style, cx) {
            return;
        }
        self.close_pane_undoably(&pane, cx);
    }

    /// Close `pane` for real: push its undo record, then drop it out of the
    /// layout tree. Shared by [`crate::pane::PaneEvent::CloseRequested`] and
    /// the confirm path, so a confirmed close is exactly as undoable as an
    /// unguarded one. The tree mutation drops the pane entity, so the capture
    /// has to come first.
    pub(crate) fn close_pane_undoably(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        if let Some(ws) = self
            .workspaces
            .iter()
            .find(|ws| ws.tab_index_containing_pane(pane).is_some())
        {
            let workspace_id = ws.id;
            if let Some(record) = capture_closed_pane_record(pane, workspace_id, cx) {
                push_closed_record(&mut self.closed_items, ClosedRecord::Pane(record));
            }
        }
        // Saves the session and repaints.
        self.remove_pane_from_tree(pane, cx);
    }

    /// Confirm a pending TAB close. Re-resolves the stable ids back to live
    /// indices first: a tab can be dragged, closed, or have its workspace
    /// removed while the modal is up, and a stale index must close NOTHING.
    pub(crate) fn confirm_pending_close_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(PendingClose {
            target:
                CloseTarget::Tab {
                    workspace_id,
                    tab_id,
                },
            ..
        }) = self.pending_close.clone()
        else {
            return;
        };
        self.set_pending_close(None, cx);
        let Some((ws_idx, tab_idx)) =
            pending_close_tab_indices(&self.workspace_tab_ids(), workspace_id, tab_id)
        else {
            // The target went away under the modal. Close nothing, but still
            // hand focus back - the modal was holding it.
            self.restore_focus_after_close_confirm(window, cx);
            return;
        };
        // `close_workspace_tab` re-focuses on the way out, so the confirm path
        // needs no restore of its own.
        self.close_workspace_tab(ws_idx, tab_idx, window, cx);
    }

    /// Confirm a pending PANE close. `Window`-free by design - see
    /// [`Self::arm_pending_close_pane`]; tree removal needs no `Window`, and
    /// the caller that HAS one restores focus afterwards.
    pub(crate) fn confirm_pending_close_pane(
        &mut self,
        pane: Entity<Pane>,
        cx: &mut Context<Self>,
    ) {
        if self
            .pending_close
            .as_ref()
            .is_some_and(|p| p.targets_pane(&pane))
        {
            self.set_pending_close(None, cx);
        }
        // A pane dropped from the tree while the modal was up is no longer
        // owned by any workspace; `remove_pane_from_tree` is a clean no-op.
        self.close_pane_undoably(&pane, cx);
    }

    /// Modal Confirm button / Enter: route to whichever half the target needs.
    pub(crate) fn confirm_pending_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.pending_close.as_ref().map(|p| p.target.clone()) {
            Some(CloseTarget::Tab { .. }) => self.confirm_pending_close_tab(window, cx),
            Some(CloseTarget::Pane { pane }) => {
                self.confirm_pending_close_pane(pane, cx);
                // The `Window`-free half cannot hand focus back, and this
                // caller has a `Window`, so do it here: the modal was the
                // focused element and dropping it silently is the issue #108
                // stranding class.
                self.restore_focus_after_close_confirm(window, cx);
            }
            None => {}
        }
    }

    /// Esc / Cancel / a click on the backdrop.
    ///
    /// Takes a `&mut Window` on purpose, and is NOT called from
    /// `dismiss_transient_surfaces`: this modal tracks focus, so clearing it
    /// from a `Window`-less caller would leave the window with nothing
    /// focused (issue #108).
    pub(crate) fn cancel_pending_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_close.is_none() {
            return;
        }
        self.set_pending_close(None, cx);
        self.restore_focus_after_close_confirm(window, cx);
    }

    fn restore_focus_after_close_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = match self.workspaces.get(self.active_idx) {
            Some(ws) => ws.focus_first(window, cx),
            None => false,
        };
        if !focused {
            window.focus(&self.empty_workspace_focus, cx);
        }
    }

    /// False once the pending close's target has gone away underneath it (an
    /// IPC `workspace.close`, a shell that exited). The render stands the
    /// modal down rather than asking about something that no longer exists.
    pub(crate) fn pending_close_target_is_live(&self, pending: &PendingClose) -> bool {
        match &pending.target {
            CloseTarget::Tab {
                workspace_id,
                tab_id,
            } => pending_close_tab_indices(&self.workspace_tab_ids(), *workspace_id, *tab_id)
                .is_some(),
            CloseTarget::Pane { pane } => self
                .workspaces
                .iter()
                .any(|ws| ws.tab_index_containing_pane(pane).is_some()),
        }
    }

    pub(crate) fn handle_close_confirm_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => self.cancel_pending_close(window, cx),
            "enter" => self.confirm_pending_close(window, cx),
            _ => {}
        }
    }

    /// Centred confirm card over a dimmed backdrop. Styling follows
    /// `custom_buttons_modal.rs` (`deferred()` backdrop + centered card +
    /// focus-handled key input); every colour comes from `UiColors`, and the
    /// danger accent is `ui.vc_deleted` - there is no `ui.danger`.
    pub(crate) fn render_close_confirm_dialog(
        &self,
        pending: &PendingClose,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let title = close_confirm_title(
            matches!(pending.target, CloseTarget::Tab { .. }),
            &pending.label,
        );
        let body = close_confirm_body(
            pending.agent,
            pending.extra_agents,
            self.shortcut_for_action("undo_close_pane"),
        );
        let cancel_resting = ui.subtle;
        let cancel_hover = ui.surface;

        let buttons = div()
            .mt(px(6.))
            .flex()
            .flex_row()
            .justify_end()
            .gap(px(8.))
            .child(
                div()
                    .id("close-confirm-cancel")
                    .px(px(14.))
                    .py(px(7.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(ui.text)
                    .animated_hover_bg(cancel_resting, cancel_hover)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.cancel_pending_close(window, cx);
                        cx.stop_propagation();
                    }))
                    .child("Cancel"),
            )
            .child(
                div()
                    .id("close-confirm-accept")
                    .px(px(14.))
                    .py(px(7.))
                    .rounded(px(6.))
                    .bg(ui.vc_deleted)
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.base)
                    .animated_hover(|style, delta| {
                        style.opacity(1.0 - 0.12 * delta);
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.confirm_pending_close(window, cx);
                        cx.stop_propagation();
                    }))
                    .child("Close anyway"),
            );

        let backdrop = div()
            .id("close-confirm-backdrop")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.cancel_pending_close(window, cx);
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("close-confirm-dialog")
                    .occlude()
                    .track_focus(&self.pending_close_focus)
                    .on_key_down(cx.listener(Self::handle_close_confirm_key_down))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .w(px(360.))
                    .bg(ui.overlay)
                    .border_1()
                    .border_color(ui.border)
                    .rounded(px(10.))
                    .shadow_lg()
                    .p(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text)
                            .child(title),
                    )
                    .child(div().text_size(px(12.)).text_color(ui.muted).child(body))
                    .child(buttons),
            );

        // Above every other overlay: this one guards an irreversible kill.
        deferred(backdrop).with_priority(9).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_confirm_body_names_the_agent_and_both_consequences() {
        let body = close_confirm_body(TerminalAgent::ClaudeCode, 0, Some("Cmd+Shift+T"));
        assert!(
            body.contains("Claude Code"),
            "the copy must name the agent it is about to kill: {body}"
        );
        assert!(
            body.contains("everything it started"),
            "the kill is `kill(-pid, …)` on the whole process group, and the copy has to say \
             so: {body}"
        );
        assert!(
            body.contains("Cmd+Shift+T"),
            "a bound undo shortcut belongs in the copy: {body}"
        );
        assert!(
            body.contains("does not resume"),
            "undo replays scrollback as inert text and never resumes the agent; the copy must \
             not imply otherwise: {body}"
        );
    }

    #[test]
    fn close_confirm_body_counts_the_agents_it_does_not_name() {
        let one = close_confirm_body(TerminalAgent::Codex, 1, Some("Cmd+Shift+T"));
        assert!(
            one.contains("1 other agent") && !one.contains("1 other agents"),
            "singular form for exactly one unnamed agent: {one}"
        );
        assert!(
            one.contains("everything they started"),
            "more than one agent dies, so the consequence clause is plural: {one}"
        );
        assert!(
            one.contains("resume the agents"),
            "the undo clause is plural too: {one}"
        );

        let many = close_confirm_body(TerminalAgent::Codex, 3, Some("Cmd+Shift+T"));
        assert!(
            many.contains("3 other agents"),
            "plural form for more than one unnamed agent: {many}"
        );
    }

    #[test]
    fn close_confirm_body_omits_an_unbound_undo_shortcut() {
        let body = close_confirm_body(TerminalAgent::ClaudeCode, 0, None);
        assert!(
            !body.contains("Cmd") && !body.contains('+'),
            "with undo unassigned the copy must not print a keystroke at all: {body}"
        );
        assert!(
            body.contains("does not resume"),
            "the fact still has to survive an unassigned shortcut: {body}"
        );
    }

    #[test]
    fn close_confirm_title_quotes_the_label_and_names_the_target() {
        assert_eq!(
            close_confirm_title(true, "Fix the bug"),
            "Close tab \u{201c}Fix the bug\u{201d}?"
        );
        assert_eq!(
            close_confirm_title(false, "claude"),
            "Close pane \u{201c}claude\u{201d}?"
        );
    }

    #[test]
    fn close_confirm_title_falls_back_when_the_label_is_blank() {
        assert_eq!(close_confirm_title(true, ""), "Close this tab?");
        assert_eq!(close_confirm_title(false, "   "), "Close this pane?");
    }

    #[test]
    fn confirm_label_strips_bidi_controls_and_clamps_length() {
        // A pane label can be an OSC title: untrusted terminal-controlled text.
        let spoof = format!("shell{}gnitide", '\u{202e}');
        let cleaned = confirm_label(&spoof);
        assert!(
            !cleaned.contains('\u{202e}'),
            "a right-to-left override would let the title spoof the quoted name: {cleaned}"
        );
        let long: String = std::iter::repeat_n('x', MAX_CONFIRM_LABEL_CHARS + 40).collect();
        assert_eq!(
            confirm_label(&long).chars().count(),
            MAX_CONFIRM_LABEL_CHARS
        );
    }

    /// Issue #83 mandates the guard on all four MODAL close paths, and the
    /// style is CARRIED to the pending close, never inferred from the call
    /// site. There is no `PaneFlowApp` to call these on (`PaneFlowApp::new`
    /// binds a real Unix socket), so assert the seam that IS reachable: the
    /// call sites as written.
    #[test]
    fn every_modal_close_path_routes_through_the_guard() {
        let tab_src = include_str!("workspace_ops/tab.rs");
        let close_tab = tab_src
            .split("pub(crate) fn handle_close_tab")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("handle_close_tab body");
        assert!(
            close_tab.contains("request_close_workspace_tab")
                && close_tab.contains("ConfirmStyle::Modal"),
            "Cmd+W must go through the guard with an explicit Modal style: {close_tab}"
        );
        assert!(
            !close_tab.contains("self.close_workspace_tab("),
            "Cmd+W must not keep a second, unguarded route into the close"
        );

        let menu_src = include_str!("sidebar/context_menu.rs");
        let tab_menu = menu_src
            .split("\"tab-context-close\".into()")
            .nth(1)
            .and_then(|rest| rest.split("));").next())
            .expect("tab context menu Close item");
        assert!(
            tab_menu.contains("request_close_workspace_tab")
                && tab_menu.contains("ConfirmStyle::Modal"),
            "the tab context menu's Close must go through the guard: {tab_menu}"
        );
        let pane_menu = menu_src
            .split("\"pane-context-close\".into()")
            .nth(1)
            .and_then(|rest| rest.split("));").next())
            .expect("pane context menu Close Pane item");
        assert!(
            pane_menu.contains("request_close_pane") && pane_menu.contains("ConfirmStyle::Modal"),
            "the pane context menu's Close Pane must go through the guard, and MODALLY - the \
             menu has already dismissed itself, so an inline affordance would be dead: {pane_menu}"
        );
        assert!(
            !pane_menu.contains("save_session"),
            "the session save belongs in the close path, or a pending close persists the \
             pre-close tree"
        );

        let ops_src = include_str!("workspace_ops/mod.rs");
        let close_pane = ops_src
            .split("pub(crate) fn handle_close_pane")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("handle_close_pane body");
        assert!(
            close_pane.contains("arm_pending_close_pane")
                && close_pane.contains("ConfirmStyle::Modal"),
            "Cmd+Shift+W must ask too: two adjacent close gestures with opposite safety \
             behaviour is worse than either alone: {close_pane}"
        );
    }

    /// The two paths that must NEVER raise a modal.
    #[test]
    fn the_unguarded_close_paths_stay_unguarded() {
        // A picker escape is a dismissal gesture, not a close request.
        let palette = include_str!("pane_palette.rs");
        assert!(
            palette.contains("self.close_workspace_tab(ws_idx, tab_idx, window, cx);"),
            "the pane picker's escape must keep closing the tab directly"
        );
        assert!(
            !palette.contains("request_close_workspace_tab"),
            "an escape gesture must not raise a confirmation modal"
        );

        // `TerminalEvent::ChildExited` re-emits `PaneEvent::Remove`: the
        // process is already gone, so there is nothing to confirm and nothing
        // to undo.
        let handlers = include_str!("event_handlers.rs");
        let remove_arm = handlers
            .split("pane::PaneEvent::Remove => {")
            .nth(1)
            .and_then(|rest| rest.split("\n            }").next())
            .expect("PaneEvent::Remove arm");
        for forbidden in [
            "request_close_pane",
            "arm_pending_close_pane",
            "close_pane_undoably",
        ] {
            assert!(
                !remove_arm.contains(forbidden),
                "an auto-close after the child exited must never confirm and must never push an \
                 undo record (`{forbidden}` found): {remove_arm}"
            );
        }
    }

    fn tab_ids() -> Vec<(u64, Vec<u64>)> {
        vec![(10, vec![100, 101, 102]), (20, vec![200])]
    }

    #[test]
    fn pending_close_tab_indices_resolves_a_live_tab() {
        assert_eq!(pending_close_tab_indices(&tab_ids(), 10, 101), Some((0, 1)));
        assert_eq!(pending_close_tab_indices(&tab_ids(), 20, 200), Some((1, 0)));
    }

    #[test]
    fn pending_close_tab_indices_returns_none_once_the_tab_is_gone() {
        // The tab closed (or the whole workspace did) while the modal was up.
        let mut ids = tab_ids();
        ids[0].1.retain(|id| *id != 101);
        assert_eq!(pending_close_tab_indices(&ids, 10, 101), None);
        assert_eq!(pending_close_tab_indices(&tab_ids(), 99, 101), None);
        // A live tab id under the WRONG workspace must not resolve either.
        assert_eq!(pending_close_tab_indices(&tab_ids(), 20, 101), None);
    }

    #[test]
    fn pending_close_tab_indices_remaps_after_a_lower_indexed_tab_closes() {
        let mut ids = tab_ids();
        ids[0].1.retain(|id| *id != 100);
        assert_eq!(pending_close_tab_indices(&ids, 10, 102), Some((0, 1)));
        // ... and after a lower-indexed WORKSPACE closes.
        let ids = vec![(20, vec![200])];
        assert_eq!(pending_close_tab_indices(&ids, 20, 200), Some((0, 0)));
    }
}
