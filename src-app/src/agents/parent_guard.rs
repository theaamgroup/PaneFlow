//! US-003: kill-on-parent-death guard for spawned agent CLIs and PTYs.
//!
//! Goal: when Paneflow dies for any reason (including `kill -9`), the
//! child processes it spawned -- `claude`, `codex`, `opencode`, the
//! shells started inside agent terminals -- must die with it. Without
//! this, orphans are reparented to PID 1 and continue streaming,
//! consuming the user's API tokens until their natural timeout.
//!
//! [`install_process_job`] returns [`ParentGuardStatus::Unsupported`]
//! because there is no process-wide Unix equivalent to a kill-on-close
//! job object in this app layer. Shim-wrapped agent CLIs are covered
//! separately: `paneflow-shim` installs a parent-death watcher on macOS
//! before it waits on the real agent binary. Raw PTY shells are covered
//! by a tiny per-PTY watcher process launched through [`spawn_pty_guard`].
//! The remaining gap is spawn paths hidden behind another API, notably
//! `paneflow-acp::spawn`, until those surfaces expose a child pre-exec
//! hook or equivalent parent-death API.

#[cfg(unix)]
use std::process::{ChildStdin, Command, Stdio};

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
    // Unsupported on Linux + macOS until paneflow-acp and portable-pty
    // expose a pre_exec hook; see the module-level docstring.
    Ok(ParentGuardStatus::Unsupported)
}

/// Whether teardown may signal process group `-pid`.
///
/// `getpgid_is_leader` is the live `getpgid(pid) == pid` result (false on
/// ESRCH / a recycled pid that is not a session leader). Start times use
/// the same `pbi_start_tvsec`/`pbi_start_tvusec` encoding as session
/// `proc_start` / `child_proc_start`; comparison is `same_process`.
pub(crate) fn may_signal_group(
    pid: i32,
    pinned_start: Option<u64>,
    current_start: Option<u64>,
    getpgid_is_leader: bool,
) -> bool {
    pid > 0
        && getpgid_is_leader
        && crate::app::event_handlers::same_process(pinned_start, current_start)
}

#[cfg(unix)]
pub fn run_pty_guard_from_args(args: &[String]) -> i32 {
    let Some(parent_pid) = args.get(2).and_then(|arg| arg.parse::<u32>().ok()) else {
        return 2;
    };
    let Some(child_pgid) = args.get(3).and_then(|arg| arg.parse::<u32>().ok()) else {
        return 2;
    };
    if parent_pid <= 1 || child_pgid <= 1 {
        return 2;
    }
    let pinned_start = match args.get(4) {
        None => None,
        Some(arg) => match arg.parse::<u64>() {
            Ok(start) => Some(start),
            Err(_) => return 2,
        },
    };

    set_control_pipe_nonblocking();
    while parent_still_attached(parent_pid) && process_group_alive(child_pgid, pinned_start) {
        if control_pipe_closed() {
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    if !parent_still_attached(parent_pid) && process_group_alive(child_pgid, pinned_start) {
        terminate_process_group(child_pgid, pinned_start);
    }
    0
}

#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
pub fn spawn_pty_guard(child_pgid: u32, child_proc_start: Option<u64>) -> Option<PtyGuardHandle> {
    if child_pgid <= 1 {
        return None;
    }
    let Ok(exe) = std::env::current_exe() else {
        log::debug!("parent_guard: current_exe unavailable; PTY guard not started");
        return None;
    };

    // Prefer the caller's spawn pin (`child_proc_start`); fall back to a
    // live probe so the guard still pins when the caller had a miss.
    let pinned_start = child_proc_start.or_else(|| current_group_start(child_pgid));

    let mut cmd = Command::new(exe);
    cmd.arg(PTY_GUARD_SUBCOMMAND)
        .arg(std::process::id().to_string())
        .arg(child_pgid.to_string());
    if let Some(start) = pinned_start {
        cmd.arg(start.to_string());
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);

    match cmd.spawn() {
        Ok(mut child) => {
            let Some(stdin) = child.stdin.take() else {
                log::warn!("parent_guard: PTY guard for pgid {child_pgid} has no control pipe");
                let _ = child.kill();
                return None;
            };
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Some(PtyGuardHandle { _stdin: stdin })
        }
        Err(err) => {
            log::warn!("parent_guard: failed to start PTY guard for pgid {child_pgid}: {err}");
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
fn current_group_start(pgid: u32) -> Option<u64> {
    crate::app::event_handlers::pid_start_time(pgid)
}

#[cfg(unix)]
fn is_session_leader(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: getpgid is a pure query; ESRCH yields -1, which is never a
    // positive pid, so a recycled-or-dead pid is not reported as leader.
    unsafe { libc::getpgid(pid) == pid }
}

#[cfg(unix)]
fn process_group_alive(pgid: u32, pinned_start: Option<u64>) -> bool {
    let Ok(pid) = i32::try_from(pgid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only probes process-group existence.
    let rc = unsafe { libc::kill(-pid, 0) };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return false;
    }
    may_signal_group(
        pid,
        pinned_start,
        current_group_start(pgid),
        is_session_leader(pid),
    )
}

#[cfg(unix)]
fn terminate_process_group(pgid: u32, pinned_start: Option<u64>) {
    let Ok(pid) = i32::try_from(pgid) else {
        return;
    };
    if !may_signal_group(
        pid,
        pinned_start,
        current_group_start(pgid),
        is_session_leader(pid),
    ) {
        return;
    }
    // SAFETY: negative pid targets the process group. Identity (leader +
    // start-time pin) was just confirmed.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    if process_group_alive(pgid, pinned_start) {
        // SAFETY: same process-group target after a leader+start re-check.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
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
            "not-a-start".to_string(),
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
            "1".to_string(),
        ];
        assert_eq!(run_pty_guard_from_args(&args), 0);
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
}
