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

use std::sync::Arc;

use gpui::{
    AnyElement, App, ClickEvent, Context, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, ScrollHandle, SharedString, Styled, Window, deferred, div,
    prelude::*, px, rgb,
};

use crate::PaneFlowApp;
use crate::limits::clamp_untrusted_label;
use crate::workspace::Workspace;
use rows::{
    CardMeta, GridRow, MAX_LIVE_THUMBNAILS, cards_per_row, filter_cards, flat_order, grid_rows,
    group_cards, initial_selection, live_thumbnail_ids, selected_row,
};

/// Compact previews keep eight terminal rows at the existing readable font size.
/// Include padding and borders in the card dimensions.
const CARD_W: f32 = crate::terminal::element::THUMBNAIL_BAND_W + 10.0;
const CARD_H: f32 = crate::terminal::element::THUMBNAIL_BAND_H + 66.0;
const CARD_GAP: f32 = 10.0;
const CARD_RADIUS: f32 = 8.0;
const GRID_PADDING: f32 = 16.0;
const OVERVIEW_MARGIN: f32 = 24.0;

/// Per-card render flags, decided by `render_pane_overview` once per frame.
#[derive(Clone, Copy)]
struct CardFlags {
    width: f32,
    /// Inside the `MAX_LIVE_THUMBNAILS` prefix: paint a live thumbnail.
    live: bool,
    /// Under the moving selection cursor.
    selected: bool,
    /// The pane that held focus when the overlay opened (spec §7.1).
    current: bool,
}

/// Open-overlay state. Cards are never cached here - only the query and the
/// cursor - so live agent state and live terminal content cannot go stale.
pub(crate) struct PaneOverviewState {
    pub query: String,
    pub selected: usize,
    /// The pane that held focus when the overlay opened - the card that
    /// carries the "current" marker. Captured once at open: while the overlay
    /// is up its own focus handle holds focus, so it cannot be re-derived on
    /// render. `None` when no terminal pane was focused.
    pub current: Option<u64>,
    /// Derived from the same viewport width used to size the overlay.
    pub cards_per_row: usize,
    viewport_size: gpui::Size<gpui::Pixels>,
    pub scroll: ScrollHandle,
    /// Only follow keyboard movement/open/filter; ordinary scrolling stays put.
    pub reveal_selection: bool,
}

impl Default for PaneOverviewState {
    fn default() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            current: None,
            cards_per_row: 1,
            viewport_size: gpui::Size::default(),
            scroll: ScrollHandle::new(),
            reveal_selection: true,
        }
    }
}

