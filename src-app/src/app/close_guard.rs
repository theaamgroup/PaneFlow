//! Pure predicate for whether closing a surface, tab, or pane would kill a
//! live coding-agent process without asking first, plus the state a close
//! confirmation UI hangs off.
//!
//! This module is intentionally standalone: no GPUI rendering, no IPC, no
//! process signaling. Everything here is a plain data → plain data function
//! so it can be unit-tested without a window, a PTY, or a real agent CLI.
//! The wiring lives next door in [`crate::app::close_confirm`]: the modal, the
//! request/confirm/cancel entry points, and the inline arm-then-confirm
//! buttons.

use std::time::{Duration, Instant};

use crate::agent_launcher::TerminalAgent;

/// Everything the guard needs from one terminal surface, copied out on the
/// GPUI thread so the predicate itself stays pure and testable.
/// See `PaneFlowApp::surface_close_states` for the read side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SurfaceCloseState {
    pub(crate) detected_agent: Option<TerminalAgent>,
    /// A latch meaning "a process scan resolved this surface at least once",
    /// NOT "an agent is present". The scan deposit
    /// (`app/event_handlers.rs`, `apply_pane_scan`) writes `detected_agent`
    /// and `agent_confirmed` in the same statement pair, so a `true` here
    /// always describes the `detected_agent` sitting beside it - which is the
    /// only reason the predicate below can trust the two together.
    pub(crate) agent_confirmed: bool,
    pub(crate) agent_declared_until: Option<Instant>,
    /// Whether the surface's own child process is still live *and* still the
    /// process it was pinned to at spawn time.
    ///
    /// Deliberately not `!exited`: `TerminalState::exited` is the **shell's**
    /// exit status (see `crate::ai_types`), never the agent's, and it stays
    /// `None` for a dead-but-unreaped child, a recycled pid, or a libproc
    /// `EPERM`. Callers compute this from
    /// `crate::app::event_handlers::terminal_identity_is_scannable`, the same
    /// admission test the scan deposit applies, so a surface the scan refuses
    /// to touch cannot report a live agent forever.
    ///
    /// That reuse is a coupling, not just a convenience: this guard's safety
    /// now moves with the scan's admission test, so anyone loosening
    /// `terminal_identity_is_scannable` for scan reasons is also deciding
    /// which closes stop to ask.
    pub(crate) child_is_live: bool,
}

/// True when closing this surface would kill a live agent's process group.
///
/// Implements design decision D5:
/// `detected_agent.is_some() && child_is_live && (agent_confirmed ||
/// declaration_still_live)`, where `declaration_still_live` is
/// `agent_declared_until.is_some_and(|until| now < until)`.
///
/// This intentionally does not reuse
/// [`crate::app::event_handlers::declaration_survives_scan`]: that helper
/// answers a different question ("should a scan deposit leave a
/// launch-declared identity alone", which requires `scanned.is_none()` as a
/// precondition). Here `agent_confirmed` already carries the "a scan has
/// backed this up" signal, so the only piece worth sharing is the deadline
/// comparison itself, inlined below.
pub(crate) fn surface_close_needs_confirmation(state: &SurfaceCloseState, now: Instant) -> bool {
    let declaration_still_live = state.agent_declared_until.is_some_and(|until| now < until);
    state.detected_agent.is_some()
        && state.child_is_live
        && (state.agent_confirmed || declaration_still_live)
}

/// The agent to name in the confirmation copy: the first surface in
/// traversal order that needs one. `None` means close instantly.
pub(crate) fn agent_needing_confirmation(
    states: &[SurfaceCloseState],
    now: Instant,
) -> Option<TerminalAgent> {
    states
        .iter()
        .find(|state| surface_close_needs_confirmation(state, now))
        .and_then(|state| state.detected_agent)
}

