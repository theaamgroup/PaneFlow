//! Session-scoped agent hook installation.

mod agents;
mod claude;
mod codex;
pub(crate) mod dsh;
mod hermes;
pub(crate) mod muse;
mod opencode;
mod owned_files;

pub(crate) use agents::{
    merge_cursor_hooks, merge_gemini_hooks, merge_qoder_hooks, remove_cursor_hooks,
    remove_gemini_hooks, remove_qoder_hooks, ManagedHookConfigGuard, ManagedHookSpec,
};
pub(crate) use claude::{prune_stale_project_hooks, HookConfigGuard};
pub(crate) use codex::CodexHookConfigGuard;
#[cfg(test)]
pub(crate) use codex::{enable_codex_feature_flag, CODEX_HOOK_EVENTS, CODEX_TOML_MARKER};
pub(crate) use dsh::DshOverlayGuard;
pub(crate) use hermes::HermesHookConfigGuard;
#[cfg(test)]
pub(crate) use hermes::{hermes_managed_block, strip_hermes_managed_block, HERMES_BLOCK_BEGIN};
pub(crate) use muse::MuseHookConfigGuard;
pub(crate) use opencode::OpenCodePluginGuard;
#[cfg(test)]
pub(crate) use owned_files::{
    render_as_sibling_instance, sibling_hook_program, PANEFLOW_TS_BASENAME,
};
pub(crate) use owned_files::{GrokHookFileGuard, PiExtensionGuard};

use crate::locate_sibling_hook_binary;
#[cfg(test)]
use paneflow_agent_config::claude_hooks::command_program_token;
#[cfg(test)]
use paneflow_agent_config::claude_hooks::{display_hook_program, shell_program_path};
pub(crate) use paneflow_agent_config::claude_hooks::{
    is_managed_group as is_paneflow_matcher_group, is_paneflow_hook_command, CLAUDE_HOOK_EVENTS,
};
use paneflow_agent_config::claude_hooks::{
    paneflow_hook_program_token, reconcile_matcher_hooks_replacing_invalid_container,
    remove_matcher_hooks_lenient, render_bare_hook_command, render_hook_command, HookConfigError,
};
use paneflow_agent_config::{read_optional_text, with_config_lock, write_json_atomic, ConfigLease};
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InvalidJsonPolicy {
    Replace,
    Refuse,
}

pub(crate) type HookLease = ConfigLease;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookInstallSkip {
    IpcUnavailable,
    BridgeUnavailable,
    PersistentClaudeHook,
    UnsupportedTool,
}

pub(crate) enum HookInstall<T> {
    Installed(T),
    Skipped(HookInstallSkip),
}

impl<T> HookInstall<T> {
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> HookInstall<U> {
        match self {
            Self::Installed(value) => HookInstall::Installed(map(value)),
            Self::Skipped(reason) => HookInstall::Skipped(reason),
        }
    }
}

pub(crate) type HookInstallResult<T> = std::io::Result<HookInstall<T>>;

pub(crate) fn safe_path_display(path: &Path) -> String {
    safe_log_text(&path.display().to_string())
}

fn safe_log_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if (' '..='~').contains(&character) {
                character
            } else {
                '?'
            }
        })
        .collect()
}

pub(crate) fn paneflow_ipc_reachable() -> bool {
    reachable_from_socket_env(env::var_os("PANEFLOW_SOCKET_PATH").as_deref())
}

fn reachable_from_socket_env(raw: Option<&OsStr>) -> bool {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return false;
    };
    Path::new(raw).exists()
}

pub(crate) fn config_dir_is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn home_unavailable() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "home directory is unavailable",
    )
}

fn refuse_symlink(path: &Path, label: &str) -> std::io::Result<()> {
    if config_dir_is_symlink(path) {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to write through a symlinked {label} directory"),
        ))
    } else {
        Ok(())
    }
}

/// Project-local hook files live in the checkout's own `.claude/` (or
/// `.codex/`, `.codebuddy/`, ...) directory, so a FILE symlink there is under
/// the repository's control, not the user's. `write_json_atomic` deliberately
/// follows a symlinked HOME config (stow, chezmoi, yadm); followed from a
/// cloned repo it would rewrite whatever user-owned file the link points at.
/// Refuse it the way `config_dir_is_symlink` refuses a symlinked directory
/// (#234). Home-scope callers do not go through this check.
pub(crate) fn refuse_symlinked_project_hook_file(path: &Path, label: &str) -> std::io::Result<()> {
    if config_dir_is_symlink(path) {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to write {label} hooks through a symlinked project-local file"),
        ))
    } else {
        Ok(())
    }
}

