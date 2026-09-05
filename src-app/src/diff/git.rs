//! Git plumbing for the multi-worktree diff viewer (US-005,
//! prd-multi-worktree-diff-2026-Q3.md).
//!
//! All heavy git operations run via `std::process::Command` subprocesses
//! (matching Zed's actual diff/worktree path and Paneflow's existing
//! `GitDiffStats::from_cwd`), never a library. Every command sets
//! `.current_dir()` to the worktree root - never the live shell cwd - and
//! returns a structured error instead of panicking on a non-zero exit or a
//! missing ref. Callers run these off the GPUI main thread (US-007).
//!
//! Diff semantic: `merge-base(HEAD, <base>)..working-tree`, including
//! uncommitted (tracked) changes - "what this branch adds since it diverged
//! from base". Base text comes from one `git cat-file --batch` of
//! `<merge-base>:<path>` specs, new text from the working-tree file on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::engine::{DiffHunk, compute_hunks};

#[cfg(test)]
thread_local! {
    static GIT_COMMANDS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn take_git_commands() -> Vec<String> {
    GIT_COMMANDS.with(|cmds| std::mem::take(&mut *cmds.borrow_mut()))
}

/// A git worktree as reported by `git worktree list --porcelain`.
///
/// On the live Worktree-scope discovery path (US-013): the porcelain parser
/// feeds [`list_repo_worktrees`], which the GUI invokes to enumerate worktrees
/// not open as workspaces. `is_main` / `is_bare` are parsed for completeness but
/// only exercised by the unit tests today, hence the field-level `allow`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub ref_name: Option<String>,
    pub sha: String,
    #[allow(dead_code)]
    pub is_main: bool,
    #[allow(dead_code)]
    pub is_bare: bool,
}

/// How a file changed between the merge-base and the working tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    /// Detected rename (`git diff -M`). The file's `path` is the new
    /// destination; the original path is carried in [`FileDiff::old_path`] /
    /// [`super::view::FileEntry::old_path`] so the UI can show `old → new` as a
    /// single entry instead of a delete + add pair.
    Renamed,
}

/// Per-file diff payload consumed by the renderer (US-006). Carries the raw
/// base/new text so the unified/side-by-side views can shape the actual lines.
#[derive(Clone, Debug)]
pub struct FileDiff {
    /// Path relative to the worktree root (the destination/new path for a
    /// rename).
    pub path: String,
    pub change: FileChange,
    /// Original path for a detected rename ([`FileChange::Renamed`]); `None`
    /// otherwise. Lets the UI render `old → new` and load the base text from the
    /// pre-rename path.
    pub old_path: Option<String>,
    pub base_text: String,
    pub new_text: String,
    pub hunks: Vec<DiffHunk>,
    /// Binary files are listed but not shown (no text rendering).
    pub is_binary: bool,
}

impl FileDiff {
    /// Total added / removed line counts across the file's hunks.
    pub fn line_counts(&self) -> (u32, u32) {
        let mut added = 0;
        let mut removed = 0;
        for h in &self.hunks {
            added += h.new_row_range.end - h.new_row_range.start;
            removed += h.base_row_range.end - h.base_row_range.start;
        }
        (added, removed)
    }
}

/// The diff of one worktree against a resolved base ref. `error` is `Some` when
/// the diff could not be computed (e.g. base ref not found, no merge base).
#[derive(Clone, Debug, Default)]
pub struct WorktreeDiff {
    pub files: Vec<FileDiff>,
    pub error: Option<String>,
}

/// Git-native per-file diffstat for one file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileDiffStat {
    pub added: u32,
    pub removed: u32,
}

/// Map the shared `git worktree list --porcelain` parser into Review
/// [`Worktree`]s. HEAD-less and bare entries are kept (a `worktree ` line is
/// enough) so this listing matches Launch Pad / managed teardown.
pub fn parse_worktrees_from_str(raw: &str, main_worktree_path: Option<&Path>) -> Vec<Worktree> {
    crate::workspace::worktree::parse_worktree_porcelain(raw)
        .into_iter()
        .map(|entry| {
            let ref_name = entry.branch.as_ref().map(|branch| {
                if branch.starts_with("refs/") {
                    branch.clone()
                } else {
                    format!("refs/heads/{branch}")
                }
            });
            let is_main = main_worktree_path.is_some_and(|main| entry.path == main);
            Worktree {
                path: entry.path,
                ref_name,
                sha: entry.sha.unwrap_or_default(),
                is_main,
                is_bare: entry.is_bare,
            }
        })
        .collect()
}

/// Wall-clock deadline for every diff-viewer git call (U-035). Generous enough
/// for a large but healthy repo, short enough that a dead/slow mount or a
/// hanging `.git/config` helper fails instead of wedging the blocking-pool task.
const GIT_DEADLINE: Duration = Duration::from_secs(30);

/// stdout cap for diff-viewer git calls. Comfortably above [`MAX_FILE_BYTES`]
/// (512 KiB) so a legitimate blob of an accepted file is never truncated,
/// while bounding a runaway/hijacked git that streams unbounded output.
/// Exceeding the cap fails the run outright; `MAX_FILE_BYTES` is no longer the
/// backstop for a truncated payload.
const GIT_STDOUT_CAP: u64 = 16 * 1024 * 1024;

/// stdout cap for a whole-column `git cat-file --batch`. Sized for
/// [`MAX_FILE_COUNT`] blobs at [`MAX_FILE_BYTES`] plus cat-file headers.
const GIT_BATCH_STDOUT_CAP: u64 = (MAX_FILE_COUNT as u64) * (MAX_FILE_BYTES + 256) + 8192;

/// Run a git subprocess in `dir`, returning captured stdout bytes on success.
/// A non-zero exit (or a timeout) returns `Err` with the trimmed stderr (or a
/// generic message); the caller renders the diff's "unavailable" state. Never
/// panics, never blocks past [`GIT_DEADLINE`].
fn run_git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    run_git_timed(dir, args, GIT_DEADLINE)
}

#[cfg(test)]
fn record_git_args(args: &[&str]) {
    GIT_COMMANDS.with(|cmds| cmds.borrow_mut().push(args.join(" ")));
}

#[cfg(not(test))]
fn record_git_args(_args: &[&str]) {}

fn run_git_timed(dir: &Path, args: &[&str], deadline: Duration) -> Result<Vec<u8>, String> {
    record_git_args(args);
    if deadline.is_zero() {
        return Err("git diff exceeded its deadline".to_string());
    }
    let mut cmd = crate::workspace::worktree::git_command();
    crate::workspace::worktree::git_subcommand(&mut cmd, args);
    cmd.current_dir(dir)
        // U-035: never block on a credential/helper prompt.
        .env("GIT_TERMINAL_PROMPT", "0");
    // U-035: bound the subprocess (run_with_timeout also nulls stdin + caps
    // stdout) so a hung git can't pin the diff viewer's blocking-pool task.
    let output =
        paneflow_process::run_with_timeout(cmd, deadline, GIT_STDOUT_CAP).map_err(|e| {
            format!(
                "git {} failed: {e}",
                args.first().copied().unwrap_or("command")
            )
        })?;
    git_stdout(args, output)
}

fn run_git_stdin_timed(
    dir: &Path,
    args: &[&str],
    stdin: &[u8],
    deadline: Duration,
    stdout_cap: u64,
) -> Result<Vec<u8>, String> {
    record_git_args(args);
    if deadline.is_zero() {
        return Err("git diff exceeded its deadline".to_string());
    }
    let mut cmd = crate::workspace::worktree::git_command();
    crate::workspace::worktree::git_subcommand(&mut cmd, args);
    cmd.current_dir(dir).env("GIT_TERMINAL_PROMPT", "0");
    let output = paneflow_process::run_with_timeout_stdin(cmd, stdin, deadline, stdout_cap)
        .map_err(|e| {
            format!(
                "git {} failed: {e}",
                args.first().copied().unwrap_or("command")
            )
        })?;
    git_stdout(args, output)
}

fn git_stdout(args: &[&str], output: paneflow_process::BoundedOutput) -> Result<Vec<u8>, String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        return Err(if msg.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or("command"))
        } else {
            msg.to_string()
        });
    }
    Ok(output.stdout)
}

/// Wall-clock budget for one [`compute_diff_against`] column build. Every git
/// subprocess inside that build consumes remaining time from this budget
/// instead of a fresh [`GIT_DEADLINE`], so 200 files cannot stack 200 timeouts.
struct GitBudget {
    deadline_at: Instant,
}

impl GitBudget {
    fn for_column() -> Self {
        Self {
            deadline_at: Instant::now() + GIT_DEADLINE,
        }
    }

    fn remaining(&self) -> Result<Duration, String> {
        let now = Instant::now();
        if now >= self.deadline_at {
            Err("git diff exceeded its deadline".to_string())
        } else {
            Ok(self.deadline_at - now)
        }
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
        run_git_timed(dir, args, self.remaining()?)
    }

    fn run_stdin(
        &self,
        dir: &Path,
        args: &[&str],
        stdin: &[u8],
        stdout_cap: u64,
    ) -> Result<Vec<u8>, String> {
        run_git_stdin_timed(dir, args, stdin, self.remaining()?, stdout_cap)
    }
}

