//! Locating and driving the `paneflow-agent-summary` helper (issue #576).
//!
//! The helper is a Swift binary (`native/agent-summary/main.swift`) because
//! `FoundationModels.framework` has no Objective-C surface to bridge from
//! Rust. It is installed by `scripts/bundle-macos.sh` as
//! `Contents/Helpers/paneflow-agent-summary`, never staged by `build.rs`, and
//! every call here goes through `paneflow-process` so a wedged model cannot
//! hold a thread: a deadline, a stdout cap, and - for the summary itself - a
//! cancel flag the overlay flips on dismiss.
//!
//! Everything in this module is **blocking** and belongs on a background
//! worker (`cx.background_spawn`), never on the GPUI thread.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use paneflow_process::{ProcError, run_with_timeout, run_with_timeout_stdin_cancellable};

use super::prompt::{ModelAvailability, SidecarReply, parse_probe, parse_reply};

/// File name of the helper, in the bundle and beside a dev binary.
pub(crate) const SIDECAR_NAME: &str = "paneflow-agent-summary";
/// Explicit override for the helper's path (a dev build, a test harness).
pub(crate) const ENV_OVERRIDE: &str = "PANEFLOW_AGENT_SUMMARY_BIN";
/// `--probe` is a framework availability lookup; anything slower is a hung
/// helper.
const PROBE_DEADLINE: Duration = Duration::from_secs(5);
/// One pane's summary. The model answers in about a second on an M-series
/// Mac; the bound covers a cold model and a busy machine, not a wedge.
pub(crate) const SUMMARY_DEADLINE: Duration = Duration::from_secs(20);
/// A reply is one JSON line of a few hundred characters.
const STDOUT_CAP: u64 = 64 * 1024;

/// Where the helper is for this process, or `None` when this build has
/// none (a `cargo run` without `scripts/build-agent-summary.sh`).
pub(crate) fn locate() -> Option<PathBuf> {
    locate_from(std::env::var_os(ENV_OVERRIDE), std::env::current_exe().ok())
}

/// The lookup order, pure so it can be tested against a fake bundle:
/// 1. `PANEFLOW_AGENT_SUMMARY_BIN`, when it names an existing file;
/// 2. `paneflow-agent-summary` beside the executable (a dev build);
/// 3. `Contents/Helpers/paneflow-agent-summary` of the enclosing `.app`.
pub(crate) fn locate_from(
    override_path: Option<OsString>,
    executable: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = override_path.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
        log::warn!(
            "{ENV_OVERRIDE}={} is not a file; falling back to the bundled helper",
            path.display()
        );
    }
    let executable = executable?;
    let sibling = executable.parent()?.join(SIDECAR_NAME);
    if sibling.is_file() {
        return Some(sibling);
    }
    let bundled = bundled_helper(&executable)?;
    bundled.is_file().then_some(bundled)
}

/// `<App>.app/Contents/MacOS/paneflow` -> `<App>.app/Contents/Helpers/<name>`,
/// the same walk `sparkle::bundled_framework_binary` does for Frameworks.
fn bundled_helper(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    if contents.parent()?.extension()? != "app" {
        return None;
    }
    Some(contents.join("Helpers").join(SIDECAR_NAME))
}

/// Ask the helper whether the on-device model can answer right now.
pub(crate) fn probe(bin: &Path) -> ModelAvailability {
    let mut cmd = Command::new(bin);
    cmd.arg("--probe");
    match run_with_timeout(cmd, PROBE_DEADLINE, STDOUT_CAP) {
        Ok(out) => parse_probe(&out.stdout),
        Err(error) => {
            log::warn!("agent summary probe failed: {error}");
            ModelAvailability::Unavailable(describe(&error))
        }
    }
}

/// Summarise one pane: `request` is the line `prompt::request_json` built.
/// `cancel` is polled by the process runner; flipping it kills the helper
/// and yields an `Error` the caller discards.
pub(crate) fn summarize(bin: &Path, request: &str, cancel: &AtomicBool) -> SidecarReply {
    match run_with_timeout_stdin_cancellable(
        Command::new(bin),
        request.as_bytes(),
        SUMMARY_DEADLINE,
        STDOUT_CAP,
        cancel,
    ) {
        Ok(out) => parse_reply(&out.stdout),
        Err(ProcError::Cancelled) => SidecarReply::Error("cancelled".to_owned()),
        Err(error) => {
            log::warn!("agent summary helper failed: {error}");
            SidecarReply::Error(describe(&error))
        }
    }
}

