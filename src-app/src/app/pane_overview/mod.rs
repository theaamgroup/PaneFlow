//! Pane Overview (issue #339): a cross-workspace overlay showing every open
//! terminal pane, grouped by workspace and tab, each bottom-cropped to its
//! last rows so a pane can be identified and jumped to.
//!
//! The overlay is a pure read. Every pane's grid snapshot is published
//! unconditionally by its runtime thread with no visibility gate, so panes in
//! background tabs of inactive workspaces already have live content sitting in
//! `SharedState` - nothing has to be pulled or woken.
//!
//! It is a `deferred(...).with_priority(6)` overlay, a peer of the Attention
//! Queue and Fleet Search, and follows their open/close/key/render shape. It
//! is repainted at ~4 fps by its own timer in `bootstrap.rs` while open; it
//! never subscribes to terminal wakeups (a chatty pane fires at the 4 ms
//! coalescing floor).

pub(crate) mod rows;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, ClickEvent, Context, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, SharedString, Styled, Window, canvas, deferred, div, prelude::*,
    px, rgb,
};

use crate::PaneFlowApp;
use crate::limits::clamp_untrusted_label;
use crate::workspace::Workspace;
use rows::{
    CardMeta, MAX_LIVE_THUMBNAILS, cards_per_row, filter_cards, flat_order, group_cards,
    live_thumbnail_ids,
};

/// Card box. The thumbnail band is 320x132 (`terminal/element/thumbnail.rs`);
/// the rest is 4px padding, a 30px header (name + status) and a 26px footer
/// (breadcrumb + grid size).
const CARD_W: f32 = 328.0;
const CARD_H: f32 = 196.0;
const CARD_GAP: f32 = 12.0;
const CARD_RADIUS: f32 = 10.0;
const GRID_PADDING: f32 = 16.0;

/// Open-overlay state. Cards are never cached here - only the query and the
/// cursor - so live agent state and live terminal content cannot go stale.
pub(crate) struct PaneOverviewState {
    pub query: String,
    pub selected: usize,
    /// Cards per wrapped row, captured from the last render's real pixel
    /// width by a `canvas()` prepaint. Up/Down move by this. Captured rather
    /// than estimated for the same reason `Container::container_size` is:
    /// the old hardcoded 800px container guess in `split.rs` was wrong at
    /// every other window size.
    pub cards_per_row: Rc<Cell<usize>>,
}

impl Default for PaneOverviewState {
    fn default() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            cards_per_row: Rc::new(Cell::new(1)),
        }
    }
}

/// Every terminal pane, in workspace -> tab -> traversal order.
///
/// Walks `ws.tabs()` and NOT `ws.active_tab()`: the Attention Queue and
/// Fleet Search both visit only the active tab, and inheriting that here
/// would hide most of what the overview exists to show. It also avoids
/// `Workspace::collect_panes`, which dedupes with a linear `contains` per
/// pane - `Tab::collect_panes` already dedupes `root` against
/// `saved_layout`, so a zoomed tab yields each pane exactly once.
///
/// A free function over the workspace list (not a `PaneFlowApp` method) so a
/// unit test can drive it without building the app.
pub(crate) fn collect_cards(
    workspaces: &[Workspace],
    active_idx: usize,
    cx: &App,
) -> Vec<CardMeta> {
    let mut cards = Vec::new();
    for (ws_idx, ws) in workspaces.iter().enumerate() {
        let active_tab_idx = ws.active_tab_idx();
        for (tab_idx, tab) in ws.tabs().iter().enumerate() {
            let tab_title = crate::app::sidebar::tab_row_title(tab, tab_idx, cx);
            for pane in tab.collect_panes() {
                let pane_ref = pane.read(cx);
                // Terminals only: markdown and diff panes are omitted.
                let Some(terminal) = pane_ref.active_terminal_opt() else {
                    continue;
                };
                let surface_id = terminal.entity_id().as_u64();
                let state = ws
                    .agent_sessions
                    .values()
                    .find(|s| s.surface_id == Some(surface_id))
                    .map(|s| s.state.clone());
                let view = terminal.read(cx);
                let metrics = view.terminal.session_backend().grid_metrics();
                cards.push(CardMeta {
                    surface_id,
                    ws_idx,
                    ws_title: clamp_untrusted_label(&ws.title),
                    tab_idx,
                    tab_title: clamp_untrusted_label(&tab_title),
                    name: clamp_untrusted_label(&crate::pane::Pane::surface_title(
                        &pane_ref.surface,
                        cx,
                    )),
                    cwd_label: view
                        .terminal
                        .current_cwd
                        .as_deref()
                        .map(std::path::Path::new)
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().to_string()),
                    agent: view.terminal.detected_agent,
                    state,
                    cols: metrics.columns,
                    rows: metrics.screen_lines,
                    exited: view.terminal.exited.is_some(),
                    is_active: ws_idx == active_idx && tab_idx == active_tab_idx,
                });
            }
        }
    }
    cards
}