fn deadline_exhausted(budget: &GitBudget, err: &str) -> bool {
    budget.remaining().is_err() || err.contains("exceeded its deadline")
}

/// Run a blocking working-tree filesystem call under `budget`. `std::fs` has
/// no deadline of its own, so the call runs on a helper thread and the caller
/// waits only for the budget's remaining time: an open or stat that never
/// returns (a dead NFS mount, a FIFO with no writer) surfaces the column's
/// deadline error instead of wedging the blocking-pool task. On timeout the
/// helper is abandoned; it exits on its own once the kernel releases it.
fn fs_within<T: Send + 'static>(
    budget: &GitBudget,
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let deadline = budget.remaining()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("paneflow-diff-fs".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| format!("{what} failed: {e}"))?;
    rx.recv_timeout(deadline).map_err(|e| match e {
        std::sync::mpsc::RecvTimeoutError::Timeout => "git diff exceeded its deadline".to_string(),
        std::sync::mpsc::RecvTimeoutError::Disconnected => format!("{what} failed"),
    })
}

/// Discover whether a workspace has a `.git` entry under a bounded filesystem
/// probe. Only absence means an ordinary folder: malformed, unreadable, and
/// dangling markers are left for Git to diagnose. Canonicalization keeps
/// symlinked subfolders attached to their actual repository.
pub(crate) fn has_repository_marker(worktree_dir: &Path) -> Result<bool, String> {
    has_repository_marker_within(&GitBudget::for_column(), worktree_dir)
}

fn has_repository_marker_within(budget: &GitBudget, worktree_dir: &Path) -> Result<bool, String> {
    let dir = worktree_dir.to_path_buf();
    fs_within(
        budget,
        "repository discovery",
        move || -> std::io::Result<bool> {
            let resolved = std::fs::canonicalize(dir)?;
            if !std::fs::metadata(&resolved)?.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "workspace path is not a directory",
                ));
            }
            for parent in resolved.ancestors() {
                match std::fs::symlink_metadata(parent.join(".git")) {
                    Ok(_) => return Ok(true),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(false)
        },
    )?
    .map_err(|err| format!("repository discovery failed: {err}"))
}

/// [`is_too_large`] charged against `budget` instead of blocking unbounded.
fn is_too_large_within(
    budget: &GitBudget,
    worktree_dir: &Path,
    rel_path: &str,
) -> Result<bool, String> {
    let dir = worktree_dir.to_path_buf();
    let rel = rel_path.to_string();
    fs_within(budget, "working-tree stat", move || {
        is_too_large(&dir, &rel)
    })
}

/// [`load_working_text`] charged against `budget` instead of blocking unbounded.
fn load_working_text_within(
    budget: &GitBudget,
    worktree_dir: &Path,
    rel_path: &str,
) -> Result<(String, bool), String> {
    let dir = worktree_dir.to_path_buf();
    let rel = rel_path.to_string();
    fs_within(budget, "working-tree read", move || {
        load_working_text(&dir, &rel)
    })
}

/// List all worktrees of the repository containing `repo_dir`. Live path:
/// [`list_repo_worktrees`] calls this for the US-013 Worktree-scope "include
/// worktrees not open as workspaces" enumeration.
pub fn list_worktrees(repo_dir: &Path) -> Result<Vec<Worktree>, String> {
    let out = run_git(repo_dir, &["worktree", "list", "--porcelain"])?;
    let text = String::from_utf8_lossy(&out);
    Ok(parse_worktrees_from_str(&text, Some(repo_dir)))
}

/// US-013 (prd-git-diff-mode-2026-Q3.md): every worktree of the repo as
/// `(path, short-branch)`, for the Worktree scope's "include worktrees not open
/// as workspaces" enumeration. Reuses the tested porcelain parser; returns an
/// empty vec on error (the caller falls back to the open-workspace set). Runs a
/// git subprocess, so callers invoke it off the GPUI main thread.
pub fn list_repo_worktrees(repo_dir: &Path) -> Vec<(PathBuf, String)> {
    let worktrees = match list_worktrees(repo_dir) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("git: failed to list repository worktrees: {e}");
            return Vec::new();
        }
    };
    review_worktree_entries(worktrees)
}

/// Review columns are checkout trees. A porcelain `bare` entry is the
/// administrative repository, not a work tree; `git diff` there exits 128.
fn review_worktree_entries(worktrees: Vec<Worktree>) -> Vec<(PathBuf, String)> {
    worktrees
        .into_iter()
        .filter(|w| !w.is_bare)
        .map(|w| {
            let branch = w
                .ref_name
                .as_deref()
                .map(short_ref)
                .unwrap_or_else(|| w.sha.chars().take(7).collect());
            (w.path, branch)
        })
        .collect()
}

/// Short branch name from a full ref (`refs/heads/develop` → `develop`).
fn short_ref(ref_name: &str) -> String {
    ref_name
        .strip_prefix("refs/heads/")
        .unwrap_or(ref_name)
        .to_string()
}

/// Whether `ref_name` resolves to a commit in `worktree_dir`. Public so the
/// multi-project shared-base seed can verify a base carried from another repo
/// actually exists here before using it (else it falls back to this repo's
/// default).
pub fn ref_exists(worktree_dir: &Path, ref_name: &str) -> bool {
    run_git(
        worktree_dir,
        &["rev-parse", "--verify", "--quiet", ref_name],
    )
    .is_ok()
}

/// Pick a sensible default base ref for `worktree_dir`: local `develop`, then
/// the remote default branch, then common local and remote defaults. Returns
/// `None` when none resolve. Every probe draws on one shared [`GitBudget`]
/// (issue #270), so the whole discovery cannot outlive a single
/// [`GIT_DEADLINE`] even if every `rev-parse` hangs.
pub fn default_base_ref(worktree_dir: &Path) -> Option<String> {
    default_base_ref_within(&GitBudget::for_column(), worktree_dir)
}

