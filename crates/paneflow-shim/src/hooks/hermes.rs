mod top_level_keys;

use super::owned_files::report_cleanup_failure;
use super::{
    config_dir_is_symlink, home_unavailable, is_paneflow_hook_command, paneflow_ipc_reachable,
    refuse_symlink, resolve_plain_hook_command, with_last_lease, with_orphan_lease, HookInstall,
    HookInstallResult, HookInstallSkip, HookLease,
};
use paneflow_agent_config::{
    home_dir, read_optional_text, with_config_lock, write_json_atomic, write_text_atomic,
};
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use top_level_keys::{top_level_hooks, TopLevelHooks};

pub(crate) const HERMES_BLOCK_BEGIN: &str =
    "# >>> paneflow managed hooks (auto-installed; removed on session end) >>>";
const HERMES_ORIGINAL_HOOKS: &str = "# paneflow original hooks: ";
const HERMES_BLOCK_END: &str = "# <<< paneflow managed hooks <<<";

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Hermes event name, then the PaneFlow event whose command is installed.
const HERMES_MANAGED_HOOKS: &[(&str, &str)] = &[
    ("pre_llm_call", "UserPromptSubmit"),
    ("post_llm_call", "Stop"),
    ("pre_tool_call", "PreToolUse"),
    ("post_tool_call", "PostToolUse"),
    ("pre_approval_request", "PermissionRequest"),
];

/// `(hermes event, exact command)` pairs written into config and into
/// `shell-hooks-allowlist.json`. The command must match the config byte
/// for byte or Hermes will not treat it as approved.
fn hermes_managed_approvals() -> Vec<(String, String)> {
    HERMES_MANAGED_HOOKS
        .iter()
        .map(|(event, paneflow_event)| {
            (
                (*event).to_string(),
                resolve_plain_hook_command(paneflow_event),
            )
        })
        .collect()
}

pub(crate) fn hermes_managed_block() -> String {
    let mut block = format!("{HERMES_BLOCK_BEGIN}\nhooks:\n");
    for (event, command) in hermes_managed_approvals() {
        block.push_str(&format!(
            "  {event}:\n    - command: {}\n      timeout: 5\n",
            yaml_quote(&command)
        ));
    }
    block.push_str(HERMES_BLOCK_END);
    block.push('\n');
    block
}

pub(crate) fn strip_hermes_managed_block(content: &str) -> Option<String> {
    let begin = content.find(HERMES_BLOCK_BEGIN)?;
    let block = &content[begin..];
    let mut end_relative = 0;
    for line in block.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == HERMES_BLOCK_END {
            break;
        }
        end_relative += line.len();
    }
    if end_relative == block.len() {
        return None;
    }
    let mut original = String::new();
    for line in block[..end_relative].lines() {
        if let Some(encoded) = line.strip_prefix(HERMES_ORIGINAL_HOOKS) {
            // A damaged backup must never turn into a destructive cleanup.
            if !original.is_empty() {
                return None;
            }
            original = serde_json::from_str(encoded).ok()?;
            if !matches!(top_level_hooks(&original), TopLevelHooks::Empty(span)
                if span == (0..original.len()))
            {
                return None;
            }
        }
    }
    let mut end = begin + end_relative + HERMES_BLOCK_END.len();
    if content[end..].starts_with('\n') {
        end += 1;
    }
    Some(format!(
        "{}{}{}",
        &content[..begin],
        original,
        &content[end..]
    ))
}

/// Profile directory Hermes reads: `HERMES_HOME` when set and non-empty,
/// otherwise `~/.hermes`. `config.yaml` lives directly in that directory.
fn hermes_config_dir(hermes_home: Option<&OsStr>) -> std::io::Result<PathBuf> {
    if let Some(configured) = hermes_home.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(configured));
    }
    home_dir()
        .map(|home| home.join(".hermes"))
        .ok_or_else(home_unavailable)
}

fn allowlist_path(directory: &Path) -> PathBuf {
    directory.join("shell-hooks-allowlist.json")
}

