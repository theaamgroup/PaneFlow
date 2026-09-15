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
/// The redirect moves a config write outside the launched directory, so the
/// pointer is verified the way git itself does rather than merely parsed - a
/// crafted `.git` file naming an unrelated directory must not steer PaneFlow's
/// writes there:
///
/// 1. `.git` is a file holding `gitdir: <main>/.git/worktrees/<name>`, and
///    that directory must exist.
/// 2. Its `gitdir` backlink file must round-trip to the very `.git` file just
///    read. A pointer that does not is not this worktree's administrative
///    directory, whoever wrote it.
/// 3. Its `commondir` (relative to the gitdir unless absolute, defaulting to
///    `../..`) must resolve to a real `.git` **directory** - the main
///    checkout's - whose parent is the root returned.
///
/// Every path is canonicalized before comparison, so `..` segments and
/// symlinks cannot smuggle a mismatch past the backlink check. The whole
/// verification is a handful of stats and two small reads, which keeps it
/// inside the shim's ~15 ms launch budget where a `git rev-parse` subprocess
/// would not.
///
/// Returns `None` for an ordinary checkout (`.git` is a directory), a cwd
/// outside any repository, a bare-repo worktree (its common dir is not named
/// `.git`, so there is no main checkout to redirect to), and any pointer
/// failing a check above - in every one of those cases the caller keeps its
/// existing cwd-relative behaviour.
pub fn linked_worktree_main_checkout(cwd: &Path) -> Option<PathBuf> {
    let pointer = cwd
        .ancestors()
        .map(|ancestor| ancestor.join(".git"))
        .find(|candidate| candidate.exists())?;
    if pointer.is_dir() {
        return None;
    }
    let content = crate::io::read_optional_text(&pointer).ok()??;
    let gitdir = resolve_against(parse_gitdir_pointer(&content)?, pointer.parent()?);
    let gitdir = std::fs::canonicalize(&gitdir).ok()?;
    if !gitdir.is_dir() {
        return None;
    }
    if !backlink_points_at(&gitdir, &pointer) {
        return None;
    }
    main_checkout_from_common_dir(&gitdir)
}

/// Whether the administrative directory's `gitdir` backlink names `pointer`.
///
/// git writes the worktree's own `.git` file path there when the worktree is
/// created, so this is the check that ties an administrative directory to the
/// worktree claiming it. A missing, unreadable, or non-matching backlink fails
/// closed.
///
/// The stored value is relative to the administrative directory when the
/// worktree was created or repaired with `--relative-paths`
/// (`worktree.useRelativePaths`), so it resolves against `gitdir` - never
/// against the shim's process cwd, which would fail every such repository and
/// silently put the hooks back under the worktree.
fn backlink_points_at(gitdir: &Path, pointer: &Path) -> bool {
    let Ok(Some(backlink)) = crate::io::read_optional_text(&gitdir.join("gitdir")) else {
        return false;
    };
    let backlink = backlink.trim();
    if backlink.is_empty() {
        return false;
    }
    let backlink = resolve_against(PathBuf::from(backlink), gitdir);
    let (Ok(backlink), Ok(pointer)) = (
        std::fs::canonicalize(backlink),
        std::fs::canonicalize(pointer),
    ) else {
        return false;
    };
    backlink == pointer
}

/// Main checkout root from a verified administrative directory, via its
/// `commondir` (git's own indirection) with the conventional `../..` layout as
/// the fallback.
///
/// Two conditions, and the second is the load-bearing one. The common dir must
/// be a real `.git` **directory**, which excludes a bare repository's worktree
/// (its common dir is the bare repo, not a `.git`). And the administrative
/// directory must actually live in that common dir's `worktrees/`: a backlink
/// only proves an administrative directory and a worktree agree about each
/// other, so without containment an attacker-built pair could still name any
/// unrelated `<victim>/.git` as its common dir and redirect the config write
/// there.
fn main_checkout_from_common_dir(gitdir: &Path) -> Option<PathBuf> {
    let common = match crate::io::read_optional_text(&gitdir.join("commondir")).ok()? {
        Some(text) if !text.trim().is_empty() => {
            resolve_against(PathBuf::from(text.trim()), gitdir)
        }
        _ => gitdir.parent()?.parent()?.to_path_buf(),
    };
    let common = std::fs::canonicalize(common).ok()?;
    if common.file_name()? != ".git" || !common.is_dir() {
        return None;
    }
    if !admin_dir_belongs_to(gitdir, &common) {
        return None;
    }
    let root = common.parent()?;
    root.is_dir().then(|| root.to_path_buf())
}

/// Whether `gitdir` is one of `<common>/worktrees/*`. Both sides are already
/// canonical, so this is a plain parent comparison.
fn admin_dir_belongs_to(gitdir: &Path, common: &Path) -> bool {
    std::fs::canonicalize(common.join("worktrees"))
        .is_ok_and(|worktrees| gitdir.parent() == Some(worktrees.as_path()))
}

