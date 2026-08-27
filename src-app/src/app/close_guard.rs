//! Pure predicate for whether closing a surface, tab, or pane would kill a
//! live coding-agent process without asking first, plus the state a close
//! confirmation UI hangs off.
//!
//! This module is intentionally standalone: no GPUI rendering, no IPC, no
//! process signaling. Everything here is a plain data → plain data function
//! so it can be unit-tested without a window, a PTY, or a real agent CLI.
//! The wiring lives next door in [`crate::app::close_confirm`] (the modal and
//! the request/confirm/cancel entry points); the inline arm-then-confirm
//! buttons land in a later task.

use std::time::Instant;

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
    /// Resolved by stable ids, never by index - rows reorder under a drag.
    Tab { workspace_id: u64, tab_id: u64 },
    Pane {
        pane: gpui::Entity<crate::pane::Pane>,
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
    // Constructed by the inline arm-then-confirm X buttons landing in the next
    // task; only the tests reach it today.
    #[allow(dead_code)]
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
}

impl PendingClose {
    /// True when `self` is the pending close for this exact tab, so a
    /// button can render its armed state.
    // Consumed by the inline tab-X armed state landing in the next task; only
    // the tests reach it today, and `cfg(test)` use does not satisfy the lint.
    #[allow(dead_code)]
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
    pub(crate) fn targets_pane(&self, pane: &gpui::Entity<crate::pane::Pane>) -> bool {
        matches!(&self.target, CloseTarget::Pane { pane: p } if p == pane)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

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
            target: CloseTarget::Pane { pane: a.clone() },
            style: ConfirmStyle::Inline,
            agent: TerminalAgent::ClaudeCode,
            extra_agents: 0,
            label: String::new(),
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
        };
        assert!(!tab_pending.targets_pane(&a));
        assert!(!tab_pending.targets_pane(&b));
    }
}
