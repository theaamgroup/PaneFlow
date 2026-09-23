use super::{
    home_unavailable, is_paneflow_hook_command, merge_strict_matcher_hooks_for_events,
    paneflow_ipc_reachable, refuse_symlink, with_last_lease, with_orphan_lease, HookInstall,
    HookInstallResult, HookInstallSkip, HookLease,
};
use paneflow_agent_config::claude_hooks::{paneflow_hook_program_token, render_hook_command};
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
            // The extension is TypeScript, so byte equality cannot recognise
            // an older rendering. Ownership is the `.created` marker, the
            // same rule repair uses.
            sweep_created_owned_file(&path);
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
            // Not `install_owned_file`: that entry point repairs only a JSON
            // rendering. A `.created` last-holder file is replaced even when
            // the bytes are an older TypeScript extension. A file without
            // the marker is still refused.
            install_owned_file_repairing(
                &path,
                PI_EXTENSION_SOURCE,
                &mut lease,
                &|existing| existing == PI_EXTENSION_SOURCE,
                &pi_extension_owned_by_marker,
            )?;
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
    is_paneflow_rendering_shape(existing, source, &|program| {
        hook_program_is_runnable(program)
            && !hook_program_is_prunable(program, own_version_dir().as_deref())
    })
}

/// True when `existing` is a PaneFlow rendering of `source` whose hook
/// commands may name any program `accept_program` allows: the structural
/// half of [`is_sibling_instance_rendering`], used to recognise a stale
/// PaneFlow-owned file that should be repaired rather than refused.
fn is_paneflow_rendering_shape(
    existing: &str,
    source: &str,
    accept_program: &dyn Fn(&Path) -> bool,
) -> bool {
    let (Ok(existing), Ok(expected)) = (
        serde_json::from_str::<serde_json::Value>(existing),
        serde_json::from_str::<serde_json::Value>(source),
    ) else {
        return false;
    };
    equal_up_to_hook_program(&existing, &expected, accept_program)
}

fn equal_up_to_hook_program(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    accept_program: &dyn Fn(&Path) -> bool,
) -> bool {
    use serde_json::Value;
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            actual.len() == expected.len()
                && expected.iter().all(|(key, value)| {
                    actual
                        .get(key)
                        .is_some_and(|other| equal_up_to_hook_program(other, value, accept_program))
                })
        }
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(other, value)| equal_up_to_hook_program(other, value, accept_program))
        }
        (Value::String(actual), Value::String(expected)) => {
            actual == expected || same_paneflow_hook_event(actual, expected, accept_program)
        }
        _ => actual == expected,
    }
}

/// Both strings are PaneFlow hook commands (`<program> <event>`, the program
/// possibly quoted) that name the same event, and the program `actual`
/// names is still runnable.
///
/// A crashed session can leave an owned file naming a version-pinned hook
/// binary that a later launch pruned; accepting that rendering would hand
/// this session hooks that invoke a missing executable, so a sibling program
/// must be an existing executable file, not merely the right basename.
fn same_paneflow_hook_event(
    actual: &str,
    expected: &str,
    accept_program: &dyn Fn(&Path) -> bool,
) -> bool {
    let Some(event) = hook_command_event(expected).filter(|_| is_paneflow_hook_command(expected))
    else {
        return false;
    };
    paneflow_hook_program_token(actual).is_some_and(|program: String| {
        let program = Path::new(&program);
        // The whole command must be the canonical rendering for that
        // program and event: `<program> Stop; touch /tmp/x Stop` shares the
        // first and last tokens with a hook and is not one.
        actual == render_hook_command(program, event) && accept_program(program)
    })
}

/// `access(X_OK)` through [`super::is_executable`], not a mode bitmask: a
/// file with only an other-class execute bit, an ACL denial, or a `noexec`
/// mount would pass the bitmask and then fail every hook with
/// `Permission denied`.
fn hook_program_is_runnable(program: &Path) -> bool {
    program.is_absolute() && program.is_file() && super::is_executable(program)
}

