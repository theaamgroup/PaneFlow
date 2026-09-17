//! Invocation of the on-device Foundation Models sidecar (issue #576).
//!
//! `FoundationModels.framework` is Swift-only - it exposes no Objective-C
//! interface, so `objc2` bridging is not an option. The model is reached
//! through a small Swift binary packaged next to the app executable, driven
//! one shot per pane through [`paneflow_process`]: stdin carries the
//! [`ModelRequest`] JSON, stdout carries the [`ModelResponse`] JSON.
//!
//! One shot per pane rather than a long-lived NDJSON daemon is deliberate.
//! `paneflow-process` already gives a bounded deadline, a capped capture and
//! a process-group kill on every error path, so a wedged model call cannot
//! outlive the overlay that asked for it.
//!
//! **Blocking**: every function here waits on a child. Call them from
//! `smol::unblock`, never on the GPUI thread.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::summarize::{ModelRequest, ModelResponse, normalize_summary};

/// Executable name, packaged into `Contents/MacOS/` beside `paneflow` so the
/// app's Developer ID signature covers it with no extra codesign step.
const SIDECAR_NAME: &str = "paneflow-summarize";

/// Overrides the resolved sidecar path. Development and test affordance; the
/// packaged app never sets it.
const SIDECAR_ENV: &str = "PANEFLOW_SUMMARIZE_BIN";

/// Wall clock for one pane's summary. On-device generation of a single short
/// sentence is normally 1-2 s; this is the ceiling before the pane is shown
/// as timed out and the child's process group is killed.
const CALL_DEADLINE: Duration = Duration::from_secs(10);

/// The sidecar answers with one small JSON object.
const STDOUT_CAP: u64 = 16 * 1024;

/// Why a summary is not on screen. Every variant is a state the overlay
/// renders; none of them is a dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SummaryError {
    /// No Foundation Models on this machine: pre-macOS 26, Apple Intelligence
    /// switched off, unsupported hardware, or the model not yet downloaded.
    /// The overlay stops asking for the rest of the session.
    Unavailable(String),
    /// The sidecar is not packaged or not executable.
    SidecarMissing(PathBuf),
    /// Model ran but produced nothing usable.
    Empty,
    /// Anything else: spawn failure, timeout, non-zero exit, unparseable JSON.
    Failed(String),
}

impl SummaryError {
    /// Whether this failure condemns the whole feature rather than one pane.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::SidecarMissing(_))
    }

    pub(crate) fn user_message(&self) -> String {
        match self {
            Self::Unavailable(why) => why.clone(),
            Self::SidecarMissing(_) => "Summariser not installed in this build".to_string(),
            Self::Empty => "No summary".to_string(),
            Self::Failed(why) => why.clone(),
        }
    }
}

/// Resolve the sidecar beside `executable` inside a `.app` bundle, or next to
/// the binary for a `cargo run` build. Mirrors
/// `sparkle::bundled_framework_binary`'s bundle walk so the two cannot
/// disagree about what "packaged" means.
pub(crate) fn sidecar_path(executable: &Path) -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os(SIDECAR_ENV) {
        return Some(PathBuf::from(override_path));
    }
    // Both the bundle layout (`Contents/MacOS/paneflow`) and a plain
    // `target/debug/paneflow` put the sidecar in the same directory.
    Some(executable.parent()?.join(SIDECAR_NAME))
}

/// Run one pane's summary to completion.
///
/// **Blocking.** See the module note.
pub(crate) fn summarize_blocking(
    sidecar: &Path,
    request: &ModelRequest,
) -> Result<String, SummaryError> {
    if !sidecar.is_file() {
        return Err(SummaryError::SidecarMissing(sidecar.to_path_buf()));
    }
    let payload = serde_json::to_vec(request)
        .map_err(|e| SummaryError::Failed(format!("could not encode request: {e}")))?;

    let mut cmd = Command::new(sidecar);
    cmd.arg("--json");

    let output = paneflow_process::run_with_timeout_stdin(cmd, &payload, CALL_DEADLINE, STDOUT_CAP)
        .map_err(|e| SummaryError::Failed(format!("summariser failed: {e}")))?;

    parse_output(&output.stdout)
}

/// Turn the sidecar's raw stdout into a summary or a typed failure.
///
/// Split out from [`summarize_blocking`] so the whole response contract is
/// testable without spawning anything.
pub(crate) fn parse_output(stdout: &[u8]) -> Result<String, SummaryError> {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(SummaryError::Failed("summariser returned nothing".into()));
    }
    let response: ModelResponse = serde_json::from_str(trimmed)
        .map_err(|e| SummaryError::Failed(format!("summariser returned malformed JSON: {e}")))?;

    if response.unavailable {
        return Err(SummaryError::Unavailable(
            response
                .error
                .unwrap_or_else(|| "Apple Intelligence is unavailable".into()),
        ));
    }
    if let Some(error) = response.error {
        return Err(SummaryError::Failed(error));
    }
    match response.summary.as_deref().and_then(normalize_summary) {
        Some(summary) => Ok(summary),
        None => Err(SummaryError::Empty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_sits_beside_the_app_executable() {
        // No override set in the test environment.
        unsafe { std::env::remove_var(SIDECAR_ENV) };
        let path = sidecar_path(Path::new(
            "/Applications/PaneFlow.app/Contents/MacOS/paneflow",
        ));
        assert_eq!(
            path.as_deref(),
            Some(Path::new(
                "/Applications/PaneFlow.app/Contents/MacOS/paneflow-summarize"
            ))
        );
    }

    #[test]
    fn parse_output_reads_a_summary() {
        let out = br#"{"summary":"Running the test suite."}"#;
        assert_eq!(parse_output(out).unwrap(), "Running the test suite.");
    }

    #[test]
    fn parse_output_normalizes_a_chatty_model() {
        let out = br#"{"summary":"\"Summary: waiting for approval\"\nextra"}"#;
        assert_eq!(parse_output(out).unwrap(), "waiting for approval");
    }

    #[test]
    fn unavailable_is_terminal_so_the_overlay_stops_asking() {
        let out = br#"{"unavailable":true,"error":"Apple Intelligence is off"}"#;
        let err = parse_output(out).unwrap_err();
        assert_eq!(
            err,
            SummaryError::Unavailable("Apple Intelligence is off".into())
        );
        assert!(err.is_terminal());
    }

    #[test]
    fn a_per_call_error_is_not_terminal() {
        let out = br#"{"error":"guardrail tripped"}"#;
        let err = parse_output(out).unwrap_err();
        assert!(!err.is_terminal(), "{err:?}");
    }

    #[test]
    fn empty_and_malformed_output_are_distinct_failures() {
        assert!(matches!(
            parse_output(b"").unwrap_err(),
            SummaryError::Failed(_)
        ));
        assert!(matches!(
            parse_output(b"not json").unwrap_err(),
            SummaryError::Failed(_)
        ));
        assert!(matches!(
            parse_output(br#"{"summary":"   "}"#).unwrap_err(),
            SummaryError::Empty
        ));
    }

    #[test]
    fn a_missing_sidecar_is_reported_before_anything_is_spawned() {
        let request = ModelRequest {
            instructions: "i".into(),
            prompt: "p".into(),
        };
        let missing = Path::new("/nonexistent/paneflow-summarize");
        let err = summarize_blocking(missing, &request).unwrap_err();
        assert!(matches!(err, SummaryError::SidecarMissing(_)), "{err:?}");
        assert!(err.is_terminal());
    }
}
