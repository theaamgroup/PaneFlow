//! Git worktree plumbing for the tab worktree picker (issues #347, #348), the
//! Review rail, and `diff/`.
//!
//! PaneFlow never deletes a checkout on its own. The picker binds a tab to the
//! checkout that holds a branch, making one with [`prepare_branch_checkout`]
//! when none exists, and closing a tab or a workspace leaves it on disk. The
//! tab menu's "Remove worktree" row is the only removal, and it runs only when
//! the user asks and [`is_clean_for_removal`] and
//! [`worktree_has_live_process_cwd`] both allow it.
//!
//! Invariants:
//! - a branch is NEVER deleted, only the worktree directory;
//! - a worktree with uncommitted changes is NEVER removed;
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

/// One entry of `git worktree list --porcelain`.
///
/// A `worktree ` line is enough to keep the entry. Bare and other HEAD-less
/// checkouts are included so collision checks, branch checkout planning and
/// checkout removal all see every registered worktree.
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

/// Map a path git reports (symlinks resolved: `/private/var/...` for a
/// `/var/...` tempdir, the real volume behind a linked `~/Github`) back onto
/// the unresolved form PaneFlow builds from `repo_root`, so a registered
/// checkout compares equal to `worktree_dir` / `worktree_dir_hashed`. A path
/// under no PaneFlow root is returned untouched.
fn in_paneflow_path_form(repo_root: &Path, path: PathBuf) -> PathBuf {
    let roots = [worktrees_parent(repo_root), repo_root.to_path_buf()];
    for root in roots {
        if path.starts_with(&root) {
            return path;
        }
        let Ok(resolved) = std::fs::canonicalize(&root) else {
            continue;
        };
        if let Ok(rest) = path.strip_prefix(&resolved) {
            return root.join(rest);
        }
    }
    path
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

/// Index of the git subcommand inside `args`.
///
/// Global options may precede it (`--literal-pathspecs`, `-c key=value`).
/// `-c` and `-C`, and the long options that take a separate value, consume
/// the next element. `--` ends the option scan. The subcommand is the first
/// later token, so a leading option is never treated as the command name.
fn git_subcommand_index(args: &[&str]) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if arg == "--" {
            let next = index + 1;
            return (next < args.len()).then_some(next);
        }
        if !arg.starts_with('-') {
            return Some(index);
        }
        let consumes_next = matches!(
            arg,
            "-c" | "-C"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--config-env"
                | "--super-prefix"
                | "--exec-path"
                | "--list-cmds"
        );
        index = index.saturating_add(if consumes_next { 2 } else { 1 });
    }
    None
}

