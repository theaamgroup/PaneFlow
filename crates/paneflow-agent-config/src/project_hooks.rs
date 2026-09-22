//! Reap PaneFlow hook commands whose program no longer exists (#544, #662).
//!
//! The Claude shim used to do this only while installing its own project
//! file. Grok executes that same `.claude/settings.local.json` and reports
//! the failure as `project/settings.local`, so a version-pinned command left
//! behind by a killed session errors on every tool call until something
//! visits the file. The shim now does that visit for every wrapped agent,
//! before the real CLI starts.

use std::io::{Error, Result};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent_dirs::linked_worktree_main_checkout;
use crate::claude_hooks::remove_dead_hooks;
use crate::io::{read_optional_text, write_json_atomic};
use crate::lock::with_config_lock;

/// How far above `cwd` to look for a checkout root. A normal repository is
/// a handful of directories; the cap keeps a pathological path from walking
/// to the filesystem root.
const MAX_PROJECT_HOOK_ANCESTORS: usize = 64;

/// Files a wrapped agent may execute that the shim itself writes.
const PROJECT_HOOK_FILES: &[&str] = &[".claude/settings.local.json", ".codex/hooks.json"];

/// Outcome of one reap. Per-file failures are reported and do not hide a
/// file that was cleaned.
pub struct ProjectHookReap {
    pub changed: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, Error)>,
}

/// Remove dead PaneFlow hook commands from the project files `cwd` will
/// cause an agent to read.
///
/// Walks from `cwd` up to the git checkout that contains it (cwd alone when
/// there is no checkout) and, for a linked worktree, the main checkout as
/// well. Claude Code opens the main checkout's `.claude/` (#543); Grok opens
/// the worktree's. Missing files are skipped. A symlink, at the file or its
/// parent directory, is left untouched (#234).
pub fn reap_dead_project_hooks(cwd: &Path) -> ProjectHookReap {
    let mut report = ProjectHookReap {
        changed: Vec::new(),
        errors: Vec::new(),
    };
    for path in project_hook_config_paths(cwd) {
        if !path.is_file() {
            continue;
        }
        match prune_dead_hook_file(&path) {
            Ok(true) => report.changed.push(path),
            Ok(false) => {}
            Err(error) => report.errors.push((path, error)),
        }
    }
    report
}

/// Strip dead PaneFlow commands from one JSON hook file.
///
/// `Ok(false)` means the file was left as it was: absent, symlinked,
/// unparseable, or already free of dead commands. The file is never deleted;
/// user keys that share it (permission grants, status line, other hooks)
/// stay. `Err` is a lock or write failure.
pub fn prune_dead_hook_file(path: &Path) -> Result<bool> {
    if is_symlink(path) || path.parent().is_some_and(is_symlink) {
        return Ok(false);
    }
    with_config_lock(path, || {
        let Some(content) = read_optional_text(path)? else {
            return Ok(false);
        };
        let Ok(mut root) = serde_json::from_str::<Value>(&content) else {
            return Ok(false);
        };
        if !remove_dead_hooks(&mut root, &hook_program_is_missing) {
            return Ok(false);
        }
        write_json_atomic(path, &root)?;
        Ok(true)
    })
}

