//! Bottom-of-sidebar mode tabs + Settings popover. The CLI, Review, and
//! Agents sidebars share one persistent mode switch, with Settings kept as
//! a compact utility button at the end of the row.
//!
//! The popover state (`PaneFlowApp::sidebar_actions_menu_open`) is
//! shared because only one sidebar is rendered at a time (mode toggle
//! swaps the whole sidebar tree).

use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, SharedString, Styled, div, prelude::*, px, svg,
};

use crate::PaneFlowApp;
use crate::settings::components::{select_item, select_menu_surface, with_alpha};
use crate::ui_primitives::AnimatedHoverExt;

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
    /// footer reads as primary navigation, while Settings remains an upward
    /// popover scoped to the current sidebar mode.
    pub(crate) fn render_sidebar_settings_footer(
        &self,
        items: Vec<SidebarMenuItem>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use paneflow_config::schema::AppMode;

        let ui = crate::theme::ui_colors();
        let settings_open = self.agents_view.sidebar_actions_menu_open;
        let mode = self.mode;

        let settings_hover_bg = crate::app::constants::sidebar_tab_active_background();
        let settings_resting_bg = if settings_open {
            settings_hover_bg
        } else {
            settings_hover_bg.opacity(0.0)
        };
        let settings_trigger = div()
            .id("sidebar-settings-trigger")
            .flex_none()
            .h(px(30.))
            .w(px(30.))
            .rounded(px(9.))
            .flex()
            .items_center()
            .justify_center()
            .animated_hover_bg(settings_resting_bg, settings_hover_bg)
            .tooltip(move |_window, cx| {
                cx.new(|_| crate::app::sidebar::SidebarTooltip {
                    label: "Settings".into(),
                })
                .into()
            })
            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                this.agents_view.sidebar_actions_menu_open =
                    !this.agents_view.sidebar_actions_menu_open;
                this.agents_view.sidebar_mode_picker_open = false;
                cx.notify();
            }))
            .child(
                svg()
                    .size(px(14.))
                    .flex_none()
                    .path("icons/settings.svg")
                    .text_color(ui.muted),
            );

        let settings_popover: Option<AnyElement> = if settings_open {
            // Vertical menu opening upward from the trigger. Mirrors the
            // Settings "Shell" select menu (`components::select_menu`) that the
            // title-bar "Files" / "Help" dropdowns also use: the same elevated
            // surface, hairline border at 0.6 alpha, soft shadow, 10px radius
            // and 4px padding, so every app menu reads as one consistent menu
            // language. The container is open-coded (not `select_menu`) because
            // this popover stretches to the sidebar width via left/right, which
            // would fight `select_menu`'s fixed 200-280px clamp.
            let mut menu = div()
                .id("sidebar-settings-popover")
                .absolute()
                .left(px(6.))
                .right(px(6.))
                .bottom(px(44.))
                .flex()
                .flex_col()
                .gap(px(1.))
                .p(px(4.))
                .rounded(px(10.))
                .bg(select_menu_surface(ui))
                .border_1()
                .border_color(with_alpha(ui.border, 0.6))
                // Click anywhere outside the popover (or its trigger)
                // dismisses it. Same pattern as `profile_menu.rs`.
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if this.agents_view.sidebar_actions_menu_open {
                        this.agents_view.sidebar_actions_menu_open = false;
                        cx.notify();
                    }
                }));
            for item in items {
                menu = menu.child(render_menu_item(item, ui, cx));
            }
            Some(menu.into_any_element())
        } else {
            None
        };

        type Activate = Box<dyn Fn(&mut PaneFlowApp, &mut gpui::Window, &mut Context<PaneFlowApp>)>;
        let mode_button = |id: &'static str,
                           label: &'static str,
                           icon: &'static str,
                           is_active: bool,
                           activate: Activate| {
            // Equal-width compact segments keep the three primary surfaces
            // visible without letting the Settings utility reclaim the row.
            let fg = if is_active { ui.text } else { ui.muted };
            let hover_bg = crate::app::constants::sidebar_tab_active_background();
            let resting_bg = if is_active {
                hover_bg
            } else {
                hover_bg.opacity(0.0)
            };
            let button = div()
                .id(id)
                .flex_1()
                .h(px(30.))
                .min_w_0()
                .px(px(2.))
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(3.))
                .rounded(px(8.))
                .child(svg().size(px(13.)).flex_none().path(icon).text_color(fg))
                .child(
                    div()
                        .min_w_0()
                        .text_size(px(11.))
                        .font_weight(FontWeight::NORMAL)
                        .text_color(fg)
                        .truncate()
                        .child(label),
                )
                .animated_hover_bg(resting_bg, hover_bg);
            // Same interaction tint as the workspace cards / thread tabs.
            // Active and hover deliberately match in dark mode.
            if is_active {
                button.into_any_element()
            } else {
                button
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        activate(this, window, cx);
                        this.agents_view.sidebar_actions_menu_open = false;
                        this.agents_view.sidebar_mode_picker_open = false;
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
                "CLI",
                "icons/terminal.svg",
                matches!(mode, AppMode::Cli),
                Box::new(|this, window, cx| this.enter_cli_mode(window, cx)),
            ))
            .child(mode_button(
                "sidebar-mode-diff",
                "Review",
                "icons/git-pull-request.svg",
                matches!(mode, AppMode::Diff),
                Box::new(|this, _window, cx| this.enter_diff_mode(cx)),
            ))
            .child(mode_button(
                "sidebar-mode-agents",
                "Agents",
                "icons/sparkles.svg",
                matches!(mode, AppMode::Agents),
                Box::new(|this, _window, cx| this.enter_agents_mode(cx)),
            ))
            .child(settings_trigger)
            .into_any_element();

        let mut footer = div().relative().flex_none().pt(px(6.)).pb(px(8.));
        if let Some(popover) = settings_popover {
            footer = footer.child(popover);
        }
        if let Some(banner) = self.render_sidebar_ipc_banner(cx) {
            footer = footer.child(banner);
        }
        footer.child(footer_row).into_any_element()
    }

    /// Compatibility slot for existing sidebar call sites. The mode picker now
    /// lives in the shared footer above.
    pub(crate) fn render_mode_toggle(&self, _cx: &mut Context<Self>) -> AnyElement {
        div().into_any_element()
    }
}

pub(crate) type SidebarMenuAction =
    Box<dyn Fn(&mut PaneFlowApp, &mut gpui::Window, &mut Context<PaneFlowApp>) + 'static>;

/// A single action row in the Settings popover.
pub(crate) struct SidebarMenuItem {
    pub id: SharedString,
    pub icon: &'static str,
    pub label: SharedString,
    pub on_click: SidebarMenuAction,
}

fn render_menu_item(
    item: SidebarMenuItem,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let handler = item.on_click;
    select_item(item.id, false, ui)
        .cursor(CursorStyle::Arrow)
        .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
            handler(this, w, cx);
            this.agents_view.sidebar_actions_menu_open = false;
            cx.notify();
        }))
        .child(
            svg()
                .size(px(14.))
                .flex_none()
                .path(item.icon)
                .text_color(ui.muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(ui.text)
                .truncate()
                .child(item.label),
        )
        .into_any_element()
}