impl PaneFlowApp {
    pub(crate) fn handle_open_pane_overview(
        &mut self,
        _: &crate::OpenPaneOverview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same mode gate as the other cockpit overlays: a mode switch must
        // not leave cockpit chrome painted over Agents/Review.
        if !matches!(self.mode, paneflow_config::schema::AppMode::Cli) {
            return;
        }
        if self.pane_overview.is_some() {
            self.close_pane_overview_and_restore_focus(window, cx);
            return;
        }
        self.pane_overview = Some(PaneOverviewState::default());
        self.pane_overview_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_pane_overview(&mut self, cx: &mut Context<Self>) {
        self.pane_overview = None;
        cx.notify();
    }

    pub(crate) fn close_pane_overview_and_restore_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_pane_overview(cx);
        // Issue #108: fall back to the empty-workspace placeholder when the
        // workspace we are restoring focus to has no pane.
        let focused = match self.workspaces.get(self.active_idx) {
            Some(ws) => ws.focus_first(window, cx),
            None => false,
        };
        if !focused {
            window.focus(&self.empty_workspace_focus, cx);
        }
    }

    /// Click / Enter on a card. The surface is re-resolved at activation time,
    /// so a pane closed between render and click is a clean no-op.
    pub(crate) fn pane_overview_activate(
        &mut self,
        surface_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.teleport_to_surface(surface_id, window, cx) {
            self.close_pane_overview(cx);
        }
    }

    pub(crate) fn collect_pane_overview_cards(&self, cx: &App) -> Vec<CardMeta> {
        collect_cards(&self.workspaces, self.active_idx, cx)
    }

    pub(crate) fn handle_pane_overview_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let Some(state) = self.pane_overview.as_ref() else {
            return;
        };
        let per_row = state.cards_per_row.get().max(1) as isize;
        let cards = filter_cards(&self.collect_pane_overview_cards(cx), &state.query);
        let order = flat_order(&group_cards(cards));
        let len = order.len();
        let selected = state.selected.min(len.saturating_sub(1));