/// How many surfaces in `states` would each lose a live agent to this close.
///
/// [`agent_needing_confirmation`] names only the FIRST one, so a tab holding
/// three agents would say "Claude Code" and kill three. The confirmation copy
/// uses this count to say so.
pub(crate) fn agents_needing_confirmation_count(
    states: &[SurfaceCloseState],
    now: Instant,
) -> usize {
    states
        .iter()
        .filter(|state| surface_close_needs_confirmation(state, now))
        .count()
}

/// What a pending close confirmation is about to close.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CloseTarget {
    /// Resolved by the stable workspace id, never by sidebar position.
    Workspace { workspace_id: u64 },
    /// Resolved by stable ids, never by index - rows reorder under a drag.
    Tab { workspace_id: u64, tab_id: u64 },
    Pane {
        /// WEAK, like every sibling overlay that parks a pane target
        /// (`composer.rs`, `launch_pad.rs`, `pane_palette.rs`): a pending
        /// close must not be the thing keeping a pane - and therefore its PTY
        /// and its unreaped child - alive. The render stand-down clears a dead
        /// target, but it is render-gated, and an IPC `workspace.close`
        /// against a minimised window may produce no frame for a long time.
        pane: gpui::WeakEntity<crate::pane::Pane>,
    },
}

/// Which UI a pending close should surface: a full modal ([`Cmd+W`][cmd_w] and
/// the tab context menu's Close) or the inline arm-then-confirm affordance
/// (the two X buttons).
///
/// [cmd_w]: crate::app::actions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmStyle {
    Modal,
    Inline,
}

/// One close awaiting confirmation. `PaneFlowApp` holds at most one of these
/// at a time; it drives both the modal and the inline armed-button state.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingClose {
    pub(crate) target: CloseTarget,
    pub(crate) style: ConfirmStyle,
    pub(crate) agent: TerminalAgent,
    /// Qualifying agents BEYOND [`Self::agent`], captured when the close was
    /// armed so the copy can say "and 2 other agents" instead of naming one
    /// and killing three.
    pub(crate) extra_agents: usize,
    /// Tab title (or pane title) shown in the modal copy. Empty is allowed.
    pub(crate) label: String,
    /// When this close was armed, so [`click_outcome`] can refuse to treat the
    /// second half of a double-click as a decision. Refreshed on every re-arm.
    pub(crate) armed_at: Instant,
}

impl PendingClose {
    pub(crate) fn targets_workspace(&self, workspace_id: u64) -> bool {
        matches!(
            &self.target,
            CloseTarget::Workspace { workspace_id: w } if *w == workspace_id
        )
    }

    /// True when `self` is the pending close for this exact tab, so a
    /// button can render its armed state.
    pub(crate) fn targets_tab(&self, workspace_id: u64, tab_id: u64) -> bool {
        matches!(
            &self.target,
            CloseTarget::Tab {
                workspace_id: w,
                tab_id: t,
            } if *w == workspace_id && *t == tab_id
        )
    }

    /// True when `self` is the pending close for this exact pane, so a
    /// button can render its armed state.
    ///
    /// Compared by entity id rather than by handle: the stored target is weak
    /// and the caller's is strong, and an id is stable for as long as the
    /// entity lives - which is longer than this comparison needs.
    pub(crate) fn targets_pane(&self, pane: &gpui::Entity<crate::pane::Pane>) -> bool {
        matches!(
            &self.target,
            CloseTarget::Pane { pane: p } if p.entity_id() == pane.entity_id()
        )
    }

    /// [`Self::targets_pane`] for a handle that is already weak - what the two
    /// click sites hold, having built a [`CloseTarget`] from the pane they
    /// were clicked on.
    fn targets_weak_pane(&self, pane: &gpui::WeakEntity<crate::pane::Pane>) -> bool {
        matches!(
            &self.target,
            CloseTarget::Pane { pane: p } if p.entity_id() == pane.entity_id()
        )
    }
}

/// What one click on an inline arm-then-confirm close button should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickOutcome {
    /// First click: remember the target and light the button up.
    Arm,
    /// Second click on the same armed target: actually close.
    Confirm,
}