fn describe(error: &ProcError) -> String {
    match error {
        ProcError::Timeout => "the on-device model took too long to answer".to_owned(),
        ProcError::Spawn(_) => "the summary helper could not be started".to_owned(),
        _ => "the summary helper failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn locate_prefers_override_then_sibling_then_bundle_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let exe = root.join("Fake.app/Contents/MacOS/paneflow");
        touch(&exe);

        assert_eq!(locate_from(None, Some(exe.clone())), None, "nothing built");

        let helper = root.join("Fake.app/Contents/Helpers").join(SIDECAR_NAME);
        touch(&helper);
        assert_eq!(locate_from(None, Some(exe.clone())), Some(helper.clone()));

        let sibling = root.join("Fake.app/Contents/MacOS").join(SIDECAR_NAME);
        touch(&sibling);
        assert_eq!(
            locate_from(None, Some(exe.clone())),
            Some(sibling.clone()),
            "a sibling (dev build) wins over the bundle path"
        );

        let custom = root.join("elsewhere").join("summary-bin");
        touch(&custom);
        assert_eq!(
            locate_from(Some(custom.clone().into_os_string()), Some(exe.clone())),
            Some(custom)
        );
        assert_eq!(
            locate_from(
                Some(root.join("missing").into_os_string()),
                Some(exe.clone())
            ),
            Some(sibling),
            "a dangling override falls back rather than disabling the feature"
        );
        assert_eq!(
            locate_from(Some(OsString::new()), None),
            None,
            "no executable and an empty override is no helper"
        );
    }

    #[test]
    fn a_dev_binary_outside_a_bundle_only_looks_beside_itself() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("target/debug/paneflow");
        touch(&exe);
        assert_eq!(locate_from(None, Some(exe.clone())), None);
        // `target/Helpers` is not a bundle layout and must not be consulted.
        touch(&dir.path().join("target/Helpers").join(SIDECAR_NAME));
        assert_eq!(locate_from(None, Some(exe.clone())), None);
        let sibling = dir.path().join("target/debug").join(SIDECAR_NAME);
        touch(&sibling);
        assert_eq!(locate_from(None, Some(exe)), Some(sibling));
    }

    /// The driver end-to-end against a stand-in helper: a shell script that
    /// answers the probe and echoes a canned summary, so the process plumbing
    /// (argv, stdin, stdout parsing) is exercised without the real model.
    #[cfg(unix)]
    #[test]
    fn probe_and_summarize_drive_a_fake_helper() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-helper");
        fs::write(
            &bin,
            "#!/bin/sh\nif [ \"$1\" = \"--probe\" ]; then echo '{\"available\":true}'; exit 0; fi\n\
             req=$(cat)\ncase \"$req\" in *\"\\\"agent\\\":\\\"Codex\\\"\"*) echo '{\"summary\":\"Codex is editing.\"}';;\n\
             *) echo '{\"error\":\"unexpected request\"}';; esac\n",
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(probe(&bin), ModelAvailability::Available);
        let cancel = AtomicBool::new(false);
        assert_eq!(
            summarize(
                &bin,
                &super::super::prompt::request_json("Codex", "Working", "tail"),
                &cancel
            ),
            SidecarReply::Summary("Codex is editing.".to_owned())
        );
        assert_eq!(
            summarize(&bin, "{\"agent\":\"Other\"}", &cancel),
            SidecarReply::Error("unexpected request".to_owned())
        );
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            summarize(&bin, "{}", &cancelled),
            SidecarReply::Error("cancelled".to_owned())
        );
    }

    #[test]
    fn a_missing_helper_probes_as_unavailable_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe(&dir.path().join("nope")),
            ModelAvailability::Unavailable(r) if r.contains("could not be started")
        ));
    }
}