fn default_base_ref_within(budget: &GitBudget, worktree_dir: &Path) -> Option<String> {
    if ref_exists_within(budget, worktree_dir, "develop") {
        return Some("develop".to_string());
    }
    if let Some(remote_head) = default_origin_head(budget, worktree_dir) {
        return Some(remote_head);
    }
    for candidate in [
        "main",
        "master",
        "origin/develop",
        "origin/main",
        "origin/master",
    ] {
        if ref_exists_within(budget, worktree_dir, candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// [`ref_exists`] charged against `budget` instead of a fresh deadline.
fn ref_exists_within(budget: &GitBudget, worktree_dir: &Path, ref_name: &str) -> bool {
    budget
        .run(
            worktree_dir,
            &["rev-parse", "--verify", "--quiet", ref_name],
        )
        .is_ok()
}

fn default_origin_head(budget: &GitBudget, worktree_dir: &Path) -> Option<String> {
    let out = budget
        .run(
            worktree_dir,
            &["rev-parse", "--abbrev-ref", "refs/remotes/origin/HEAD"],
        )
        .ok()?;
    let branch = String::from_utf8_lossy(&out).trim().to_string();
    (!branch.is_empty()
        && branch != "origin/HEAD"
        && ref_exists_within(budget, worktree_dir, &branch))
    .then_some(branch)
}

/// Cheap content fingerprint of a worktree's diff inputs, used on diff-mode
/// re-entry to decide whether a column's already-loaded rows are still valid or
/// must be re-diffed (US-016 warm-resume). It captures the worktree `HEAD`, the
/// resolved `base_ref` commit, a bounded tracked-diff hash, and a bounded
/// untracked-input hash, so content edits are detected even when `git status`
/// would keep reporting the same path status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnFingerprint {
    head: String,
    base: String,
    diff_hash: u64,
    /// `None` when the untracked scan failed or timed out, so it can never
    /// equal the hash of a genuinely empty untracked set.
    untracked_hash: Option<u64>,
}

/// Compute a [`ColumnFingerprint`] for `worktree_dir` against `base_ref`. Runs
/// git subprocesses, so callers invoke it off the GPUI main thread (inside the
/// column build closure / a `smol::unblock`). Failed git reads yield empty or
/// zero components, and a failed or timed-out untracked scan yields `None`
/// (never the hash of a real empty set), so unstable repo states fail closed by
/// not matching a prior complete fingerprint.
pub fn column_fingerprint(worktree_dir: &Path, base_ref: &str) -> ColumnFingerprint {
    // Resolve the worktree's own root first, exactly as `load_column` does. The
    // seed `worktree_dir` may be a SUBDIRECTORY (the workspace opened after a
    // shell `cd`). Keying off the toplevel makes the fingerprint cover the same
    // scope the diff does.
    let budget = GitBudget::for_column();
    let toplevel = worktree_toplevel_within(&budget, worktree_dir);
    let merge_base = merge_base_within(&budget, &toplevel, base_ref).unwrap_or_default();
    column_fingerprint_within(&budget, &toplevel, base_ref, &merge_base)
}

/// [`column_fingerprint`] for an already-resolved `worktree_dir` toplevel and
/// `merge_base` (empty when unresolved), charged against `budget` (issue #309:
/// one column load draws every git call from one budget).
fn column_fingerprint_within(
    budget: &GitBudget,
    worktree_dir: &Path,
    base_ref: &str,
    merge_base: &str,
) -> ColumnFingerprint {
    let rev = |r: &str| {
        budget
            .run(worktree_dir, &["rev-parse", r])
            .ok()
            .map(|o| String::from_utf8_lossy(&o).trim().to_string())
            .unwrap_or_default()
    };
    let diff_hash = if merge_base.is_empty() {
        0
    } else {
        budget
            .run(
                worktree_dir,
                &["diff", "--binary", "--no-color", merge_base, "--"],
            )
            .ok()
            .map(|out| hash_bytes(&out))
            .unwrap_or(0)
    };
    ColumnFingerprint {
        head: rev("HEAD"),
        base: rev(base_ref),
        diff_hash,
        untracked_hash: budget
            .remaining()
            .ok()
            .and_then(|deadline| hash_untracked_inputs(worktree_dir, deadline)),
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;

    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// Hash the untracked inputs of `worktree_dir`, or `None` when the untracked
/// scan fails or exceeds `deadline`: an exhausted scan must not fingerprint
/// like an empty untracked set.
fn hash_untracked_inputs(worktree_dir: &Path, deadline: Duration) -> Option<u64> {
    use std::hash::{Hash as _, Hasher as _};
    use std::io::Read as _;

    let mut h = std::collections::hash_map::DefaultHasher::new();
    let (paths, truncated) =
        match list_untracked_limited_timed(worktree_dir, MAX_FILE_COUNT + 1, deadline) {
            Ok(listing) => listing,
            Err(e) => {
                log::warn!("git: fingerprint untracked listing failed: {e}");
                return None;
            }
        };
    truncated.hash(&mut h);
    for path in paths {
        path.hash(&mut h);
        if is_skipped_name(&path) || is_too_large(worktree_dir, &path) {
            "stub".hash(&mut h);
            continue;
        }
        let abs = worktree_dir.join(&path);
        match std::fs::symlink_metadata(&abs) {
            Ok(meta) if meta.file_type().is_symlink() => {
                "symlink".hash(&mut h);
                if let Ok(target) = std::fs::read_link(&abs) {
                    target.to_string_lossy().hash(&mut h);
                }
            }
            Ok(_) => match std::fs::File::open(&abs) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    let read_ok = file
                        .take(MAX_FILE_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .is_ok();
                    read_ok.hash(&mut h);
                    (bytes.len() as u64 > MAX_FILE_BYTES).hash(&mut h);
                    h.write(&bytes);
                }
                Err(err) => {
                    err.kind().hash(&mut h);
                }
            },
            Err(err) => {
                err.kind().hash(&mut h);
            }
        }
    }
    Some(h.finish())
}

/// Candidate base refs for the selector (US-013): local branches *and*
/// remote-tracking branches (`origin/develop`, `origin/main`, …), so the user
/// can diff a worktree against an upstream base, not just locals. The
/// `origin/HEAD` alias is filtered out. Sorted, deduplicated; empty on error
/// (the selector then just shows the resolved default).
pub fn list_base_ref_candidates(worktree_dir: &Path) -> Vec<String> {
    let out = match run_git(
        worktree_dir,
        &["branch", "-a", "--format=%(refname:short)", "--list"],
    ) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = String::from_utf8_lossy(&out)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        .collect();
    // Tags too, so a worktree can be diffed against a release tag (e.g.
    // `v0.3.6`), not just a branch. Arbitrary SHAs are handled separately by the
    // picker's free-text resolution (see `DiffView::resolve_and_set_base`).
    if let Ok(out) = run_git(worktree_dir, &["tag", "--list"]) {
        names.extend(
            String::from_utf8_lossy(&out)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
        );
    }
    names.sort();
    names.dedup();
    names
}

/// Resolve the working-tree root of `dir` via `git rev-parse --show-toplevel`.
///
/// `dir` is the column's seed path, which may be a *subdirectory* of the
/// worktree (the workspace was opened after a shell `cd`, or seeded from the
/// shell cwd). git resolves the toplevel from any subdir, so this returns the
/// worktree's own root - never the shared repo root (that would diff the main
/// checkout for every column). All file reads + git calls then key off this so
/// `worktree_dir.join(repo_root_relative_path)` lands on the right file.
/// Falls back to `dir` when git can't resolve (non-repo, error).
fn worktree_toplevel(dir: &Path) -> PathBuf {
    worktree_toplevel_within(&GitBudget::for_column(), dir)
}

/// [`worktree_toplevel`] charged against `budget` instead of a fresh deadline.
fn worktree_toplevel_within(budget: &GitBudget, dir: &Path) -> PathBuf {
    match budget.run(dir, &["rev-parse", "--show-toplevel"]) {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out).trim().to_string();
            if s.is_empty() {
                dir.to_path_buf()
            } else {
                PathBuf::from(s)
            }
        }
        Err(_) => dir.to_path_buf(),
    }
}

fn list_untracked_limited_timed(
    dir: &Path,
    limit: usize,
    deadline: Duration,
) -> Result<(Vec<String>, bool), String> {
    if limit == 0 {
        return Ok((Vec::new(), false));
    }
    let out = run_git_timed(
        dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        deadline,
    )?;
    let mut paths = Vec::new();
    let mut truncated = false;
    for raw_path in out.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        if paths.len() >= limit {
            truncated = true;
            break;
        }
        let Some(path) = decode_git_path(raw_path, "ls-files --others") else {
            continue;
        };
        paths.push(path);
    }
    Ok((paths, truncated))
}

/// Resolve the merge-base SHA between `HEAD` and `base_ref` in `worktree_dir`.
fn merge_base_within(
    budget: &GitBudget,
    worktree_dir: &Path,
    base_ref: &str,
) -> Result<String, String> {
    let out = budget.run(worktree_dir, &["merge-base", "HEAD", base_ref])?;
    let sha = String::from_utf8_lossy(&out).trim().to_string();
    if sha.is_empty() {
        return Err(format!("no common ancestor with '{base_ref}'"));
    }
    Ok(sha)
}

/// Normalize text the way git's diff stats do for text files: repository blobs
/// are LF-normalized, while a Windows worktree may contain CRLF due
/// `core.autocrlf`. The renderer should not turn that checkout detail into a
/// whole-file edit.
fn normalize_git_text(text: String) -> String {
    if text.as_bytes().contains(&b'\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text
    }
}

/// Bytes look textual if they contain no NUL and decode as UTF-8.
fn classify(bytes: Vec<u8>) -> (String, bool) {
    if bytes.contains(&0) {
        return (String::new(), true);
    }
    match String::from_utf8(bytes) {
        Ok(s) => (normalize_git_text(s), false),
        Err(_) => (String::new(), true),
    }
}

#[derive(Clone)]
struct BaseBlob {
    text: String,
    is_binary: bool,
    too_large: bool,
}

impl BaseBlob {
    fn empty() -> Self {
        Self {
            text: String::new(),
            is_binary: false,
            too_large: false,
        }
    }

    fn binary() -> Self {
        Self {
            text: String::new(),
            is_binary: true,
            too_large: false,
        }
    }

    fn too_large() -> Self {
        Self {
            text: String::new(),
            is_binary: true,
            too_large: true,
        }
    }
}

enum BatchCheck {
    Missing,
    Blob { size: u64 },
    Other,
}

fn cat_file_specs(merge_base: &str, lookups: &[String]) -> Vec<u8> {
    // `-Z` NUL-frames each `<merge-base>:<path>` spec so a tracked name
    // that contains a newline cannot split the batch into extra requests.
    let mut stdin = Vec::new();
    for path in lookups {
        stdin.extend_from_slice(merge_base.as_bytes());
        stdin.push(b':');
        stdin.extend_from_slice(path.as_bytes());
        stdin.push(0);
    }
    stdin
}

fn parse_batch_check_line(line: &[u8]) -> BatchCheck {
    if line.ends_with(b" missing") || line.ends_with(b" ambiguous") {
        return BatchCheck::Missing;
    }
    let Ok(header) = std::str::from_utf8(line) else {
        return BatchCheck::Other;
    };
    let mut parts = header.splitn(3, ' ');
    let _sha = parts.next();
    let kind = parts.next();
    let size = parts.next();
    match (kind, size) {
        (Some("blob"), Some(size)) => size
            .parse::<u64>()
            .map(|size| BatchCheck::Blob { size })
            .unwrap_or(BatchCheck::Other),
        _ => BatchCheck::Other,
    }
}

/// Parse `git cat-file --batch -Z` stdout for `count` requests. `None` is a
/// missing/ambiguous object; `Some(bytes)` is the raw blob (or other object)
/// payload. Headers and payloads are NUL-terminated.
fn parse_cat_file_batch(stdout: &[u8], count: usize) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut rest = stdout;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(nul) = rest.iter().position(|&b| b == 0) else {
            return Err("truncated git cat-file --batch header".to_string());
        };
        let header = &rest[..nul];
        rest = &rest[nul + 1..];
        if header.ends_with(b" missing") || header.ends_with(b" ambiguous") {
            records.push(None);
            continue;
        }
        let header_s = std::str::from_utf8(header)
            .map_err(|_| "non-utf8 git cat-file --batch header".to_string())?;
        let size = header_s
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or_else(|| format!("invalid git cat-file --batch header: {header_s}"))?;
        if rest.len() < size + 1 {
            return Err("truncated git cat-file --batch payload".to_string());
        }
        let bytes = rest[..size].to_vec();
        rest = &rest[size..];
        if rest.first() != Some(&0) {
            return Err("git cat-file --batch payload missing trailing NUL".to_string());
        }
        rest = &rest[1..];
        records.push(Some(bytes));
    }
    Ok(records)
}