/// Every terminal pane, in workspace -> tab -> traversal order.
///
/// Walks `ws.tabs()` and NOT `ws.active_tab()`: the Attention Queue and
/// Fleet Search both visit only the active tab, and inheriting that here
/// would hide most of what the overview exists to show. It also avoids
/// `Workspace::collect_panes`, which dedupes with a linear `contains` per
/// pane. The saved layout is authoritative during zoom: walking it preserves
/// split order instead of moving the zoomed pane to the front of the list.
///
/// A free function over the workspace list (not a `PaneFlowApp` method) so a
/// unit test can drive it without building the app.
///
/// Takes a `&Window` because `is_active` is the FOCUSED pane, and focus is a
/// window property: `LayoutTree::focused_pane(window, cx)`
/// (`layout/queries.rs`) is the accessor, and it walks the tree testing each
/// leaf's `focus_handle(cx).is_focused(window)`. There is no
/// `Workspace::focused_pane`; go through `ws.active_tab().root`.
pub(crate) fn collect_cards(
    workspaces: &[Workspace],
    active_idx: usize,
    window: &Window,
    cx: &App,
) -> Vec<CardMeta> {
    let mut cards = Vec::new();
    for (ws_idx, ws) in workspaces.iter().enumerate() {
        let ws_is_active = ws_idx == active_idx;
        let active_tab_idx = ws.active_tab_idx();
        // The one pane that is "current": only the active workspace's active
        // tab can hold focus, so resolve it once per workspace and only
        // there. `None` when focus sits in chrome, in a markdown or diff
        // pane, or on the empty-workspace placeholder.
        let focused_pane = if ws_is_active {
            ws.active_tab()
                .root
                .as_ref()
                .and_then(|root| root.focused_pane(window, cx))
        } else {
            None
        };
        for (tab_idx, tab) in ws.tabs().iter().enumerate() {
            let tab_title = crate::app::sidebar::tab_row_title(tab, tab_idx, cx);
            let panes: Vec<_> = tab
                .saved_layout
                .as_ref()
                .or(tab.root.as_ref())
                .map(|tree| tree.collect_leaves())
                .unwrap_or_default()
                .into_iter()
                .filter(|pane| pane.read(cx).active_terminal_opt().is_some())
                .collect();
            let tab_pane_count = panes.len();
            for (tab_pane_index, pane) in panes.into_iter().enumerate() {
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
                    tab_pane_index,
                    tab_pane_count,
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
                    // The focused pane, not merely a pane of the active tab:
                    // `Entity<Pane>` compares by entity id, so this is true
                    // for exactly one card at most.
                    is_active: tab_idx == active_tab_idx && focused_pane.as_ref() == Some(&pane),
                    ws_is_active,
                    ws_branch: ws.git_branch.clone(),
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
        // Product decision (2026-09-03): open on the current pane's card, so
        // Esc then Enter is a no-op round trip. `is_active` is the focused
        // pane, resolved while the terminal still holds focus - i.e. BEFORE
        // the overlay takes it below.
        let cards = self.collect_pane_overview_cards(window, cx);
        let current = cards.iter().find(|c| c.is_active).map(|c| c.surface_id);
        let order = flat_order(&group_cards(cards));
        self.pane_overview = Some(PaneOverviewState {
            selected: initial_selection(&order, current),
            current,
            ..PaneOverviewState::default()
        });
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

    pub(crate) fn collect_pane_overview_cards(&self, window: &Window, cx: &App) -> Vec<CardMeta> {
        collect_cards(&self.workspaces, self.active_idx, window, cx)
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
        let per_row = state.cards_per_row.max(1);
        let cards = filter_cards(&self.collect_pane_overview_cards(window, cx), &state.query);
        let groups = group_cards(cards);
        let order = flat_order(&groups);
        let visual_rows = grid_rows(&groups, per_row);
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
            "up" | "down" => {
                if let Some(state) = self.pane_overview.as_mut() {
                    state.selected = rows::move_vertical(&visual_rows, selected, key == "down");
                    state.reveal_selection = true;
                }
                cx.notify();
                return;
            }
            "backspace" => {
                if let Some(state) = self.pane_overview.as_mut()
                    && state.query.pop().is_some()
                {
                    state.selected = 0;
                    state.reveal_selection = true;
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
                    state.reveal_selection = true;
                    cx.notify();
                }
                return;
            }
        };
        if let Some(state) = self.pane_overview.as_mut() {
            state.selected = rows::move_selection(selected, len, delta);
            state.reveal_selection = true;
        }
        cx.notify();
    }

    pub(crate) fn render_pane_overview(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let theme = Arc::new(crate::theme::active_theme());
        let viewport = window.viewport_size();
        let overlay_width = (f32::from(viewport.width) - 2.0 * OVERVIEW_MARGIN).max(1.0);
        let overlay_height = (f32::from(viewport.height) - 2.0 * OVERVIEW_MARGIN).max(1.0);
        let grid_width = (overlay_width - 2.0 - 2.0 * GRID_PADDING).max(1.0);
        let card_width = CARD_W.min(grid_width);
        let columns = cards_per_row(grid_width, card_width, CARD_GAP);
        let Some(state) = self.pane_overview.as_mut() else {
            return div().into_any_element();
        };
        if state.viewport_size != viewport {
            state.reveal_selection = true;
        }
        state.viewport_size = viewport;
        state.cards_per_row = columns;
        let query = state.query.clone();
        let scroll = state.scroll.clone();
        // Note: while the overlay is open its own focus handle holds focus,
        // so `is_active` is false for every card on re-render. That is fine -
        // the "current" marker is decided once, at open, and carried below.
        let all = self.collect_pane_overview_cards(window, cx);
        let groups = group_cards(filter_cards(&all, &query));
        let order = flat_order(&groups);
        let live = live_thumbnail_ids(&order, MAX_LIVE_THUMBNAILS);
        let selected_id = self.pane_overview.as_ref().and_then(|s| {
            order
                .get(s.selected.min(order.len().saturating_sub(1)))
                .copied()
        });
        let current_id = self.pane_overview.as_ref().and_then(|s| s.current);

        let visual_rows = grid_rows(&groups, columns);
        if let Some(state) = self.pane_overview.as_mut()
            && state.reveal_selection
        {
            if let Some(row) = selected_id.and_then(|sid| selected_row(&visual_rows, sid)) {
                reveal_overview_row(&scroll, row, window);
            } else {
                scroll.set_offset(gpui::point(px(0.), px(0.)));
            }
            state.reveal_selection = false;
        }
        let mut body = overview_grid_body(&scroll);

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
            for row in visual_rows {
                match row {
                    GridRow::Workspace(group) => {
                        // Section header: title, branch, active marker (spec §7.1).
                        let mut header = div()
                            .flex_none()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(ui.text)
                                    .child(SharedString::from(group.title.clone())),
                            );
                        if !group.branch.is_empty() {
                            header = header.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(ui.muted)
                                    .child(SharedString::from(group.branch.clone())),
                            );
                        }
                        if group.is_active {
                            header = header.child(
                                div()
                                    .px(px(6.))
                                    .py(px(1.))
                                    .rounded(px(4.))
                                    .bg(ui.subtle)
                                    .text_size(px(10.))
                                    .text_color(ui.text)
                                    .child("active"),
                            );
                        }
                        body = body.child(header);
                    }
                    GridRow::Cards(cards) => {
                        let mut row = overview_grid_row();
                        for card in cards {
                            row = row.child(self.render_pane_overview_card(
                                card,
                                CardFlags {
                                    width: card_width,
                                    live: live.contains(&card.surface_id),
                                    selected: selected_id == Some(card.surface_id),
                                    current: current_id == Some(card.surface_id),
                                },
                                ui,
                                &theme,
                                cx,
                            ));
                        }
                        body = body.child(row);
                    }
                }
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
            .w(px(overlay_width))
            .h(px(overlay_height))
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
                .pt(px(OVERVIEW_MARGIN))
                .bg(gpui::hsla(0., 0., 0., 0.4))
                .child(card),
        )
        .with_priority(6)
        .into_any_element()
    }

    fn render_pane_overview_card(
        &self,
        card: &CardMeta,
        flags: CardFlags,
        ui: crate::theme::UiColors,
        theme: &Arc<crate::theme::TerminalTheme>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let CardFlags {
            width,
            live,
            selected,
            current,
        } = flags;
        let sid = card.surface_id;
        let (dot, dot_color, status) = pane_overview_status_visual(card.state.as_ref(), ui);
        // The "current" marker (spec §7.1): the pane that held focus when the
        // overlay opened. It does not move with the selection.
        let current_chip = current.then(|| {
            div()
                .flex_none()
                .px(px(5.))
                .py(px(1.))
                .rounded(px(4.))
                .bg(ui.subtle)
                .text_size(px(9.))
                .text_color(ui.text)
                .child("current")
        });
        let status: SharedString = if card.exited {
            SharedString::from("exited")
        } else {
            status
        };
        let mut shell = div()
            .id(SharedString::from(format!("pane-overview-card-{sid}")))
            .flex_none()
            .w(px(width))
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
                    .children(current_chip)
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
            .w_full()
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
                            .child(SharedString::from(card.tab_title.clone())),
                    )
                    .children((card.tab_pane_count > 1).then(|| {
                        div().flex_none().child(SharedString::from(format!(
                            "pane {}/{}",
                            card.tab_pane_index + 1,
                            card.tab_pane_count
                        )))
                    }))
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

fn overview_grid_body(scroll: &ScrollHandle) -> gpui::Stateful<gpui::Div> {
    div()
        .id("pane-overview-scroll")
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(CARD_GAP))
        .p(px(GRID_PADDING))
        .overflow_y_scroll()
        .track_scroll(scroll)
}

