//! Selection logic behind the sidebar's one-time "Install MCP bridge"
//! callout (issue #443).
//!
//! `paneflow mcp status` on a machine that runs Claude Code, Codex, Gemini
//! and opencode inside PaneFlow every day reports each of them as "detected
//! but not installed", because the only entry points to the installer were
//! the CLI and a button under Settings. The sidebar footer now offers the
//! bridge once per agent: when a pane runs an agent whose MCP config has no
//! `paneflow` entry, a callout names that agent and runs the existing
//! off-thread installer. A dismissal is remembered per agent in
//! `mcp_bridge_prompt_dismissed`, so the offer never nags.
//!
//! Everything here is pure or a thin `PaneFlowApp` walk; the rendering
//! lives in `app/sidebar_actions_menu.rs` beside the IPC banner it sits
//! next to.

use std::collections::BTreeSet;

use gpui::{App, Context};
use paneflow_mcp_install::{StatusKind, StatusReport};

use crate::PaneFlowApp;

/// The agent the callout should offer the bridge for, if any: the first
/// report (by id, so the choice is deterministic across probes) whose agent
/// is `NotInstalled`, is live in some pane, and has not been dismissed.
///
/// `Stale` and `NeedsRepair` are deliberately not offered here: they belong
/// to the Settings page, which explains what is wrong. `NotDetected` cannot
/// be live, and `Installed` needs nothing.
pub(crate) fn pending_mcp_agent<'a>(
    status: &'a [StatusReport],
    live_agent_ids: &BTreeSet<&str>,
    dismissed: &[String],
) -> Option<&'a StatusReport> {
    status
        .iter()
        .filter(|report| matches!(report.kind, StatusKind::NotInstalled))
        .filter(|report| live_agent_ids.contains(report.id.as_str()))
        .filter(|report| !dismissed.iter().any(|d| d == &report.id))
        .min_by(|a, b| a.id.cmp(&b.id))
}

/// `existing` plus `id`, recorded once: dismissing the same agent twice
/// (two windows, a race with the config watcher) must not grow the list.
pub(crate) fn with_dismissed_id(existing: &[String], id: &str) -> Vec<String> {
    let mut ids = existing.to_vec();
    if !ids.iter().any(|d| d == id) {
        ids.push(id.to_string());
    }
    ids
}

/// Whether a fresh status probe is worth running after panes resolved the
/// agents in `resolved`: the cache is empty, or it has no verdict for one
/// of them, or it still calls one of them `NotDetected` - a pane running
/// that agent is proof the CLI exists, so the earlier probe predates its
/// install and the callout would otherwise never appear for it.
pub(crate) fn status_cache_is_stale_for(
    status: Option<&[StatusReport]>,
    resolved: &BTreeSet<&'static str>,
) -> bool {
    if resolved.is_empty() {
        return false;
    }
    let Some(status) = status else {
        return true;
    };
    resolved.iter().any(|id| {
        !status
            .iter()
            .any(|report| report.id == *id && !matches!(report.kind, StatusKind::NotDetected))
    })
}

