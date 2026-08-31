//! Desktop notification routing for agent lifecycle events.
//!
//! This module owns both sides of the notification gate:
//! - the process-wide focus flag updated by the GPUI app;
//! - a single cross-platform `notify-rust` firing path used by `ai.*` handlers.

use std::sync::{
    Once,
    atomic::{AtomicBool, Ordering},
};

use gpui::BackgroundExecutor;
use paneflow_config::schema::{AgentPanelConfig, NotifyWhenAgentWaiting, PaneFlowConfig};

use crate::agent_launcher::TerminalAgent;

const NOTIFICATION_DETAIL_CAP_CHARS: usize = 512;
const PANEFLOW_BUNDLE_IDENTIFIER: &str = "com.theaamgroup.paneflow";

/// `notify-rust` otherwise asks macOS to resolve its internal sentinel app name
/// `use_default`. That lookup opens an unexplained Choose Application dialog,
/// and concurrent first notifications can each open their own dialog. Set the
/// real bundle identity ourselves, serialized before any notification is shown.
static NOTIFICATION_APPLICATION_INIT: Once = Once::new();

/// Window-active gate updated by `cx.observe_window_activation`.
/// `true` while the OS reports the Paneflow window as the focused one.
static WINDOW_ACTIVE: AtomicBool = AtomicBool::new(true);

/// Update the window-active flag. Called from
/// `cx.observe_window_activation` and from the initial activation
/// tick that GPUI fires when the observer registers.
pub fn set_window_active(active: bool) {
    WINDOW_ACTIVE.store(active, Ordering::Relaxed);
}

