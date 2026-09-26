//! First-launch cleanup of the retired MCP bridge (issue #857).
//!
//! Older builds extracted `paneflow-mcp` on every launch and could register
//! it with Claude Code, Codex, Gemini and opencode. [`spawn_cleanup`] removes
//! those entries, and a later launch that finds none left deletes the binary,
//! through `paneflow_mcp_install::remove_legacy_bridge`. Issue #868 deletes this
//! module with that crate once the cleanup has shipped for two releases.

use std::path::PathBuf;

/// Start the cleanup on a background thread, only while the extracted bridge
/// binary still exists (one `stat` once a pass has deleted it).
///
/// Called from `PaneFlowApp::new` right after the IPC singleton guard: a
/// second instance that is about to exit never starts editing agent configs,
/// so it cannot leave a half-written temp file behind. The thread reads and
/// edits agent configs under the config lock, so it stays off the UI thread.
///
/// Besides the files the process environment names, the pass probes the
/// ones the login shell's profile names (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
/// …), captured by `login_shell_env` on a GUI launch without exporting them.
/// That capture finished before the app started, so nothing here waits on it.
pub(crate) fn spawn_cleanup() {
    let Some(bridge) = present_bridge_binary() else {
        return;
    };
    let edits_refused = crate::runtime_paths::legacy_bridge_edit_refusal();
    let login_shell =
        paneflow_mcp_install::AgentConfigEnv::from_lookup(crate::login_shell_env::side_env);
    if let Err(e) = std::thread::Builder::new()
        .name("paneflow-bridge-cleanup".into())
        .spawn(move || {
            paneflow_mcp_install::remove_legacy_bridge(&bridge, edits_refused, Some(&login_shell));
        })
    {
        log::warn!("paneflow: could not start the MCP bridge cleanup: {e}");
    }
}

/// The legacy bridge path, when something is still there.
fn present_bridge_binary() -> Option<PathBuf> {
    let bridge = crate::runtime_paths::bridge_binary_path()?;
    std::fs::symlink_metadata(&bridge).is_ok().then_some(bridge)
}

#[cfg(test)]
mod tests {
    use crate::source_probe::source_slice;

    /// The cleanup must start after the singleton guard in
    /// `ipc::start_server` has had its chance to exit the process.
    #[test]
    fn the_cleanup_starts_after_the_ipc_singleton_guard() {
        let constructor = source_slice(
            include_str!("app/bootstrap.rs"),
            "pub(crate) fn new(",
            "\n    }\n",
        );
        let guard = constructor
            .find("ipc::start_server()")
            .expect("PaneFlowApp::new starts the IPC server");
        let spawn = constructor
            .find(&format!("legacy_bridge::{}()", "spawn_cleanup"))
            .expect("PaneFlowApp::new starts the bridge cleanup");
        assert!(
            guard < spawn,
            "the bridge cleanup must start after the singleton guard"
        );
        assert!(
            !include_str!("main.rs").contains(&format!("{}(", "remove_legacy_bridge")),
            "main() must not start the cleanup before the singleton guard"
        );
    }

    #[test]
    fn the_login_shell_captures_every_variable_the_cleanup_reads() {
        assert_eq!(
            crate::login_shell_env::SIDE_ENV_VARS,
            paneflow_mcp_install::AgentConfigEnv::VARS
        );
    }
}
