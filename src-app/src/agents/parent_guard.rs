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

    set_control_pipe_nonblocking();
    while parent_still_attached(parent_pid) && process_group_alive(child_pgid) {
        if control_pipe_closed() {
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    if !parent_still_attached(parent_pid) && process_group_alive(child_pgid) {
        terminate_process_group(child_pgid);
    }
    0
}

#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
pub fn spawn_pty_guard(child_pgid: u32) -> Option<PtyGuardHandle> {
    if child_pgid <= 1 {
        return None;
    }
    let Ok(exe) = std::env::current_exe() else {
        log::debug!("parent_guard: current_exe unavailable; PTY guard not started");
        return None;
    };

    let mut cmd = Command::new(exe);
    cmd.arg(PTY_GUARD_SUBCOMMAND)
        .arg(std::process::id().to_string())
        .arg(child_pgid.to_string())
        .stdin(Stdio::piped())
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
fn process_group_alive(pgid: u32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only probes process-group existence.
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
fn terminate_process_group(pgid: u32) {
    let Ok(pgid) = i32::try_from(pgid) else {
        return;
    };
    // SAFETY: negative pid targets the process group. The pgid is checked above.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    if process_group_alive(pgid as u32) {
        // SAFETY: same process-group target after a liveness probe.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
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
}
