use super::{
    home_unavailable, is_paneflow_hook_command, merge_strict_matcher_hooks_for_events,
    paneflow_ipc_reachable, refuse_symlink, with_last_lease, with_orphan_lease, HookInstall,
    HookInstallResult, HookInstallSkip, HookLease,
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
            sweep_matching_owned_file(&path, PI_EXTENSION_SOURCE);
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
        cleanup_matching_owned_file(&self.path, &mut self.lease, PI_EXTENSION_SOURCE);
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
            let source = grok_source()?;
            sweep_accepted_owned_file(&path, &|existing| {
                is_own_or_sibling_rendering(existing, &source)
            });
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
        // The file embeds this instance's hook path; another PaneFlow
        // instance (a different `PANEFLOW_BIN_DIR`) renders different bytes
        // for the same hooks, and that file serves this session as well.
        with_config_lock(&path, || {
            install_accepted_owned_file(&path, &source, &mut lease, &|existing| {
                is_own_or_sibling_rendering(existing, &source)
            })
        })?;
        Ok(Self {
            path,
            lease,
            source,
        })
    }
}

impl Drop for GrokHookFileGuard {
    fn drop(&mut self) {
        let source = std::mem::take(&mut self.source);
        cleanup_accepted_owned_file(&self.path, &mut self.lease, &|existing| {
            is_own_or_sibling_rendering(existing, &source)
        });
    }
}

fn grok_source() -> std::io::Result<String> {
    let mut root = serde_json::json!({});
    merge_strict_matcher_hooks_for_events(&mut root, GROK_HOOK_EVENTS)?;
    Ok(serde_json::to_string_pretty(&root).map_err(std::io::Error::other)? + "\n")
}

/// True when `existing` is `source` byte for byte, or the same hooks as
/// rendered by a sibling PaneFlow instance (see
/// [`is_sibling_instance_rendering`]).
pub(super) fn is_own_or_sibling_rendering(existing: &str, source: &str) -> bool {
    existing == source || is_sibling_instance_rendering(existing, source)
}

/// True when `existing` is the JSON `source` renders, differing only in the
/// PaneFlow hook program each command names.
///
/// A managed hook file embeds the absolute `paneflow-ai-hook` path, so two
/// PaneFlow instances with different `PANEFLOW_BIN_DIR` /
/// `PANEFLOW_AI_HOOK_PATH` values render different bytes for the same hooks.
/// The comparison walks both documents in lockstep: every key, array length,
/// number and boolean must match, and a string may differ only where both
/// sides are a PaneFlow hook command for the same event. A file carrying any
/// other command, event, or key is not a sibling rendering, whatever else it
/// shares with `source`.
pub(super) fn is_sibling_instance_rendering(existing: &str, source: &str) -> bool {
    let (Ok(existing), Ok(expected)) = (
        serde_json::from_str::<serde_json::Value>(existing),
        serde_json::from_str::<serde_json::Value>(source),
    ) else {
        return false;
    };
    equal_up_to_hook_program(&existing, &expected)
}

fn equal_up_to_hook_program(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            actual.len() == expected.len()
                && expected.iter().all(|(key, value)| {
                    actual
                        .get(key)
                        .is_some_and(|other| equal_up_to_hook_program(other, value))
                })
        }
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(other, value)| equal_up_to_hook_program(other, value))
        }
        (Value::String(actual), Value::String(expected)) => {
            actual == expected || same_paneflow_hook_event(actual, expected)
        }
        _ => actual == expected,
    }
}

/// Both strings are PaneFlow hook commands (`<program> <event>`, the program
/// possibly quoted) that name the same event.
fn same_paneflow_hook_event(actual: &str, expected: &str) -> bool {
    is_paneflow_hook_command(actual)
        && is_paneflow_hook_command(expected)
        && hook_command_event(actual) == hook_command_event(expected)
}

fn hook_command_event(command: &str) -> Option<&str> {
    command.trim_end().rsplit(char::is_whitespace).next()
}

pub(super) fn install_owned_file(
    path: &Path,
    source: &str,
    lease: &mut HookLease,
) -> std::io::Result<()> {
    install_accepted_owned_file(path, source, lease, &|existing| existing == source)
}