fn project_hook_config_paths(cwd: &Path) -> Vec<PathBuf> {
    let root = git_checkout_root(cwd);
    let mut paths = Vec::new();
    for (index, ancestor) in cwd.ancestors().enumerate() {
        if index == MAX_PROJECT_HOOK_ANCESTORS {
            break;
        }
        push_project_hook_files(&mut paths, ancestor);
        if root.is_none() || root.as_deref() == Some(ancestor) {
            break;
        }
    }
    if let Some(main) = linked_worktree_main_checkout(cwd) {
        push_project_hook_files(&mut paths, &main);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn push_project_hook_files(paths: &mut Vec<PathBuf>, directory: &Path) {
    for relative in PROJECT_HOOK_FILES {
        paths.push(directory.join(relative));
    }
}

fn git_checkout_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Whether `program` is not a file this machine can open.
///
/// Absolute paths and relative paths that already name a directory are
/// missing unless that exact file exists. A bare `paneflow-ai-hook` is
/// missing unless some `PATH` entry has it. Matches the Claude shim's
/// staleness check so the two cannot disagree about the same command.
fn hook_program_is_missing(program: &str) -> bool {
    !hook_program_exists(program)
}

fn hook_program_exists(program: &str) -> bool {
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
    let Some(search_path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&search_path).any(|directory| directory.join(program).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_hooks::MANAGED_MARKER;
    use serde_json::json;

    fn write_hooks(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn read_hooks(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The user's failure: `post_tool_use hook (project/settings.local)` runs
    /// a version-pinned `paneflow-ai-hook` that an upgrade already deleted.
    /// Reaping from a subdirectory must still find the checkout's file, and
    /// must not climb into a parent project or drop the user's permissions.
    #[test]
    fn reaping_from_inside_a_checkout_removes_only_that_checkouts_dead_hooks() {
        let temp = tempfile::TempDir::new().unwrap();
        let parent = temp.path().join("outside");
        let repo = parent.join("repo");
        let nested = repo.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let dead = "/Users/dayers/Library/Caches/paneflow/bin/0.2.0/paneflow-ai-hook";
        let project = repo.join(".claude/settings.local.json");
        write_hooks(
            &project,
            &json!({
                "permissions": { "allow": ["Bash(python3 *)"] },
                "hooks": {
                    "PostToolUse": [{
                        MANAGED_MARKER: true,
                        "hooks": [{
                            "type": "command",
                            "command": format!("{dead} PostToolUse"),
                            "timeout": 5
                        }]
                    }],
                    "PreToolUse": [{
                        "hooks": [
                            { "type": "command", "command": format!("{dead} PreToolUse") },
                            { "type": "command", "command": "my-own-hook" }
                        ]
                    }]
                }
            }),
        );
        let outside = parent.join(".claude/settings.local.json");
        write_hooks(
            &outside,
            &json!({
                "hooks": {
                    "PostToolUse": [{
                        MANAGED_MARKER: true,
                        "hooks": [{ "command": format!("{dead} PostToolUse") }]
                    }]
                }
            }),
        );
        let codex = repo.join(".codex/hooks.json");
        write_hooks(
            &codex,
            &json!({
                "hooks": {
                    "SessionStart": [{
                        MANAGED_MARKER: true,
                        "hooks": [{ "command": format!("{dead} SessionStart") }]
                    }]
                }
            }),
        );

        let report = reap_dead_project_hooks(&nested);

        assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
        assert!(report.changed.iter().any(|path| path == &project));
        assert!(report.changed.iter().any(|path| path == &codex));
        assert!(!report.changed.iter().any(|path| path == &outside));
        assert_eq!(
            read_hooks(&project),
            json!({
                "permissions": { "allow": ["Bash(python3 *)"] },
                "hooks": {
                    "PreToolUse": [{
                        "hooks": [{ "type": "command", "command": "my-own-hook" }]
                    }]
                }
            })
        );
        assert_eq!(read_hooks(&codex), json!({}));
        assert!(read_hooks(&outside).get("hooks").is_some());
    }

    #[test]
    fn a_live_hook_program_and_a_symlink_are_left_alone() {
        let temp = tempfile::TempDir::new().unwrap();
        let live = temp.path().join("paneflow-ai-hook");
        std::fs::File::create(&live).unwrap();
        let path = temp.path().join("settings.local.json");
        let settings = json!({
            "hooks": {
                "Stop": [{
                    MANAGED_MARKER: true,
                    "hooks": [{ "command": format!("{} Stop", live.display()) }]
                }]
            }
        });
        let bytes = serde_json::to_string(&settings).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        assert!(!prune_dead_hook_file(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);

        let link = temp.path().join("linked.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(!prune_dead_hook_file(&link).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }

    #[test]
    fn an_unparseable_hook_file_is_not_rewritten() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("settings.local.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(!prune_dead_hook_file(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }

    /// Claude reads the main checkout. Grok reads the worktree. Both files
    /// have to be reaped or the host that was not visited keeps erroring.
    #[test]
    fn a_linked_worktree_reaps_its_own_file_and_the_main_checkout() {
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

        let dead = temp.path().join("missing/paneflow-ai-hook");
        let command = format!("{} PostToolUse", dead.display());
        let body = json!({
            "hooks": {
                "PostToolUse": [{
                    MANAGED_MARKER: true,
                    "hooks": [{ "command": command }]
                }]
            }
        });
        let main_hooks = main.join(".claude/settings.local.json");
        let worktree_hooks = worktree.join(".claude/settings.local.json");
        write_hooks(&main_hooks, &body);
        write_hooks(&worktree_hooks, &body);

        let report = reap_dead_project_hooks(&std::fs::canonicalize(&worktree).unwrap());

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(read_hooks(&main_hooks), json!({}));
        assert_eq!(read_hooks(&worktree_hooks), json!({}));
    }

    #[test]
    fn quoted_missing_programs_count_as_dead() {
        let raw = "/tmp/backup(1)/paneflow-ai-hook";
        let command = format!("'{raw}' PostToolUse");
        assert_eq!(
            crate::claude_hooks::paneflow_hook_program_token(&command).as_deref(),
            Some(raw)
        );
        assert!(hook_program_is_missing(raw));
    }
}