        let delta = match key {
            "escape" => {
                self.close_pane_overview_and_restore_focus(window, cx);
                return;
            }
            "enter" if len > 0 => {
                if let Some(sid) = order.get(selected).copied() {
                    self.pane_overview_activate(sid, window, cx);
                }
                return;
            }
            "left" => -1,
            "right" => 1,
            "up" => -per_row,
            "down" => per_row,
            "backspace" => {
                if let Some(state) = self.pane_overview.as_mut()
                    && state.query.pop().is_some()
                {
                    state.selected = 0;
                    cx.notify();
                }
                return;
            }
            _ => {
                // Type-to-filter, the theme-picker idiom: printable keys
                // without a command/control/alt modifier extend the query.
                if let Some(ch) = &event.keystroke.key_char
                    && !ch.is_empty()
                    && !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.alt
                    && let Some(state) = self.pane_overview.as_mut()
                {
                    state.query.push_str(ch);
                    state.selected = 0;
                    cx.notify();
                }
                return;
            }
        };
        if let Some(state) = self.pane_overview.as_mut() {
            state.selected = rows::move_selection(selected, len, delta);
        }
        cx.notify();
    }

    pub(crate) fn render_pane_overview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let theme = Arc::new(crate::theme::active_theme());
        let (query, per_row_capture) = match self.pane_overview.as_ref() {
            Some(state) => (state.query.clone(), state.cards_per_row.clone()),
            None => (String::new(), Rc::new(Cell::new(1))),
        };
        let all = self.collect_pane_overview_cards(cx);
        let groups = group_cards(filter_cards(&all, &query));
        let order = flat_order(&groups);
        let live = live_thumbnail_ids(&order, MAX_LIVE_THUMBNAILS);
        let selected_id = self.pane_overview.as_ref().and_then(|s| {
            order
                .get(s.selected.min(order.len().saturating_sub(1)))
                .copied()
        });

        let mut body = div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(18.))
            .p(px(GRID_PADDING))
            // Capture the real grid width each frame so Up/Down move by the
            // number of cards that actually fit on a row.
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        let width = f32::from(bounds.size.width) - 2.0 * GRID_PADDING;
                        per_row_capture.set(cards_per_row(width, CARD_W, CARD_GAP));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );

        if order.is_empty() {
            body = body.child(
                div()
                    .py(px(48.))
                    .flex()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(ui.muted)
                    .child(if all.is_empty() {
                        SharedString::from(
                            "No terminal panes are open - split a pane (Cmd+Shift+D) or open a \
                             new tab (Cmd+Alt+T)",
                        )
                    } else {
                        SharedString::from(format!("No panes match \u{201c}{query}\u{201d}"))
                    }),
            );
        } else {
            for group in &groups {
                let mut section = div().flex().flex_col().gap(px(10.)).child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(ui.text)
                        .child(SharedString::from(group.title.clone())),
                );
                for tab in &group.tabs {
                    let mut row = div().flex().flex_col().gap(px(6.)).child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(SharedString::from(tab.title.clone())),
                    );
                    let mut grid = div().flex().flex_row().flex_wrap().gap(px(CARD_GAP));
                    for card in &tab.cards {
                        grid = grid.child(self.render_pane_overview_card(
                            card,
                            live.contains(&card.surface_id),
                            selected_id == Some(card.surface_id),
                            ui,
                            &theme,
                            cx,
                        ));
                    }
                    row = row.child(grid);
                    section = section.child(row);
                }
                body = body.child(section);
            }
        }

        let filter_line: SharedString = if query.is_empty() {
            SharedString::from("Type to filter")
        } else {
            SharedString::from(format!("Filter: {query}"))
        };

        let card = div()
            .id("pane-overview")
            .occlude()
            .track_focus(&self.pane_overview_focus)
            .on_key_down(cx.listener(Self::handle_pane_overview_key_down))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.close_pane_overview_and_restore_focus(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .w(px(1080.))
            .max_h(px(720.))
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
                            .child("All panes"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(if query.is_empty() { ui.muted } else { ui.text })
                            .child(filter_line),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(SharedString::from(format!("{} panes", order.len()))),
                    ),
            )
            .child(
                div()
                    .id("pane-overview-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(ui.border)
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child(
                        "Arrows select \u{b7} Enter focuses the pane \u{b7} Esc closes \u{b7} type \
                         to filter by name, workspace, tab or agent (fleet search finds pane \
                         contents)",
                    ),
            );

        deferred(
            div()
                .id("pane-overview-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(64.))
                .bg(gpui::hsla(0., 0., 0., 0.4))
                .child(card),
        )
        .with_priority(6)
        .into_any_element()
    }

    fn render_pane_overview_card(
        &self,
        card: &CardMeta,
        live: bool,
        selected: bool,
        ui: crate::theme::UiColors,
        theme: &Arc<crate::theme::TerminalTheme>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sid = card.surface_id;
        let (dot, dot_color, status) = pane_overview_status_visual(card.state.as_ref(), ui);
        let status: SharedString = if card.exited {
            SharedString::from("exited")
        } else {
            status
        };
        let mut shell = div()
            .id(SharedString::from(format!("pane-overview-card-{sid}")))
            .flex_none()
            .w(px(CARD_W))
            .h(px(CARD_H))
            .flex()
            .flex_col()
            .p(px(4.))
            .rounded(px(CARD_RADIUS))
            .border_1()
            .border_color(if selected { ui.accent } else { ui.border })
            .bg(if selected { ui.subtle } else { ui.overlay })
            .hover(move |style| style.bg(ui.subtle))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.pane_overview_activate(sid, window, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .h(px(30.))
                    .flex_none()
                    .px(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(dot)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(ui.text)
                            .child(SharedString::from(card.name.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(if card.exited { ui.muted } else { dot_color })
                            .child(status),
                    ),
            );

        let band = div()
            .flex_none()
            .w(px(crate::terminal::element::THUMBNAIL_BAND_W))
            .h(px(crate::terminal::element::THUMBNAIL_BAND_H))
            .overflow_hidden()
            .rounded(px(6.))
            .bg(theme.background);
        let band = if live {
            match self.pane_overview_backend(sid, cx) {
                Some(backend) => band.child(crate::terminal::element::TerminalThumbnail::new(
                    backend,
                    theme.clone(),
                )),
                None => band,
            }
        } else {
            band.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child("Preview paused"),
            )
        };
        shell = shell.child(if card.exited { band.opacity(0.5) } else { band });

        let breadcrumb = if card.is_active {
            format!("\u{25cf} {} \u{203a} {}", card.ws_title, card.tab_title)
        } else {
            format!("{} \u{203a} {}", card.ws_title, card.tab_title)
        };
        shell
            .child(
                div()
                    .h(px(26.))
                    .flex_none()
                    .px(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(6.))
                    .text_size(px(10.))
                    .text_color(ui.muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(breadcrumb)),
                    )
                    .child(div().flex_none().child(SharedString::from(format!(
                        "{}\u{d7}{}",
                        card.cols, card.rows
                    )))),
            )
            .into_any_element()
    }

    /// The backend for one card's thumbnail, resolved by surface id so a pane
    /// closed since `collect_pane_overview_cards` yields `None` rather than a
    /// stale handle. The thumbnail resolves its own font internally.
    fn pane_overview_backend(
        &self,
        surface_id: u64,
        cx: &App,
    ) -> Option<crate::terminal::TerminalSessionBackend> {
        let loc =
            crate::app::ipc_handler::find_pane_by_surface_id(&self.workspaces, surface_id, cx)?;
        let terminal = loc.pane.read(cx).active_terminal_opt()?.clone();
        Some(terminal.read(cx).terminal.session_backend())
    }
}

