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
    worktrees
        .into_iter()
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
/// `None` when none resolve.
pub fn default_base_ref(worktree_dir: &Path) -> Option<String> {
    if ref_exists(worktree_dir, "develop") {
        return Some("develop".to_string());
    }
    if let Some(remote_head) = default_origin_head(worktree_dir) {
        return Some(remote_head);
    }
    for candidate in [
        "main",
        "master",
        "origin/develop",
        "origin/main",
        "origin/master",
    ] {
        if ref_exists(worktree_dir, candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn default_origin_head(worktree_dir: &Path) -> Option<String> {
    let out = run_git(
        worktree_dir,
        &["rev-parse", "--abbrev-ref", "refs/remotes/origin/HEAD"],
    )
    .ok()?;
    let branch = String::from_utf8_lossy(&out).trim().to_string();
    (!branch.is_empty() && branch != "origin/HEAD" && ref_exists(worktree_dir, &branch))
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
    untracked_hash: u64,
}

/// Compute a [`ColumnFingerprint`] for `worktree_dir` against `base_ref`. Runs
/// git subprocesses, so callers invoke it off the GPUI main thread (inside the
/// column build closure / a `smol::unblock`). Failed git reads yield empty or
/// zero components, so unstable repo states fail closed by not matching a prior
/// complete fingerprint.
pub fn column_fingerprint(worktree_dir: &Path, base_ref: &str) -> ColumnFingerprint {
    // Resolve the worktree's own root first, exactly as `compute_worktree_diff`
    // does. The seed `worktree_dir` may be a SUBDIRECTORY (the workspace opened
    // after a shell `cd`). Keying off the toplevel makes the fingerprint cover
    // the same scope the diff does.
    let toplevel = worktree_toplevel(worktree_dir);
    let worktree_dir = toplevel.as_path();
    let rev = |r: &str| {
        run_git(worktree_dir, &["rev-parse", r])
            .ok()
            .map(|o| String::from_utf8_lossy(&o).trim().to_string())
            .unwrap_or_default()
    };
    let merge_base = merge_base(worktree_dir, base_ref).unwrap_or_default();
    let diff_hash = if merge_base.is_empty() {
        0
    } else {
        run_git(
            worktree_dir,
            &["diff", "--binary", "--no-color", &merge_base, "--"],
        )
        .ok()
        .map(|out| hash_bytes(&out))
        .unwrap_or(0)
    };
    ColumnFingerprint {
        head: rev("HEAD"),
        base: rev(base_ref),
        diff_hash,
        untracked_hash: hash_untracked_inputs(worktree_dir),
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;

    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

fn hash_untracked_inputs(worktree_dir: &Path) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    use std::io::Read as _;

    let mut h = std::collections::hash_map::DefaultHasher::new();
    let (paths, truncated) = list_untracked_limited(worktree_dir, MAX_FILE_COUNT + 1);
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
    h.finish()
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
    match run_git(dir, &["rev-parse", "--show-toplevel"]) {
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

fn list_untracked_limited(dir: &Path, limit: usize) -> (Vec<String>, bool) {
    list_untracked_limited_timed(dir, limit, GIT_DEADLINE)
}

fn list_untracked_limited_timed(
    dir: &Path,
    limit: usize,
    deadline: Duration,
) -> (Vec<String>, bool) {
    if limit == 0 {
        return (Vec::new(), false);
    }
    let Ok(out) = run_git_timed(
        dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        deadline,
    ) else {
        return (Vec::new(), false);
    };
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
    (paths, truncated)
}

/// Resolve the merge-base SHA between `HEAD` and `base_ref` in `worktree_dir`.
fn merge_base(worktree_dir: &Path, base_ref: &str) -> Result<String, String> {
    let out = run_git(worktree_dir, &["merge-base", "HEAD", base_ref])?;
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

fn cat_file_specs(merge_base: &str, lookups: &[String]) -> String {
    let mut stdin = String::new();
    for path in lookups {
        stdin.push_str(merge_base);
        stdin.push(':');
        stdin.push_str(path);
        stdin.push('\n');
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

/// Parse `git cat-file --batch` stdout for `count` requests. `None` is a
/// missing/ambiguous object; `Some(bytes)` is the raw blob (or other object)
/// payload.
fn parse_cat_file_batch(stdout: &[u8], count: usize) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut rest = stdout;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
            return Err("truncated git cat-file --batch header".to_string());
        };
        let header = &rest[..nl];
        rest = &rest[nl + 1..];
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
        if rest.first() != Some(&b'\n') {
            return Err("git cat-file --batch payload missing trailing newline".to_string());
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
        &["cat-file", "--batch-check"],
        stdin.as_bytes(),
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
        .split(|&b| b == b'\n')
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
        &["cat-file", "--batch"],
        stdin.as_bytes(),
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
/// caught by the post-load length check in [`compute_worktree_diff`].
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

/// Compute the diff of `worktree_dir` against `base_ref`:
/// `merge-base(HEAD, base_ref)..working-tree`, including uncommitted changes.
///
/// Runs entirely via subprocess; safe to call off the main thread. Returns a
/// `WorktreeDiff` whose `error` is set (rather than panicking) when the base
/// ref or merge base cannot be resolved. Oversized / lockfile / over-count
/// files are shown as stubs rather than loaded, bounding peak RAM.
pub fn compute_worktree_diff(worktree_dir: &Path, base_ref: &str) -> WorktreeDiff {
    // Resolve the worktree's own root once: the seed path may be a subdirectory
    // (shell `cd`), which would make `worktree_dir.join(rel_path)` miss every
    // file. Everything below - merge-base, name-status, file reads - keys off
    // this so the diff is correct regardless of the seed path's depth.
    let toplevel = worktree_toplevel(worktree_dir);
    let worktree_dir = toplevel.as_path();
    log::debug!(
        "git: compute_worktree_diff dir={} base={base_ref}",
        worktree_dir.display()
    );
    let merge_base = match merge_base(worktree_dir, base_ref) {
        Ok(mb) => mb,
        Err(e) => {
            log::warn!("git: merge_base failed (base={base_ref}): {e}");
            return WorktreeDiff {
                files: Vec::new(),
                error: Some(e),
            };
        }
    };
    log::debug!("git: merge_base={merge_base}");

    compute_diff_against(worktree_dir, &merge_base)
}

/// Per-file Git-native diffstat for the same semantic as
/// [`compute_worktree_diff`]: `merge-base(HEAD, base_ref)..working-tree`, plus
/// untracked files. This is used for Review's sidebar/global counters so they
/// match `git diff --numstat` instead of drifting with renderer hunk details.
pub fn compute_worktree_file_stats(
    worktree_dir: &Path,
    base_ref: &str,
) -> HashMap<String, FileDiffStat> {
    let toplevel = worktree_toplevel(worktree_dir);
    let worktree_dir = toplevel.as_path();
    let Ok(merge_base) = merge_base(worktree_dir, base_ref) else {
        return HashMap::new();
    };
    compute_file_stats_against(worktree_dir, &merge_base)
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
/// size / count / binary guards are identical to [`compute_worktree_diff`].
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

/// Shared core of [`compute_worktree_diff`] and [`compute_head_diff`]: diff the
/// working tree against the already-resolved commit-ish `base`. `worktree_dir`
/// must already be the worktree toplevel (both callers resolve it first).
/// Oversized / lockfile / over-count files are shown as stubs rather than
/// loaded, bounding peak RAM.
fn compute_diff_against(worktree_dir: &Path, base: &str) -> WorktreeDiff {
    let budget = GitBudget::for_column();
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
            list_untracked_limited_timed(worktree_dir, remaining, deadline);
        truncated |= untracked_truncated;
        for path in untracked {
            changes.push((FileChange::Added, path, None));
        }
    }
    log::debug!("git: {} changed files", changes.len());

    let mut lookups = Vec::new();
    for (change, path, old_path) in &changes {
        if is_skipped_name(path) || is_too_large(worktree_dir, path) {
            continue;
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
    let blobs = match load_base_texts_batch(worktree_dir, base, &lookups, &budget) {
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
        if is_skipped_name(&path) || is_too_large(worktree_dir, &path) {
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
            _ => load_working_text(worktree_dir, &path),
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

fn compute_file_stats_against(worktree_dir: &Path, base: &str) -> HashMap<String, FileDiffStat> {
    let mut stats = run_git(
        worktree_dir,
        &["diff", "--numstat", "-z", "--no-color", base, "--"],
    )
    .map(|out| parse_numstat_z(&out))
    .unwrap_or_default();

    let remaining = MAX_FILE_COUNT.saturating_sub(stats.len());
    if remaining == 0 {
        return stats;
    }

    let (untracked, truncated) = list_untracked_limited(worktree_dir, remaining);
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

    stats
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

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
        stdout.extend_from_slice(b"HEAD:gone.rs missing\n");
        stdout.extend_from_slice(b"abc blob 0\n\n");
        stdout.extend_from_slice(b"def blob 5\nhello\n");
        let parsed = parse_cat_file_batch(&stdout, 3).unwrap();
        assert_eq!(parsed[0], None);
        assert_eq!(parsed[1], Some(Vec::new()));
        assert_eq!(parsed[2], Some(b"hello".to_vec()));
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
        if !test_git(root, &["init"]) {
            return;
        }
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

        let stats = compute_worktree_file_stats(root, "HEAD");
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
    fn list_untracked_limited_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !test_git(root, &["init"]) {
            return;
        }
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        std::fs::write(root.join("c.txt"), "c\n").unwrap();

        let (paths, truncated) = list_untracked_limited(root, 2);
        assert_eq!(paths.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn column_fingerprint_changes_when_modified_file_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !test_git(root, &["init"]) {
            return;
        }
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
        if !test_git(root, &["init"]) {
            return;
        }
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
        let diff = compute_worktree_diff(root, "HEAD");
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
        });
        assert!(
            cat_file_batch,
            "one worktree diff must load blobs via git cat-file --batch, commands={cmds:?}"
        );
        assert!(
            show_count == 0,
            "one worktree diff of {} files must not issue one git show per file (got {show_count} show calls), commands={cmds:?}",
            files.len()
        );
    }
}