fn overview_grid_row() -> gpui::Div {
    div().flex_none().flex().flex_row().gap(px(CARD_GAP))
}

fn reveal_overview_row(scroll: &ScrollHandle, row: usize, window: &Window) {
    // GPUI resolves scroll_to_item against the previous frame's viewport.
    // Wait for layout so opening and resizing use the new scroll bounds.
    let scroll = scroll.clone();
    window.on_next_frame(move |window, _| {
        scroll.scroll_to_item(row);
        window.refresh();
    });
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

    fn terminal_pane(cx: &mut gpui::VisualTestContext) -> (gpui::Entity<Pane>, u64) {
        use gpui::AppContext;
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        let surface_id = terminal.entity_id().as_u64();
        (cx.new(|cx| Pane::new(terminal, 1, cx)), surface_id)
    }

    #[gpui::test]
    fn overview_numbering_stays_in_layout_order_while_zoomed(cx: &mut gpui::TestAppContext) {
        use crate::layout::{LayoutTree, SplitDirection};
        use gpui::Focusable;
        let cx = cx.add_empty_window();
        let (first, first_sid) = terminal_pane(cx);
        let (second, second_sid) = terminal_pane(cx);
        let tree =
            LayoutTree::from_panes_equal(SplitDirection::Vertical, vec![first, second.clone()])
                .expect("split layout");
        let mut workspaces = vec![Workspace::with_layout_and_id(
            1,
            "split",
            std::path::PathBuf::new(),
            tree,
        )];
        cx.update(|window, cx| second.read(cx).focus_handle(cx).focus(window, cx));
        let collect = |workspaces: &[Workspace], cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| collect_cards(workspaces, 0, window, cx))
                .into_iter()
                .map(|card| {
                    (
                        card.surface_id,
                        card.tab_pane_index,
                        card.tab_pane_count,
                        card.is_active,
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = collect(&workspaces, cx);
        assert_eq!(
            before,
            vec![(first_sid, 0, 2, false), (second_sid, 1, 2, true)]
        );
        let tab = workspaces[0].active_tab_mut();
        tab.saved_layout = tab.root.take();
        tab.root = Some(LayoutTree::Leaf(second));
        assert_eq!(
            collect(&workspaces, cx),
            before,
            "zoom must not reorder or renumber cards"
        );
        cx.update(|_, cx| {
            workspaces[0].exit_zoom(cx);
        });
        assert_eq!(
            collect(&workspaces, cx),
            before,
            "unzoom preserves the same identities"
        );
    }

    #[gpui::test]
    fn overview_numbering_ignores_nonterminal_surfaces(cx: &mut gpui::TestAppContext) {
        use crate::layout::{LayoutTree, SplitDirection};
        use crate::pane::PaneSurface;
        use gpui::AppContext;
        let cx = cx.add_empty_window();
        let dir = tempfile::tempdir().expect("fixture directory");
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# Notes").expect("markdown fixture");
        let markdown = cx.new(|cx| crate::markdown::MarkdownView::build(path, cx));
        let markdown = cx.new(|cx| Pane::new_with_surface(PaneSurface::Markdown(markdown), 1, cx));
        let diff =
            cx.new(|cx| crate::diff::DiffView::build(dir.path().to_path_buf(), vec![], None, cx));
        let diff = cx.new(|cx| Pane::new_with_surface(PaneSurface::Diff(diff), 1, cx));
        let (first, first_sid) = terminal_pane(cx);
        let (second, second_sid) = terminal_pane(cx);
        let tree = LayoutTree::from_panes_equal(
            SplitDirection::Vertical,
            vec![markdown.clone(), first, diff.clone(), second.clone()],
        )
        .expect("mixed layout");
        let mut workspaces = vec![Workspace::with_layout_and_id(
            1,
            "mixed",
            dir.path().to_path_buf(),
            tree,
        )];
        let cards = cx.update(|window, cx| collect_cards(&workspaces, 0, window, cx));
        let labels: Vec<_> = cards
            .iter()
            .map(|card| (card.surface_id, card.tab_pane_index, card.tab_pane_count))
            .collect();
        assert_eq!(labels, vec![(first_sid, 0, 2), (second_sid, 1, 2)]);
        workspaces[0].active_tab_mut().root = LayoutTree::from_panes_equal(
            SplitDirection::Vertical,
            vec![markdown.clone(), diff.clone(), second],
        );
        let cards = cx.update(|window, cx| collect_cards(&workspaces, 0, window, cx));
        assert_eq!(cards.len(), 1);
        assert_eq!((cards[0].tab_pane_index, cards[0].tab_pane_count), (0, 1));
        workspaces[0].active_tab_mut().root =
            LayoutTree::from_panes_equal(SplitDirection::Vertical, vec![markdown, diff]);
        assert!(
            cx.update(|window, cx| collect_cards(&workspaces, 0, window, cx))
                .is_empty()
        );
    }

    #[gpui::test]
    fn compact_grid_fits_twenty_cards_and_scrolls_to_keyboard_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::{AppContext, AvailableSpace, Render, point, size};
        struct TestGrid {
            scroll: ScrollHandle,
            width: f32,
            height: f32,
            columns: usize,
        }
        impl Render for TestGrid {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let mut body = overview_grid_body(&self.scroll).child(div().flex_none().h(px(22.)));
                for row_index in 0..4 {
                    let mut row = overview_grid_row();
                    for column in 0..self.columns {
                        let index = row_index * self.columns + column;
                        row = row.child(
                            div()
                                .flex_none()
                                .w(px(CARD_W))
                                .h(px(CARD_H))
                                .debug_selector(move || format!("overview-test-card-{index}")),
                        );
                    }
                    body = body.child(row);
                }
                div()
                    .w(px(self.width))
                    .h(px(self.height))
                    .flex()
                    .flex_col()
                    .child(body)
            }
        }
        let cx = cx.add_empty_window();
        let scroll = ScrollHandle::new();
        let width = 1440.0 - 2.0 * OVERVIEW_MARGIN - 2.0;
        let columns = cards_per_row(width - 2.0 * GRID_PADDING, CARD_W, CARD_GAP);
        assert_eq!(columns, 5);
        let grid = cx.new(|_| TestGrid {
            scroll: scroll.clone(),
            width,
            height: 750.0,
            columns,
        });
        let draw = |height: f32, cx: &mut gpui::VisualTestContext| {
            grid.update(cx, |grid, cx| {
                grid.height = height;
                cx.notify();
            });
            cx.draw(
                point(px(0.), px(0.)),
                size(
                    AvailableSpace::Definite(px(width)),
                    AvailableSpace::Definite(px(height)),
                ),
                |_, _| grid.clone().into_any_element(),
            );
        };
        // 750 px leaves 150 px of a 900 px window for margins and chrome.
        draw(750.0, cx);
        let first = cx.debug_bounds("overview-test-card-0").expect("first card");
        let fifth = cx.debug_bounds("overview-test-card-4").expect("fifth card");
        let last = cx.debug_bounds("overview-test-card-19").expect("last card");
        assert_eq!(first.top(), fifth.top());
        assert!(last.bottom() <= px(750.0));
        assert!(last.right() <= px(width));
        assert_eq!(first.size.height, px(CARD_H));
        assert_eq!(scroll.offset().y, px(0.));
        // A shorter window must scroll to the selected row without shrinking cards.
        cx.update(|window, _| reveal_overview_row(&scroll, 4, window));
        draw(360.0, cx);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(360.0, cx);
        let last = cx
            .debug_bounds("overview-test-card-19")
            .expect("selected row");
        assert!(scroll.offset().y < px(0.));
        assert!(last.top() >= px(0.));
        assert!(last.bottom() <= px(360.0));
        assert_eq!(last.size.height, px(CARD_H));
    }

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

        // Focus the visible pane: `is_active` is the FOCUSED pane, not "any
        // pane of the active tab".
        cx.update(|window, cx| {
            assert!(workspaces[0].focus_first(window, cx));
        });
        let cards = cx.update(|window, cx| collect_cards(&workspaces, 0, window, cx));
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
        assert!(hidden.ws_is_active, "but its workspace is the active one");
        assert!(hidden.state.is_none(), "no session means idle");

        let visible = cards
            .iter()
            .find(|c| c.surface_id == visible_sid)
            .expect("the visible pane is listed");
        assert!(visible.is_active, "the focused pane is the current card");
        assert_eq!(
            (visible.cols, visible.rows),
            (80, 24),
            "grid from the pane itself"
        );
        let other = cards
            .iter()
            .find(|c| c.surface_id == other_sid)
            .expect("the other workspace's pane is listed");
        assert!(!other.is_active);
        assert!(!other.ws_is_active, "workspace 1 is not the active one");
        assert_eq!(
            cards.iter().filter(|c| c.is_active).count(),
            1,
            "exactly one card is current"
        );

        let groups = group_cards(cards);
        assert_eq!(groups.len(), 2, "one section per workspace");
        assert_eq!(
            groups[0].tabs.len(),
            2,
            "tab identity survives packing, hidden included"
        );
        assert!(groups[0].is_active, "the header marks the active workspace");
        assert_eq!(groups[1].title, "beta");
        assert!(!groups[1].is_active);
    }
}
