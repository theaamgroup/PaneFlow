//! "AI Agent" settings page - compact toggles for the built-in AI launcher
//! buttons rendered in every tab bar. The Permissions and AI access sections
//! live on the General page: they gate what an agent may do to the machine and
//! to its peer panes, which is a workspace-level decision rather than a
//! per-launcher one.
//!
//! Sections use lowercase eyebrows followed by `setting_card` groups of toggles,
//! separated by `hairline()` dividers. Only the switch is interactive - the row
//! itself does not hover or click.
//!
//! Persistence goes through [`PaneFlowApp::persist_setting`] - it mutates the
//! cached config for instant feedback and writes `paneflow.json` off the main
//! thread; `pane.rs` picks up the new state via the ConfigWatcher propagation so
//! the tab bar reflects changes without a restart. The MCP bridge installer
//! lives on its own page (`settings::tabs::mcp`).

use gpui::{
    AnyElement, Context, Hsla, IntoElement, ParentElement, SharedString, Styled, div, img, px, rgb,
    svg,
};

use crate::PaneFlowApp;
use crate::agent_launcher::TerminalAgent;
use crate::settings::components::{hairline, section_header, setting_card, toggle_row};

struct AgentToggleRow {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    agent: TerminalAgent,
}

/// Settings-only order. This deliberately differs from the frozen launcher
/// order in [`TerminalAgent::ALL`]: Grok sits directly after Codex here, while
/// tab bars, Launch Pad, and status surfaces retain their established order.
const SETTINGS_AGENT_ORDER: &[AgentToggleRow] = &[
    AgentToggleRow {
        id: "row-claude-visible",
        title: "Claude Code",
        description: "Show the Claude Code launcher button in every tab bar.",
        agent: TerminalAgent::ClaudeCode,
    },
    AgentToggleRow {
        id: "row-codex-visible",
        title: "Codex",
        description: "Show the Codex launcher button in every tab bar.",
        agent: TerminalAgent::Codex,
    },
    AgentToggleRow {
        id: "row-grok-visible",
        title: "Grok",
        description: "Show the Grok launcher button in every tab bar.",
        agent: TerminalAgent::Grok,
    },
    AgentToggleRow {
        id: "row-opencode-visible",
        title: "Opencode",
        description: "Show the Opencode launcher button in every tab bar.",
        agent: TerminalAgent::OpenCode,
    },
    AgentToggleRow {
        id: "row-pi-visible",
        title: "Pi",
        description: "Show the Pi launcher button in every tab bar.",
        agent: TerminalAgent::Pi,
    },
    AgentToggleRow {
        id: "row-hermes-agent-visible",
        title: "Hermes Agent",
        description: "Show the Hermes Agent launcher button in every tab bar.",
        agent: TerminalAgent::Hermes,
    },
    AgentToggleRow {
        id: "row-amp-visible",
        title: "Amp",
        description: "Show the Amp launcher button in every tab bar.",
        agent: TerminalAgent::Amp,
    },
    AgentToggleRow {
        id: "row-cursor-visible",
        title: "Cursor",
        description: "Show the Cursor launcher button in every tab bar.",
        agent: TerminalAgent::Cursor,
    },
    AgentToggleRow {
        id: "row-gemini-visible",
        title: "Gemini",
        description: "Show the Gemini launcher button in every tab bar.",
        agent: TerminalAgent::Gemini,
    },
    AgentToggleRow {
        id: "row-kiro-visible",
        title: "Kiro",
        description: "Show the Kiro launcher button in every tab bar.",
        agent: TerminalAgent::Kiro,
    },
    AgentToggleRow {
        id: "row-antigravity-visible",
        title: "Antigravity",
        description: "Show the Antigravity launcher button in every tab bar.",
        agent: TerminalAgent::Antigravity,
    },
    AgentToggleRow {
        id: "row-copilot-visible",
        title: "Copilot",
        description: "Show the Copilot launcher button in every tab bar.",
        agent: TerminalAgent::Copilot,
    },
    AgentToggleRow {
        id: "row-codebuddy-visible",
        title: "CodeBuddy",
        description: "Show the CodeBuddy launcher button in every tab bar.",
        agent: TerminalAgent::CodeBuddy,
    },
    AgentToggleRow {
        id: "row-factory-visible",
        title: "Factory",
        description: "Show the Factory launcher button in every tab bar.",
        agent: TerminalAgent::Factory,
    },
    AgentToggleRow {
        id: "row-qoder-visible",
        title: "Qoder",
        description: "Show the Qoder launcher button in every tab bar.",
        agent: TerminalAgent::Qoder,
    },
    AgentToggleRow {
        id: "row-openclaw-visible",
        title: "Openclaw",
        description: "Show the Openclaw launcher button in every tab bar.",
        agent: TerminalAgent::Openclaw,
    },
];