/// How long an inline arm has to have been on screen before a click on the
/// same X counts as the decision to close.
///
/// A double-click delivers BOTH clicks to the button's listener, so without
/// this the second one confirms a kill behind an armed state that was painted
/// for a single frame - imperceptible, and on the two most-clicked close
/// controls in the app. 350 ms clears the macOS double-click interval while
/// staying under a deliberate second click.
pub(crate) const ARM_SETTLE: Duration = Duration::from_millis(350);

/// Decide whether a click on the X for `this_target` arms a confirmation or
/// confirms the one already pending.
///
/// Pure on purpose - the two call sites (the sidebar rail row's X and the pane
/// header's X) are both deep inside GPUI closures where nothing is testable,
/// so the whole decision lives here instead.
///
/// A click on the SAME target inside [`ARM_SETTLE`] re-arms rather than
/// confirming: both inline X buttons receive both clicks of a double-click, so
/// the settle delay is what makes the armed state a perception gate instead of
/// a single unperceivable frame.
///
/// A pending close in [`ConfirmStyle::Modal`] never confirms from here: the
/// modal owns its own Confirm button and its own `Enter`, and a click that
/// lands on the X behind it is a fresh gesture. Arming instead is the strictly
/// safer half of that choice - it TEARS THE MODAL DOWN and replaces it with an
/// inline arm, which is a visible re-ask, where confirming would kill a process
/// group through a dialog that is still asking the question.
///
/// Defensive, not reachable: `render_close_confirm_dialog` defers a full-screen
/// `.occlude()`d backdrop above every other overlay, so while a modal is up no
/// click reaches either X.
pub(crate) fn click_outcome(
    pending: Option<&PendingClose>,
    this_target: &CloseTarget,
    now: Instant,
) -> ClickOutcome {
    let Some(pending) = pending else {
        return ClickOutcome::Arm;
    };
    if pending.style != ConfirmStyle::Inline {
        return ClickOutcome::Arm;
    }
    let same_target = match this_target {
        CloseTarget::Workspace { workspace_id } => pending.targets_workspace(*workspace_id),
        CloseTarget::Tab {
            workspace_id,
            tab_id,
        } => pending.targets_tab(*workspace_id, *tab_id),
        CloseTarget::Pane { pane } => pending.targets_weak_pane(pane),
    };
    // A second click inside [`ARM_SETTLE`] is a double-click, not a decision:
    // the armed state was painted for one frame the user could not perceive.
    // Re-arming refreshes `armed_at`, so a burst of clicks never accumulates
    // into a confirmation either.
    if same_target && now.duration_since(pending.armed_at) >= ARM_SETTLE {
        ClickOutcome::Confirm
    } else {
        ClickOutcome::Arm
    }
}

