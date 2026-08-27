//! Issue #83: the confirmation that stands between a user close gesture and
//! `kill(-pid, SIGTERM)` on a live coding agent's whole process group.
//!
//! [`crate::app::close_guard`] answers *whether* to ask; this module owns the
//! asking: the request/confirm/cancel entry points every modal close path
//! funnels through, and the modal itself.
//!
//! Split out of `close_guard.rs` so the predicate stays free of GPUI: the
//! guard is drivable with plain data, this half needs a window.

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
use crate::app::workspace_ops::capture_closed_pane_record;
use crate::pane::Pane;
use crate::ui_primitives::AnimatedHoverExt;
use crate::{ClosedRecord, PaneFlowApp};

/// Result returned to the window-less IPC path. A guarded request is not a
/// close: it arms the same in-app modal the UI uses and reports that fact to
/// the caller instead of pretending the workspace is already gone.
pub(crate) enum WorkspaceCloseOutcome {
    Closed,
    ConfirmationRequired,
    NotFound,
}

/// Scrub and clamp a label before it lands in the modal copy.
///
/// The pane label can be an OSC title: a bidi override there could visually
/// reverse the `Close "…"?` around it and make the modal name a different
/// surface than the one about to die. Same scrub the port-conflict tooltip and
/// the fleet-search target list apply to the same source, and literally the
/// same cap: [`crate::limits::MAX_UNTRUSTED_LABEL_CHARS`].
///
/// Two characters beyond that scrub, because this sink QUOTES the label rather
/// than merely printing it. [`close_confirm_title`] wraps it in curly double
/// quotes, which `strip_bidi_zero_width` leaves alone, so a label carrying one
/// closes the quoted region early and lets the rest of the payload read as the
/// sentence's own words. Control characters go for the same reason, and
/// because `sanitize_pane_name` (`app/ipc_handler.rs`) - upstream of the
/// `surface.rename` route that reaches here, and one that is NOT
/// scripting-gated - already strips them: a sink laxer than its own source is
/// the wrong way round.
fn confirm_label(raw: &str) -> String {
    crate::limits::clamp_untrusted_label(raw)
        .chars()
        .filter(|c| !matches!(c, '\u{201c}' | '\u{201d}') && !c.is_control())
        .collect()
}

