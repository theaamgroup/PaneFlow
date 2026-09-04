//! "MCP Servers" settings page - registers the embedded `paneflow-mcp` bridge
//! with every detected CLI agent (Claude Code, Codex, Gemini, opencode) so they
//! can read other panes' output.
//!
//! One button plus a per-agent recap. The state (status snapshot, last install
//! result, busy flag) is cached on `PaneFlowApp` and refreshed off the GPUI
//! main thread, so this render does zero config I/O. Warmed when the page is
//! opened (`select_settings_section`) and after each install.
//!
//! Moved out of the AI-agent tab into its own Codex-style page during the
//! inline-settings migration.

use gpui::{
    ClickEvent, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, Styled, div, prelude::*, px,
};

use paneflow_mcp_install::{InstallKind, OverallState, StatusKind};

use crate::PaneFlowApp;
use crate::settings::components::{
    SETTINGS_CONTROL_CORNER_RADIUS, hairline, section_header, setting_card, setting_text,
    with_alpha,
};
use crate::terminal::element::{MIN_APCA_CONTRAST, ensure_minimum_contrast};
use crate::ui_primitives::AnimatedHoverExt;

/// Label color for the install button: on-accent when live, muted when not.
///
/// `accent` and `text` are independent theme tokens (Vercel Dark's accent is
/// #ffffff), so a fixed white label can vanish on the fill. Lift the label
/// off the fill with the same APCA pass the Shortcuts page uses.
fn mcp_button_text_color(ui: crate::theme::UiColors, enabled: bool) -> gpui::Hsla {
    if enabled {
        ensure_minimum_contrast(ui.text, ui.accent, MIN_APCA_CONTRAST)
    } else {
        ui.muted
    }
}

impl PaneFlowApp {
    pub(crate) fn render_mcp_servers_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = crate::theme::ui_colors();

        let state = self
            .mcp_status
            .as_deref()
            .map(paneflow_mcp_install::overall_state);
        let debug_blocked = !crate::runtime_paths::durable_agent_install_allowed();

        // Button label + whether the click is live.
        let (label, enabled): (SharedString, bool) = if self.mcp_busy {
            ("Installing…".into(), false)
        } else if debug_blocked {
            ("Unavailable in debug".into(), false)
        } else {
            match state {
                None => ("Checking…".into(), false),
                Some(OverallState::NoAgents) => ("No agents detected".into(), false),
                Some(OverallState::AllInstalled) => ("Reinstall".into(), true),
                Some(OverallState::NeedsRepair) => ("Repair".into(), true),
                Some(OverallState::NeedsInstall) => ("Install MCP bridge".into(), true),
            }
        };

