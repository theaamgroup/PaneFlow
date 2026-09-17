//! Rendering for the fleet agent summary overlay (issue #576).
//!
//! A single scrollable list, grouped by workspace, one row per agent pane:
//! pane identity on the left, the model's one-line summary filling the rest.
//! Deliberately not a grid - the summary text is the content here, and a
//! Pane Overview-style card would leave no room for it.

use gpui::{
    AnyElement, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, Styled,
    Window, deferred, div, prelude::*, px,
};

use super::{SummaryEntry, SummaryStatus};
use crate::PaneFlowApp;

const OVERLAY_MARGIN: f32 = 24.0;
const MAX_OVERLAY_WIDTH: f32 = 920.0;

impl PaneFlowApp {
    pub(crate) fn render_agent_summary(
        &mut self,
        window: &Window,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let viewport = window.viewport_size();
        let overlay_width =
            (f32::from(viewport.width) - 2.0 * OVERLAY_MARGIN).clamp(1.0, MAX_OVERLAY_WIDTH);
        let overlay_height = (f32::from(viewport.height) - 2.0 * OVERLAY_MARGIN).max(1.0);

        let Some(state) = self.agent_summary.as_ref() else {
            return div().into_any_element();
        };
        let entries = state.entries.clone();
        let selected = state.selected;
        let progress = state.progress();
        let unavailable = state.unavailable.clone();

        let mut body = div()
            .id("agent-summary-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();

        if let Some(message) = unavailable.as_deref() {
            // One explanation, not N identical row failures. This is the
            // pre-macOS-26 / Apple-Intelligence-off path, and it must read as
            // a capability notice rather than an error.
            body = body.child(
                div()
                    .py(px(40.))
                    .px(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .items_center()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(ui.text)
                            .child(SharedString::from(message.to_string())),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child("Summaries need Apple Intelligence on macOS 26 or later."),
                    ),
            );
        } else if entries.is_empty() {
            body = body.child(
                div()
                    .py(px(40.))
                    .flex()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(ui.muted)
                    .child("No agent panes are running."),
            );
        } else {
            let mut last_ws: Option<usize> = None;
            for (index, entry) in entries.iter().enumerate() {
                if last_ws != Some(entry.ws_idx) {
                    last_ws = Some(entry.ws_idx);
                    body = body.child(
                        div()
                            .px(px(16.))
                            .pt(px(14.))
                            .pb(px(4.))
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui.muted)
                            .child(SharedString::from(entry.ws_title.clone())),
                    );
                }
                body = body.child(self.render_agent_summary_row(entry, index == selected, cx));
            }
        }

        let header_right = progress.unwrap_or_else(|| {
            let count = entries.len();
            format!("{count} agent{}", if count == 1 { "" } else { "s" })
        });

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
                            .child("What the agents are doing"),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child(SharedString::from(header_right)),
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
                         summarised on-device, nothing leaves this Mac",
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
        entry: &SummaryEntry,
        selected: bool,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let ui = crate::theme::ui_colors();
        let surface_id = entry.surface_id;

        let agent_label = entry
            .agent
            .map(|a| a.display_name().to_string())
            .unwrap_or_else(|| "shell".into());

        let summary_text = super::row_summary_text(&entry.status).to_string();
        let summary_color = match &entry.status {
            SummaryStatus::Ready(_) => ui.text,
            _ => ui.muted,
        };

        div()
            .id(("agent-summary-row", surface_id as usize))
            .px(px(16.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .border_l_2()
            .border_color(if selected {
                ui.accent
            } else {
                gpui::transparent_black()
            })
            .when(selected, |d| d.bg(ui.subtle))
            .hover(|d| d.bg(ui.subtle))
            .on_mouse_down(MouseButton::Left, {
                cx.listener(move |this, _, window, cx| {
                    this.agent_summary_activate(surface_id, window, cx);
                })
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(ui.text)
                            .child(SharedString::from(entry.name.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(ui.muted)
                            .child(SharedString::from(agent_label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(ui.muted)
                            .child(SharedString::from(entry.tab_title.clone())),
                    )
                    .when(entry.exited, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(10.))
                                .text_color(ui.muted)
                                .child("exited"),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(summary_color)
                    .child(SharedString::from(summary_text)),
            )
            .into_any_element()
    }
}