/// Any PaneFlow approval for a managed Hermes event, whichever build or
/// hook-binary path granted it. Sessions of different builds share the
/// lease, so the last holder cannot rely on its own granted set alone.
fn is_managed_approval(item: &serde_json::Value) -> bool {
    let text = |key: &str| item.get(key).and_then(|value| value.as_str());
    text("event").is_some_and(|event| {
        HERMES_MANAGED_HOOKS
            .iter()
            .any(|(managed, _)| *managed == event)
    }) && text("command").is_some_and(is_paneflow_hook_command)
}

fn approval_matches(item: &serde_json::Value, event: &str, command: &str) -> bool {
    item.get("event").and_then(|value| value.as_str()) == Some(event)
        && item.get("command").and_then(|value| value.as_str()) == Some(command)
}

/// Record consent for the managed commands only. A user's other approvals
/// stay. Hermes matches `event` + `command` exactly (`shell-hooks-allowlist.json`).
/// Idempotent, so every concurrent session can grant; `lease` records a
/// file this call created for whichever session exits last.
fn grant_managed_approvals(
    path: &Path,
    lease: &mut HookLease,
    granted: &[(String, String)],
) -> std::io::Result<()> {
    with_config_lock(path, || {
        let existing = read_optional_text(path)?;
        let created = existing.is_none();
        let mut root: serde_json::Value = match existing {
            None => serde_json::json!({"approvals": []}),
            Some(text) if text.trim().is_empty() => serde_json::json!({"approvals": []}),
            Some(text) => serde_json::from_str(&text).map_err(|err| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Hermes allowlist is not JSON: {err}"),
                )
            })?,
        };
        let approvals = root
            .as_object_mut()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Hermes allowlist is not an object",
                )
            })?
            .entry("approvals")
            .or_insert_with(|| serde_json::json!([]));
        let list = approvals.as_array_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Hermes allowlist approvals is not a list",
            )
        })?;
        for (event, command) in granted {
            if !list
                .iter()
                .any(|item| approval_matches(item, event, command))
            {
                list.push(serde_json::json!({"event": event, "command": command}));
            }
        }
        write_json_atomic(path, &root)?;
        if created {
            lease.mark_created()?;
        }
        Ok(())
    })
}

/// Remove every PaneFlow approval for a managed event (see
/// [`is_managed_approval`]); the user's own entries stay. The caller holds
/// the config lock: this runs inside the allowlist's last-lease cleanup,
/// which already took it.
fn revoke_managed_approvals(path: &Path, created: bool) -> std::io::Result<()> {
    let Some(text) = read_optional_text(path)? else {
        return Ok(());
    };
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut root: serde_json::Value = serde_json::from_str(&text).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Hermes allowlist is not JSON: {err}"),
        )
    })?;
    let Some(list) = root
        .get_mut("approvals")
        .and_then(|value| value.as_array_mut())
    else {
        return Ok(());
    };
    let before = list.len();
    list.retain(|item| !is_managed_approval(item));
    let removed = list.len() != before;
    let only_empty_approvals = list.is_empty()
        && root
            .as_object()
            .is_some_and(|object| object.len() == 1 && object.contains_key("approvals"));
    if created && only_empty_approvals {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    } else if removed {
        write_json_atomic(path, &root)
    } else {
        // Nothing to revoke: a rewrite would only reformat the user's file
        // and could drop a consent Hermes is recording concurrently.
        Ok(())
    }
}

pub(crate) struct HermesHookConfigGuard {
    path: PathBuf,
    allowlist_path: PathBuf,
    created_file: bool,
    granted: Vec<(String, String)>,
    lease: HookLease,
    /// #1057: the approvals are shared by every session on this profile, so
    /// they get their own lease and only the last holder revokes them.
    allowlist_lease: HookLease,
}