        let button_bg = if enabled { ui.accent } else { ui.subtle };
        let button_hover_bg = if enabled {
            with_alpha(ui.accent, 0.85)
        } else {
            button_bg
        };
        let button = div()
            .id("mcp-install-btn")
            .role(Role::Button)
            .aria_label(label.clone())
            .flex_shrink_0()
            .px(px(12.))
            .py(px(6.))
            .rounded(SETTINGS_CONTROL_CORNER_RADIUS)
            .text_size(px(12.))
            .font_weight(FontWeight::MEDIUM)
            .bg(button_bg)
            .text_color(mcp_button_text_color(ui, enabled))
            .animated_hover_bg(button_bg, button_hover_bg)
            .child(label)
            .when(enabled, |button| {
                button.on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.start_mcp_install(cx);
                }))
            });

        let header_row = div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(16.))
            .px(px(12.))
            .py(px(10.))
            .child(setting_text(
                ui,
                "Read your panes from your agents",
                "Registers the bundled paneflow-mcp bridge with every detected CLI \
                 agent (Claude Code, Codex, Gemini, opencode) so they can read other \
                 panes' output. Idempotent, backed up, and only touches the paneflow \
                 entry. Re-run after an update if a path goes stale.",
            ))
            .child(button);

        let mut card = setting_card(ui).child(header_row);

        // Per-agent recap: prefer the last install result; else the cached
        // status snapshot. Each row is one agent + a short state.
        let recap_lines = self.mcp_recap_lines();
        if let Some(error) = self.mcp_install_error() {
            card = card.child(hairline(ui)).child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .text_size(px(12.))
                    .text_color(danger_color())
                    .child(error),
            );
        } else if debug_blocked {
            card = card.child(hairline(ui)).child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .text_size(px(12.))
                    .text_color(danger_color())
                    .child(SharedString::from(
                        crate::runtime_paths::durable_agent_install_refusal_message(),
                    )),
            );
        }
        for (line, is_error) in recap_lines {
            card = card.child(hairline(ui)).child(
                div()
                    .px(px(12.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(if is_error { danger_color() } else { ui.muted })
                    .child(line),
            );
        }

        div()
            .flex()
            .flex_col()
            .child(section_header(ui, "MCP bridge"))
            .child(card)
    }

    /// The refusal message from the last install, if it failed wholesale
    /// (bridge missing / data dir unresolved).
    fn mcp_install_error(&self) -> Option<SharedString> {
        match &self.mcp_install {
            Some(Err(msg)) => Some(SharedString::from(msg.clone())),
            _ => None,
        }
    }

    /// Per-agent recap lines `(text, is_error)`. Uses the last install result
    /// when present, else the cached status snapshot.
    fn mcp_recap_lines(&self) -> Vec<(SharedString, bool)> {
        if let Some(Ok(results)) = &self.mcp_install {
            return results
                .iter()
                .map(|r| {
                    let (state, err) = match &r.kind {
                        InstallKind::Installed => ("installed", false),
                        InstallKind::Updated => ("updated", false),
                        InstallKind::AlreadyCurrent => ("already up to date", false),
                        InstallKind::SkippedAbsent => ("not detected", false),
                        InstallKind::Error(e) => {
                            return (format!("{}: error - {e}", r.label).into(), true);
                        }
                    };
                    (format!("{}: {state}", r.label).into(), err)
                })
                .collect();
        }
        match &self.mcp_status {
            Some(statuses) => statuses
                .iter()
                .map(|r| {
                    let (state, err) = match &r.kind {
                        StatusKind::NotDetected => ("not detected", false),
                        StatusKind::Installed { .. } => ("installed", false),
                        StatusKind::Stale { .. } => ("stale path - click Repair", false),
                        StatusKind::NeedsRepair { .. } => ("needs repair - click Repair", false),
                        StatusKind::NotInstalled => ("not installed", false),
                        StatusKind::Error(e) => {
                            return (format!("{}: error - {e}", r.label).into(), true);
                        }
                    };
                    (format!("{}: {state}", r.label).into(), err)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Refresh the cached MCP bridge status off the main thread. Reads each
    /// agent's config (no writes), then stores the snapshot + repaints. Called
    /// when the settings page opens and when the MCP page is selected.
    pub(crate) fn refresh_mcp_status(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let status = smol::unblock(|| {
                let bridge = crate::runtime_paths::bridge_binary_path();
                paneflow_mcp_install::status_all(bridge.as_deref())
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.mcp_status = Some(status);
                cx.notify();
            });
        })
        .detach();
    }

    /// Install the bridge into every detected agent, off the main thread.
    /// Extracts the bridge binary first (so the registered path exists), then
    /// runs the install + a fresh status probe, and stores both.
    fn start_mcp_install(&mut self, cx: &mut Context<Self>) {
        if self.mcp_busy {
            return;
        }
        if !crate::runtime_paths::durable_agent_install_allowed() {
            self.mcp_install = Some(Err(
                crate::runtime_paths::durable_agent_install_refusal_message(),
            ));
            cx.notify();
            return;
        }
        self.mcp_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (install, status) = smol::unblock(|| {
                let bridge = match crate::ai_hooks::extract::ensure_bridge_extracted() {
                    Ok(p) => Some(p),
                    Err(e) => {
                        log::warn!(
                            "settings: MCP bridge extraction failed ({e:#}); install may refuse"
                        );
                        crate::runtime_paths::bridge_binary_path()
                    }
                };
                let install = paneflow_mcp_install::install_all(bridge.as_deref());
                let status = paneflow_mcp_install::status_all(bridge.as_deref());
                (install, status)
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.mcp_busy = false;
                this.mcp_install = Some(install);
                this.mcp_status = Some(status);
                cx.notify();
            });
        })
        .detach();
    }
}

/// Error-text color for the MCP recap. `UiColors` has no danger slot and the
/// settings chrome is theme-independent, so a fixed One Dark red keeps the cue
/// readable on every bundled theme.
fn danger_color() -> gpui::Hsla {
    gpui::rgb(0xE0_6C_75).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::element::apca_contrast;

    #[test]
    fn enabled_mcp_button_label_is_readable_on_every_bundled_theme() {
        // The enabled button paints its label on `ui.accent`. Vercel Dark's
        // accent is #ffffff and PaneFlow Dark's is a light teal, so a fixed
        // white label vanishes or falls under the APCA floor. The label must
        // clear the same Lc threshold the terminal uses for selected text.
        for (name, _) in crate::theme::THEMES {
            let theme = crate::theme::theme_by_name(name).expect("bundled theme not found");
            let ui = crate::theme::ui_colors_with(&theme);
            let label = mcp_button_text_color(ui, true);
            let lc = apca_contrast(label, ui.accent).abs();
            assert!(
                lc >= MIN_APCA_CONTRAST,
                "{name}: APCA Lc({lc}) < {MIN_APCA_CONTRAST} for the enabled MCP button label vs accent"
            );
        }
    }
}