/// The version-pinned `bin/<version>/` directory this shim was launched
/// from: `PANEFLOW_BIN_DIR` when the app advertised it, else the shim's own
/// location.
fn own_version_dir() -> Option<PathBuf> {
    std::env::var_os("PANEFLOW_BIN_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(std::env::current_exe().ok()?.parent()?.to_path_buf()))
}

/// True when `program` sits in another version's `bin/<version>/` beside
/// this shim's own, the one layout the app prunes once the process that
/// staged it is gone. A sibling instance's hook there is runnable today and
/// can vanish mid-session after that instance exits, so it is never adopted;
/// the durable copy under the data directory, or this shim's own leased
/// version directory, is fine.
fn hook_program_is_prunable(program: &Path, own_version_dir: Option<&Path>) -> bool {
    let (Some(own), Some(parent)) = (own_version_dir, program.parent()) else {
        return false;
    };
    parent != own && parent.parent() == own.parent()
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
/// created, so cleanup can only ever delete what PaneFlow itself wrote. A
/// stale file is repaired only when [`is_paneflow_rendering_shape`] says it
/// is still a PaneFlow rendering; any other existing file is refused with
/// `AlreadyExists`. That shape proof is what keeps a user-edited JSON hook
/// from being overwritten.
pub(super) fn install_accepted_owned_file(
    path: &Path,
    source: &str,
    lease: &mut HookLease,
    accepts: &dyn Fn(&str) -> bool,
) -> std::io::Result<()> {
    install_owned_file_repairing(path, source, lease, accepts, &|existing| {
        is_paneflow_rendering_shape(existing, source, &|_| true)
    })
}

fn install_owned_file_repairing(
    path: &Path,
    source: &str,
    lease: &mut HookLease,
    accepts: &dyn Fn(&str) -> bool,
    proves_owned: &dyn Fn(&str) -> bool,
) -> std::io::Result<()> {
    refuse_symlink(path, "managed hook")?;
    match read_optional_text(path)? {
        Some(existing) if accepts(&existing) => Ok(()),
        Some(existing) => {
            if repair_stale_owned_file(path, &existing, source, lease, proves_owned)? {
                return Ok(());
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "{} contains user changes; refusing to overwrite it",
                    path.display()
                ),
            ))
        }
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

/// Replace a stale file PaneFlow itself wrote with this instance's
/// rendering. Three proofs are required, and a miss on any of them leaves
/// the file alone: `proves_owned` says the bytes are still PaneFlow's, no
/// other session holds the lease (a live sibling is using the file), and the
/// durable `.created` marker says PaneFlow created it.
///
/// JSON hooks pass [`is_paneflow_rendering_shape`], so only the hook program
/// may differ and a user edit is refused. That repairs a crashed session
/// whose version-pinned hook binary was later pruned. The Pi extension is
/// TypeScript, so that parse cannot succeed; its caller accepts any bytes
/// and the marker plus last-holder are the proof. A file without the marker
/// is not repaired. Returns whether it repaired.
fn repair_stale_owned_file(
    path: &Path,
    existing: &str,
    source: &str,
    lease: &mut HookLease,
    proves_owned: &dyn Fn(&str) -> bool,
) -> std::io::Result<bool> {
    if !proves_owned(existing) {
        return Ok(false);
    }
    let Some(mut last) = lease.try_take_last()? else {
        // `try_take_last` released the shared lease; hold one again so the
        // refusal below leaves this session where it started.
        *lease = HookLease::acquire(path)?;
        return Ok(false);
    };
    // Peek only. `take_created` deletes the marker; doing that before the
    // replacement is durable makes a failed persist look unowned, so the
    // next repair refuses the stale file.
    let owned = last.is_created();
    let repaired = if owned {
        use std::io::Write;
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("hook path has no parent"))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(source.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        last.take_created()?;
        true
    } else {
        false
    };
    drop(last);
    *lease = HookLease::acquire(path)?;
    if repaired {
        lease.mark_created()?;
    }
    Ok(repaired)
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

/// Content gate for the Pi extension. It is TypeScript, so
/// [`is_paneflow_rendering_shape`] cannot match an older copy. Repair and
/// the orphan sweep still require the durable `.created` marker, and repair
/// also requires this session to be the last holder.
fn pi_extension_owned_by_marker(_existing: &str) -> bool {
    true
}

/// Remove a PaneFlow-created Pi extension even when its bytes are not the
/// current source. An upgrade changes [`PI_EXTENSION_SOURCE`]; comparing
/// bytes would leave the stale file loaded.
fn sweep_created_owned_file(path: &Path) {
    sweep_accepted_owned_file(path, &pi_extension_owned_by_marker);
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
/// every PaneFlow hook command re-pointed at `program` (an executable stub from `sibling_hook_program`).
#[cfg(test)]
pub(crate) fn render_as_sibling_instance(source: &str, program: &Path) -> String {
    fn repoint(value: &mut serde_json::Value, program: &str) {
        match value {
            serde_json::Value::Object(object) => object
                .values_mut()
                .for_each(|value| repoint(value, program)),
            serde_json::Value::Array(array) => {
                array.iter_mut().for_each(|value| repoint(value, program))
            }
            serde_json::Value::String(command) if is_paneflow_hook_command(command) => {
                let event = hook_command_event(command).unwrap_or_default().to_owned();
                *command = format!("{program} {event}");
            }
            _ => {}
        }
    }
    let mut root: serde_json::Value = serde_json::from_str(source).unwrap();
    repoint(&mut root, &program.to_string_lossy());
    serde_json::to_string_pretty(&root).unwrap() + "\n"
}

/// Writes an executable stub named `paneflow-ai-hook` under `directory` and
/// returns its path, the program a sibling-instance fixture points at.
#[cfg(test)]
pub(crate) fn sibling_hook_program(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(directory).unwrap();
    let program = directory.join("paneflow-ai-hook");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    program
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_in_another_versions_bin_dir_is_prunable_and_never_adopted() {
        let own = Path::new("/cache/paneflow/bin/0.6.1");
        assert!(hook_program_is_prunable(
            Path::new("/cache/paneflow/bin/0.6.0/paneflow-ai-hook"),
            Some(own)
        ));
        assert!(!hook_program_is_prunable(
            Path::new("/cache/paneflow/bin/0.6.1/paneflow-ai-hook"),
            Some(own)
        ));
        assert!(!hook_program_is_prunable(
            Path::new("/data/paneflow/bin/paneflow-ai-hook"),
            Some(own)
        ));
        assert!(!hook_program_is_prunable(
            Path::new("/cache/paneflow/bin/0.6.0/paneflow-ai-hook"),
            None
        ));

        // End to end: a runnable sibling hook under a prunable version dir is
        // still refused, while the same file under a durable path is adopted.
        let temp = tempfile::TempDir::new().unwrap();
        let versioned = temp.path().join("bin");
        let source = grok_source().unwrap();
        let prunable = sibling_hook_program(&versioned.join("0.6.0"));
        let own_dir = versioned.join("0.6.1");
        std::fs::create_dir_all(&own_dir).unwrap();
        let rendering = render_as_sibling_instance(&source, &prunable);
        let program = paneflow_hook_program_token(
            rendering
                .lines()
                .find(|line| line.contains("paneflow-ai-hook"))
                .and_then(|line| line.split('"').nth(3))
                .unwrap(),
        )
        .unwrap();
        assert!(hook_program_is_runnable(Path::new(&program)));
        assert!(hook_program_is_prunable(
            Path::new(&program),
            Some(&own_dir)
        ));
    }

    #[test]
    fn a_hook_the_current_user_cannot_execute_is_not_runnable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let program = sibling_hook_program(temp.path());
        assert!(hook_program_is_runnable(&program));
        // Only the "other" class may execute: the owner cannot, although the
        // mode has an execute bit.
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o001)).unwrap();
        assert!(!hook_program_is_runnable(&program));
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!hook_program_is_runnable(&program));
        assert!(
            !hook_program_is_runnable(temp.path()),
            "a directory is not a program"
        );
    }

    #[test]
    fn a_stale_paneflow_owned_grok_file_is_repaired_and_a_user_file_is_not() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("hooks");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("paneflow.json");
        let source = grok_source().unwrap();
        // A crashed session's rendering names a hook binary that no longer
        // exists, and left the durable ownership marker behind.
        let stale = render_as_sibling_instance(&source, Path::new("/gone/paneflow-ai-hook"));
        std::fs::write(&path, &stale).unwrap();
        let mut crashed = HookLease::acquire(&path).unwrap();
        crashed.mark_created().unwrap();
        drop(crashed);

        let guard = GrokHookFileGuard::install_at(&directory)
            .expect("a stale PaneFlow-owned file must be repaired");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        drop(guard);
        assert!(
            !path.exists(),
            "the repaired file is owned and removed by the last session"
        );

        // Without the marker the same stale bytes are a user's file: refused
        // and untouched.
        std::fs::write(&path, &stale).unwrap();
        let error = GrokHookFileGuard::install_at(&directory)
            .err()
            .expect("an unowned stale file must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), stale);

        // A PaneFlow-owned file the user edited is never repaired either.
        let edited = stale.replacen("paneflow-ai-hook Stop", "my-hook Stop", 1);
        std::fs::write(&path, &edited).unwrap();
        let mut crashed = HookLease::acquire(&path).unwrap();
        crashed.mark_created().unwrap();
        drop(crashed);
        let error = GrokHookFileGuard::install_at(&directory)
            .err()
            .expect("an edited file must be refused even when PaneFlow created it");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn repair_stale_owned_file_keeps_created_marker_when_replacement_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("hooks");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("paneflow.json");
        let source = grok_source().unwrap();
        let stale = render_as_sibling_instance(&source, Path::new("/gone/paneflow-ai-hook"));
        std::fs::write(&path, &stale).unwrap();
        let mut crashed = HookLease::acquire(&path).unwrap();
        crashed.mark_created().unwrap();
        drop(crashed);

        // `TempDir` cannot remove a non-writable directory. Restore the mode
        // on every exit, including a failed assertion.
        struct RestoreWritable(PathBuf);
        impl Drop for RestoreWritable {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let _restore = RestoreWritable(directory.clone());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();

        let error = GrokHookFileGuard::install_at(&directory)
            .err()
            .expect("a failed replacement must surface the write error");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            HookLease::acquire(&path).unwrap().is_created(),
            "a failed repair must leave the created marker in place"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), stale);

        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        let guard = GrokHookFileGuard::install_at(&directory)
            .expect("a later repair must replace the stale file once the directory is writable");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        drop(guard);
    }

    #[test]
    fn a_stale_paneflow_owned_pi_extension_is_repaired() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join("extensions");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(PANEFLOW_TS_BASENAME);
        // Older extension: TypeScript, not the current source, and not JSON,
        // so the rendering-shape proof cannot be what lets repair through.
        let stale = "export default function (pi) { /* old protocol, id: 1 */ }\n";
        assert_ne!(stale, PI_EXTENSION_SOURCE);
        assert!(serde_json::from_str::<serde_json::Value>(stale).is_err());

        std::fs::write(&path, stale).unwrap();
        let mut crashed = HookLease::acquire(&path).unwrap();
        crashed.mark_created().unwrap();
        drop(crashed);

        let guard = PiExtensionGuard::install_at(&directory)
            .expect("a stale PaneFlow-owned Pi extension must be repaired");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PI_EXTENSION_SOURCE);
        drop(guard);
        assert!(
            !path.exists(),
            "the repaired file is owned and removed by the last session"
        );

        // Without the marker the same stale bytes are a user's file.
        std::fs::write(&path, stale).unwrap();
        let error = PiExtensionGuard::install_at(&directory)
            .err()
            .expect("an unowned stale file must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(
            error.to_string().contains("contains user changes"),
            "refusing a user file must say so: {error}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), stale);

        // A live sibling holds the lease, so repair must not replace the file.
        let mut crashed = HookLease::acquire(&path).unwrap();
        crashed.mark_created().unwrap();
        drop(crashed);
        let sibling = HookLease::acquire(&path).unwrap();
        let error = PiExtensionGuard::install_at(&directory)
            .err()
            .expect("a live sibling must block repair");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), stale);
        drop(sibling);
        assert!(
            HookLease::acquire(&path).unwrap().is_created(),
            "a blocked repair must leave the created marker in place"
        );

        // The orphan sweep uses that marker, not byte equality with the
        // current source.
        sweep_created_owned_file(&path);
        assert!(
            !path.exists(),
            "the orphan sweep must remove a stale created Pi extension"
        );
        std::fs::write(&path, stale).unwrap();
        sweep_created_owned_file(&path);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            stale,
            "a file PaneFlow did not create must survive the orphan sweep"
        );
    }

    #[test]
    fn sibling_rendering_differs_only_in_the_hook_program() {
        let temp = tempfile::TempDir::new().unwrap();
        let program = sibling_hook_program(&temp.path().join("elsewhere"));
        let program_text = program.to_string_lossy().into_owned();
        let source = grok_source().unwrap();
        let sibling = render_as_sibling_instance(&source, &program);
        assert_ne!(sibling, source, "the fixture must change the bytes");
        assert!(is_sibling_instance_rendering(&sibling, &source));
        assert!(is_own_or_sibling_rendering(&source, &source));

        // A quoted program path (spaces) is still the same hook.
        let spaced = sibling_hook_program(&temp.path().join("my dir"));
        let quoted = sibling.replace(&program_text, &format!("'{}'", spaced.to_string_lossy()));
        assert_ne!(quoted, sibling);
        assert!(is_sibling_instance_rendering(&quoted, &source));

        // A sibling whose hook binary was pruned is not accepted: this
        // session would inherit hooks that invoke a missing executable.
        let pruned = sibling.replace(&program_text, "/elsewhere/gone/paneflow-ai-hook");
        assert!(!is_sibling_instance_rendering(&pruned, &source));
        std::fs::remove_file(&program).unwrap();
        assert!(!is_sibling_instance_rendering(&sibling, &source));
        let _ = sibling_hook_program(&temp.path().join("elsewhere"));

        // A command that merely starts and ends like a hook is not one.
        let augmented = sibling.replacen(
            &format!("{program_text} Stop"),
            &format!("{program_text} Stop; touch /tmp/pwn Stop"),
            1,
        );
        assert_ne!(augmented, sibling);
        assert!(!is_sibling_instance_rendering(&augmented, &source));

        // Same program, different event: not the same hook.
        let swapped = sibling.replacen("paneflow-ai-hook Stop", "paneflow-ai-hook Notification", 1);
        assert!(!is_sibling_instance_rendering(&swapped, &source));
        // A user command in place of a PaneFlow hook.
        let user = sibling.replacen(&format!("{program_text} Stop"), "my-hook Stop", 1);
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
        let program = sibling_hook_program(&temp.path().join("elsewhere"));
        let sibling = render_as_sibling_instance(&grok_source().unwrap(), &program);
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
        let program = sibling_hook_program(&temp.path().join("elsewhere"));
        let user = render_as_sibling_instance(&grok_source().unwrap(), &program).replacen(
            &format!("{} Stop", program.to_string_lossy()),
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
