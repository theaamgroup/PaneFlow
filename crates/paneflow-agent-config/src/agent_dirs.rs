use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::io::home_dir;

/// `$CLAUDE_CONFIG_DIR` when set and non-empty, else `~/.claude`.
pub fn claude_config_dir() -> Option<PathBuf> {
    claude_config_dir_from(home_dir(), std::env::var_os("CLAUDE_CONFIG_DIR"))
}

/// Pure core: the precedence rule, unit-testable without mutating process env.
pub fn claude_config_dir_from(
    home: Option<PathBuf>,
    claude_config_dir: Option<OsString>,
) -> Option<PathBuf> {
    claude_config_dir
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| h.join(".claude")))
}

/// `$CLAUDE_CONFIG_DIR/settings.json` (default `~/.claude/settings.json`).
pub fn claude_settings_json() -> Option<PathBuf> {
    claude_config_dir().map(|dir| dir.join("settings.json"))
}

/// `$CODEX_HOME` when set and non-empty, else `~/.codex`.
pub fn codex_config_dir() -> Option<PathBuf> {
    codex_config_dir_from(home_dir(), std::env::var_os("CODEX_HOME"))
}

pub fn codex_config_dir_from(
    home: Option<PathBuf>,
    codex_home: Option<OsString>,
) -> Option<PathBuf> {
    codex_home
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| h.join(".codex")))
}

/// `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`).
pub fn codex_config_toml() -> Option<PathBuf> {
    codex_config_dir().map(|dir| dir.join("config.toml"))
}

/// Application directory namespace, mirroring `runtime_paths::APP_SUBDIR` in
/// the app crate. The shim ships as a `release-min` build, so this resolves to
/// `paneflow` in every binary that actually writes agent configs.
const APP_SUBDIR: &str = if cfg!(debug_assertions) {
    "paneflow-dev"
} else {
    "paneflow"
};

/// Stable, **non-versioned** path of the extracted `paneflow-ai-hook` callback
/// (issue #542).
///
/// The app materializes this copy at launch
/// (`ai_hooks::extract::ensure_ai_hook_extracted`, the byte-for-byte mirror of
/// `runtime_paths::ai_hook_binary_path`). The shim renders hook commands from
/// it in preference to the version-pinned cache sibling
/// (`<cache_dir>/paneflow/bin/<VERSION>/`), because a managed block that
/// outlives the process that wrote it must keep resolving after the next
/// upgrade prunes that version directory.
///
/// macOS: `~/Library/Application Support/paneflow/bin/paneflow-ai-hook`.
///
/// Computes only; it never creates the directory or extracts anything.
pub fn stable_ai_hook_binary_path() -> Option<PathBuf> {
    stable_ai_hook_binary_path_in(dirs::data_local_dir())
}

/// Pure core, so the layout is testable without a real data directory.
pub fn stable_ai_hook_binary_path_in(data_local_dir: Option<PathBuf>) -> Option<PathBuf> {
    Some(
        data_local_dir?
            .join(APP_SUBDIR)
            .join("bin")
            .join("paneflow-ai-hook"),
    )
}

/// Main-checkout root when `cwd` sits inside a **linked git worktree** (issue
/// #543), else `None`.
///
/// Claude Code does not read project settings from the pane cwd; it resolves
/// the project through the repository, and for a linked worktree that is the
/// main checkout. Writing `<worktree>/.claude/settings.local.json` therefore
/// installs hooks into a file Claude Code never reads, and the pane's `ai.*`
/// lifecycle events never arrive.
///
/// The `.git` entry of a linked worktree is a file holding
/// `gitdir: <main>/.git/worktrees/<name>`; the main checkout is the parent of
/// that `.git` directory. Parsing the pointer directly avoids a
/// `git rev-parse --git-common-dir` subprocess, which would eat the shim's
/// ~15 ms launch budget.
///
/// Returns `None` for an ordinary checkout (`.git` is a directory), for a
/// cwd outside any repository, and for any pointer that does not have the
/// `<main>/.git/worktrees/<name>` shape (a bare-repo worktree has no main
/// checkout to redirect to) - in every one of those cases the caller keeps
/// its existing cwd-relative behaviour.
pub fn linked_worktree_main_checkout(cwd: &Path) -> Option<PathBuf> {
    let pointer = cwd
        .ancestors()
        .map(|ancestor| ancestor.join(".git"))
        .find(|candidate| candidate.exists())?;
    if pointer.is_dir() {
        return None;
    }
    let content = crate::io::read_optional_text(&pointer).ok()??;
    let gitdir = parse_gitdir_pointer(&content)?;
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        pointer.parent()?.join(gitdir)
    };
    let root = main_checkout_from_worktree_gitdir(&gitdir)?;
    root.is_dir().then_some(root)
}

/// `<main>/.git/worktrees/<name>` -> `<main>`. Pure.
fn main_checkout_from_worktree_gitdir(gitdir: &Path) -> Option<PathBuf> {
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let git_dir = worktrees.parent()?;
    if git_dir.file_name()? != ".git" {
        return None;
    }
    git_dir.parent().map(Path::to_path_buf)
}

