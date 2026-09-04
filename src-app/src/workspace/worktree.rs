//! Git worktree-per-agent management (EP-002, prd-orchestration-v2).
//!
//! `paneflow up` panes can declare `worktree = "branch"`: the CLI process
//! creates (or reuses) a git worktree in a SIBLING directory of the repo -
//! `<repo>.worktrees/<branch-slug>`, or `<branch-slug>-<hash>` on slug
//! collision - copies the top-level gitignored `.env*` files, optionally runs
//! a `setup` command, and the pane spawns with the worktree as its cwd. The
//! app side records ownership ([`ManagedWorktree`]). Closing transfers that
//! ownership to the undo record; retirement happens when the record is evicted
//! or on final quit, and removes the worktree only IF it is clean.
//!
//! Invariants (US-006/US-009):
//! - a branch is NEVER deleted, only the worktree directory;
//! - a worktree with uncommitted changes is NEVER removed;
//! - only worktrees Paneflow created (tracked in `managed_worktrees`) are
//!   ever torn down - a pre-existing worktree pointed at by `cwd` is not ours;
//! - every git invocation is a subprocess with argv (no shell interpolation)
//!   under [`paneflow_process::run_with_timeout`], and on the app side it runs
//!   off the render thread (`smol::unblock`).
//!
//! Sibling (not in-repo) placement keeps recursive file watchers - including
//! Paneflow's own diff watcher - from descending into N extra checkouts.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Wall-clock bound for plumbing git calls (list/status/remove/prune)
/// and the destructive-gate process CWD scan.
const GIT_DEADLINE: Duration = Duration::from_secs(10);
/// `worktree add` checks out a full tree - give it more room on big repos.
const ADD_DEADLINE: Duration = Duration::from_secs(120);
const STDOUT_CAP: u64 = 256 * 1024;
const OWNER_MARKER_FILE: &str = ".paneflow-worktree";
const MAX_OWNER_MARKER_BYTES: u64 = 8 * 1024;

/// Teardown policy for a managed worktree (US-009). `Auto` removes the
/// worktree when its lifecycle ownership is finally retired and it has no
/// uncommitted changes; `Keep` opts out entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TeardownPolicy {
    #[default]
    Auto,
    Keep,
}

impl TeardownPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            TeardownPolicy::Auto => "auto",
            TeardownPolicy::Keep => "keep",
        }
    }
}

/// A worktree Paneflow created for a pane and therefore owns the lifecycle of.
/// Carried by `Workspace`, persisted in `session.json` (so a crash does not
/// orphan the ownership record), transferred through workspace undo, and torn
/// down only when that ownership is finally retired.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedWorktree {
    /// Worktree checkout directory (`<repo>.worktrees/<slug>`).
    pub path: PathBuf,
    /// Main repository root (where `git worktree …` commands run).
    pub repo_root: PathBuf,
    /// Branch checked out in the worktree. Recorded for diagnostics only -
    /// teardown never touches the branch.
    pub branch: String,
    pub teardown: TeardownPolicy,
    /// Stable identity of the checkout directory. New/live records always
    /// carry it; `None` is accepted only long enough to upgrade legacy records
    /// that still have a valid owner marker.
    pub(crate) identity: Option<WorktreeIdentity>,
}

/// macOS directory identity persisted across marker unlink but changed by a
/// remove/recreate at the same path. Birth time supplements dev/inode so an
/// immediately reused inode cannot transfer lifecycle ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeIdentity(String);

impl WorktreeIdentity {
    fn from_path(path: &Path) -> Result<Self, String> {
        use std::os::macos::fs::MetadataExt;

        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect worktree identity: {error}"))?;
        if !metadata.is_dir() {
            return Err(format!("worktree {} is not a directory", path.display()));
        }
        Self::from_parts(
            path,
            metadata.st_dev(),
            metadata.st_ino(),
            metadata.st_birthtime(),
            metadata.st_birthtime_nsec(),
        )
    }

    /// Birth time is the only field that changes on a remove/recreate at the
    /// same path; without it the identity is dev/inode alone and an
    /// immediately reused inode looks identical. Volumes that do not implement
    /// it (NFS, some SMB/FUSE mounts) report 0/0, so that reading is treated as
    /// "identity unavailable" and every caller fails closed - no teardown, no
    /// marker recovery - instead of trusting inode reuse.
    fn from_parts(
        path: &Path,
        dev: u64,
        ino: u64,
        birthtime: i64,
        birthtime_nsec: i64,
    ) -> Result<Self, String> {
        if birthtime == 0 && birthtime_nsec == 0 {
            return Err(format!(
                "worktree {} is on a filesystem without directory birth time",
                path.display()
            ));
        }
        Ok(Self(format!("{dev}:{ino}:{birthtime}:{birthtime_nsec}")))
    }

