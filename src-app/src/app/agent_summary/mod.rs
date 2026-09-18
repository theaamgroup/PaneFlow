//! Fleet agent summary (issue #576): one global chord opens an overlay that
//! says, in plain English, what every agent pane across every workspace is
//! doing right now.
//!
//! Summaries are generated on-device by Apple's Foundation Models through the
//! packaged Swift sidecar (see [`model`]). Nothing leaves the machine, there
//! is no API key, and there is no telemetry.
//!
//! Shape follows Pane Overview (`app/pane_overview/`), its nearest peer: a
//! `deferred(...)` overlay with its own focus handle, opened by a Global
//! action, closed on Esc, and gated to CLI mode.
//!
//! Work is dispatched one pane at a time onto background threads and lands
//! back on the GPUI thread as it completes, so a twelve-agent fleet fills in
//! progressively instead of blocking on the slowest pane. Every task carries
//! the `generation` it was issued under and is dropped on arrival if the
//! overlay has since closed or been reopened.

pub(crate) mod model;
pub(crate) mod summarize;
pub(crate) mod view;

use gpui::{Context, Window};

use crate::app::overlay_origin::OverlayKind;

use crate::PaneFlowApp;
use crate::limits::clamp_untrusted_label;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use model::SummaryError;
use summarize::PaneContext;

/// Model calls in flight at once.
///
/// Each one is a subprocess that loads the Foundation Models runtime, and the
/// on-device model serializes internally anyway, so spawning one per pane just
/// costs memory and blocking-pool threads without finishing any sooner. Four
/// keeps the pipeline full on a large fleet - `MAX_PANES` is 32 per workspace
/// and `MAX_WORKSPACES` is 32 - while rows still land progressively.
pub(crate) const MAX_CONCURRENT_SUMMARIES: usize = 4;

/// Per-pane row state in the overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SummaryStatus {
    /// Dispatched, model has not answered yet.
    Pending,
    /// One-line summary from the model.
    Ready(String),
    /// The pane had no transcript worth summarizing. Distinct from
    /// [`Self::Ready`] with an "Idle." text: no model call was made at all.
    Empty,
    /// This pane's call failed. Other panes may still succeed.
    Failed(String),
}

/// One row: a pane's identity, plus whatever we know about its summary.
#[derive(Clone, Debug)]
pub(crate) struct SummaryEntry {
    pub surface_id: u64,
    pub ws_idx: usize,
    pub ws_title: String,
    pub tab_title: String,
    pub name: String,
    pub agent: Option<crate::agent_launcher::TerminalAgent>,
    pub exited: bool,
    pub status: SummaryStatus,
}

/// Open-overlay state.
pub(crate) struct AgentSummaryState {
    pub entries: Vec<SummaryEntry>,
    /// Bumped on every open and close. A background task whose generation no
    /// longer matches has been orphaned and must not write.
    pub generation: u64,
    /// Set when the model is unavailable on this machine at all. The overlay
    /// renders one explanatory line instead of a list of identical failures,
    /// and no further calls are dispatched.
    pub unavailable: Option<String>,
    /// Row under the keyboard cursor.
    pub selected: usize,
}

/// The line shown under a row, for a given status.
///
/// Pure so the wording of every state - including the two that are easy to
/// get wrong, a pane with no output and a pane whose call failed - is pinned
/// by tests rather than by reading the render code.
pub(crate) fn row_summary_text(status: &SummaryStatus) -> &str {
    match status {
        SummaryStatus::Pending => "Reading the pane…",
        SummaryStatus::Ready(text) => text,
        SummaryStatus::Empty => "Nothing on screen yet.",
        SummaryStatus::Failed(why) => why,
    }
}

/// Project the Pane Overview card list onto summary rows.
///
/// Agent panes only: a plain shell is not what this overlay is for, and
/// summarizing every idle `zsh` would spend the model budget on prompts
/// nobody is supervising. Card order is preserved, so rows stay grouped by
/// workspace and tab the way the collector walked them.
pub(crate) fn entries_from_cards(
    cards: &[crate::app::pane_overview::rows::CardMeta],
) -> Vec<SummaryEntry> {
    cards
        .iter()
        .filter(|c| c.agent.is_some())
        .map(|c| SummaryEntry {
            surface_id: c.surface_id,
            ws_idx: c.ws_idx,
            ws_title: c.ws_title.clone(),
            tab_title: c.tab_title.clone(),
            name: c.name.clone(),
            agent: c.agent,
            exited: c.exited,
            status: SummaryStatus::Pending,
        })
        .collect()
}