pub(super) fn with_last_lease<T>(
    lock_path: &Path,
    lease: &mut HookLease,
    cleanup: impl FnOnce(bool) -> std::io::Result<T>,
) -> std::io::Result<Option<T>> {
    with_config_lock(lock_path, || {
        let Some(mut last) = lease.try_take_last()? else {
            return Ok(None);
        };
        let created = last.take_created()?;
        cleanup(created).map(Some)
    })
}

pub(super) fn with_orphan_lease<T>(
    resource_path: &Path,
    lock_path: &Path,
    cleanup: impl FnOnce(bool) -> std::io::Result<T>,
) -> std::io::Result<Option<T>> {
    let mut lease = HookLease::acquire(resource_path)?;
    with_last_lease(lock_path, &mut lease, cleanup)
}

pub(crate) fn sweep_orphan_hook_config(path: &Path, remove: fn(&mut serde_json::Value)) {
    if path.parent().is_some_and(config_dir_is_symlink) {
        return;
    }
    // #234: a FILE symlink is under the checkout's control, not the user's.
    // Cleanup-only launches (`install()` when IPC is down, persistent-hook
    // Alive) call this before `install_at`'s refuse, so a parent-dir check
    // alone would still follow the link and rewrite a user-owned target.
    if config_dir_is_symlink(path) {
        return;
    }
    let _ = with_orphan_lease(path, path, |created_file| {
        let Some(content) = read_optional_text(path)? else {
            return Ok(());
        };
        let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Ok(());
        };
        let before = root.clone();
        remove(&mut root);
        if root == before {
            return Ok(());
        }
        if created_file && root.as_object().is_some_and(serde_json::Map::is_empty) {
            std::fs::remove_file(path)
        } else {
            write_json_atomic(path, &root)
        }
    });
}

pub(crate) struct InstalledHookConfig {
    pub(crate) path: PathBuf,
    pub(crate) created_file: bool,
    pub(crate) created_directory: bool,
    pub(crate) lease: HookLease,
}

pub(crate) fn install_hook_config_file(
    directory: &Path,
    filename: &str,
    label: &str,
    merge: impl FnOnce(&mut serde_json::Value) -> std::io::Result<()>,
    invalid_json: InvalidJsonPolicy,
) -> std::io::Result<InstalledHookConfig> {
    if config_dir_is_symlink(directory) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to write {label} hooks through a symlink"),
        ));
    }
    let directory_existed = directory.is_dir();
    if directory.exists() && !directory_existed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} is not a directory", safe_path_display(directory)),
        ));
    }
    if !directory_existed {
        std::fs::create_dir_all(directory)?;
    }

    let path = directory.join(filename);
    let mut lease = match HookLease::acquire(&path) {
        Ok(lease) => lease,
        Err(error) => {
            if !directory_existed {
                let _ = std::fs::remove_dir(directory);
            }
            return Err(error);
        }
    };
    let result = with_config_lock(&path, || {
        let existing = read_optional_text(&path)?;
        let created_file = existing.is_none();
        let existing = existing.unwrap_or_default();
        let mut root = if existing.trim().is_empty() {
            serde_json::json!({})
        } else {
            match serde_json::from_str(&existing) {
                Ok(root) => root,
                Err(error) if invalid_json == InvalidJsonPolicy::Replace => {
                    eprintln!(
                        "paneflow-shim: {} contained invalid JSON ({error}); replacing it",
                        safe_path_display(&path)
                    );
                    serde_json::json!({})
                }
                Err(error) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
                }
            }
        };
        merge(&mut root)?;
        write_json_atomic(&path, &root)?;
        if created_file {
            lease.mark_created()?;
        }
        Ok(created_file)
    });
    let created_file = match result {
        Ok(created_file) => created_file,
        Err(error) => {
            if !directory_existed {
                let _ = std::fs::remove_dir(directory);
            }
            return Err(error);
        }
    };
    Ok(InstalledHookConfig {
        path,
        created_file,
        created_directory: !directory_existed,
        lease,
    })
}

