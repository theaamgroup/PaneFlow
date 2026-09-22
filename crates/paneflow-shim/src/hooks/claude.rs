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

/// Drop PaneFlow commands whose program no longer exists, from the project
/// files a wrapped agent executes before it starts (#662).
///
/// Claude Code and Grok both read `.claude/settings.local.json`. Codex reads
/// `.codex/hooks.json`. A linked worktree resolves those files through the
/// main checkout (#543), which is the copy that has to be clean. User
/// permissions, user hooks, and commands whose program still exists stay.
/// The Claude installer also prunes its own file; this runs for every tool,
/// including agents that never open `HookConfigGuard`.
pub(crate) fn prune_stale_project_hooks(cwd: &Path) {
    let root = linked_worktree_main_checkout(cwd).unwrap_or_else(|| cwd.to_path_buf());
    prune_dead_project_hooks(&root.join(".claude").join("settings.local.json"));
    prune_dead_project_hooks(&root.join(".codex").join("hooks.json"));
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
            "removed stale managed hooks from {}",
            safe_path_display(path)
        )),
        Ok(false) => {}
        Err(error) => crate::diagnose(&format!(
            "could not prune stale hooks in {}: {error}",
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
    /// main checkout's `.claude/`, the only file Claude Code opens. Driven
    /// against a real `git worktree` so the redirect is checked end to end,
    /// verification included, rather than against a hand-built fixture.
    #[test]
    fn worktree_panes_target_the_main_checkout_dot_claude() {
        let temp = tempfile::TempDir::new().unwrap();
        let main = temp.path().join("repo");
        std::fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        if !git(&["init", "-q", "-b", "main", "."], &main) {
            eprintln!("skip: git is unavailable in this environment");
            return;
        }
        std::fs::write(main.join("seed"), b"seed").unwrap();
        assert!(git(&["add", "seed"], &main));
        assert!(git(&["commit", "-qm", "seed"], &main));
        let worktree = temp.path().join("repo.worktrees").join("feature");
        assert!(git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap()
            ],
            &main
        ));

        assert_eq!(
            claude_project_dir(&std::fs::canonicalize(&worktree).unwrap()),
            std::fs::canonicalize(&main).unwrap().join(".claude")
        );

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

    /// Issue #662: any wrapped agent can execute these files, so a dead
    /// version-pinned command is removed from both of them while user
    /// permissions and user hooks stay.
    #[test]
    fn stale_commands_are_pruned_from_claude_and_codex_project_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let dead = temp.path().join("gone/paneflow-ai-hook");
        let live = temp.path().join("paneflow-ai-hook");
        std::fs::File::create(&live).unwrap();
        let claude = temp.path().join(".claude");
        let codex = temp.path().join(".codex");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            claude.join("settings.local.json"),
            serde_json::to_string_pretty(&json!({
                "permissions": { "allow": ["Bash"] },
                "hooks": {
                    "PostToolUse": [{
                        MANAGED_MARKER: true,
                        "hooks": [{
                            "type": "command",
                            "command": format!("{} PostToolUse", dead.display()),
                        }]
                    }],
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "echo user-hook" }] },
                        {
                            MANAGED_MARKER: true,
                            "hooks": [{
                                "type": "command",
                                "command": format!("{} Stop", live.display()),
                            }]
                        }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            codex.join("hooks.json"),
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "SessionStart": [{
                        MANAGED_MARKER: true,
                        "hooks": [{
                            "type": "command",
                            "command": format!("{} SessionStart", dead.display()),
                        }]
                    }],
                    "PreToolUse": [{
                        "hooks": [{ "type": "command", "command": "echo codex-user" }]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        prune_stale_project_hooks(temp.path());

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude.join("settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings,
            json!({
                "permissions": { "allow": ["Bash"] },
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "echo user-hook" }] },
                        {
                            MANAGED_MARKER: true,
                            "hooks": [{
                                "type": "command",
                                "command": format!("{} Stop", live.display()),
                            }]
                        }
                    ]
                }
            })
        );
        let hooks: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(codex.join("hooks.json")).unwrap())
                .unwrap();
        assert_eq!(
            hooks,
            json!({
                "hooks": {
                    "PreToolUse": [{
                        "hooks": [{ "type": "command", "command": "echo codex-user" }]
                    }]
                }
            })
        );
    }

    /// Issue #662: a pane whose cwd is a linked worktree executes the main
    /// checkout's hook files, so that is the copy that has to be reaped.
    #[test]
    fn worktree_launch_prunes_the_main_checkout_and_leaves_the_worktree_copy() {
        let temp = tempfile::TempDir::new().unwrap();
        let main = temp.path().join("repo");
        std::fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        if !git(&["init", "-q", "-b", "main", "."], &main) {
            eprintln!("skip: git is unavailable in this environment");
            return;
        }
        std::fs::write(main.join("seed"), b"seed").unwrap();
        assert!(git(&["add", "seed"], &main));
        assert!(git(&["commit", "-qm", "seed"], &main));
        let worktree = temp.path().join("repo.worktrees").join("feature");
        assert!(git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap()
            ],
            &main
        ));

        let dead = temp.path().join("gone/paneflow-ai-hook");
        let dead_command = format!("{} PostToolUse", dead.display());
        let managed = json!({
            "permissions": { "allow": ["Bash"] },
            "hooks": {
                "PostToolUse": [{
                    MANAGED_MARKER: true,
                    "hooks": [{ "type": "command", "command": &dead_command }]
                }]
            }
        });
        for root in [&main, &worktree] {
            let claude = root.join(".claude");
            std::fs::create_dir_all(&claude).unwrap();
            std::fs::write(
                claude.join("settings.local.json"),
                serde_json::to_string_pretty(&managed).unwrap(),
            )
            .unwrap();
            let codex = root.join(".codex");
            std::fs::create_dir_all(&codex).unwrap();
            std::fs::write(
                codex.join("hooks.json"),
                serde_json::to_string_pretty(&json!({
                    "hooks": {
                        "SessionStart": [{
                            MANAGED_MARKER: true,
                            "hooks": [{ "type": "command", "command": &dead_command }]
                        }]
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        prune_stale_project_hooks(&std::fs::canonicalize(&worktree).unwrap());

        let main_settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(main.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            main_settings,
            json!({ "permissions": { "allow": ["Bash"] } })
        );
        let main_hooks: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(main.join(".codex/hooks.json")).unwrap())
                .unwrap();
        assert_eq!(main_hooks, json!({}));

        let worktree_settings =
            std::fs::read_to_string(worktree.join(".claude/settings.local.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&worktree_settings).unwrap(),
            managed,
            "the worktree-local file is not the one the agent reads"
        );
        let worktree_hooks = std::fs::read_to_string(worktree.join(".codex/hooks.json")).unwrap();
        assert!(
            worktree_hooks.contains("paneflow-ai-hook"),
            "the worktree-local Codex file stays until that checkout is launched"
        );
    }
}