/// Title line for the close confirmation.
pub(crate) fn close_confirm_title(noun: &str, label: &str) -> String {
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
        }) => pane.upgrade(),
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
        }) => pane.upgrade(),
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

    fn workspace_close_states(&self, ws_idx: usize, cx: &App) -> Vec<SurfaceCloseState> {
        self.workspaces
            .get(ws_idx)
            .map(|ws| Self::surface_close_states(&ws.collect_panes(), cx))
            .unwrap_or_default()
    }

    /// `(workspace id, that workspace's tab ids in order)`, the input
    /// [`pending_close_tab_indices`] resolves against.
    fn workspace_tab_ids(&self) -> Vec<(u64, Vec<u64>)> {
        self.workspaces
            .iter()
            .map(|ws| (ws.id, ws.tabs().iter().map(|tab| tab.id).collect()))
            .collect()
    }

    /// Arm a workspace close when any terminal in any tab holds a live agent.
    /// `None` means the index was stale; `Some(false)` means no confirmation
    /// was needed; `Some(true)` means the modal is now pending.
    fn arm_pending_close_workspace(
        &mut self,
        ws_idx: usize,
        style: ConfirmStyle,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let workspace = self.workspaces.get(ws_idx)?;
        let workspace_id = workspace.id;
        let label = confirm_label(&workspace.title);
        let states = self.workspace_close_states(ws_idx, cx);
        let now = std::time::Instant::now();
        let Some(agent) = agent_needing_confirmation(&states, now) else {
            return Some(false);
        };
        self.set_pending_close(
            Some(PendingClose {
                target: CloseTarget::Workspace { workspace_id },
                style,
                agent,
                extra_agents: agents_needing_confirmation_count(&states, now).saturating_sub(1),
                label,
                armed_at: now,
            }),
            cx,
        );
        Some(true)
    }

    /// The single entry point for UI workspace-close gestures.
    pub(crate) fn request_close_workspace(
        &mut self,
        ws_idx: usize,
        style: ConfirmStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.arm_pending_close_workspace(ws_idx, style, cx) {
            Some(true) | None => {}
            Some(false) => {
                self.close_workspace_at_inner(ws_idx, Some(window), cx);
            }
        }
    }

    /// Window-less sibling for `workspace.close`. It may arm the modal, but
    /// only the app's visible confirmation can complete a guarded close.
    pub(crate) fn request_close_workspace_without_window(
        &mut self,
        ws_idx: usize,
        style: ConfirmStyle,
        cx: &mut Context<Self>,
    ) -> WorkspaceCloseOutcome {
        match self.arm_pending_close_workspace(ws_idx, style, cx) {
            None => WorkspaceCloseOutcome::NotFound,
            Some(true) => WorkspaceCloseOutcome::ConfirmationRequired,
            Some(false) => {
                if self.close_workspace_at_inner(ws_idx, None, cx) {
                    WorkspaceCloseOutcome::Closed
                } else {
                    WorkspaceCloseOutcome::NotFound
                }
            }
        }
    }

    /// The one entry point every user-initiated tab close GESTURE goes
    /// through. `pane_palette.rs` closes a tab directly and on purpose - a
    /// picker escape is a dismissal, not a close request.
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
                armed_at: now,
            }),
            cx,
        );
    }

    /// Arm a confirmation for `pane` when closing it would kill a live agent.
    /// `true` means one is now pending and the caller must NOT close.
    ///
    /// Deliberately `Window`-free: the inline half is reached from
    /// `handle_pane_event`, which has no `&mut Window` - all five pane
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
                target: CloseTarget::Pane {
                    pane: pane.downgrade(),
                },
                style,
                agent,
                extra_agents: agents_needing_confirmation_count(&states, now).saturating_sub(1),
                label: confirm_label(&label),
                armed_at: now,
            }),
            cx,
        );
        true
    }

    /// Confirm-or-close for a pane close that has no window-dependent close of
    /// its own (the sidebar pane context menu; the pane header `x`).
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
                self.push_closed_record(ClosedRecord::Pane(record), cx);
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
            style,
            ..
        }) = self.pending_close.clone()
        else {
            return;
        };
        // Only the MODAL took focus, so only the modal hands it back. This
        // method has two callers, and the other is the sidebar's inline x,
        // where nothing was focused: re-focusing there would move focus to the
        // ACTIVE workspace's first pane, out from under whatever the user is
        // typing. Same rule as the root Escape handler (`set_pending_close`,
        // not `cancel_pending_close`), and the one the PANE half of this guard
        // already follows.
        let restores_focus = style == ConfirmStyle::Modal;
        self.set_pending_close(None, cx);
        let Some((ws_idx, tab_idx)) =
            pending_close_tab_indices(&self.workspace_tab_ids(), workspace_id, tab_id)
        else {
            // The target went away underneath the confirmation. Close nothing.
            if restores_focus {
                self.restore_focus_after_close_confirm(window, cx);
            }
            return;
        };
        self.close_workspace_tab(ws_idx, tab_idx, window, cx);
        // `close_workspace_tab`'s own re-focus is GATED on
        // `ws_idx == self.active_idx`, and a background workspace's expanded
        // tab row right-click reaches here with a non-active `ws_idx`. The
        // modal was holding focus, so without this the window would name an
        // unmounted handle (issue #108). Unconditional WITHIN the modal branch
        // rather than gated on the same test: it also covers the early
        // `close_tab(..).is_none()` return inside the close, and on the active
        // branch it re-focuses the pane the close just focused, a no-op.
        if restores_focus {
            self.restore_focus_after_close_confirm(window, cx);
        }
    }

    /// Confirm a pending workspace close after re-resolving its stable id.
    pub(crate) fn confirm_pending_close_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(PendingClose {
            target: CloseTarget::Workspace { workspace_id },
            ..
        }) = self.pending_close.clone()
        else {
            return;
        };
        self.set_pending_close(None, cx);
        let Some(ws_idx) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            self.restore_focus_after_close_confirm(window, cx);
            return;
        };
        if !self.close_workspace_at_inner(ws_idx, Some(window), cx) {
            self.restore_focus_after_close_confirm(window, cx);
        }
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
            Some(CloseTarget::Workspace { .. }) => self.confirm_pending_close_workspace(window, cx),
            Some(CloseTarget::Tab { .. }) => self.confirm_pending_close_tab(window, cx),
            Some(CloseTarget::Pane { pane }) => {
                match pane.upgrade() {
                    Some(pane) => self.confirm_pending_close_pane(pane, cx),
                    // The pane was dropped underneath the modal: nothing left
                    // to close, but the slot still has to be cleared through
                    // the single writer so no button is left lit.
                    None => self.set_pending_close(None, cx),
                }
                // The `Window`-free half cannot hand focus back, and this
                // caller has a `Window`, so do it here: the modal was the
                // focused element and dropping it silently is the issue #108
                // stranding class.
                //
                // Unconditional, unlike the tab half a few lines up in
                // `confirm_pending_close_tab`, which only restores focus when
                // `style == ConfirmStyle::Modal`. This arm gets away with
                // skipping that check only because `confirm_pending_close`
                // itself is wired exclusively from `render_close_confirm_dialog`'s
                // own key handler and buttons, and that dialog's one mount
                // site is gated on `is_modal` in `main.rs` - so every call
                // that reaches here already IS a modal confirm. The
                // invariant therefore holds by CALL-SITE STRUCTURE, not by
                // data read off `self.pending_close.style`. A future caller
                // of `confirm_pending_close_pane` reached from a non-modal
                // path (an inline arm-then-confirm, say) would restore focus
                // after a click nothing focused, yanking it out from under
                // whatever the user was typing - the exact class `style ==
                // ConfirmStyle::Modal` guards against on the tab side.
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
        // Unconditional for the same reason as the pane arm of
        // `confirm_pending_close` above: this is reached only through the
        // modal's own Escape handler and backdrop click, both wired inside
        // `render_close_confirm_dialog`, whose sole mount site is gated on
        // `is_modal` in `main.rs` - never from the inline arm-then-confirm
        // path, which stands down through `set_pending_close` directly
        // instead. That is a call-site guarantee, not one read from
        // `self.pending_close.style`; a future non-modal caller of
        // `cancel_pending_close` would restore focus nothing gave away,
        // stranding it the same way issue #108 did.
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

    /// True when this pending close's target lives in the workspace at
    /// `ws_idx` - so destroying that workspace also destroys the thing the
    /// confirmation is asking about.
    ///
    /// Separate from [`Self::pending_close_target_is_live`], which asks the
    /// same question of the whole app AFTER the fact. This one has to be
    /// answered BEFORE `workspaces.remove`, while the doomed workspace is
    /// still there to be looked in.
    pub(crate) fn pending_close_targets_workspace(
        &self,
        pending: &PendingClose,
        ws_idx: usize,
    ) -> bool {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return false;
        };
        match &pending.target {
            CloseTarget::Workspace { workspace_id } => ws.id == *workspace_id,
            CloseTarget::Tab { workspace_id, .. } => ws.id == *workspace_id,
            CloseTarget::Pane { pane } => pane
                .upgrade()
                .is_some_and(|pane| ws.tab_index_containing_pane(&pane).is_some()),
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
            CloseTarget::Workspace { workspace_id } => self
                .workspaces
                .iter()
                .any(|workspace| workspace.id == *workspace_id),
            CloseTarget::Tab {
                workspace_id,
                tab_id,
            } => self
                .workspaces
                .iter()
                .find(|ws| ws.id == *workspace_id)
                .is_some_and(|ws| ws.tabs().iter().any(|tab| tab.id == *tab_id)),
            CloseTarget::Pane { pane } => pane.upgrade().is_some_and(|pane| {
                self.workspaces
                    .iter()
                    .any(|ws| ws.tab_index_containing_pane(&pane).is_some())
            }),
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
        let noun = match pending.target {
            CloseTarget::Workspace { .. } => "workspace",
            CloseTarget::Tab { .. } => "tab",
            CloseTarget::Pane { .. } => "pane",
        };
        let title = close_confirm_title(noun, &pending.label);
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

    /// Every `.rs` file under `src-app/src`, read off disk at test time.
    ///
    /// The scans below used to enumerate files by hand, which is a guard that
    /// goes stale in silence: the deferral scan was blind to 13 deferral
    /// sites, and the single-writer scan to every file nobody remembered to
    /// add. `env!` resolves at compile time, so the walk does not depend on
    /// the working directory the suite is run from.
    fn rust_sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            let mut paths: Vec<std::path::PathBuf> =
                entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && let Ok(src) = std::fs::read_to_string(&path)
                {
                    out.push((path.display().to_string(), src));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        walk(&root, &mut out);
        assert!(
            out.len() > 100,
            "the source walk found {} files - a walk that reads nothing passes every scan \
             built on it",
            out.len()
        );
        out
    }

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
            close_confirm_title("tab", "Fix the bug"),
            "Close tab \u{201c}Fix the bug\u{201d}?"
        );
        assert_eq!(
            close_confirm_title("pane", "claude"),
            "Close pane \u{201c}claude\u{201d}?"
        );
        assert_eq!(
            close_confirm_title("workspace", "Paneflow"),
            "Close workspace \u{201c}Paneflow\u{201d}?"
        );
    }

    #[test]
    fn close_confirm_title_falls_back_when_the_label_is_blank() {
        assert_eq!(close_confirm_title("tab", ""), "Close this tab?");
        assert_eq!(close_confirm_title("pane", "   "), "Close this pane?");
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

    /// The label is QUOTED, not merely printed, so a curly double quote inside
    /// it terminates the quoted region early and the rest reads as the
    /// sentence's own words. `strip_bidi_zero_width` does not touch those, and
    /// the OSC channel is not the only way in: IPC `surface.rename` sets
    /// `custom_name` and is not scripting-gated.
    #[test]
    fn confirm_label_strips_the_quotes_the_title_wraps_it_in() {
        let spoof = format!(
            "sh{}\u{201d} - idle, safe {}\u{201c}",
            '\u{201d}', '\u{201c}'
        );
        let cleaned = confirm_label(&spoof);
        for quote in ['\u{201c}', '\u{201d}'] {
            assert!(
                !cleaned.contains(quote),
                "a curly quote in the label re-opens the title's own quoting: {cleaned}"
            );
        }
        let title = close_confirm_title("pane", &cleaned);
        assert_eq!(
            title.matches('\u{201c}').count(),
            1,
            "exactly one opening quote, the title's own: {title}"
        );
        assert_eq!(
            title.matches('\u{201d}').count(),
            1,
            "exactly one closing quote, the title's own: {title}"
        );

        // Control characters too - `sanitize_pane_name`, upstream of the
        // `surface.rename` route into this sink, already strips them, and a
        // sink laxer than its own source is the wrong way round.
        let noisy = confirm_label("claude\u{7}\n\u{1b}[31m");
        assert!(
            !noisy.chars().any(char::is_control),
            "control characters must not reach the modal copy: {noisy:?}"
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
        // Bounded on `into_any_element()`, not on `));`: this item's chain
        // ends `))` with no semicolon, so the next `));` is 37 lines further
        // on - by which point the region has run 17 lines into
        // `render_pane_context_menu`, and the assertions below would be
        // reading the wrong menu item.
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

    /// Issue #111: a workspace close has the largest blast radius of any
    /// close gesture, so all four entry points must share the same guard and
    /// the destructive closer must capture one undo record before removal.
    #[test]
    fn every_workspace_close_path_requires_confirmation_and_captures_undo() {
        let ops = include_str!("workspace_ops/mod.rs");
        let shortcut = ops
            .split("pub(crate) fn handle_close_workspace(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) fn handle_copy_workspace_path(")
                    .next()
            })
            .expect("CloseWorkspace action handler");
        assert!(
            shortcut.contains("request_close_workspace")
                && shortcut.contains("ConfirmStyle::Modal"),
            "Cmd+Shift+Q must ask before closing a workspace: {shortcut}"
        );
        assert!(
            !shortcut.contains("close_workspace_at("),
            "the shortcut must not retain an unguarded route: {shortcut}"
        );
        let bootstrap = include_str!("bootstrap.rs");
        let menu_fallback = bootstrap
            .split("cx.on_action(|_: &CloseWorkspace")
            .nth(1)
            .and_then(|rest| rest.split("cx.on_action(|_: &NextWorkspace").next())
            .expect("macOS CloseWorkspace action fallback");
        assert!(
            menu_fallback.contains("request_close_workspace")
                && menu_fallback.contains("ConfirmStyle::Modal"),
            "the macOS menu fallback for the same action must ask too: {menu_fallback}"
        );
        assert!(!menu_fallback.contains("close_workspace_at("));

        let sidebar = include_str!("sidebar/mod.rs");
        let row_x = sidebar
            .split("ws-close-{ws_id}")
            .nth(1)
            .and_then(|rest| rest.split("let row =").next())
            .expect("workspace row close button");
        assert!(
            row_x.contains("request_close_workspace") && row_x.contains("ConfirmStyle::Modal"),
            "the workspace row x must ask before closing: {row_x}"
        );
        assert!(!row_x.contains("close_workspace_at("));

        let menu = include_str!("sidebar/context_menu.rs");
        let menu_close = menu
            .split("workspace-context-close")
            .nth(1)
            .and_then(|rest| rest.split("into_any_element()").next())
            .expect("workspace context menu close item");
        assert!(
            menu_close.contains("request_close_workspace")
                && menu_close.contains("ConfirmStyle::Modal"),
            "the workspace context menu must ask before closing: {menu_close}"
        );
        assert!(!menu_close.contains("close_workspace_at("));

        let ipc = include_str!("ipc_handler.rs");
        let ipc_close = ipc
            .split("\"workspace.close\"")
            .nth(1)
            .and_then(|rest| rest.split("\"surface.list\"").next())
            .expect("workspace.close IPC arm");
        assert!(
            ipc_close.contains("request_close_workspace_without_window"),
            "IPC must ask in-app rather than closing agents remotely: {ipc_close}"
        );
        assert!(!ipc_close.contains("close_workspace_at_without_window"));

        let closer = ops
            .split("fn close_workspace_at_inner(")
            .nth(1)
            .and_then(|rest| rest.split("\n    /// Move a workspace").next())
            .expect("workspace closer");
        let capture_at = closer
            .find("capture_closed_workspace_record(")
            .expect("workspace close must capture an undo record");
        let remove_at = closer
            .find("self.workspaces.remove(idx)")
            .expect("workspace close removes the workspace");
        assert!(
            capture_at < remove_at && closer.contains("ClosedRecord::Workspace"),
            "the workspace undo record must be pushed before its panes are dropped: {closer}"
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

    /// The belt to [`crate::app::close_guard::ARM_SETTLE`]'s braces.
    ///
    /// GPUI fires an `on_click` listener for BOTH clicks of a double-click -
    /// the sidebar row's own double-click-to-rename works only because it
    /// does - so a double-click on either inline X used to arm and then
    /// immediately confirm, killing an agent's whole process group behind a
    /// confirmation painted for one frame nobody can perceive. Neither button
    /// may act on a click that arrives with `click_count >= 2`.
    ///
    /// Scanned rather than driven: both listeners are GPUI closures with no
    /// test seam, and the pane header's X reaches the guard indirectly (it
    /// emits `PaneEvent::CloseRequested`, which is where the click count is
    /// already gone - `pane.rs` is the only place that still has it).
    #[test]
    fn neither_inline_close_button_acts_on_the_second_click_of_a_double_click() {
        let sidebar = include_str!("sidebar/mod.rs");
        let tab_x = sidebar
            .split("let close_armed = self.pending_close")
            .nth(1)
            .and_then(|rest| rest.split("let row_shell =").next())
            .expect("sidebar tab close button");
        let pane_src = include_str!("../pane.rs");
        let header_x = pane_src
            .split("fn render_close_button(")
            .nth(1)
            .and_then(|rest| rest.split("\n    /// Close this pane").next())
            .expect("render_close_button body");

        for (label, body) in [
            ("the sidebar tab x", tab_x),
            ("the pane header x", header_x),
        ] {
            assert!(
                body.contains("click_count >= 2"),
                "{label} must drop the second click of a double-click, or it arms and \
                 confirms in one gesture: {body}"
            );
        }
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
        // The bare `priority` stem, not `with_priority`: GPUI exposes BOTH
        // spellings and `diff/view/review.rs` uses `deferred(menu).priority(8)`,
        // which the narrower needle could not see at all.
        let needle = format!("priority{}", '(');
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
        // Every source file, not a hand-kept list of the overlays someone
        // remembered: a new overlay deferring above this one is exactly the
        // regression, and it would arrive in a file no list mentions.
        for (path, src) in rust_sources() {
            if path.ends_with("close_confirm.rs") {
                continue;
            }
            let theirs = max_priority(&src);
            assert!(
                ours > theirs,
                "{path} defers at {theirs} and the close confirmation at {ours}: an occluded \
                 confirmation still holds focus and still kills a process group on Enter"
            );
        }
    }

    /// Issue #108's stranding class, on the confirm path - and its opposite
    /// on the inline one.
    ///
    /// `close_workspace_tab` re-focuses only when the closed tab belonged to
    /// the ACTIVE workspace, and a background workspace's expanded tab row
    /// right-click reaches this path with `ws_idx != self.active_idx`. The
    /// MODAL was the focused element, so dropping it there without a restore
    /// leaves the window naming an unmounted focus handle - exactly the state
    /// the four commits before this branch each fixed.
    ///
    /// The sidebar's inline x calls the very same method, and there nothing
    /// took focus: restoring would move focus to the ACTIVE workspace's first
    /// pane, out from under whatever the user is typing. So both restores are
    /// gated on the style, and neither may be reachable without the gate.
    #[test]
    fn confirming_a_modal_tab_close_hands_focus_back_and_an_inline_one_does_not() {
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
        assert!(
            body.contains("style == ConfirmStyle::Modal"),
            "the restore has to be decided from the pending close's own style, not from the \
             call site: {body}"
        );
        assert_eq!(
            body.matches("self.restore_focus_after_close_confirm(")
                .count(),
            body.matches("if restores_focus {").count(),
            "every restore on this path must sit behind the modal-only gate, or the sidebar's \
             inline x yanks the caret out of whatever is typing: {body}"
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
        // The arm itself is eight lines - this branch moved its body into
        // `remove_pane_from_tree` - so scanning only the arm would sail past a
        // `push_closed_record` added one call deeper. Follow the call.
        let removal = handlers
            .split("pub(crate) fn remove_pane_from_tree(")
            .nth(1)
            .and_then(|rest| rest.split("\n    pub(crate) fn handle_pane_event(").next())
            .expect("remove_pane_from_tree body");
        for (label, body) in [
            ("the PaneEvent::Remove arm", remove_arm),
            ("remove_pane_from_tree, which it delegates to", removal),
        ] {
            for forbidden in [
                "request_close_pane",
                "arm_pending_close_pane",
                "close_pane_undoably",
                "push_closed_record",
                "capture_closed_pane_record",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "an auto-close after the child exited must never confirm and must never \
                     push an undo record (`{forbidden}` found in {label}): {body}"
                );
            }
        }
    }

    fn inline_pane_pending(pane: &Entity<Pane>) -> PendingClose {
        PendingClose {
            target: CloseTarget::Pane {
                pane: pane.downgrade(),
            },
            style: ConfirmStyle::Inline,
            agent: TerminalAgent::ClaudeCode,
            extra_agents: 0,
            label: String::new(),
            armed_at: std::time::Instant::now(),
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

    /// A pending close must not be the thing keeping a pane - and therefore
    /// its PTY and its unreaped child - alive.
    ///
    /// Every sibling overlay that parks a pane target holds it weakly for the
    /// same reason (`composer.rs`, `launch_pad.rs`, `pane_palette.rs`). The
    /// render stand-down that clears a dead target is render-GATED, so an IPC
    /// `workspace.close` against a minimised window can leave the frame that
    /// would run it arbitrarily far away.
    #[gpui::test]
    fn a_pending_close_does_not_keep_its_target_pane_alive(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        let pane = cx.new(|cx| Pane::new(terminal, 1, cx));
        let pending = inline_pane_pending(&pane);
        let CloseTarget::Pane { pane: target } = &pending.target else {
            panic!("a pane pending close");
        };
        assert!(
            target.upgrade().is_some(),
            "the target resolves while the pane is alive"
        );

        drop(pane);

        assert!(
            target.upgrade().is_none(),
            "the pane died with its last real owner; a pending close that outlived it would \
             hold the PTY open with nothing on screen to close"
        );
    }

    /// Re-stating the SAME pending close must not flicker the armed pane off
    /// and on: a re-arm of the target already in the slot would otherwise
    /// repaint the button twice per gesture.
    ///
    /// The final boolean cannot show that - it is `true` either way, which is
    /// why this counts REPAINTS instead. Dropping `sync_close_armed`'s "skip a
    /// target that is staying" filter turns one silent re-arm into
    /// `set_close_armed` false-then-true; `set_close_armed` notifies only on a
    /// real change, so the flicker shows up as a repaint where there should be
    /// none at all (GPUI coalesces the pair within one update).
    #[gpui::test]
    fn re_arming_the_same_pane_leaves_it_armed(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        let a = cx.new(|cx| Pane::new(terminal, 1, cx));

        let pending = Some(inline_pane_pending(&a));
        cx.update(|_, cx| sync_close_armed(None, pending.as_ref(), cx));
        assert!(cx.update(|_, cx| a.read(cx).close_armed()));

        let repaints = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let subscription = cx.update(|_, cx| {
            let repaints = repaints.clone();
            cx.observe(&a, move |_, _| repaints.set(repaints.get() + 1))
        });

        let again = Some(inline_pane_pending(&a));
        cx.update(|_, cx| sync_close_armed(pending.as_ref(), again.as_ref(), cx));
        cx.update(|_, _| {});

        assert!(cx.update(|_, cx| a.read(cx).close_armed()));
        assert_eq!(
            repaints.get(),
            0,
            "re-arming the target already in the slot must not disarm it first: the button \
             would blink off and on inside one gesture"
        );
        drop(subscription);
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
        // source lines. Seven forms, not one: a plain assignment is the
        // obvious way to bypass the setter, `.take()`, `.replace(..)` and -
        // the realistic one - an `.as_mut()` followed by a field write all
        // leave a pane lit with nothing pending, and taking a mutable borrow
        // of the field directly (through either receiver name it is ever
        // read through in this file) hands out the same bypass without even
        // needing a named method - a single-click kill either way.
        let eq = '=';
        let dot = '.';
        let forms = [
            format!("pending_close {eq}"),
            format!("pending_close{eq}"),
            format!("pending_close{dot}take()"),
            format!("pending_close{dot}replace("),
            format!("pending_close{dot}as_mut()"),
            format!("mut self{dot}pending_close"),
            format!("mut this{dot}pending_close"),
        ];
        // The one legitimate write lives inside `set_pending_close` itself.
        let self_src = include_str!("close_confirm.rs");
        let (before, after) = self_src
            .split_once("pub(crate) fn set_pending_close(")
            .expect("set_pending_close");
        let (setter, rest) = after.split_once("\n    }").expect("set_pending_close body");
        assert!(
            setter.contains("sync_close_armed("),
            "set_pending_close must push the armed flag onto the panes: {setter}"
        );

        // The whole tree, not a hand-kept list: the slot is a crate-wide
        // field, so the file that bypasses the setter next is by definition
        // one nobody thought to enumerate.
        let mut sources: Vec<(String, String)> = rust_sources()
            .into_iter()
            .filter(|(path, _)| !path.ends_with("close_confirm.rs"))
            .collect();
        sources.push((
            "close_confirm.rs (outside the setter)".to_string(),
            format!("{before}{rest}"),
        ));

        for (label, src) in sources {
            for line in src.lines() {
                let line = line.trim();
                let writes = forms.iter().any(|form| {
                    line.find(form.as_str()).is_some_and(|idx| {
                        let after = &line[idx + form.len()..];
                        if form.starts_with("mut ") {
                            // A borrow form has no fixed suffix, so the
                            // boundary has to be checked instead: without it
                            // this form would also match inside the sibling
                            // field `pending_close_focus_claim`, which merely
                            // shares the same prefix and is not the slot.
                            !after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                        } else {
                            // `==` is a comparison, not a write.
                            !after.starts_with(eq)
                        }
                    })
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