/// Publish `source` at `path` unless a file is already there. An existing
/// file that `accepts` (this instance's own bytes, or a sibling instance's
/// rendering of the same content) is left exactly as it is and never marked
/// created, so cleanup can only ever delete what PaneFlow itself wrote; any
/// other existing file is refused with `AlreadyExists`.
pub(super) fn install_accepted_owned_file(
    path: &Path,
    source: &str,
    lease: &mut HookLease,
    accepts: &dyn Fn(&str) -> bool,
) -> std::io::Result<()> {
    refuse_symlink(path, "managed hook")?;
    match read_optional_text(path)? {
        Some(existing) if accepts(&existing) => Ok(()),
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

fn remove_unchanged_file(
    path: &Path,
    created: bool,
    accepts: &dyn Fn(&str) -> bool,
) -> std::io::Result<()> {
    if !created {
        return Ok(());
    }
    refuse_symlink(path, "managed hook")?;
    if read_optional_text(path)?.is_some_and(|existing| accepts(&existing)) {
        remove_created_file(path, true)?;
    }
    Ok(())
}

pub(super) fn sweep_matching_owned_file(path: &Path, source: &str) {
    sweep_accepted_owned_file(path, &|existing| existing == source);
}

pub(super) fn sweep_accepted_owned_file(path: &Path, accepts: &dyn Fn(&str) -> bool) {
    let _ = with_orphan_lease(path, path, |created| {
        remove_unchanged_file(path, created, accepts)
    });
}

pub(super) fn cleanup_matching_owned_file(path: &Path, lease: &mut HookLease, source: &str) {
    cleanup_accepted_owned_file(path, lease, &|existing| existing == source);
}

/// Last-session cleanup: remove the file only when the lease's durable
/// ownership bit says a PaneFlow session created it and its content still
/// `accepts` (unchanged by the user; a sibling instance's rendering counts,
/// since the bit proves PaneFlow wrote the file and the shape check proves
/// nothing else was added).
pub(super) fn cleanup_accepted_owned_file(
    path: &Path,
    lease: &mut HookLease,
    accepts: &dyn Fn(&str) -> bool,
) {
    let _ = with_last_lease(path, lease, |created| {
        remove_unchanged_file(path, created, accepts)
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

/// Test fixture: `source` as a sibling PaneFlow instance would render it,
/// every PaneFlow hook command re-pointed at `/elsewhere/bin/paneflow-ai-hook`.
#[cfg(test)]
pub(crate) fn render_as_sibling_instance(source: &str) -> String {
    fn repoint(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => object.values_mut().for_each(repoint),
            serde_json::Value::Array(array) => array.iter_mut().for_each(repoint),
            serde_json::Value::String(command) if is_paneflow_hook_command(command) => {
                let event = hook_command_event(command).unwrap_or_default().to_owned();
                *command = format!("/elsewhere/bin/paneflow-ai-hook {event}");
            }
            _ => {}
        }
    }
    let mut root: serde_json::Value = serde_json::from_str(source).unwrap();
    repoint(&mut root);
    serde_json::to_string_pretty(&root).unwrap() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_rendering_differs_only_in_the_hook_program() {
        let source = grok_source().unwrap();
        let sibling = render_as_sibling_instance(&source);
        assert_ne!(sibling, source, "the fixture must change the bytes");
        assert!(is_sibling_instance_rendering(&sibling, &source));
        assert!(is_own_or_sibling_rendering(&source, &source));

        // A quoted program path (spaces) is still the same hook.
        let quoted = sibling.replace(
            "/elsewhere/bin/paneflow-ai-hook",
            "'/my dir/paneflow-ai-hook'",
        );
        assert_ne!(quoted, sibling);
        assert!(is_sibling_instance_rendering(&quoted, &source));

        // Same program, different event: not the same hook.
        let swapped = sibling.replacen("paneflow-ai-hook Stop", "paneflow-ai-hook Notification", 1);
        assert!(!is_sibling_instance_rendering(&swapped, &source));
        // A user command in place of a PaneFlow hook.
        let user = sibling.replacen("/elsewhere/bin/paneflow-ai-hook Stop", "my-hook Stop", 1);
        assert!(!is_sibling_instance_rendering(&user, &source));
        // Extra content beyond the rendered shape.
        let mut extended: serde_json::Value = serde_json::from_str(&sibling).unwrap();
        extended["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "command", "command": "my-hook Stop"}));
        assert!(!is_sibling_instance_rendering(
            &extended.to_string(),
            &source
        ));
        extended["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .pop();
        extended["permissions"] = serde_json::json!({});
        assert!(!is_sibling_instance_rendering(
            &extended.to_string(),
            &source
        ));
        // Not JSON at all.
        assert!(!is_sibling_instance_rendering("- insert:", &source));
    }

    #[test]
    fn sibling_instance_grok_hook_file_is_shared_not_owned() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("hooks");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("paneflow.json");
        let sibling = render_as_sibling_instance(&grok_source().unwrap());
        std::fs::write(&path, &sibling).unwrap();

        let guard = GrokHookFileGuard::install_at(&directory)
            .expect("a sibling instance's hook file must serve this session");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), sibling);
        drop(guard);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            sibling,
            "the sibling instance's file is not ours to delete"
        );
    }

    #[test]
    fn grok_hook_file_with_a_user_command_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("hooks");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("paneflow.json");
        let user = render_as_sibling_instance(&grok_source().unwrap()).replacen(
            "/elsewhere/bin/paneflow-ai-hook Stop",
            "my-hook Stop",
            1,
        );
        std::fs::write(&path, &user).unwrap();

        let error = GrokHookFileGuard::install_at(&directory).err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), user);
    }

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
        sweep_matching_owned_file(&path, "original managed content");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "user changes");
    }

    #[test]
    fn orphan_sweep_preserves_preexisting_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("paneflow.json");
        std::fs::write(&path, "{}").unwrap();

        sweep_matching_owned_file(&path, "{}");

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

        sweep_matching_owned_file(&path, "{}");

        assert!(
            !path.exists(),
            "the orphan sweep must remove a crashed session's created file"
        );
    }
}