impl AgentSummaryState {
    fn pending_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, SummaryStatus::Pending))
            .count()
    }

    /// Progress line for the overlay header.
    pub(crate) fn progress(&self) -> Option<String> {
        let pending = self.pending_count();
        (pending > 0).then(|| {
            let done = self.entries.len() - pending;
            format!("Summarising… {done}/{}", self.entries.len())
        })
    }
}

impl PaneFlowApp {
    pub(crate) fn handle_open_agent_summary(
        &mut self,
        _: &crate::OpenAgentSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same mode gate as the other cockpit overlays: a mode switch must not
        // leave cockpit chrome painted over Agents/Review.
        if !matches!(self.mode, paneflow_config::schema::AppMode::Cli) {
            return;
        }
        if !self.cached_config.agent_summary_enabled() {
            return;
        }
        if self.agent_summary.is_some() {
            self.close_agent_summary_and_restore_focus(window, cx);
            return;
        }
        self.open_agent_summary(window, cx);
    }

    fn open_agent_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A reopen must cancel whatever the previous generation left in flight
        // before issuing a fresh token, or the stale tasks keep their sidecars
        // alive beside the new overlay's and the global cap is exceeded.
        self.cancel_agent_summary_work();
        let generation = self.agent_summary_generation.wrapping_add(1);
        self.agent_summary_generation = generation;
        let cancellation = Arc::new(AtomicBool::new(false));
        self.agent_summary_cancellation = Some(cancellation.clone());

        // Reuse Pane Overview's collector: it already walks every workspace
        // and every tab (not just the active one) and resolves agent identity
        // and exit state per pane.
        let cards = self.collect_pane_overview_cards(window, cx);
        let entries = entries_from_cards(&cards);
        // Issue #584: Escape and a command palette that folds this overlay
        // land on the pane it was opened from, resolved before it takes focus.
        self.remember_overlay_origin(OverlayKind::AgentSummary, window, cx);

        self.agent_summary = Some(AgentSummaryState {
            entries,
            generation,
            unavailable: None,
            selected: 0,
        });
        self.agent_summary_focus.focus(window, cx);
        cx.notify();