/// Is the Paneflow window currently the focused surface?
pub fn window_active() -> bool {
    WINDOW_ACTIVE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesktopNotificationUrgency {
    Normal,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DesktopNotification {
    summary: String,
    body: String,
    urgency: DesktopNotificationUrgency,
}

impl DesktopNotification {
    pub(crate) fn turn_finished(
        agent: TerminalAgent,
        workspace_title: &str,
        session_summary: Option<&str>,
    ) -> Self {
        Self {
            summary: format!("{} finished", agent.display_name()),
            body: notification_context_body(workspace_title, session_summary),
            urgency: DesktopNotificationUrgency::Normal,
        }
    }

    pub(crate) fn needs_input(
        agent: TerminalAgent,
        workspace_title: &str,
        message: Option<&str>,
    ) -> Self {
        Self {
            summary: format!("{} needs input", agent.display_name()),
            body: attention_notification_body(workspace_title, message),
            urgency: DesktopNotificationUrgency::Critical,
        }
    }

    pub(crate) fn agent_exited(
        agent: TerminalAgent,
        workspace_title: &str,
        exit_code: i32,
    ) -> Self {
        Self {
            summary: format!("{} exited unexpectedly", agent.display_name()),
            body: agent_exit_notification_body(workspace_title, exit_code),
            urgency: DesktopNotificationUrgency::Critical,
        }
    }

    pub(crate) fn stalled(agent: TerminalAgent, workspace_title: &str, silent_secs: u64) -> Self {
        Self {
            summary: format!("{} may be stuck", agent.display_name()),
            body: stalled_notification_body(workspace_title, silent_secs),
            urgency: DesktopNotificationUrgency::Critical,
        }
    }
}

/// Fire a best-effort desktop notification without blocking the GPUI thread.
pub(crate) fn fire_desktop_notification(
    notification: DesktopNotification,
    config: &PaneFlowConfig,
    executor: BackgroundExecutor,
) {
    let gate = config.agent_panel.as_ref().map_or(
        NotifyWhenAgentWaiting::Never,
        AgentPanelConfig::resolved_notify_when_agent_waiting,
    );
    if !should_fire_desktop_notification(gate, window_active()) {
        return;
    }

    executor
        .spawn(async move {
            let _ = smol::unblock(move || show_desktop_notification(notification)).await;
        })
        .detach();
}

pub(crate) fn should_fire_desktop_notification(
    gate: NotifyWhenAgentWaiting,
    window_active: bool,
) -> bool {
    match gate {
        NotifyWhenAgentWaiting::Never => false,
        NotifyWhenAgentWaiting::PrimaryScreen | NotifyWhenAgentWaiting::AllScreens => {
            !window_active
        }
    }
}

/// Bound + sanitize an agent question before it is stored on the session
/// and mirrored to notifications.
pub(crate) fn sanitize_notification_message(raw: &str) -> String {
    crate::markdown::strip_bidi_zero_width(raw.chars().take(512).collect())
}

fn notification_detail(raw: &str) -> Option<String> {
    let clean: String = crate::markdown::strip_bidi_zero_width(
        raw.chars().take(NOTIFICATION_DETAIL_CAP_CHARS).collect(),
    )
    .trim()
    .to_string();
    (!clean.is_empty()).then_some(clean)
}

fn notification_context_body(workspace_title: &str, session_summary: Option<&str>) -> String {
    session_summary
        .and_then(notification_detail)
        .or_else(|| notification_detail(workspace_title))
        .unwrap_or_else(|| "PaneFlow".to_string())
}

pub(crate) fn attention_notification_body(workspace_title: &str, message: Option<&str>) -> String {
    notification_context_body(workspace_title, message)
}

pub(crate) fn agent_exit_notification_body(workspace_title: &str, exit_code: i32) -> String {
    format!(
        "{}: exited with code {exit_code}",
        notification_context_body(workspace_title, None)
    )
}

pub(crate) fn stalled_notification_body(workspace_title: &str, silent_secs: u64) -> String {
    format!(
        "{}: no activity for {silent_secs} s",
        notification_context_body(workspace_title, None)
    )
}

fn show_desktop_notification(notification: DesktopNotification) -> Result<(), String> {
    initialize_notification_application();

    let mut builder = notify_rust::Notification::new();
    builder
        .summary(&notification.summary)
        .body(&notification.body)
        .appname("PaneFlow")
        .icon("paneflow")
        .timeout(std::time::Duration::from_secs(8));

    builder.show().map(|_| ()).map_err(|err| err.to_string())
}

fn initialize_notification_application() {
    initialize_notification_application_with(&NOTIFICATION_APPLICATION_INIT, || {
        notify_rust::set_application(PANEFLOW_BUNDLE_IDENTIFIER).map_err(|err| err.to_string())
    });
}

fn initialize_notification_application_with(
    guard: &Once,
    initialize: impl FnOnce() -> Result<(), String>,
) {
    guard.call_once(|| {
        if let Err(error) = initialize() {
            log::warn!("desktop notifications: failed to use PaneFlow bundle identity: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[test]
    fn notification_bundle_identifier_matches_the_app_bundle() {
        let plist = include_str!("../../../assets/Info.plist");
        let expected = format!(
            "<key>CFBundleIdentifier</key>\n    <string>{PANEFLOW_BUNDLE_IDENTIFIER}</string>"
        );
        assert!(
            plist.contains(&expected),
            "desktop notification identity must match assets/Info.plist"
        );
    }

    #[test]
    fn notification_application_initialization_is_serialized() {
        const WORKERS: usize = 8;

        let guard = Arc::new(Once::new());
        let barrier = Arc::new(Barrier::new(WORKERS));
        let calls = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                let guard = Arc::clone(&guard);
                let barrier = Arc::clone(&barrier);
                let calls = Arc::clone(&calls);
                std::thread::spawn(move || {
                    barrier.wait();
                    initialize_notification_application_with(&guard, || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    });
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("notification initializer worker");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_notification_application_initialization_is_not_retried() {
        let guard = Once::new();
        let calls = AtomicUsize::new(0);

        initialize_notification_application_with(&guard, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Err("test failure".to_string())
        });
        initialize_notification_application_with(&guard, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn notification_gate_honors_never_and_window_focus() {
        assert!(!should_fire_desktop_notification(
            NotifyWhenAgentWaiting::Never,
            false
        ));
        assert!(
            !should_fire_desktop_notification(NotifyWhenAgentWaiting::PrimaryScreen, true),
            "active Paneflow window suppresses OS notifications"
        );
        assert!(
            should_fire_desktop_notification(NotifyWhenAgentWaiting::PrimaryScreen, false),
            "inactive Paneflow window notifies"
        );
        assert!(should_fire_desktop_notification(
            NotifyWhenAgentWaiting::AllScreens,
            false
        ));
    }

    #[test]
    fn notification_message_is_bounded_and_bidi_stripped() {
        let spoofed = "Allow \u{202E}?fr- mr\u{202C} ?";
        let clean = sanitize_notification_message(spoofed);
        assert!(!clean.contains('\u{202E}'), "RLO stripped");
        assert!(!clean.contains('\u{202C}'), "PDF stripped");
        assert!(clean.contains("Allow"), "visible text kept: {clean}");

        let long = "é".repeat(600);
        assert_eq!(
            sanitize_notification_message(&long).chars().count(),
            512,
            "char-bounded, multibyte-safe"
        );
    }

    #[test]
    fn notification_bodies_are_specific_and_non_empty() {
        assert_eq!(
            attention_notification_body("backend", Some("Allow `cargo test`?")),
            "Allow `cargo test`?"
        );
        assert_eq!(attention_notification_body("backend", None), "backend");
        assert_eq!(
            attention_notification_body("backend", Some("   ")),
            "backend"
        );
        assert_eq!(
            agent_exit_notification_body("api", 1),
            "api: exited with code 1"
        );
        assert_eq!(
            stalled_notification_body("api", 300),
            "api: no activity for 300 s"
        );
        assert_eq!(
            notification_context_body("workspace", Some("Finished the release draft")),
            "Finished the release draft"
        );
    }

    #[test]
    fn desktop_notification_constructors_set_title_body_and_urgency() {
        let finished =
            DesktopNotification::turn_finished(TerminalAgent::Codex, "backend", Some("Tests pass"));
        assert_eq!(finished.summary, "Codex finished");
        assert_eq!(finished.body, "Tests pass");
        assert_eq!(finished.urgency, DesktopNotificationUrgency::Normal);

        let finished_without_summary =
            DesktopNotification::turn_finished(TerminalAgent::Codex, "backend", None);
        assert_eq!(finished_without_summary.summary, "Codex finished");
        assert_eq!(finished_without_summary.body, "backend");

        let attention = DesktopNotification::needs_input(
            TerminalAgent::ClaudeCode,
            "backend",
            Some("Approve edit?"),
        );
        assert_eq!(attention.summary, "Claude Code needs input");
        assert_eq!(attention.body, "Approve edit?");
        assert_eq!(attention.urgency, DesktopNotificationUrgency::Critical);
    }
}