/// Read the `gitdir:` line out of a linked worktree's `.git` file. Pure.
fn parse_gitdir_pointer(content: &str) -> Option<PathBuf> {
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    #[test]
    fn claude_config_dir_from_honors_claude_config_dir() {
        assert_eq!(
            claude_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("/tmp/claude-cfg")),
            ),
            Some(PathBuf::from("/tmp/claude-cfg")),
        );
    }

    #[test]
    fn claude_config_dir_from_default_is_home_dot_claude() {
        assert_eq!(
            claude_config_dir_from(Some(PathBuf::from("/home/alice")), None),
            Some(PathBuf::from("/home/alice/.claude")),
        );
        assert_eq!(
            claude_config_dir_from(Some(PathBuf::from("/home/alice")), Some(OsString::from("")),),
            Some(PathBuf::from("/home/alice/.claude")),
        );
    }

    /// Process-env path: `CLAUDE_CONFIG_DIR` pointed at a temp dir must win
    /// over `HOME/.claude` when resolving durable Claude hooks.
    #[test]
    fn claude_settings_json_reads_claude_config_dir_env() {
        let td = tempfile::TempDir::new().unwrap();
        let _guard = ClaudeConfigDirGuard::set(td.path());
        assert_eq!(
            claude_settings_json(),
            Some(td.path().join("settings.json")),
        );
    }

    struct ClaudeConfigDirGuard {
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static CLAUDE_CONFIG_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(deprecated)]
    impl ClaudeConfigDirGuard {
        fn set(path: &Path) -> Self {
            let lock = CLAUDE_CONFIG_DIR_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
            std::env::set_var("CLAUDE_CONFIG_DIR", path);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    #[allow(deprecated)]
    impl Drop for ClaudeConfigDirGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
        }
    }

    #[test]
    fn stable_ai_hook_path_is_non_versioned_under_data_local() {
        assert_eq!(
            stable_ai_hook_binary_path_in(Some(PathBuf::from(
                "/home/alice/Library/Application Support"
            ))),
            Some(PathBuf::from(format!(
                "/home/alice/Library/Application Support/{APP_SUBDIR}/bin/paneflow-ai-hook"
            ))),
        );
        assert_eq!(stable_ai_hook_binary_path_in(None), None);
    }

    /// Issue #543: the pane cwd of a linked worktree must resolve to the main
    /// checkout, because that is the file Claude Code actually reads.
    #[test]
    fn linked_worktree_resolves_to_the_main_checkout() {
        let temp = tempfile::TempDir::new().unwrap();
        let main = temp.path().join("repo");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("feature")).unwrap();
        let worktree = temp.path().join("repo.worktrees").join("feature");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();

        assert_eq!(linked_worktree_main_checkout(&worktree), Some(main.clone()));
        assert_eq!(
            linked_worktree_main_checkout(&worktree.join("src")),
            Some(main),
            "a pane deeper inside the worktree resolves the same way"
        );
    }

    #[test]
    fn ordinary_checkouts_and_non_repositories_keep_their_cwd() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(linked_worktree_main_checkout(&repo), None);

        let loose = temp.path().join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(linked_worktree_main_checkout(&loose), None);
    }

    /// A bare-repo worktree has no main checkout to redirect into, so the
    /// caller must keep its cwd rather than invent a root.
    #[test]
    fn worktree_of_a_bare_repository_has_no_main_checkout() {
        let temp = tempfile::TempDir::new().unwrap();
        let bare = temp.path().join("repo.git");
        std::fs::create_dir_all(bare.join("worktrees").join("feature")).unwrap();
        let worktree = temp.path().join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", bare.join("worktrees/feature").display()),
        )
        .unwrap();

        assert_eq!(linked_worktree_main_checkout(&worktree), None);
    }

    #[test]
    fn gitdir_pointer_parsing_tolerates_whitespace_and_junk() {
        assert_eq!(
            parse_gitdir_pointer("gitdir: /tmp/repo/.git/worktrees/a\n"),
            Some(PathBuf::from("/tmp/repo/.git/worktrees/a"))
        );
        assert_eq!(parse_gitdir_pointer("gitdir:\n"), None);
        assert_eq!(parse_gitdir_pointer("ref: refs/heads/main\n"), None);
    }

    #[test]
    fn codex_config_dir_from_honors_codex_home() {
        let td = tempfile::TempDir::new().unwrap();
        assert_eq!(
            super::codex_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(td.path().as_os_str().to_os_string()),
            ),
            Some(td.path().to_path_buf()),
        );
    }

    #[test]
    fn codex_config_dir_from_falls_back_when_codex_home_empty() {
        assert_eq!(
            super::codex_config_dir_from(
                Some(PathBuf::from("/home/alice")),
                Some(OsString::from("")),
            ),
            Some(PathBuf::from("/home/alice/.codex")),
        );
        assert_eq!(
            super::codex_config_dir_from(Some(PathBuf::from("/home/alice")), None),
            Some(PathBuf::from("/home/alice/.codex")),
        );
    }

    /// Process-env path: `CODEX_HOME` pointed at a temp dir must win over
    /// `HOME/.codex` when resolving the Unix `hooks = true` flag file.
    #[test]
    fn codex_config_toml_reads_codex_home_env() {
        let td = tempfile::TempDir::new().unwrap();
        let _guard = CodexHomeGuard::set(td.path());
        assert_eq!(
            super::codex_config_toml(),
            Some(td.path().join("config.toml")),
        );
    }

    struct CodexHomeGuard {
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static CODEX_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(deprecated)]
    impl CodexHomeGuard {
        fn set(path: &Path) -> Self {
            let lock = CODEX_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("CODEX_HOME");
            // Edition 2021: `set_var` is still safe (unsafe only in 2024).
            std::env::set_var("CODEX_HOME", path);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    #[allow(deprecated)]
    impl Drop for CodexHomeGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }
}