pub(crate) fn cleanup_hook_config_file(
    path: &Path,
    directory: &Path,
    created_file: bool,
    created_directory: bool,
    remove: fn(&mut serde_json::Value),
    lease: &mut HookLease,
) {
    let remove_directory = with_last_lease(path, lease, |lease_created_file| {
        let Some(content) = read_optional_text(path)? else {
            return Ok(false);
        };
        let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Ok(false);
        };
        remove(&mut root);
        let empty = root.as_object().is_some_and(serde_json::Map::is_empty);
        let owned_file = created_file || lease_created_file;
        if empty && owned_file {
            std::fs::remove_file(path)?;
        } else {
            write_json_atomic(path, &root)?;
        }
        Ok(empty && owned_file && created_directory)
    })
    .unwrap_or_else(|error| {
        eprintln!(
            "paneflow-shim: could not clean up {}: {error}",
            safe_path_display(path)
        );
        None
    })
    .unwrap_or(false);
    if remove_directory {
        let _ = std::fs::remove_dir(directory);
    }
}

/// Env var carrying the stable, non-versioned `paneflow-ai-hook` path, set on
/// every pane by `pty_session::inject_ai_hook_env` (#542). Preferred over the
/// computed fallback because the app knows its own build namespace
/// (`paneflow` vs `paneflow-dev`), which a `release-min` shim cannot infer.
pub(crate) const AI_HOOK_PATH_ENV: &str = "PANEFLOW_AI_HOOK_PATH";

/// Absolute path to write into a managed hook command.
///
/// Two candidates, first runnable one wins:
///
/// 1. `PANEFLOW_AI_HOOK_PATH` from the pane env - the stable, non-versioned
///    copy, advertised only when the running app verified that the bytes there
///    are its own (`ai_hooks::extract::verified_ai_hook_path`).
/// 2. The version-pinned sibling next to this shim.
///
/// Issue #542: (1) is what survives an upgrade. A managed block that outlives
/// the process that wrote it keeps naming the version directory that wrote it,
/// and the next launch prunes that directory because its `.paneflow-live`
/// lease died with the old app - after which every agent tool call fails with
/// "No such file or directory".
///
/// The shim deliberately does **not** compute the stable path itself. Only the
/// app knows whether the file there is current: when launch extraction cannot
/// replace a previous release's binary, that stale copy is still present and
/// still runnable, and a locally computed candidate would select it over the
/// sibling and pin panes to the old hook behaviour and IPC protocol. Absence
/// of the env var is therefore meaningful: it means "no verified stable copy",
/// and the sibling, which always matches the shim being executed, is the
/// correct answer. A shim in a pane from an app too old to advertise the
/// variable never had a verified stable copy either, so it loses nothing.
fn locate_hook_binary() -> Option<PathBuf> {
    first_executable([advertised_hook_binary()]).or_else(locate_sibling_hook_binary)
}

fn advertised_hook_binary() -> Option<PathBuf> {
    env::var_os(AI_HOOK_PATH_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Pure core of the preference order, so the ordering is testable without
/// mutating process env. A candidate that is missing **or not executable** is
/// skipped, never written into a hook command: a stable copy stripped of `+x`
/// (a backup restore, a stray `chmod`) would otherwise shadow the runnable
/// sibling and turn every hook into `Permission denied`.
fn first_executable(candidates: impl IntoIterator<Item = Option<PathBuf>>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| candidate.is_file() && is_executable(candidate))
}

/// Whether **this process** may execute `path`.
///
/// `access(2)` with `X_OK`, not a `mode & 0o111` bitmask: Unix applies only
/// the first matching permission class, so a file owned by this user with mode
/// `0o001` has an execute bit yet cannot be executed by its owner. A bitmask
/// would accept it, let it shadow the runnable sibling, and turn every
/// generated hook into `Permission denied`. `access` also honours ACLs and a
/// `noexec` mount, which no bitmask can see.
///
/// The caller pairs this with an `is_file()` check: `X_OK` on a directory
/// tests traversability and would otherwise succeed.
pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the call,
    // and `access` only reads it.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

pub(crate) fn resolve_hook_command(event: &str) -> String {
    locate_hook_binary().map_or_else(
        || render_bare_hook_command(event),
        |path| render_hook_command(&path, event),
    )
}

fn resolve_plain_hook_command(event: &str) -> String {
    resolve_hook_command(event)
}

pub(crate) fn merge_paneflow_hooks(root: &mut serde_json::Value) -> std::io::Result<()> {
    paneflow_agent_config::claude_hooks::reconcile_hooks_replacing_invalid_container(
        root,
        resolve_hook_command,
    )
    .map(|_| ())
    .map_err(hook_config_error)
}

pub(crate) fn merge_codebuddy_hooks(root: &mut serde_json::Value) -> std::io::Result<()> {
    merge_strict_matcher_hooks_for_events(root, CLAUDE_HOOK_EVENTS)
}

pub(crate) fn remove_paneflow_hooks(root: &mut serde_json::Value) {
    paneflow_agent_config::claude_hooks::remove_hooks_lenient(root);
}

pub(super) fn plain_hook_handler(event: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        "command": resolve_plain_hook_command(event),
        "timeout": 5,
    })
}