    fn from_persisted(raw: &str) -> Option<Self> {
        let mut fields = raw.split(':');
        fields.next()?.parse::<u64>().ok()?;
        fields.next()?.parse::<u64>().ok()?;
        let birthtime = fields.next()?.parse::<i64>().ok()?;
        let birthtime_nsec = fields.next()?.parse::<i64>().ok()?;
        if fields.next().is_some() {
            return None;
        }
        // A 0/0 birth time asserts nothing beyond inode reuse (see
        // `from_parts`), so it is refused rather than matched against a live
        // directory.
        if birthtime == 0 && birthtime_nsec == 0 {
            return None;
        }
        Some(Self(raw.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn worktree_identity(path: &Path) -> Result<WorktreeIdentity, String> {
    WorktreeIdentity::from_path(path)
}

/// Coalesce canonical ownership records without weakening teardown policy.
/// Session files written by older/manual clients can contain duplicates; an
/// explicit `Keep` on any duplicate must win over `Auto` before one record is
/// discarded. Inconsistent identity metadata also fails closed to `Keep`.
pub(crate) fn merge_managed_worktree_records(
    mut worktrees: Vec<ManagedWorktree>,
) -> Vec<ManagedWorktree> {
    worktrees.sort_by(|left, right| left.path.cmp(&right.path));
    let mut merged: Vec<ManagedWorktree> = Vec::with_capacity(worktrees.len());
    for worktree in worktrees {
        let Some(previous) = merged.last_mut().filter(|last| last.path == worktree.path) else {
            merged.push(worktree);
            continue;
        };
        if previous.repo_root != worktree.repo_root || previous.branch != worktree.branch {
            log::warn!(
                "managed worktree: conflicting ownership metadata for {}; keeping checkout",
                previous.path.display()
            );
            previous.teardown = TeardownPolicy::Keep;
        } else if worktree.teardown == TeardownPolicy::Keep {
            previous.teardown = TeardownPolicy::Keep;
        }
        match (&previous.identity, &worktree.identity) {
            (Some(left), Some(right)) if left != right => {
                log::warn!(
                    "managed worktree: conflicting directory identity for {}; keeping checkout",
                    previous.path.display()
                );
                previous.teardown = TeardownPolicy::Keep;
            }
            (None, Some(identity)) => previous.identity = Some(identity.clone()),
            _ => {}
        }
    }
    merged
}

pub fn owner_marker_path(worktree_path: &Path) -> PathBuf {
    worktree_path.join(OWNER_MARKER_FILE)
}

fn owner_marker_contents(repo_root: &Path, branch: &str) -> Result<Vec<u8>, String> {
    let repo_root = std::fs::canonicalize(repo_root)
        .map_err(|error| format!("cannot canonicalize repo root: {error}"))?;
    let repo_root = repo_root.to_string_lossy();
    if repo_root.contains(['\n', '\r']) || branch.contains(['\n', '\r']) {
        return Err("repo root and branch must not contain line breaks".to_string());
    }
    Ok(format!("owner=paneflow\nrepo_root={repo_root}\nbranch={branch}\n").into_bytes())
}

fn write_owner_marker(worktree_path: &Path, repo_root: &Path, branch: &str) -> Result<(), String> {
    use std::io::Write;

    write_owner_marker_with(worktree_path, repo_root, branch, |file, contents| {
        file.write_all(contents)
    })
}

fn write_owner_marker_with(
    worktree_path: &Path,
    repo_root: &Path,
    branch: &str,
    write_contents: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> Result<(), String> {
    use std::io::Write;

    let marker = owner_marker_path(worktree_path);
    let contents = owner_marker_contents(repo_root, branch)?;
    let parent = worktree_path
        .parent()
        .ok_or_else(|| format!("worktree {} has no parent", worktree_path.display()))?;
    // Build the complete marker outside the checkout. A short/failed write can
    // therefore never leave a malformed reserved marker that neither normal
    // ownership validation nor pending-journal recovery can recognize.
    let mut temporary = tempfile::Builder::new()
        .prefix(".paneflow-owner-")
        .tempfile_in(parent)
        .map_err(|error| format!("cannot stage owner marker {}: {error}", marker.display()))?;
    write_contents(temporary.as_file_mut(), &contents)
        .map_err(|error| format!("cannot write owner marker {}: {error}", marker.display()))?;
    temporary
        .as_file_mut()
        .flush()
        .map_err(|error| format!("cannot flush owner marker {}: {error}", marker.display()))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync owner marker {}: {error}", marker.display()))?;
    // hard_link creates the destination atomically and never replaces an
    // existing file, symlink, FIFO, or tracked path. The temporary name is
    // removed by Drop after the complete inode has been published.
    std::fs::hard_link(temporary.path(), &marker)
        .map_err(|error| format!("cannot create owner marker {}: {error}", marker.display()))
}

fn validated_owner_marker(
    worktree_path: &Path,
    repo_root: &Path,
    branch: &str,
) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let marker = owner_marker_path(worktree_path);
    let link_metadata = std::fs::symlink_metadata(&marker)
        .map_err(|error| format!("cannot inspect owner marker {}: {error}", marker.display()))?;
    if !link_metadata.file_type().is_file() {
        return Err(format!(
            "owner marker {} is not a regular file",
            marker.display()
        ));
    }
    if link_metadata.len() > MAX_OWNER_MARKER_BYTES {
        return Err(format!(
            "owner marker {} exceeds {MAX_OWNER_MARKER_BYTES} bytes",
            marker.display()
        ));
    }
    let file = std::fs::File::open(&marker)
        .map_err(|error| format!("cannot read owner marker {}: {error}", marker.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect owner marker {}: {error}", marker.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_OWNER_MARKER_BYTES {
        return Err(format!(
            "owner marker {} is not a small regular file",
            marker.display()
        ));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_OWNER_MARKER_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| format!("cannot read owner marker {}: {error}", marker.display()))?;
    if contents.len() as u64 > MAX_OWNER_MARKER_BYTES {
        return Err(format!(
            "owner marker {} grew beyond {MAX_OWNER_MARKER_BYTES} bytes",
            marker.display()
        ));
    }
    let text = std::str::from_utf8(&contents)
        .map_err(|_| format!("owner marker {} is not UTF-8", marker.display()))?;
    let Some(body) = text.strip_suffix('\n') else {
        return Err(format!(
            "owner marker {} is missing its final newline",
            marker.display()
        ));
    };
    let mut lines = body.split('\n');
    if lines.next() != Some("owner=paneflow") {
        return Err(format!(
            "owner marker {} has the wrong owner",
            marker.display()
        ));
    }
    let marker_repo_root = lines
        .next()
        .and_then(|line| line.strip_prefix("repo_root="))
        .ok_or_else(|| format!("owner marker {} has no repo root", marker.display()))?;
    let marker_branch = lines
        .next()
        .and_then(|line| line.strip_prefix("branch="))
        .ok_or_else(|| format!("owner marker {} has no branch", marker.display()))?;
    if lines.next().is_some() {
        return Err(format!(
            "owner marker {} has unexpected fields",
            marker.display()
        ));
    }

    let expected = owner_marker_contents(repo_root, branch)?;
    let expected_text = std::str::from_utf8(&expected)
        .map_err(|_| "generated owner marker is not UTF-8".to_string())?;
    let mut expected_lines = expected_text.lines();
    let _owner = expected_lines.next();
    let expected_repo_root = expected_lines
        .next()
        .and_then(|line| line.strip_prefix("repo_root="))
        .unwrap_or_default();
    if marker_repo_root != expected_repo_root {
        return Err(format!(
            "owner marker {} names a different repo root",
            marker.display()
        ));
    }
    if marker_branch != branch {
        return Err(format!(
            "owner marker {} names a different branch",
            marker.display()
        ));
    }
    if contents != expected {
        return Err(format!(
            "owner marker {} does not match the expected schema",
            marker.display()
        ));
    }
    Ok(contents)
}

/// Rehydrate a persisted or IPC-provided ownership record. The record is only
/// accepted when it matches Paneflow's deterministic worktree directory and the
/// on-disk worktree carries Paneflow's owner marker.
pub fn managed_worktree_from_record(
    path_raw: &str,
    repo_root_raw: &str,
    branch_raw: &str,
    teardown_raw: &str,
) -> Option<ManagedWorktree> {
    managed_worktree_from_persisted_record(path_raw, repo_root_raw, branch_raw, teardown_raw, None)
}

pub(crate) fn managed_worktree_from_persisted_record(
    path_raw: &str,
    repo_root_raw: &str,
    branch_raw: &str,
    teardown_raw: &str,
    identity_raw: Option<&str>,
) -> Option<ManagedWorktree> {
    let path = PathBuf::from(path_raw);
    let repo_root = PathBuf::from(repo_root_raw);
    if !path.is_absolute() || !repo_root.is_absolute() {
        log::warn!("managed worktree: dropping record with non-absolute path");
        return None;
    }
    let branch = branch_raw.trim();
    if branch.is_empty() || branch_slug(branch).is_empty() {
        log::warn!("managed worktree: dropping record with invalid branch");
        return None;
    }
    if !is_paneflow_worktree_dir(&repo_root, branch, &path) {
        log::warn!(
            "managed worktree: dropping record outside Paneflow worktree dir: {}",
            path.display()
        );
        return None;
    }
    // Ownership identity is the canonical checkout directory, never an IPC-
    // supplied spelling. This collapses symlink/`..` aliases before the app's
    // exclusive-owner checks and retirement journal compare paths.
    let path = match std::fs::canonicalize(&path) {
        Ok(path) => path,
        Err(error) => {
            log::warn!("managed worktree: cannot canonicalize path: {error}");
            return None;
        }
    };
    let repo_root = match std::fs::canonicalize(&repo_root) {
        Ok(repo_root) => repo_root,
        Err(error) => {
            log::warn!("managed worktree: cannot canonicalize repo root: {error}");
            return None;
        }
    };
    if !is_paneflow_worktree_dir(&repo_root, branch, &path) {
        log::warn!(
            "managed worktree: canonical path escapes Paneflow worktree dir: {}",
            path.display()
        );
        return None;
    }
    let identity = match identity_raw {
        Some(raw) => {
            let expected = WorktreeIdentity::from_persisted(raw)?;
            let actual = WorktreeIdentity::from_path(&path).ok()?;
            if actual != expected {
                log::warn!(
                    "managed worktree: directory identity changed for {}",
                    path.display()
                );
                return None;
            }
            actual
        }
        None => WorktreeIdentity::from_path(&path).ok()?,
    };
    if let Err(error) = validated_owner_marker(&path, &repo_root, branch) {
        log::warn!(
            "managed worktree: dropping record with invalid owner marker in {}: {error}",
            path.display()
        );
        return None;
    }
    let teardown = match teardown_raw {
        "auto" => TeardownPolicy::Auto,
        "keep" => TeardownPolicy::Keep,
        other => {
            log::warn!("managed worktree: unknown teardown policy {other:?}; keeping");
            TeardownPolicy::Keep
        }
    };
    Some(ManagedWorktree {
        path,
        repo_root,
        branch: branch.to_string(),
        teardown,
        identity: Some(identity),
    })
}

/// Rehydrate an entry from the durable retirement journal. In addition to the
/// normal strict path, this recovers the one crash window in managed teardown:
/// the owner marker was unlinked but `git worktree remove` had not completed.
/// Recovery is deliberately unavailable to live workspace records and only
/// recreates a missing marker for the exact deterministic checkout/branch
/// still registered by the canonical repository.
#[cfg(test)]
fn pending_managed_worktree_from_record(
    path_raw: &str,
    repo_root_raw: &str,
    branch_raw: &str,
    teardown_raw: &str,
) -> Option<ManagedWorktree> {
    pending_managed_worktree_from_persisted_record(
        path_raw,
        repo_root_raw,
        branch_raw,
        teardown_raw,
        None,
    )
}

pub(crate) fn pending_managed_worktree_from_persisted_record(
    path_raw: &str,
    repo_root_raw: &str,
    branch_raw: &str,
    teardown_raw: &str,
    identity_raw: Option<&str>,
) -> Option<ManagedWorktree> {
    if let Some(worktree) = managed_worktree_from_persisted_record(
        path_raw,
        repo_root_raw,
        branch_raw,
        teardown_raw,
        identity_raw,
    ) {
        return Some(worktree);
    }
    // Only Auto teardown unlinks this marker. Keep (including an unknown
    // policy that fails closed to Keep) cannot represent the interrupted
    // removal window and must never mint ownership metadata.
    if teardown_raw != "auto" {
        return None;
    }

    let raw_path = PathBuf::from(path_raw);
    let raw_repo_root = PathBuf::from(repo_root_raw);
    let branch = branch_raw.trim();
    if !raw_path.is_absolute()
        || !raw_repo_root.is_absolute()
        || branch.is_empty()
        || branch_slug(branch).is_empty()
        || !is_paneflow_worktree_dir(&raw_repo_root, branch, &raw_path)
    {
        return None;
    }
    let path = std::fs::canonicalize(&raw_path).ok()?;
    let repo_root = std::fs::canonicalize(&raw_repo_root).ok()?;
    if !is_paneflow_worktree_dir(&repo_root, branch, &path) {
        return None;
    }
    let expected_identity = WorktreeIdentity::from_persisted(identity_raw?)?;
    if WorktreeIdentity::from_path(&path).ok()? != expected_identity {
        log::warn!(
            "managed worktree: refusing marker recovery after directory replacement in {}",
            path.display()
        );
        return None;
    }

    let marker = owner_marker_path(&path);
    match std::fs::symlink_metadata(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        // A malformed file, symlink, FIFO, or racing replacement is evidence
        // we must not overwrite. A valid marker already returned above.
        _ => return None,
    }
    let registered = list_worktrees(&repo_root).ok()?.into_iter().any(|entry| {
        entry.branch.as_deref() == Some(branch)
            && std::fs::canonicalize(entry.path).ok().as_ref() == Some(&path)
    });
    if !registered {
        return None;
    }
    if let Err(error) = write_owner_marker(&path, &repo_root, branch) {
        log::warn!(
            "managed worktree: cannot recover interrupted teardown marker in {}: {error}",
            path.display()
        );
        return None;
    }
    managed_worktree_from_persisted_record(
        &path.to_string_lossy(),
        &repo_root.to_string_lossy(),
        branch,
        teardown_raw,
        Some(expected_identity.as_str()),
    )
}

/// One entry of `git worktree list --porcelain`.
///
/// A `worktree ` line is enough to keep the entry. Bare and other HEAD-less
/// checkouts are included so Launch Pad collision checks and the Review
/// Worktree-scope picker list the same set.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// `None` for a detached-HEAD or HEAD-less (including bare) worktree.
    pub branch: Option<String>,
    /// SHA from the porcelain `HEAD` line; `None` when that line is absent.
    pub sha: Option<String>,
    pub is_bare: bool,
}

/// Display name for a checkout: its branch, or a directory name when HEAD is
/// detached.
///
/// Detached is not an edge case - `git worktree add --detach` leaves that
/// state, and agent tooling that lays checkouts out as `<...>/<slug>/<repo>`
/// produces a directory whose own name is just the repository's, identical for
/// every worktree. So when the last component repeats the repository's name,
/// the parent is what actually distinguishes this checkout and the label uses
/// it.
pub fn checkout_label(branch: Option<&str>, path: &Path, repo_root: &Path) -> String {
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        return branch.to_string();
    }
    let name = path.file_name();
    if name.is_some()
        && name == repo_root.file_name()
        && let Some(parent) = path.parent().and_then(Path::file_name)
    {
        return parent.to_string_lossy().into_owned();
    }
    name.map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Filesystem-safe directory name for a branch (`feat/x` → `feat-x`).
/// Conservative whitelist: anything outside `[A-Za-z0-9._-]` becomes `-`.
/// Leading/trailing `-` AND `.` are trimmed: a dot-only branch (`.`/`..`)
/// would otherwise survive as a path-traversal component of the (destructive)
/// worktree path, and a leading dot would hide the directory. May return ""
/// for degenerate input - spec validation rejects that before any git call,
/// and [`worktree_dir`] falls back to a safe constant as defense in depth.
pub fn branch_slug(branch: &str) -> String {
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches(|c: char| c == '-' || c == '.')
        .to_string()
}

fn branch_slug_or_default(branch: &str) -> String {
    let slug = branch_slug(branch);
    if slug.is_empty() {
        "branch".to_string()
    } else {
        slug
    }
}

fn branch_hash_suffix(branch: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in branch.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")[..8].to_string()
}

fn worktrees_parent(repo_root: &Path) -> PathBuf {
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let parent = repo_root.parent().unwrap_or(repo_root);
    parent.join(format!("{repo_name}.worktrees"))
}

/// Sibling worktree directory for a branch: `<repo>.worktrees/<slug>`, next to
/// the repo (NOT inside it - recursive watchers must not descend into it).
/// Total function: a branch whose slug is empty (dot-only - rejected upstream
/// by spec validation) maps to the constant `branch` so the result can never
/// resolve outside `<repo>.worktrees/`.
pub fn worktree_dir(repo_root: &Path, branch: &str) -> PathBuf {
    worktrees_parent(repo_root).join(branch_slug_or_default(branch))
}

/// Collision-resistant sibling directory for a branch. Kept separate from
/// [`worktree_dir`] so existing readable paths remain valid; planners switch
/// to this path only when the slug path is already claimed by another branch.
pub fn worktree_dir_hashed(repo_root: &Path, branch: &str) -> PathBuf {
    let slug = branch_slug_or_default(branch);
    worktrees_parent(repo_root).join(format!("{slug}-{}", branch_hash_suffix(branch)))
}

pub fn is_paneflow_worktree_dir(repo_root: &Path, branch: &str, path: &Path) -> bool {
    path == worktree_dir(repo_root, branch) || path == worktree_dir_hashed(repo_root, branch)
}

/// Isolated `git` spawn: ignore the opened repo's `core.hooksPath` /
/// `core.fsmonitor` / `diff.external`, drop inherited git location/SSH env,
/// and never prompt.
pub(crate) fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "core.fsmonitor=",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "diff.external=",
    ]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env_remove("GIT_DIR");
    cmd.env_remove("GIT_WORK_TREE");
    cmd.env_remove("GIT_SSH_COMMAND");
    cmd
}

/// Append a git subcommand, forcing `--no-ext-diff` on `git diff`.
pub(crate) fn git_subcommand(cmd: &mut Command, args: &[&str]) {
    match args {
        ["diff", rest @ ..] => {
            cmd.arg("diff").arg("--no-ext-diff").args(rest);
        }
        _ => {
            cmd.args(args);
        }
    }
}

/// Run a git plumbing command and return trimmed stdout, mapping every
/// failure mode (spawn, timeout, non-zero exit) to a displayable message.
fn run_git(repo: &Path, args: &[&str], deadline: Duration) -> Result<String, String> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(repo);
    git_subcommand(&mut cmd, args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let out = paneflow_process::run_with_timeout(cmd, deadline, STDOUT_CAP)
        .map_err(|e| format!("git {} failed: {e}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim().lines().last().unwrap_or("non-zero exit")
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git worktree list --porcelain`, parsed.
pub fn list_worktrees(repo_root: &Path) -> Result<Vec<WorktreeEntry>, String> {
    let stdout = run_git(
        repo_root,
        &["worktree", "list", "--porcelain"],
        GIT_DEADLINE,
    )?;
    Ok(parse_worktree_porcelain(&stdout))
}

/// Pure porcelain parser (unit-tested). Entries are blank-line separated;
/// `branch refs/heads/<name>` is absent for detached or bare entries.
/// A `worktree ` line is enough; HEAD-less and bare entries are kept.
pub fn parse_worktree_porcelain(stdout: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut sha: Option<String> = None;
    let mut is_bare = false;
    for line in stdout.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(p) = path.take() {
                entries.push(WorktreeEntry {
                    path: p,
                    branch: branch.take(),
                    sha: sha.take(),
                    is_bare,
                });
            }
            branch = None;
            sha = None;
            is_bare = false;
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            sha = Some(h.to_string());
        } else if line == "bare" {
            is_bare = true;
        }
    }
    entries
}

/// True when `branch` exists locally in the repo.
pub fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    run_git(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        GIT_DEADLINE,
    )
    .is_ok()
}

/// Where a chosen branch's checkout is, or has to be made (issue #347).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchCheckout {
    /// A worktree already holds the branch - bind to it, create nothing.
    Existing(PathBuf),
    /// Nothing holds it yet; this is where its worktree belongs.
    Create(PathBuf),
}

/// Resolve a branch to a checkout directory, without touching the filesystem.
///
/// Split from [`prepare_branch_checkout`] so the collision rules are testable
/// without a repository. `Existing` first: git refuses a second worktree on the
/// same branch, so reusing the one that holds it is the only way selecting an
/// already-checked-out branch can work at all. The path rules are the Launch
/// Pad's (`launch_pad_worktree_plan`): the slug directory, or the hashed one
/// when the slug is already claimed by another branch.
pub fn plan_branch_checkout(
    entries: &[WorktreeEntry],
    repo_root: &Path,
    branch: &str,
) -> Result<BranchCheckout, String> {
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.branch.as_deref() == Some(branch))
    {
        return Ok(BranchCheckout::Existing(entry.path.clone()));
    }
    let legacy = worktree_dir(repo_root, branch);
    let path = if entries.iter().any(|entry| entry.path == legacy) {
        worktree_dir_hashed(repo_root, branch)
    } else {
        legacy
    };
    if let Some(entry) = entries.iter().find(|entry| entry.path == path) {
        return Err(format!(
            "{} exists but holds another branch ({})",
            path.display(),
            entry.branch.as_deref().unwrap_or("detached")
        ));
    }
    Ok(BranchCheckout::Create(path))
}

/// The directory to work in for `branch`: the worktree that already holds it,
/// or a new sibling worktree checked out from it (issue #347).
///
/// Blocking (two to three git subprocesses, one of them a full checkout, every
/// one through [`run_git`] with `GIT_TERMINAL_PROMPT=0`) - the caller runs it
/// through `smol::unblock`, never on the render thread.
///
/// The result is deliberately NOT a [`ManagedWorktree`], and no owner marker
/// is written: teardown is for the checkouts orchestration creates behind the
/// user's back, and a directory asked for by name, by hand, is the user's.
/// Nothing here removes it, workspace close never touches it, and the branch
/// is untouched either way. Without the marker a later `managed_worktree`
/// record naming this path cannot adopt it either
/// (`managed_worktree_from_record` validates the marker), so the checkout
/// cannot be torn down by accident later.
pub fn prepare_branch_checkout(repo_root: &Path, branch: &str) -> Result<PathBuf, String> {
    let entries = list_worktrees(repo_root)?;
    match plan_branch_checkout(&entries, repo_root, branch)? {
        BranchCheckout::Existing(path) => Ok(path),
        BranchCheckout::Create(path) => {
            if path.exists() {
                return Err(format!(
                    "{} exists but is not a registered worktree; remove it first",
                    path.display()
                ));
            }
            git_worktree_add(repo_root, &path, branch, false)?;
            // Same courtesy `paneflow up` and the Launch Pad extend their
            // worktrees: a checkout without the repository's gitignored
            // `.env*` cannot run the app it holds. Best-effort by design.
            let _ = copy_env_files(repo_root, &path);
            Ok(path)
        }
    }
}

/// `git worktree add <path> [-b] <branch>` alone: the checkout, and nothing
/// that claims lifecycle ownership of it.
fn git_worktree_add(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    create_branch: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let path_s = path.to_string_lossy();
    let mut args: Vec<&str> = vec!["worktree", "add", &path_s];
    if create_branch {
        args.push("-b");
    }
    args.push(branch);
    run_git(repo_root, &args, ADD_DEADLINE)?;
    Ok(())
}

/// `git worktree add <path> [-b] <branch>`, then the owner marker that makes
/// the checkout a [`ManagedWorktree`]. `create_branch` chooses between
/// branching off HEAD (`-b`) and checking out the existing branch.
pub fn add_worktree(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    create_branch: bool,
) -> Result<(), String> {
    git_worktree_add(repo_root, path, branch, create_branch)?;
    if let Err(e) = write_owner_marker(path, repo_root, branch) {
        return Err(add_worktree_marker_failure(
            e,
            remove_worktree(repo_root, path),
        ));
    }
    Ok(())
}

fn add_worktree_marker_failure(marker_error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => marker_error,
        Err(rollback_error) => format!(
            "{marker_error}; worktree rollback failed and the checkout may remain: {rollback_error}"
        ),
    }
}

/// True when the worktree has no uncommitted changes, untracked files, or
/// ignored files. The sole exception is Paneflow's own root owner marker,
/// which is teardown metadata rather than user data. An error (worktree gone,
/// git missing) is NOT "clean" - the caller must keep its hands off when it
/// cannot prove cleanliness.
pub fn is_clean(worktree_path: &Path) -> Result<bool, String> {
    run_git(
        worktree_path,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ],
        GIT_DEADLINE,
    )
    .map(|out| managed_status_is_clean(&out))
}

fn managed_status_is_clean(status: &str) -> bool {
    status
        .lines()
        .all(|line| matches!(line, "?? .paneflow-worktree" | "!! .paneflow-worktree"))
}

/// True when the worktree holds no uncommitted work a user could lose: no
/// tracked modification and no untracked file that git would track. Ignored
/// files do not count (issue #348), which is exactly the gate
/// `git worktree remove` applies itself: the `.env*` copies
/// [`prepare_branch_checkout`] makes for a picker checkout, build output, and
/// caches are all ignored, and a removal that refused them would refuse every
/// checkout the picker ever made. [`is_clean`] stays stricter for managed
/// teardown, which deletes without a user in the loop. An error is NOT
/// "clean", as with [`is_clean`].
pub fn is_clean_for_removal(worktree_path: &Path) -> Result<bool, String> {
    run_git(
        worktree_path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        GIT_DEADLINE,
    )
    .map(|out| out.trim().is_empty())
}

/// `git worktree remove <path>`. Refuses dirty worktrees by itself too (git
/// native), but callers must check [`is_clean`] first to control messaging.
/// The BRANCH IS NEVER DELETED - that is the US-009 invariant, not a TODO.
pub fn remove_worktree(repo_root: &Path, path: &Path) -> Result<(), String> {
    let path_s = path.to_string_lossy();
    run_git(repo_root, &["worktree", "remove", &path_s], GIT_DEADLINE).map(|_| ())
}

fn validated_worktree_identity(worktree: &ManagedWorktree) -> Result<(), String> {
    let expected = worktree
        .identity
        .as_ref()
        .ok_or_else(|| "managed worktree has no persisted directory identity".to_string())?;
    let actual = WorktreeIdentity::from_path(&worktree.path)?;
    if &actual != expected {
        return Err(format!(
            "managed worktree directory identity changed for {}",
            worktree.path.display()
        ));
    }
    Ok(())
}

/// Remove Paneflow's untracked/ignored ownership marker before asking git to
/// remove an otherwise-clean managed worktree. `git worktree remove` refuses
/// even a checkout whose only untracked file is our marker. If user data
/// appears after the status check, git still refuses removal; losing only our
/// marker in that race is safer than forcing removal or overwriting a file
/// that appeared at the marker path.
fn remove_managed_worktree(
    worktree: &ManagedWorktree,
    protected_session_ids: &[u32],
) -> Result<(), String> {
    if worktree_has_live_process_cwd(&worktree.path, protected_session_ids)? {
        return Err("a live process uses the checkout as its cwd".to_string());
    }
    // Revalidate ownership after the process scan and immediately before the
    // unlink. A replacement marker must never be deleted on stale evidence.
    validated_worktree_identity(worktree)?;
    let marker_contents =
        validated_owner_marker(&worktree.path, &worktree.repo_root, &worktree.branch)?;
    remove_validated_managed_worktree_with(worktree, &marker_contents, || {
        remove_worktree(&worktree.repo_root, &worktree.path)
    })
}

fn restore_owner_marker(marker: &Path, contents: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .map_err(|error| format!("cannot restore owner marker {}: {error}", marker.display()))?;
    file.write_all(contents)
        .map_err(|error| format!("cannot restore owner marker {}: {error}", marker.display()))
}

#[cfg(test)]
fn remove_managed_worktree_with(
    worktree: &ManagedWorktree,
    remove: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    validated_worktree_identity(worktree)?;
    let marker_contents =
        validated_owner_marker(&worktree.path, &worktree.repo_root, &worktree.branch)?;
    remove_validated_managed_worktree_with(worktree, &marker_contents, remove)
}

fn remove_validated_managed_worktree_with(
    worktree: &ManagedWorktree,
    marker_contents: &[u8],
    remove: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let marker = owner_marker_path(&worktree.path);
    std::fs::remove_file(&marker)
        .map_err(|error| format!("cannot remove owner marker {}: {error}", marker.display()))?;
    match remove() {
        Ok(()) => Ok(()),
        Err(remove_error) => match validated_worktree_identity(worktree)
            .and_then(|()| restore_owner_marker(&marker, marker_contents))
        {
            Ok(()) => Err(remove_error),
            Err(restore_error) => Err(format!("{remove_error}; {restore_error}")),
        },
    }
}

/// Resolve one process CWD through macOS libproc. This is intentionally local
/// instead of `libproc::pidcwd`, which is not implemented on macOS.
#[derive(Debug)]
struct ProcessCwdError {
    message: String,
    errno: Option<i32>,
}

impl std::fmt::Display for ProcessCwdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl ProcessCwdError {
    fn proves_process_gone(&self) -> bool {
        self.errno == Some(libc::ESRCH)
    }
}

fn process_cwd(pid: i32) -> Result<PathBuf, ProcessCwdError> {
    use std::os::unix::ffi::OsStrExt;

    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let info_size =
        i32::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).map_err(|_| {
            ProcessCwdError {
                message: "proc_vnodepathinfo size does not fit c_int".to_string(),
                errno: None,
            }
        })?;
    // SAFETY: `info` points to an exactly-sized writable structure and
    // PROC_PIDVNODEPATHINFO initializes it on a full-size success.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            info_size,
        )
    };
    if read != info_size {
        let error = std::io::Error::last_os_error();
        return Err(ProcessCwdError {
            message: format!("cannot inspect cwd for pid {pid}: {error}"),
            // A short positive read is malformed but may leave a stale errno;
            // only the syscall's failure return can prove an ESRCH race.
            errno: (read <= 0).then(|| error.raw_os_error()).flatten(),
        });
    }
    // SAFETY: a full-size proc_pidinfo result initialized the structure.
    let info = unsafe { info.assume_init() };
    let path_storage = &info.pvi_cdir.vip_path;
    // SAFETY: `vip_path` is an inline MAXPATHLEN byte array represented by
    // libc as nested fixed arrays; flattening it preserves the exact bounds.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            path_storage.as_ptr().cast::<u8>(),
            std::mem::size_of_val(path_storage),
        )
    };
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| ProcessCwdError {
            message: format!("cwd for pid {pid} is not NUL-terminated"),
            errno: None,
        })?;
    if end == 0 {
        return Err(ProcessCwdError {
            message: format!("cwd for pid {pid} is empty"),
            errno: None,
        });
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end])))
}

fn process_probe_proves_gone(result: i32, errno: Option<i32>) -> bool {
    result != 0 && errno == Some(libc::ESRCH)
}

fn process_is_gone(pid: i32) -> bool {
    // SAFETY: signal 0 performs an existence/permission probe.
    let result = unsafe { libc::kill(pid, 0) };
    let errno = (result != 0)
        .then(|| std::io::Error::last_os_error().raw_os_error())
        .flatten();
    process_probe_proves_gone(result, errno)
}

fn direct_child_pids(parent: u32) -> Result<Vec<u32>, String> {
    let parent = i32::try_from(parent).map_err(|_| "parent PID does not fit pid_t".to_string())?;
    // `libproc`'s safe wrapper treats Darwin's zero-byte "no children" result
    // as an errno failure. Call the dedicated API directly so an empty leaf is
    // distinguishable from a real enumeration error.
    //
    // Unlike `proc_listpids`, `proc_listchildpids` already divides by
    // `sizeof(pid_t)`: both the size query and the fill return a pid COUNT,
    // never a byte count. Dividing again truncated one to three children to
    // zero (issue #255), which is what the "returns 0 children" note in
    // `ports.rs` actually observed.
    // SAFETY: the first call is the documented size query with a null buffer.
    let required = unsafe { libc::proc_listchildpids(parent, std::ptr::null_mut(), 0) };
    if required < 0 {
        return Err(format!(
            "cannot size child-process scan for {parent}: {}",
            std::io::Error::last_os_error()
        ));
    }
    if required == 0 {
        return Ok(Vec::new());
    }
    let item_size = std::mem::size_of::<u32>();
    let capacity = (required as usize).saturating_add(32);
    let mut children = vec![0u32; capacity];
    let buffer_size = i32::try_from(children.len().saturating_mul(item_size))
        .map_err(|_| "child-process buffer exceeds c_int".to_string())?;
    // SAFETY: `children` owns `buffer_size` writable bytes.
    let read =
        unsafe { libc::proc_listchildpids(parent, children.as_mut_ptr().cast(), buffer_size) };
    if read < 0 {
        return Err(format!(
            "cannot enumerate child processes for {parent}: {}",
            std::io::Error::last_os_error()
        ));
    }
    children.truncate(read as usize);
    children.retain(|pid| *pid > 1);
    Ok(children)
}

fn collect_descendant_pids(
    owner_pid: u32,
    relevant: &mut std::collections::HashSet<u32>,
) -> Result<(), String> {
    use std::collections::VecDeque;

    let mut queue = VecDeque::from([owner_pid]);
    while let Some(parent) = queue.pop_front() {
        for child in direct_child_pids(parent)? {
            if relevant.insert(child) {
                if relevant.len() > 8192 {
                    return Err("PaneFlow descendant scan exceeded 8192 processes".to_string());
                }
                queue.push_back(child);
            }
        }
    }
    Ok(())
}

fn protected_session_contains(
    session_id: i32,
    protected_sessions: &std::collections::HashSet<u32>,
) -> bool {
    session_id > 1 && protected_sessions.contains(&(session_id as u32))
}

const MAX_PROCESS_CWD_SCAN: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessCwdProbe {
    Required,
    BestEffort,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessCwdScanBudget {
    Proceed,
    SkipBestEffort,
}

/// Remaining work after [`GIT_DEADLINE`]: skip leftover same-UID PIDs, but
/// fail closed if a descendant or protected-session probe never ran.
fn process_cwd_scan_budget(
    deadline_at: Instant,
    required: bool,
) -> Result<ProcessCwdScanBudget, String> {
    if Instant::now() < deadline_at {
        return Ok(ProcessCwdScanBudget::Proceed);
    }
    if required {
        Err("process CWD scan exceeded its deadline".to_string())
    } else {
        Ok(ProcessCwdScanBudget::SkipBestEffort)
    }
}

fn process_cwd_probe(
    session_id: i32,
    effective_uid: u32,
    current_uid: u32,
    is_paneflow_descendant: bool,
    protected_sessions: &std::collections::HashSet<u32>,
) -> ProcessCwdProbe {
    if is_paneflow_descendant || protected_session_contains(session_id, protected_sessions) {
        ProcessCwdProbe::Required
    } else if effective_uid == current_uid {
        ProcessCwdProbe::BestEffort
    } else {
        ProcessCwdProbe::Skip
    }
}

/// A failed query for a previously relevant process may be ignored only when
/// the PID is proven gone or a protected-session PID is proven to have moved
/// to a different session. PaneFlow descendants remain required regardless of
/// session changes.
fn process_still_requires_cwd_probe(
    pid: i32,
    is_paneflow_descendant: bool,
    was_in_protected_session: bool,
    protected_sessions: &std::collections::HashSet<u32>,
) -> bool {
    if process_is_gone(pid) {
        return false;
    }
    if is_paneflow_descendant {
        return true;
    }
    if was_in_protected_session {
        // SAFETY: getsid is a read-only process query.
        let current_session = unsafe { libc::getsid(pid) };
        if current_session < 0 {
            return !process_is_gone(pid);
        }
        return protected_session_contains(current_session, protected_sessions);
    }
    false
}

/// Final destructive-operation gate for PaneFlow's process tree, known PTY
/// sessions, and accessible same-UID survivors. An existing shell can `cd`
/// after the GPUI-side live-terminal sample, and retirement can outlive the
/// terminal entity entirely, so the background worker independently scans all
/// processes. CWD failures are fatal only for authenticated session members or
/// PaneFlow descendants; unrelated inaccessible processes are ignored. The
/// scan is bounded by [`GIT_DEADLINE`]: leftover best-effort PIDs are skipped
/// when the budget expires, and the gate fails closed only if a required
/// descendant or protected-session probe could not complete.
pub(crate) fn worktree_has_live_process_cwd(
    worktree_path: &Path,
    protected_session_ids: &[u32],
) -> Result<bool, String> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;
    use libproc::processes::{ProcFilter, pids_by_type};
    use std::collections::{HashMap, HashSet};

    let deadline_at = Instant::now() + GIT_DEADLINE;
    let worktree_path = worktree_path
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize worktree before cwd scan: {error}"))?;
    let owner_pid = std::process::id();
    let mut relevant = HashSet::from([owner_pid]);
    collect_descendant_pids(owner_pid, &mut relevant)?;
    let protected_sessions: HashSet<_> = protected_session_ids
        .iter()
        .copied()
        .filter(|session_id| *session_id > 1 && *session_id <= i32::MAX as u32)
        .collect();
    // SAFETY: geteuid has no preconditions or side effects.
    let current_uid = unsafe { libc::geteuid() };
    let pids = pids_by_type(ProcFilter::All)
        .map_err(|error| format!("cannot enumerate processes before worktree removal: {error}"))?;
    if pids.len() > MAX_PROCESS_CWD_SCAN {
        return Err(format!(
            "process CWD scan exceeded {MAX_PROCESS_CWD_SCAN} processes"
        ));
    }
    let mut parents = HashMap::with_capacity(pids.len());
    let mut protected_processes = HashSet::new();
    let mut same_uid_candidates = HashSet::new();
    for pid in &pids {
        let pid = *pid;
        if pid <= 1 {
            continue;
        }
        let Ok(pid_i32) = i32::try_from(pid) else {
            continue;
        };
        // Classify session membership before the deadline skip: an
        // unclassified protected-session PID must not be treated as
        // best-effort leftover.
        // SAFETY: getsid is a read-only process query.
        let session_id = unsafe { libc::getsid(pid_i32) };
        if session_id < 0 {
            let session_error = std::io::Error::last_os_error();
            if session_error.raw_os_error() == Some(libc::ESRCH) {
                // The enumerated PID exited before its session query. This
                // syscall's own errno is the proof; a follow-up kill(0) can
                // still observe a zombie or an immediately reused PID.
                continue;
            }
            if relevant.contains(&pid) && !process_is_gone(pid_i32) {
                return Err(format!(
                    "cannot identify PaneFlow process session for pid {pid}: {session_error}"
                ));
            }
            continue;
        }
        let in_protected_session = protected_session_contains(session_id, &protected_sessions);
        if in_protected_session {
            protected_processes.insert(pid);
        }
        match process_cwd_scan_budget(deadline_at, relevant.contains(&pid) || in_protected_session)?
        {
            ProcessCwdScanBudget::SkipBestEffort => continue,
            ProcessCwdScanBudget::Proceed => {}
        }
        match pidinfo::<BSDInfo>(pid_i32, 0) {
            Ok(info) => {
                parents.insert(pid, info.pbi_ppid);
                match process_cwd_probe(
                    session_id,
                    info.pbi_uid,
                    current_uid,
                    relevant.contains(&pid),
                    &protected_sessions,
                ) {
                    ProcessCwdProbe::Required => {}
                    ProcessCwdProbe::BestEffort => {
                        same_uid_candidates.insert(pid);
                    }
                    ProcessCwdProbe::Skip => {}
                }
            }
            Err(error)
                if process_still_requires_cwd_probe(
                    pid_i32,
                    relevant.contains(&pid),
                    in_protected_session,
                    &protected_sessions,
                ) =>
            {
                return Err(format!("cannot identify PaneFlow process {pid}: {error}"));
            }
            Err(_) => continue,
        }
    }

    for pid in parents.keys().copied() {
        let mut cursor = pid;
        for _ in 0..256 {
            let Some(parent) = parents.get(&cursor).copied() else {
                break;
            };
            if parent == owner_pid {
                relevant.insert(pid);
                break;
            }
            if parent <= 1 || parent == cursor {
                break;
            }
            cursor = parent;
        }
    }
    // Close the window for children spawned while the same-UID snapshot was
    // being inspected. These PIDs are queried below even if BSDInfo was not
    // readable or they were absent from the earlier snapshot.
    collect_descendant_pids(owner_pid, &mut relevant)?;

    let mut candidates = same_uid_candidates;
    candidates.extend(relevant.iter().copied());
    candidates.extend(protected_processes.iter().copied());
    for pid in candidates {
        let Ok(pid) = i32::try_from(pid) else {
            continue;
        };
        let pid_u32 = pid as u32;
        let is_paneflow_descendant = relevant.contains(&pid_u32);
        let was_in_protected_session = protected_processes.contains(&pid_u32);
        let required = is_paneflow_descendant || was_in_protected_session;
        match process_cwd_scan_budget(deadline_at, required)? {
            ProcessCwdScanBudget::SkipBestEffort => continue,
            ProcessCwdScanBudget::Proceed => {}
        }
        match process_cwd(pid) {
            Ok(cwd) => {
                let cwd = match cwd.canonicalize() {
                    Ok(cwd) => cwd,
                    Err(error)
                        if required
                            && process_still_requires_cwd_probe(
                                pid,
                                is_paneflow_descendant,
                                was_in_protected_session,
                                &protected_sessions,
                            ) =>
                    {
                        return Err(format!(
                            "cannot canonicalize cwd for live pid {pid}: {error}"
                        ));
                    }
                    Err(_) => continue,
                };
                if cwd.starts_with(&worktree_path) {
                    return Ok(true);
                }
            }
            Err(error) => {
                if !error.proves_process_gone()
                    && required
                    && process_still_requires_cwd_probe(
                        pid,
                        is_paneflow_descendant,
                        was_in_protected_session,
                        &protected_sessions,
                    )
                {
                    return Err(error.to_string());
                }
            }
        }
    }
    Ok(false)
}

/// `git worktree prune` - drops references whose directory no longer exists.
/// Git-native guarantee: a worktree whose directory still exists is untouched
/// (US-009 AC5), so this is safe to run blindly at startup.
pub fn prune(repo_root: &Path) -> Result<(), String> {
    run_git(repo_root, &["worktree", "prune"], GIT_DEADLINE).map(|_| ())
}

/// Copy top-level `.env*` FILES from `src_root` into `dst_root`, skipping any
/// that already exist there (a tracked `.env.example` arrives via checkout -
/// don't clobber it). Best-effort by design (US-007): a missing source dir or
/// an unreadable entry yields an empty/partial copy, never an error. Returns
/// the file names copied.
///
/// Symlinks are never followed: `Path::is_file` / `std::fs::copy` would copy
/// a `.env` that points at `~/.ssh/id_rsa` into the new worktree as a regular
/// file. Source entries that are not regular files are skipped, and the
/// destination is created with `O_EXCL` so a planted dest symlink cannot be
/// written through.
pub fn copy_env_files(src_root: &Path, dst_root: &Path) -> Vec<String> {
    let entries = match std::fs::read_dir(src_root) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "failed to read env files from {}: {error}",
                    src_root.display()
                );
            }
            return Vec::new();
        }
    };
    let mut copied = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        "failed to read env file entry from {}: {error}",
                        src_root.display()
                    );
                }
                continue;
            }
        };
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if !name_s.starts_with(".env") {
            continue;
        }
        let src = entry.path();
        let src_meta = match std::fs::symlink_metadata(&src) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!("failed to inspect env file {}: {error}", src.display());
                continue;
            }
        };
        if !src_meta.file_type().is_file() {
            continue;
        }
        let dst = dst_root.join(&name);
        // `Path::exists` follows dest symlinks; a dangling dest link would
        // look absent and `std::fs::copy` would create the pointee.
        if std::fs::symlink_metadata(&dst).is_ok() {
            continue;
        }
        match copy_env_file_no_follow(&src, &dst) {
            Ok(()) => copied.push(name_s.into_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => tracing::warn!(
                "failed to copy env file from {} to {}: {error}",
                src.display(),
                dst.display()
            ),
        }
    }
    copied.sort();
    copied
}

/// Copy `src` onto a newly created regular `dst`. Source is opened with
/// `O_NOFOLLOW` and dest with `O_EXCL` so neither name can be a symlink.
fn copy_env_file_no_follow(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::io::{self, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut src_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(src)?;
    let permissions = src_file.metadata()?.permissions();
    let mut dst_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(permissions.mode())
        .custom_flags(libc::O_NOFOLLOW)
        .open(dst)?;
    // Apply the source mode before any secret bytes land, so a 0600
    // `.env` is never world-readable for the duration of `io::copy`.
    dst_file.set_permissions(permissions)?;
    io::copy(&mut src_file, &mut dst_file)?;
    dst_file.flush()?;
    Ok(())
}

/// Tear down a batch of managed worktrees (blocking - run via `smol::unblock`
/// on the app side). Per entry: `Keep` policy → skip; dirty or unverifiable →
/// keep + warn (NEVER remove what might hold work); clean → remove. The
/// branch is never touched.
pub fn teardown_all(worktrees: Vec<ManagedWorktree>, protected_session_ids: Vec<u32>) {
    for wt in worktrees {
        if wt.teardown == TeardownPolicy::Keep {
            continue;
        }
        if !wt.path.exists() {
            // Directory already gone (user rm -rf'd it): just prune the ref.
            let _ = prune(&wt.repo_root);
            continue;
        }
        if let Err(error) = validated_worktree_identity(&wt) {
            log::warn!(
                "worktree kept: invalid directory identity in {}: {error}",
                wt.path.display(),
            );
            continue;
        }
        if let Err(error) = validated_owner_marker(&wt.path, &wt.repo_root, &wt.branch) {
            log::warn!(
                "worktree kept: invalid Paneflow owner marker in {}: {error}",
                wt.path.display(),
            );
            continue;
        }
        match is_clean(&wt.path) {
            Ok(true) => match remove_managed_worktree(&wt, &protected_session_ids) {
                Ok(()) => log::info!("worktree removed: {}", wt.path.display()),
                Err(e) => log::warn!("worktree kept ({}): {e}", wt.path.display()),
            },
            Ok(false) => log::warn!(
                "worktree kept: uncommitted changes in {}",
                wt.path.display()
            ),
            Err(e) => log::warn!(
                "worktree kept (cannot verify cleanliness): {} - {e}",
                wt.path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_probe::source_slice;
    use tracing_test::traced_test;

    #[test]
    fn run_git_sets_git_terminal_prompt_off() {
        let source = include_str!("worktree.rs");
        let run_git_body =
            source_slice(source, "fn run_git(", "/// `git worktree list --porcelain`");

        assert!(
            run_git_body.contains(".env(\"GIT_TERMINAL_PROMPT\", \"0\")"),
            "worktree git commands must fail instead of opening an interactive credential prompt"
        );
    }

    const GIT_ISOLATION_PRELUDE: &[&str] = &[
        "core.fsmonitor=",
        "core.hooksPath=/dev/null",
        "diff.external=",
        "GIT_CONFIG_NOSYSTEM",
        "env_remove(\"GIT_DIR\")",
        "env_remove(\"GIT_WORK_TREE\")",
        "env_remove(\"GIT_SSH_COMMAND\")",
    ];

    fn git_fn_source<'a>(source: &'a str, marker: &str) -> &'a str {
        let after = source.split_once(marker).expect("git spawn helper").1;
        after.get(..after.len().min(2000)).unwrap_or(after)
    }

    fn source_has_git_isolation_prelude(source: &str) -> bool {
        GIT_ISOLATION_PRELUDE
            .iter()
            .all(|needle| source.contains(needle))
    }

    fn assert_production_git_command_isolated(source: &str, marker: &str) {
        let run_git_src = git_fn_source(source, marker);
        if source_has_git_isolation_prelude(run_git_src) {
            return;
        }
        assert!(
            run_git_src.contains("git_command()"),
            "production git Command in `{marker}` must carry the isolation prelude or call git_command()"
        );
        // Scan only the production `git_command()` body: the whole file also
        // holds this test module, whose needle list would satisfy every check.
        let git_command_body = source_slice(
            include_str!("worktree.rs"),
            "fn git_command() -> Command {",
            "fn git_subcommand(",
        );
        assert!(
            source_has_git_isolation_prelude(git_command_body),
            "git_command() helper must pass -c core.fsmonitor= -c core.hooksPath=/dev/null \
             -c diff.external=, GIT_CONFIG_NOSYSTEM, and env_remove GIT_DIR/GIT_WORK_TREE/GIT_SSH_COMMAND"
        );
    }

    #[test]
    fn git_run_disables_repo_hooks() {
        assert_production_git_command_isolated(include_str!("worktree.rs"), "fn run_git(");
        assert_production_git_command_isolated(include_str!("../diff/git.rs"), "fn run_git_timed(");
        assert_production_git_command_isolated(
            include_str!("../diff/git.rs"),
            "fn run_git_stdin_timed(",
        );
        assert_production_git_command_isolated(include_str!("git.rs"), "fn git_stdout(");
        assert_production_git_command_isolated(
            include_str!("../app/diff_dock/branch.rs"),
            "fn list_branches(",
        );
        assert_production_git_command_isolated(
            include_str!("../app/diff_dock/branch.rs"),
            "fn switch_branch(",
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        run_git(&repo_root, &["init"], GIT_DEADLINE).expect("git init");
        run_git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
            GIT_DEADLINE,
        )
        .expect("git config email");
        run_git(
            &repo_root,
            &["config", "user.name", "PaneFlow Tests"],
            GIT_DEADLINE,
        )
        .expect("git config name");
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        run_git(&repo_root, &["add", "."], GIT_DEADLINE).expect("git add");
        run_git(&repo_root, &["commit", "-m", "fixture"], GIT_DEADLINE).expect("git commit");

        let marker = tmp.path().join("HOOK_RAN");
        let marker_script = tmp.path().join("marker.sh");
        std::fs::write(
            &marker_script,
            format!(
                "#!/bin/sh\nprintf 'ran\\n' >> '{}'\nexit 0\n",
                marker.display()
            ),
        )
        .expect("marker script");
        let mut permissions = std::fs::metadata(&marker_script)
            .expect("marker metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&marker_script, permissions).expect("chmod marker");

        let hooks_dir = repo_root.join("hostile-hooks");
        std::fs::create_dir_all(&hooks_dir).expect("hooks dir");
        std::fs::copy(&marker_script, hooks_dir.join("post-checkout")).expect("post-checkout hook");
        let mut hook_permissions = std::fs::metadata(hooks_dir.join("post-checkout"))
            .expect("hook metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut hook_permissions, 0o755);
        std::fs::set_permissions(hooks_dir.join("post-checkout"), hook_permissions)
            .expect("chmod hook");

        let hooks_dir_s = hooks_dir.to_string_lossy();
        let marker_script_s = marker_script.to_string_lossy();
        run_git(
            &repo_root,
            &["config", "core.hooksPath", &hooks_dir_s],
            GIT_DEADLINE,
        )
        .expect("config hooksPath");
        run_git(
            &repo_root,
            &["config", "core.fsmonitor", &marker_script_s],
            GIT_DEADLINE,
        )
        .expect("config fsmonitor");
        run_git(
            &repo_root,
            &["config", "diff.external", &marker_script_s],
            GIT_DEADLINE,
        )
        .expect("config diff.external");

        std::fs::write(repo_root.join("README.md"), "changed\n").expect("dirty worktree");

        let listed = list_worktrees(&repo_root).expect("list worktrees");
        assert!(!listed.is_empty(), "hostile repo must still list");
        is_clean(&repo_root).expect("git status against hostile hooksPath/fsmonitor");
        let diff = crate::diff::compute_head_diff(&repo_root);
        assert!(
            diff.error.is_none(),
            "git diff against hostile diff.external: {:?}",
            diff.error
        );
        let branch = "feat/hostile-hooks";
        let path = worktree_dir(&repo_root, branch);
        add_worktree(&repo_root, &path, branch, true).expect("worktree add");

        assert!(
            !marker.exists(),
            "repo core.hooksPath/core.fsmonitor/diff.external must not run: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
    }

    #[test]
    fn same_workspace_duplicate_records_preserve_keep_policy() {
        let record = ManagedWorktree {
            path: PathBuf::from("/tmp/repo.worktrees/feature"),
            repo_root: PathBuf::from("/tmp/repo"),
            branch: "feature".to_string(),
            teardown: TeardownPolicy::Auto,
            identity: None,
        };
        let mut keep = record.clone();
        keep.teardown = TeardownPolicy::Keep;

        let merged = merge_managed_worktree_records(vec![record, keep]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].teardown, TeardownPolicy::Keep);
    }

    #[test]
    fn zero_birthtime_directory_identity_is_refused() {
        let path = Path::new("/tmp/repo.worktrees/feature");
        assert!(
            WorktreeIdentity::from_parts(path, 1, 2, 0, 0).is_err(),
            "a volume without birth time cannot distinguish a recreated directory"
        );
        assert!(
            WorktreeIdentity::from_parts(path, 1, 2, 0, 1).is_ok(),
            "a non-zero birth time still yields an identity"
        );
        assert!(
            WorktreeIdentity::from_persisted("1:2:0:0").is_none(),
            "a 0/0 birth time is only dev/inode, so it must not restore ownership"
        );
        assert!(
            WorktreeIdentity::from_persisted("1:2:3:4").is_some(),
            "an identity with a real birth time is still accepted"
        );
    }

    #[test]
    fn merge_managed_worktree_records_conflicting_identity_fails_closed_to_keep() {
        let first = ManagedWorktree {
            path: PathBuf::from("/tmp/repo.worktrees/feature"),
            repo_root: PathBuf::from("/tmp/repo"),
            branch: "feature".to_string(),
            teardown: TeardownPolicy::Auto,
            identity: Some(WorktreeIdentity("1:2:3:4".to_string())),
        };
        let mut second = first.clone();
        second.identity = Some(WorktreeIdentity("5:6:7:8".to_string()));

        let merged = merge_managed_worktree_records(vec![first, second]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].teardown, TeardownPolicy::Keep);
    }

    #[test]
    fn merge_managed_worktree_records_conflicting_ownership_metadata_fails_closed_to_keep() {
        let first = ManagedWorktree {
            path: PathBuf::from("/tmp/repo.worktrees/feature"),
            repo_root: PathBuf::from("/tmp/repo"),
            branch: "feature".to_string(),
            teardown: TeardownPolicy::Auto,
            identity: Some(WorktreeIdentity("1:2:3:4".to_string())),
        };

        let mut other_root = first.clone();
        other_root.repo_root = PathBuf::from("/tmp/other-repo");
        let merged_root = merge_managed_worktree_records(vec![first.clone(), other_root]);
        assert_eq!(merged_root.len(), 1);
        assert_eq!(merged_root[0].teardown, TeardownPolicy::Keep);

        let mut other_branch = first.clone();
        other_branch.branch = "other-feature".to_string();
        let merged_branch = merge_managed_worktree_records(vec![first, other_branch]);
        assert_eq!(merged_branch.len(), 1);
        assert_eq!(merged_branch[0].teardown, TeardownPolicy::Keep);
    }

    #[test]
    fn merge_preserves_more_than_the_layout_pane_cap() {
        let records: Vec<_> = (0..40)
            .map(|index| ManagedWorktree {
                path: PathBuf::from(format!("/tmp/repo.worktrees/feature-{index}")),
                repo_root: PathBuf::from("/tmp/repo"),
                branch: format!("feature-{index}"),
                teardown: TeardownPolicy::Auto,
                identity: None,
            })
            .collect();

        assert_eq!(merge_managed_worktree_records(records).len(), 40);
    }

    #[test]
    fn only_esrch_proves_an_unreadable_process_is_gone() {
        assert!(process_probe_proves_gone(-1, Some(libc::ESRCH)));
        assert!(!process_probe_proves_gone(-1, Some(libc::EPERM)));
        assert!(!process_probe_proves_gone(0, None));
    }

    #[test]
    fn nonleader_pid_is_protected_by_its_session_identity() {
        let member_pid = 200;
        let session_id = 100;
        assert_ne!(member_pid, session_id);
        assert!(protected_session_contains(
            session_id,
            &std::collections::HashSet::from([session_id as u32]),
        ));
    }

    #[test]
    fn protected_session_pid_is_selected_regardless_of_uid() {
        let protected_sessions = std::collections::HashSet::from([100]);

        assert_eq!(
            process_cwd_probe(100, 0, 501, false, &protected_sessions),
            ProcessCwdProbe::Required,
            "a setuid/sudo session member must not be filtered out by effective UID"
        );
        assert_eq!(
            process_cwd_probe(200, 501, 501, false, &protected_sessions),
            ProcessCwdProbe::BestEffort,
            "same-UID survivors remain accessible-CWD candidates without a terminal entity"
        );
        assert_eq!(
            process_cwd_probe(200, 0, 501, false, &protected_sessions),
            ProcessCwdProbe::Skip,
            "an unrelated different-UID process must not be probed"
        );
    }

    #[test]
    fn worktree_has_live_process_cwd_respects_deadline() {
        let source = include_str!("worktree.rs");
        // Issue #219: the old `.unwrap_or(scan)` fallback silently widened
        // the region to end-of-file (this test module included) if the end
        // anchor ever moved; a missing anchor now panics instead.
        let scan = source_slice(
            source,
            "fn worktree_has_live_process_cwd(",
            "\npub fn prune(",
        );

        assert!(
            scan.contains("GIT_DEADLINE") || scan.contains("deadline"),
            "process CWD scan must take a deadline argument or reuse a deadline constant"
        );
        assert!(
            scan.contains("Instant::now()") || scan.contains("deadline_at"),
            "process CWD scan must observe a wall-clock deadline, not only a process-count cap"
        );
        let budget_hits = scan.matches("process_cwd_scan_budget(").count();
        assert!(
            budget_hits >= 2,
            "enumeration and CWD probes must both consult the deadline, found {budget_hits}"
        );
        let first_loop = source_slice(scan, "for pid in &pids", "for pid in parents.keys()");
        let getsid_at = first_loop
            .find("libc::getsid")
            .expect("getsid in first PID loop");
        let budget_at = first_loop
            .find("process_cwd_scan_budget(")
            .expect("budget in first PID loop");
        assert!(
            getsid_at < budget_at,
            "session membership must be classified before the deadline skip"
        );
        assert!(
            first_loop.contains("in_protected_session"),
            "deadline skip must treat protected-session PIDs as required: {first_loop}"
        );

        let expired = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            process_cwd_scan_budget(expired, false),
            Ok(ProcessCwdScanBudget::SkipBestEffort),
            "expired budget must skip leftover best-effort PIDs"
        );
        let required = process_cwd_scan_budget(expired, true);
        assert!(
            required
                .as_ref()
                .is_err_and(|error| error.contains("deadline")),
            "expired budget must fail closed if a required probe never ran: {required:?}"
        );
        let live = Instant::now() + GIT_DEADLINE;
        assert_eq!(
            process_cwd_scan_budget(live, true),
            Ok(ProcessCwdScanBudget::Proceed)
        );
        assert_eq!(
            process_cwd_scan_budget(live, false),
            Ok(ProcessCwdScanBudget::Proceed)
        );
    }

    #[test]
    fn cwd_scan_finds_same_uid_survivor_after_terminal_entity_is_gone() {
        use libproc::libproc::bsd_info::BSDInfo;
        use libproc::libproc::proc_pid::pidinfo;
        use std::io::BufRead;
        use std::process::Stdio;

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut launcher = Command::new("/bin/sh")
            .args(["-c", "/bin/sleep 30 </dev/null >/dev/null 2>&1 & echo $!"])
            .current_dir(tmp.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn terminal launcher fixture");
        let launcher_pid = launcher.id();
        let mut pid_line = String::new();
        std::io::BufReader::new(launcher.stdout.take().expect("launcher stdout"))
            .read_line(&mut pid_line)
            .expect("read survivor pid");
        let survivor_pid: i32 = pid_line.trim().parse().expect("numeric survivor pid");
        assert!(launcher.wait().expect("reap launcher").success());

        struct ProcessCleanup(i32);
        impl Drop for ProcessCleanup {
            fn drop(&mut self) {
                // SAFETY: the fixture PID is positive and SIGKILL is used only
                // to ensure a failed assertion cannot leak the test process.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                }
            }
        }
        let _cleanup = ProcessCleanup(survivor_pid);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let info = pidinfo::<BSDInfo>(survivor_pid, 0).expect("survivor identity");
            if info.pbi_ppid != launcher_pid {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "survivor was not reparented after its terminal entity disappeared"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            worktree_has_live_process_cwd(tmp.path(), &[]),
            Ok(true),
            "an accessible same-UID survivor must block retirement even after its terminal entity is gone"
        );
    }

    // Issue #255: `proc_listchildpids` returns a pid COUNT, not a byte
    // count. Dividing it by `size_of::<u32>()` truncated one to three live
    // children down to zero, so the post-snapshot descendant re-scan in
    // `worktree_has_live_process_cwd` never saw a freshly spawned child.
    #[test]
    fn direct_child_pids_sees_a_single_live_child() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        let child_pid = child.id();
        // Let the kernel register the new process before we enumerate.
        std::thread::sleep(Duration::from_millis(250));
        let children = direct_child_pids(std::process::id());
        let _ = child.kill();
        let _ = child.wait();

        let children = children.expect("enumerate direct children");
        assert!(
            children.contains(&child_pid),
            "direct_child_pids must list live child {child_pid}; got {children:?}"
        );
    }

    #[test]
    fn branch_slug_is_filesystem_safe() {
        assert_eq!(
            branch_slug("feat/cli-orchestration"),
            "feat-cli-orchestration"
        );
        assert_eq!(branch_slug("fix/US-006_teardown"), "fix-US-006_teardown");
        assert_eq!(branch_slug("a b\\c:d"), "a-b-c-d");
        // Leading/trailing separators are trimmed so the dir never hides.
        assert_eq!(branch_slug("/weird/"), "weird");
        assert_eq!(branch_slug(".hidden"), "hidden");
        // Inner dots survive (version-style branches stay readable).
        assert_eq!(branch_slug("release/v1.2.3"), "release-v1.2.3");
    }

    #[test]
    fn branch_slug_neutralizes_dot_only_traversal() {
        // NFR (orchestration-v2): the slug is the one untrusted component of
        // a destructive path - `.`/`..` must never survive as a path segment.
        assert_eq!(branch_slug(".."), "");
        assert_eq!(branch_slug("."), "");
        assert_eq!(branch_slug("..."), "");
        assert_eq!(branch_slug("-..-"), "");
    }

    #[test]
    fn worktree_dir_never_escapes_the_worktrees_dir() {
        // Defense in depth below spec validation: even a dot-only branch maps
        // INSIDE `<repo>.worktrees/` (fallback slug), never to its parent.
        let dir = worktree_dir(Path::new("/home/a/dev/paneflow"), "..");
        assert_eq!(dir, PathBuf::from("/home/a/dev/paneflow.worktrees/branch"));
    }

    fn entry(path: PathBuf, branch: Option<&str>) -> WorktreeEntry {
        WorktreeEntry {
            path,
            branch: branch.map(str::to_string),
            sha: None,
            is_bare: false,
        }
    }

    #[test]
    fn a_branch_already_checked_out_is_reused_never_recreated() {
        let repo = Path::new("/home/a/dev/paneflow");
        let entries = vec![
            entry(repo.to_path_buf(), Some("main")),
            entry(
                PathBuf::from("/home/a/dev/paneflow.worktrees/feat-x"),
                Some("feat/x"),
            ),
        ];
        // git refuses a second worktree on one branch, so the only workable
        // answer for an already-checked-out branch is the checkout it is in -
        // including the repository's own, which is how selecting `main` unbinds.
        assert_eq!(
            plan_branch_checkout(&entries, repo, "feat/x"),
            Ok(BranchCheckout::Existing(PathBuf::from(
                "/home/a/dev/paneflow.worktrees/feat-x"
            )))
        );
        assert_eq!(
            plan_branch_checkout(&entries, repo, "main"),
            Ok(BranchCheckout::Existing(repo.to_path_buf()))
        );
        assert_eq!(
            plan_branch_checkout(&entries, repo, "chore/rust-1.98"),
            Ok(BranchCheckout::Create(PathBuf::from(
                "/home/a/dev/paneflow.worktrees/chore-rust-1.98"
            )))
        );
    }

    #[test]
    fn a_slug_collision_falls_back_to_the_hashed_dir() {
        let repo = Path::new("/home/a/dev/paneflow");
        // `feat/x` and `feat-x` slugify the same; the second one asked for
        // takes the hashed path rather than the occupied one.
        let entries = vec![entry(worktree_dir(repo, "feat/x"), Some("feat/x"))];
        assert_eq!(
            plan_branch_checkout(&entries, repo, "feat-x"),
            Ok(BranchCheckout::Create(worktree_dir_hashed(repo, "feat-x")))
        );
    }

    #[test]
    fn a_registered_checkout_on_the_target_path_is_refused_not_overwritten() {
        let repo = Path::new("/home/a/dev/paneflow");
        // Both candidate paths are taken by checkouts of something else (a
        // detached HEAD names no branch, so neither matches by branch).
        let entries = vec![
            entry(worktree_dir(repo, "feat/x"), None),
            entry(worktree_dir_hashed(repo, "feat/x"), None),
        ];
        let planned = plan_branch_checkout(&entries, repo, "feat/x");
        assert!(
            planned.is_err(),
            "a registered checkout on the target path must never be written over: {planned:?}"
        );
    }

    #[test]
    fn a_detached_checkout_is_named_by_what_distinguishes_it() {
        let repo = Path::new("/home/u/dev/paneflow");
        assert_eq!(
            checkout_label(Some("feat/login"), Path::new("/wt/feat-login"), repo),
            "feat/login"
        );
        // The layout agent tooling produces: every worktree directory is named
        // after the repository, so the last component says nothing.
        assert_eq!(
            checkout_label(
                None,
                Path::new("/home/u/dev/worktrees/paneflow/poplar-plume/paneflow"),
                repo
            ),
            "poplar-plume"
        );
        // A directory that already differs is kept as-is.
        assert_eq!(
            checkout_label(None, Path::new("/wt/hotfix-42"), repo),
            "hotfix-42"
        );
        // An empty branch is a detached HEAD, not a branch named "".
        assert_eq!(
            checkout_label(Some(""), Path::new("/wt/hotfix-42"), repo),
            "hotfix-42"
        );
    }

    /// Issue #347, against a real repository: a checkout made for a picked
    /// branch is registered with git, reused on the next pick, resolves the
    /// repository's own branch to the repository root, and carries no owner
    /// marker.
    #[test]
    fn prepare_branch_checkout_reuses_an_existing_worktree_and_owns_nothing() {
        let sandbox = tempfile::tempdir().expect("tempdir");
        let repo = sandbox.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
            vec!["branch", "feat/x"],
        ] {
            run_git(&repo, &args, GIT_DEADLINE).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        }
        // `git worktree list` prints canonical paths (`/private/var` on macOS).
        let repo = std::fs::canonicalize(&repo).expect("canonical repo");

        let first = prepare_branch_checkout(&repo, "feat/x").expect("first pick checks out");
        assert_eq!(first, worktree_dir(&repo, "feat/x"));
        assert!(first.is_dir(), "the checkout exists at {}", first.display());
        let entries = list_worktrees(&repo).expect("worktree list");
        assert!(
            entries
                .iter()
                .any(|e| e.path == first && e.branch.as_deref() == Some("feat/x")),
            "git must register the checkout on its branch: {entries:?}"
        );
        assert!(
            !owner_marker_path(&first).exists(),
            "a palette checkout is the user's, not a ManagedWorktree"
        );

        let again = prepare_branch_checkout(&repo, "feat/x").expect("second pick reuses");
        assert_eq!(again, first, "the same branch reuses its checkout");
        assert_eq!(
            list_worktrees(&repo).expect("worktree list").len(),
            entries.len(),
            "reusing must not add a worktree"
        );

        // The repository's own branch resolves to the repository root: that is
        // how picking it unbinds a tab rather than duplicating the checkout.
        assert_eq!(
            prepare_branch_checkout(&repo, "main").expect("main resolves"),
            repo
        );
    }

    #[test]
    fn a_palette_checkout_is_not_managed_and_carries_no_owner_marker() {
        // Issue #347: `prepare_branch_checkout` must never route through
        // `add_worktree`, which writes the owner marker that makes a checkout
        // adoptable as a ManagedWorktree - and therefore removable at close.
        let src = include_str!("worktree.rs");
        let body = src
            .split("pub fn prepare_branch_checkout(")
            .nth(1)
            .and_then(|rest| rest.split("fn git_worktree_add(").next())
            .expect("prepare_branch_checkout body");
        assert!(
            body.contains("git_worktree_add(repo_root, &path, branch, false)"),
            "the palette checkout must use the marker-less add"
        );
        assert!(
            !body.contains("add_worktree(") && !body.contains("write_owner_marker("),
            "the palette checkout must not claim lifecycle ownership"
        );
        assert!(
            !body.contains("ManagedWorktree {"),
            "the palette checkout must not build an ownership record"
        );
    }

    #[test]
    fn worktree_dir_is_a_sibling_of_the_repo() {
        let dir = worktree_dir(Path::new("/home/a/dev/paneflow"), "feat/x");
        assert_eq!(dir, PathBuf::from("/home/a/dev/paneflow.worktrees/feat-x"));
        // NOT inside the repo: recursive watchers must not see it.
        assert!(!dir.starts_with("/home/a/dev/paneflow/"));
    }

    #[test]
    fn hashed_worktree_dir_disambiguates_slug_collisions() {
        let repo = Path::new("/home/a/dev/paneflow");
        let a = "feat/a b";
        let b = "feat/a-b";
        assert_eq!(branch_slug(a), branch_slug(b));
        assert_eq!(worktree_dir(repo, a), worktree_dir(repo, b));

        let hashed_a = worktree_dir_hashed(repo, a);
        let hashed_b = worktree_dir_hashed(repo, b);
        assert_ne!(hashed_a, hashed_b);
        assert!(is_paneflow_worktree_dir(repo, a, &hashed_a));
        assert!(is_paneflow_worktree_dir(repo, b, &hashed_b));
        assert!(!hashed_a.starts_with("/home/a/dev/paneflow/"));
    }

    #[test]
    fn parses_worktree_porcelain_with_detached_and_branches() {
        let out = "worktree /home/a/dev/repo\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /home/a/dev/repo.worktrees/feat-x\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/feat/x\n\nworktree /tmp/detached\nHEAD 3333333333333333333333333333333333333333\ndetached\n";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(
            entries[1].path,
            PathBuf::from("/home/a/dev/repo.worktrees/feat-x")
        );
        assert_eq!(entries[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(entries[2].branch, None, "detached HEAD has no branch");
        assert_eq!(
            entries[2].sha.as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
        assert!(!entries[2].is_bare);
    }

    #[test]
    fn parse_worktree_porcelain_keeps_headless_and_bare() {
        let out = "worktree /repo/bare\nbare\n\nworktree /repo/no-head\nlocked\n";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("/repo/bare"));
        assert!(entries[0].is_bare);
        assert_eq!(entries[0].sha, None);
        assert_eq!(entries[0].branch, None);
        assert_eq!(entries[1].path, PathBuf::from("/repo/no-head"));
        assert!(!entries[1].is_bare);
        assert_eq!(entries[1].sha, None);
        assert_eq!(entries[1].branch, None);
    }

    #[test]
    fn parse_worktree_porcelain_handles_missing_trailing_blank() {
        let out = "worktree /r\nbranch refs/heads/main";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn managed_status_exempts_only_the_owner_marker() {
        assert!(managed_status_is_clean(""));
        assert!(managed_status_is_clean("?? .paneflow-worktree"));
        assert!(managed_status_is_clean("!! .paneflow-worktree"));

        for dirty in [
            "?? notes.txt",
            "!! .env",
            " M tracked.txt",
            " M .paneflow-worktree",
            "?? .paneflow-worktree\n?? notes.txt",
            "!! .paneflow-worktree\n!! .env",
        ] {
            assert!(
                !managed_status_is_clean(dirty),
                "every non-marker status entry must fail closed: {dirty:?}"
            );
        }
    }

    #[test]
    fn teardown_removes_marker_only_worktree_but_keeps_ignored_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        run_git(&repo_root, &["init"], GIT_DEADLINE).expect("git init");
        run_git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
            GIT_DEADLINE,
        )
        .expect("git config email");
        run_git(
            &repo_root,
            &["config", "user.name", "PaneFlow Tests"],
            GIT_DEADLINE,
        )
        .expect("git config name");
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        std::fs::write(repo_root.join(".gitignore"), ".env\n").expect("ignore file");
        run_git(&repo_root, &["add", "."], GIT_DEADLINE).expect("git add");
        run_git(&repo_root, &["commit", "-m", "fixture"], GIT_DEADLINE).expect("git commit");

        let branch = "feat/managed-cleanup";
        let path = worktree_dir(&repo_root, branch);
        add_worktree(&repo_root, &path, branch, true).expect("add managed worktree");
        let managed = ManagedWorktree {
            path: path.clone(),
            repo_root: repo_root.clone(),
            branch: branch.to_string(),
            teardown: TeardownPolicy::Auto,
            identity: worktree_identity(&path).ok(),
        };

        let valid_marker = std::fs::read(owner_marker_path(&path)).expect("valid owner marker");
        let valid_marker_text = String::from_utf8(valid_marker.clone()).expect("UTF-8 marker");
        let repo_line = valid_marker_text
            .lines()
            .find(|line| line.starts_with("repo_root="))
            .expect("repo-root marker field");
        let invalid_markers = [
            valid_marker_text.replacen("owner=paneflow", "owner=somebody-else", 1),
            valid_marker_text.replacen(repo_line, "repo_root=/wrong", 1),
            valid_marker_text.replacen(
                "branch=feat/managed-cleanup",
                "branch=feat/somebody-else",
                1,
            ),
            "owner=paneflow\nmalformed\n".to_string(),
            "x".repeat(MAX_OWNER_MARKER_BYTES as usize + 1),
        ];
        for invalid in &invalid_markers {
            std::fs::write(owner_marker_path(&path), invalid).expect("invalid marker fixture");
            teardown_all(vec![managed.clone()], Vec::new());
            assert!(
                path.exists(),
                "an invalid marker must block teardown: {invalid:?}"
            );
            assert_eq!(
                std::fs::read(owner_marker_path(&path)).expect("preserved invalid marker"),
                invalid.as_bytes(),
                "a rejected marker must survive byte-for-byte"
            );
        }
        std::fs::write(owner_marker_path(&path), valid_marker).expect("restore valid marker");

        std::fs::write(path.join(".env"), "SECRET=test\n").expect("ignored env file");
        assert!(
            !is_clean(&path).expect("status with ignored file"),
            "an ignored file must make managed teardown fail closed"
        );
        teardown_all(vec![managed.clone()], Vec::new());
        assert!(path.exists(), "ignored user data must keep the worktree");
        assert!(path.join(".env").exists(), "ignored user data must survive");

        std::fs::remove_file(path.join(".env")).expect("remove ignored fixture");
        assert!(
            is_clean(&path).expect("marker-only status"),
            "the sole Paneflow marker is teardown metadata, not user data"
        );
        teardown_all(vec![managed], Vec::new());
        assert!(
            !path.exists(),
            "marker removal must let git worktree remove actually succeed"
        );
        assert!(
            branch_exists(&repo_root, branch),
            "managed teardown must never delete the branch"
        );
    }

    #[test]
    fn teardown_rechecks_live_process_cwd_immediately_before_removal() {
        use std::io::BufRead;
        use std::process::Stdio;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        run_git(&repo_root, &["init"], GIT_DEADLINE).expect("git init");
        run_git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
            GIT_DEADLINE,
        )
        .expect("git config email");
        run_git(
            &repo_root,
            &["config", "user.name", "PaneFlow Tests"],
            GIT_DEADLINE,
        )
        .expect("git config name");
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        run_git(&repo_root, &["add", "."], GIT_DEADLINE).expect("git add");
        run_git(&repo_root, &["commit", "-m", "fixture"], GIT_DEADLINE).expect("git commit");

        let branch = "feat/live-cwd";
        let path = worktree_dir(&repo_root, branch);
        add_worktree(&repo_root, &path, branch, true).expect("add managed worktree");
        let managed = ManagedWorktree {
            path: path.clone(),
            repo_root,
            branch: branch.to_string(),
            teardown: TeardownPolicy::Auto,
            identity: worktree_identity(&path).ok(),
        };

        struct ChildCleanup(std::process::Child);
        impl Drop for ChildCleanup {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Command::new("/bin/sh")
            .args(["-c", "echo ready; exec sleep 30"])
            .current_dir(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn process in managed worktree");
        let mut child = ChildCleanup(child);
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().expect("child stdout"))
            .read_line(&mut ready)
            .expect("read child readiness");
        assert_eq!(ready.trim_end(), "ready");

        teardown_all(vec![managed.clone()], Vec::new());
        assert!(
            path.exists(),
            "worker-side CWD revalidation must keep a checkout entered after the UI sample"
        );
        child.0.kill().expect("stop live-cwd fixture");
        child.0.wait().expect("reap live-cwd fixture");

        let final_scan = worktree_has_live_process_cwd(&path, &[]);
        assert_eq!(
            final_scan,
            Ok(false),
            "fixture exit should clear the worker-side CWD gate"
        );
        teardown_all(vec![managed], Vec::new());
        assert!(
            !path.exists(),
            "the same clean checkout is removable after its live CWD user exits"
        );
    }

    #[test]
    fn failed_worktree_removal_restores_exact_owner_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir(&repo_root).expect("repo root");
        let path = tmp.path().join("managed-worktree");
        std::fs::create_dir(&path).expect("worktree path");
        let marker = owner_marker_path(&path);
        write_owner_marker(&path, &repo_root, "feat/exact").expect("owner marker");
        let marker_bytes = std::fs::read(&marker).expect("exact owner marker bytes");
        let managed = ManagedWorktree {
            path: path.clone(),
            // This directory exists (so marker validation succeeds) but is not
            // a git repository, forcing removal to fail after marker unlink.
            repo_root,
            branch: "feat/exact".to_string(),
            teardown: TeardownPolicy::Auto,
            identity: worktree_identity(&path).ok(),
        };

        assert!(remove_managed_worktree(&managed, &[]).is_err());
        assert_eq!(
            std::fs::read(marker).expect("restored owner marker"),
            marker_bytes,
            "a failed git removal must preserve ownership evidence byte-for-byte"
        );
    }

    #[test]
    fn failed_removal_never_overwrites_a_replacement_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir(&repo_root).expect("repo root");
        let path = tmp.path().join("managed-worktree");
        std::fs::create_dir(&path).expect("worktree path");
        let marker = owner_marker_path(&path);
        write_owner_marker(&path, &repo_root, "feat/race").expect("owner marker");
        let managed = ManagedWorktree {
            path: path.clone(),
            repo_root,
            branch: "feat/race".to_string(),
            teardown: TeardownPolicy::Auto,
            identity: worktree_identity(&path).ok(),
        };
        let replacement = b"replacement-created-during-removal";

        let error = remove_managed_worktree_with(&managed, || {
            std::fs::write(&marker, replacement).expect("replacement marker");
            Err("forced git removal failure".to_string())
        })
        .expect_err("forced removal must fail");

        assert!(error.contains("forced git removal failure"));
        assert!(error.contains("cannot restore owner marker"));
        assert_eq!(
            std::fs::read(marker).expect("replacement marker survives"),
            replacement,
            "restoration must never overwrite a marker created during the removal window"
        );
    }

    #[test]
    fn tracked_owner_marker_survives_failed_add_and_rollback() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir(&repo_root).expect("repo root");
        run_git(&repo_root, &["init"], GIT_DEADLINE).expect("git init");
        run_git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
            GIT_DEADLINE,
        )
        .expect("git config email");
        run_git(
            &repo_root,
            &["config", "user.name", "PaneFlow Tests"],
            GIT_DEADLINE,
        )
        .expect("git config name");
        let user_marker = b"user-owned marker content\n";
        std::fs::write(owner_marker_path(&repo_root), user_marker).expect("tracked marker");
        run_git(&repo_root, &["add", OWNER_MARKER_FILE], GIT_DEADLINE).expect("git add");
        run_git(&repo_root, &["commit", "-m", "fixture"], GIT_DEADLINE).expect("git commit");

        let branch = "feat/tracked-marker";
        let path = worktree_dir(&repo_root, branch);
        assert!(
            add_worktree(&repo_root, &path, branch, true).is_err(),
            "Paneflow must not overwrite a tracked marker in the new checkout"
        );
        assert!(
            !path.exists(),
            "failed marker creation must roll back the linked worktree"
        );
        assert_eq!(
            std::fs::read(owner_marker_path(&repo_root)).expect("main marker survives"),
            user_marker
        );
        assert_eq!(
            run_git(
                &repo_root,
                &["show", &format!("{branch}:{OWNER_MARKER_FILE}")],
                GIT_DEADLINE,
            )
            .expect("branch marker blob"),
            "user-owned marker content",
            "rollback must leave the tracked branch content untouched"
        );
    }

    #[test]
    fn failed_marker_and_failed_rollback_report_that_checkout_may_remain() {
        let error = add_worktree_marker_failure(
            "owner marker creation failed".to_string(),
            Err("git worktree remove timed out".to_string()),
        );

        assert!(error.contains("owner marker creation failed"));
        assert!(error.contains("rollback failed"));
        assert!(error.contains("checkout may remain"));
        assert!(error.contains("git worktree remove timed out"));
    }

    #[test]
    fn owner_marker_creation_does_not_follow_symlinks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir(&repo_root).expect("repo root");
        std::fs::create_dir(&worktree).expect("worktree");
        let target = tmp.path().join("user-owned-target");
        let original = b"do not overwrite\n";
        std::fs::write(&target, original).expect("symlink target");
        std::os::unix::fs::symlink(&target, owner_marker_path(&worktree)).expect("marker symlink");

        assert!(write_owner_marker(&worktree, &repo_root, "feat/symlink").is_err());
        assert_eq!(std::fs::read(&target).expect("target survives"), original);
        assert!(
            std::fs::symlink_metadata(owner_marker_path(&worktree))
                .expect("marker link survives")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn partial_marker_write_never_publishes_or_claims_a_markerless_checkout() {
        use std::io::Write;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        run_git(&repo_root, &["init"], GIT_DEADLINE).expect("git init");
        run_git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
            GIT_DEADLINE,
        )
        .expect("git config email");
        run_git(
            &repo_root,
            &["config", "user.name", "PaneFlow Tests"],
            GIT_DEADLINE,
        )
        .expect("git config name");
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        run_git(&repo_root, &["add", "."], GIT_DEADLINE).expect("git add");
        run_git(&repo_root, &["commit", "-m", "fixture"], GIT_DEADLINE).expect("git commit");

        let branch = "feat/partial-marker";
        let path = worktree_dir(&repo_root, branch);
        let path_text = path.to_string_lossy();
        run_git(
            &repo_root,
            &["worktree", "add", &path_text, "-b", branch],
            GIT_DEADLINE,
        )
        .expect("registered markerless checkout");

        let error = write_owner_marker_with(&path, &repo_root, branch, |file, contents| {
            file.write_all(&contents[..5])?;
            Err(std::io::Error::other("injected short write"))
        })
        .expect_err("injected marker write must fail");
        assert!(error.contains("injected short write"));
        assert!(
            !owner_marker_path(&path).exists(),
            "a partial inode must remain staged outside the checkout and never reach the marker path"
        );

        assert!(
            pending_managed_worktree_from_record(
                &path.to_string_lossy(),
                &repo_root.to_string_lossy(),
                branch,
                "auto",
            )
            .is_none(),
            "a pre-creation reservation without directory identity must not claim a markerless checkout"
        );
        remove_worktree(&repo_root, &path).expect("fixture cleanup");
    }

    #[test]
    fn managed_worktree_record_requires_marker_and_generated_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        let branch = "feat/hardening";
        let path = worktree_dir(&repo_root, branch);
        std::fs::create_dir_all(&path).expect("worktree dir");

        assert!(
            managed_worktree_from_record(
                &path.to_string_lossy(),
                &repo_root.to_string_lossy(),
                branch,
                "auto",
            )
            .is_none(),
            "a matching path without owner marker is not enough"
        );

        write_owner_marker(&path, &repo_root, branch).expect("marker");
        let restored = managed_worktree_from_record(
            &path.to_string_lossy(),
            &repo_root.to_string_lossy(),
            branch,
            "delete",
        )
        .expect("marker-backed record restores");
        assert_eq!(
            restored.path,
            std::fs::canonicalize(&path).expect("canonical path")
        );
        assert_eq!(restored.teardown, TeardownPolicy::Keep);

        let outside = tmp.path().join("external");
        std::fs::create_dir_all(&outside).expect("outside dir");
        std::fs::write(owner_marker_path(&outside), "owner=paneflow\n").expect("outside marker");
        assert!(
            managed_worktree_from_record(
                &outside.to_string_lossy(),
                &repo_root.to_string_lossy(),
                branch,
                "auto",
            )
            .is_none(),
            "marker cannot bless a path outside the deterministic Paneflow dir"
        );

        let alias_branch = "feat/alias";
        let alias_path = worktree_dir(&repo_root, alias_branch);
        std::os::unix::fs::symlink(&outside, &alias_path).expect("worktree alias");
        assert!(
            managed_worktree_from_record(
                &alias_path.to_string_lossy(),
                &repo_root.to_string_lossy(),
                alias_branch,
                "auto",
            )
            .is_none(),
            "a deterministic-looking symlink must not transfer ownership of an external path"
        );
    }

    #[test]
    fn pending_record_recovers_only_the_interrupted_marker_unlink_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        run_git(&repo_root, &["init"], GIT_DEADLINE).expect("git init");
        run_git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
            GIT_DEADLINE,
        )
        .expect("git config email");
        run_git(
            &repo_root,
            &["config", "user.name", "PaneFlow Tests"],
            GIT_DEADLINE,
        )
        .expect("git config name");
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        run_git(&repo_root, &["add", "."], GIT_DEADLINE).expect("git add");
        run_git(&repo_root, &["commit", "-m", "fixture"], GIT_DEADLINE).expect("git commit");

        let branch = "feat/pending-marker-recovery";
        let path = worktree_dir(&repo_root, branch);
        add_worktree(&repo_root, &path, branch, true).expect("add managed worktree");
        let identity = worktree_identity(&path).expect("created checkout identity");
        std::fs::remove_file(owner_marker_path(&path)).expect("simulate crash after marker unlink");

        assert!(
            managed_worktree_from_record(
                &path.to_string_lossy(),
                &repo_root.to_string_lossy(),
                branch,
                "auto",
            )
            .is_none(),
            "a live workspace record must never claim a markerless checkout"
        );
        for policy in ["keep", "delete"] {
            assert!(
                pending_managed_worktree_from_record(
                    &path.to_string_lossy(),
                    &repo_root.to_string_lossy(),
                    branch,
                    policy,
                )
                .is_none(),
                "a {policy:?} record cannot come from Auto teardown's marker-unlink window"
            );
            assert!(
                !owner_marker_path(&path).exists(),
                "a non-Auto pending record must not recreate the marker"
            );
        }
        let recovered = pending_managed_worktree_from_persisted_record(
            &path.to_string_lossy(),
            &repo_root.to_string_lossy(),
            branch,
            "auto",
            Some(identity.as_str()),
        )
        .expect("the durable pending record recovers its exact registered checkout");
        validated_owner_marker(&path, &repo_root, branch).expect("marker recreated exactly");

        remove_managed_worktree_with(&recovered, || remove_worktree(&repo_root, &path))
            .expect("recovered retirement completes");
        assert!(!path.exists());
    }

    #[test]
    fn pending_marker_recovery_refuses_same_path_branch_replacement() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        run_git(&repo_root, &["init"], GIT_DEADLINE).expect("git init");
        run_git(
            &repo_root,
            &["config", "user.email", "paneflow-tests@example.invalid"],
            GIT_DEADLINE,
        )
        .expect("git config email");
        run_git(
            &repo_root,
            &["config", "user.name", "PaneFlow Tests"],
            GIT_DEADLINE,
        )
        .expect("git config name");
        std::fs::write(repo_root.join("README.md"), "test\n").expect("tracked file");
        run_git(&repo_root, &["add", "."], GIT_DEADLINE).expect("git add");
        run_git(&repo_root, &["commit", "-m", "fixture"], GIT_DEADLINE).expect("git commit");

        let branch = "feat/replaced-checkout";
        let path = worktree_dir(&repo_root, branch);
        add_worktree(&repo_root, &path, branch, true).expect("original managed checkout");
        let original_identity = worktree_identity(&path).expect("original identity");
        std::fs::remove_file(owner_marker_path(&path)).expect("interrupted teardown unlink");
        remove_worktree(&repo_root, &path).expect("original checkout removed");
        let path_text = path.to_string_lossy();
        run_git(
            &repo_root,
            &["worktree", "add", &path_text, branch],
            GIT_DEADLINE,
        )
        .expect("external same-path same-branch replacement");
        assert_ne!(
            worktree_identity(&path).expect("replacement identity"),
            original_identity,
            "replacement fixture must have a distinct directory identity"
        );

        assert!(
            pending_managed_worktree_from_persisted_record(
                &path.to_string_lossy(),
                &repo_root.to_string_lossy(),
                branch,
                "auto",
                Some(original_identity.as_str()),
            )
            .is_none(),
            "the old journal must never mint a marker into a replacement checkout"
        );
        assert!(!owner_marker_path(&path).exists());
        remove_worktree(&repo_root, &path).expect("replacement fixture cleanup");
    }

    #[test]
    fn managed_worktree_record_accepts_hashed_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        let branch = "feat/a-b";
        let path = worktree_dir_hashed(&repo_root, branch);
        std::fs::create_dir_all(&path).expect("worktree dir");
        write_owner_marker(&path, &repo_root, branch).expect("marker");

        let restored = managed_worktree_from_record(
            &path.to_string_lossy(),
            &repo_root.to_string_lossy(),
            branch,
            "auto",
        )
        .expect("hashed path restores");

        assert_eq!(
            restored.path,
            std::fs::canonicalize(&path).expect("canonical path")
        );
        assert_eq!(restored.branch, branch);
    }

    #[test]
    fn managed_worktree_matching_directory_identity_restores() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        let branch = "feat/identity-match";
        let path = worktree_dir(&repo_root, branch);
        std::fs::create_dir_all(&path).expect("worktree dir");
        write_owner_marker(&path, &repo_root, branch).expect("marker");
        let identity = worktree_identity(&path).expect("directory identity");

        let restored = managed_worktree_from_persisted_record(
            &path.to_string_lossy(),
            &repo_root.to_string_lossy(),
            branch,
            "auto",
            Some(identity.as_str()),
        )
        .expect("matching directory identity restores a marked checkout");

        assert_eq!(
            restored.path,
            std::fs::canonicalize(&path).expect("canonical path")
        );
        assert_eq!(
            restored.identity.as_ref().map(WorktreeIdentity::as_str),
            Some(identity.as_str())
        );
    }

    #[test]
    fn managed_worktree_mismatched_directory_identity_is_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo root");
        let branch = "feat/identity-mismatch";
        let path = worktree_dir(&repo_root, branch);
        std::fs::create_dir_all(&path).expect("worktree dir");
        write_owner_marker(&path, &repo_root, branch).expect("marker");
        let identity = worktree_identity(&path).expect("directory identity");
        let other = "0:0:0:0";
        assert_ne!(
            identity.as_str(),
            other,
            "fixture identity must differ from the mismatched persisted value"
        );

        assert!(
            managed_worktree_from_record(
                &path.to_string_lossy(),
                &repo_root.to_string_lossy(),
                branch,
                "auto",
            )
            .is_some(),
            "a marked checkout still restores without persisted identity"
        );
        assert!(
            managed_worktree_from_persisted_record(
                &path.to_string_lossy(),
                &repo_root.to_string_lossy(),
                branch,
                "auto",
                Some(other),
            )
            .is_none(),
            "a marked checkout with a different directory identity is dropped"
        );
    }

    #[test]
    fn copy_env_files_preserves_source_mode_before_bytes_land() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempfile::tempdir().expect("src");
        let dst = tempfile::tempdir().expect("dst");
        let env = src.path().join(".env");
        std::fs::write(&env, "SECRET=1").unwrap();
        let mut permissions = std::fs::metadata(&env).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&env, permissions).unwrap();

        let copied = copy_env_files(src.path(), dst.path());
        assert_eq!(copied, vec![".env".to_string()]);
        let mode = std::fs::metadata(dst.path().join(".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "copied .env must not be created world-readable"
        );
    }

    #[test]
    fn copy_env_files_copies_top_level_env_only_and_never_clobbers() {
        let src = tempfile::tempdir().expect("src");
        let dst = tempfile::tempdir().expect("dst");
        std::fs::write(src.path().join(".env"), "A=1").unwrap();
        std::fs::write(src.path().join(".env.local"), "B=2").unwrap();
        std::fs::write(src.path().join("notenv"), "x").unwrap();
        // Nested .env must NOT be picked up (top-level only).
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/.env"), "C=3").unwrap();
        // Pre-existing destination file must survive (checkout owns it).
        std::fs::write(dst.path().join(".env"), "KEEP").unwrap();

        let copied = copy_env_files(src.path(), dst.path());
        assert_eq!(copied, vec![".env.local".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dst.path().join(".env")).unwrap(),
            "KEEP",
            "existing destination file is never clobbered"
        );
        assert!(dst.path().join(".env.local").exists());
        assert!(!dst.path().join("notenv").exists());
    }

    #[test]
    fn copy_env_files_missing_source_is_silent_empty() {
        let dst = tempfile::tempdir().expect("dst");
        let copied = copy_env_files(Path::new("/nonexistent-paneflow-test"), dst.path());
        assert!(copied.is_empty());
    }

    #[test]
    fn copy_env_files_skips_symlinks() {
        let src = tempfile::tempdir().expect("src");
        let dst = tempfile::tempdir().expect("dst");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "SECRET_KEY").unwrap();
        std::os::unix::fs::symlink(&secret, src.path().join(".env")).expect("src .env symlink");
        std::fs::write(src.path().join(".env.local"), "SAFE=1").unwrap();

        let planted = outside.path().join("planted");
        std::os::unix::fs::symlink(&planted, dst.path().join(".env.remote"))
            .expect("dangling dest .env symlink");
        std::fs::write(src.path().join(".env.remote"), "LEAK=1").unwrap();

        let copied = copy_env_files(src.path(), dst.path());

        assert_eq!(copied, vec![".env.local".to_string()]);
        assert!(
            !dst.path().join(".env").exists(),
            "src/.env symlink to a file outside the repo must not materialize dest .env"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join(".env.local")).unwrap(),
            "SAFE=1"
        );
        assert!(
            !planted.exists(),
            "copy must not follow a planted dest symlink"
        );
        assert!(
            std::fs::symlink_metadata(dst.path().join(".env.remote"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[traced_test]
    fn copy_env_files_logs_copy_failure() {
        let src = tempfile::tempdir().expect("src");
        let src_file = src.path().join(".env.local");
        std::fs::write(&src_file, "A=1").expect("source env");
        let mut permissions = std::fs::metadata(&src_file)
            .expect("metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
        std::fs::set_permissions(&src_file, permissions).expect("unreadable source env");
        let dst = tempfile::tempdir().expect("dst");

        let copied = copy_env_files(src.path(), dst.path());

        assert!(copied.is_empty());
        assert!(
            logs_contain("failed to copy env file"),
            "copy failure should emit a warning"
        );
    }
}
