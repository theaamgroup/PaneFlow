use super::{
    cleanup_hook_config_file, config_dir_is_symlink, install_hook_config_file,
    is_paneflow_hook_command, is_paneflow_matcher_group, merge_paneflow_hooks,
    paneflow_hook_program_token, paneflow_ipc_reachable, refuse_symlinked_project_hook_file,
    remove_paneflow_hooks, safe_log_text, safe_path_display, sweep_orphan_hook_config, HookInstall,
    HookInstallResult, HookInstallSkip, HookLease, InvalidJsonPolicy, CLAUDE_HOOK_EVENTS,
};
use paneflow_agent_config::{
    claude_settings_json, linked_worktree_main_checkout, read_optional_text, with_config_lock,
    write_json_atomic,
};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersistentHookState {
    Absent,
    Alive { command: String },
    Stale { command: Option<String> },
}

fn persistent_claude_hooks_state() -> std::io::Result<PersistentHookState> {
    let path = claude_settings_json().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "home directory is unavailable",
        )
    })?;
    persistent_claude_hooks_state_at(&path)
}

fn persistent_claude_hooks_state_at(path: &Path) -> std::io::Result<PersistentHookState> {
    let Some(content) = read_optional_text(path)? else {
        return Ok(PersistentHookState::Absent);
    };
    let root = serde_json::from_str(&content)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(settings_managed_hook_state(&root))
}

#[cfg(test)]
fn settings_has_managed_hook(root: &serde_json::Value) -> bool {
    settings_managed_hook_state(root) != PersistentHookState::Absent
}

fn settings_managed_hook_state(root: &serde_json::Value) -> PersistentHookState {
    let Some(hooks) = root.get("hooks").and_then(serde_json::Value::as_object) else {
        return PersistentHookState::Absent;
    };
    let mut stale = None;
    let mut managed_without_command = false;
    for event in CLAUDE_HOOK_EVENTS {
        let Some(groups) = hooks.get(*event).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for group in groups
            .iter()
            .filter(|group| is_paneflow_matcher_group(group))
        {
            let commands = paneflow_hook_commands_in_group(group);
            managed_without_command |= commands.is_empty();
            for command in commands {
                if paneflow_hook_command_program_exists(&command) {
                    return PersistentHookState::Alive { command };
                }
                stale.get_or_insert(command);
            }
        }
    }
    if stale.is_some() || managed_without_command {
        PersistentHookState::Stale { command: stale }
    } else {
        PersistentHookState::Absent
    }
}

pub(crate) struct HookConfigGuard {
    path: PathBuf,
    directory: PathBuf,
    created_file: bool,
    created_directory: bool,
    lease: HookLease,
}

impl HookConfigGuard {
    pub(crate) fn install() -> HookInstallResult<Self> {
        let directory = claude_project_dir(&env::current_dir()?);
        let path = directory.join("settings.local.json");
        // #544: a managed block naming a binary that no longer exists is dead
        // config by definition. Reap it before any branch below returns, so an
        // orphan survives neither an unreachable IPC socket nor a persistent
        // global hook short-circuiting the install.
        prune_dead_project_hooks(&path);
        if !paneflow_ipc_reachable() {
            sweep_orphan_hook_config(&path, remove_paneflow_hooks);
            return Ok(HookInstall::Skipped(HookInstallSkip::IpcUnavailable));
        }
        match persistent_claude_hooks_state()? {
            PersistentHookState::Alive { command } => {
                crate::diagnose(&format!(
                    "claude: using persistent hook ({})",
                    safe_log_text(&command)
                ));
                sweep_orphan_hook_config(&path, remove_paneflow_hooks);
                return Ok(HookInstall::Skipped(HookInstallSkip::PersistentClaudeHook));
            }
            PersistentHookState::Stale { command } => {
                crate::diagnose(&format!(
                    "claude: ignoring stale persistent hook ({})",
                    command
                        .as_deref()
                        .map(safe_log_text)
                        .unwrap_or_else(|| "missing command".into())
                ));
            }
            PersistentHookState::Absent => {}
        }
        Self::install_at(&directory).map(HookInstall::Installed)
    }

    pub(crate) fn install_at(directory: &Path) -> std::io::Result<Self> {
        refuse_symlinked_project_hook_file(&directory.join("settings.local.json"), "Claude Code")?;
        let installed = install_hook_config_file(
            directory,
            "settings.local.json",
            "Claude Code",
            merge_paneflow_hooks,
            // #202: settings.local.json carries the user's permission
            // grants; a parse failure must refuse, never clobber.
            InvalidJsonPolicy::Refuse,
        )?;
        Ok(Self {
            path: installed.path,
            directory: directory.to_path_buf(),
            created_file: installed.created_file,
            created_directory: installed.created_directory,
            lease: installed.lease,
        })
    }
}

/// `.claude` directory Claude Code actually reads for `cwd` (#543).
///
/// A linked git worktree resolves its project through the main checkout, so a
/// pane whose cwd is a worktree must install into the main checkout's
/// `.claude/` or the hooks are written to a file Claude Code never opens - and
/// the pane's `ai.*` lifecycle events silently never arrive. Every other cwd
/// keeps the existing behaviour.
fn claude_project_dir(cwd: &Path) -> PathBuf {
    match linked_worktree_main_checkout(cwd) {
        Some(root) => {
            crate::diagnose(&format!(
                "claude: worktree {} resolves to main checkout {}",
                safe_path_display(cwd),
                safe_path_display(&root)
            ));
            root.join(".claude")
        }
        None => cwd.join(".claude"),
    }
}

