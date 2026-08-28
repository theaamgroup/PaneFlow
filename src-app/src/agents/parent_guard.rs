//! US-003: kill-on-parent-death guard for spawned agent CLIs and PTYs.
//!
//! Goal: prevent child agents and terminal work from surviving Paneflow.
//! Shim-wrapped agent CLIs have parent-death watchers. Each raw PTY guard also
//! inherits a dedicated master descriptor: on orderly teardown or hard parent
//! death it authenticates the pinned terminal session, refreshes late shell
//! members, and discovers every live process group in the terminal session
//! before owning the complete TERM-to-KILL ladder.
//!
//! [`install_process_job`] returns [`ParentGuardStatus::Unsupported`]
//! because there is no process-wide Unix equivalent to a kill-on-close
//! job object in this app layer. Shim-wrapped agent CLIs are covered
//! separately: `paneflow-shim` installs a parent-death watcher on macOS
//! before it waits on the real agent binary. Raw PTY shells are covered
//! by tiny per-PTY watcher processes launched through [`spawn_pty_guard`].

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::process::{ChildStdin, Command, Stdio};

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
/// any worker thread or PTY backend starts. Alacritty 0.26 inherits the parent
/// environment and does not expose arbitrary `env_remove` entries, so this
/// process-level guard remains necessary until that spawn boundary can own the
/// exclusion directly.
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
/// [`scrub_claudecode_from_command`] for per-child scrubbing after startup.
pub(crate) unsafe fn scrub_claudecode_env_before_threads() {
    // SAFETY: delegated to the caller by this function's contract.
    unsafe {
        for key in INHERITED_AGENT_SESSION_ENV {
            std::env::remove_var(*key);
        }
    }
}

/// Remove inherited agent-session markers from one child command without
/// mutating global process environment.
#[allow(dead_code)]
pub fn scrub_claudecode_from_command(command: &mut std::process::Command) {
    for key in INHERITED_AGENT_SESSION_ENV {
        command.env_remove(key);
    }
}

#[cfg(unix)]
pub const PTY_GUARD_SUBCOMMAND: &str = "__paneflow-pty-guard";

#[cfg(unix)]
pub struct PtyGuardHandle {
    _stdin: ChildStdin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ParentGuardStatus {
    Installed,
    Unsupported,
}

/// Install the process-wide kill-on-parent-death guard. Call once,
/// early in `fn main()`, before any agent CLI or PTY is spawned.
/// Currently always returns [`ParentGuardStatus::Unsupported`].
pub fn install_process_job() -> Result<ParentGuardStatus, Box<dyn std::error::Error>> {
    // Unix has no process-wide equivalent to a Windows Job Object; PTY
    // shells and shim-wrapped agents install per-child guards instead.
    Ok(ParentGuardStatus::Unsupported)
}

/// Whether teardown may signal process group `-pid`.
///
/// `getpgid_is_leader` is the live `getpgid(pid) == pid` result (false on
/// ESRCH / a recycled pid that is not a session leader). Start times use
/// the same `pbi_start_tvsec`/`pbi_start_tvusec` encoding as session
/// `proc_start` / `child_proc_start`. Unlike conservative UI liveness, a
/// destructive signal requires both probes to exist and match exactly.
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
        same_group_and_session && current_process_start(pid) == Some(pinned_start)
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
        let start = info
            .pbi_start_tvsec
            .wrapping_mul(1_000_000)
            .wrapping_add(info.pbi_start_tvusec);
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
        let start = info
            .pbi_start_tvsec
            .wrapping_mul(1_000_000)
            .wrapping_add(info.pbi_start_tvusec);
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
            current_process_start(pgid),
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
    loop {
        if !parent_still_attached(parent_pid) {
            shutdown_guard_targets(&group, &mode, &observed_groups);
            return 0;
        }
        if let Some(groups) = observe_session_groups(&group, &mode) {
            observed_groups = groups;
        }
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
        std::thread::sleep(std::time::Duration::from_millis(500));
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

#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
pub fn spawn_pty_guard(
    child_pgid: u32,
    child_proc_start: Option<u64>,
    pty_master_fd: i32,
) -> Option<PtyGuardHandle> {
    let pinned_start = child_proc_start.or_else(|| current_process_start(child_pgid));
    let group = pin_session_process_group(child_pgid, pinned_start)?;
    spawn_process_group_guard_with_mode(group, Some(pty_master_fd))
}

#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn spawn_process_group_guard(group: PinnedProcessGroup) -> Option<PtyGuardHandle> {
    spawn_process_group_guard_with_mode(group, None)
}

#[cfg(unix)]
fn spawn_process_group_guard_with_mode(
    group: PinnedProcessGroup,
    session_pty_master_fd: Option<i32>,
) -> Option<PtyGuardHandle> {
    if !pinned_process_group_is_current(&group) {
        log::warn!(
            "parent_guard: cannot validate PTY group {}; guard not started",
            group.pgid
        );
        return None;
    }
    let Ok(exe) = std::env::current_exe() else {
        log::debug!("parent_guard: current_exe unavailable; PTY guard not started");
        return None;
    };

    let inherited_master = session_pty_master_fd.and_then(|fd| {
        // SAFETY: dup creates a guard-dedicated descriptor and clears
        // FD_CLOEXEC so Command's child inherits it across exec.
        let duplicate = unsafe { libc::dup(fd) };
        if duplicate < 0 {
            log::warn!(
                "parent_guard: cannot duplicate PTY master for pgid {}; foreground hard-death guard unavailable",
                group.pgid
            );
            None
        } else {
            // SAFETY: this is the only owner of the fresh duplicate in the
            // parent. It is dropped immediately after `spawn` returns.
            Some(unsafe { OwnedFd::from_raw_fd(duplicate) })
        }
    });
    let mode_arg = match &inherited_master {
        Some(fd) => format!("session:{}", fd.as_raw_fd()),
        None if session_pty_master_fd.is_some() => "session:none".to_string(),
        None => "frozen".to_string(),
    };

    let mut cmd = Command::new(exe);
    cmd.arg(PTY_GUARD_SUBCOMMAND)
        .arg(std::process::id().to_string())
        .arg(group.pgid.to_string())
        .arg(group.session_id.to_string())
        .arg(serialize_member_pins(&group.members))
        .arg(mode_arg);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);