/// Append a git subcommand, forcing `--no-ext-diff` on `git diff`.
///
/// `-c alias.<name>=` is inserted immediately before the subcommand token,
/// after any global options, so a repo or global alias cannot replace it.
/// An `alias.status` that exits 0 with empty stdout would otherwise make
/// [`is_clean_for_removal`] report a dirty tree as clean. The same clearance
/// covers `rev-parse`, `ls-tree`, `branch`, and `switch` (issue #681).
pub(crate) fn git_subcommand(cmd: &mut Command, args: &[&str]) {
    let Some(index) = git_subcommand_index(args) else {
        cmd.args(args);
        return;
    };
    let name = args[index];
    if index > 0 {
        cmd.args(&args[..index]);
    }
    cmd.arg("-c").arg(format!("alias.{name}="));
    if name == "diff" {
        cmd.arg("diff").arg("--no-ext-diff");
        if index + 1 < args.len() {
            cmd.args(&args[index + 1..]);
        }
    } else {
        cmd.args(&args[index..]);
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
    Ok(parse_worktree_porcelain(&stdout)
        .into_iter()
        .map(|entry| WorktreeEntry {
            path: in_paneflow_path_form(repo_root, entry.path),
            ..entry
        })
        .collect())
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

/// Wall-clock ceiling and stdout/stderr cap for [`list_branches`]. `branch
/// --format` is fast; the bounds only exist so a wedged git (a hook prompting
/// for input, a stale lock) cannot pin a thread.
const BRANCH_GIT_DEADLINE: Duration = Duration::from_secs(30);
const BRANCH_GIT_OUTPUT_CAP: u64 = 512 * 1024;

/// Local branches of the repository at `cwd`, sorted and deduplicated. The
/// tab worktree picker's branch reader (issue #347).
pub(crate) fn list_branches(cwd: &str) -> Result<Vec<String>, String> {
    let mut command = git_command();
    git_subcommand(&mut command, &["branch", "--format=%(refname:short)"]);
    command.current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0");

    let output =
        paneflow_process::run_with_timeout(command, BRANCH_GIT_DEADLINE, BRANCH_GIT_OUTPUT_CAP)
            .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(git_output_error(&output));
    }

    let mut branches = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    branches.sort();
    branches.dedup();
    Ok(branches)
}

/// The first line of a failed git call's stderr, or its exit status when
/// stderr is empty.
fn git_output_error(output: &paneflow_process::BoundedOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if message.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        message.lines().next().unwrap_or(message).to_string()
    }
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
/// already-checked-out branch can work at all. The path is the slug directory,
/// or the hashed one when the slug is already claimed by another branch.
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
/// Blocking git subprocesses, including a full checkout when needed, all
/// through [`run_git`] with `GIT_TERMINAL_PROMPT=0` - the caller runs it
/// through `smol::unblock`, never on the render thread.
///
/// A directory asked for by name, by hand, is the user's. Nothing here removes
/// it, workspace close never touches it, and the branch is untouched either way.
pub fn prepare_branch_checkout(repo_root: &Path, branch: &str) -> Result<PathBuf, String> {
    // Configured defaults are arbitrary input, unlike the branch picker's list.
    // Reject options, revision expressions, and previous-checkout expansion.
    let checked = run_git(
        repo_root,
        &["check-ref-format", "--branch", branch],
        GIT_DEADLINE,
    )?;
    if checked.trim() != branch {
        return Err(format!("Not a literal branch name: {branch}"));
    }
    if !branch_exists(repo_root, branch) {
        return Err(format!("Local branch {branch} does not exist"));
    }
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
            // A checkout without the repository's gitignored `.env*` cannot
            // run the app it holds. Best-effort by design.
            let _ = copy_env_files(repo_root, &path);
            Ok(path)
        }
    }
}

/// `git worktree add <path> [-b] <branch>`. `create_branch` chooses between
/// branching off HEAD (`-b`) and checking out the existing branch.
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

/// True when the worktree holds no uncommitted work a user could lose: no
/// tracked modification and no untracked file that git would track. Ignored
/// files do not count (issue #348), which is exactly the gate
/// `git worktree remove` applies itself: the `.env*` copies
/// [`prepare_branch_checkout`] makes for a picker checkout, build output, and
/// caches are all ignored, and a removal that refused them would refuse every
/// checkout the picker ever made. An error (worktree gone, git missing) is NOT
/// "clean" - the caller must keep its hands off when it cannot prove
/// cleanliness.
pub fn is_clean_for_removal(worktree_path: &Path) -> Result<bool, String> {
    run_git(
        worktree_path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        GIT_DEADLINE,
    )
    .map(|out| out.trim().is_empty())
}

