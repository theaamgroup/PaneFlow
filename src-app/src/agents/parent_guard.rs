//! US-003: kill-on-parent-death guard for spawned agent CLIs and PTYs.
//!
//! Goal: prevent child agents and terminal work from surviving Paneflow.
//! Shim-wrapped agent CLIs have parent-death watchers. Each raw PTY guard also
//! inherits a dedicated master descriptor: on orderly teardown or hard parent
//! death it authenticates the pinned terminal session, refreshes late shell
//! members, and discovers every live process group in the terminal session
//! before owning the complete TERM-to-KILL ladder.
//!
//! There is no process-wide guard: each child class carries its own.
//! Shim-wrapped agent CLIs are covered by `paneflow-shim`, which installs a
//! parent-death watcher on macOS before it waits on the real agent binary.
//! Raw PTY shells are covered by tiny per-PTY watcher processes launched
//! through [`spawn_pty_guard`].

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Identity and credential markers Claude Code exports into the processes it
/// spawns. Single source for the process-env ACP scrub and the PTY overlay
/// strip in `pty_session`.
///
/// `CLAUDECODE` is the original refusal ("cannot launch inside another
/// Claude Code session"). The rest are session identity / IPC credentials
/// a pane must never inherit; `assemble_pty_env` only overlays, so this
/// process-env scrub is the half that actually unsets them.
pub const INHERITED_AGENT_SESSION_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// Remove inherited agent-session markers from the process environment before
/// any worker thread or PTY backend starts. The PTY spawn already calls
/// `env_remove` for each key, but this scrub covers the whole process: every
/// other child and in-process reader of these variables sees them unset too.
///
/// Must be called from the very first lines of `main()`, before any
/// `std::thread::spawn`, `tokio::runtime::Builder::build`, or smol
/// executor initialization. Rust 1.85 made `std::env::remove_var`
/// `unsafe` because it races with concurrent `getenv` from any
/// other thread; the runtime sub-systems above all read env on
/// startup, so calling this before any thread exists is genuinely safe
/// by construction.
///
/// # Safety
///
/// Must run before any other thread, async runtime, or foreign library can
/// concurrently read environment variables. Prefer
/// `Command::env_remove` for per-child scrubbing after startup.
///
/// This is the one pre-thread scrub `main` calls, so it also runs
/// [`scrub_inherited_git_env_before_threads`] (issue #1110).
pub(crate) unsafe fn scrub_claudecode_env_before_threads() {
    // SAFETY: delegated to the caller by this function's contract.
    unsafe {
        for key in INHERITED_AGENT_SESSION_ENV {
            std::env::remove_var(*key);
        }
        scrub_inherited_git_env_before_threads();
    }
}

/// Remove the repository-redirecting git environment PaneFlow was launched
/// with ([`INHERITED_GIT_ENV`](crate::workspace::worktree::INHERITED_GIT_ENV))
/// from the whole process (issue #1110).
///
/// These names normally reach PaneFlow only when it was started from inside a
/// git process (a hook, `rebase --exec`, `bisect run`). A Dock or Finder
/// launch can carry them too, through `launchctl setenv`, but a
/// repository-wide redirect set that way is still one no workspace should
/// follow. Every child inherits the process env: pane shells
/// (`CommandBuilder` seeds from it), the agent session-list CLIs, the external
/// editor, and the workspace launchers. Left in place, each of them would read
/// the launcher's repository, index, or object store instead of its own `cwd`.
/// Names a user's shell rc exports are unaffected: the shell sets them after
/// it starts. The login-shell capture imports only `PATH`, so no git name can
/// come back through it.
///
/// [`GIT_ENV_KEPT_BY_STARTUP_SCRUB`] names the exceptions.
///
/// Runs for the GUI and the CLI verbs alike, since both pass through the one
/// scrub in `main`. The CLI verbs spawn no git, so the scrub is harmless there
/// and keeps a single call site.
///
/// # Safety
///
/// Same contract as [`scrub_claudecode_env_before_threads`]: no other thread,
/// async runtime, or foreign library may be reading the environment.
pub(crate) unsafe fn scrub_inherited_git_env_before_threads() {
    // SAFETY: delegated to the caller by this function's contract.
    unsafe {
        for key in crate::workspace::worktree::INHERITED_GIT_ENV {
            if !GIT_ENV_KEPT_BY_STARTUP_SCRUB.contains(key) {
                std::env::remove_var(*key);
            }
        }
    }
}

/// `INHERITED_GIT_ENV` names the startup scrub leaves in the process, so pane
/// shells and every other child still inherit them. `git_command` still drops
/// them from PaneFlow's own git spawns.
///
/// `GIT_SSH_COMMAND` picks the SSH command, not which repository git
/// inspects. A user may set it with `launchctl setenv`, which Dock launches
/// inherit; removing it would make `git push` in every pane use the wrong key.
const GIT_ENV_KEPT_BY_STARTUP_SCRUB: &[&str] = &["GIT_SSH_COMMAND"];

/// Remove inherited agent-session markers from one child command without
/// mutating global process environment.
#[cfg(test)]
pub fn scrub_claudecode_from_command(command: &mut std::process::Command) {
    for key in INHERITED_AGENT_SESSION_ENV {
        command.env_remove(key);
    }
}

#[cfg(unix)]
pub const PTY_GUARD_SUBCOMMAND: &str = "__paneflow-pty-guard";

/// The descriptor number a long-lived PTY guard finds its own copy of the
/// PTY master at (issues #1123, #1126). Its `session:` argument names it.
#[cfg(unix)]
const PTY_GUARD_MASTER_FD: i32 = 3;

/// A running session-mode PTY guard (issue #1129).
#[cfg(unix)]
pub struct PtyGuardHandle {
    /// The parent end of the guard's stdin control pipe. Dropping it is the
    /// orderly-teardown signal.
    _control: std::process::ChildStdin,
    /// Set once the guard process has exited and been reaped.
    exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(unix)]
impl PtyGuardHandle {
    /// Whether the guard process is still running, so closing the control
    /// pipe will still make it re-enumerate and shut down the session.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn is_live(&self) -> bool {
        !self.exited.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// The one PID liveness probe: `kill(pid, 0)` with `ESRCH` semantics, so a
/// process we may not signal (`EPERM`) still counts as alive. Pid 0 and pids
/// above `i32::MAX` are dead: `kill` would read them as "this process group"
/// or a negative group id, not as the process the caller named.
///
/// A probe, not a policy: the stale-session sweep (`pid_matches`) and the shim
/// lease prune (`lock_holder_is_live`) each decide what an unpinnable live pid
/// means, and destructive signaling goes through [`may_signal_group`].
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    let Some(pid) = probe_pid(pid) else {
        return false;
    };
    // SAFETY: `kill` with sig=0 performs error checking only and delivers no
    // signal. It takes the pid by value and has no memory requirements.
    if unsafe { libc::kill(pid, 0) } == -1 {
        return std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    }
    true
}

/// The one PID start-time probe: the identity pin that session `proc_start`,
/// terminal `child_proc_start` and the process-group member pins record and
/// compare for equality. `EPERM` (SIP-protected targets), a dead-pid race,
/// pid 0 and pids above `i32::MAX` all read `None`; read-only callers apply
/// their own conservative rule to that, and [`may_signal_group`] refuses it.
#[cfg(target_os = "macos")]
pub(crate) fn pid_start_time(pid: u32) -> Option<u64> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;
    let info = pidinfo::<BSDInfo>(probe_pid(pid)?, 0).ok()?;
    Some(bsd_info_start_time(&info))
}

/// Encode a process's start time (`pbi_start_tvsec`/`pbi_start_tvusec`) as
/// microseconds. Opaque: only ever compared for equality.
#[cfg(target_os = "macos")]
fn bsd_info_start_time(info: &libproc::libproc::bsd_info::BSDInfo) -> u64 {
    info.pbi_start_tvsec
        .wrapping_mul(1_000_000)
        .wrapping_add(info.pbi_start_tvusec)
}

/// `pid` as a probe target: 1..=`i32::MAX`, else `None`.
fn probe_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&pid| pid > 0)
}

/// Whether teardown may signal process group `-pid`.
///
/// `getpgid_is_leader` is the live `getpgid(pid) == pid` result (false on
/// ESRCH / a recycled pid that is not a session leader). Start times come
/// from [`pid_start_time`], the same probe that pins session `proc_start` /
/// `child_proc_start`. Unlike conservative UI liveness, a destructive signal
/// requires both probes to exist and match exactly.
pub(crate) fn may_signal_group(
    pid: i32,
    pinned_start: Option<u64>,
    current_start: Option<u64>,
    getpgid_is_leader: bool,
) -> bool {
    pid > 0
        && getpgid_is_leader
        && matches!(
            (pinned_start, current_start),
            (Some(pinned), Some(current)) if pinned == current
        )
}

/// Destructive authorization for one process group. A foreground pipeline may
/// outlive the process whose PID originally named its PGID, so identity is a
/// bounded set of live member PID/start pins plus the owning terminal session.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinnedProcessGroup {
    pub(crate) pgid: u32,
    pub(crate) session_id: u32,
    pub(crate) members: Vec<(u32, u64)>,
}

#[cfg(unix)]
const MAX_PINNED_GROUP_MEMBERS: usize = 4096;

