use super::{
    home_unavailable, merge_strict_matcher_hooks_for_events, paneflow_ipc_reachable,
    refuse_symlink, with_last_lease, with_orphan_lease, HookInstall, HookInstallResult,
    HookInstallSkip, HookLease,
};
use paneflow_agent_config::{home_dir, with_config_lock, write_json_atomic, write_text_atomic};
use std::path::{Path, PathBuf};

pub(crate) const PANEFLOW_TS_BASENAME: &str = "paneflow-status.ts";
const PI_EXTENSION_SOURCE: &str = include_str!("../../assets/pi-paneflow-status.ts");

pub(crate) struct PiExtensionGuard {
    path: PathBuf,
    lease: HookLease,
}

impl PiExtensionGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let home = home_dir().ok_or_else(home_unavailable)?;
        let directory = home.join(".pi").join("agent").join("extensions");
        let path = directory.join(PANEFLOW_TS_BASENAME);
        if !paneflow_ipc_reachable() {
            sweep_owned_file(&path);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        refuse_symlink(directory, "Pi extension")?;
        std::fs::create_dir_all(directory)?;
        let path = directory.join(PANEFLOW_TS_BASENAME);
        let mut lease = HookLease::acquire(&path)?;
        with_config_lock(&path, || {
            let created_file = !path.exists();
            write_text_atomic(&path, PI_EXTENSION_SOURCE)?;
            if created_file {
                lease.mark_created()?;
            }
            Ok(())
        })?;
        Ok(Self { path, lease })
    }
}

impl Drop for PiExtensionGuard {
    fn drop(&mut self) {
        cleanup_owned_file(&self.path, &mut self.lease);
    }
}

const GROK_HOOK_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
];

pub(crate) struct GrokHookFileGuard {
    path: PathBuf,
    lease: HookLease,
}

impl GrokHookFileGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let home = home_dir().ok_or_else(home_unavailable)?;
        let directory = home.join(".grok").join("hooks");
        let path = directory.join("paneflow.json");
        if !paneflow_ipc_reachable() {
            sweep_owned_file(&path);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        refuse_symlink(directory, "Grok hook")?;
        std::fs::create_dir_all(directory)?;
        let path = directory.join("paneflow.json");
        let mut lease = HookLease::acquire(&path)?;
        let mut root = serde_json::json!({});
        merge_strict_matcher_hooks_for_events(&mut root, GROK_HOOK_EVENTS)?;
        with_config_lock(&path, || {
            let created_file = !path.exists();
            write_json_atomic(&path, &root)?;
            if created_file {
                lease.mark_created()?;
            }
            Ok(())
        })?;
        Ok(Self { path, lease })
    }
}

impl Drop for GrokHookFileGuard {
    fn drop(&mut self) {
        cleanup_owned_file(&self.path, &mut self.lease);
    }
}

fn sweep_owned_file(path: &Path) {
    let _ = with_orphan_lease(path, path, |created| remove_created_file(path, created));
}

fn cleanup_owned_file(path: &Path, lease: &mut HookLease) {
    let _ = with_last_lease(path, lease, |created| remove_created_file(path, created));
}

/// Remove an owned file only when the lease's durable ownership bit says
/// PaneFlow created it. A pre-existing file is left in place: per the lease
/// contract (`lease.rs`), cleanup may leave a managed file behind, but it
/// must never delete a file PaneFlow did not create.
pub(super) fn remove_created_file(path: &Path, created: bool) -> std::io::Result<()> {
    if !created {
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_plugin_frames_are_notifications() {
        assert!(
            !PI_EXTENSION_SOURCE.contains("id: 1"),
            "Pi frames must be JSON-RPC notifications (no id), matching OpenCode"
        );
        assert!(
            PI_EXTENSION_SOURCE.contains("JSON.stringify({ jsonrpc: \"2.0\", method, params: p })"),
            "Pi stringify must emit jsonrpc/method/params only"
        );
    }

    #[test]
    fn preexisting_pi_extension_survives_cleanup() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("extensions");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(PANEFLOW_TS_BASENAME);
        std::fs::write(&path, "// user-managed copy\n").unwrap();

        drop(PiExtensionGuard::install_at(&directory).unwrap());

        assert!(
            path.exists(),
            "cleanup must not delete a file PaneFlow did not create"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PI_EXTENSION_SOURCE);
    }

    #[test]
    fn preexisting_grok_hook_file_survives_cleanup() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("hooks");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("paneflow.json");
        std::fs::write(&path, "{\"user\": true}\n").unwrap();

        drop(GrokHookFileGuard::install_at(&directory).unwrap());

        assert!(
            path.exists(),
            "cleanup must not delete a file PaneFlow did not create"
        );
    }

    #[test]
    fn created_grok_hook_file_is_removed_by_the_last_session() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("hooks");
        let first = GrokHookFileGuard::install_at(&directory).unwrap();
        let second = GrokHookFileGuard::install_at(&directory).unwrap();
        let path = directory.join("paneflow.json");
        assert!(path.exists());

        drop(first);
        assert!(
            path.exists(),
            "an earlier session must leave the file for the last one"
        );
        drop(second);
        assert!(
            !path.exists(),
            "the last session must remove the file PaneFlow created"
        );
    }

    #[test]
    fn orphan_sweep_preserves_preexisting_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("paneflow.json");
        std::fs::write(&path, "{}").unwrap();

        sweep_owned_file(&path);

        assert!(
            path.exists(),
            "the orphan sweep must not delete a file PaneFlow did not create"
        );
    }

    #[test]
    fn orphan_sweep_removes_created_file_after_crash() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("paneflow.json");
        std::fs::write(&path, "{}").unwrap();
        let mut lease = HookLease::acquire(&path).unwrap();
        lease.mark_created().unwrap();
        drop(lease); // simulated crash: the lock releases, the marker persists

        sweep_owned_file(&path);

        assert!(
            !path.exists(),
            "the orphan sweep must remove a crashed session's created file"
        );
    }
}
