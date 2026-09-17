//! Agent Summary (issue #576): one global chord (`Cmd+Shift+I`) opens an
//! overlay listing every agent pane across every workspace with a
//! plain-English line about what it is doing, generated on this Mac by
//! Apple's Foundation Models through the `paneflow-agent-summary` sidecar.
//! No network, no API key, no telemetry.
//!
//! Shape: a `deferred(...).with_priority(6)` overlay like the Pane Overview,
//! which also supplies the pane walk (`pane_overview::collect_cards`) and the
//! status grammar (`pane_overview_status_visual`). Rows are captured once at
//! open - the summaries describe that moment, and reopening takes a fresh
//! snapshot - in workspace, tab, pane order so the list reads like the
//! sidebar; the *work* order is attention-first (`summary_order`) so the
//! pane that needs the person is summarised before the ones that are busy.
//!
//! Each pane's tail is read off the render thread through its
//! `ScrollbackReader`, stripped and capped (`prompt::prepare_tail`), wrapped
//! in the `surface.read` anti-injection fence (`ipc_handler::wrap_untrusted`)
//! and handed to one sidecar process per pane under a deadline
//! (`sidecar::summarize`). Closing the overlay flips the cancel flag the
//! process runner polls - the in-flight helper is killed - and every later
//! reply is discarded by the generation check in `agent_summary_apply_reply`.
//!
//! Degrade quietly: no helper in this build, or a helper whose `--probe`
//! says the model is unavailable, leaves the overlay open with the agent
//! list and a one-line note where the summaries would be. Never a dialog.

pub(crate) mod prompt;
pub(crate) mod sidecar;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    AnyElement, App, AsyncApp, ClickEvent, Context, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, ScrollHandle, SharedString, Styled, WeakEntity, Window, deferred,
    div, prelude::*, px,
};

use crate::PaneFlowApp;
use crate::agent_launcher::TerminalAgent;
use crate::ai_types::AgentState;
use crate::app::ipc_handler::{find_pane_by_surface_id, wrap_untrusted};
use crate::app::pane_overview::rows::CardMeta;
use crate::app::pane_overview::{agent_state_label, pane_overview_status_visual};
use prompt::{ModelAvailability, SidecarReply, TAIL_LINES};

/// Same top inset as the Pane Overview so the two fleet overlays sit on one
/// line.
const OVERLAY_MARGIN: f32 = 24.0;
/// A reading-width column: summaries are sentences, not cards.
const OVERLAY_MAX_W: f32 = 720.0;
const LIST_PADDING: f32 = 12.0;
const ROW_GAP: f32 = 6.0;
const ROW_RADIUS: f32 = 8.0;

/// Where one row's summary stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SummaryStatus {
    /// Queued or in flight.
    Pending,
    Ready(String),
    /// The helper answered with an error (already tidied); shown muted.
    Failed(String),
    /// Never attempted: the pane's process exited, or the model is
    /// unavailable. The header explains the latter; the row shows nothing.
    Skipped,
}

/// What the overlay knows about the on-device model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelStatus {
    Probing,
    Available,
    Unavailable(String),
}

/// One agent pane. Plain data captured at open, no entities, so the pure
/// transforms below stay testable.
#[derive(Clone, Debug)]
pub(crate) struct SummaryRow {
    pub surface_id: u64,
    pub ws_idx: usize,
    pub ws_title: String,
    pub tab_title: String,
    /// Already clamped through `limits::clamp_untrusted_label`.
    pub name: String,
    pub agent: Option<TerminalAgent>,
    pub state: Option<AgentState>,
    pub exited: bool,
    pub summary: SummaryStatus,
}

/// Open-overlay state. Absent from `PaneFlowApp` when closed.
pub(crate) struct AgentSummaryState {
    pub rows: Vec<SummaryRow>,
    pub selected: usize,
    pub model: ModelStatus,
    /// Which open this is; a reply stamped with another generation is
    /// dropped on the floor.
    pub generation: u64,
    /// Flipped on close. Polled by `paneflow_process` inside the sidecar
    /// run, so the in-flight helper dies within a poll interval.
    pub cancel: Arc<AtomicBool>,
    pub scroll: ScrollHandle,
    /// Follow keyboard movement; ordinary scrolling stays put.
    pub reveal_selection: bool,
}