impl PaneFlowApp {
    /// MCP-install ids of every agent some pane is running right now, across
    /// every workspace and tab. Reads each terminal's `detected_agent`, the
    /// value the PID scan writes, so a pane that merely typed `claude` into
    /// a shell counts only once the scan confirms it.
    pub(crate) fn live_mcp_agent_ids(&self, cx: &App) -> BTreeSet<&'static str> {
        self.workspaces
            .iter()
            .flat_map(|ws| ws.collect_panes())
            .flat_map(|pane| {
                pane.read(cx)
                    .terminals()
                    .filter_map(|terminal| terminal.read(cx).terminal.detected_agent)
                    .collect::<Vec<_>>()
            })
            .filter_map(|agent| agent.mcp_install_id())
            .collect()
    }

    /// Re-probe the MCP status cache after the pane scan resolved the agents
    /// in `resolved`, when the cache cannot answer for one of them. Runs off
    /// the main thread and never while an install is in flight. Not a poll:
    /// the scan only reports an agent here when a pane newly resolved it.
    pub(crate) fn refresh_mcp_status_for_resolved_agents(
        &self,
        resolved: &BTreeSet<&'static str>,
        cx: &mut Context<Self>,
    ) {
        if self.mcp_busy {
            return;
        }
        if status_cache_is_stale_for(self.mcp_status.as_deref(), resolved) {
            self.refresh_mcp_status(cx);
        }
    }

    /// Hide the callout for one agent for good: persist its id in
    /// `mcp_bridge_prompt_dismissed` and mirror the write into the cached
    /// config so this frame already reflects it (the file write comes back
    /// through the config watcher on a later tick).
    pub(crate) fn dismiss_mcp_callout(&mut self, id: &str, cx: &mut Context<Self>) {
        let ids = with_dismissed_id(&self.cached_config.mcp_bridge_prompt_dismissed, id);
        if !crate::config_writer::save_config_values_checked([(
            "mcp_bridge_prompt_dismissed",
            serde_json::json!(ids),
        )]) {
            self.show_toast("Could not save the MCP bridge dismissal", cx);
            return;
        }
        self.cached_config.mcp_bridge_prompt_dismissed = ids;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: &str, kind: StatusKind) -> StatusReport {
        StatusReport {
            id: id.to_string(),
            label: format!("Label {id}"),
            kind,
        }
    }

    fn live<'a>(ids: &[&'a str]) -> BTreeSet<&'a str> {
        ids.iter().copied().collect()
    }

    #[test]
    fn pending_mcp_agent_skips_dismissed_installed_and_absent_agents() {
        let not_installed = vec![report("codex", StatusKind::NotInstalled)];

        // Not installed, live, not dismissed: offered.
        let pending = pending_mcp_agent(&not_installed, &live(&["codex"]), &[]);
        assert_eq!(pending.map(|r| r.id.as_str()), Some("codex"));

        // Dismissed: never offered again.
        assert!(
            pending_mcp_agent(&not_installed, &live(&["codex"]), &["codex".to_string()]).is_none()
        );

        // Not live in any pane: nothing to offer, even though not installed.
        assert!(pending_mcp_agent(&not_installed, &live(&["claude-code"]), &[]).is_none());
        assert!(pending_mcp_agent(&not_installed, &live(&[]), &[]).is_none());

        // Installed, not detected, stale, needs repair, error: not this
        // callout's business (Stale/NeedsRepair belong to Settings).
        for kind in [
            StatusKind::Installed {
                path: "/bridge".to_string(),
            },
            StatusKind::NotDetected,
            StatusKind::Stale {
                found: "/old".to_string(),
                expected: "/new".to_string(),
            },
            StatusKind::NeedsRepair {
                path: None,
                reason: "disabled".to_string(),
            },
            StatusKind::Error("boom".to_string()),
        ] {
            let status = vec![report("codex", kind.clone())];
            assert!(
                pending_mcp_agent(&status, &live(&["codex"]), &[]).is_none(),
                "{kind:?} must not raise the callout"
            );
        }
    }

    #[test]
    fn pending_mcp_agent_picks_the_lowest_id_regardless_of_report_order() {
        let status = vec![
            report("opencode", StatusKind::NotInstalled),
            report("gemini", StatusKind::NotInstalled),
            report("claude-code", StatusKind::NotInstalled),
        ];
        let all = live(&["opencode", "gemini", "claude-code"]);
        let pending = pending_mcp_agent(&status, &all, &[]);
        assert_eq!(pending.map(|r| r.id.as_str()), Some("claude-code"));
        // Dismissing the first moves on to the next, not to nothing.
        let pending = pending_mcp_agent(&status, &all, &["claude-code".to_string()]);
        assert_eq!(pending.map(|r| r.id.as_str()), Some("gemini"));
    }

    #[test]
    fn dismissal_records_each_agent_once() {
        let ids = with_dismissed_id(&[], "codex");
        assert_eq!(ids, ["codex"]);
        let ids = with_dismissed_id(&ids, "codex");
        assert_eq!(ids, ["codex"]);
        let ids = with_dismissed_id(&ids, "gemini");
        assert_eq!(ids, ["codex", "gemini"]);
        // The dismissal of one agent leaves the others offered.
        let status = vec![
            report("codex", StatusKind::NotInstalled),
            report("claude-code", StatusKind::NotInstalled),
        ];
        let pending = pending_mcp_agent(&status, &live(&["codex", "claude-code"]), &ids);
        assert_eq!(pending.map(|r| r.id.as_str()), Some("claude-code"));
    }

    #[test]
    fn status_cache_is_stale_only_for_unknown_or_undetected_resolved_agents() {
        let resolved: BTreeSet<&'static str> = ["codex"].into_iter().collect();
        let none: BTreeSet<&'static str> = BTreeSet::new();

        // Nothing resolved: never probe, whatever the cache holds.
        assert!(!status_cache_is_stale_for(None, &none));
        // No cache yet: probe.
        assert!(status_cache_is_stale_for(None, &resolved));
        // The cache has no verdict for the agent: probe.
        let other = vec![report("gemini", StatusKind::NotInstalled)];
        assert!(status_cache_is_stale_for(Some(&other), &resolved));
        // The cache predates the agent's install: probe.
        let undetected = vec![report("codex", StatusKind::NotDetected)];
        assert!(status_cache_is_stale_for(Some(&undetected), &resolved));
        // A real verdict, whichever it is: the cache stands.
        for kind in [
            StatusKind::NotInstalled,
            StatusKind::Installed {
                path: "/bridge".to_string(),
            },
            StatusKind::Error("boom".to_string()),
        ] {
            let known = vec![report("codex", kind)];
            assert!(!status_cache_is_stale_for(Some(&known), &resolved));
        }
    }
}