fn stub_lookups(lookups: &[String]) -> HashMap<String, BaseBlob> {
    lookups
        .iter()
        .cloned()
        .map(|path| (path, BaseBlob::binary()))
        .collect()
}

/// Load merge-base blobs for `lookups` with at most two git processes
/// (`cat-file --batch-check` then `cat-file --batch`), under `budget`.
fn load_base_texts_batch(
    worktree_dir: &Path,
    merge_base: &str,
    lookups: &[String],
    budget: &GitBudget,
) -> Result<HashMap<String, BaseBlob>, String> {
    if lookups.is_empty() {
        return Ok(HashMap::new());
    }
    let stdin = cat_file_specs(merge_base, lookups);
    let check = match budget.run_stdin(
        worktree_dir,
        &["cat-file", "--batch-check", "-Z"],
        &stdin,
        GIT_STDOUT_CAP,
    ) {
        Ok(bytes) => bytes,
        Err(e) if deadline_exhausted(budget, &e) => return Err(e),
        Err(e) => {
            log::warn!("git: cat-file --batch-check failed: {e}");
            return Ok(stub_lookups(lookups));
        }
    };
    let lines: Vec<&[u8]> = check
        .split(|&b| b == 0)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() != lookups.len() {
        log::warn!(
            "git: cat-file --batch-check returned {} records for {} specs",
            lines.len(),
            lookups.len()
        );
        return Ok(stub_lookups(lookups));
    }

    let mut out = HashMap::new();
    let mut fetch: Vec<String> = Vec::new();
    for (path, line) in lookups.iter().zip(lines) {
        match parse_batch_check_line(line) {
            BatchCheck::Missing => {
                out.insert(path.clone(), BaseBlob::empty());
            }
            BatchCheck::Blob { size } if size > MAX_FILE_BYTES => {
                out.insert(path.clone(), BaseBlob::too_large());
            }
            BatchCheck::Blob { .. } => fetch.push(path.clone()),
            BatchCheck::Other => {
                out.insert(path.clone(), BaseBlob::binary());
            }
        }
    }
    if fetch.is_empty() {
        return Ok(out);
    }

    let stdin = cat_file_specs(merge_base, &fetch);
    let payload = match budget.run_stdin(
        worktree_dir,
        &["cat-file", "--batch", "-Z"],
        &stdin,
        GIT_BATCH_STDOUT_CAP,
    ) {
        Ok(bytes) => bytes,
        Err(e) if deadline_exhausted(budget, &e) => return Err(e),
        Err(e) => {
            log::warn!("git: cat-file --batch failed: {e}");
            for path in fetch {
                out.insert(path, BaseBlob::binary());
            }
            return Ok(out);
        }
    };
    let records = match parse_cat_file_batch(&payload, fetch.len()) {
        Ok(records) => records,
        Err(e) => {
            log::warn!("git: failed to parse cat-file --batch: {e}");
            for path in fetch {
                out.insert(path, BaseBlob::binary());
            }
            return Ok(out);
        }
    };
    for (path, record) in fetch.into_iter().zip(records) {
        match record {
            Some(bytes) => {
                let (text, is_binary) = classify(bytes);
                out.insert(
                    path,
                    BaseBlob {
                        text,
                        is_binary,
                        too_large: false,
                    },
                );
            }
            None => {
                out.insert(path, BaseBlob::empty());
            }
        }
    }
    Ok(out)
}

/// Read the working-tree text of `rel_path`. Returns `(text, is_binary)`; a
/// missing file (deleted in the working tree) yields empty text. Any other I/O
/// error (permission denied, device error) is logged and rendered as an
/// unreadable (binary) stub rather than masquerading as a deletion.
fn load_working_text(worktree_dir: &Path, rel_path: &str) -> (String, bool) {
    use std::io::Read as _;

    let path = worktree_dir.join(rel_path);
    // U-041: lstat first. A tracked/untracked symlink in a crafted repo could
    // point outside the worktree; `fs::read` would dereference it and pull an
    // out-of-tree file into `new_text`. Render the LINK TARGET instead of
    // following it - this also matches git's own symlink-blob semantics (the
    // base side via `git cat-file` returns the target path, not the pointee's
    // content), so an unchanged symlink produces no spurious diff.
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(&path)
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            (target, false)
        }
        Ok(_) => match std::fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                match file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
                    Ok(_) if bytes.len() as u64 > MAX_FILE_BYTES => (String::new(), true),
                    Ok(_) => classify(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
                    Err(e) => {
                        log::warn!("git: failed to read working-tree file {rel_path}: {e}");
                        (String::new(), true)
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
            Err(e) => {
                log::warn!("git: failed to read working-tree file {rel_path}: {e}");
                (String::new(), true)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(e) => {
            log::warn!("git: failed to lstat working-tree file {rel_path}: {e}");
            (String::new(), true)
        }
    }
}

/// Parse `git diff --name-status -z <merge_base>` (NUL-delimited records).
/// Each record is a status field followed by its path(s); renames/copies carry
/// a source and a destination path - we key on the destination.
fn parse_name_status_z(stdout: &[u8]) -> Vec<(FileChange, String, Option<String>)> {
    let mut fields = stdout.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let Some(status) = fields.next() {
        let code = status.first().copied().unwrap_or(b'M') as char;
        // Rename/copy: status is followed by <src>\0<dst>; key on dst, keep src.
        let (path, old) = if matches!(code, 'R' | 'C') {
            let Some(src) = fields.next() else {
                break;
            };
            let Some(dst) = fields.next() else {
                break;
            };
            let Some(src) = decode_git_path(src, "diff --name-status source") else {
                continue;
            };
            let Some(dst) = decode_git_path(dst, "diff --name-status destination") else {
                continue;
            };
            (dst, Some(src))
        } else {
            let Some(path) = fields.next() else {
                break;
            };
            let Some(path) = decode_git_path(path, "diff --name-status") else {
                continue;
            };
            (path, None)
        };
        let change = match code {
            'A' => FileChange::Added,
            'D' => FileChange::Deleted,
            'R' => FileChange::Renamed,
            _ => FileChange::Modified, // M, C, T → modified content
        };
        out.push((change, path, old));
    }
    out
}

fn parse_numstat_z(stdout: &[u8]) -> HashMap<String, FileDiffStat> {
    let mut out = HashMap::new();
    let mut fields = stdout.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(record) = fields.next() {
        let Some((added, removed, path)) = split_numstat_record(record) else {
            continue;
        };
        let path = if path.is_empty() {
            let _old_path = fields.next();
            let Some(new_path) = fields.next() else {
                break;
            };
            new_path
        } else {
            path
        };
        let Some(path) = decode_git_path(path, "diff --numstat") else {
            continue;
        };
        let stat = out.entry(path).or_insert(FileDiffStat {
            added: 0,
            removed: 0,
        });
        stat.added = stat.added.saturating_add(parse_numstat_count(added));
        stat.removed = stat.removed.saturating_add(parse_numstat_count(removed));
    }
    out
}

fn decode_git_path(path: &[u8], source: &str) -> Option<String> {
    match std::str::from_utf8(path) {
        Ok(path) if !path.is_empty() => Some(path.to_string()),
        Ok(_) => None,
        Err(_) => {
            log::warn!("git: skipping non-UTF-8 path from {source}");
            None
        }
    }
}

fn split_numstat_record(record: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let first_tab = record.iter().position(|&b| b == b'\t')?;
    let rest = &record[first_tab + 1..];
    let second_tab = rest.iter().position(|&b| b == b'\t')?;
    Some((
        &record[..first_tab],
        &rest[..second_tab],
        &rest[second_tab + 1..],
    ))
}

fn parse_numstat_count(raw: &[u8]) -> u32 {
    if raw == b"-" {
        return 0;
    }
    std::str::from_utf8(raw)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
}

/// Files above this size (either side, bytes) are shown as a stub instead of
/// being loaded + diffed + highlighted. Without this a single huge generated
/// file (minified bundle, vendored blob) loads megabytes into RAM, runs
/// `imara-diff` + a full syntect pass over it, and - across N columns - OOMs the
/// process. 512 KiB comfortably covers hand-written source.
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Hard cap on changed files diffed per worktree. A 1000-file refactor would
/// otherwise load every file into RAM (×N columns); beyond this the column
/// stops and shows a truncation row.
const MAX_FILE_COUNT: usize = 200;

/// Lockfiles and other large, low-signal generated files - never worth a
/// line-by-line diff and a prime OOM trigger (`Cargo.lock` alone is ~12k lines).
const SKIP_FILENAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "bun.lockb",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "poetry.lock",
    "Gemfile.lock",
];

/// Whether `path`'s final component is a known generated/lockfile name. Public
/// so the file watcher ([`super::view`]) shares this single source of truth and
/// cannot drift from the diff-time skip list.
pub fn is_skipped_name(path: &str) -> bool {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| SKIP_FILENAMES.contains(&n))
}

