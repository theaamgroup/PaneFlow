mod top_level_keys;

use super::owned_files::report_cleanup_failure;
use super::{
    home_unavailable, is_paneflow_hook_command, paneflow_ipc_reachable, refuse_symlink,
    resolve_plain_hook_command, with_last_lease, with_orphan_lease, HookInstall, HookInstallResult,
    HookInstallSkip, HookLease,
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
    let end_relative = content[begin..].find(HERMES_BLOCK_END)?;
    let mut end = begin + end_relative + HERMES_BLOCK_END.len();
    if content[end..].starts_with('\n') {
        end += 1;
    }
    Some(format!("{}{}", &content[..begin], &content[end..]))
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
    list.retain(|item| !is_managed_approval(item));
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
    } else {
        write_json_atomic(path, &root)
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
        let path = directory.join("config.yaml");
        if !paneflow_ipc_reachable() {
            Self::sweep_orphan(&path);
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
                TopLevelHooks::Absent => {}
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
            if !base.is_empty() && !base.ends_with('\n') {
                base.push('\n');
            }
            base.push_str(&hermes_managed_block());
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

    fn sweep_orphan(path: &Path) {
        let _ = with_orphan_lease(path, path, |created_file| {
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