/// Whether an `Escape` at the window root belongs to a live inline arm - and
/// so must be CONSUMED rather than forwarded to the terminal underneath.
///
/// Escape is the interrupt key for Claude Code and several other agents, so
/// forwarding it here would make "I changed my mind about closing" also
/// interrupt the very agent the user just decided to keep: a destructive side
/// effect on the cancel path of a safety feature. When nothing is armed - or
/// when the pending close is a [`ConfirmStyle::Modal`], which tracks focus and
/// handles its own key events - Escape passes through untouched, so vim and
/// every other Escape-driven program keep the keystroke.
///
/// Pure so both halves are assertable: the root capture handler it drives is a
/// GPUI closure with no test seam.
pub(crate) fn escape_consumes_inline_arm(pending: Option<&PendingClose>) -> bool {
    pending.is_some_and(|pending| pending.style == ConfirmStyle::Inline)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn tab_target(workspace_id: u64, tab_id: u64) -> CloseTarget {
        CloseTarget::Tab {
            workspace_id,
            tab_id,
        }
    }

    fn inline_pending(target: CloseTarget) -> PendingClose {
        inline_pending_armed_at(target, Instant::now())
    }

    fn inline_pending_armed_at(target: CloseTarget, armed_at: Instant) -> PendingClose {
        PendingClose {
            target,
            style: ConfirmStyle::Inline,
            agent: TerminalAgent::ClaudeCode,
            extra_agents: 0,
            label: String::new(),
            armed_at,
        }
    }

    /// A `now` far enough past the arm that the settle delay has elapsed, for
    /// the tests that are about something other than the delay itself.
    fn settled(pending: &PendingClose) -> Instant {
        pending.armed_at + ARM_SETTLE
    }

    /// Both halves of the Escape trade. Consuming unconditionally would eat a
    /// keystroke vim needs; forwarding unconditionally would interrupt the
    /// agent the user just chose to keep.
    #[test]
    fn escape_is_consumed_only_while_an_inline_arm_is_live() {
        assert!(
            !escape_consumes_inline_arm(None),
            "with nothing armed, Escape belongs to whatever is focused"
        );
        assert!(escape_consumes_inline_arm(Some(&inline_pending(
            tab_target(1, 2)
        ))));

        let mut modal = inline_pending(tab_target(1, 2));
        modal.style = ConfirmStyle::Modal;
        assert!(
            !escape_consumes_inline_arm(Some(&modal)),
            "the modal tracks focus and handles its own Escape; the root capture must not \
             swallow the key on its behalf"
        );
    }

    #[test]
    fn a_click_with_nothing_pending_arms() {
        assert_eq!(
            click_outcome(None, &tab_target(1, 2), Instant::now()),
            ClickOutcome::Arm
        );
    }

    #[test]
    fn a_second_click_on_the_same_inline_target_confirms() {
        let pending = inline_pending(tab_target(1, 2));
        assert_eq!(
            click_outcome(Some(&pending), &tab_target(1, 2), settled(&pending)),
            ClickOutcome::Confirm
        );
    }

    /// The reason [`ARM_SETTLE`] exists. Both of the inline X buttons receive
    /// BOTH clicks of a double-click, so without a settle delay the second one
    /// confirms a kill behind an armed state that was painted for a single
    /// frame - imperceptible, on the two most-clicked close controls in the
    /// app, and not recoverable by undo (the tab comes back, the agent does
    /// not).
    #[test]
    fn a_second_click_inside_the_settle_delay_re_arms_instead_of_confirming() {
        let armed_at = Instant::now();
        let pending = inline_pending_armed_at(tab_target(1, 2), armed_at);
        for early in [
            Duration::from_millis(0),
            Duration::from_millis(100),
            ARM_SETTLE - Duration::from_millis(1),
        ] {
            assert_eq!(
                click_outcome(Some(&pending), &tab_target(1, 2), armed_at + early),
                ClickOutcome::Arm,
                "a click {early:?} after the arm is the tail of a double-click, not a decision"
            );
        }
        // Past the delay the armed state has been on screen long enough to
        // read, so the second click means what it says.
        for late in [ARM_SETTLE, Duration::from_millis(500)] {
            assert_eq!(
                click_outcome(Some(&pending), &tab_target(1, 2), armed_at + late),
                ClickOutcome::Confirm,
                "a deliberate second click {late:?} after the arm must still confirm"
            );
        }
    }

    #[test]
    fn a_click_on_a_different_inline_target_re_arms_instead_of_confirming() {
        let pending = inline_pending(tab_target(1, 2));
        let now = settled(&pending);
        // Same workspace, different tab.
        assert_eq!(
            click_outcome(Some(&pending), &tab_target(1, 3), now),
            ClickOutcome::Arm
        );
        // Same tab id, different workspace.
        assert_eq!(
            click_outcome(Some(&pending), &tab_target(9, 2), now),
            ClickOutcome::Arm
        );
    }

    #[test]
    fn a_click_under_a_modal_on_the_same_target_arms_rather_than_confirming() {
        // The modal owns its own Confirm button and its own Enter key, so a
        // click on the X behind it is a fresh gesture, not the second half of
        // an inline arm. Arming replaces the modal with an inline arm - a
        // visible re-ask - which is strictly safer than letting a stray click
        // confirm a kill through a dialog that is still asking the question.
        // Defensive either way: the modal's occluding backdrop means no click
        // reaches the X while it is up.
        let mut pending = inline_pending(tab_target(1, 2));
        pending.style = ConfirmStyle::Modal;
        assert_eq!(
            click_outcome(Some(&pending), &tab_target(1, 2), settled(&pending)),
            ClickOutcome::Arm
        );
    }

    fn pane_target(pane: &gpui::Entity<crate::pane::Pane>) -> CloseTarget {
        CloseTarget::Pane {
            pane: pane.downgrade(),
        }
    }

    #[gpui::test]
    fn click_outcome_discriminates_pane_targets_by_entity(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let a = test_pane(cx, 1);
        let b = test_pane(cx, 1);

        let pending = inline_pending(pane_target(&a));
        let now = settled(&pending);
        assert_eq!(
            click_outcome(Some(&pending), &pane_target(&a), now),
            ClickOutcome::Confirm
        );
        assert_eq!(
            click_outcome(Some(&pending), &pane_target(&b), now),
            ClickOutcome::Arm
        );
        // A pane click never confirms a pending TAB close, and vice versa.
        let tab_pending = inline_pending(tab_target(1, 2));
        assert_eq!(
            click_outcome(Some(&tab_pending), &pane_target(&a), now),
            ClickOutcome::Arm
        );
        assert_eq!(
            click_outcome(Some(&pending), &tab_target(1, 2), now),
            ClickOutcome::Arm
        );
    }

    fn state(
        detected_agent: Option<TerminalAgent>,
        agent_confirmed: bool,
        agent_declared_until: Option<Instant>,
        child_is_live: bool,
    ) -> SurfaceCloseState {
        SurfaceCloseState {
            detected_agent,
            agent_confirmed,
            agent_declared_until,
            child_is_live,
        }
    }

    #[test]
    fn no_detected_agent_never_confirms() {
        let now = Instant::now();
        // Every combination of the other fields, agent absent throughout.
        assert!(!surface_close_needs_confirmation(
            &state(None, false, None, true),
            now
        ));
        assert!(!surface_close_needs_confirmation(
            &state(None, true, Some(now + Duration::from_secs(5)), true),
            now
        ));
        assert!(!surface_close_needs_confirmation(
            &state(None, true, None, false),
            now
        ));
    }

    #[test]
    fn confirmed_agent_with_live_child_needs_confirmation() {
        let now = Instant::now();
        assert!(surface_close_needs_confirmation(
            &state(Some(TerminalAgent::ClaudeCode), true, None, true),
            now
        ));
    }

    #[test]
    fn confirmed_agent_with_exited_child_never_confirms() {
        // `child_is_live: false` covers more than a clean shell exit: a
        // dead-but-unreaped child, a recycled pid, and a libproc `EPERM` all
        // land here too, and all of them used to leave `exited == None`
        // forever - which is exactly why the field is a liveness signal
        // rather than `!exited`.
        let now = Instant::now();
        assert!(!surface_close_needs_confirmation(
            &state(Some(TerminalAgent::ClaudeCode), true, None, false),
            now
        ));
    }

    #[test]
    fn unconfirmed_declared_agent_inside_grace_window_needs_confirmation() {
        let now = Instant::now();
        let until = now + Duration::from_secs(5);
        assert!(surface_close_needs_confirmation(
            &state(Some(TerminalAgent::Codex), false, Some(until), true),
            now
        ));
    }

    #[test]
    fn unconfirmed_declared_agent_past_grace_window_never_confirms() {
        let now = Instant::now();
        let until = now - Duration::from_secs(5);
        assert!(!surface_close_needs_confirmation(
            &state(Some(TerminalAgent::Codex), false, Some(until), true),
            now
        ));
    }

    #[test]
    fn unconfirmed_session_restored_agent_never_confirms() {
        // Session restore leaves agent_declared_until = None and
        // agent_confirmed = false (pty_session.rs field doc comment); a
        // "last known" agent from a previous run must not gate a close.
        let now = Instant::now();
        assert!(!surface_close_needs_confirmation(
            &state(Some(TerminalAgent::OpenCode), false, None, true),
            now
        ));
    }

    #[test]
    fn agent_needing_confirmation_over_empty_slice_is_none() {
        let now = Instant::now();
        assert_eq!(agent_needing_confirmation(&[], now), None);
    }

    #[test]
    fn agent_needing_confirmation_returns_first_qualifying_and_skips_others() {
        let now = Instant::now();
        let states = [
            // Leading: no agent at all.
            state(None, false, None, true),
            // Leading: agent present but its child already exited.
            state(Some(TerminalAgent::Grok), true, None, false),
            // First surface that actually needs confirmation.
            state(Some(TerminalAgent::ClaudeCode), true, None, true),
            // A second qualifying surface further down must NOT win.
            state(Some(TerminalAgent::Codex), true, None, true),
        ];
        assert_eq!(
            agent_needing_confirmation(&states, now),
            Some(TerminalAgent::ClaudeCode)
        );
    }

    #[test]
    fn agents_needing_confirmation_count_is_zero_when_nothing_qualifies() {
        let now = Instant::now();
        assert_eq!(agents_needing_confirmation_count(&[], now), 0);
        let states = [
            state(None, true, None, true),
            // Agent present, but its child already exited.
            state(Some(TerminalAgent::Grok), true, None, false),
            // Session-restored "last known" agent, never scan-confirmed.
            state(Some(TerminalAgent::OpenCode), false, None, true),
        ];
        assert_eq!(agents_needing_confirmation_count(&states, now), 0);
    }

    #[test]
    fn agents_needing_confirmation_count_counts_every_qualifying_surface() {
        let now = Instant::now();
        let states = [
            state(None, false, None, true),
            state(Some(TerminalAgent::ClaudeCode), true, None, true),
            state(Some(TerminalAgent::Grok), true, None, false),
            state(Some(TerminalAgent::Codex), true, None, true),
            state(
                Some(TerminalAgent::Gemini),
                false,
                Some(now + Duration::from_secs(5)),
                true,
            ),
        ];
        assert_eq!(agents_needing_confirmation_count(&states, now), 3);
    }

    #[test]
    fn targets_tab_discriminates() {
        let pending = PendingClose {
            target: CloseTarget::Tab {
                workspace_id: 1,
                tab_id: 2,
            },
            style: ConfirmStyle::Modal,
            agent: TerminalAgent::ClaudeCode,
            extra_agents: 0,
            label: "Fix the bug".into(),
            armed_at: Instant::now(),
        };
        assert!(pending.targets_tab(1, 2));
        assert!(!pending.targets_tab(1, 3));
        assert!(!pending.targets_tab(9, 2));
    }

    fn test_pane(
        cx: &mut impl gpui::AppContext,
        workspace_id: u64,
    ) -> gpui::Entity<crate::pane::Pane> {
        let terminal =
            cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(workspace_id, cx));
        cx.new(|cx| crate::pane::Pane::new(terminal, workspace_id, cx))
    }

    #[gpui::test]
    fn targets_pane_discriminates_and_a_tab_target_never_matches_a_pane(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let a = test_pane(cx, 1);
        let b = test_pane(cx, 1);

        let pane_pending = PendingClose {
            target: pane_target(&a),
            style: ConfirmStyle::Inline,
            agent: TerminalAgent::ClaudeCode,
            extra_agents: 0,
            label: String::new(),
            armed_at: Instant::now(),
        };
        assert!(pane_pending.targets_pane(&a));
        assert!(!pane_pending.targets_pane(&b));

        let tab_pending = PendingClose {
            target: CloseTarget::Tab {
                workspace_id: 1,
                tab_id: 2,
            },
            style: ConfirmStyle::Modal,
            agent: TerminalAgent::ClaudeCode,
            extra_agents: 0,
            label: String::new(),
            armed_at: Instant::now(),
        };
        assert!(!tab_pending.targets_pane(&a));
        assert!(!tab_pending.targets_pane(&b));
    }
}