/// Remove managed groups from the project-local settings whose hook program is
/// gone (#544). Best-effort: unreadable, unparseable, and symlinked files are
/// left exactly as they are, and the file itself is never deleted - ownership
/// is unknown here, only the dead groups are.
fn prune_dead_project_hooks(path: &Path) {
    // #234: the same two symlink refusals `sweep_orphan_hook_config` makes.
    // A project-local link is under the checkout's control, not the user's.
    if path.parent().is_some_and(config_dir_is_symlink) || config_dir_is_symlink(path) {
        return;
    }
    let pruned = with_config_lock(path, || {
        let Some(content) = read_optional_text(path)? else {
            return Ok(false);
        };
        let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Ok(false);
        };
        if !paneflow_agent_config::claude_hooks::remove_dead_hooks(&mut root, &|program| {
            !program_exists(program)
        }) {
            return Ok(false);
        }
        write_json_atomic(path, &root)?;
        Ok(true)
    });
    match pruned {
        Ok(true) => crate::diagnose(&format!(
            "claude: removed stale managed hooks from {}",
            safe_path_display(path)
        )),
        Ok(false) => {}
        Err(error) => crate::diagnose(&format!(
            "claude: could not prune stale hooks in {}: {error}",
            safe_path_display(path)
        )),
    }
}

impl Drop for HookConfigGuard {
    fn drop(&mut self) {
        cleanup_hook_config_file(
            &self.path,
            &self.directory,
            self.created_file,
            self.created_directory,
            remove_paneflow_hooks,
            &mut self.lease,
        );
    }
}

fn paneflow_hook_commands_in_group(group: &serde_json::Value) -> Vec<String> {
    group
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(serde_json::Value::as_str))
        .filter(|command| is_paneflow_hook_command(command))
        .map(ToOwned::to_owned)
        .collect()
}

fn paneflow_hook_command_program_exists(command: &str) -> bool {
    paneflow_hook_program_token(command)
        .as_deref()
        .is_some_and(program_exists)
}

fn program_exists(program: &str) -> bool {
    let path = Path::new(program);
    if path.is_file() {
        return true;
    }
    if path.is_absolute()
        || path
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return false;
    }
    let Some(search_path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&search_path).any(|directory| {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return true;
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use paneflow_agent_config::claude_hooks::MANAGED_MARKER;
    use serde_json::json;

    /// Issue #543: a pane whose cwd is a linked worktree must install into the
    /// main checkout's `.claude/`, the only file Claude Code opens.
    #[test]
    fn worktree_panes_target_the_main_checkout_dot_claude() {
        let temp = tempfile::TempDir::new().unwrap();
        let main = temp.path().join("repo");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("feature")).unwrap();
        let worktree = temp.path().join("repo.worktrees").join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();

        assert_eq!(claude_project_dir(&worktree), main.join(".claude"));

        let plain = temp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            claude_project_dir(&plain),
            plain.join(".claude"),
            "a cwd outside any repository keeps its own .claude"
        );
    }

    /// Issue #544: an orphaned managed block naming a pruned cache directory
    /// is reaped no matter who owns the file, while the rest of the user's
    /// settings are left byte-identical.
    #[test]
    fn dead_managed_blocks_are_pruned_from_the_project_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("settings.local.json");
        let dead = temp.path().join("gone/paneflow-ai-hook");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "permissions": { "allow": ["Bash"] },
                "hooks": {
                    "Stop": [{
                        MANAGED_MARKER: true,
                        "hooks": [{
                            "type": "command",
                            "command": format!("{} Stop", dead.display()),
                        }]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        prune_dead_project_hooks(&path);

        let pruned: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(pruned, json!({ "permissions": { "allow": ["Bash"] } }));
    }

    #[test]
    fn pruning_leaves_a_live_block_and_an_unreadable_file_alone() {
        let temp = tempfile::TempDir::new().unwrap();
        let live = temp.path().join("paneflow-ai-hook");
        std::fs::File::create(&live).unwrap();

        let path = temp.path().join("settings.local.json");
        let settings = json!({
            "hooks": {
                "Stop": [{
                    MANAGED_MARKER: true,
                    "hooks": [{
                        "type": "command",
                        "command": format!("{} Stop", live.display()),
                    }]
                }]
            }
        });
        std::fs::write(&path, serde_json::to_string(&settings).unwrap()).unwrap();
        prune_dead_project_hooks(&path);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap())
                .unwrap(),
            settings
        );

        let corrupt = temp.path().join("corrupt.json");
        std::fs::write(&corrupt, "{not json").unwrap();
        prune_dead_project_hooks(&corrupt);
        assert_eq!(std::fs::read_to_string(&corrupt).unwrap(), "{not json");
    }

    #[test]
    fn persistent_hook_requires_an_existing_program() {
        let stale = json!({
            "hooks": { "Stop": [{
                MANAGED_MARKER: true,
                "hooks": [{ "command": "/missing/paneflow-ai-hook Stop" }]
            }]}
        });
        assert!(matches!(
            settings_managed_hook_state(&stale),
            PersistentHookState::Stale { .. }
        ));

        let temp = tempfile::TempDir::new().unwrap();
        let executable = temp.path().join("paneflow-ai-hook");
        std::fs::File::create(&executable).unwrap();
        let alive = json!({
            "hooks": { "Stop": [{
                MANAGED_MARKER: true,
                "hooks": [{ "command": format!("{} Stop", executable.display()) }]
            }]}
        });
        assert!(matches!(
            settings_managed_hook_state(&alive),
            PersistentHookState::Alive { .. }
        ));
        assert!(settings_has_managed_hook(&alive));
    }

    #[test]
    fn persistent_hook_read_errors_are_not_treated_as_absence() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(&path, [0xff]).unwrap();

        assert_eq!(
            persistent_claude_hooks_state_at(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(path).unwrap(), [0xff]);
    }
}
