//! Bottom-of-sidebar mode tabs. The Agents and Review sidebars share one
//! persistent mode switch. There is no Settings affordance here: it moved to
//! the macOS menu bar and the title-bar profile menu, finishing issue #105.
//! The strip itself disappears when `review_enabled` is off, because one
//! reachable mode is not a choice.

use crate::app::sidebar::SIDEBAR_ROW_LINE_HEIGHT;
use crate::ui_primitives::{ROW_RADIUS, squircle_skin};

use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Role, StatefulInteractiveElement, Styled, div, px, svg,
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

    /// One-time "Install MCP bridge" offer (issue #443), beside the IPC
    /// banner: shown when some pane runs an agent whose MCP config has no
    /// `paneflow` entry and the user has not dismissed the offer for that
    /// agent. Install runs the same off-thread installer as Settings; `×`
    /// records the agent id in `mcp_bridge_prompt_dismissed`. `None` when
    /// there is nothing to offer, when the status cache is still cold, or
    /// when this build may not register its bridge path at all (debug
    /// builds without `PANEFLOW_ALLOW_DEBUG_MCP_INSTALL=1`).
    pub(crate) fn render_sidebar_mcp_callout(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !crate::runtime_paths::durable_agent_install_allowed() {
            return None;
        }
        let status = self.mcp_status.as_deref()?;
        let live = self.live_mcp_agent_ids(cx);
        let pending = crate::app::sidebar::mcp_callout::pending_mcp_agent(
            status,
            &live,
            &self.cached_config.mcp_bridge_prompt_dismissed,
        )?;
        let agent_id = pending.id.clone();
        let label = pending.label.clone();
        let ui = crate::theme::ui_colors();
        let dismiss_hover = crate::app::constants::sidebar_tab_active_background();

        let install: AnyElement = if self.mcp_busy {
            div()
                .text_color(ui.muted)
                .text_size(px(12.))
                .child("Installing…")
                .into_any_element()
        } else {
            div()
                .id("sidebar-mcp-install")
                .role(Role::Button)
                .aria_label("Install MCP bridge")
                .flex_none()
                .px(px(8.))
                .py(px(4.))
                .rounded(px(6.))
                .bg(ui.accent)
                .text_color(crate::settings::tabs::mcp::mcp_button_text_color(ui, true))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .child("Install MCP bridge")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.start_mcp_install(cx);
                }))
                .into_any_element()
        };

        Some(
            div()
                .id("sidebar-mcp-callout")
                .mx(px(6.))
                .mb(px(2.))
                .px(px(8.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(ui.border)
                .bg(ui.subtle)
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(ui.text)
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .truncate()
                                .child(format!("Let {label} see other panes")),
                        )
                        .child(
                            div()
                                .id("sidebar-mcp-dismiss")
                                .role(Role::Button)
                                .aria_label("Dismiss")
                                .flex_none()
                                .size(px(18.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.))
                                .text_color(ui.muted)
                                .cursor_pointer()
                                .hover(move |style| style.bg(dismiss_hover))
                                .child(
                                    svg()
                                        .size(px(10.))
                                        .flex_none()
                                        .path("icons/close.svg")
                                        .text_color(ui.muted),
                                )
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    this.dismiss_mcp_callout(&agent_id, cx);
                                })),
                        ),
                )
                .child(install)
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
        // The MCP bridge offer (issue #443) lives here for the same reason.
        let callout = self.render_sidebar_mcp_callout(cx);
        if banner.is_none() && callout.is_none() && strip.is_none() {
            return div().into_any_element();
        }

        let mut footer = div().relative().flex_none().pt(px(6.)).pb(px(8.));
        if let Some(banner) = banner {
            footer = footer.child(banner);
        }
        if let Some(callout) = callout {
            footer = footer.child(callout);
        }
        if let Some(strip) = strip {
            footer = footer.child(strip);
        }
        footer.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use crate::source_probe::source_slice;
    use crate::terminal::element::{MIN_APCA_CONTRAST, apca_contrast};

    fn production() -> &'static str {
        include_str!("sidebar_actions_menu.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module")
    }

    /// Issue #443: the callout has to carry both affordances and route the
    /// install through the one installer Settings already uses. Read from
    /// source because `PaneFlowApp` cannot be built in a unit test.
    #[test]
    fn the_mcp_callout_offers_install_and_dismiss_through_the_settings_installer() {
        let body = source_slice(
            production(),
            "fn render_sidebar_mcp_callout(",
            "fn render_sidebar_settings_footer(",
        );
        for needle in [
            "\"sidebar-mcp-callout\"",
            "\"sidebar-mcp-install\"",
            "\"sidebar-mcp-dismiss\"",
            "this.start_mcp_install(cx)",
            "this.dismiss_mcp_callout(",
            "durable_agent_install_allowed()",
            "mcp_callout::pending_mcp_agent(",
        ] {
            assert!(
                body.contains(needle),
                "render_sidebar_mcp_callout lost `{needle}`"
            );
        }

        // The footer must keep rendering when the callout is the only thing
        // in it (Review off, socket up), or the offer never shows there.
        let footer = source_slice(
            production(),
            "fn render_sidebar_settings_footer(",
            "footer.into_any_element()",
        );
        assert!(footer.contains("self.render_sidebar_mcp_callout(cx)"));
        assert!(footer.contains("banner.is_none() && callout.is_none() && strip.is_none()"));
    }

    #[test]
    fn the_mcp_callout_install_label_is_readable_on_every_bundled_theme() {
        // The install button paints its label on `ui.accent`, like the
        // Settings button it mirrors, so the same APCA floor applies.
        for (name, _) in crate::theme::THEMES {
            let theme = crate::theme::theme_by_name(name).expect("bundled theme not found");
            let ui = crate::theme::ui_colors_with(&theme);
            let label = crate::settings::tabs::mcp::mcp_button_text_color(ui, true);
            let lc = apca_contrast(label, ui.accent).abs();
            assert!(
                lc >= MIN_APCA_CONTRAST,
                "{name}: APCA Lc({lc}) < {MIN_APCA_CONTRAST} for the sidebar MCP install label vs accent"
            );
        }
    }
}