impl Drop for AgentSummaryState {
    fn drop(&mut self) {
        // Belt and braces: every close path sets this explicitly, and a
        // state that is replaced or dropped any other way must not leave a
        // helper running to its deadline.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// One unit of background work: read a pane's tail, fence it, ask the
/// helper. Built on the GPUI thread at open, consumed on a worker.
struct SummaryJob {
    surface_id: u64,
    agent: &'static str,
    state: &'static str,
    reader: crate::terminal::ScrollbackReader,
}

/// Keep only panes with an agent - a detected launcher or a live session -
/// in the caller's traversal order (workspace, tab, pane). Exited panes
/// stay listed (their state still says "Stopped") but are never summarised.
pub(crate) fn agent_rows(cards: Vec<CardMeta>) -> Vec<SummaryRow> {
    cards
        .into_iter()
        .filter(|card| card.agent.is_some() || card.state.is_some())
        .map(|card| SummaryRow {
            summary: if card.exited {
                SummaryStatus::Skipped
            } else {
                SummaryStatus::Pending
            },
            surface_id: card.surface_id,
            ws_idx: card.ws_idx,
            ws_title: card.ws_title,
            tab_title: card.tab_title,
            name: card.name,
            agent: card.agent,
            state: card.state,
            exited: card.exited,
        })
        .collect()
}

/// Work order: the panes that need the person first, then the busy ones,
/// then the finished and idle ones. Stable within a rank, so two waiting
/// panes keep their on-screen order. Exited panes are not in the queue.
pub(crate) fn summary_order(rows: &[SummaryRow]) -> Vec<usize> {
    let mut order: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.summary == SummaryStatus::Pending)
        .map(|(i, _)| i)
        .collect();
    order.sort_by_key(|&i| attention_rank(rows[i].state.as_ref()));
    order
}

fn attention_rank(state: Option<&AgentState>) -> u8 {
    match state {
        Some(AgentState::WaitingForInput | AgentState::Errored | AgentState::Stalled) => 0,
        Some(AgentState::Thinking) => 1,
        Some(AgentState::Finished) => 2,
        None => 3,
    }
}

/// Index of the selected row among the scroll container's children, which
/// interleave one header per workspace group ahead of that group's rows.
pub(crate) fn scroll_item_index(rows: &[SummaryRow], selected: usize) -> usize {
    let mut index = 0;
    let mut last_ws = None;
    for (i, row) in rows.iter().enumerate() {
        if last_ws != Some(row.ws_idx) {
            last_ws = Some(row.ws_idx);
            index += 1;
        }
        if i == selected {
            return index;
        }
        index += 1;
    }
    index.saturating_sub(1)
}

/// The header's one-line model note, or `None` while every row speaks for
/// itself. Pure so the copy is pinned by a test.
pub(crate) fn model_note(model: &ModelStatus, pending: usize) -> Option<String> {
    match model {
        ModelStatus::Probing => Some("Checking the on-device model\u{2026}".to_owned()),
        ModelStatus::Available if pending > 0 => {
            Some(format!("Summarizing on this Mac\u{2026} {pending} to go"))
        }
        ModelStatus::Available => None,
        ModelStatus::Unavailable(reason) => {
            Some(format!("On-device summaries unavailable: {reason}"))
        }
    }
}

impl PaneFlowApp {
    pub(crate) fn handle_open_agent_summary(
        &mut self,
        _: &crate::OpenAgentSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same mode gate as the other cockpit overlays.
        if !matches!(self.mode, paneflow_config::schema::AppMode::Cli) {
            return;
        }
        // The config opt-out is a silent no-op, like `review_enabled`.
        if !self.cached_config.agent_summary_enabled() {
            return;
        }
        if self.agent_summary.is_some() {
            self.close_agent_summary_and_restore_focus(window, cx);
            return;
        }
        let mut rows = agent_rows(self.collect_pane_overview_cards(window, cx));
        // Readers are resolved while the panes are certainly alive, on this
        // thread; the blocking reads happen on the worker. A pane that
        // closed between the walk and this lookup settles as failed rather
        // than sitting on "Summarizing…" for as long as the overlay is up.
        let mut jobs: Vec<SummaryJob> = Vec::new();
        for i in summary_order(&rows) {
            let row = &mut rows[i];
            match self.agent_summary_reader(row.surface_id, cx) {
                Some(reader) => jobs.push(SummaryJob {
                    surface_id: row.surface_id,
                    agent: row
                        .agent
                        .map(TerminalAgent::display_name)
                        .unwrap_or("Agent"),
                    state: agent_state_label(row.state.as_ref(), row.exited),
                    reader,
                }),
                None => row.summary = SummaryStatus::Failed("the pane has closed".to_owned()),
            }
        }
        self.agent_summary_generation += 1;
        let generation = self.agent_summary_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.agent_summary = Some(AgentSummaryState {
            rows,
            selected: 0,
            model: ModelStatus::Probing,
            generation,
            cancel: Arc::clone(&cancel),
            scroll: ScrollHandle::new(),
            reveal_selection: true,
        });
        self.agent_summary_focus.focus(window, cx);
        cx.notify();
        self.spawn_agent_summary_worker(generation, cancel, jobs, cx);
    }