#[cfg(unix)]
fn pinned_process_group_is_current(group: &PinnedProcessGroup) -> bool {
    if group.pgid <= 1 || group.session_id <= 1 || group.members.is_empty() {
        return false;
    }
    let Ok(pgid) = i32::try_from(group.pgid) else {
        return false;
    };
    let Ok(session_id) = i32::try_from(group.session_id) else {
        return false;
    };
    group.members.iter().any(|&(pid, pinned_start)| {
        let Ok(pid_i32) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: getpgid/getsid are read-only process queries. A missing,
        // moved, or recycled member fails one of these checks.
        let same_group_and_session =
            unsafe { libc::getpgid(pid_i32) == pgid && libc::getsid(pid_i32) == session_id };
        same_group_and_session && pid_start_time(pid) == Some(pinned_start)
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn pin_process_group(pgid: u32, session_id: u32) -> Option<PinnedProcessGroup> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;
    use libproc::processes::{ProcFilter, pids_by_type};

    if pgid <= 1 || session_id <= 1 {
        return None;
    }
    let mut members = Vec::new();
    for pid in pids_by_type(ProcFilter::ByProgramGroup { pgrpid: pgid }).ok()? {
        if pid <= 1 || pid > i32::MAX as u32 {
            continue;
        }
        let info = match pidinfo::<BSDInfo>(pid as i32, 0) {
            Ok(info) => info,
            Err(_) => continue,
        };
        if info.pbi_pgid != pgid {
            continue;
        }
        // SAFETY: getsid is a read-only query for this enumerated PID. Ignore
        // members that disappeared during enumeration, but reject a live
        // member from another session: that cannot be this PTY's group.
        let current_session = unsafe { libc::getsid(pid as i32) };
        if current_session < 0 {
            continue;
        }
        if current_session != session_id as i32 {
            return None;
        }
        let start = bsd_info_start_time(&info);
        if members.len() == MAX_PINNED_GROUP_MEMBERS {
            return None;
        }
        members.push((pid, start));
    }
    members.sort_unstable();
    let group = PinnedProcessGroup {
        pgid,
        session_id,
        members,
    };
    pinned_process_group_is_current(&group).then_some(group)
}

/// Enumerate every live process group in one terminal session and pin every
/// observed member. Returning `None` on enumeration/size failure prevents a
/// partial snapshot from being mistaken for complete teardown coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailedSessionMemberQuery {
    SkipExitedOrMoved,
    FailSnapshot,
}

fn classify_failed_session_member_query(
    current_session: i32,
    errno: Option<i32>,
    expected_session: i32,
) -> FailedSessionMemberQuery {
    if current_session < 0 {
        return if errno == Some(libc::ESRCH) {
            FailedSessionMemberQuery::SkipExitedOrMoved
        } else {
            FailedSessionMemberQuery::FailSnapshot
        };
    }
    if current_session != expected_session {
        FailedSessionMemberQuery::SkipExitedOrMoved
    } else {
        FailedSessionMemberQuery::FailSnapshot
    }
}

#[cfg(target_os = "macos")]
fn pin_process_groups_in_session(session_id: u32) -> Option<Vec<PinnedProcessGroup>> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;
    use libproc::processes::{ProcFilter, pids_by_type};
    use std::collections::BTreeMap;

    if session_id <= 1 || session_id > i32::MAX as u32 {
        return None;
    }
    const MAX_SESSION_PROCESSES: usize = MAX_PINNED_GROUP_MEMBERS;
    const MAX_SESSION_GROUPS: usize = 256;
    let mut groups: BTreeMap<u32, Vec<(u32, u64)>> = BTreeMap::new();
    let mut process_count = 0usize;
    for pid in pids_by_type(ProcFilter::All).ok()? {
        if pid <= 1 || pid > i32::MAX as u32 {
            continue;
        }
        // SAFETY: getsid is a read-only query for this enumerated PID.
        if unsafe { libc::getsid(pid as i32) } != session_id as i32 {
            continue;
        }
        let info = match pidinfo::<BSDInfo>(pid as i32, 0) {
            Ok(info) => info,
            Err(_) => {
                // Distinguish the expected exit race from an unreadable live
                // member. Only a proven disappearance/session change may be
                // skipped; an unreadable live member fails the whole snapshot.
                // SAFETY: getsid is a read-only recheck of the same PID.
                let current_session = unsafe { libc::getsid(pid as i32) };
                let errno = (current_session < 0)
                    .then(|| std::io::Error::last_os_error().raw_os_error())
                    .flatten();
                match classify_failed_session_member_query(
                    current_session,
                    errno,
                    session_id as i32,
                ) {
                    FailedSessionMemberQuery::SkipExitedOrMoved => continue,
                    FailedSessionMemberQuery::FailSnapshot => return None,
                }
            }
        };
        let pgid = info.pbi_pgid;
        if pgid <= 1 || pgid > i32::MAX as u32 {
            return None;
        }
        process_count += 1;
        if process_count > MAX_SESSION_PROCESSES {
            return None;
        }
        if !groups.contains_key(&pgid) && groups.len() == MAX_SESSION_GROUPS {
            return None;
        }
        let start = bsd_info_start_time(&info);
        groups.entry(pgid).or_default().push((pid, start));
    }

    let mut pinned = Vec::with_capacity(groups.len());
    for (pgid, mut members) in groups {
        members.sort_unstable();
        let group = PinnedProcessGroup {
            pgid,
            session_id,
            members,
        };
        // Groups that disappear during enumeration need no signal. Every group
        // returned still has at least one fully matching member identity.
        if pinned_process_group_is_current(&group) {
            pinned.push(group);
        }
    }
    Some(pinned)
}

/// Capture the terminal's distinct foreground process group. A live numeric
/// group leader must belong to the shell session; when a pipeline is
/// leaderless, the enumerated live members establish the same ownership.
#[cfg(target_os = "macos")]
pub(crate) fn pin_foreground_process_group(
    pty_master_fd: i32,
    shell_pid: u32,
) -> Option<PinnedProcessGroup> {
    let shell_session = i32::try_from(shell_pid).ok().filter(|pid| *pid > 1)?;
    // SAFETY: tcgetpgrp is a read-only query on the caller-owned PTY master.
    let foreground_pgid = unsafe { libc::tcgetpgrp(pty_master_fd) };
    if foreground_pgid <= 1 || foreground_pgid == shell_session {
        return None;
    }
    // SAFETY: getsid is a read-only process query. ESRCH is expected for a
    // leaderless pipeline; every live member is checked below in that case.
    let leader_session = unsafe { libc::getsid(foreground_pgid) };
    if leader_session < 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return None;
        }
    } else if leader_session != shell_session {
        return None;
    }
    let pgid = u32::try_from(foreground_pgid).ok()?;
    let group = pin_process_group(pgid, shell_pid)?;
    // Fail closed if foreground ownership changed while members were pinned.
    // SAFETY: same read-only query on the still-owned PTY master.
    (unsafe { libc::tcgetpgrp(pty_master_fd) } == foreground_pgid).then_some(group)
}

#[cfg(target_os = "macos")]
fn pty_master_matches_session(pty_master_fd: i32, session_id: u32) -> bool {
    let Ok(session_id) = i32::try_from(session_id) else {
        return false;
    };
    // SAFETY: tcgetsid is a read-only terminal query. The long-lived guard
    // owns this inherited descriptor, so it cannot have been closed/recycled.
    unsafe { libc::tcgetsid(pty_master_fd) == session_id }
}

/// Authenticate an inherited PTY master against its pinned session and return
/// a complete snapshot of all live process groups in that session.
#[cfg(target_os = "macos")]
pub(crate) fn pin_terminal_session_process_groups(
    pty_master_fd: i32,
    session_id: u32,
) -> Option<Vec<PinnedProcessGroup>> {
    let foreground = pin_foreground_process_group(pty_master_fd, session_id);
    if !pty_master_matches_session(pty_master_fd, session_id) && foreground.is_none() {
        return None;
    }
    let groups = pin_process_groups_in_session(session_id)?;
    let foreground = pin_foreground_process_group(pty_master_fd, session_id);
    if !pty_master_matches_session(pty_master_fd, session_id) && foreground.is_none() {
        return None;
    }
    Some(groups)
}

#[cfg(unix)]
pub(crate) fn pin_leader_process_group(
    pgid: u32,
    pinned_start: Option<u64>,
) -> Option<PinnedProcessGroup> {
    let pinned_start = pinned_start?;
    let pid = i32::try_from(pgid).ok().filter(|pid| *pid > 1)?;
    // SAFETY: getsid is a read-only query. A dead/recycled/nonleader target is
    // rejected again by `may_signal_group` and the member checks below.
    let session_id = unsafe { libc::getsid(pid) };
    if session_id <= 1
        || !may_signal_group(
            pid,
            Some(pinned_start),
            pid_start_time(pgid),
            is_process_group_leader(pid),
        )
    {
        return None;
    }
    Some(PinnedProcessGroup {
        pgid,
        session_id: session_id as u32,
        members: vec![(pgid, pinned_start)],
    })
}

/// Pin the shell's whole session-leader group for orderly teardown. The
/// original leader/start match is checked first, then all current group
/// members are captured so KILL remains authorized if the shell honors TERM
/// before one of its same-PGID descendants.
#[cfg(target_os = "macos")]
pub(crate) fn pin_session_process_group(
    pgid: u32,
    pinned_start: Option<u64>,
) -> Option<PinnedProcessGroup> {
    let leader = pin_leader_process_group(pgid, pinned_start)?;
    if leader.session_id != pgid {
        return None;
    }
    pin_process_group(pgid, pgid)
}