/// `git worktree remove <path>`. Refuses dirty worktrees by itself too (git
/// native), but callers must check [`is_clean_for_removal`] first to control
/// messaging.
/// The BRANCH IS NEVER DELETED - that is the US-009 invariant, not a TODO.
pub fn remove_worktree(repo_root: &Path, path: &Path) -> Result<(), String> {
    let path_s = path.to_string_lossy();
    run_git(repo_root, &["worktree", "remove", &path_s], GIT_DEADLINE).map(|_| ())
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
/// sessions, and accessible same-UID survivors. The caller samples the open
/// terminals' PTY session IDs on the UI thread when "Remove worktree" is
/// clicked (`live_terminal_session_ids`), but a shell in one of them can `cd`
/// into the checkout after that sample, and a process can outlive its terminal
/// entity entirely, so this background scan checks every process. CWD failures are fatal only for authenticated session members or
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
/// (US-009 AC5), so this is safe to run blindly after a removal.
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

    fn rendered_git_subcommand(args: &[&str]) -> String {
        let mut cmd = git_command();
        git_subcommand(&mut cmd, args);
        format!("{cmd:?}")
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
        assert_production_git_command_isolated(include_str!("worktree.rs"), "fn list_branches(");

        // `git_subcommand` puts `-c alias.<name>=` on the Command before the
        // subcommand token. Git applies `-c` after the repo config and after
        // `GIT_CONFIG_GLOBAL`, so the same flag blocks an inherited global
        // alias. Do not set `GIT_CONFIG_GLOBAL` on this process: other tests
        // share it. The live case below is the repo alias.
        let subcommand_body = source_slice(
            include_str!("worktree.rs"),
            "fn git_subcommand(",
            "fn run_git(",
        );
        assert!(
            subcommand_body.contains("alias."),
            "git_subcommand must clear alias.<name> before the subcommand"
        );
        let status_cmd = rendered_git_subcommand(&[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ]);
        let alias_status_at = status_cmd
            .find("alias.status=")
            .expect("status command disables alias.status");
        let status_token = status_cmd
            .find("\"status\"")
            .expect("status command names the subcommand");
        assert!(
            alias_status_at < status_token,
            "-c alias.status= must precede the status token: {status_cmd}"
        );
        let diff_cmd = rendered_git_subcommand(&["diff", "--stat"]);
        let alias_diff_at = diff_cmd
            .find("alias.diff=")
            .expect("diff command disables alias.diff");
        let diff_token = diff_cmd
            .find("\"diff\"")
            .expect("diff command names the subcommand");
        let no_ext = diff_cmd
            .find("--no-ext-diff")
            .expect("diff keeps --no-ext-diff");
        assert!(
            alias_diff_at < diff_token && diff_token < no_ext,
            "-c alias.diff= must precede diff --no-ext-diff: {diff_cmd}"
        );
        let dashed = rendered_git_subcommand(&["-c", "color.ui=never", "status"]);
        assert!(
            !dashed.contains("alias.-c"),
            "a leading option is not a subcommand name: {dashed}"
        );
        let dashed_alias = dashed
            .find("alias.status=")
            .expect("a leading -c still disables alias.status");
        let dashed_status = dashed
            .rfind("\"status\"")
            .expect("status command names the subcommand");
        assert!(
            dashed_alias < dashed_status,
            "-c alias.status= must follow other global options and precede status: {dashed}"
        );
        let literal = rendered_git_subcommand(&["--literal-pathspecs", "ls-tree", "-z", "HEAD"]);
        let literal_flag = literal
            .find("--literal-pathspecs")
            .expect("literal pathspecs stays a global option");
        let literal_alias = literal
            .find("alias.ls-tree=")
            .expect("ls-tree behind a global option disables alias.ls-tree");
        let literal_cmd = literal
            .find("\"ls-tree\"")
            .expect("ls-tree command names the subcommand");
        assert!(
            literal_flag < literal_alias && literal_alias < literal_cmd,
            "alias.ls-tree= must sit between --literal-pathspecs and ls-tree: {literal}"
        );
        for (file, marker) in [
            (include_str!("../diff/git.rs"), "fn run_git_timed("),
            (include_str!("worktree.rs"), "fn list_branches("),
            (include_str!("../app/work_review/model.rs"), "fn git("),
        ] {
            let body = git_fn_source(file, marker);
            assert!(
                body.contains("git_subcommand("),
                "{marker} must clear alias.<subcommand> through git_subcommand"
            );
        }

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
        // `run_git` itself must not recurse through alias.status: this config
        // subcommand is also dispatched by `git_subcommand`.
        let alias_status = format!("!{}", marker_script.display());
        run_git(
            &repo_root,
            &["config", "alias.status", &alias_status],
            GIT_DEADLINE,
        )
        .expect("config alias.status");

        std::fs::write(repo_root.join("README.md"), "changed\n").expect("dirty worktree");

        let listed = list_worktrees(&repo_root).expect("list worktrees");
        assert!(!listed.is_empty(), "hostile repo must still list");
        assert!(
            !is_clean_for_removal(&repo_root)
                .expect("git status against hostile hooksPath/fsmonitor"),
            "a dirty tree must not look clean when alias.status exits 0 with empty stdout"
        );
        assert!(
            !marker.exists(),
            "alias.status must not run: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
        let diff = crate::diff::load_column(&repo_root, "HEAD").diff;
        assert!(
            diff.error.is_none(),
            "git diff against hostile diff.external: {:?}",
            diff.error
        );
        let branch = "feat/hostile-hooks";
        let path = worktree_dir(&repo_root, branch);
        git_worktree_add(&repo_root, &path, branch, true).expect("worktree add");

        assert!(
            !marker.exists(),
            "repo core.hooksPath/core.fsmonitor/diff.external must not run: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
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
            "an accessible same-UID survivor must block removal even after its terminal entity is gone"
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
    /// branch is registered with git and reused on the next pick, and the
    /// repository's own branch resolves to the repository root.
    #[test]
    fn prepare_branch_checkout_reuses_an_existing_worktree() {
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

    /// Issue #529 (upstream e87a03ed), against a real repository under an
    /// UN-canonicalized tempdir (`$TMPDIR` is `/var/folders/...`, a symlink
    /// to `/private/var/...` on macOS): `git worktree list` reports resolved
    /// paths, and the second pick of the same branch must still find its
    /// checkout instead of refusing the slug path as unregistered.
    #[test]
    fn prepare_branch_checkout_recognises_its_checkout_through_a_symlinked_root() {
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

        let first = prepare_branch_checkout(&repo, "feat/x").expect("first pick checks out");
        assert_eq!(first, worktree_dir(&repo, "feat/x"));
        let entries = list_worktrees(&repo).expect("worktree list");
        assert!(
            entries
                .iter()
                .any(|e| e.path == first && e.branch.as_deref() == Some("feat/x")),
            "the listing must come back in PaneFlow path form: {entries:?}"
        );

        let again = prepare_branch_checkout(&repo, "feat/x").expect("second pick reuses");
        assert_eq!(again, first, "the same branch reuses its checkout");
        assert_eq!(
            list_worktrees(&repo).expect("worktree list").len(),
            entries.len(),
            "reusing must not add a worktree"
        );
    }

    #[test]
    fn a_resolved_git_path_comes_back_in_paneflow_path_form() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        let dir = worktree_dir(&repo_root, "feat/x");
        std::fs::create_dir_all(&dir).expect("worktree dir");

        let parent = worktrees_parent(&repo_root);
        let resolved = std::fs::canonicalize(&parent).expect("canonical");
        let as_git_reports_it = resolved.join("feat-x");

        assert_eq!(in_paneflow_path_form(&repo_root, as_git_reports_it), dir);
        assert_eq!(
            in_paneflow_path_form(&repo_root, dir.clone()),
            dir,
            "a path already in PaneFlow form is untouched"
        );
    }

    #[test]
    fn a_path_outside_every_paneflow_root_is_left_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        let outside = tmp.path().join("somewhere-else").join("checkout");

        assert_eq!(in_paneflow_path_form(&repo_root, outside.clone()), outside);
    }

    #[test]
    fn new_tab_branch_checkouts_preserve_the_workspace_branch_and_dirty_files() {
        let sandbox = tempfile::tempdir().expect("tempdir");
        let repo = sandbox.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
            vec!["branch", "staging"],
            vec!["tag", "tag-only"],
            vec!["checkout", "-q", "-b", "feature"],
        ] {
            run_git(&repo, &args, GIT_DEADLINE).expect("git fixture");
        }
        let repo = std::fs::canonicalize(repo).expect("canonical repo");
        std::fs::write(repo.join("unfinished.txt"), "keep my work").expect("dirty file");
        for branch in ["main", "staging"] {
            let checkout = prepare_branch_checkout(&repo, branch).expect("branch checkout");
            assert_ne!(checkout, repo);
            assert_eq!(
                prepare_branch_checkout(&repo, branch).expect("reuse"),
                checkout
            );
            let entries = list_worktrees(&repo).expect("listing");
            assert!(
                entries
                    .iter()
                    .any(|e| e.path == repo && e.branch.as_deref() == Some("feature"))
            );
            assert!(
                entries
                    .iter()
                    .any(|e| e.path == checkout && e.branch.as_deref() == Some(branch))
            );
        }
        assert_eq!(
            std::fs::read_to_string(repo.join("unfinished.txt")).expect("preserved"),
            "keep my work"
        );
        for invalid in [
            "missing-branch",
            "tag-only",
            "--detach",
            "HEAD",
            "main~1",
            "@{-1}",
        ] {
            assert!(
                prepare_branch_checkout(&repo, invalid).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            list_worktrees(&repo).expect("listing after refusals").len(),
            3
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
