use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::Hsla;

use crate::theme::UiColors;

const GIT_STATUS_DEADLINE: Duration = Duration::from_secs(10);
const GIT_STATUS_STDOUT_CAP: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrackedSummary {
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

impl std::ops::AddAssign for TrackedSummary {
    fn add_assign(&mut self, rhs: Self) {
        self.added += rhs.added;
        self.modified += rhs.modified;
        self.deleted += rhs.deleted;
    }
}

impl std::ops::Add for TrackedSummary {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self {
        self += rhs;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GitSummary {
    pub index: TrackedSummary,
    pub worktree: TrackedSummary,
    pub untracked: u32,
    pub conflict: u32,
}

impl std::ops::AddAssign for GitSummary {
    fn add_assign(&mut self, rhs: Self) {
        self.index += rhs.index;
        self.worktree += rhs.worktree;
        self.untracked += rhs.untracked;
        self.conflict += rhs.conflict;
    }
}

impl GitSummary {
    pub(crate) fn is_unchanged(self) -> bool {
        self == Self::default()
    }

    fn from_porcelain_code(code: [u8; 2]) -> Self {
        match &code {
            b"??" => Self {
                untracked: 1,
                ..Self::default()
            },
            b"!!" => Self::default(),
            b"AA" | b"DD" | [_, b'U'] | [b'U', _] => Self {
                conflict: 1,
                ..Self::default()
            },
            [index, worktree] => Self {
                index: tracked_from_code(*index),
                worktree: tracked_from_code(*worktree),
                ..Self::default()
            },
        }
    }
}

fn tracked_from_code(code: u8) -> TrackedSummary {
    match code {
        b'M' | b'T' => TrackedSummary {
            modified: 1,
            ..TrackedSummary::default()
        },
        b'A' => TrackedSummary {
            added: 1,
            ..TrackedSummary::default()
        },
        b'D' => TrackedSummary {
            deleted: 1,
            ..TrackedSummary::default()
        },
        _ => TrackedSummary::default(),
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GitStatuses {
    entries: HashMap<PathBuf, GitSummary>,
}

impl GitStatuses {
    pub(crate) fn summary(&self, path: &Path) -> GitSummary {
        self.entries.get(path).copied().unwrap_or_default()
    }

    pub(crate) fn parse(root: &Path, prefix: &str, stdout: &[u8]) -> Self {
        let mut entries: HashMap<PathBuf, GitSummary> = HashMap::new();
        for record in stdout.split(|byte| *byte == 0) {
            if record.len() < 4 || record[2] != b' ' {
                continue;
            }
            let summary = GitSummary::from_porcelain_code([record[0], record[1]]);
            if summary.is_unchanged() {
                continue;
            }
            let Ok(path) = std::str::from_utf8(&record[3..]) else {
                continue;
            };
            let Some(relative) = path.strip_prefix(prefix) else {
                continue;
            };
            let absolute = root.join(relative);
            for ancestor in absolute.ancestors().take_while(|dir| dir.starts_with(root)) {
                *entries.entry(ancestor.to_path_buf()).or_default() += summary;
            }
        }
        Self { entries }
    }
}

/// The git status of everything under `root`.
///
/// `None` means git could not be consulted at all - a spawn failure, the
/// deadline, the stdout cap, or a non-zero exit - and the caller must keep the
/// statuses it already has: an empty map is indistinguishable from a clean
/// tree, so storing one on a failed probe paints every row plain until a later
/// probe succeeds. `--no-optional-locks` keeps the read from touching the
/// index, so a concurrent `git add` holding `.git/index.lock` cannot fail it.
pub(crate) fn read(root: &Path) -> Option<GitStatuses> {
    let prefix = git_stdout(root, &["rev-parse", "--show-prefix"])?;
    let prefix = strip_line_terminator(&prefix);
    let stdout = git_stdout(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--no-renames",
            "-z",
            "--",
            ".",
        ],
    )?;
    Some(GitStatuses::parse(root, &prefix, &stdout))
}

/// The git directory that owns `root`, resolved by git itself on the same
/// budget as the status read.
///
/// `workspace::find_git_dir` is deliberately not used: its stat probe is
/// fail-closed at 250 ms (issue #403), and a slow or network mount answering
/// `None` would leave the tree with no `.git` watch, so an index-only change
/// (an agent's `git add` in another pane) would raise no watcher event at all.
/// `--absolute-git-dir` also answers the per-worktree git directory of a
/// linked worktree, which is where that worktree's `index` lives.
pub(crate) fn git_dir(root: &Path) -> Option<PathBuf> {
    let stdout = git_stdout(root, &["rev-parse", "--absolute-git-dir"])?;
    let path = strip_line_terminator(&stdout);
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Drop only the line terminator git appends to a single-line answer. A path
/// segment may legally begin or end with a space and the porcelain records
/// keep those spaces, so trimming arbitrary whitespace off the prefix makes
/// every record fail to strip it and silently uncolors the whole subtree.
fn strip_line_terminator(stdout: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).into_owned();
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

fn git_stdout(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = crate::workspace::worktree::git_command();
    crate::workspace::worktree::git_subcommand(&mut cmd, args);
    cmd.current_dir(root);
    let output =
        paneflow_process::run_with_timeout(cmd, GIT_STATUS_DEADLINE, GIT_STATUS_STDOUT_CAP).ok()?;
    output.status.success().then_some(output.stdout)
}

pub(crate) fn label_color(summary: GitSummary, ui: UiColors) -> Option<Hsla> {
    let tracked = summary.index + summary.worktree;
    if summary.conflict > 0 {
        Some(ui.vc_conflict)
    } else if tracked.deleted > 0 {
        Some(ui.vc_deleted)
    } else if tracked.modified > 0 {
        Some(ui.vc_modified)
    } else if tracked.added > 0 || summary.untracked > 0 {
        Some(ui.vc_added)
    } else {
        None
    }
}

pub(crate) fn status_indicator(summary: GitSummary, ui: UiColors) -> Option<(&'static str, Hsla)> {
    if summary.conflict > 0 {
        return Some(("!", ui.vc_conflict));
    }
    if summary.untracked > 0 {
        return Some(("U", ui.vc_added));
    }
    if summary.worktree.deleted > 0 {
        return Some(("D", ui.vc_deleted));
    }
    if summary.worktree.modified > 0 {
        return Some(("M", ui.vc_modified));
    }
    if summary.index.deleted > 0 {
        return Some(("D", ui.vc_deleted));
    }
    if summary.index.modified > 0 {
        return Some(("M", ui.vc_modified));
    }
    if summary.index.added > 0 {
        return Some(("A", ui.vc_added));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui() -> UiColors {
        crate::theme::ui_colors_with(&crate::theme::paneflow_dark())
    }

    fn porcelain(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            out.extend_from_slice(record.as_bytes());
            out.push(0);
        }
        out
    }

    fn statuses(records: &[&str]) -> GitStatuses {
        GitStatuses::parse(Path::new("/repo"), "", &porcelain(records))
    }

    fn test_git(cwd: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn committed_repo(root: &Path) -> bool {
        if !test_git(root, &["init"]) {
            return false;
        }
        std::fs::create_dir_all(root.join("src").join("app")).expect("nested directory");
        std::fs::write(root.join("src").join("app").join("row.rs"), "one\n").expect("row");
        std::fs::write(root.join("README.md"), "readme\n").expect("readme");
        test_git(root, &["add", "."])
            && test_git(
                root,
                &[
                    "-c",
                    "user.email=tests@paneflow.dev",
                    "-c",
                    "user.name=tests",
                    "commit",
                    "-m",
                    "init",
                ],
            )
    }

    #[test]
    fn porcelain_codes_map_to_tracked_index_and_worktree_sides() {
        let modified_index = GitSummary::from_porcelain_code(*b"M ");
        assert_eq!(modified_index.index.modified, 1);
        assert_eq!(modified_index.worktree.modified, 0);

        let modified_worktree = GitSummary::from_porcelain_code(*b" M");
        assert_eq!(modified_worktree.worktree.modified, 1);
        assert_eq!(modified_worktree.index.modified, 0);

        assert_eq!(GitSummary::from_porcelain_code(*b"A ").index.added, 1);
        assert_eq!(GitSummary::from_porcelain_code(*b" D").worktree.deleted, 1);
        assert_eq!(GitSummary::from_porcelain_code(*b"T ").index.modified, 1);
    }

    #[test]
    fn untracked_conflicted_and_ignored_codes_follow_zed() {
        assert_eq!(GitSummary::from_porcelain_code(*b"??").untracked, 1);
        assert_eq!(GitSummary::from_porcelain_code(*b"UU").conflict, 1);
        assert_eq!(GitSummary::from_porcelain_code(*b"AA").conflict, 1);
        assert_eq!(GitSummary::from_porcelain_code(*b"DD").conflict, 1);
        assert_eq!(GitSummary::from_porcelain_code(*b"AU").conflict, 1);
        assert!(GitSummary::from_porcelain_code(*b"!!").is_unchanged());
        assert!(GitSummary::from_porcelain_code(*b"  ").is_unchanged());
    }

    #[test]
    fn renamed_and_copied_codes_carry_no_summary() {
        assert!(GitSummary::from_porcelain_code(*b"R ").is_unchanged());
        assert!(GitSummary::from_porcelain_code(*b"C ").is_unchanged());
    }

    #[test]
    fn summaries_roll_up_into_every_ancestor_directory() {
        let statuses = statuses(&[" M src/app/row.rs", "?? src/app/new.rs", "D  docs/old.md"]);

        assert_eq!(
            statuses
                .summary(Path::new("/repo/src/app/row.rs"))
                .worktree
                .modified,
            1
        );
        let app = statuses.summary(Path::new("/repo/src/app"));
        assert_eq!(app.worktree.modified, 1);
        assert_eq!(app.untracked, 1);
        let src = statuses.summary(Path::new("/repo/src"));
        assert_eq!(src.worktree.modified, 1);
        assert_eq!(src.untracked, 1);
        assert_eq!(statuses.summary(Path::new("/repo/docs")).index.deleted, 1);
        let root = statuses.summary(Path::new("/repo"));
        assert_eq!(root.worktree.modified, 1);
        assert_eq!(root.untracked, 1);
        assert_eq!(root.index.deleted, 1);
    }

    #[test]
    fn unchanged_paths_have_no_summary() {
        let statuses = statuses(&[" M src/app/row.rs"]);
        assert!(
            statuses
                .summary(Path::new("/repo/src/app/view.rs"))
                .is_unchanged()
        );
        assert!(statuses.summary(Path::new("/repo/docs")).is_unchanged());
    }

    #[test]
    fn a_subdirectory_root_strips_the_repository_prefix() {
        let statuses = GitStatuses::parse(
            Path::new("/repo/src-app"),
            "src-app/",
            &porcelain(&[" M src-app/src/main.rs"]),
        );

        assert_eq!(
            statuses
                .summary(Path::new("/repo/src-app/src/main.rs"))
                .worktree
                .modified,
            1
        );
        assert_eq!(
            statuses
                .summary(Path::new("/repo/src-app/src"))
                .worktree
                .modified,
            1
        );
    }

    #[test]
    fn malformed_records_are_skipped() {
        let statuses = statuses(&["", "M", " M"]);
        assert!(statuses.summary(Path::new("/repo")).is_unchanged());
    }

    #[test]
    fn read_maps_a_real_repository_onto_absolute_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        if !committed_repo(root) {
            return;
        }
        std::fs::write(root.join("src").join("app").join("row.rs"), "two\n").expect("edit");
        std::fs::write(root.join("src").join("app").join("new.rs"), "new\n").expect("new file");

        let statuses = read(root).expect("a repository answers Some");

        assert_eq!(
            statuses
                .summary(&root.join("src").join("app").join("row.rs"))
                .worktree
                .modified,
            1
        );
        assert_eq!(
            statuses
                .summary(&root.join("src").join("app").join("new.rs"))
                .untracked,
            1
        );
        let app = statuses.summary(&root.join("src").join("app"));
        assert_eq!(app.worktree.modified, 1);
        assert_eq!(app.untracked, 1);
        assert_eq!(statuses.summary(&root.join("src")).untracked, 1);
        assert!(statuses.summary(&root.join("README.md")).is_unchanged());
    }

    #[test]
    fn read_from_a_subdirectory_scopes_to_that_subtree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        if !committed_repo(root) {
            return;
        }
        std::fs::write(root.join("src").join("app").join("row.rs"), "two\n").expect("edit");
        std::fs::write(root.join("README.md"), "changed\n").expect("readme edit");

        let statuses = read(&root.join("src")).expect("a repository answers Some");

        assert_eq!(
            statuses
                .summary(&root.join("src").join("app").join("row.rs"))
                .worktree
                .modified,
            1
        );
        assert!(statuses.summary(&root.join("README.md")).is_unchanged());
    }

    /// Outside a repository `rev-parse --show-prefix` exits non-zero, so
    /// `read` answers `None`: git could not be consulted, which the caller
    /// must not confuse with a clean tree.
    #[test]
    fn read_outside_a_repository_answers_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        if test_git(dir.path(), &["rev-parse", "--git-dir"]) {
            return;
        }
        std::fs::write(dir.path().join("loose.txt"), "loose\n").expect("loose file");

        assert!(read(dir.path()).is_none());
    }

    #[test]
    fn a_clean_repository_answers_some_with_no_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        if !committed_repo(root) {
            return;
        }

        let statuses = read(root).expect("a clean repository still answers Some");

        assert!(statuses.entries.is_empty());
    }

    #[test]
    fn git_dir_answers_the_repository_git_directory_and_none_outside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        if !committed_repo(root) {
            return;
        }

        let resolved = git_dir(root).expect("a repository answers its git directory");

        assert_eq!(
            resolved.canonicalize().expect("canonical git dir"),
            root.join(".git").canonicalize().expect("canonical .git")
        );

        let outside = tempfile::tempdir().expect("tempdir");
        if test_git(outside.path(), &["rev-parse", "--git-dir"]) {
            return;
        }
        assert_eq!(git_dir(outside.path()), None);
    }

    #[test]
    fn the_prefix_keeps_path_whitespace_and_loses_only_its_line_terminator() {
        assert_eq!(strip_line_terminator(b"has space /\n"), "has space /");
        assert_eq!(strip_line_terminator(b"has space /\r\n"), "has space /");
        assert_eq!(strip_line_terminator(b"has space /"), "has space /");

        let statuses = GitStatuses::parse(
            Path::new("/repo/has space "),
            &strip_line_terminator(b"has space /\n"),
            &porcelain(&[" M has space /main.rs"]),
        );

        assert_eq!(
            statuses
                .summary(Path::new("/repo/has space /main.rs"))
                .worktree
                .modified,
            1
        );
    }

    /// Finding 3 (#539): one summary ranks `vc_modified` for the label and
    /// `vc_added` for the letter, which is why `row.rs` paints a directory's
    /// dot from `label_color` instead of the indicator's own hue.
    #[test]
    fn a_mixed_directory_ranks_its_label_and_its_letter_differently() {
        let ui = ui();
        let mixed = GitSummary {
            worktree: TrackedSummary {
                modified: 1,
                ..TrackedSummary::default()
            },
            untracked: 1,
            ..GitSummary::default()
        };

        assert_eq!(label_color(mixed, ui), Some(ui.vc_modified));
        assert_eq!(status_indicator(mixed, ui), Some(("U", ui.vc_added)));
        assert_ne!(ui.vc_modified, ui.vc_added);
    }

    #[test]
    fn label_color_orders_conflict_deleted_modified_created() {
        let ui = ui();
        let conflict = GitSummary {
            conflict: 1,
            worktree: TrackedSummary {
                modified: 1,
                ..TrackedSummary::default()
            },
            ..GitSummary::default()
        };
        assert_eq!(label_color(conflict, ui), Some(ui.vc_conflict));

        let deleted = GitSummary {
            index: TrackedSummary {
                deleted: 1,
                ..TrackedSummary::default()
            },
            worktree: TrackedSummary {
                modified: 1,
                ..TrackedSummary::default()
            },
            ..GitSummary::default()
        };
        assert_eq!(label_color(deleted, ui), Some(ui.vc_deleted));

        let modified = GitSummary {
            worktree: TrackedSummary {
                modified: 1,
                ..TrackedSummary::default()
            },
            untracked: 1,
            ..GitSummary::default()
        };
        assert_eq!(label_color(modified, ui), Some(ui.vc_modified));

        let created = GitSummary {
            untracked: 1,
            ..GitSummary::default()
        };
        assert_eq!(label_color(created, ui), Some(ui.vc_added));
        assert_eq!(label_color(GitSummary::default(), ui), None);
    }

    #[test]
    fn status_indicator_prefers_the_worktree_side_over_the_index() {
        let ui = ui();
        let both = GitSummary {
            index: TrackedSummary {
                added: 1,
                ..TrackedSummary::default()
            },
            worktree: TrackedSummary {
                modified: 1,
                ..TrackedSummary::default()
            },
            ..GitSummary::default()
        };
        assert_eq!(status_indicator(both, ui), Some(("M", ui.vc_modified)));

        let staged = GitSummary {
            index: TrackedSummary {
                added: 1,
                ..TrackedSummary::default()
            },
            ..GitSummary::default()
        };
        assert_eq!(status_indicator(staged, ui), Some(("A", ui.vc_added)));

        let untracked = GitSummary {
            untracked: 1,
            ..GitSummary::default()
        };
        assert_eq!(status_indicator(untracked, ui), Some(("U", ui.vc_added)));

        let conflict = GitSummary {
            conflict: 1,
            ..GitSummary::default()
        };
        assert_eq!(status_indicator(conflict, ui), Some(("!", ui.vc_conflict)));
        assert_eq!(status_indicator(GitSummary::default(), ui), None);
    }
}