#[cfg(unix)]
fn serialize_member_pins(members: &[(u32, u64)]) -> String {
    members
        .iter()
        .map(|(pid, start)| format!("{pid}:{start}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(unix)]
fn parse_member_pins(serialized: &str) -> Option<Vec<(u32, u64)>> {
    // Bound the raw argument and member count so a manually invoked internal
    // subcommand cannot force unbounded parsing.
    if serialized.is_empty() || serialized.len() > 192 * 1024 {
        return None;
    }
    let mut members = Vec::new();
    for entry in serialized.split(',') {
        if members.len() == MAX_PINNED_GROUP_MEMBERS {
            return None;
        }
        let (pid, start) = entry.split_once(':')?;
        let pid = pid.parse::<u32>().ok()?;
        let start = start.parse::<u64>().ok()?;
        if pid <= 1 || pid > i32::MAX as u32 || start == 0 {
            return None;
        }
        members.push((pid, start));
    }
    members.sort_unstable();
    if members.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return None;
    }
    Some(members)
}

#[cfg(unix)]
enum PtyGuardMode {
    /// The caller already captured every target member immediately before
    /// spawning this short-lived orderly-teardown guard.
    Frozen,
    /// Long-lived shell guard. Refresh shell members and discover the current
    /// every session group only after authenticating the inherited PTY.
    Session { pty_master: Option<OwnedFd> },
}

#[cfg(unix)]
fn parse_guard_mode(arg: &str) -> Option<PtyGuardMode> {
    if arg == "frozen" {
        return Some(PtyGuardMode::Frozen);
    }
    let fd = arg.strip_prefix("session:")?;
    if fd == "none" {
        return Some(PtyGuardMode::Session { pty_master: None });
    }
    let fd = fd.parse::<i32>().ok().filter(|fd| *fd >= 3)?;
    // SAFETY: F_GETFD validates the inherited descriptor before OwnedFd takes
    // responsibility for closing it at guard exit.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return None;
    }
    // SAFETY: the descriptor is an inherited duplicate dedicated to this
    // guard process and has not been wrapped elsewhere here.
    let pty_master = unsafe { OwnedFd::from_raw_fd(fd) };
    Some(PtyGuardMode::Session {
        pty_master: Some(pty_master),
    })
}

#[cfg(unix)]
pub fn run_pty_guard_from_args(args: &[String]) -> i32 {
    if args.len() != 7 {
        return 2;
    }
    let Some(parent_pid) = args.get(2).and_then(|arg| arg.parse::<u32>().ok()) else {
        return 2;
    };
    let Some(child_pgid) = args.get(3).and_then(|arg| arg.parse::<u32>().ok()) else {
        return 2;
    };
    let Some(session_id) = args.get(4).and_then(|arg| arg.parse::<u32>().ok()) else {
        return 2;
    };
    let Some(members) = args.get(5).and_then(|arg| parse_member_pins(arg)) else {
        return 2;
    };
    let Some(mode) = args.get(6).and_then(|arg| parse_guard_mode(arg)) else {
        return 2;
    };
    if parent_pid <= 1 || child_pgid <= 1 || session_id <= 1 {
        return 2;
    }
    let group = PinnedProcessGroup {
        pgid: child_pgid,
        session_id,
        members,
    };

    run_pty_guard(parent_pid, group, mode, true)
}

/// Liveness poll. Parent death, shell-group death, and the control pipe stay
/// on this cadence. It does not walk the process table.
#[cfg(unix)]
const GUARD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Minimum gap between full process-table walks while a long-lived guard is
/// idle. A new session group is noticed within this bound. Shutdown does not
/// wait for it: it re-pins the foreground group and every live session group
/// before signaling.
#[cfg(unix)]
const SESSION_SNAPSHOT_REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

/// Throttles `ProcFilter::All` session snapshots in the long-lived PTY guard.
///
/// Foreground pinning stays inside the session snapshot (and runs again on
/// shutdown). A failed snapshot keeps the previous groups but still consumes
/// the interval, so a walk that returns nothing cannot retry on the next
/// 500 ms tick.
#[cfg(unix)]
struct SessionSnapshotRefresh {
    last_full_scan: Option<std::time::Instant>,
}

#[cfg(unix)]
impl SessionSnapshotRefresh {
    fn on_tick(
        &mut self,
        now: std::time::Instant,
        observed_groups: &mut Vec<PinnedProcessGroup>,
        snapshot: impl FnOnce() -> Option<Vec<PinnedProcessGroup>>,
    ) {
        if let Some(last) = self.last_full_scan
            && now.saturating_duration_since(last) < SESSION_SNAPSHOT_REFRESH
        {
            return;
        }
        // Record the caller's tick time, not the time after the walk, so a
        // virtual clock and a slow enumeration both honor the same gap.
        self.last_full_scan = Some(now);
        if let Some(groups) = snapshot() {
            *observed_groups = groups;
        }
    }
}

#[cfg(unix)]
fn run_pty_guard(
    parent_pid: u32,
    group: PinnedProcessGroup,
    mode: PtyGuardMode,
    monitor_control_pipe: bool,
) -> i32 {
    let observed_groups = observe_session_groups(&group, &mode).unwrap_or_default();
    run_pty_guard_with_groups(
        parent_pid,
        group,
        mode,
        monitor_control_pipe,
        observed_groups,
    )
}

#[cfg(unix)]
fn run_pty_guard_with_groups(
    parent_pid: u32,
    group: PinnedProcessGroup,
    mode: PtyGuardMode,
    monitor_control_pipe: bool,
    mut observed_groups: Vec<PinnedProcessGroup>,
) -> i32 {
    if monitor_control_pipe {
        set_control_pipe_nonblocking();
    }
    // The caller enumerated immediately before this loop. That walk starts
    // the refresh interval, so the first liveness ticks do not scan again.
    let mut session_refresh = SessionSnapshotRefresh {
        last_full_scan: Some(std::time::Instant::now()),
    };
    loop {
        if !parent_still_attached(parent_pid) {
            shutdown_guard_targets(&group, &mode, &observed_groups);
            return 0;
        }
        session_refresh.on_tick(std::time::Instant::now(), &mut observed_groups, || {
            observe_session_groups(&group, &mode)
        });
        if !process_group_alive(&group)
            && !guard_session_still_authenticated(&group, &mode, &observed_groups)
        {
            return 0;
        }
        if monitor_control_pipe && control_pipe_closed() {
            // Clean TerminalState teardown closes this pipe. Own the complete
            // TERM -> grace -> KILL ladder in this external watcher so an
            // immediate app exit cannot cancel the GPUI executor's fallback.
            shutdown_guard_targets(&group, &mode, &observed_groups);
            return 0;
        }
        std::thread::sleep(GUARD_POLL_INTERVAL);
    }
}

#[cfg(target_os = "macos")]
fn observe_session_groups(
    group: &PinnedProcessGroup,
    mode: &PtyGuardMode,
) -> Option<Vec<PinnedProcessGroup>> {
    match mode {
        PtyGuardMode::Frozen => None,
        PtyGuardMode::Session { pty_master } => pty_master.as_ref().and_then(|master| {
            pin_terminal_session_process_groups(master.as_raw_fd(), group.session_id)
        }),
    }
}

#[cfg(target_os = "macos")]
fn guard_session_still_authenticated(
    group: &PinnedProcessGroup,
    mode: &PtyGuardMode,
    observed_groups: &[PinnedProcessGroup],
) -> bool {
    match mode {
        PtyGuardMode::Frozen => false,
        PtyGuardMode::Session { pty_master } => pty_master.as_ref().is_some_and(|master| {
            let fd = master.as_raw_fd();
            pty_master_matches_session(fd, group.session_id)
                || pin_terminal_session_process_groups(fd, group.session_id).is_some()
                || observed_groups.iter().any(pinned_process_group_is_current)
        }),
    }
}

/// The program and leading arguments a guard process is started with: this
/// executable in production, a libtest entrypoint in the spawn-path tests.
#[cfg(unix)]
struct GuardLauncher {
    program: std::path::PathBuf,
    leading_args: Vec<String>,
    env: Vec<(String, String)>,
}

#[cfg(unix)]
impl GuardLauncher {
    #[cfg_attr(test, allow(dead_code))]
    fn current_exe() -> Option<Self> {
        match std::env::current_exe() {
            Ok(program) => Some(Self {
                program,
                leading_args: Vec::new(),
                env: Vec::new(),
            }),
            Err(err) => {
                log::debug!("parent_guard: current_exe unavailable ({err}); PTY guard not started");
                None
            }
        }
    }
}

/// Start the long-lived session-mode guard for a pane that has just gone
/// live (issue #1129).
///
/// Never call it on the GPUI thread: it walks the process table, and it
/// spawns through [`paneflow_process::spawn_piped`], which waits for every
/// spawn already in flight before it creates the control pipe. The view runs
/// it on the background executor right after promotion and installs the
/// handle when it arrives.
#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
pub fn spawn_pty_guard(
    child_pgid: u32,
    child_proc_start: Option<u64>,
    pty_master_fd: i32,
) -> Option<PtyGuardHandle> {
    let launcher = GuardLauncher::current_exe()?;
    spawn_session_guard_with(&launcher, child_pgid, child_proc_start, pty_master_fd)
}

#[cfg(target_os = "macos")]
fn spawn_session_guard_with(
    launcher: &GuardLauncher,
    child_pgid: u32,
    child_proc_start: Option<u64>,
    pty_master_fd: i32,
) -> Option<PtyGuardHandle> {
    let pinned_start = child_proc_start.or_else(|| pid_start_time(child_pgid));
    let group = pin_session_process_group(child_pgid, pinned_start)?;
    let mut cmd = guard_command(launcher, &group)?;
    // The guard's own copy of the master, at `PTY_GUARD_MASTER_FD`. `cmd`
    // owns the parent's copy and closes it when this function returns, right
    // after the spawn.
    let mode_arg = match pass_pty_master_to_child(&mut cmd, pty_master_fd) {
        Ok(()) => format!("session:{PTY_GUARD_MASTER_FD}"),
        Err(err) => {
            log::warn!(
                "parent_guard: cannot duplicate PTY master for pgid {}: {err}; foreground hard-death guard unavailable",
                group.pgid
            );
            "session:none".to_string()
        }
    };
    cmd.arg(mode_arg);
    let control = paneflow_process::Pipes {
        stdin: true,
        ..paneflow_process::Pipes::default()
    };
    match paneflow_process::spawn_piped(&mut cmd, control) {
        Ok(mut child) => {
            let Some(control) = child.stdin.take() else {
                log::warn!(
                    "parent_guard: PTY guard for pgid {} has no control pipe",
                    group.pgid
                );
                let _ = child.kill();
                reap_guard(child, None);
                return None;
            };
            let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            reap_guard(child, Some(std::sync::Arc::clone(&exited)));
            Some(PtyGuardHandle {
                _control: control,
                exited,
            })
        }
        Err(err) => {
            log::warn!(
                "parent_guard: failed to start PTY guard for pgid {}: {err}",
                group.pgid
            );
            None
        }
    }
}

/// Start a frozen-mode teardown guard for `group` from `TerminalState::Drop`,
/// before the app sends its own SIGTERM, so the TERM-to-KILL ladder survives
/// an immediate app exit. Returns whether the guard started.
///
/// This runs on the GPUI thread, so it has no control pipe (issue #1129):
/// stdin is `/dev/null`, the guard reads EOF on its first poll and runs the
/// ladder at once, which is what closing the pipe right after SIGTERM used
/// to trigger.
#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn spawn_process_group_guard(group: PinnedProcessGroup) -> bool {
    GuardLauncher::current_exe().is_some_and(|launcher| spawn_frozen_guard_with(&launcher, &group))
}

#[cfg(unix)]
fn spawn_frozen_guard_with(launcher: &GuardLauncher, group: &PinnedProcessGroup) -> bool {
    let Some(mut cmd) = guard_command(launcher, group) else {
        return false;
    };
    cmd.arg("frozen").stdin(paneflow_process::Stdio::Null);
    match cmd.start() {
        Ok(child) => {
            reap_guard(child, None);
            true
        }
        Err(err) => {
            log::warn!(
                "parent_guard: failed to start PTY guard for pgid {}: {err}",
                group.pgid
            );
            false
        }
    }
}

/// The guard command for `group`, every argument but the mode, or `None`
/// when the group no longer matches its pins.
#[cfg(unix)]
fn guard_command(
    launcher: &GuardLauncher,
    group: &PinnedProcessGroup,
) -> Option<paneflow_process::Command> {
    if !pinned_process_group_is_current(group) {
        log::warn!(
            "parent_guard: cannot validate PTY group {}; guard not started",
            group.pgid
        );
        return None;
    }
    let mut cmd = paneflow_process::Command::new(&launcher.program);
    cmd.args(&launcher.leading_args)
        .envs(launcher.env.iter().map(|(key, value)| (key, value)))
        .arg(PTY_GUARD_SUBCOMMAND)
        .arg(std::process::id().to_string())
        .arg(group.pgid.to_string())
        .arg(group.session_id.to_string())
        .arg(serialize_member_pins(&group.members))
        .stdout(paneflow_process::Stdio::Null)
        .stderr(paneflow_process::Stdio::Null)
        .process_group(0);
    Some(cmd)
}

/// Reap a guard on its own thread and record its exit in `exited`.
#[cfg(unix)]
fn reap_guard(
    mut child: paneflow_process::Child,
    exited: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) {
    std::thread::spawn(move || {
        let _ = child.wait();
        if let Some(exited) = exited {
            exited.store(true, std::sync::atomic::Ordering::Release);
        }
    });
}