        self.dispatch_agent_summaries(generation, cancellation, cx);
    }

    /// Kick off one background task per pane.
    fn dispatch_agent_summaries(
        &mut self,
        generation: u64,
        cancellation: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let Some(sidecar) = std::env::current_exe()
            .ok()
            .and_then(|exe| model::sidecar_path(&exe))
        else {
            self.finish_agent_summary_unavailable(
                "Could not locate the summariser".into(),
                generation,
                cx,
            );
            return;
        };

        // A pane that closed between `collect_pane_overview_cards` and here
        // has no terminal to read. Those rows must be SETTLED, not dropped:
        // an entry silently removed from the dispatch list keeps
        // `SummaryStatus::Pending` forever, so its row reads "Reading the
        // pane…" and `progress()` pins the header on "Summarising… n/N" for
        // as long as the overlay stays open.
        let mut unresolved: Vec<u64> = Vec::new();
        let targets: Vec<(u64, PaneContext, _)> = self
            .agent_summary
            .as_ref()
            .map(|state| state.entries.clone())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                // An exited pane has nothing live to describe; its last screen
                // is still worth a summary, so it is kept, but a pane whose
                // terminal has gone away entirely cannot be read at all.
                let Some(terminal) = crate::app::ipc_handler::find_terminal_by_surface_id(
                    &self.workspaces,
                    entry.surface_id,
                    cx,
                ) else {
                    unresolved.push(entry.surface_id);
                    return None;
                };
                let view = terminal.read(cx);
                let reader = view.terminal.scrollback_reader();
                let context = PaneContext {
                    surface_id: entry.surface_id,
                    agent: entry.agent.map(|a| a.display_name().to_string()),
                    name: entry.name.clone(),
                    cwd: view
                        .terminal
                        .current_cwd
                        .as_deref()
                        .map(std::path::Path::new)
                        .and_then(|p| p.file_name())
                        .map(|n| clamp_untrusted_label(&n.to_string_lossy())),
                    transcript: String::new(),
                };
                Some((entry.surface_id, context, reader))
            })
            .collect();

        for surface_id in unresolved {
            self.apply_agent_summary(
                surface_id,
                generation,
                Err(SummaryError::Failed("Pane is gone".into())),
                cx,
            );
        }

        // One shared, app-lifetime limiter: reopens reuse the same permit pool,
        // so the global sidecar cap holds across generations rather than being
        // rebuilt (and effectively reset) on every dispatch.
        let permits = Arc::clone(&self.agent_summary_permits);
        for (surface_id, mut context, reader) in targets {
            let sidecar = sidecar.clone();
            let permits = Arc::clone(&permits);
            let cancellation = Arc::clone(&cancellation);
            cx.spawn(async move |app, cx| {
                // Do not even queue for a permit the overlay no longer wants.
                if cancellation.load(Ordering::Acquire) {
                    return;
                }
                // Held across the blocking section and released on drop, so a
                // cancelled or panicking task cannot leak a permit.
                let _permit = permits.acquire_arc().await;
                // The overlay may have closed or reopened while this task
                // waited for a permit; do not spawn a sidecar for it.
                if cancellation.load(Ordering::Acquire) {
                    return;
                }
                let cancel_flag = Arc::clone(&cancellation);
                // Both halves block: the transcript read parks on the runtime
                // thread's reply, and the model call waits on a child. Neither
                // may run on the GPUI thread (issue #363).
                let outcome = smol::unblock(move || {
                    let Some((text, _, _, _)) =
                        reader.extract_scrollback_window(summarize::TAIL_LINES, 0)
                    else {
                        return Err(SummaryError::Failed(
                            "terminal runtime did not answer".into(),
                        ));
                    };
                    context.transcript = text;
                    let request =
                        summarize::build_request(&context, crate::app::ipc_handler::wrap_untrusted);
                    if request.prompt.is_empty() {
                        return Ok(None);
                    }
                    // The overlay may have been dismissed while the transcript
                    // was being read; do not start a child for a dead overlay.
                    if cancel_flag.load(Ordering::Acquire) {
                        return Err(SummaryError::Failed("cancelled".into()));
                    }
                    model::summarize_blocking(&sidecar, &request, &cancel_flag).map(Some)
                })
                .await;

                // A cancelled task's result is worthless; skip the apply round
                // trip rather than hand a stale answer to a newer overlay.
                if cancellation.load(Ordering::Acquire) {
                    return;
                }
                let _ = app.update(cx, |app, cx| {
                    app.apply_agent_summary(surface_id, generation, outcome, cx);
                });
            })
            .detach();
        }
    }

    /// Land one pane's result, or discard it if the overlay moved on.
    fn apply_agent_summary(
        &mut self,
        surface_id: u64,
        generation: u64,
        outcome: Result<Option<String>, SummaryError>,
        cx: &mut Context<Self>,
    ) {
        // A terminal failure condemns the feature, not the pane, so it is
        // handled before the generation-scoped entry lookup.
        if let Err(error) = &outcome
            && error.is_terminal()
        {
            self.finish_agent_summary_unavailable(error.user_message(), generation, cx);
            return;
        }

        let Some(state) = self.agent_summary.as_mut() else {
            return;
        };
        if state.generation != generation {
            return;
        }
        let Some(entry) = state
            .entries
            .iter_mut()
            .find(|e| e.surface_id == surface_id)
        else {
            return;
        };
        entry.status = match outcome {
            Ok(Some(summary)) => SummaryStatus::Ready(summary),
            Ok(None) => SummaryStatus::Empty,
            Err(error) => SummaryStatus::Failed(error.user_message()),
        };
        cx.notify();
    }

    fn finish_agent_summary_unavailable(
        &mut self,
        message: String,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(state) = self.agent_summary.as_mut() else {
                return;
            };
            if state.generation != generation {
                return;
            }
            state.unavailable = Some(message);
            // Stop every in-flight task from writing a row-level duplicate of
            // the same failure.
            state.generation = state.generation.wrapping_add(1);
            self.agent_summary_generation = state.generation;
        }
        // The feature is off for the rest of the session, so kill the sidecars
        // still running rather than let them wait out their deadline.
        self.cancel_agent_summary_work();
        cx.notify();
    }

    /// Close without touching the focus: a jump to a row's pane, or a command
    /// palette fold, lands the focus itself.
    pub(crate) fn close_agent_summary(&mut self, cx: &mut Context<Self>) {
        self.drop_agent_summary_state();
        self.forget_overlay_origin(OverlayKind::AgentSummary);
        cx.notify();
    }

    /// Kill in-flight sidecars and orphan every queued task so a late model
    /// answer cannot repopulate a closed overlay.
    fn drop_agent_summary_state(&mut self) {
        self.cancel_agent_summary_work();
        self.agent_summary_generation = self.agent_summary_generation.wrapping_add(1);
        self.agent_summary = None;
    }

    /// Signal any in-flight summary work to stop and clear the shared token.
    ///
    /// The queued tasks observe the flag before acquiring a permit, after
    /// acquiring it, and before spawning a child; the sidecar runner observes
    /// it while the child runs and terminates the process tree. A task from a
    /// stale generation can therefore never keep a sidecar alive past the
    /// overlay that asked for it.
    fn cancel_agent_summary_work(&mut self) {
        if let Some(cancellation) = self.agent_summary_cancellation.take() {
            cancellation.store(true, Ordering::Release);
        }
    }

    pub(crate) fn close_agent_summary_and_restore_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Issue #584: the pane the overlay was opened from, then the first
        // pane, then the empty-workspace placeholder.
        self.drop_agent_summary_state();
        self.restore_overlay_origin_focus(OverlayKind::AgentSummary, window, cx);
        cx.notify();
    }

    /// Enter / click on a row: jump to that pane and close.
    pub(crate) fn agent_summary_activate(
        &mut self,
        surface_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.teleport_to_surface(surface_id, window, cx) {
            self.close_agent_summary(cx);
        }
    }

    pub(crate) fn handle_agent_summary_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let Some(state) = self.agent_summary.as_ref() else {
            return;
        };
        let len = state.entries.len();
        match key {
            "escape" => self.close_agent_summary_and_restore_focus(window, cx),
            "enter" => {
                if let Some(entry) = state.entries.get(state.selected) {
                    let surface_id = entry.surface_id;
                    self.agent_summary_activate(surface_id, window, cx);
                }
            }
            "down" | "j" if len > 0 => {
                if let Some(state) = self.agent_summary.as_mut() {
                    state.selected = (state.selected + 1) % len;
                    cx.notify();
                }
            }
            "up" | "k" if len > 0 => {
                if let Some(state) = self.agent_summary.as_mut() {
                    state.selected = (state.selected + len - 1) % len;
                    cx.notify();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(surface_id: u64, status: SummaryStatus) -> SummaryEntry {
        SummaryEntry {
            surface_id,
            ws_idx: 0,
            ws_title: "ws".into(),
            tab_title: "tab".into(),
            name: "pane".into(),
            agent: None,
            exited: false,
            status,
        }
    }

    fn state(statuses: Vec<SummaryStatus>) -> AgentSummaryState {
        AgentSummaryState {
            entries: statuses
                .into_iter()
                .enumerate()
                .map(|(i, s)| entry(i as u64, s))
                .collect(),
            generation: 1,
            unavailable: None,
            selected: 0,
        }
    }

    #[test]
    fn progress_counts_only_pending_rows() {
        let s = state(vec![
            SummaryStatus::Pending,
            SummaryStatus::Ready("x".into()),
            SummaryStatus::Empty,
        ]);
        assert_eq!(s.progress().as_deref(), Some("Summarising… 2/3"));
    }

    #[test]
    fn progress_is_absent_once_every_row_settled() {
        let s = state(vec![
            SummaryStatus::Ready("x".into()),
            SummaryStatus::Failed("boom".into()),
        ]);
        assert_eq!(s.progress(), None);
    }

    fn card(
        surface_id: u64,
        ws_idx: usize,
        agent: Option<crate::agent_launcher::TerminalAgent>,
    ) -> crate::app::pane_overview::rows::CardMeta {
        crate::app::pane_overview::rows::CardMeta {
            surface_id,
            ws_idx,
            ws_title: format!("ws{ws_idx}"),
            tab_idx: 0,
            tab_title: "tab".into(),
            tab_pane_index: 0,
            tab_pane_count: 1,
            name: "pane".into(),
            cwd_label: None,
            agent,
            state: None,
            cols: 80,
            rows: 24,
            exited: false,
            is_active: false,
            ws_is_active: false,
            ws_branch: String::new(),
        }
    }

    #[test]
    fn only_agent_panes_become_rows() {
        // A window full of plain shells must not spend a model call each.
        let cards = vec![
            card(1, 0, Some(crate::agent_launcher::TerminalAgent::ClaudeCode)),
            card(2, 0, None),
            card(3, 1, Some(crate::agent_launcher::TerminalAgent::Codex)),
        ];
        let rows = entries_from_cards(&cards);
        assert_eq!(
            rows.iter().map(|r| r.surface_id).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn rows_keep_collector_order_so_workspace_grouping_holds() {
        // The view emits a workspace header whenever ws_idx changes, so a
        // reordering here would print the same workspace header twice.
        let cards = vec![
            card(1, 0, Some(crate::agent_launcher::TerminalAgent::ClaudeCode)),
            card(2, 1, Some(crate::agent_launcher::TerminalAgent::ClaudeCode)),
            card(3, 1, Some(crate::agent_launcher::TerminalAgent::Codex)),
        ];
        let rows = entries_from_cards(&cards);
        assert_eq!(
            rows.iter().map(|r| r.ws_idx).collect::<Vec<_>>(),
            vec![0, 1, 1]
        );
    }

    #[test]
    fn every_row_starts_pending_so_the_header_counts_the_whole_fleet() {
        let cards = vec![card(
            1,
            0,
            Some(crate::agent_launcher::TerminalAgent::ClaudeCode),
        )];
        let rows = entries_from_cards(&cards);
        assert!(matches!(rows[0].status, SummaryStatus::Pending));
    }

    #[test]
    fn row_text_distinguishes_no_output_from_a_failure() {
        // Both render muted; they must not render the same words, or a broken
        // summariser is indistinguishable from a quiet agent.
        let failed_status = SummaryStatus::Failed("timeout".into());
        let ready_status = SummaryStatus::Ready("x".into());
        let empty = row_summary_text(&SummaryStatus::Empty);
        let failed = row_summary_text(&failed_status);
        assert_ne!(empty, failed);
        assert_eq!(failed, "timeout");
        assert_eq!(row_summary_text(&ready_status), "x");
    }

    #[test]
    fn only_pending_can_hang_the_progress_header() {
        // The invariant behind the "pane is gone" fix: every terminal state
        // settles, so a row that is never dispatched MUST NOT be left Pending.
        for status in [
            SummaryStatus::Ready("x".into()),
            SummaryStatus::Empty,
            SummaryStatus::Failed("Pane is gone".into()),
        ] {
            assert_eq!(state(vec![status.clone()]).progress(), None, "{status:?}");
        }
        assert!(state(vec![SummaryStatus::Pending]).progress().is_some());
    }

    #[test]
    fn a_gone_pane_settles_only_its_own_row() {
        // A pane closing between collection and dispatch is an ordinary
        // per-row outcome; it must not read as "the model is unavailable" and
        // condemn the whole overlay.
        let gone = SummaryError::Failed("Pane is gone".into());
        assert!(!gone.is_terminal());
        assert_eq!(gone.user_message(), "Pane is gone");
    }

    #[test]
    fn a_failed_row_settles_the_overlay_rather_than_hanging_it() {
        // A per-pane failure must not leave the header stuck on "Summarising…".
        let s = state(vec![SummaryStatus::Failed("timeout".into())]);
        assert_eq!(s.progress(), None);
    }
}