/// Working-tree size of `rel_path` exceeds [`MAX_FILE_BYTES`]. Fast pre-load
/// guard for the common added/modified case; a metadata miss (deleted file)
/// reads as not-too-large. The base side - and the metadata-miss case - is
/// caught by the post-load length check in [`load_column`].
fn is_too_large(worktree_dir: &Path, rel_path: &str) -> bool {
    std::fs::metadata(worktree_dir.join(rel_path))
        .map(|m| m.len() > MAX_FILE_BYTES)
        .unwrap_or(false)
}

/// A "not shown" stub: rendered as a single notice row, never loaded/diffed.
fn stub_file(path: String, change: FileChange) -> FileDiff {
    FileDiff {
        path,
        change,
        old_path: None,
        base_text: String::new(),
        new_text: String::new(),
        hunks: Vec::new(),
        is_binary: true,
    }
}

/// Everything one Review column build reads from git, produced by
/// [`load_column`] under ONE [`GitBudget`].
pub struct ColumnLoad {
    /// US-016 warm-resume fingerprint, snapshotted BEFORE the tree is read.
    pub fingerprint: ColumnFingerprint,
    /// The diff of the worktree against `base_ref`:
    /// `merge-base(HEAD, base_ref)..working-tree`, including uncommitted
    /// changes. `error` is set (rather than panicking) when the base ref or
    /// merge base cannot be resolved. Oversized / lockfile / over-count files
    /// are shown as stubs rather than loaded, bounding peak RAM.
    pub diff: WorktreeDiff,
    /// Per-file Git-native diffstat for the same semantic as `diff`, plus
    /// untracked files. Used for Review's sidebar/global counters so they match
    /// `git diff --numstat` instead of drifting with renderer hunk details.
    /// `Err` (never an empty map) when a git read fails or times out.
    pub file_stats: Result<HashMap<String, FileDiffStat>, String>,
}

/// Load one Review column: fingerprint, diff, and file stats of `worktree_dir`
/// against `base_ref`. Issue #309: every git call draws on ONE [`GitBudget`],
/// and the worktree toplevel + merge-base are resolved once and shared, so a
/// wedged git fails the column inside a single [`GIT_DEADLINE`] instead of
/// stacking one per pipeline.
///
/// Runs entirely via subprocess; safe to call off the main thread.
pub fn load_column(worktree_dir: &Path, base_ref: &str) -> ColumnLoad {
    load_column_within(&GitBudget::for_column(), worktree_dir, base_ref)
}

/// [`load_column`] charged against `budget`.
fn load_column_within(budget: &GitBudget, worktree_dir: &Path, base_ref: &str) -> ColumnLoad {
    // Resolve the worktree's own root once: the seed path may be a subdirectory
    // (shell `cd`), which would make `worktree_dir.join(rel_path)` miss every
    // file. Everything below - merge-base, name-status, file reads - keys off
    // this so the diff is correct regardless of the seed path's depth.
    let toplevel = worktree_toplevel_within(budget, worktree_dir);
    let worktree_dir = toplevel.as_path();
    log::debug!(
        "git: load_column dir={} base={base_ref}",
        worktree_dir.display()
    );
    let merge_base = merge_base_within(budget, worktree_dir, base_ref);
    match &merge_base {
        Ok(mb) => log::debug!("git: merge_base={mb}"),
        Err(e) => log::warn!("git: merge_base failed (base={base_ref}): {e}"),
    }
    // US-016: snapshot the fingerprint BEFORE reading the tree, so a commit
    // landing mid-build makes the stored fingerprint LAG the rows.
    let fingerprint = column_fingerprint_within(
        budget,
        worktree_dir,
        base_ref,
        merge_base.as_deref().unwrap_or_default(),
    );
    let diff = match &merge_base {
        Ok(mb) => compute_diff_against_within(budget, worktree_dir, mb),
        Err(e) => WorktreeDiff {
            files: Vec::new(),
            error: Some(e.clone()),
        },
    };
    let file_stats = merge_base
        .as_deref()
        .map_err(Clone::clone)
        .and_then(|mb| compute_file_stats_against_within(budget, worktree_dir, mb));
    ColumnLoad {
        fingerprint,
        diff,
        file_stats,
    }
}

/// Git's well-known empty-tree object hash. Diffing against it (used when `HEAD`
/// is unborn - a repo with no commits yet) shows every tracked file as a pure
/// addition, so a first changeset still renders in the Agents dock.
const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Compute the diff of `worktree_dir` against `HEAD`: the working tree vs the
/// last commit (staged + unstaged tracked changes) plus untracked files - the
/// "what did the agent just touch" semantic used by the diff dock
/// ([`crate::app::diff_dock`]). When `HEAD` is unborn, everything is diffed
/// against the empty tree. Reuses [`compute_diff_against`], so the lockfile /
/// size / count / binary guards are identical to [`load_column`].
///
/// Runs entirely via subprocess; safe to call off the main thread.
pub fn compute_head_diff(worktree_dir: &Path) -> WorktreeDiff {
    let toplevel = worktree_toplevel(worktree_dir);
    let worktree_dir = toplevel.as_path();
    log::debug!("git: compute_head_diff dir={}", worktree_dir.display());
    // HEAD's commit SHA, or the empty tree when HEAD is unborn (no commits yet).
    let base = match run_git(worktree_dir, &["rev-parse", "--verify", "HEAD"]) {
        Ok(out) => String::from_utf8_lossy(&out).trim().to_string(),
        Err(_) => EMPTY_TREE_SHA.to_string(),
    };
    compute_diff_against(worktree_dir, &base)
}

/// Shared core of [`load_column`] and [`compute_head_diff`]: diff the
/// working tree against the already-resolved commit-ish `base`. `worktree_dir`
/// must already be the worktree toplevel (both callers resolve it first).
/// Oversized / lockfile / over-count files are shown as stubs rather than
/// loaded, bounding peak RAM.
fn compute_diff_against(worktree_dir: &Path, base: &str) -> WorktreeDiff {
    compute_diff_against_within(&GitBudget::for_column(), worktree_dir, base)
}

/// [`compute_diff_against`] charged against `budget`; an exhausted budget fails
/// closed with `WorktreeDiff::error` set before any git subprocess spawns.
fn compute_diff_against_within(
    budget: &GitBudget,
    worktree_dir: &Path,
    base: &str,
) -> WorktreeDiff {
    let name_status = match budget.run(
        worktree_dir,
        // `-M` enables rename detection so a moved file reads as one `R` record
        // (old → new) instead of a delete + add pair - de-noises task-branch diffs.
        &["diff", "--name-status", "-M", "-z", "--no-color", base],
    ) {
        Ok(out) => out,
        Err(e) => {
            log::warn!("git: name-status failed: {e}");
            return WorktreeDiff {
                files: Vec::new(),
                error: Some(e),
            };
        }
    };

    let mut changes = parse_name_status_z(&name_status);
    let mut truncated = changes.len() > MAX_FILE_COUNT;
    if changes.len() > MAX_FILE_COUNT + 1 {
        changes.truncate(MAX_FILE_COUNT + 1);
    }
    // Tracked changes (above) miss untracked new files; append them as Added so
    // a freshly-created file on the branch shows up (loaded from the working
    // tree, empty base → rendered as a pure addition).
    if changes.len() <= MAX_FILE_COUNT {
        let remaining = MAX_FILE_COUNT + 1 - changes.len();
        let deadline = match budget.remaining() {
            Ok(deadline) => deadline,
            Err(e) => {
                return WorktreeDiff {
                    files: Vec::new(),
                    error: Some(e),
                };
            }
        };
        let (untracked, untracked_truncated) =
            match list_untracked_limited_timed(worktree_dir, remaining, deadline) {
                Ok(listing) => listing,
                Err(e) => {
                    log::warn!("git: untracked listing failed: {e}");
                    return WorktreeDiff {
                        files: Vec::new(),
                        error: Some(e),
                    };
                }
            };
        truncated |= untracked_truncated;
        for path in untracked {
            changes.push((FileChange::Added, path, None));
        }
    }
    log::debug!("git: {} changed files", changes.len());

    let mut lookups = Vec::new();
    for (change, path, old_path) in &changes {
        if is_skipped_name(path) {
            continue;
        }
        match is_too_large_within(budget, worktree_dir, path) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                return WorktreeDiff {
                    files: Vec::new(),
                    error: Some(e),
                };
            }
        }
        if *change == FileChange::Added {
            continue;
        }
        let lookup = match (*change, old_path) {
            (FileChange::Renamed, Some(src)) => src.clone(),
            _ => path.clone(),
        };
        lookups.push(lookup);
    }
    lookups.sort();
    lookups.dedup();
    let blobs = match load_base_texts_batch(worktree_dir, base, &lookups, budget) {
        Ok(blobs) => blobs,
        Err(e) => {
            log::warn!("git: batched base load failed: {e}");
            return WorktreeDiff {
                files: Vec::new(),
                error: Some(e),
            };
        }
    };

    let mut files = Vec::new();
    for (change, path, old_path) in changes {
        if files.len() >= MAX_FILE_COUNT {
            truncated = true;
            break;
        }
        if budget.remaining().is_err() {
            return WorktreeDiff {
                files: Vec::new(),
                error: Some("git diff exceeded its deadline".to_string()),
            };
        }
        // Skip lockfiles and oversized files: emit a stub, never load/diff/
        // highlight them. This is the primary OOM guard.
        let too_large = if is_skipped_name(&path) {
            true
        } else {
            match is_too_large_within(budget, worktree_dir, &path) {
                Ok(too_large) => too_large,
                Err(e) => {
                    return WorktreeDiff {
                        files: Vec::new(),
                        error: Some(e),
                    };
                }
            }
        };
        if too_large {
            log::debug!("git: skip (lockfile/large) {path}");
            files.push(stub_file(path, change));
            continue;
        }
        log::debug!("git: load {path}");
        // For a rename, the base text lives at the pre-rename path.
        let base_lookup = match (change, &old_path) {
            (FileChange::Renamed, Some(src)) => src.as_str(),
            _ => path.as_str(),
        };
        let (base_text, base_bin) = match change {
            FileChange::Added => (String::new(), false),
            _ => match blobs.get(base_lookup) {
                Some(blob) if blob.too_large => {
                    log::debug!("git: skip (oversized base blob) {path}");
                    files.push(stub_file(path, change));
                    continue;
                }
                Some(blob) => (blob.text.clone(), blob.is_binary),
                None => (String::new(), true),
            },
        };
        let (new_text, new_bin) = match change {
            FileChange::Deleted => (String::new(), false),
            _ => match load_working_text_within(budget, worktree_dir, &path) {
                Ok(loaded) => loaded,
                Err(e) => {
                    return WorktreeDiff {
                        files: Vec::new(),
                        error: Some(e),
                    };
                }
            },
        };
        // Post-load size guard. `is_too_large` only sees the working-tree side
        // via metadata, so a file that is huge at the merge-base but small or
        // deleted now (a bulk rewrite / deletion commit) would otherwise load
        // its full base blob into a retained `FileDiff` - unbounded across the N
        // columns. Stub it instead, bounding retained RAM symmetrically.
        if base_text.len() as u64 > MAX_FILE_BYTES || new_text.len() as u64 > MAX_FILE_BYTES {
            log::debug!("git: skip (oversized post-load) {path}");
            files.push(stub_file(path, change));
            continue;
        }
        let is_binary = base_bin || new_bin;
        let hunks = if is_binary {
            Vec::new()
        } else {
            compute_hunks(&base_text, &new_text)
        };
        files.push(FileDiff {
            path,
            change,
            old_path,
            base_text,
            new_text,
            hunks,
            is_binary,
        });
    }

    if truncated {
        // Visible notice, not a silent cap (NFR). Rendered as a stub row.
        files.push(stub_file(
            format!("… more files not shown (truncated at {MAX_FILE_COUNT})"),
            FileChange::Modified,
        ));
    }

    WorktreeDiff { files, error: None }
}