/// Give the child `cmd` starts its own copy of `pty_master_fd`, at
/// [`PTY_GUARD_MASTER_FD`], and give it to no other child (issues #1123,
/// #1126).
///
/// The parent's copy is close-on-exec from the moment it exists
/// (`F_DUPFD_CLOEXEC`), so no other child inherits it. `cmd` owns it and
/// hands it to its child with a `posix_spawn` `dup2` file action, so the
/// guard spawns without a fork. It sits above the guard's number, so that
/// `dup2` never lands on itself.
#[cfg(unix)]
fn pass_pty_master_to_child(
    cmd: &mut paneflow_process::Command,
    pty_master_fd: i32,
) -> std::io::Result<()> {
    // SAFETY: fcntl only reads `pty_master_fd`. F_DUPFD_CLOEXEC returns a
    // fresh descriptor above `PTY_GUARD_MASTER_FD`, or -1.
    let duplicate = unsafe {
        libc::fcntl(
            pty_master_fd,
            libc::F_DUPFD_CLOEXEC,
            PTY_GUARD_MASTER_FD + 1,
        )
    };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: this is the only owner of the fresh duplicate in the parent.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    cmd.pass_fd(duplicate, PTY_GUARD_MASTER_FD);
    Ok(())
}

/// A test child that is killed and reaped however the test ends, so a failed
/// assertion never leaves it running.
#[cfg(all(test, target_os = "macos"))]
pub(crate) struct KillOnDrop(pub(crate) std::process::Child);

#[cfg(all(test, target_os = "macos"))]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // Already reaped: killing now could signal a reused pid.
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Device numbers of the vnode descriptors `pid` holds, as `(fd, rdev)`.
///
/// A PTY master's `rdev` names its pair, so this shows which process still
/// holds a pane's master (issue #1123), the way `lsof` does.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn vnode_devices_held_by(pid: i32) -> Vec<(i32, u32)> {
    /// `struct proc_fileinfo` from `<sys/proc_info.h>`, for layout only.
    #[allow(dead_code)]
    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: libc::off_t,
        fi_type: i32,
        fi_guardflags: u32,
    }
    /// `struct vnode_fdinfo` from `<sys/proc_info.h>`.
    #[allow(dead_code)]
    #[repr(C)]
    struct VnodeFdInfo {
        pfi: ProcFileInfo,
        pvi: libc::vnode_info,
    }
    const PROC_PIDFDVNODEINFO: i32 = 1;

    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks only for the size of the descriptor table.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    assert!(needed > 0, "PROC_PIDLISTFDS size for pid {pid}");
    // Headroom for descriptors opened between the two calls.
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(needed as usize / entry + 64);
    // SAFETY: the buffer holds `capacity` entries and the kernel writes at
    // most the byte count it is given.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.capacity() * entry) as i32,
        )
    };
    assert!(written > 0, "PROC_PIDLISTFDS for pid {pid}");
    // SAFETY: the kernel initialized `written` bytes of whole entries.
    unsafe { fds.set_len(written as usize / entry) };

    let mut devices = Vec::new();
    for fd in fds {
        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            continue;
        }
        let mut info = std::mem::MaybeUninit::<VnodeFdInfo>::zeroed();
        let size = std::mem::size_of::<VnodeFdInfo>() as i32;
        // SAFETY: `info` is a writable buffer of exactly `size` bytes.
        let got = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEINFO,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if got == size {
            // SAFETY: the kernel filled the whole struct.
            let info = unsafe { info.assume_init() };
            devices.push((fd.proc_fd, info.pvi.vi_stat.vst_rdev));
        }
    }
    devices
}

#[cfg(unix)]
fn parent_still_attached(parent_pid: u32) -> bool {
    // SAFETY: getppid has no preconditions.
    unsafe { libc::getppid() as u32 == parent_pid }
}

#[cfg(unix)]
fn is_process_group_leader(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: getpgid is a pure query; ESRCH yields -1, which is never a
    // positive pid, so a recycled-or-dead pid is not reported as leader.
    unsafe { libc::getpgid(pid) == pid }
}

#[cfg(unix)]
fn process_group_alive(group: &PinnedProcessGroup) -> bool {
    let Ok(pgid) = i32::try_from(group.pgid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only probes process-group existence.
    let rc = unsafe { libc::kill(-pgid, 0) };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return false;
    }
    pinned_process_group_is_current(group)
}

#[cfg(unix)]
/// Signal a pinned group only while a captured member still has the same PID,
/// start time, PGID, and terminal session. Unlike leader-only authorization,
/// this remains safe and effective after a pipeline's group leader exits.
pub(crate) fn signal_pinned_process_group(group: &PinnedProcessGroup, signal: i32) -> bool {
    let Ok(pgid) = i32::try_from(group.pgid) else {
        return false;
    };
    if !pinned_process_group_is_current(group) {
        return false;
    }
    // SAFETY: negative PGID targets the process group. A member's full process
    // identity and session ownership were just confirmed.
    unsafe { libc::kill(-pgid, signal) == 0 }
}