/// Absolute paths stand; relative ones resolve against `base`. Pure.
fn resolve_against(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
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

    /// Build a real repository with a real linked worktree, so the resolver
    /// is verified against git's actual on-disk layout rather than a fixture
    /// that encodes the same assumptions the code makes. Returns
    /// `(main checkout, worktree)`, or `None` when git is unavailable.
    fn real_worktree(temp: &Path, extra: &[&str]) -> Option<(PathBuf, PathBuf)> {
        let main = temp.join("repo");
        std::fs::create_dir_all(&main).ok()?;
        let git = |args: &[&str], cwd: &Path| -> Option<()> {
            let status = std::process::Command::new("git")
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
                .ok()?;
            status.success().then_some(())
        };
        git(&["init", "-q", "-b", "main", "."], &main)?;
        std::fs::write(main.join("seed"), b"seed").ok()?;
        git(&["add", "seed"], &main)?;
        git(&["commit", "-qm", "seed"], &main)?;
        let worktree = temp.join("repo.worktrees").join("feature");
        let mut args = vec!["worktree", "add", "-q"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-b", "feature", worktree.to_str()?]);
        git(&args, &main)?;
        Some((
            std::fs::canonicalize(main).ok()?,
            std::fs::canonicalize(worktree).ok()?,
        ))
    }

    /// Issue #543: the pane cwd of a linked worktree must resolve to the main
    /// checkout, because that is the file Claude Code actually reads.
    #[test]
    fn linked_worktree_resolves_to_the_main_checkout() {
        let temp = tempfile::TempDir::new().unwrap();
        let Some((main, worktree)) = real_worktree(temp.path(), &[]) else {
            eprintln!("skip: git is unavailable in this environment");
            return;
        };

        assert_eq!(linked_worktree_main_checkout(&worktree), Some(main.clone()));

        let nested = worktree.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            linked_worktree_main_checkout(&nested),
            Some(main.clone()),
            "a pane deeper inside the worktree resolves the same way"
        );

        assert_eq!(
            linked_worktree_main_checkout(&main),
            None,
            "the main checkout is an ordinary checkout and keeps its own .claude"
        );
    }

    /// `git worktree add --relative-paths` (and `worktree.useRelativePaths`)
    /// stores the backlink relative to the administrative directory. Resolving
    /// it against the process cwd instead would fail validation on every such
    /// repository and silently put the hooks back under the worktree - the
    /// exact bug #543 is about.
    #[test]
    fn a_relative_backlink_resolves_against_the_admin_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let Some((main, worktree)) = real_worktree(temp.path(), &["--relative-paths"]) else {
            eprintln!("skip: git is unavailable or too old for --relative-paths");
            return;
        };

        let admin = main.join(".git/worktrees/feature");
        let backlink = std::fs::read_to_string(admin.join("gitdir")).unwrap();
        assert!(
            Path::new(backlink.trim()).is_relative(),
            "precondition: git must have stored a relative backlink, got {backlink:?}"
        );

        assert_eq!(linked_worktree_main_checkout(&worktree), Some(main));
    }

    #[test]
    fn non_repositories_keep_their_cwd() {
        let temp = tempfile::TempDir::new().unwrap();
        let loose = temp.path().join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(linked_worktree_main_checkout(&loose), None);
    }

    /// A `.git` file is project-controlled data, and accepting it unverified
    /// would let it steer a config write into an unrelated directory. Every
    /// pointer that git itself would reject must fail closed here.
    #[test]
    fn crafted_worktree_pointers_are_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let victim = temp.path().join("victim");
        std::fs::create_dir_all(victim.join(".git").join("worktrees").join("fake")).unwrap();
        let hostile = temp.path().join("hostile");
        std::fs::create_dir_all(&hostile).unwrap();
        let pointer = hostile.join(".git");
        let admin = victim.join(".git/worktrees/fake");

        // The whole shape is present and only the backlink is missing: the
        // gitdir exists, and `../..` is a real `.git` directory.
        std::fs::write(&pointer, format!("gitdir: {}\n", admin.display())).unwrap();
        assert_eq!(
            linked_worktree_main_checkout(&hostile),
            None,
            "an administrative dir with no gitdir backlink must be refused"
        );

        // Backlink present but naming somebody else's worktree.
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(".git"), "gitdir: /nowhere\n").unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", other.join(".git").display()),
        )
        .unwrap();
        assert_eq!(
            linked_worktree_main_checkout(&hostile),
            None,
            "a backlink pointing at another worktree must be refused"
        );

        // Pointing at a gitdir that does not exist at all.
        std::fs::write(&pointer, "gitdir: /nonexistent/.git/worktrees/x\n").unwrap();
        assert_eq!(linked_worktree_main_checkout(&hostile), None);

        // A backlink only proves an admin dir and a worktree agree about each
        // other - an attacker can build both. Here the pair is fully
        // self-consistent and the admin dir simply names an unrelated
        // `<victim>/.git` as its common dir; only containment rejects it.
        let attacker_admin = temp.path().join("attacker/worktrees/fake");
        std::fs::create_dir_all(&attacker_admin).unwrap();
        std::fs::write(&pointer, format!("gitdir: {}\n", attacker_admin.display())).unwrap();
        std::fs::write(
            attacker_admin.join("gitdir"),
            format!("{}\n", pointer.display()),
        )
        .unwrap();
        std::fs::write(
            attacker_admin.join("commondir"),
            format!("{}\n", victim.join(".git").display()),
        )
        .unwrap();
        assert_eq!(
            linked_worktree_main_checkout(&hostile),
            None,
            "an admin dir outside <common>/worktrees/ must not select that common dir"
        );
    }

    /// A bare repository's worktree has no main checkout to redirect into, so
    /// the caller must keep its cwd rather than invent a root.
    #[test]
    fn worktree_of_a_bare_repository_has_no_main_checkout() {
        let temp = tempfile::TempDir::new().unwrap();
        let bare = temp.path().join("repo.git");
        let admin = bare.join("worktrees").join("feature");
        std::fs::create_dir_all(&admin).unwrap();
        let worktree = temp.path().join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        let pointer = worktree.join(".git");
        std::fs::write(&pointer, format!("gitdir: {}\n", admin.display())).unwrap();
        // A well-formed backlink, so only the bare layout can reject it.
        std::fs::write(admin.join("gitdir"), format!("{}\n", pointer.display())).unwrap();

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