    pub(crate) fn close_agent_summary(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.agent_summary.take() {
            state.cancel.store(true, Ordering::Relaxed);
            cx.notify();
        }
    }

    pub(crate) fn close_agent_summary_and_restore_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_agent_summary(cx);
        self.restore_focus_after_close_confirm(window, cx);
    }

    /// Click / Enter on a row. Re-resolved by surface id at activation, so a
    /// pane closed since open is a clean no-op that leaves the overlay up.
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

    fn agent_summary_reader(
        &self,
        surface_id: u64,
        cx: &App,
    ) -> Option<crate::terminal::ScrollbackReader> {
        let loc = find_pane_by_surface_id(&self.workspaces, surface_id, cx)?;
        let pane = loc.pane.read(cx);
        let terminal = pane
            .terminals()
            .into_iter()
            .find(|t| t.entity_id().as_u64() == surface_id)?;
        Some(terminal.read(cx).terminal.scrollback_reader())
    }

    /// Probe the helper once, then run the jobs one at a time, posting each
    /// reply back through the generation check. Every blocking step runs on
    /// a background worker; this task only sequences them.
    fn spawn_agent_summary_worker(
        &mut self,
        generation: u64,
        cancel: Arc<AtomicBool>,
        jobs: Vec<SummaryJob>,
        cx: &mut Context<Self>,
    ) {
        let bin = sidecar::locate();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let availability = match bin.clone() {
                None => ModelAvailability::Unavailable(
                    "this build has no summary helper installed".to_owned(),
                ),
                Some(bin) => {
                    cx.background_spawn(async move { sidecar::probe(&bin) })
                        .await
                }
            };
            let proceed = this
                .update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                    app.agent_summary_apply_model(generation, availability, cx)
                })
                .unwrap_or(false);
            let Some(bin) = bin.filter(|_| proceed) else {
                return;
            };
            for job in jobs {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let reader = job.reader;
                let window = cx
                    .background_spawn(
                        async move { reader.extract_scrollback_window(TAIL_LINES, 0) },
                    )
                    .await;
                let reply = match window {
                    None => SidecarReply::Error("the pane did not answer".to_owned()),
                    Some((text, _, _, _)) => {
                        let tail = prompt::prepare_tail(&text);
                        let fenced = wrap_untrusted(
                            &format!("source=\"surface:{}\"", job.surface_id),
                            &tail,
                        );
                        let request = prompt::request_json(job.agent, job.state, &fenced);
                        let bin = bin.clone();
                        let cancel = Arc::clone(&cancel);
                        cx.background_spawn(
                            async move { sidecar::summarize(&bin, &request, &cancel) },
                        )
                        .await
                    }
                };
                let alive = this
                    .update(cx, |app: &mut Self, cx: &mut Context<Self>| {
                        app.agent_summary_apply_reply(generation, job.surface_id, reply, cx)
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
            }
        })
        .detach();
    }

    /// Record the probe. Returns whether summarisation should proceed: only
    /// for the same open, and only when the model answered.
    pub(crate) fn agent_summary_apply_model(
        &mut self,
        generation: u64,
        availability: ModelAvailability,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(state) = self.agent_summary.as_mut() else {
            return false;
        };
        if state.generation != generation {
            return false;
        }
        let proceed = match availability {
            ModelAvailability::Available => {
                state.model = ModelStatus::Available;
                true
            }
            ModelAvailability::Unavailable(reason) => {
                state.model = ModelStatus::Unavailable(reason);
                for row in &mut state.rows {
                    if row.summary == SummaryStatus::Pending {
                        row.summary = SummaryStatus::Skipped;
                    }
                }
                false
            }
        };
        cx.notify();
        proceed
    }

    /// Record one pane's reply. Returns whether the overlay that asked is
    /// still the one open, so the worker stops after a close or a reopen.
    pub(crate) fn agent_summary_apply_reply(
        &mut self,
        generation: u64,
        surface_id: u64,
        reply: SidecarReply,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(state) = self.agent_summary.as_mut() else {
            return false;
        };
        if state.generation != generation {
            return false;
        }
        if let Some(row) = state.rows.iter_mut().find(|r| r.surface_id == surface_id) {
            row.summary = match reply {
                SidecarReply::Summary(text) => SummaryStatus::Ready(text),
                SidecarReply::Error(reason) => SummaryStatus::Failed(reason),
            };
            cx.notify();
        }
        true
    }

    pub(crate) fn handle_agent_summary_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let Some(state) = self.agent_summary.as_ref() else {
            return;
        };
        let len = state.rows.len();
        let selected = state.selected.min(len.saturating_sub(1));
        match key {
            "escape" => self.close_agent_summary_and_restore_focus(window, cx),
            "enter" => {
                if let Some(sid) = state.rows.get(selected).map(|r| r.surface_id) {
                    self.agent_summary_activate(sid, window, cx);
                }
            }
            "up" | "down" if len > 0 => {
                let next = if key == "down" {
                    (selected + 1).min(len - 1)
                } else {
                    selected.saturating_sub(1)
                };
                if let Some(state) = self.agent_summary.as_mut() {
                    state.selected = next;
                    state.reveal_selection = true;
                }
                cx.notify();
            }
            _ => {}
        }
    }

    pub(crate) fn render_agent_summary(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let viewport = window.viewport_size();
        let overlay_width =
            (f32::from(viewport.width) - 2.0 * OVERLAY_MARGIN).clamp(1.0, OVERLAY_MAX_W);
        let overlay_height = (f32::from(viewport.height) - 2.0 * OVERLAY_MARGIN).max(1.0);
        let Some(state) = self.agent_summary.as_mut() else {
            return div().into_any_element();
        };
        let rows = state.rows.clone();
        let selected = state.selected.min(rows.len().saturating_sub(1));
        let scroll = state.scroll.clone();
        if std::mem::take(&mut state.reveal_selection) && !rows.is_empty() {
            reveal_row(&scroll, scroll_item_index(&rows, selected), window);
        }
        let pending = rows
            .iter()
            .filter(|r| r.summary == SummaryStatus::Pending)
            .count();
        let note = model_note(&state.model, pending);

        let mut body = div()
            .id("agent-summary-scroll")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            .p(px(LIST_PADDING))
            .overflow_y_scroll()
            .track_scroll(&scroll);

        if rows.is_empty() {
            body = body.child(
                div()
                    .py(px(48.))
                    .flex()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(ui.muted)
                    .child(
                        "No agent panes are open - launch one from a tab bar or the Launch Pad \
                         (Cmd+Shift+L)",
                    ),
            );
        } else {
            let mut last_ws = None;
            for (index, row) in rows.iter().enumerate() {
                if last_ws != Some(row.ws_idx) {
                    last_ws = Some(row.ws_idx);
                    body = body.child(
                        div()
                            .flex_none()
                            .pt(px(if index == 0 { 0. } else { 6. }))
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui.text)
                            .child(SharedString::from(row.ws_title.clone())),
                    );
                }
                body = body.child(self.render_agent_summary_row(row, index == selected, ui, cx));
            }
        }

        let card = div()
            .id("agent-summary")
            .occlude()
            .track_focus(&self.agent_summary_focus)
            .on_key_down(cx.listener(Self::handle_agent_summary_key_down))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.close_agent_summary_and_restore_focus(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .w(px(overlay_width))
            .max_h(px(overlay_height))
            .flex()
            .flex_col()
            .bg(ui.overlay)
            .border_1()
            .border_color(ui.border)
            .rounded(px(12.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .px(px(16.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(ui.border)
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui.text)
                            .child("Agent summary"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(SharedString::from(note.unwrap_or_default())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(SharedString::from(format!(
                                "{} agent{}",
                                rows.len(),
                                if rows.len() == 1 { "" } else { "s" }
                            ))),
                    ),
            )
            .child(body)
            .child(
                div()
                    .flex_none()
                    .px(px(16.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(ui.border)
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child(
                        "Arrows select \u{b7} Enter focuses the pane \u{b7} Esc closes \u{b7} \
                         summaries are generated on this Mac and never leave it",
                    ),
            );

        deferred(
            div()
                .id("agent-summary-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(OVERLAY_MARGIN))
                .bg(gpui::hsla(0., 0., 0., 0.4))
                .child(card),
        )
        .with_priority(6)
        .into_any_element()
    }

    fn render_agent_summary_row(
        &self,
        row: &SummaryRow,
        selected: bool,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sid = row.surface_id;
        let (dot, dot_color, status) =
            pane_overview_status_visual(row.state.as_ref(), row.exited, ui);
        let background = if selected { ui.subtle } else { ui.overlay };
        let status_color = crate::terminal::element::ensure_minimum_contrast(
            dot_color,
            background,
            crate::terminal::element::MIN_APCA_CONTRAST,
        );
        let agent_label = row
            .agent
            .map(TerminalAgent::display_name)
            .unwrap_or("Agent");
        let (summary_text, summary_color): (Option<String>, gpui::Hsla) = match &row.summary {
            SummaryStatus::Pending => (Some("Summarizing\u{2026}".to_owned()), ui.muted),
            SummaryStatus::Ready(text) => (Some(text.clone()), ui.text),
            SummaryStatus::Failed(reason) => {
                (Some(format!("Could not summarize: {reason}")), ui.muted)
            }
            SummaryStatus::Skipped if row.exited => {
                (Some("The pane's process has exited.".to_owned()), ui.muted)
            }
            // The header carries the unavailability note once; repeating it
            // per row would drown the list.
            SummaryStatus::Skipped => (None, ui.muted),
        };
        let aria = format!(
            "{}, {agent_label}, {status}{}",
            row.name,
            summary_text
                .as_deref()
                .map(|s| format!(", {s}"))
                .unwrap_or_default()
        );

        let mut shell = div()
            .id(SharedString::from(format!("agent-summary-row-{sid}")))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(aria))
            .aria_selected(selected)
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(ROW_RADIUS))
            .border_1()
            .border_color(if selected { ui.accent } else { ui.border })
            .bg(background)
            .hover(move |style| style.bg(ui.subtle))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.agent_summary_activate(sid, window, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(dot)
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(240.))
                            .truncate()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui.text)
                            .child(SharedString::from(row.name.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(ui.subtle)
                            .text_size(px(10.))
                            .text_color(ui.text)
                            .child(agent_label),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(status_color)
                            .child(status),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(ui.muted)
                            .text_right()
                            .child(SharedString::from(row.tab_title.clone())),
                    ),
            );
        if let Some(text) = summary_text {
            shell = shell.child(
                div()
                    .w_full()
                    .text_size(px(12.))
                    .line_height(px(17.))
                    .text_color(summary_color)
                    .child(SharedString::from(text)),
            );
        }
        shell.into_any_element()
    }
}

fn reveal_row(scroll: &ScrollHandle, item: usize, window: &Window) {
    // GPUI resolves scroll_to_item against the previous frame's viewport;
    // wait for layout so opening and resizing use the new bounds.
    let scroll = scroll.clone();
    window.on_next_frame(move |window, _| {
        scroll.scroll_to_item(item);
        window.refresh();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(
        surface_id: u64,
        ws_idx: usize,
        agent: Option<TerminalAgent>,
        state: Option<AgentState>,
        exited: bool,
    ) -> CardMeta {
        CardMeta {
            surface_id,
            ws_idx,
            ws_title: format!("ws{ws_idx}"),
            tab_idx: 0,
            tab_title: "tab".to_owned(),
            tab_pane_index: 0,
            tab_pane_count: 1,
            name: format!("pane{surface_id}"),
            cwd_label: None,
            agent,
            state,
            cols: 80,
            rows: 24,
            exited,
            is_active: false,
            ws_is_active: false,
            ws_branch: String::new(),
        }
    }

    #[test]
    fn agent_rows_keep_agent_panes_in_traversal_order_and_skip_exited_ones() {
        let cards = vec![
            card(1, 0, None, None, false), // a plain shell: dropped
            card(
                2,
                0,
                Some(TerminalAgent::Codex),
                Some(AgentState::Thinking),
                false,
            ),
            card(3, 1, None, Some(AgentState::WaitingForInput), false), // session, no launcher
            card(
                4,
                1,
                Some(TerminalAgent::ClaudeCode),
                Some(AgentState::Thinking),
                true,
            ),
        ];
        let rows = agent_rows(cards);
        assert_eq!(
            rows.iter().map(|r| r.surface_id).collect::<Vec<_>>(),
            vec![2, 3, 4],
            "visual order is the traversal order, never re-sorted"
        );
        assert_eq!(rows[0].summary, SummaryStatus::Pending);
        assert_eq!(rows[1].summary, SummaryStatus::Pending);
        assert_eq!(
            rows[2].summary,
            SummaryStatus::Skipped,
            "an exited pane is listed but never summarised"
        );
    }

    #[test]
    fn summary_order_is_attention_first_and_stable() {
        let rows = agent_rows(vec![
            card(
                1,
                0,
                Some(TerminalAgent::Codex),
                Some(AgentState::Finished),
                false,
            ),
            card(
                2,
                0,
                Some(TerminalAgent::Codex),
                Some(AgentState::Thinking),
                false,
            ),
            card(
                3,
                0,
                Some(TerminalAgent::Codex),
                Some(AgentState::WaitingForInput),
                false,
            ),
            card(4, 0, Some(TerminalAgent::Codex), None, false),
            card(
                5,
                0,
                Some(TerminalAgent::Codex),
                Some(AgentState::Errored),
                false,
            ),
            card(
                6,
                0,
                Some(TerminalAgent::Codex),
                Some(AgentState::Thinking),
                true,
            ),
        ]);
        let order: Vec<u64> = summary_order(&rows)
            .into_iter()
            .map(|i| rows[i].surface_id)
            .collect();
        assert_eq!(
            order,
            vec![3, 5, 2, 1, 4],
            "waiting/errored, busy, done, idle; exited absent"
        );
    }

    #[test]
    fn scroll_item_index_counts_one_header_per_workspace_group() {
        let rows = agent_rows(vec![
            card(1, 0, Some(TerminalAgent::Codex), None, false),
            card(2, 0, Some(TerminalAgent::Codex), None, false),
            card(3, 2, Some(TerminalAgent::Codex), None, false),
        ]);
        // children: [ws0 header, row1, row2, ws2 header, row3]
        assert_eq!(scroll_item_index(&rows, 0), 1);
        assert_eq!(scroll_item_index(&rows, 1), 2);
        assert_eq!(scroll_item_index(&rows, 2), 4);
        assert_eq!(
            scroll_item_index(&rows, 99),
            4,
            "out of range clamps to the last child"
        );
        assert_eq!(scroll_item_index(&[], 0), 0);
    }

    #[test]
    fn model_note_explains_every_state_but_a_finished_run() {
        assert_eq!(
            model_note(&ModelStatus::Probing, 3).as_deref(),
            Some("Checking the on-device model\u{2026}")
        );
        assert_eq!(
            model_note(&ModelStatus::Available, 2).as_deref(),
            Some("Summarizing on this Mac\u{2026} 2 to go")
        );
        assert_eq!(model_note(&ModelStatus::Available, 0), None);
        assert_eq!(
            model_note(
                &ModelStatus::Unavailable("Apple Intelligence is off".to_owned()),
                0
            )
            .as_deref(),
            Some("On-device summaries unavailable: Apple Intelligence is off")
        );
    }

    /// The overlay's open path must gate on the config switch and the mode,
    /// and every close path must flip the cancel flag: pinned on the source
    /// so a refactor cannot quietly drop the opt-out or leak a helper.
    #[test]
    fn open_gates_on_config_and_close_cancels_the_worker() {
        let src = include_str!("mod.rs");
        let open = src
            .split("pub(crate) fn handle_open_agent_summary(")
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .expect("open handler");
        assert!(open.contains("self.cached_config.agent_summary_enabled()"));
        assert!(open.contains("AppMode::Cli"));
        let close = src
            .split("pub(crate) fn close_agent_summary(")
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .expect("close handler");
        assert!(close.contains("cancel.store(true"));
        assert!(
            src.contains("impl Drop for AgentSummaryState"),
            "a replaced state must cancel its worker too"
        );
    }

    /// The render root dispatches the action and the overlay is mounted
    /// under the same mode gate as the Pane Overview (`main.rs`).
    #[test]
    fn the_action_is_wired_into_the_render_root() {
        let main = include_str!("../../main.rs");
        assert!(main.contains(".on_action(cx.listener(Self::handle_open_agent_summary))"));
        assert!(main.contains("if self.agent_summary.is_some() && in_cli_mode {"));
    }
}
