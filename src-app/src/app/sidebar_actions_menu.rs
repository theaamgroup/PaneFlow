//! Bottom-of-sidebar mode tabs + Settings button. The Agents and Review
//! sidebars share one persistent mode switch, with Settings kept as
//! a compact utility button at the end of the row that opens the settings
//! surface directly - no intermediate menu.

use crate::app::sidebar::SIDEBAR_ROW_LINE_HEIGHT;
use crate::ui_primitives::{ROW_RADIUS, TooltipDelayExt, squircle_skin};

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

    /// Render the bottom footer: persistent interface tabs plus a compact
    /// Settings trigger. The mode switch stays visible after selection so the
    /// footer reads as primary navigation, while Settings opens the settings
    /// surface in one click.
    pub(crate) fn render_sidebar_settings_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        use paneflow_config::schema::AppMode;

        let ui = crate::theme::ui_colors();
        let mode = self.mode;

        // Skinned exactly like a workspace card: the trigger rests on the
        // active tint while the settings surface is up, and lifts to the hover
        // tint otherwise, both on the rail's continuous corner.
        let active_bg = crate::app::constants::sidebar_tab_active_background();
        let hover_bg = crate::app::constants::sidebar_tab_hover_background();
        let settings_open = self.settings_section.is_some();
        let settings_trigger = squircle_skin(
            div()
                .id("sidebar-settings-trigger")
                .flex_none()
                .h(px(30.))
                .w(px(30.))
                .flex()
                .items_center()
                .justify_center(),
            "sidebar-settings-trigger-group",
            ROW_RADIUS,
            settings_open.then_some(active_bg),
            (!settings_open).then_some(hover_bg),
        )
        .delayed_tooltip(move |_window, cx| {
            cx.new(|_| crate::app::sidebar::SidebarTooltip {
                label: "Settings".into(),
            })
            .into()
        })
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.open_settings_window(window, cx);
        }))
        .child(
            svg()
                .size(px(14.))
                .flex_none()
                .path("icons/settings.svg")
                .text_color(ui.muted),
        );

        type Activate = Box<dyn Fn(&mut PaneFlowApp, &mut gpui::Window, &mut Context<PaneFlowApp>)>;
        let mode_button =
            |id: &'static str, label: &'static str, is_active: bool, activate: Activate| {
                // Equal-width compact segments keep both primary surfaces visible
                // without letting the Settings utility reclaim the row.
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

        let footer_row: AnyElement = div()
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
            .child(settings_trigger)
            .into_any_element();

        let mut footer = div().relative().flex_none().pt(px(6.)).pb(px(8.));
        // Cockpit home of the old title-bar IPC pill, right above the Settings
        // trigger, shared by both modes.
        if let Some(banner) = self.render_sidebar_ipc_banner(cx) {
            footer = footer.child(banner);
        }
        footer.child(footer_row).into_any_element()
    }
}
