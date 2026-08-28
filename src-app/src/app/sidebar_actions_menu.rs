//! Bottom-of-sidebar mode tabs. The Agents and Review sidebars share one
//! persistent mode switch. There is no Settings affordance here: it moved to
//! the macOS menu bar and the title-bar profile menu, finishing issue #105.
//! The strip itself disappears when `review_enabled` is off, because one
//! reachable mode is not a choice.

use crate::app::sidebar::SIDEBAR_ROW_LINE_HEIGHT;
use crate::ui_primitives::{ROW_RADIUS, squircle_skin};

use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Styled, div, prelude::*, px, svg,
};

use crate::PaneFlowApp;

impl PaneFlowApp {
    /// "IPC offline" notice at the bottom of the sidebar - the cockpit home
    /// of the title-bar IPC pill. Purely informational, like the original
    /// pill: no click handler. `None` while the IPC server is up.
    pub(crate) fn render_sidebar_ipc_banner(&self, _cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.ipc_status.state() != crate::ipc::IpcState::Disabled {
            return None;
        }
        let ui = crate::theme::ui_colors();
        Some(
            div()
                .id("sidebar-ipc-banner")
                .mx(px(6.))
                .mb(px(2.))
                .px(px(8.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(ui.border)
                .bg(ui.subtle)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(
                    svg()
                        .size(px(14.))
                        .flex_none()
                        .path("icons/triangle-alert.svg")
                        .text_color(ui.muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(ui.text)
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child("IPC offline"),
                )
                .into_any_element(),
        )
    }

    /// Render the bottom footer: the interface mode tabs, and the IPC banner
    /// above them when the socket is down. The mode switch stays visible after
    /// selection so the footer reads as primary navigation.
    ///
    /// There is no Settings affordance here any more (issue #105 finished the
    /// job it started): Settings lives on the macOS menu bar under
    /// `PaneFlow ▸ Settings…` and in the title-bar profile menu. A rail footer
    /// is navigation between surfaces; a global preferences window is not one
    /// of those surfaces.
    pub(crate) fn render_sidebar_settings_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        use paneflow_config::schema::AppMode;

        let ui = crate::theme::ui_colors();
        let mode = self.mode;

        // Skinned exactly like a workspace card, on the rail's continuous
        // corner.
        let active_bg = crate::app::constants::sidebar_tab_active_background();
        let hover_bg = crate::app::constants::sidebar_tab_hover_background();

        type Activate = Box<dyn Fn(&mut PaneFlowApp, &mut gpui::Window, &mut Context<PaneFlowApp>)>;
        let mode_button =
            |id: &'static str, label: &'static str, is_active: bool, activate: Activate| {
                // Equal-width segments: with the Settings utility gone from
                // this row, the two surfaces split it evenly.
                //
                // Same grammar as a workspace card, down to the typography: one
                // text size, one weight, one color in every state. Exactly one
                // segment rests filled - the current mode - and the others are
                // pure hover affordances one tint step below it, so the fill
                // carries the selection and the label never has to.
                let button = squircle_skin(
                    div()
                        .id(id)
                        .flex_1()
                        .h(px(30.))
                        .min_w_0()
                        .px(px(2.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_center(),
                    format!("{id}-group"),
                    ROW_RADIUS,
                    is_active.then_some(active_bg),
                    (!is_active).then_some(hover_bg),
                )
                .child(
                    div()
                        .min_w_0()
                        .text_sm()
                        .line_height(px(SIDEBAR_ROW_LINE_HEIGHT))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ui.text)
                        .truncate()
                        .child(label),
                );
                if is_active {
                    button.into_any_element()
                } else {
                    button
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            activate(this, window, cx);
                            cx.notify();
                        }))
                        .into_any_element()
                }
            };

        // With Review switched off there is exactly one reachable mode, and a
        // one-segment strip is dead chrome: the builder above already drops
        // the click handler from the active segment, so it would render a
        // button that can never do anything. Drop the whole strip instead and
        // let the workspace list run to the bottom edge.
        let strip = self.cached_config.review_view_enabled().then(|| {
            div()
                .id("sidebar-mode-tabs")
                .mx(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.))
                .child(mode_button(
                    "sidebar-mode-cli",
                    "Agents",
                    matches!(mode, AppMode::Cli),
                    Box::new(|this, window, cx| this.enter_cli_mode(window, cx)),
                ))
                .child(mode_button(
                    "sidebar-mode-diff",
                    "Review",
                    matches!(mode, AppMode::Diff),
                    Box::new(|this, _window, cx| this.enter_diff_mode(cx)),
                ))
                .into_any_element()
        });

        // Cockpit home of the old title-bar IPC pill, above the mode strip and
        // shared by both modes. It is independent of the strip: the socket can
        // be down whether or not Review is enabled, so the banner still needs a
        // footer to live in when the strip is gone.
        let banner = self.render_sidebar_ipc_banner(cx);
        if banner.is_none() && strip.is_none() {
            return div().into_any_element();
        }

        let mut footer = div().relative().flex_none().pt(px(6.)).pb(px(8.));
        if let Some(banner) = banner {
            footer = footer.child(banner);
        }
        if let Some(strip) = strip {
            footer = footer.child(strip);
        }
        footer.into_any_element()
    }
}
