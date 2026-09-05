use super::{
    home_unavailable, merge_strict_matcher_hooks_for_events, paneflow_ipc_reachable,
    refuse_symlink, with_last_lease, with_orphan_lease, HookInstall, HookInstallResult,
    HookInstallSkip, HookLease,
};
use paneflow_agent_config::{home_dir, read_optional_text, with_config_lock};
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
            sweep_owned_file(&path, PI_EXTENSION_SOURCE);
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
            install_owned_file(&path, PI_EXTENSION_SOURCE, &mut lease)?;
            Ok(())
        })?;
        Ok(Self { path, lease })
    }
}

impl Drop for PiExtensionGuard {
    fn drop(&mut self) {
        cleanup_owned_file(&self.path, &mut self.lease, PI_EXTENSION_SOURCE);
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
    source: String,
    lease: HookLease,
}

impl GrokHookFileGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let home = home_dir().ok_or_else(home_unavailable)?;
        let directory = home.join(".grok").join("hooks");
        let path = directory.join("paneflow.json");
        if !paneflow_ipc_reachable() {
            sweep_owned_file(&path, &grok_source()?);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        refuse_symlink(directory, "Grok hook")?;
        std::fs::create_dir_all(directory)?;
        let path = directory.join("paneflow.json");
        let mut lease = HookLease::acquire(&path)?;
        let source = grok_source()?;
        with_config_lock(&path, || install_owned_file(&path, &source, &mut lease))?;
        Ok(Self {
            path,
            lease,
            source,
        })
    }
}

impl Drop for GrokHookFileGuard {
    fn drop(&mut self) {
        cleanup_owned_file(&self.path, &mut self.lease, &self.source);
    }
}

fn grok_source() -> std::io::Result<String> {
    let mut root = serde_json::json!({});
    merge_strict_matcher_hooks_for_events(&mut root, GROK_HOOK_EVENTS)?;
    Ok(serde_json::to_string_pretty(&root).map_err(std::io::Error::other)? + "\n")
}

fn install_owned_file(path: &Path, source: &str, lease: &mut HookLease) -> std::io::Result<()> {
    refuse_symlink(path, "managed hook")?;
    match read_optional_text(path)? {
        Some(existing) if existing == source => Ok(()),
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} contains user changes; refusing to overwrite it",
                path.display()
            ),
        )),
        None => {
            use std::io::Write;
            let parent = path
                .parent()
                .ok_or_else(|| std::io::Error::other("hook path has no parent"))?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.write_all(source.as_bytes())?;
            file.as_file().sync_all()?;
            // A user can create the destination after the read; an exclusive
            // publish preserves their file even when they do not use our lock.
            file.persist_noclobber(path).map_err(|error| error.error)?;
            lease.mark_created()
        }
    }
}

fn remove_unchanged_file(path: &Path, created: bool, source: &str) -> std::io::Result<()> {
    if !created {
        return Ok(());
    }
    refuse_symlink(path, "managed hook")?;
    if read_optional_text(path)?.as_deref() == Some(source) {
        remove_created_file(path, true)?;
    }
    Ok(())
}

fn sweep_owned_file(path: &Path, source: &str) {
    let _ = with_orphan_lease(path, path, |created| {
        remove_unchanged_file(path, created, source)
    });
}

fn cleanup_owned_file(path: &Path, lease: &mut HookLease, source: &str) {
    let _ = with_last_lease(path, lease, |created| {
        remove_unchanged_file(path, created, source)
    });
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

        assert!(PiExtensionGuard::install_at(&directory).is_err());

        assert!(
            path.exists(),
            "cleanup must not delete a file PaneFlow did not create"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "// user-managed copy\n"
        );
    }

    #[test]
    fn preexisting_grok_hook_file_survives_cleanup() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("hooks");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("paneflow.json");
        std::fs::write(&path, "{\"user\": true}\n").unwrap();

        assert!(GrokHookFileGuard::install_at(&directory).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"user\": true}\n"
        );

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
    fn cleanup_preserves_files_edited_during_a_session() {
        let temp = tempfile::TempDir::new().unwrap();
        let pi = PiExtensionGuard::install_at(&temp.path().join("pi")).unwrap();
        let grok = GrokHookFileGuard::install_at(&temp.path().join("grok")).unwrap();
        let paths = [pi.path.clone(), grok.path.clone()];
        for path in &paths {
            std::fs::write(path, "user changes").unwrap();
        }
        drop(pi);
        drop(grok);
        for path in paths {
            assert_eq!(std::fs::read_to_string(path).unwrap(), "user changes");
        }
    }

    #[test]
    fn orphan_sweep_preserves_modified_owned_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("hook");
        std::fs::write(&path, "user changes").unwrap();
        let mut lease = HookLease::acquire(&path).unwrap();
        lease.mark_created().unwrap();
        drop(lease);
        sweep_owned_file(&path, "original managed content");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "user changes");
    }

    #[test]
    fn orphan_sweep_preserves_preexisting_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("paneflow.json");
        std::fs::write(&path, "{}").unwrap();

        sweep_owned_file(&path, "{}");

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

        sweep_owned_file(&path, "{}");

        assert!(
            !path.exists(),
            "the orphan sweep must remove a crashed session's created file"
        );
    }
}