/// Status dot, colour and label for one card.
///
/// Colours come from the sidebar's grammar (`agent_summary_visual`) so the
/// two surfaces cannot fork: amber = needs input, `agent_error` = errored,
/// `agent_stalled` = stalled, muted = thinking, blue = finished, nothing =
/// idle.
fn pane_overview_status_visual(
    state: Option<&crate::ai_types::AgentState>,
    ui: crate::theme::UiColors,
) -> (AnyElement, gpui::Hsla, SharedString) {
    use crate::ai_types::AgentState;
    let (color, label) = match state {
        Some(AgentState::WaitingForInput) => (rgb(0xFBBF24).into(), "Input"),
        Some(AgentState::Errored) => (ui.agent_error, "Error"),
        Some(AgentState::Stalled) => (ui.agent_stalled, "Stalled"),
        Some(AgentState::Thinking) => (ui.muted, "Working"),
        Some(AgentState::Finished) => (rgb(0x83C3FF).into(), "Done"),
        None => (ui.muted.opacity(0.0), ""),
    };
    let dot = div()
        .flex_none()
        .w(px(6.))
        .h(px(6.))
        .rounded_full()
        .bg(color)
        .into_any_element();
    (dot, color, SharedString::from(label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::Pane;

    /// The walk pins §6 of the design: every tab, not just the active one.
    /// The Attention Queue and Fleet Search both visit only `ws.active_tab()`,
    /// and regressing to that walk would hide most of what the overview
    /// exists to show.
    #[gpui::test]
    fn overview_walks_every_tab_not_just_the_active_one(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext;

        let cx = cx.add_empty_window();
        let make_pane = |cx: &mut gpui::VisualTestContext| {
            let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
            let surface_id = terminal.entity_id().as_u64();
            let pane = cx.new(|cx| Pane::new(terminal, 1, cx));
            (pane, surface_id)
        };
        let (visible_pane, visible_sid) = make_pane(cx);
        let (hidden_pane, hidden_sid) = make_pane(cx);
        let (other_pane, other_sid) = make_pane(cx);

        let mut ws = Workspace::with_layout_and_id(
            1,
            "alpha",
            std::path::PathBuf::new(),
            crate::layout::LayoutTree::Leaf(visible_pane),
        );
        assert!(ws.open_tab(crate::workspace::Tab::new(
            "background",
            Some(crate::layout::LayoutTree::Leaf(hidden_pane)),
        )));
        // Make tab 0 the visible one so the second tab is genuinely hidden.
        ws.set_active_tab(0);
        let other = Workspace::with_layout_and_id(
            2,
            "beta",
            std::path::PathBuf::new(),
            crate::layout::LayoutTree::Leaf(other_pane),
        );
        let workspaces = vec![ws, other];

        let cards = cx.update(|_, cx| collect_cards(&workspaces, 0, cx));
        let ids: Vec<u64> = cards.iter().map(|c| c.surface_id).collect();
        assert_eq!(
            ids,
            vec![visible_sid, hidden_sid, other_sid],
            "workspace -> tab -> traversal order, including the background tab"
        );

        let hidden = cards
            .iter()
            .find(|c| c.surface_id == hidden_sid)
            .expect("the background tab's pane is listed");
        assert_eq!((hidden.ws_idx, hidden.tab_idx), (0, 1));
        assert_eq!(hidden.ws_title, "alpha");
        assert_eq!(hidden.tab_title, "background");
        assert!(!hidden.is_active, "a background tab is not the active one");
        assert!(hidden.state.is_none(), "no session means idle");

        let visible = cards
            .iter()
            .find(|c| c.surface_id == visible_sid)
            .expect("the visible pane is listed");
        assert!(visible.is_active);
        assert_eq!(
            (visible.cols, visible.rows),
            (80, 24),
            "grid from the pane itself"
        );

        let groups = group_cards(cards);
        assert_eq!(groups.len(), 2, "one section per workspace");
        assert_eq!(groups[0].tabs.len(), 2, "one row per tab, hidden included");
        assert_eq!(groups[1].title, "beta");
    }
}