impl PaneFlowApp {
    pub(crate) fn render_ai_agent_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Read the cached config (no per-frame `load_config()`).
        let config = &self.cached_config;
        let ui = crate::theme::ui_colors();

        let mut buttons_card = setting_card(ui);
        for (idx, row) in SETTINGS_AGENT_ORDER.iter().enumerate() {
            if idx > 0 {
                buttons_card = buttons_card.child(hairline(ui));
            }
            // Effective state, not the raw key: absent keys use the
            // Claude/Codex/Grok-and-installed allowlist (see
            // `TerminalAgent::is_visible`). Toggling pins an explicit choice.
            buttons_card = buttons_card.child(toggle_row(
                row.id,
                row.title,
                row.description,
                Some(agent_icon_el(row.agent, ui)),
                row.agent.is_visible(config),
                row.agent.button_visibility_key(),
                ui,
                cx,
            ));
        }

        let buttons_section = div()
            .flex()
            .flex_col()
            .child(section_header(ui, "Tab bar buttons"))
            .child(buttons_card);

        let new_pane_section = div()
            .flex()
            .flex_col()
            .child(section_header(ui, "New pane"))
            .child(setting_card(ui).child(toggle_row(
                "row-new-pane-shows-sessions",
                "Show agent sessions beside the New pane picker",
                "Open the Agent sessions sidebar on the right of every new-pane tab so a listed session can be resumed into it.",
                None,
                config.new_pane_shows_sessions(),
                "new_pane_shows_sessions",
                ui,
                cx,
            )));

        div()
            .flex()
            .flex_col()
            .child(new_pane_section)
            .child(buttons_section.mt(px(24.)))
            .child(div().h(px(180.)).flex_none())
    }
}

/// The agent's logo for its settings row, rendered identically to the tab
/// bar: multi-color logos via `img()` (native palette preserved), monochrome
/// logos via a `text_color`-tinted `svg()` mask (brand accent if any, else
/// the theme's primary text color).
fn agent_icon_el(agent: TerminalAgent, ui: crate::theme::UiColors) -> AnyElement {
    let path = SharedString::from(agent.icon_path());
    if agent.icon_multicolor() {
        img(path).size(px(18.)).flex_none().into_any_element()
    } else {
        let tint: Hsla = agent.accent().map(|c| rgb(c).into()).unwrap_or(ui.text);
        svg()
            .size(px(18.))
            .flex_none()
            .path(path)
            .text_color(tint)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn settings_agent_order_is_a_permutation_with_grok_after_codex() {
        assert_eq!(SETTINGS_AGENT_ORDER.len(), TerminalAgent::ALL.len());
        assert_eq!(
            SETTINGS_AGENT_ORDER
                .iter()
                .map(|row| row.agent)
                .collect::<HashSet<_>>(),
            TerminalAgent::ALL.into_iter().collect::<HashSet<_>>(),
            "Settings order must contain every TerminalAgent exactly once"
        );

        let codex = SETTINGS_AGENT_ORDER
            .iter()
            .position(|row| row.agent == TerminalAgent::Codex)
            .expect("Codex in Settings order");
        assert_eq!(
            SETTINGS_AGENT_ORDER.get(codex + 1).map(|row| row.agent),
            Some(TerminalAgent::Grok),
            "Grok must appear directly after Codex in Settings only"
        );
        assert_eq!(
            TerminalAgent::ALL[2],
            TerminalAgent::OpenCode,
            "TerminalAgent::ALL stays frozen for launcher surfaces"
        );
    }
}
