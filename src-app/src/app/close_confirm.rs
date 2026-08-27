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

/// Scrub and clamp a label before it lands in the modal copy.
///
/// The pane label can be an OSC title: a bidi override there could visually
/// reverse the `Close "…"?` around it and make the modal name a different
/// surface than the one about to die. Same scrub the port-conflict tooltip and
/// the fleet-search target list apply to the same source, and now literally
/// the same cap: [`crate::limits::MAX_UNTRUSTED_LABEL_CHARS`].
fn confirm_label(raw: &str) -> String {
    crate::markdown::strip_bidi_zero_width(
        raw.chars()
            .take(crate::limits::MAX_UNTRUSTED_LABEL_CHARS)
            .collect(),
    )
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

/// The pane an inline arm should light up, if this pending close is one.
///
/// A `Modal` pane close deliberately does not arm the button: the modal is
/// already asking, and a red `x` behind it would read as a second, different
/// question.
fn inline_armed_pane(pending: Option<&PendingClose>) -> Option<Entity<Pane>> {
    match pending {
        Some(PendingClose {
            target: CloseTarget::Pane { pane },
            style: ConfirmStyle::Inline,
            ..
        }) => Some(pane.clone()),
        _ => None,
    }
}

/// Move the armed flag from the outgoing pending close to the incoming one.
///
/// The correctness property this exists for: `pending_close` holds exactly ONE
/// target, so arming a second pane has to disarm the first. A pane left
/// visually armed after another target took the slot would close on a SINGLE
/// click with no confirmation - the exact kill issue #83 is about.
///
/// A free function rather than a method because `PaneFlowApp::new` binds a real
/// Unix socket and cannot be constructed in a test; this half is drivable with
/// nothing but two `Pane` entities.
fn sync_close_armed(current: Option<&PendingClose>, next: Option<&PendingClose>, cx: &mut App) {
    let outgoing = match current {
        Some(PendingClose {
            target: CloseTarget::Pane { pane },
            ..
        }) => Some(pane.clone()),
        _ => None,
    };
    let incoming = inline_armed_pane(next);
    // Re-stating the same pending close must not flicker the button off and
    // on, so the disarm skips a target that is staying.
    if let Some(pane) = outgoing.filter(|pane| Some(pane) != incoming.as_ref()) {
        pane.update(cx, |pane, cx| pane.set_close_armed(false, cx));
    }
    if let Some(pane) = incoming {
        pane.update(cx, |pane, cx| pane.set_close_armed(true, cx));
    }
}

impl PaneFlowApp {
    /// The single writer of `pending_close` (R9): every arm, disarm, and
    /// confirm goes through here, so the "disarm the OUTGOING target" body
    /// lives in exactly one place.
    /// `set_pending_close_is_the_only_writer_of_pending_close` pins that.
    pub(crate) fn set_pending_close(&mut self, next: Option<PendingClose>, cx: &mut Context<Self>) {
        sync_close_armed(self.pending_close.as_ref(), next.as_ref(), cx);
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
        self.close_workspace_tab(ws_idx, tab_idx, window, cx);
        // `close_workspace_tab`'s own re-focus is GATED on
        // `ws_idx == self.active_idx`, and a background workspace's expanded
        // tab row right-click reaches here with a non-active `ws_idx`. The
        // modal was holding focus, so without this the window would name an
        // unmounted handle (issue #108). Unconditional rather than gated on
        // the same test: it also covers the early `close_tab(..).is_none()`
        // return inside the close, and on the active branch it re-focuses the
        // pane the close just focused, which is a no-op.
        self.restore_focus_after_close_confirm(window, cx);
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
    ///
    /// Answers the id question directly instead of through
    /// [`Self::workspace_tab_ids`]: this is a per-frame call, and only the
    /// confirm path actually needs the resolved indices that helper builds a
    /// `Vec<(u64, Vec<u64>)>` to produce.
    pub(crate) fn pending_close_target_is_live(&self, pending: &PendingClose) -> bool {
        match &pending.target {
            CloseTarget::Tab {
                workspace_id,
                tab_id,
            } => self
                .workspaces
                .iter()
                .find(|ws| ws.id == *workspace_id)
                .is_some_and(|ws| ws.tabs().iter().any(|tab| tab.id == *tab_id)),
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
        // 11, not 9: the About dialog (`about_dialog.rs`) and the diff base-ref
        // popover (`diff/view/render.rs`) both defer at 10, and this modal is
        // deliberately not mode-gated - "About open, then Cmd+W" would
        // otherwise paint it UNDER an occluding backdrop while it silently
        // held focus, leaving a live Enter that kills a process group.
        deferred(backdrop).with_priority(11).into_any_element()
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
        // Pinned whole, not by absent substrings: `shortcut_for_action`
        // returns Apple HIG glyphs on macOS (`keybindings/display.rs` formats
        // `secondary-shift-t` as `⌘⇧T`), which contains neither "Cmd" nor
        // '+', so the negative form would have passed even if the real
        // shortcut had leaked into the `None` case.
        assert_eq!(
            body,
            "Claude Code is still running here. Closing stops it and everything it started. \
             Undo close brings it back with its scrollback, but does not resume the agent."
        );
        assert!(
            !body.contains('\u{2318}'),
            "with undo unassigned the copy must not print a keystroke at all: {body}"
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
        let cap = crate::limits::MAX_UNTRUSTED_LABEL_CHARS;
        let long: String = std::iter::repeat_n('x', cap + 40).collect();
        assert_eq!(confirm_label(&long).chars().count(), cap);
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
        // Bounded on `into_any_element()`, not on `));`: this item's chain
        // ends `))` with no semicolon, so a `));` delimiter is not found until
        // 37 lines into `render_pane_context_menu` and the assertions below
        // would pass over the wrong region.
        let tab_menu = menu_src
            .split("\"tab-context-close\".into()")
            .nth(1)
            .and_then(|rest| rest.split(".into_any_element()").next())
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

    /// The two INLINE paths (R4), and nothing else. Both have to route the
    /// click decision through the shared pure helper rather than re-deriving
    /// "is this the second click?" locally, and neither may keep a direct
    /// unguarded route into the close.
    #[test]
    fn both_inline_close_paths_route_through_the_click_helper() {
        let sidebar = include_str!("sidebar/mod.rs");
        let tab_x = sidebar
            .split("let close_armed = self.pending_close")
            .nth(1)
            .and_then(|rest| rest.split("let row_shell =").next())
            .expect("sidebar tab close button");
        assert!(
            tab_x.contains("click_outcome(") && tab_x.contains("ConfirmStyle::Inline"),
            "the sidebar tab x must arm inline through the shared helper: {tab_x}"
        );
        assert!(
            tab_x.contains("confirm_pending_close_tab"),
            "a second click on the same tab's x must confirm: {tab_x}"
        );
        assert!(
            !tab_x.contains("this.close_workspace_tab("),
            "the sidebar tab x must not keep a second, unguarded route into the close: {tab_x}"
        );
        // R3: arming has to be visible, and without an inline hex.
        assert!(
            tab_x.contains("ui.vc_deleted") && tab_x.contains("Click again to close"),
            "an armed x that looks and reads like an unarmed one is worse than no guard, because \
             the first click silently does nothing: {tab_x}"
        );

        let handlers = include_str!("event_handlers.rs");
        let close_requested = handlers
            .split("pane::PaneEvent::CloseRequested => {")
            .nth(1)
            .and_then(|rest| rest.split("\n            }").next())
            .expect("PaneEvent::CloseRequested arm");
        assert!(
            close_requested.contains("click_outcome(")
                && close_requested.contains("ConfirmStyle::Inline"),
            "the pane header x must arm inline through the shared helper: {close_requested}"
        );
        assert!(
            close_requested.contains("confirm_pending_close_pane"),
            "the second click arrives with no `&mut Window`, so it must use the Window-free \
             confirm: {close_requested}"
        );
        assert!(
            !close_requested.contains("close_pane_undoably"),
            "closing straight from the arm would bypass the guard entirely: {close_requested}"
        );
    }

    /// R6: Escape stands an inline arm down, through the one mutator, and
    /// without touching focus - `cancel_pending_close` would re-focus the
    /// first pane out from under whatever is typing, and a write that skipped
    /// the mutator would leave the pane lit.
    ///
    /// It also CONSUMES the key, but only there: the decision is
    /// [`crate::app::close_guard::escape_consumes_inline_arm`], whose own test
    /// pins both halves. Escape is the interrupt key for Claude Code and
    /// several other agents, so forwarding a disarm would interrupt the agent
    /// the user just chose to keep; with nothing armed the key passes through
    /// untouched and vim keeps it.
    #[test]
    fn escape_stands_an_inline_arm_down_and_consumes_only_that_key() {
        let src = include_str!("../main.rs");
        let capture = src
            .split(".capture_key_down(cx.listener(")
            .nth(1)
            .and_then(|rest| rest.split("\n            }))").next())
            .expect("root capture_key_down body");
        assert!(
            capture.contains("escape_consumes_inline_arm(")
                && capture.contains("set_pending_close(None"),
            "the root Escape capture must stand an inline arm down: {capture}"
        );
        assert!(
            !capture.contains("cancel_pending_close"),
            "an inline arm never took focus, so standing it down must not re-focus: {capture}"
        );
        // Two swallows, both inside a guarded branch: the drag branch (which
        // returns before the close-arm branch is reached) and the disarm.
        // Nothing at the top level, so an Escape with nothing armed is still
        // forwarded.
        assert_eq!(
            capture.matches("cx.stop_propagation()").count(),
            2,
            "only the drag branch and the inline disarm may swallow Escape: {capture}"
        );
        let disarm_at = capture
            .find("escape_consumes_inline_arm(")
            .expect("the close-arm branch");
        let last_stop_at = capture
            .rfind("cx.stop_propagation()")
            .expect("the disarm's stop_propagation");
        assert!(
            last_stop_at > disarm_at,
            "the second swallow must sit INSIDE the disarm branch, not above it: {capture}"
        );
    }

    /// The confirmation is deliberately NOT mode-gated, so it can be raised
    /// over any other overlay - "About dialog open, then `Cmd+W`". Painting it
    /// UNDER one does not stand it down: it still holds focus and `Enter`
    /// still kills a process group, so an occluded confirmation is an
    /// invisible modal with a live destructive default.
    #[test]
    fn the_close_confirmation_defers_above_every_overlay_it_can_share_a_frame_with() {
        // Built rather than written out, so the scan does not match itself.
        let needle = format!("with_priority{}", '(');
        let max_priority = |src: &str| -> u32 {
            src.match_indices(needle.as_str())
                .filter_map(|(idx, _)| {
                    src[idx + needle.len()..]
                        .split(')')
                        .next()
                        .and_then(|n| n.trim().parse::<u32>().ok())
                })
                .max()
                .unwrap_or(0)
        };
        let ours = max_priority(include_str!("close_confirm.rs"));
        assert!(
            ours > 0,
            "the confirmation must defer at an explicit priority"
        );
        // Every overlay that defers at 6 or above; the rest sit at 4 or lower.
        for (label, src) in [
            ("about_dialog.rs", include_str!("about_dialog.rs")),
            (
                "../diff/view/render.rs",
                include_str!("../diff/view/render.rs"),
            ),
            (
                "custom_buttons_modal.rs",
                include_str!("custom_buttons_modal.rs"),
            ),
            ("launch_pad.rs", include_str!("launch_pad.rs")),
            ("attention_queue.rs", include_str!("attention_queue.rs")),
            ("fleet_search.rs", include_str!("fleet_search.rs")),
            ("theme_picker.rs", include_str!("theme_picker.rs")),
            ("broadcast.rs", include_str!("broadcast.rs")),
        ] {
            let theirs = max_priority(src);
            assert!(
                ours > theirs,
                "{label} defers at {theirs} and the close confirmation at {ours}: an occluded \
                 confirmation still holds focus and still kills a process group on Enter"
            );
        }
    }

    /// Issue #108's stranding class, on the confirm path.
    ///
    /// `close_workspace_tab` re-focuses only when the closed tab belonged to
    /// the ACTIVE workspace, and a background workspace's expanded tab row
    /// right-click reaches this path with `ws_idx != self.active_idx`. The
    /// modal was the focused element, so dropping it there without a restore
    /// leaves the window naming an unmounted focus handle - exactly the state
    /// the four commits before this branch each fixed.
    #[test]
    fn confirming_a_tab_close_always_hands_focus_back() {
        let src = include_str!("close_confirm.rs");
        let body = src
            .split("pub(crate) fn confirm_pending_close_tab(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("confirm_pending_close_tab body");
        let closes_at = body
            .find("self.close_workspace_tab(")
            .expect("the confirm path's close call");
        let restores_at = body
            .rfind("self.restore_focus_after_close_confirm(")
            .expect("the confirm path must hand focus back");
        assert!(
            restores_at > closes_at,
            "the modal held focus, and the close it delegates to only re-focuses for the ACTIVE \
             workspace - so the restore has to run AFTER the close, on every path: {body}"
        );

        // The premise this rests on. If the gate below ever goes away, the
        // restore above becomes redundant rather than wrong - re-read both.
        let tab_src = include_str!("workspace_ops/tab.rs");
        let close_tab = tab_src
            .split("pub(crate) fn close_workspace_tab(")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("close_workspace_tab body");
        assert!(
            close_tab.contains("if ws_idx == self.active_idx"),
            "the close's own re-focus is gated on the active workspace: {close_tab}"
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

    fn inline_pane_pending(pane: &Entity<Pane>) -> PendingClose {
        PendingClose {
            target: CloseTarget::Pane { pane: pane.clone() },
            style: ConfirmStyle::Inline,
            agent: TerminalAgent::ClaudeCode,
            extra_agents: 0,
            label: String::new(),
        }
    }

    /// The single most important property of the inline affordance: arming a
    /// SECOND target has to disarm the first. `pending_close` holds exactly one
    /// target, so a pane left visually armed after another one takes over would
    /// close on ONE click with no confirmation - the exact kill this feature
    /// exists to prevent.
    ///
    /// `PaneFlowApp::new` binds a real Unix socket, so `set_pending_close`
    /// itself is unconstructable here; this drives the arm/disarm half it
    /// delegates to, in the same order, and
    /// `set_pending_close_is_the_only_writer_of_pending_close` pins the
    /// delegation.
    #[gpui::test]
    fn arming_a_second_pane_disarms_the_first(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        let cx = cx.add_empty_window();
        let new_pane = |cx: &mut gpui::VisualTestContext| {
            let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
            cx.new(|cx| Pane::new(terminal, 1, cx))
        };
        let a = new_pane(cx);
        let b = new_pane(cx);
        let armed = |cx: &mut gpui::VisualTestContext, pane: &Entity<Pane>| {
            cx.update(|_, cx| pane.read(cx).close_armed())
        };

        // Nothing pending: neither X is lit.
        assert!(!armed(cx, &a));
        assert!(!armed(cx, &b));

        // First click on A's X.
        let mut pending: Option<PendingClose> = None;
        let next = Some(inline_pane_pending(&a));
        cx.update(|_, cx| sync_close_armed(pending.as_ref(), next.as_ref(), cx));
        pending = next;
        assert!(armed(cx, &a), "A's X must light up once it is pending");
        assert!(!armed(cx, &b));

        // First click on B's X, while A is still armed.
        let next = Some(inline_pane_pending(&b));
        cx.update(|_, cx| sync_close_armed(pending.as_ref(), next.as_ref(), cx));
        pending = next;
        assert!(
            !armed(cx, &a),
            "A must be disarmed the instant B takes the single pending slot, or A's next single \
             click closes it with no confirmation"
        );
        assert!(armed(cx, &b));
        assert!(
            pending.as_ref().is_some_and(|p| p.targets_pane(&b)),
            "only B is pending"
        );
        assert!(!pending.as_ref().is_some_and(|p| p.targets_pane(&a)));

        // Esc / cancel / confirm all clear the slot the same way.
        cx.update(|_, cx| sync_close_armed(pending.as_ref(), None, cx));
        assert!(!armed(cx, &a));
        assert!(!armed(cx, &b));
    }

    /// Re-stating the SAME pending close must not flicker the armed pane off
    /// and on: a modal that re-arms its own target would otherwise repaint the
    /// button twice per gesture.
    #[gpui::test]
    fn re_arming_the_same_pane_leaves_it_armed(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        let a = cx.new(|cx| Pane::new(terminal, 1, cx));

        let pending = Some(inline_pane_pending(&a));
        cx.update(|_, cx| sync_close_armed(None, pending.as_ref(), cx));
        assert!(cx.update(|_, cx| a.read(cx).close_armed()));

        let again = Some(inline_pane_pending(&a));
        cx.update(|_, cx| sync_close_armed(pending.as_ref(), again.as_ref(), cx));
        assert!(cx.update(|_, cx| a.read(cx).close_armed()));
    }

    /// A MODAL pane close must not light the inline X: the modal is already
    /// asking, and a red X behind it would read as a second, different
    /// question.
    #[gpui::test]
    fn a_modal_pane_close_never_arms_the_inline_button(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        let a = cx.new(|cx| Pane::new(terminal, 1, cx));

        let mut pending = inline_pane_pending(&a);
        pending.style = ConfirmStyle::Modal;
        let pending = Some(pending);
        cx.update(|_, cx| sync_close_armed(None, pending.as_ref(), cx));
        assert!(!cx.update(|_, cx| a.read(cx).close_armed()));
    }

    /// R2: the pending-close slot has exactly one writer, so the disarm can
    /// never be bypassed. A direct assignment to the field anywhere else
    /// reintroduces the stuck-armed pane this whole helper exists to prevent,
    /// and it compiles silently.
    #[test]
    fn set_pending_close_is_the_only_writer_of_pending_close() {
        // Built rather than written out, so this scan does not match its own
        // source line.
        let eq = '=';
        let forms = [format!("pending_close {eq}"), format!("pending_close{eq}")];
        let self_src = include_str!("close_confirm.rs");
        // The one legitimate write lives inside `set_pending_close` itself.
        let (before, after) = self_src
            .split_once("pub(crate) fn set_pending_close(")
            .expect("set_pending_close");
        let (setter, rest) = after.split_once("\n    }").expect("set_pending_close body");
        assert!(
            setter.contains("sync_close_armed("),
            "set_pending_close must push the armed flag onto the panes: {setter}"
        );

        for (label, src) in [
            ("close_confirm.rs (outside the setter)", before),
            ("close_confirm.rs (outside the setter)", rest),
            ("close_guard.rs", include_str!("close_guard.rs")),
            ("../main.rs", include_str!("../main.rs")),
            ("bootstrap.rs", include_str!("bootstrap.rs")),
            ("workspace_ops/mod.rs", include_str!("workspace_ops/mod.rs")),
            ("workspace_ops/tab.rs", include_str!("workspace_ops/tab.rs")),
            ("event_handlers.rs", include_str!("event_handlers.rs")),
            ("sidebar/mod.rs", include_str!("sidebar/mod.rs")),
            (
                "sidebar/context_menu.rs",
                include_str!("sidebar/context_menu.rs"),
            ),
            ("../pane.rs", include_str!("../pane.rs")),
        ] {
            for line in src.lines() {
                let line = line.trim();
                let writes = forms.iter().any(|form| {
                    line.find(form.as_str())
                        // `==` is a comparison, not a write.
                        .is_some_and(|idx| !line[idx + form.len()..].starts_with(eq))
                });
                assert!(
                    !writes,
                    "{label}: every write to the pending-close slot must go through \
                     `set_pending_close`, which disarms the outgoing pane: {line}"
                );
            }
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