impl HermesHookConfigGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let directory = hermes_config_dir(env::var_os("HERMES_HOME").as_deref())?;
        if !paneflow_ipc_reachable() {
            Self::sweep_orphan(&directory);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        let guard = Self::install_at(&directory)?;
        Ok(HookInstall::Installed(guard))
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        refuse_symlink(directory, "Hermes")?;
        std::fs::create_dir_all(directory)?;
        let path = directory.join("config.yaml");
        let allowlist = allowlist_path(directory);
        let mut lease = HookLease::acquire(&path)?;
        // Held before any grant, so a session exiting meanwhile sees this
        // one as a live holder and leaves the approvals in place.
        let allowlist_lease = HookLease::acquire(&allowlist)?;
        let created_file = with_config_lock(&path, || {
            let existing = read_optional_text(&path)?;
            let created = existing.is_none();
            let content = existing.unwrap_or_default();
            let mut base = strip_hermes_managed_block(&content).unwrap_or(content);
            match top_level_hooks(&base) {
                TopLevelHooks::Absent => {
                    if !base.is_empty() && !base.ends_with('\n') {
                        base.push('\n');
                    }
                    base.push_str(&hermes_managed_block());
                }
                TopLevelHooks::Empty(span) => {
                    let encoded = serde_json::to_string(&base[span.clone()])?;
                    let mut block = hermes_managed_block();
                    block.insert_str(
                        HERMES_BLOCK_BEGIN.len() + 1,
                        &format!("{HERMES_ORIGINAL_HOOKS}{encoded}\n"),
                    );
                    base.replace_range(span, &block);
                }
                TopLevelHooks::Present => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "user Hermes config already has hooks",
                    ));
                }
                TopLevelHooks::Unsure => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "user Hermes config is not a single top-level block mapping \
                         PaneFlow can safely add hooks to",
                    ));
                }
            }
            write_text_atomic(&path, &base)?;
            if created {
                lease.mark_created()?;
            }
            Ok(created)
        })?;
        let mut guard = Self {
            path,
            allowlist_path: allowlist,
            created_file,
            granted: hermes_managed_approvals(),
            lease,
            allowlist_lease,
        };
        match grant_managed_approvals(
            &guard.allowlist_path,
            &mut guard.allowlist_lease,
            &guard.granted,
        ) {
            Ok(()) => Ok(guard),
            Err(err) => {
                // Drop strips the managed block, and only the last holder
                // revokes approvals: a no-op when the file is absent, while a
                // file written but not yet marked created keeps only the
                // user's entries (`{"approvals":[]}` if none).
                drop(guard);
                Err(err)
            }
        }
    }

    /// Both halves of what a crashed session left, each under its own lease
    /// and in `Drop`'s order: approvals are revoked only when no live
    /// session holds the allowlist lease (#1075).
    fn sweep_orphan(directory: &Path) {
        // `install_at` refuses a symlinked profile directory; so does this.
        if config_dir_is_symlink(directory) {
            return;
        }
        let allowlist = &allowlist_path(directory);
        let revoked = with_orphan_lease(allowlist, allowlist, |created_allowlist| {
            revoke_managed_approvals(allowlist, created_allowlist)
        });
        report_cleanup_failure(allowlist, revoked.as_ref().err());
        let path = &directory.join("config.yaml");
        let cleaned = with_orphan_lease(path, path, |created_file| {
            let Some(content) = read_optional_text(path)? else {
                return Ok(());
            };
            if let Some(cleaned) = strip_hermes_managed_block(&content) {
                if created_file && cleaned.trim().is_empty() {
                    std::fs::remove_file(path)
                } else {
                    write_text_atomic(path, &cleaned)
                }
            } else {
                Ok(())
            }
        });
        report_cleanup_failure(path, cleaned.as_ref().err());
    }
}

impl Drop for HermesHookConfigGuard {
    fn drop(&mut self) {
        let revoked = with_last_lease(
            &self.allowlist_path,
            &mut self.allowlist_lease,
            |created_allowlist| revoke_managed_approvals(&self.allowlist_path, created_allowlist),
        );
        report_cleanup_failure(&self.allowlist_path, revoked.as_ref().err());
        let cleaned = with_last_lease(&self.path, &mut self.lease, |lease_created_file| {
            let Some(content) = read_optional_text(&self.path)? else {
                return Ok(());
            };
            let Some(cleaned) = strip_hermes_managed_block(&content) else {
                return Ok(());
            };
            if (self.created_file || lease_created_file) && cleaned.trim().is_empty() {
                std::fs::remove_file(&self.path)
            } else {
                write_text_atomic(&self.path, &cleaned)
            }
        });
        report_cleanup_failure(&self.path, cleaned.as_ref().err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_config_survives_invalid_utf8() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config.yaml");
        std::fs::write(&config, [0xff]).unwrap();

        assert!(HermesHookConfigGuard::install_at(&directory).is_err());
        assert_eq!(std::fs::read(config).unwrap(), [0xff]);
    }

    #[test]
    fn preexisting_empty_config_survives_cleanup() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config.yaml");
        std::fs::write(&config, "").unwrap();

        let guard = HermesHookConfigGuard::install_at(&directory).unwrap();
        drop(guard);

        assert!(config.exists());
        assert_eq!(std::fs::read_to_string(config).unwrap(), "");
    }