fn merge_strict_matcher_hooks_for_events(
    root: &mut serde_json::Value,
    events: &[&str],
) -> std::io::Result<()> {
    reconcile_matcher_hooks_replacing_invalid_container(
        root,
        events,
        |event| serde_json::json!({ "hooks": [plain_hook_handler(event)] }),
    )
    .map(|_| ())
    .map_err(hook_config_error)
}

fn remove_matcher_hooks_for_events(root: &mut serde_json::Value, events: &[&str]) {
    remove_matcher_hooks_lenient(root, events);
}

fn hook_config_error(error: HookConfigError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #542: an advertised path that is gone must fall through to the
    /// sibling rather than be written into a hook command.
    #[test]
    fn a_missing_advertised_path_falls_through() {
        let temp = tempfile::TempDir::new().unwrap();
        let advertised = temp.path().join("env-paneflow-ai-hook");

        assert_eq!(first_executable([Some(advertised.clone())]), None);
        create_executable(&advertised);
        assert_eq!(
            first_executable([Some(advertised.clone())]),
            Some(advertised)
        );
        assert_eq!(first_executable([None]), None);
    }

    /// The shim must never compute the stable path itself. Only the app knows
    /// whether the file there is current; a locally computed candidate would
    /// select a previous release's binary when launch extraction could not
    /// replace it, in exactly the case where the app deliberately advertised
    /// nothing. Absence of the env var has to mean "use the sibling".
    #[test]
    fn the_shim_does_not_compute_a_stable_path_of_its_own() {
        // Split so the needle does not appear literally in this file, which
        // `include_str!` would otherwise match against the assertion itself.
        let needle = concat!("stable_ai_hook", "_binary_path");
        let source = include_str!("hooks.rs");
        assert!(
            !source.contains(needle),
            "the shim must not recompute the stable ai-hook path (#542)"
        );
    }

    /// A stable copy stripped of `+x` must fall through to the runnable
    /// sibling rather than pin every hook command to `Permission denied`.
    #[test]
    fn a_non_executable_candidate_is_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let stripped = temp.path().join("stripped-paneflow-ai-hook");
        create_executable(&stripped);
        std::fs::set_permissions(&stripped, std::fs::Permissions::from_mode(0o644)).unwrap();
        let usable = temp.path().join("usable-paneflow-ai-hook");
        create_executable(&usable);

        assert_eq!(
            first_executable([Some(stripped.clone()), Some(usable.clone())]),
            Some(usable)
        );
        assert_eq!(first_executable([Some(stripped)]), None);
    }

    /// `mode & 0o111` is not the same question as "can this process run it":
    /// Unix consults only the first matching permission class, so an
    /// owner-readable file with just the other-execute bit is unrunnable by
    /// its owner despite having an execute bit set.
    #[test]
    fn an_execute_bit_for_the_wrong_class_does_not_count() {
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: `geteuid` reads process state and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skip: root bypasses the X_OK permission check");
            return;
        }

        let temp = tempfile::TempDir::new().unwrap();
        let wrong_class = temp.path().join("wrong-class-paneflow-ai-hook");
        std::fs::File::create(&wrong_class).unwrap();
        std::fs::set_permissions(&wrong_class, std::fs::Permissions::from_mode(0o601)).unwrap();
        assert_ne!(
            std::fs::metadata(&wrong_class)
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0,
            "precondition: the file must carry an execute bit a bitmask would accept"
        );

        let usable = temp.path().join("usable-paneflow-ai-hook");
        create_executable(&usable);

        assert_eq!(
            first_executable([Some(wrong_class.clone()), Some(usable.clone())]),
            Some(usable)
        );
        assert_eq!(first_executable([Some(wrong_class)]), None);
    }

    fn create_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::File::create(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn socket_reachability_rejects_absent_values() {
        assert!(!reachable_from_socket_env(None));
        assert!(!reachable_from_socket_env(Some(OsStr::new(""))));
    }

    #[test]
    fn unix_socket_probe_is_passive() {
        let temp = tempfile::TempDir::new().unwrap();
        let socket = temp.path().join("paneflow.sock");
        assert!(!reachable_from_socket_env(Some(socket.as_os_str())));
        std::fs::File::create(&socket).unwrap();
        assert!(reachable_from_socket_env(Some(socket.as_os_str())));
    }

    #[test]
    fn orphan_sweep_preserves_file_when_ownership_is_unknown() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("hooks.json");
        let mut root = serde_json::json!({});
        merge_paneflow_hooks(&mut root).unwrap();
        write_json_atomic(&path, &root).unwrap();

        sweep_orphan_hook_config(&path, remove_paneflow_hooks);

        assert!(path.exists());
        let cleaned: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(cleaned, serde_json::json!({}));
    }

    #[test]
    fn sweep_orphan_does_not_follow_a_project_file_symlink() {
        use std::os::unix::fs::symlink;

        // #234: cleanup-only launches sweep before install_at's refuse. A
        // project-local FILE symlink to a user-owned JSON with PaneFlow
        // hooks must be left alone - following it would rewrite the target.
        let temp = tempfile::TempDir::new().unwrap();
        let outside = temp.path().join("outside.json");
        let mut root = serde_json::json!({});
        merge_paneflow_hooks(&mut root).unwrap();
        write_json_atomic(&outside, &root).unwrap();
        let original = std::fs::read(&outside).unwrap();

        let project_dir = temp.path().join(".claude");
        std::fs::create_dir_all(&project_dir).unwrap();
        let link = project_dir.join("settings.local.json");
        symlink(&outside, &link).unwrap();

        sweep_orphan_hook_config(&link, remove_paneflow_hooks);

        assert_eq!(
            std::fs::read(&outside).unwrap(),
            original,
            "the outside file must not be rewritten through the link"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself must be left in place"
        );
    }

    #[test]
    fn no_hook_installer_opts_into_replacing_malformed_json() {
        // Issue #202: `InvalidJsonPolicy::Replace` silently rewrote a
        // present-but-unparseable user config as `{}` plus PaneFlow hooks,
        // erasing unrelated permissions and hooks. Every installer call
        // site must refuse instead (the shim degrades gracefully: the
        // agent still launches, the failure lands in the diagnostic log).
        for (name, source) in [
            ("hooks/claude.rs", include_str!("hooks/claude.rs")),
            ("hooks/codex.rs", include_str!("hooks/codex.rs")),
            ("hooks/agents.rs", include_str!("hooks/agents.rs")),
        ] {
            assert!(
                !source.contains("InvalidJsonPolicy::Replace"),
                "{name} must not opt into InvalidJsonPolicy::Replace (issue #202)"
            );
        }
    }

    #[test]
    fn quoted_hook_commands_round_trip() {
        assert_eq!(
            command_program_token("'/tmp/Pane Flow/paneflow-ai-hook' Stop").as_deref(),
            Some("/tmp/Pane Flow/paneflow-ai-hook")
        );
    }

    #[test]
    fn unix_shell_path_quotes_spaces() {
        let path = Path::new("/tmp/Pane Flow/paneflow-ai-hook");
        let command = format!("{} Stop", shell_program_path(path));
        assert!(is_paneflow_hook_command(&command));
        assert_eq!(
            display_hook_program(path),
            "/tmp/Pane Flow/paneflow-ai-hook"
        );
    }
}