/// Per-file diffstat of the working tree against `base`, charged against
/// `budget`. A failed or timed-out numstat or untracked scan is an `Err`,
/// never a partial or empty map that reads as "no changes".
fn compute_file_stats_against_within(
    budget: &GitBudget,
    worktree_dir: &Path,
    base: &str,
) -> Result<HashMap<String, FileDiffStat>, String> {
    let mut stats = budget
        .run(
            worktree_dir,
            &["diff", "--numstat", "-z", "--no-color", base, "--"],
        )
        .map(|out| parse_numstat_z(&out))?;

    let remaining = MAX_FILE_COUNT.saturating_sub(stats.len());
    if remaining == 0 {
        return Ok(stats);
    }

    let (untracked, truncated) =
        list_untracked_limited_timed(worktree_dir, remaining, budget.remaining()?)?;
    if truncated {
        log::debug!("git: untracked file stats truncated at {remaining}");
    }
    for path in untracked {
        if is_skipped_name(&path) || is_too_large(worktree_dir, &path) {
            stats.insert(
                path,
                FileDiffStat {
                    added: 0,
                    removed: 0,
                },
            );
            continue;
        }
        let (text, is_binary) = load_working_text(worktree_dir, &path);
        let added = if is_binary {
            0
        } else {
            u32::try_from(text.lines().count()).unwrap_or(u32::MAX)
        };
        stats.insert(path, FileDiffStat { added, removed: 0 });
    }

    Ok(stats)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

    #[test]
    fn repository_discovery_fails_closed_on_exhausted_budget() {
        let dir = tempfile::tempdir().unwrap();
        let budget = GitBudget {
            deadline_at: Instant::now(),
        };
        let err = has_repository_marker_within(&budget, dir.path()).unwrap_err();
        assert!(err.contains("deadline"), "got {err}");
    }

    #[test]
    fn filesystem_probe_returns_before_a_blocked_operation_finishes() {
        let budget = GitBudget {
            deadline_at: Instant::now() + Duration::from_millis(50),
        };
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let result = fs_within(&budget, "repository discovery", move || {
            blocked.recv_timeout(Duration::from_secs(5))
        });
        // Release the helper thread even if the assertion below fails.
        drop(release);
        let err = result.unwrap_err();
        assert!(err.contains("deadline"), "got {err}");
    }

    struct TestLogger {
        records: Mutex<Vec<(log::Level, String)>>,
    }

    impl log::Log for TestLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Warn
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                self.records
                    .lock()
                    .expect("test logger lock poisoned")
                    .push((record.level(), record.args().to_string()));
            }
        }

        fn flush(&self) {}
    }

    static TEST_LOGGER: TestLogger = TestLogger {
        records: Mutex::new(Vec::new()),
    };

    pub(crate) fn capture_logs() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            log::set_logger(&TEST_LOGGER).expect("test logger should initialize once");
            log::set_max_level(log::LevelFilter::Warn);
        });
    }

    pub(crate) fn captured_logs_contain(needle: &str) -> bool {
        TEST_LOGGER
            .records
            .lock()
            .expect("test logger lock poisoned")
            .iter()
            .any(|(_, message)| message.contains(needle))
    }

    #[test]
    fn list_repo_worktrees_warns_when_git_fails() {
        capture_logs();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        assert!(list_repo_worktrees(&missing).is_empty());

        let records = TEST_LOGGER
            .records
            .lock()
            .expect("test logger lock poisoned");
        assert!(records.iter().any(|(level, message)| {
            *level == log::Level::Warn
                && message.contains("git: failed to list repository worktrees")
                && message.contains("git worktree failed")
        }));
    }

    #[test]
    fn parse_worktrees_basic() {
        let raw = "worktree /repo/main\nHEAD abc123\nbranch refs/heads/develop\n\n\
                   worktree /repo/wt-a\nHEAD def456\nbranch refs/heads/feature-a\n";
        let wts = parse_worktrees_from_str(raw, Some(Path::new("/repo/main")));
        assert_eq!(wts.len(), 2);
        assert!(wts[0].is_main);
        assert_eq!(wts[0].ref_name.as_deref(), Some("refs/heads/develop"));
        assert_eq!(wts[0].sha, "abc123");
        assert!(!wts[1].is_main);
        assert_eq!(wts[1].path, PathBuf::from("/repo/wt-a"));
    }

    #[test]
    fn parse_worktrees_detached_and_bare() {
        let raw = "worktree /repo/bare\nbare\n\n\
                   worktree /repo/det\nHEAD aaa111\ndetached\n";
        let wts = parse_worktrees_from_str(raw, None);
        // A `worktree ` line is enough: HEAD-less/bare entries stay so Launch
        // Pad and Review list the same checkout set.
        assert_eq!(wts.len(), 2);
        assert_eq!(wts[0].path, PathBuf::from("/repo/bare"));
        assert!(wts[0].is_bare);
        assert_eq!(wts[0].sha, "");
        assert_eq!(wts[0].ref_name, None);
        assert_eq!(wts[1].path, PathBuf::from("/repo/det"));
        assert!(!wts[1].is_bare);
        assert_eq!(wts[1].ref_name, None);
        assert_eq!(wts[1].sha, "aaa111");
    }

    #[test]
    fn porcelain_parsers_agree_on_headless_and_detached() {
        let raw = "worktree /repo/bare\nbare\n\n\
                   worktree /repo/det\nHEAD aaa111\ndetached\n\n\
                   worktree /repo/main\nHEAD abc123\nbranch refs/heads/main\n";
        let workspace_paths: Vec<_> = crate::workspace::worktree::parse_worktree_porcelain(raw)
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        let diff_paths: Vec<_> = parse_worktrees_from_str(raw, None)
            .into_iter()
            .map(|worktree| worktree.path)
            .collect();
        assert_eq!(
            workspace_paths, diff_paths,
            "Launch Pad and Review must list the same worktrees"
        );
        assert_eq!(
            workspace_paths,
            vec![
                PathBuf::from("/repo/bare"),
                PathBuf::from("/repo/det"),
                PathBuf::from("/repo/main"),
            ]
        );
    }

    #[test]
    fn review_worktree_entries_drop_bare_admin_repos() {
        let raw = "worktree /repo/bare\nbare\n\n\
                   worktree /repo/det\nHEAD aaa111\ndetached\n\n\
                   worktree /repo/main\nHEAD abc123\nbranch refs/heads/main\n";
        let columns = review_worktree_entries(parse_worktrees_from_str(raw, None));
        assert_eq!(
            columns
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("/repo/det"), PathBuf::from("/repo/main")]
        );
    }

    #[test]
    fn name_status_z_parsing() {
        // status\0path\0 records; includes an addition, a deletion, a rename.
        let raw = b"M\0src/main.rs\0A\0src/new.rs\0D\0old.rs\0R100\0from.rs\0to.rs\0";
        let parsed = parse_name_status_z(raw);
        assert_eq!(parsed.len(), 4);
        assert_eq!(
            parsed[0],
            (FileChange::Modified, "src/main.rs".to_string(), None)
        );
        assert_eq!(
            parsed[1],
            (FileChange::Added, "src/new.rs".to_string(), None)
        );
        assert_eq!(parsed[2], (FileChange::Deleted, "old.rs".to_string(), None));
        // Rename keys on destination, keeping the source as old_path.
        assert_eq!(
            parsed[3],
            (
                FileChange::Renamed,
                "to.rs".to_string(),
                Some("from.rs".to_string())
            )
        );
    }

    #[test]
    fn name_status_z_skips_non_utf8_paths() {
        let raw = b"M\0src/\xff.rs\0A\0src/ok.rs\0";
        let parsed = parse_name_status_z(raw);
        assert_eq!(
            parsed,
            vec![(FileChange::Added, "src/ok.rs".to_string(), None)]
        );
    }

    #[test]
    fn classify_binary_and_text() {
        assert_eq!(
            classify(b"hello\n".to_vec()),
            ("hello\n".to_string(), false)
        );
        assert_eq!(
            classify(b"hello\r\nworld\r\n".to_vec()),
            ("hello\nworld\n".to_string(), false)
        );
        let (_, bin) = classify(vec![0x00, 0x01, 0x02]);
        assert!(bin);
    }

    #[test]
    fn parse_cat_file_batch_missing_empty_and_blob() {
        let mut stdout = Vec::new();
        stdout.extend_from_slice(b"HEAD:gone.rs missing\0");
        stdout.extend_from_slice(b"abc blob 0\0\0");
        stdout.extend_from_slice(b"def blob 5\0hello\0");
        let parsed = parse_cat_file_batch(&stdout, 3).unwrap();
        assert_eq!(parsed[0], None);
        assert_eq!(parsed[1], Some(Vec::new()));
        assert_eq!(parsed[2], Some(b"hello".to_vec()));
    }

    #[test]
    fn cat_file_specs_nul_frames_paths_that_contain_newlines() {
        let specs = cat_file_specs("abc123", &["foo\nbar.rs".into(), "ok.rs".into()]);
        let records: Vec<&[u8]> = specs.split(|&b| b == 0).filter(|r| !r.is_empty()).collect();
        assert_eq!(
            records,
            [b"abc123:foo\nbar.rs".as_slice(), b"abc123:ok.rs".as_slice()]
        );
    }

    #[test]
    fn load_working_text_caps_bytes() {
        // Call load_working_text directly so the is_too_large metadata
        // pre-check cannot hide an unbounded read (metadata miss, TOCTOU
        // grow, or a file already over the cap).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let oversized_len = MAX_FILE_BYTES as usize + 2;
        std::fs::write(root.join("huge.txt"), vec![b'a'; oversized_len]).unwrap();

        let (text, is_binary) = load_working_text(root, "huge.txt");
        assert!(
            text.len() as u64 <= MAX_FILE_BYTES + 1,
            "load_working_text retained {} bytes from an oversized working-tree file",
            text.len()
        );
        assert!(
            text.is_empty() && is_binary,
            "oversized working-tree content should stub as binary without keeping the buffer"
        );
    }

    #[test]
    fn load_working_text_within_fails_instead_of_hanging_on_blocking_path() {
        use std::os::unix::ffi::OsStrExt as _;

        // A FIFO with no writer blocks `File::open` forever - the shape of a
        // working-tree file on a dead NFS mount. The column budget, not the
        // kernel, has to bound the read.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let fifo = std::ffi::CString::new(root.join("stuck").as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path for the call's duration.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let budget = GitBudget {
            deadline_at: Instant::now() + Duration::from_millis(200),
        };
        // Watchdog: a regression here hangs forever, so wait on a channel
        // instead of joining the worker.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(load_working_text_within(&budget, &root, "stuck"));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("load_working_text_within hung on a blocking working-tree path");
        assert_eq!(result, Err("git diff exceeded its deadline".to_string()));
    }

    #[test]
    fn working_tree_reads_fail_closed_once_budget_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("small.txt"), b"hello\n").unwrap();
        let live = GitBudget::for_column();
        assert_eq!(is_too_large_within(&live, root, "small.txt"), Ok(false));
        assert_eq!(
            load_working_text_within(&live, root, "small.txt"),
            Ok(("hello\n".to_string(), false))
        );
        let spent = GitBudget {
            deadline_at: Instant::now(),
        };
        assert!(is_too_large_within(&spent, root, "small.txt").is_err());
        assert!(load_working_text_within(&spent, root, "small.txt").is_err());
    }

    #[test]
    fn numstat_z_parsing() {
        let raw = b"3\t1\tsrc/main.rs\0-\t-\timage.png\0";
        let parsed = parse_numstat_z(raw);
        assert_eq!(
            parsed.get("src/main.rs"),
            Some(&FileDiffStat {
                added: 3,
                removed: 1
            })
        );
        assert_eq!(
            parsed.get("image.png"),
            Some(&FileDiffStat {
                added: 0,
                removed: 0
            })
        );
    }

    #[test]
    fn numstat_z_renames_key_on_destination() {
        let raw = b"2\t1\t\0src/old.rs\0src/new.rs\0";
        let parsed = parse_numstat_z(raw);
        assert_eq!(
            parsed.get("src/new.rs"),
            Some(&FileDiffStat {
                added: 2,
                removed: 1
            })
        );
        assert!(!parsed.contains_key("src/old.rs"));
    }

    #[test]
    fn numstat_z_skips_non_utf8_paths() {
        let raw = b"1\t0\tsrc/\xff.rs\x002\t0\tsrc/ok.rs\0";
        let parsed = parse_numstat_z(raw);
        assert_eq!(
            parsed.get("src/ok.rs"),
            Some(&FileDiffStat {
                added: 2,
                removed: 0
            })
        );
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn worktree_file_stats_count_tracked_and_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        assert!(test_git(root, &["config", "core.autocrlf", "false"]));
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        assert!(test_git(root, &["add", "tracked.txt"]));
        assert!(test_git(
            root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "-m",
                "init",
            ],
        ));

        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "alpha\nbeta\n").unwrap();

        let stats = load_column(root, "HEAD").file_stats.expect("file stats");
        assert_eq!(
            stats.get("tracked.txt"),
            Some(&FileDiffStat {
                added: 1,
                removed: 0
            })
        );
        assert_eq!(
            stats.get("untracked.txt"),
            Some(&FileDiffStat {
                added: 2,
                removed: 0
            })
        );
    }

    #[test]
    fn list_untracked_limited_timed_propagates_deadline() {
        // No `git init`: a zero deadline must fail closed before any git
        // subprocess is spawned, so a bare temp dir is enough.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let err = list_untracked_limited_timed(root, 8, Duration::from_secs(0))
            .expect_err("zero deadline must fail closed");
        assert!(
            err.contains("deadline") || err.contains("timed") || err.contains("timeout"),
            "untracked listing must surface the timeout, got {err}"
        );
    }

    #[test]
    fn default_base_ref_probes_share_one_deadline() {
        // Issue #270: every probe in default-base discovery must draw on one
        // GitBudget, not a fresh GIT_DEADLINE each, so a wedged git cannot
        // stack eight 30 s timeouts before the column fails.
        let src = include_str!("git.rs");
        let body = src
            .split("pub fn default_base_ref(")
            .nth(1)
            .and_then(|rest| rest.split("/// Cheap content fingerprint").next())
            .expect("default_base_ref + default_origin_head bodies");
        assert!(
            !body.contains("ref_exists(") && !body.contains("run_git("),
            "default_base_ref must not probe refs with a fresh per-call deadline: {body}"
        );
        assert!(
            body.contains("GitBudget::for_column()"),
            "default_base_ref must take exactly one GitBudget: {body}"
        );
    }

    #[test]
    fn default_base_ref_stops_probing_once_budget_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !test_git(root, &["init", "-b", "develop"]) {
            return;
        }
        assert!(test_git(
            root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        ));
        // A live budget resolves `develop` as before.
        assert_eq!(
            default_base_ref_within(&GitBudget::for_column(), root).as_deref(),
            Some("develop")
        );
        // An exhausted budget fails closed: no probe may spawn git at all,
        // let alone one per candidate with its own 30 s deadline.
        let spent = GitBudget {
            deadline_at: Instant::now(),
        };
        let _ = take_git_commands();
        assert_eq!(default_base_ref_within(&spent, root), None);
        assert!(
            take_git_commands().is_empty(),
            "no git probe may run after the shared deadline has passed"
        );
    }

    #[test]
    fn compute_diff_against_surfaces_untracked_scan_timeout() {
        let src = include_str!("git.rs");
        let body = src
            .split("fn compute_diff_against(")
            .nth(1)
            .and_then(|rest| rest.split("fn compute_file_stats_against_within(").next())
            .expect("compute_diff_against body");
        assert!(
            body.contains("list_untracked_limited_timed")
                && body.contains("untracked listing failed")
                && body.contains("error: Some(e)"),
            "an exhausted untracked scan must become WorktreeDiff.error, not an empty listing: {body}"
        );
    }

    #[test]
    fn list_untracked_limited_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        std::fs::write(root.join("c.txt"), "c\n").unwrap();

        let (paths, truncated) =
            list_untracked_limited_timed(root, 2, GIT_DEADLINE).expect("untracked listing");
        assert_eq!(paths.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn compute_diff_against_fails_closed_on_exhausted_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        assert!(test_git(
            root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        ));
        std::fs::write(root.join("ghost.txt"), "x\n").unwrap();
        // A live budget lists the untracked file.
        let live = compute_diff_against_within(&GitBudget::for_column(), root, "HEAD");
        assert_eq!(live.error, None);
        assert!(live.files.iter().any(|f| f.path == "ghost.txt"));
        // An exhausted budget is an error on the return value, not an empty
        // listing, and spawns no git at all.
        let spent = GitBudget {
            deadline_at: Instant::now(),
        };
        let _ = take_git_commands();
        let diff = compute_diff_against_within(&spent, root, "HEAD");
        assert!(diff.files.is_empty());
        let err = diff.error.expect("exhausted budget must fail closed");
        assert!(err.contains("deadline"), "got {err}");
        assert!(take_git_commands().is_empty());
    }

    #[test]
    fn file_stats_fail_closed_on_exhausted_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        assert!(test_git(
            root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        ));
        std::fs::write(root.join("ghost.txt"), "x\n").unwrap();
        let live = compute_file_stats_against_within(&GitBudget::for_column(), root, "HEAD")
            .expect("live budget");
        assert_eq!(
            live.get("ghost.txt"),
            Some(&FileDiffStat {
                added: 1,
                removed: 0
            })
        );
        let spent = GitBudget {
            deadline_at: Instant::now(),
        };
        let _ = take_git_commands();
        let err = compute_file_stats_against_within(&spent, root, "HEAD")
            .expect_err("exhausted budget must not read as empty stats");
        assert!(err.contains("deadline"), "got {err}");
        assert!(take_git_commands().is_empty());
    }

    #[test]
    fn untracked_hash_fails_closed_when_scan_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        assert!(test_git(
            root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        ));
        // A genuinely empty untracked set hashes to a value; a timed-out scan
        // must not produce that same value.
        let empty = hash_untracked_inputs(root, GIT_DEADLINE);
        assert!(empty.is_some());
        assert_eq!(hash_untracked_inputs(root, Duration::from_secs(0)), None);
        let complete = column_fingerprint(root, "HEAD");
        assert_eq!(complete.untracked_hash, empty);
        let timed_out = ColumnFingerprint {
            untracked_hash: None,
            ..complete.clone()
        };
        assert_ne!(complete, timed_out);
    }

    #[test]
    fn column_fingerprint_changes_when_modified_file_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        assert!(test_git(root, &["config", "core.autocrlf", "false"]));
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        assert!(test_git(root, &["add", "tracked.txt"]));
        assert!(test_git(
            root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "-m",
                "init",
            ],
        ));

        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        let first = column_fingerprint(root, "HEAD");
        std::fs::write(root.join("tracked.txt"), "one\nthree\n").unwrap();
        let second = column_fingerprint(root, "HEAD");

        assert_ne!(first, second);
    }

    #[test]
    fn line_counts_sums_hunks() {
        use super::super::engine::DiffHunkStatus;
        let fd = FileDiff {
            path: "x".into(),
            change: FileChange::Modified,
            old_path: None,
            base_text: String::new(),
            new_text: String::new(),
            hunks: vec![
                DiffHunk {
                    base_row_range: 0..1,
                    new_row_range: 0..2,
                    status: DiffHunkStatus::Modified,
                },
                DiffHunk {
                    base_row_range: 5..5,
                    new_row_range: 9..12,
                    status: DiffHunkStatus::Added,
                },
            ],
            is_binary: false,
        };
        assert_eq!(fd.line_counts(), (5, 1));
    }

    #[test]
    fn column_load_runs_under_one_git_budget() {
        // Issue #309: one Review column load must be ONE GitBudget - the
        // fingerprint, the diff, and the file stats share a single deadline and
        // a single toplevel + merge-base resolution, so a wedged git fails the
        // column once, not once per pipeline.
        let loader = include_str!("view/loader.rs");
        for stale in [
            "column_fingerprint(",
            "compute_worktree_diff(",
            "compute_worktree_file_stats(",
        ] {
            assert!(
                !loader.contains(stale),
                "loader.rs must not run `{stale}` as its own git pipeline"
            );
        }
        assert!(
            loader.contains("load_column("),
            "loader.rs must load a column through one budgeted `load_column`"
        );

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        assert!(test_git(root, &["config", "core.autocrlf", "false"]));
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        assert!(test_git(root, &["add", "tracked.txt"]));
        assert!(test_git(
            root,
            &[
                "-c",
                "user.email=paneflow@example.com",
                "-c",
                "user.name=Paneflow",
                "commit",
                "-m",
                "init",
            ],
        ));
        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("ghost.txt"), "x\n").unwrap();

        let _ = take_git_commands();
        let load = load_column(root, "HEAD");
        let cmds = take_git_commands();
        assert_eq!(load.diff.error, None);
        assert!(load.diff.files.iter().any(|f| f.path == "tracked.txt"));
        let stats = load.file_stats.expect("file stats");
        assert_eq!(
            stats.get("ghost.txt"),
            Some(&FileDiffStat {
                added: 1,
                removed: 0
            })
        );
        assert_eq!(load.fingerprint, column_fingerprint(root, "HEAD"));
        let count = |needle: &str| cmds.iter().filter(|c| c.contains(needle)).count();
        assert_eq!(
            count("rev-parse --show-toplevel"),
            1,
            "toplevel resolved once per column load, commands={cmds:?}"
        );
        assert_eq!(
            count("merge-base HEAD"),
            1,
            "merge-base resolved once per column load, commands={cmds:?}"
        );
    }

    #[test]
    fn column_load_fails_within_one_deadline_when_git_never_returns() {
        use std::os::unix::ffi::OsStrExt as _;

        // A FIFO with no writer in place of `.git/config` blocks every git
        // invocation forever. The whole column load - fingerprint, diff, and
        // file stats - has to fail inside ONE budget, not one per pipeline.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        assert!(test_git(&root, &["init"]), "git init is required");
        let config = root.join(".git").join("config");
        std::fs::remove_file(&config).unwrap();
        let fifo = std::ffi::CString::new(config.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path for the call's duration.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let budget = GitBudget {
            deadline_at: Instant::now() + Duration::from_millis(300),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(load_column_within(&budget, &root, "HEAD"));
        });
        let load = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("column load must fail inside one budget, not one per git pipeline");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "column load took {:?} for a 300 ms budget",
            started.elapsed()
        );
        let err = load.diff.error.expect("hung git must fail the diff");
        assert!(err.contains("deadline"), "got {err}");
        assert!(load.file_stats.is_err());
        assert_eq!(load.fingerprint.untracked_hash, None);
    }

    fn test_git(cwd: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn git_subcommand_name(recorded: &str) -> Option<&str> {
        recorded.split_whitespace().next()
    }

    #[test]
    fn compute_diff_against_batches_git_show() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(test_git(root, &["init"]), "git init is required");
        assert!(test_git(root, &["config", "core.autocrlf", "false"]));
        assert!(test_git(
            root,
            &["config", "user.email", "paneflow@example.com"]
        ));
        assert!(test_git(root, &["config", "user.name", "Paneflow"]));

        let files = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
        for name in files {
            std::fs::write(root.join(name), format!("base-{name}\n")).unwrap();
            assert!(test_git(root, &["add", name]));
        }
        assert!(test_git(root, &["commit", "-m", "init"]));
        for name in files {
            std::fs::write(root.join(name), format!("new-{name}\n")).unwrap();
        }

        let _ = take_git_commands();
        let diff = load_column(root, "HEAD").diff;
        let cmds = take_git_commands();

        assert!(
            diff.error.is_none(),
            "diff should succeed, error={:?}",
            diff.error
        );
        assert_eq!(
            diff.files.len(),
            files.len(),
            "expected one FileDiff per modified file, got {:?}",
            diff.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        for name in files {
            let file = diff
                .files
                .iter()
                .find(|f| f.path == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(
                file.base_text.contains(&format!("base-{name}")),
                "{name} base_text={:?}",
                file.base_text
            );
            assert!(
                file.new_text.contains(&format!("new-{name}")),
                "{name} new_text={:?}",
                file.new_text
            );
        }

        let show_count = cmds
            .iter()
            .filter(|c| git_subcommand_name(c) == Some("show"))
            .count();
        let cat_file_batch = cmds.iter().any(|c| {
            git_subcommand_name(c) == Some("cat-file")
                && c.split_whitespace().any(|a| a == "--batch")
                && c.split_whitespace().any(|a| a == "-Z")
        });
        assert!(
            cat_file_batch,
            "one worktree diff must load blobs via git cat-file --batch -Z, commands={cmds:?}"
        );
        assert!(
            show_count == 0,
            "one worktree diff of {} files must not issue one git show per file (got {show_count} show calls), commands={cmds:?}",
            files.len()
        );
    }
}