    #[test]
    fn config_path_follows_hermes_home_when_set() {
        let profile = tempfile::TempDir::new().unwrap();
        let directory = super::hermes_config_dir(Some(profile.path().as_os_str())).unwrap();
        assert_eq!(directory, profile.path());

        let guard = HermesHookConfigGuard::install_at(&directory).unwrap();
        let config = directory.join("config.yaml");
        let content = std::fs::read_to_string(&config).unwrap();
        assert!(
            content.contains(HERMES_BLOCK_BEGIN),
            "managed block must be written to $HERMES_HOME/config.yaml"
        );
        assert!(
            !content.contains("HERMES_ACCEPT_HOOKS"),
            "config must not record the process-wide consent bypass"
        );
        drop(guard);
        assert!(
            !directory.join("shell-hooks-allowlist.json").exists(),
            "a allowlist this install created is removed with the managed block"
        );
    }

    #[test]
    fn allowlist_approves_managed_commands_and_keeps_a_user_approval() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let allowlist = directory.join("shell-hooks-allowlist.json");
        std::fs::write(
            &allowlist,
            r#"{"approvals":[{"event":"pre_tool_call","command":"/usr/bin/user-hook"}]}"#,
        )
        .unwrap();

        let guard = HermesHookConfigGuard::install_at(&directory).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&allowlist).unwrap()).unwrap();
        let approvals = value["approvals"].as_array().unwrap();
        assert!(
            approvals.iter().any(|item| {
                item["event"] == "pre_tool_call" && item["command"] == "/usr/bin/user-hook"
            }),
            "a user approval must survive, got {value}"
        );
        for (event, command) in super::hermes_managed_approvals() {
            assert!(
                approvals
                    .iter()
                    .any(|item| { item["event"] == event && item["command"] == command }),
                "managed {event} command must be approved, got {value}"
            );
        }
        drop(guard);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&allowlist).unwrap()).unwrap();
        let approvals = value["approvals"].as_array().unwrap();
        assert_eq!(
            approvals.len(),
            1,
            "only the user approval remains: {value}"
        );
        assert_eq!(approvals[0]["command"], "/usr/bin/user-hook");
    }

    #[test]
    fn next_last_holder_cleans_up_after_a_crashed_session() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let allowlist = allowlist_path(&directory);

        // A session that died after granting: the kernel dropped its lease
        // lock with the process, but its approvals and the durable
        // created-file marker remain.
        let mut crashed = HookLease::acquire(&allowlist).unwrap();
        grant_managed_approvals(&allowlist, &mut crashed, &hermes_managed_approvals()).unwrap();
        drop(crashed);

        let guard = HermesHookConfigGuard::install_at(&directory).unwrap();
        drop(guard);
        assert!(
            !allowlist.exists(),
            "the next last holder removes the allowlist the crashed session created"
        );
        assert!(
            !HookLease::acquire(&allowlist).unwrap().is_created(),
            "the last holder consumes the created-file marker"
        );
    }

    #[test]
    fn last_holder_revokes_approvals_another_build_granted() {
        // Debug and release builds (or a versioned-cache fallback) grant
        // different command strings for the same events, and share the
        // lease. The last holder must revoke every PaneFlow approval, not
        // only the set its own session granted.
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let allowlist = allowlist_path(&directory);
        let other_build = [(
            "pre_tool_call".to_string(),
            "'/Users/me/Library/Application Support/paneflow-dev/bin/paneflow-ai-hook' PreToolUse"
                .to_string(),
        )];
        assert!(is_paneflow_hook_command(&other_build[0].1));
        for (_, command) in hermes_managed_approvals() {
            assert!(is_paneflow_hook_command(&command), "{command}");
        }

        let mut other = HookLease::acquire(&allowlist).unwrap();
        grant_managed_approvals(&allowlist, &mut other, &other_build).unwrap();
        let guard = HermesHookConfigGuard::install_at(&directory).unwrap();
        drop(other);
        drop(guard);
        assert!(
            !allowlist.exists(),
            "no PaneFlow approval may outlive the last session, got {:?}",
            std::fs::read_to_string(&allowlist).ok()
        );
    }

    #[test]
    fn concurrent_guards_keep_a_user_allowlist_and_its_entries() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let allowlist = allowlist_path(&directory);
        let user = r#"{"approvals":[{"event":"pre_tool_call","command":"/usr/bin/user-hook"}]}"#;
        std::fs::write(&allowlist, user).unwrap();

        let first = HermesHookConfigGuard::install_at(&directory).unwrap();
        let second = HermesHookConfigGuard::install_at(&directory).unwrap();
        assert!(
            !HookLease::acquire(&allowlist).unwrap().is_created(),
            "a user-created allowlist never gets a PaneFlow created marker"
        );
        drop(first);
        drop(second);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&allowlist).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"approvals": [
                {"event": "pre_tool_call", "command": "/usr/bin/user-hook"}
            ]}),
            "the user's file and entry survive both sessions"
        );
        assert!(!HookLease::acquire(&allowlist).unwrap().is_created());
    }

    /// SIGKILL before `Drop`: the kernel releases both lease locks, but the
    /// managed block, the approvals, and the durable created-file markers
    /// stay. The guard's leases are swapped for leases on an unrelated
    /// resource so the real ones drop without the guard's cleanup running.
    fn crash(mut guard: HermesHookConfigGuard, unrelated: &Path) {
        drop(std::mem::replace(
            &mut guard.lease,
            HookLease::acquire(unrelated).unwrap(),
        ));
        drop(std::mem::replace(
            &mut guard.allowlist_lease,
            HookLease::acquire(unrelated).unwrap(),
        ));
        std::mem::forget(guard);
    }

    fn approvals(allowlist: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(allowlist).unwrap()).unwrap()
    }

    #[test]
    fn empty_hooks_survive_crash_sweep_and_reinstall() {
        for reinstall in [false, true] {
            let temp = tempfile::TempDir::new().unwrap();
            let directory = temp.path().join("profile");
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("config.yaml");
            let original = "model: x\r\nhooks: null # original\r\nverbose: false\r\n";
            std::fs::write(&path, original).unwrap();
            let guard = HermesHookConfigGuard::install_at(&directory).unwrap();
            crash(guard, &temp.path().join("unrelated"));
            if reinstall {
                let next = HermesHookConfigGuard::install_at(&directory).unwrap();
                assert_eq!(
                    std::fs::read_to_string(&path)
                        .unwrap()
                        .matches(HERMES_BLOCK_BEGIN)
                        .count(),
                    1,
                );
                drop(next);
            } else {
                HermesHookConfigGuard::sweep_orphan(&directory);
            }
            assert_eq!(std::fs::read_to_string(path).unwrap(), original);
            assert!(!allowlist_path(&directory).exists());
        }
    }

    #[test]
    fn orphan_sweep_revokes_a_crashed_sessions_approvals_and_keeps_the_users() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config.yaml");
        let allowlist = allowlist_path(&directory);
        let user = serde_json::json!({"approvals": [
            {"event": "pre_tool_call", "command": "/usr/bin/user-hook"}
        ]});
        std::fs::write(&allowlist, user.to_string()).unwrap();

        crash(
            HermesHookConfigGuard::install_at(&directory).unwrap(),
            &temp.path().join("unrelated"),
        );
        assert!(approvals(&allowlist)["approvals"].as_array().unwrap().len() > 1);

        HermesHookConfigGuard::sweep_orphan(&directory);
        assert!(
            !config.exists(),
            "the sweep removes the config.yaml the crashed session created"
        );
        assert_eq!(
            approvals(&allowlist),
            user,
            "the sweep revokes the managed approvals and keeps the user's"
        );
    }

    #[test]
    fn orphan_sweep_removes_an_allowlist_a_crashed_session_created() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let allowlist = allowlist_path(&directory);

        crash(
            HermesHookConfigGuard::install_at(&directory).unwrap(),
            &temp.path().join("unrelated"),
        );
        assert!(allowlist.exists());

        HermesHookConfigGuard::sweep_orphan(&directory);
        assert!(
            !allowlist.exists(),
            "the sweep removes the allowlist the crashed session created, got {:?}",
            std::fs::read_to_string(&allowlist).ok()
        );
        assert!(
            !HookLease::acquire(&allowlist).unwrap().is_created(),
            "the sweep consumes the created-file marker"
        );
    }

    #[test]
    fn orphan_sweep_leaves_a_live_sessions_approvals() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config.yaml");
        let allowlist = allowlist_path(&directory);

        let live = HermesHookConfigGuard::install_at(&directory).unwrap();
        HermesHookConfigGuard::sweep_orphan(&directory);
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains(HERMES_BLOCK_BEGIN));
        let value = approvals(&allowlist);
        for (event, command) in hermes_managed_approvals() {
            assert!(
                value["approvals"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| approval_matches(item, &event, &command)),
                "a live session keeps its {event} approval, got {value}"
            );
        }

        drop(live);
        assert!(
            !allowlist.exists(),
            "the live session, still the last holder, cleans up on exit"
        );
    }

    #[test]
    fn orphan_sweep_leaves_an_allowlist_without_managed_approvals_untouched() {
        use std::os::unix::fs::MetadataExt;

        // Every IPC-down launch sweeps. A file with nothing to revoke must
        // not be rewritten: that reformats it, and races a consent Hermes
        // is writing at the same moment.
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let allowlist = allowlist_path(&directory);
        let user = br#"{"approvals":[{"event":"pre_tool_call","command":"/usr/bin/user-hook"}]}"#;
        std::fs::write(&allowlist, user).unwrap();
        let inode = std::fs::metadata(&allowlist).unwrap().ino();

        HermesHookConfigGuard::sweep_orphan(&directory);
        assert_eq!(std::fs::read(&allowlist).unwrap(), user);
        assert_eq!(
            std::fs::metadata(&allowlist).unwrap().ino(),
            inode,
            "a sweep with nothing to revoke must not replace the file"
        );
    }

    #[test]
    fn orphan_sweep_reports_an_allowlist_it_cannot_parse() {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = temp.path().join(".hermes");
        std::fs::create_dir_all(&directory).unwrap();
        let allowlist = allowlist_path(&directory);
        std::fs::write(&allowlist, "not json").unwrap();
        super::super::owned_files::take_recorded_cleanup_failures();

        HermesHookConfigGuard::sweep_orphan(&directory);
        let failures = super::super::owned_files::take_recorded_cleanup_failures();
        assert!(
            failures
                .iter()
                .any(|message| message.contains("Hermes allowlist is not JSON")),
            "a failed sweep is logged like a failed Drop, got {failures:?}"
        );
        assert_eq!(std::fs::read(&allowlist).unwrap(), b"not json");
    }

    #[test]
    fn orphan_sweep_does_not_follow_a_symlinked_profile_dir() {
        use std::os::unix::fs::symlink;

        // `install_at` refuses a symlinked profile directory; the sweep
        // must not write through one either.
        let temp = tempfile::TempDir::new().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let config = outside.join("config.yaml");
        std::fs::write(&config, hermes_managed_block()).unwrap();
        let approvals: Vec<_> = hermes_managed_approvals()
            .into_iter()
            .map(|(event, command)| serde_json::json!({"event": event, "command": command}))
            .collect();
        let allowlist = allowlist_path(&outside);
        std::fs::write(
            &allowlist,
            serde_json::json!({ "approvals": approvals }).to_string(),
        )
        .unwrap();
        let config_before = std::fs::read(&config).unwrap();
        let allowlist_before = std::fs::read(&allowlist).unwrap();

        let link = temp.path().join(".hermes");
        symlink(&outside, &link).unwrap();
        HermesHookConfigGuard::sweep_orphan(&link);

        assert_eq!(
            std::fs::read(&config).unwrap(),
            config_before,
            "the config must not be rewritten through the link"
        );
        assert_eq!(
            std::fs::read(&allowlist).unwrap(),
            allowlist_before,
            "the allowlist must not be rewritten through the link"
        );
    }

    #[test]
    fn empty_hermes_home_uses_the_default_config_dir() {
        let expected = super::home_dir().unwrap().join(".hermes");
        assert_eq!(
            super::hermes_config_dir(Some(std::ffi::OsStr::new(""))).unwrap(),
            expected
        );
        assert_eq!(super::hermes_config_dir(None).unwrap(), expected);
    }
}