#[cfg(all(unix, test))]
/// Complete a guarded TERM -> 100 ms -> KILL ladder. Both signals revalidate
/// the pinned member identity; missing probes fail closed.
pub(crate) fn shutdown_pinned_process_group(group: &PinnedProcessGroup) -> bool {
    if !signal_pinned_process_group(group, libc::SIGTERM) {
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    if process_group_alive(group) {
        signal_pinned_process_group(group, libc::SIGKILL);
    }
    true
}

#[cfg(unix)]
fn shutdown_guard_targets(
    origin: &PinnedProcessGroup,
    mode: &PtyGuardMode,
    observed_groups: &[PinnedProcessGroup],
) -> bool {
    let targets = match mode {
        PtyGuardMode::Frozen => vec![origin.clone()],
        PtyGuardMode::Session { pty_master } => {
            // A long-lived guard's spawn-time member list is intentionally not
            // enough: the shell may have created descendants since then. The
            // original shell leader/start is the authority to refresh.
            let shell_start = origin
                .members
                .iter()
                .find_map(|(pid, start)| (*pid == origin.pgid).then_some(*start));
            if let Some(master) = pty_master.as_ref() {
                let fd = master.as_raw_fd();
                // The inherited, non-recyclable PTY description plus pinned
                // session authorizes refreshing every live group even if the
                // numeric shell leader has already exited. If terminal queries
                // are no longer available, retain only cached groups whose
                // member identities still match exactly.
                let targets = pin_terminal_session_process_groups(fd, origin.session_id)
                    .unwrap_or_else(|| {
                        observed_groups
                            .iter()
                            .filter(|group| pinned_process_group_is_current(group))
                            .cloned()
                            .collect()
                    });
                if targets.is_empty() {
                    return false;
                }
                targets
            } else {
                // Without the inherited PTY authority, retain the strict
                // original-leader rule and fail closed after leader loss.
                let Some(shell) = pin_session_process_group(origin.pgid, shell_start) else {
                    return false;
                };
                vec![shell]
            }
        }
    };

    let mut signaled = false;
    for target in &targets {
        signaled |= signal_pinned_process_group(target, libc::SIGTERM);
    }
    if !signaled {
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    for target in &targets {
        if process_group_alive(target) {
            signal_pinned_process_group(target, libc::SIGKILL);
        }
    }
    true
}

#[cfg(unix)]
fn set_control_pipe_nonblocking() {
    // SAFETY: fcntl on stdin fd 0. Failure is non-fatal; the guard still has
    // parent/process-group polling and will exit on parent death.
    unsafe {
        let flags = libc::fcntl(0, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(0, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(unix)]
fn control_pipe_closed() -> bool {
    let mut byte = [0u8; 1];
    // SAFETY: reads at most one byte into a valid stack buffer from stdin fd 0.
    let rc = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
    if rc == 0 {
        return true;
    }
    if rc > 0 {
        return false;
    }
    let err = std::io::Error::last_os_error().raw_os_error();
    !matches!(err, Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK || code == libc::EINTR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const SUBPROCESS_GUARD_GROUP_ENV: &str = "PANEFLOW_TEST_GUARD_GROUP";
    const SUBPROCESS_GUARD_MASTER_ENV: &str = "PANEFLOW_TEST_GUARD_MASTER";

    /// Wall-clock budget for one fixture wait: readiness lines, shells
    /// honoring SIGTERM, and process groups vanishing after SIGKILL.
    ///
    /// Like the #562 / #564 budget in `workspace/git.rs`, a budget no
    /// scheduler stall reaches keeps the assertions about the mechanism: the
    /// wait ends on the observed exit or disappearance, never on the deadline.
    ///
    /// Issue #568: `shell did not honor SIGTERM` was not a slow runner, and no
    /// budget fixes it. The fixtures whose leader runs `trap 'exit 42' TERM`
    /// and then `(...) & wait` run under `/bin/dash`, not `sh`:
    /// - macOS `/bin/sh` is bash 3.2, whose `wait` only acts on a trapped
    ///   signal that arrives once it is blocked. A TERM landing between the
    ///   fork and that point stays pending, and `wait` then blocks forever on
    ///   a descendant that ignores TERM.
    /// - Polling with `while :; do sleep 0.05; done` instead adds up to one
    ///   poll interval plus a fork/exec of `sleep` (about 60 ms measured, with
    ///   no stall), which races the guard's KILL 100 ms after its TERM.
    /// - dash's `wait` blocks signals, checks for a pending one, and only then
    ///   suspends, so it has neither problem: the trap runs at once.
    const FIXTURE_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

    /// Issue #1123: the guard's copy of the PTY master reaches the guard at
    /// the number it is told, and no child spawned while that copy exists.
    /// The copy was a plain `dup`, inheritable by any child another thread
    /// spawned before the guard's `spawn` returned.
    #[cfg(target_os = "macos")]
    #[test]
    fn guard_pty_master_copy_reaches_only_the_guard_child() {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: openpty initializes both fd outputs using default terminal
        // settings when termios/winsize are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty returned two fresh descriptors this test owns.
        let (master, _slave) = unsafe {
            (
                OwnedFd::from_raw_fd(master_fd),
                OwnedFd::from_raw_fd(slave_fd),
            )
        };
        // As portable-pty leaves the engine's copy.
        // SAFETY: F_SETFD only changes this test's own descriptors' flags.
        unsafe {
            libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let device = vnode_devices_held_by(std::process::id() as i32)
            .into_iter()
            .find(|(fd, _)| *fd == master.as_raw_fd())
            .map(|(_, rdev)| rdev)
            .expect("the test's master is listed");

        let sleeper = || {
            let mut command = Command::new("/bin/sleep");
            command
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command
        };
        let mut guard_command = paneflow_process::Command::from(&sleeper());
        guard_command
            .stdin(paneflow_process::Stdio::Null)
            .stdout(paneflow_process::Stdio::Null)
            .stderr(paneflow_process::Stdio::Null);
        pass_pty_master_to_child(&mut guard_command, master.as_raw_fd())
            .expect("duplicate the master for the guard");
        // A std spawn, which inherits every descriptor not marked
        // close-on-exec, lands between the duplicate and the guard's own
        // spawn.
        let bystander =
            KillOnDrop(paneflow_process::spawn(&mut sleeper()).expect("spawn bystander"));
        let mut guard = guard_command.start().expect("spawn guard stand-in");
        // Closes the parent's copy.
        drop(guard_command);

        let bystander_held = vnode_devices_held_by(bystander.0.id() as i32);
        let guard_held = vnode_devices_held_by(guard.id() as i32);
        let _ = guard.kill();
        let _ = guard.wait();
        let own_held = vnode_devices_held_by(std::process::id() as i32);

        let leaked: Vec<i32> = bystander_held
            .iter()
            .filter(|(_, rdev)| *rdev == device)
            .map(|(fd, _)| *fd)
            .collect();
        assert!(
            leaked.is_empty(),
            "a child spawned while the guard's copy existed holds the master at fds {leaked:?}"
        );
        let guard_masters: Vec<i32> = guard_held
            .iter()
            .filter(|(_, rdev)| *rdev == device)
            .map(|(fd, _)| *fd)
            .collect();
        assert_eq!(
            guard_masters,
            vec![PTY_GUARD_MASTER_FD],
            "the guard holds its master exactly at the number its argument names"
        );
        let parent_masters: Vec<i32> = own_held
            .iter()
            .filter(|(_, rdev)| *rdev == device)
            .map(|(fd, _)| *fd)
            .collect();
        assert_eq!(
            parent_masters,
            vec![master.as_raw_fd()],
            "the parent's copy for the guard is closed once the guard has started"
        );
    }

    #[test]
    fn failed_session_member_query_skips_only_exit_or_positive_session_change() {
        use FailedSessionMemberQuery::{FailSnapshot, SkipExitedOrMoved};

        assert_eq!(
            classify_failed_session_member_query(-1, Some(libc::ESRCH), 100),
            SkipExitedOrMoved
        );
        assert_eq!(
            classify_failed_session_member_query(200, None, 100),
            SkipExitedOrMoved
        );
        assert_eq!(
            classify_failed_session_member_query(-1, Some(libc::EPERM), 100),
            FailSnapshot
        );
        assert_eq!(
            classify_failed_session_member_query(100, None, 100),
            FailSnapshot
        );
    }

    /// Several 500 ms liveness ticks may enumerate the process table at most
    /// once per [`SESSION_SNAPSHOT_REFRESH`]. Scanning on every tick fails.
    #[cfg(unix)]
    #[test]
    fn guard_refresh_does_not_scan_all_processes_every_tick() {
        use std::time::Instant;

        assert!(
            SESSION_SNAPSHOT_REFRESH > GUARD_POLL_INTERVAL,
            "full session enumeration must not run on every liveness tick"
        );
        assert!(
            SESSION_SNAPSHOT_REFRESH <= std::time::Duration::from_secs(5),
            "a new session process group must be noticed within a few seconds"
        );
        let polls_per_refresh =
            u32::try_from(SESSION_SNAPSHOT_REFRESH.as_nanos() / GUARD_POLL_INTERVAL.as_nanos())
                .expect("refresh interval spans a countable number of polls");
        assert!(
            polls_per_refresh >= 2,
            "refresh interval must cover more than one 500 ms tick"
        );

        let tick_count = polls_per_refresh * 2 + 1;
        let start = Instant::now();
        let mut refresh = SessionSnapshotRefresh {
            last_full_scan: None,
        };
        let mut observed = Vec::new();
        let mut scan_ticks = Vec::new();
        for tick in 0..tick_count {
            let now = start + GUARD_POLL_INTERVAL * tick;
            refresh.on_tick(now, &mut observed, || {
                scan_ticks.push(tick);
                Some(Vec::new())
            });
        }

        assert!(
            scan_ticks
                .windows(2)
                .all(|pair| pair[1] - pair[0] >= polls_per_refresh),
            "more than one full enumeration per {SESSION_SNAPSHOT_REFRESH:?} refresh interval: ticks {scan_ticks:?}"
        );
        let min_scans = 1 + (tick_count - 1) / polls_per_refresh;
        assert!(
            scan_ticks.len() >= min_scans as usize,
            "expected a full enumeration at least once per {SESSION_SNAPSHOT_REFRESH:?}: ticks {scan_ticks:?}"
        );
        assert!(
            scan_ticks.len() < tick_count as usize,
            "snapshot ran on every 500 ms tick: {scan_ticks:?}"
        );

        // The running guard is armed from the snapshot taken just before the
        // loop, so every liveness tick strictly inside that interval must
        // not walk the process table again.
        let armed = Instant::now();
        let mut warm = SessionSnapshotRefresh {
            last_full_scan: Some(armed),
        };
        let mut warm_scans = 0u32;
        let mut warm_observed = Vec::new();
        for tick in 0..polls_per_refresh {
            warm.on_tick(
                armed + GUARD_POLL_INTERVAL * tick,
                &mut warm_observed,
                || {
                    warm_scans += 1;
                    Some(Vec::new())
                },
            );
        }
        assert_eq!(
            warm_scans, 0,
            "armed guard enumerated every process on a 500 ms tick inside one refresh interval"
        );
    }

    /// Entry point used by parent-death tests. The outer test launches this
    /// exact libtest case under a disposable `/bin/sh` parent, then SIGKILLs
    /// that parent. This exercises a real, separately exec'd guard process.
    #[cfg(target_os = "macos")]
    #[test]
    fn pty_guard_subprocess_entrypoint() {
        use std::io::Write;

        let Ok(spec) = std::env::var(SUBPROCESS_GUARD_GROUP_ENV) else {
            return;
        };
        let mut fields = spec.splitn(3, '|');
        let pgid = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .expect("guard test pgid");
        let session_id = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .expect("guard test session");
        let members = fields
            .next()
            .and_then(parse_member_pins)
            .expect("guard test pins");
        let master = std::env::var(SUBPROCESS_GUARD_MASTER_ENV).expect("guard test master");
        let mode =
            parse_guard_mode(&format!("session:{master}")).expect("guard test inherited master");
        let parent_pid = unsafe { libc::getppid() } as u32;
        let group = PinnedProcessGroup {
            pgid,
            session_id,
            members,
        };
        let observed_groups = observe_session_groups(&group, &mode).unwrap_or_default();

        println!("PANEFLOW_GUARD_READY");
        std::io::stdout().flush().expect("flush guard readiness");
        assert_eq!(
            run_pty_guard_with_groups(parent_pid, group, mode, false, observed_groups),
            0
        );
    }

    #[cfg(target_os = "macos")]
    struct DisposableGuardParent {
        child: std::process::Child,
        guard_pid: i32,
    }

    #[cfg(target_os = "macos")]
    impl DisposableGuardParent {
        fn kill_parent(&mut self) {
            // SAFETY: this is the disposable shell spawned by the test.
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for DisposableGuardParent {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            // Test-only cleanup if the guard did not exit after its parent.
            if self.guard_pid > 1 {
                unsafe {
                    libc::kill(self.guard_pid, libc::SIGKILL);
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn spawn_disposable_guard_parent(
        group: &PinnedProcessGroup,
        pty_master_fd: Option<i32>,
    ) -> DisposableGuardParent {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let inherited_master = pty_master_fd.map(|fd| {
            // SAFETY: dup creates a non-CLOEXEC descriptor for the shell and
            // its guard child to inherit. The root test drops its copy below.
            let duplicate = unsafe { libc::dup(fd) };
            assert!(duplicate >= 3, "duplicate guard test PTY master");
            // SAFETY: this is the only owner in the root test process.
            unsafe { OwnedFd::from_raw_fd(duplicate) }
        });
        let master_arg = inherited_master
            .as_ref()
            .map_or_else(|| "none".to_string(), |fd| fd.as_raw_fd().to_string());
        let spec = format!(
            "{}|{}|{}",
            group.pgid,
            group.session_id,
            serialize_member_pins(&group.members)
        );
        let exe = std::env::current_exe().expect("current test executable");
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "\"$1\" --exact agents::parent_guard::tests::pty_guard_subprocess_entrypoint --nocapture & echo PANEFLOW_GUARD_PID:$!; wait",
                "paneflow-guard-parent",
            ])
            .arg(exe)
            .env(SUBPROCESS_GUARD_GROUP_ENV, spec)
            .env(SUBPROCESS_GUARD_MASTER_ENV, master_arg)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn disposable guard parent");
        let mut guard_parent = DisposableGuardParent {
            child,
            guard_pid: 0,
        };
        drop(inherited_master);

        let mut stdout = guard_parent
            .child
            .stdout
            .take()
            .expect("guard parent stdout");
        let stdout_fd = stdout.as_raw_fd();
        // SAFETY: set nonblocking on this test-owned pipe so a broken helper
        // cannot hang the suite indefinitely.
        unsafe {
            let flags = libc::fcntl(stdout_fd, libc::F_GETFL);
            assert!(flags >= 0);
            assert_eq!(
                libc::fcntl(stdout_fd, libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );
        }
        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        let mut output = Vec::new();
        let mut buffer = [0u8; 1024];
        let mut guard_pid = None;
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => panic!(
                    "guard parent exited before readiness: {}",
                    String::from_utf8_lossy(&output)
                ),
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read guard readiness: {error}"),
            }
            let text = String::from_utf8_lossy(&output);
            if guard_pid.is_none()
                && let Some(value) = text
                    .split_whitespace()
                    .find_map(|word| word.strip_prefix("PANEFLOW_GUARD_PID:"))
            {
                guard_pid = value.parse::<i32>().ok();
            }
            if text.contains("PANEFLOW_GUARD_READY")
                && let Some(guard_pid) = guard_pid
            {
                guard_parent.guard_pid = guard_pid;
                return guard_parent;
            }
            assert!(
                Instant::now() < deadline,
                "guard subprocess readiness timed out: {text}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn pty_guard_rejects_invalid_args() {
        let args = vec![
            "paneflow".to_string(),
            PTY_GUARD_SUBCOMMAND.to_string(),
            "bad".to_string(),
            "2".to_string(),
        ];
        assert_eq!(run_pty_guard_from_args(&args), 2);
    }

    #[cfg(unix)]
    #[test]
    fn pty_guard_rejects_unparseable_start_pin() {
        let args = vec![
            "paneflow".to_string(),
            PTY_GUARD_SUBCOMMAND.to_string(),
            "2".to_string(),
            "3".to_string(),
            "3".to_string(),
            "3:not-a-start".to_string(),
            "frozen".to_string(),
        ];
        assert_eq!(run_pty_guard_from_args(&args), 2);
    }

    #[cfg(unix)]
    #[test]
    fn pty_guard_rejects_missing_start_pin() {
        let args = vec![
            "paneflow".to_string(),
            PTY_GUARD_SUBCOMMAND.to_string(),
            "2".to_string(),
            "3".to_string(),
            "3".to_string(),
        ];
        assert_eq!(run_pty_guard_from_args(&args), 2);
    }

    #[cfg(unix)]
    #[test]
    fn pty_guard_exits_immediately_when_group_is_gone() {
        // parent_pid is this process (not getppid), so the parent looks
        // detached; child_pgid is unused so kill(-pgid,0) is ESRCH. Must
        // return without signaling and without polling.
        let args = vec![
            "paneflow".to_string(),
            PTY_GUARD_SUBCOMMAND.to_string(),
            std::process::id().to_string(),
            "999999".to_string(),
            "999999".to_string(),
            "999999:1".to_string(),
            "frozen".to_string(),
        ];
        assert_eq!(run_pty_guard_from_args(&args), 0);
    }

    /// Helper whose stdin is the control pipe. Returns immediately unless the
    /// parent test set [`SUBPROCESS_GUARD_GROUP_ENV`]. `monitor_control_pipe`
    /// is true, unlike [`pty_guard_subprocess_entrypoint`].
    #[cfg(unix)]
    #[test]
    fn pty_guard_control_pipe_entrypoint() {
        use std::io::Write;

        let Ok(spec) = std::env::var(SUBPROCESS_GUARD_GROUP_ENV) else {
            return;
        };
        let mut fields = spec.splitn(3, '|');
        let pgid = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .expect("guard test pgid");
        let session_id = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .expect("guard test session");
        let members = fields
            .next()
            .and_then(parse_member_pins)
            .expect("guard test pins");
        // SAFETY: getppid has no preconditions. This helper's parent is the
        // test process that holds the control pipe.
        let parent_pid = unsafe { libc::getppid() } as u32;
        let group = PinnedProcessGroup {
            pgid,
            session_id,
            members,
        };
        println!("PANEFLOW_GUARD_READY");
        std::io::stdout().flush().expect("flush guard readiness");
        assert_eq!(
            run_pty_guard_with_groups(parent_pid, group, PtyGuardMode::Frozen, true, Vec::new()),
            0
        );
    }

    /// Set by [`test_launcher`] so [`pty_guard_launcher_entrypoint`] runs.
    const GUARD_LAUNCHER_ENV: &str = "PANEFLOW_TEST_GUARD_LAUNCHER";

    /// The guard the spawn-path tests start: this test binary, which runs
    /// the real guard verb on the arguments after `--`. The other arguments
    /// are libtest filters that `--exact` matches to no test.
    #[cfg(unix)]
    #[test]
    fn pty_guard_launcher_entrypoint() {
        if std::env::var_os(GUARD_LAUNCHER_ENV).is_none() {
            return;
        }
        let args: Vec<String> = std::env::args().collect();
        let verb = args
            .iter()
            .position(|arg| arg == PTY_GUARD_SUBCOMMAND)
            .expect("guard verb in the launcher arguments");
        let guard_args: Vec<String> = std::iter::once(args[0].clone())
            .chain(args[verb..].iter().cloned())
            .collect();
        std::process::exit(run_pty_guard_from_args(&guard_args));
    }

    /// Starts guards through [`pty_guard_launcher_entrypoint`] instead of
    /// the production executable, which a test binary is not.
    #[cfg(unix)]
    fn test_launcher() -> GuardLauncher {
        GuardLauncher {
            program: std::env::current_exe().expect("current test executable"),
            leading_args: [
                "--exact",
                "agents::parent_guard::tests::pty_guard_launcher_entrypoint",
                "--nocapture",
                "--",
            ]
            .map(String::from)
            .to_vec(),
            env: vec![(GUARD_LAUNCHER_ENV.to_string(), "1".to_string())],
        }
    }

    /// Wait for `child` to exit within the fixture budget.
    #[cfg(unix)]
    fn wait_for_fixture_exit(
        child: &mut std::process::Child,
        what: &str,
    ) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + FIXTURE_WAIT_BUDGET;
        loop {
            if let Some(status) = child.try_wait().expect("try_wait fixture") {
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Issue #1129: a close-time guard has `/dev/null` for stdin, no control
    /// pipe. It reads EOF on its first poll and runs the TERM-to-KILL ladder
    /// on its pinned group, through the real spawn path.
    #[cfg(target_os = "macos")]
    #[test]
    fn close_time_guard_with_null_stdin_kills_its_pinned_group() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid runs in the forked child before exec and gives this
        // fixture its own process group, so the guard can signal -pid.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn TERM-ignoring group");
        let pgid = child.id();
        let _cleanup = KillGroupOnDrop(pgid as i32);
        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut ready)
            .expect("read readiness line");
        assert_eq!(ready.trim_end(), "ready");

        let pinned_start = pid_start_time(pgid).expect("pin child start time");
        let group =
            pin_leader_process_group(pgid, Some(pinned_start)).expect("pin child process group");
        assert!(
            spawn_frozen_guard_with(&test_launcher(), &group),
            "the close-time guard must start"
        );

        let status = wait_for_fixture_exit(
            &mut child,
            "a close-time guard with a null stdin did not tear down its group",
        );
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "a TERM-ignoring group must reach the guard's SIGKILL escalation"
        );
    }

    /// Issue #1129: a pane-open guard handle that arrives after its pane is
    /// gone is dropped, and that alone shuts the PTY session down. While the
    /// handle is held, the guard leaves the session alone.
    #[cfg(target_os = "macos")]
    #[test]
    fn late_pane_open_guard_handle_dropped_kills_the_session() {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::time::{Duration, Instant};

        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: openpty initializes both fd outputs using default terminal
        // settings when termios/winsize are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty returned two fresh descriptors this test owns; the
        // master is the app's copy, close-on-exec like the pane's.
        let (mut master, slave) = unsafe {
            libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            (
                std::fs::File::from_raw_fd(master_fd),
                OwnedFd::from_raw_fd(slave_fd),
            )
        };
        let stdio = || Stdio::from(slave.try_clone().expect("duplicate the slave"));
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "trap '' HUP TERM; echo __PANE_READY__; exec sleep 30"])
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio());
        // SAFETY: only async-signal-safe syscalls run before exec. The shell
        // becomes session leader and takes the PTY as its terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut shell = command.spawn().expect("spawn the pane shell");
        drop(command);
        drop(slave);
        let shell_pid = shell.id();
        let _cleanup = KillGroupOnDrop(shell_pid as i32);

        // SAFETY: nonblocking reads keep a broken fixture from hanging.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        let mut output = Vec::new();
        let mut buffer = [0u8; 256];
        while !String::from_utf8_lossy(&output).contains("__PANE_READY__") {
            match master.read(&mut buffer) {
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read the pane PTY: {error}"),
            }
            assert!(Instant::now() < deadline, "pane shell never got ready");
            std::thread::sleep(Duration::from_millis(10));
        }

        // As the view does: the spawn runs off the GPUI thread.
        let start = pid_start_time(shell_pid);
        let master_raw = master.as_raw_fd();
        let handle = std::thread::spawn(move || {
            spawn_session_guard_with(&test_launcher(), shell_pid, start, master_raw)
        })
        .join()
        .expect("guard spawn thread")
        .expect("the pane-open guard must start");

        // An open control pipe is not EOF: the session must survive it.
        std::thread::sleep(GUARD_POLL_INTERVAL * 2 + Duration::from_millis(200));
        assert!(
            shell.try_wait().expect("try_wait shell").is_none(),
            "the pane-open guard must leave a live pane alone while its handle is held"
        );
        assert!(
            handle.is_live(),
            "the pane-open guard must still be running"
        );

        // The view is gone, so the late handle is dropped.
        drop(handle);
        let status = wait_for_fixture_exit(
            &mut shell,
            "dropping the late pane-open guard handle did not shut the session down",
        );
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "a TERM- and HUP-ignoring shell must reach the guard's SIGKILL"
        );
        drop(master);
    }

    /// SIGKILLs a fixture-owned process group however the test ends.
    #[cfg(unix)]
    struct KillGroupOnDrop(i32);

    #[cfg(unix)]
    impl Drop for KillGroupOnDrop {
        fn drop(&mut self) {
            // SAFETY: the group was created by this fixture.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }

    #[cfg(unix)]
    fn wait_for_guard_ready(stdout: &mut std::process::ChildStdout) {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::time::{Duration, Instant};

        let stdout_fd = stdout.as_raw_fd();
        // SAFETY: set nonblocking on this test-owned pipe so a broken helper
        // cannot hang the suite indefinitely.
        unsafe {
            let flags = libc::fcntl(stdout_fd, libc::F_GETFL);
            assert!(flags >= 0, "fcntl getfl on guard stdout");
            assert_eq!(
                libc::fcntl(stdout_fd, libc::F_SETFL, flags | libc::O_NONBLOCK),
                0,
                "fcntl setfl on guard stdout"
            );
        }
        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        let mut output = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => panic!(
                    "control-pipe guard exited before readiness: {}",
                    String::from_utf8_lossy(&output)
                ),
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read control-pipe guard readiness: {error}"),
            }
            if String::from_utf8_lossy(&output).contains("PANEFLOW_GUARD_READY") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "control-pipe guard readiness timed out: {}",
                String::from_utf8_lossy(&output)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn control_pipe_close_kills_a_term_ignoring_group() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::time::{Duration, Instant};

        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid runs in the forked child before exec and gives this
        // fixture its own process group, so the test can safely signal -pid.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn TERM-ignoring group");
        let pgid = child.id();

        struct GroupCleanup(i32);
        impl Drop for GroupCleanup {
            fn drop(&mut self) {
                // Test-only best effort; this group was created by the fixture.
                unsafe {
                    libc::kill(-self.0, libc::SIGKILL);
                }
            }
        }
        let _cleanup = GroupCleanup(pgid as i32);

        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut ready)
            .expect("read readiness line");
        assert_eq!(ready.trim_end(), "ready");

        let pinned_start = pid_start_time(pgid).expect("pin child start time");
        let group =
            pin_leader_process_group(pgid, Some(pinned_start)).expect("pin child process group");
        let spec = format!(
            "{}|{}|{}",
            group.pgid,
            group.session_id,
            serialize_member_pins(&group.members)
        );
        let exe = std::env::current_exe().expect("current test executable");
        let helper = Command::new(exe)
            .args([
                "--exact",
                "agents::parent_guard::tests::pty_guard_control_pipe_entrypoint",
                "--nocapture",
            ])
            .env(SUBPROCESS_GUARD_GROUP_ENV, spec)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn control-pipe guard");

        struct HelperCleanup(Option<std::process::Child>);
        impl Drop for HelperCleanup {
            fn drop(&mut self) {
                if let Some(helper) = self.0.as_mut() {
                    let _ = helper.kill();
                    let _ = helper.wait();
                }
            }
        }
        let mut helper = HelperCleanup(Some(helper));
        let guard_stdout = {
            let guard = helper.0.as_mut().expect("helper");
            let stdin = guard.stdin.take().expect("guard stdin");
            let mut stdout = guard.stdout.take().expect("guard stdout");
            wait_for_guard_ready(&mut stdout);
            // The pipe is open and empty, so the guard's nonblocking read
            // returns EAGAIN. That is not EOF: closing is what must kill.
            let open_pipe_grace = GUARD_POLL_INTERVAL * 2 + Duration::from_millis(200);
            std::thread::sleep(open_pipe_grace);
            assert!(
                child.try_wait().expect("try_wait child").is_none(),
                "open control pipe must not kill the group; EAGAIN is not EOF"
            );
            assert!(
                guard.try_wait().expect("try_wait guard").is_none(),
                "guard must keep polling while its control pipe is open"
            );
            drop(stdin);
            stdout
        };

        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait child") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("external guard did not terminate the group after control EOF");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "a TERM-ignoring group must reach the guard's SIGKILL escalation"
        );

        let helper_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if helper
                .0
                .as_mut()
                .expect("helper")
                .try_wait()
                .expect("try_wait guard")
                .is_some()
            {
                break;
            }
            if Instant::now() >= helper_deadline {
                panic!("control-pipe guard did not exit after EOF");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // try_wait already reaped the helper. Disarm Drop so it cannot
        // signal a recycled pid.
        helper.0.take();
        drop(guard_stdout);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn member_pins_authorize_kill_after_shell_leader_exits() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};

        // dash, not `sh`: see FIXTURE_WAIT_BUDGET for the bash 3.2 `wait`
        // race (#568) that loses a TERM sent right after `ready`.
        let mut command = Command::new("/bin/dash");
        command
            .args([
                "-c",
                "trap 'exit 42' TERM; trap '' HUP; (trap '' HUP TERM; echo ready; while :; do sleep 30; done) & wait",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid gives this fixture an isolated session/group.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .expect("spawn shell with stubborn descendant");
        let pgid = child.id();

        struct GroupCleanup(i32);
        impl Drop for GroupCleanup {
            fn drop(&mut self) {
                // Test-only best effort; this group was created by the fixture.
                unsafe {
                    libc::kill(-self.0, libc::SIGKILL);
                }
            }
        }
        let cleanup = GroupCleanup(pgid as i32);

        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut ready)
            .expect("read readiness line");
        assert_eq!(ready.trim_end(), "ready");

        let pinned_start = pid_start_time(pgid).expect("pin shell start time");
        let group = pin_session_process_group(pgid, Some(pinned_start))
            .expect("pin shell and same-PGID descendant");
        assert!(
            group.members.len() >= 2,
            "fixture must pin both shell and descendant: {:?}",
            group.members
        );
        assert!(signal_pinned_process_group(&group, libc::SIGTERM));

        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait shell") {
                break status;
            }
            assert!(Instant::now() < deadline, "shell did not honor SIGTERM");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(42));
        // The numeric group leader has been reaped, but its TERM-ignoring
        // descendant keeps the process group alive.
        assert!(unsafe { libc::getpgid(pgid as i32) } < 0);
        assert_eq!(unsafe { libc::kill(-(pgid as i32), 0) }, 0);

        assert!(
            signal_pinned_process_group(&group, libc::SIGKILL),
            "a surviving member pin must authorize the delayed KILL"
        );
        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        while unsafe { libc::kill(-(pgid as i32), 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "same-PGID descendant survived SIGKILL"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(cleanup);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parent_death_guard_refreshes_late_shell_group_members() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};

        // dash, not `sh`: see FIXTURE_WAIT_BUDGET (#568). The guard KILLs
        // 100 ms after its TERM, so the leader's trap must run at once.
        let mut command = Command::new("/bin/dash");
        command
            .args([
                "-c",
                "trap 'exit 42' TERM; trap '' HUP; echo shell-ready; IFS= read -r go; (trap '' HUP TERM; echo descendant-ready; while :; do sleep 30; done) & wait",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: isolate the fixture in a new session/process group.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut shell = command.spawn().expect("spawn delayed-descendant shell");
        let shell_pid = shell.id();
        let mut stdout = BufReader::new(shell.stdout.take().expect("shell stdout"));
        let mut ready = String::new();
        stdout.read_line(&mut ready).expect("shell readiness");
        assert_eq!(ready.trim_end(), "shell-ready");

        // This is exactly the immutable identity the production guard receives
        // at PTY spawn: the later descendant does not exist yet.
        let shell_start = pid_start_time(shell_pid).expect("shell start pin");
        let origin = pin_session_process_group(shell_pid, Some(shell_start))
            .expect("initial shell-only identity");
        assert_eq!(origin.members, vec![(shell_pid, shell_start)]);
        let mut guard_parent = spawn_disposable_guard_parent(&origin, None);

        shell
            .stdin
            .as_mut()
            .expect("shell stdin")
            .write_all(b"go\n")
            .expect("release descendant");
        ready.clear();
        stdout.read_line(&mut ready).expect("descendant readiness");
        assert_eq!(ready.trim_end(), "descendant-ready");
        assert!(
            pin_process_group(shell_pid, shell_pid).is_some_and(|group| group.members.len() >= 2),
            "late same-PGID member must exist before parent death"
        );

        guard_parent.kill_parent();
        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        let status = loop {
            if let Some(status) = shell.try_wait().expect("try_wait refreshed shell") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "refreshed guard did not terminate the shell leader"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            status.code(),
            Some(42),
            "shell must honor TERM before the refreshed member reaches KILL"
        );

        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        loop {
            // SAFETY: signal 0 only probes this fixture-owned process group.
            if unsafe { libc::kill(-(shell_pid as i32), 0) } < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "refreshed hard-death guard left a shell-group descendant"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parent_death_guard_kills_foreground_and_stopped_background_jobs() {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::time::{Duration, Instant};

        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: openpty initializes both fd outputs using default terminal
        // settings when termios/winsize are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let duplicate_slave = || {
            // SAFETY: duplicate the fixture-owned slave for one stdio slot.
            let duplicate = unsafe { libc::dup(slave_fd) };
            assert!(duplicate >= 0);
            // SAFETY: each fresh duplicate transfers exactly once to Stdio.
            unsafe { Stdio::from_raw_fd(duplicate) }
        };
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "set -m; trap '' HUP TERM; /bin/sh -c 'trap \"\" HUP TERM; echo __PANEFLOW_HARD_DEATH_BG__:$$; kill -STOP $$; while :; do sleep 30; done' & /bin/sh -c 'trap \"\" HUP TERM; echo __PANEFLOW_HARD_DEATH_FG__; while :; do sleep 30; done'",
            ])
            .stdin(duplicate_slave())
            .stdout(duplicate_slave())
            .stderr(duplicate_slave());
        // SAFETY: only async-signal-safe syscalls run before exec. The shell
        // becomes session leader and acquires the PTY as controlling terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawned = command.spawn();
        // SAFETY: Command owns the duplicates; parent no longer needs slave.
        unsafe {
            libc::close(slave_fd);
        }
        let mut shell = spawned.expect("spawn controlling-terminal shell");
        let shell_pid = shell.id();
        // SAFETY: transfer the unique master fd into File.
        let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };

        struct PtyGroupCleanup {
            shell_pgid: i32,
            master_fd: i32,
        }
        impl Drop for PtyGroupCleanup {
            fn drop(&mut self) {
                // Test-only best effort for fixture-owned groups.
                unsafe {
                    if let Some(groups) = pin_process_groups_in_session(self.shell_pgid as u32) {
                        for group in groups {
                            libc::kill(-(group.pgid as i32), libc::SIGKILL);
                        }
                    }
                    let foreground = libc::tcgetpgrp(self.master_fd);
                    if foreground > 1 && foreground != self.shell_pgid {
                        libc::kill(-foreground, libc::SIGKILL);
                    }
                    libc::kill(-self.shell_pgid, libc::SIGKILL);
                }
            }
        }
        let cleanup = PtyGroupCleanup {
            shell_pgid: shell_pid as i32,
            master_fd: master.as_raw_fd(),
        };
        // SAFETY: make reads on our PTY master nonblocking for a bounded wait.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert!(flags >= 0);
            assert_eq!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );
        }
        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        let mut output = Vec::new();
        let mut buffer = [0u8; 1024];
        while !output
            .windows(b"__PANEFLOW_HARD_DEATH_FG__".len())
            .any(|window| window == b"__PANEFLOW_HARD_DEATH_FG__")
            || !output
                .windows(b"__PANEFLOW_HARD_DEATH_BG__:".len())
                .any(|window| window == b"__PANEFLOW_HARD_DEATH_BG__:")
        {
            match master.read(&mut buffer) {
                Ok(0) => panic!("hard-death PTY closed before readiness"),
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read hard-death PTY: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "foreground readiness timed out: {}",
                String::from_utf8_lossy(&output)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let output_text = String::from_utf8_lossy(&output);
        let background_pgid = output_text
            .split("__PANEFLOW_HARD_DEATH_BG__:")
            .nth(1)
            .and_then(|suffix| {
                let digits = suffix
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>();
                digits.parse::<u32>().ok()
            })
            .expect("parse stopped background PGID");

        let shell_start = pid_start_time(shell_pid).expect("shell start pin");
        let origin = pin_session_process_group(shell_pid, Some(shell_start))
            .expect("pin hard-death shell identity");
        let foreground = pin_foreground_process_group(master.as_raw_fd(), shell_pid)
            .expect("pin distinct foreground group before parent death");
        assert_ne!(foreground.pgid, shell_pid);
        assert_ne!(background_pgid, shell_pid);
        assert_ne!(background_pgid, foreground.pgid);
        let session_groups = pin_terminal_session_process_groups(master.as_raw_fd(), shell_pid)
            .expect("pin every interactive PTY process group");
        for expected in [shell_pid, foreground.pgid, background_pgid] {
            assert!(
                session_groups.iter().any(|group| group.pgid == expected),
                "session snapshot omitted PGID {expected}: {session_groups:?}"
            );
        }

        let mut guard_parent = spawn_disposable_guard_parent(&origin, Some(master.as_raw_fd()));
        // Reproduce the fail-open case: lose the original shell identity while
        // a HUP/TERM-resistant foreground job remains, then kill the guard's
        // own parent. The inherited PTY/session authority must keep it alive.
        // SAFETY: this shell belongs to the fixture.
        unsafe {
            libc::kill(shell_pid as i32, libc::SIGKILL);
        }
        let shell_status = shell
            .wait()
            .expect("reap session leader before parent death");
        assert_eq!(shell_status.signal(), Some(libc::SIGKILL));
        // SAFETY: signal 0 only probes fixture-owned processes.
        assert_eq!(unsafe { libc::kill(-(foreground.pgid as i32), 0) }, 0);
        assert_eq!(unsafe { libc::kill(-(background_pgid as i32), 0) }, 0);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(unsafe { libc::kill(guard_parent.guard_pid, 0) }, 0);

        guard_parent.kill_parent();

        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        loop {
            // SAFETY: signal 0 only probes the fixture-owned foreground group.
            if unsafe { libc::kill(-(foreground.pgid as i32), 0) } < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "hard-death guard left foreground job-control group alive"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let deadline = Instant::now() + FIXTURE_WAIT_BUDGET;
        loop {
            // SAFETY: signal 0 only probes the fixture-owned stopped group.
            if unsafe { libc::kill(-(background_pgid as i32), 0) } < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "hard-death guard left stopped/background job-control group alive"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(cleanup);
    }

    #[test]
    fn may_signal_group_leader_matching_start() {
        assert!(may_signal_group(42, Some(100), Some(100), true));
    }

    #[test]
    fn may_signal_group_leader_mismatched_start() {
        assert!(!may_signal_group(42, Some(100), Some(200), true));
    }

    #[test]
    fn may_signal_group_not_leader() {
        assert!(!may_signal_group(42, Some(100), Some(100), false));
    }

    #[test]
    fn may_signal_group_dead_or_esrch() {
        // getpgid ESRCH / dead pid: not a leader, no current start.
        assert!(!may_signal_group(42, Some(100), None, false));
        // Pinned + missing current start is not the original process
        // (`same_process` treats this as dead).
        assert!(!may_signal_group(42, Some(100), None, true));
    }

    #[test]
    fn may_signal_group_requires_both_start_time_pins() {
        assert!(
            !may_signal_group(42, None, Some(100), true),
            "destructive signaling must fail closed without a spawn-time pin"
        );
        assert!(
            !may_signal_group(42, None, None, true),
            "two missing probes are not proof of process-group identity"
        );
    }

    /// `kill(0, 0)` probes the caller's own process group and a pid above
    /// `i32::MAX` wraps negative, so neither may reach the syscalls.
    #[test]
    fn pid_probes_reject_pid_zero_and_pids_above_i32_max() {
        for pid in [0, i32::MAX as u32 + 1, u32::MAX] {
            assert!(!pid_is_alive(pid), "pid {pid} must read as dead");
            assert_eq!(pid_start_time(pid), None, "pid {pid} has no start pin");
        }
        // Control: both probes answer for a real process, so the refusals
        // above are the range check, not a probe that never succeeds.
        let own = std::process::id();
        assert!(pid_is_alive(own));
        assert!(pid_start_time(own).is_some());
    }

    // Hardcoded independently of `INHERITED_AGENT_SESSION_ENV` so shrinking
    // the production slice fails these tests instead of vacuously passing.
    const MARKERS: &[&str] = &[
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
    ];

    #[test]
    fn scrub_claudecode_is_idempotent() {
        // SAFETY: test-only -- single-threaded test runner step. Sets, scrubs,
        // and re-scrubs to confirm the second call does not panic.
        unsafe {
            for key in MARKERS {
                std::env::set_var(key, "1");
            }
        }
        // SAFETY: this test holds no extra threads and only exercises these env vars.
        unsafe { scrub_claudecode_env_before_threads() };
        for key in MARKERS {
            assert!(
                std::env::var(key).is_err(),
                "{key} must be scrubbed from the process env"
            );
        }
        // SAFETY: same as above.
        unsafe { scrub_claudecode_env_before_threads() };
        for key in MARKERS {
            assert!(
                std::env::var(key).is_err(),
                "{key} must stay absent after a second scrub"
            );
        }
    }

    #[test]
    fn scrub_claudecode_from_command_is_local_to_child() {
        let mut command = std::process::Command::new("noop");
        for key in MARKERS {
            command.env(*key, "1");
        }
        scrub_claudecode_from_command(&mut command);
        for key in MARKERS {
            assert!(
                command
                    .get_envs()
                    .any(|(k, value)| k == *key && value.is_none()),
                "child command should explicitly remove {key}"
            );
        }
    }

    /// Turns an issue #1110 test into its re-exec'd child.
    const GIT_ENV_PROBE: &str = "PANEFLOW_GIT_ENV_SCRUB_PROBE";
    /// Kept by the scrub on purpose (it only decides whether discovery
    /// succeeds), so it proves the scrub is targeted.
    const KEPT_GIT_NAME: &str = "GIT_CEILING_DIRECTORIES";
    /// A plain inherited name every child must still see.
    const KEPT_PLAIN_NAME: &str = "PANEFLOW_GIT_ENV_SCRUB_KEEP";
    /// In `INHERITED_GIT_ENV` for `git_command`, but kept by the startup scrub
    /// so panes still push with the user's SSH command. Spelled out rather
    /// than read from `GIT_ENV_KEPT_BY_STARTUP_SCRUB`, so dropping the
    /// exception fails these tests.
    const KEPT_SSH_NAME: &str = "GIT_SSH_COMMAND";

    /// Re-exec `test` (a test in this module) in a fresh process launched the
    /// way a git hook launches PaneFlow: every `INHERITED_GIT_ENV` name set.
    /// The scrub mutates the process env, which is only sound before other
    /// threads read it, so it runs in that child, never in this runner.
    /// Returns true in the parent once the child passed; false in the child,
    /// which then runs the test body and prints `done` last.
    fn ran_in_git_launched_child(test: &str, done: &str) -> bool {
        if std::env::var_os(GIT_ENV_PROBE).is_some() {
            return false;
        }
        // `--exact` matches the full path without the crate name, so a bare
        // function name filters out every test and exits 0 (#1109).
        let module = module_path!();
        let module = module.split_once("::").map_or(module, |(_, rest)| rest);
        let filter = format!("{module}::{test}");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test exe"));
        child
            .args([
                filter.as_str(),
                "--exact",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(GIT_ENV_PROBE, "1")
            .env(KEPT_GIT_NAME, "/nonexistent/paneflow-1110")
            .env(KEPT_PLAIN_NAME, "1");
        for name in crate::workspace::worktree::INHERITED_GIT_ENV {
            child.env(name, "/nonexistent/paneflow-1110");
        }
        let output = child.output().expect("re-exec the git env scrub probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(done) && stdout.contains("1 passed"),
            "git env scrub probe `{filter}` failed or did not run:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    /// Issue #1110: the pre-thread scrub `main` runs removes every inherited
    /// repository-redirecting git name from the process, and keeps
    /// `GIT_SSH_COMMAND`.
    #[test]
    fn startup_scrub_removes_every_inherited_git_env_name() {
        const DONE: &str = "PANEFLOW_1110_SCRUB_NAMES_DONE";
        if ran_in_git_launched_child("startup_scrub_removes_every_inherited_git_env_name", DONE) {
            return;
        }
        let names = crate::workspace::worktree::INHERITED_GIT_ENV;
        for pinned in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", KEPT_SSH_NAME] {
            assert!(names.contains(&pinned), "INHERITED_GIT_ENV lost {pinned}");
        }
        for name in names {
            assert!(
                std::env::var_os(name).is_some(),
                "control: the probe must be launched with {name}"
            );
        }
        // SAFETY: a re-exec'd child running this one test under
        // `--test-threads=1`; libtest's other thread only waits for it, and
        // nothing else in this process reads the environment meanwhile.
        unsafe { scrub_claudecode_env_before_threads() };
        for name in names.iter().filter(|name| **name != KEPT_SSH_NAME) {
            assert!(
                std::env::var_os(name).is_none(),
                "the startup scrub must remove {name}"
            );
        }
        assert!(
            std::env::var_os(KEPT_GIT_NAME).is_some(),
            "{KEPT_GIT_NAME} is not a redirect and must survive"
        );
        assert!(std::env::var_os(KEPT_PLAIN_NAME).is_some());
        assert!(
            std::env::var_os(KEPT_SSH_NAME).is_some(),
            "{KEPT_SSH_NAME} picks the SSH command, not a repository, and must survive"
        );
        println!("{DONE}");
    }

    /// Issue #1110: after the startup scrub, a child PaneFlow spawns (a plain
    /// `Command`, as the session lists, editor and launchers use, and a pane
    /// shell's `CommandBuilder`, which seeds from the process env) no longer
    /// sees `GIT_INDEX_FILE` or any other inherited redirect, but still sees
    /// `GIT_SSH_COMMAND`.
    #[test]
    fn child_spawned_after_startup_scrub_does_not_see_git_index_file() {
        const DONE: &str = "PANEFLOW_1110_SCRUB_CHILD_DONE";
        if ran_in_git_launched_child(
            "child_spawned_after_startup_scrub_does_not_see_git_index_file",
            DONE,
        ) {
            return;
        }
        // SAFETY: as in `startup_scrub_removes_every_inherited_git_env_name`.
        unsafe { scrub_claudecode_env_before_threads() };

        let output = std::process::Command::new("/usr/bin/env")
            .output()
            .expect("run /usr/bin/env");
        assert!(output.status.success(), "/usr/bin/env failed");
        let listed = String::from_utf8_lossy(&output.stdout);
        let seen: Vec<&str> = listed
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        assert!(
            seen.contains(&KEPT_PLAIN_NAME) && seen.contains(&KEPT_GIT_NAME),
            "control: the child must still inherit the rest of the env:\n{listed}"
        );
        assert!(
            seen.contains(&KEPT_SSH_NAME),
            "a spawned child must still see {KEPT_SSH_NAME}:\n{listed}"
        );
        assert!(
            !seen.contains(&"GIT_INDEX_FILE"),
            "a spawned child still sees GIT_INDEX_FILE:\n{listed}"
        );
        for name in crate::workspace::worktree::INHERITED_GIT_ENV
            .iter()
            .filter(|name| **name != KEPT_SSH_NAME)
        {
            assert!(!seen.contains(name), "a spawned child still sees {name}");
        }

        let pane = portable_pty::CommandBuilder::new("/bin/sh");
        assert!(
            pane.get_env(KEPT_PLAIN_NAME).is_some(),
            "control: a pane shell seeds from the process env"
        );
        assert!(
            pane.get_env(KEPT_SSH_NAME).is_some(),
            "a pane shell must still inherit {KEPT_SSH_NAME}"
        );
        for name in crate::workspace::worktree::INHERITED_GIT_ENV
            .iter()
            .filter(|name| **name != KEPT_SSH_NAME)
        {
            assert!(
                pane.get_env(name).is_none(),
                "a pane shell still inherits {name}"
            );
        }
        println!("{DONE}");
    }
}