    match cmd.spawn() {
        Ok(mut child) => {
            let Some(stdin) = child.stdin.take() else {
                log::warn!(
                    "parent_guard: PTY guard for pgid {} has no control pipe",
                    group.pgid
                );
                let _ = child.kill();
                return None;
            };
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Some(PtyGuardHandle { _stdin: stdin })
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

#[cfg(unix)]
fn parent_still_attached(parent_pid: u32) -> bool {
    // SAFETY: getppid has no preconditions.
    unsafe { libc::getppid() as u32 == parent_pid }
}

#[cfg(unix)]
fn current_process_start(pid: u32) -> Option<u64> {
    crate::app::event_handlers::pid_start_time(pid)
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

    const SUBPROCESS_GUARD_GROUP_ENV: &str = "PANEFLOW_TEST_GUARD_GROUP";
    const SUBPROCESS_GUARD_MASTER_ENV: &str = "PANEFLOW_TEST_GUARD_MASTER";

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
        let deadline = Instant::now() + Duration::from_secs(10);
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

    /// The call must NOT panic. The unsupported shim short-circuits cleanly.
    /// The production call site already logs the error and proceeds without
    /// blocking startup.
    #[test]
    fn install_process_job_does_not_panic() {
        // Calling twice is also safe -- both calls are no-ops.
        let _ = install_process_job();
        let _ = install_process_job();
    }

    /// Contract: the call must report unsupported explicitly. The behavioural
    /// assertion is that we did not silently fall through to a panic or to a
    /// `unimplemented!()`.
    #[test]
    fn unix_install_is_documented_unsupported() {
        assert_eq!(
            install_process_job().unwrap(),
            ParentGuardStatus::Unsupported
        );
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
        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut ready)
            .expect("read readiness line");
        assert_eq!(ready.trim_end(), "ready");

        let pinned_start = current_process_start(pgid).expect("pin child start time");
        let group =
            pin_leader_process_group(pgid, Some(pinned_start)).expect("pin child process group");
        shutdown_guard_targets(&group, &PtyGuardMode::Frozen, &[]);

        let deadline = Instant::now() + Duration::from_secs(10);
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
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn member_pins_authorize_kill_after_shell_leader_exits() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};

        let mut command = Command::new("sh");
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

        let pinned_start = current_process_start(pgid).expect("pin shell start time");
        let group = pin_session_process_group(pgid, Some(pinned_start))
            .expect("pin shell and same-PGID descendant");
        assert!(
            group.members.len() >= 2,
            "fixture must pin both shell and descendant: {:?}",
            group.members
        );
        assert!(signal_pinned_process_group(&group, libc::SIGTERM));

        let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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

        let mut command = Command::new("sh");
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
        let shell_start = current_process_start(shell_pid).expect("shell start pin");
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
        let deadline = Instant::now() + Duration::from_secs(10);
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

        let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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

        let shell_start = current_process_start(shell_pid).expect("shell start pin");
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

        let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
}
