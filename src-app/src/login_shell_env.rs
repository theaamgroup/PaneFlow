//! Adopt the login shell's **PATH** at startup (GUI-launch PATH fix).
//!
//! When PaneFlow is launched from Finder, Dock, or launchd, it inherits
//! launchd's minimal environment - the PATH is missing Homebrew
//! (`/opt/homebrew/bin`, `/usr/local/bin`), a Nix profile, and anything
//! the user appended in their login profile. Terminals opened inside that
//! process then cannot find the user's tools, and agent-CLI detection
//! (`which::which("bunx")`) comes up empty.
//!
//! We run the user's login shell once and adopt **only its `PATH`**. We
//! deliberately do NOT import the rest of the captured environment: a login
//! profile that re-exports session variables would otherwise clobber the
//! live values before the IPC socket is composed. (Zed keeps the captured
//! env in a side `HashMap` applied only to PTYs/tasks; importing just PATH
//! is the same idea with a smaller surface, sufficient for the discovery
//! problem this module exists to solve.)
//!
//! Properties:
//! - **skipped on a terminal launch** (stdin is a TTY) - PATH was already
//!   inherited correctly;
//! - **skipped when PATH was set explicitly** - anything other than launchd's
//!   stock `/usr/bin:/bin:/usr/sbin:/sbin` (e.g. `open --env PATH=...` or
//!   `launchctl setenv PATH`) is honoured as-is;
//! - **rejects a captured PATH without `/usr/bin` or `/bin`** - a profile that
//!   assigns `PATH=$HOME/bin` instead of appending must not strip the system
//!   dirs from the GUI process;
//! - **portable** - uses POSIX `env` (not GNU `env -0`)
//!   and falls back to `/bin/sh` for shells whose `-l -i -c`
//!   can't run the POSIX capture script (nushell, tcsh, xonsh, …); `/bin/sh`
//!   still sources `/etc/profile` + `/etc/profile.d` + `~/.profile`, i.e. the
//!   system PATH;
//! - **bounded** by a 5 s deadline so a pathological rc script can't wedge
//!   startup. The wait is on the capture child exiting, not on stdout EOF.
//!   A login profile that leaves a background job holding the pipe must not
//!   drop a `PATH=` line the shell already wrote (issue #712). A child still
//!   running at the deadline with no complete PATH line is terminated and
//!   the inherited PATH stands. Stdout closing is still not process exit
//!   (issue #683): once the pipe does hit EOF, a child that keeps running is
//!   reaped only until the same deadline, then killed. A 256 KiB read cap
//!   stops one that writes continuously from ballooning the capture buffer;
//! - **best-effort** - any failure logs and leaves the inherited PATH untouched.
//!
//! Safety: like [`crate::runtime_paths::augment_path_for_gui_launch`], this
//! mutates the process-global environment and MUST run on the main thread
//! before any other thread is spawned (Rust 2024 marks `set_var` `unsafe`). The
//! one helper thread it spawns publishes stdout into a shared buffer and is
//! joined before the `set_var` when the read can be unblocked. The PATH bytes
//! are snapshotted under that mutex first. If a holder keeps the read blocked,
//! the reader is detached and keeps its own handle to the buffer, so it cannot
//! overlap `set_var` or use-after-free it.

#[cfg(unix)]
pub fn load_login_shell_env() {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // A terminal launch already inherited the login PATH from its parent shell
    // - skip the (~50-200 ms) re-capture. Only GUI launches (Finder / Dock /
    // launchd) lack a controlling TTY on stdin.
    // SAFETY: `isatty` is a side-effect-free query on a file descriptor.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        return;
    }

    // A PATH that differs from launchd's stock GUI environment was set on
    // purpose (`open --env PATH=...`, `launchctl setenv PATH`, a wrapper
    // script) - honour it instead of replacing it with the login shell's.
    let inherited = std::env::var("PATH").unwrap_or_default();
    if !is_launchd_default_path(&inherited) {
        log::info!(
            "login-shell env: PATH was set explicitly ({} bytes); keeping it",
            inherited.len()
        );
        return;
    }

    let user_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    // Only POSIX-family shells (and fish, which parses `printf …; exec env`)
    // understand the capture script. Exotic shells (nushell / tcsh / csh /
    // xonsh / elvish) reject `-l -i -c '<posix>'`, so capture with `/bin/sh` as
    // the login shell instead - it still sources `/etc/profile`,
    // `/etc/profile.d/*`, and `~/.profile`, which is where the system PATH
    // (Homebrew, Nix, distro additions) lives. We only need PATH, so that's
    // enough.
    let capture_shell = if is_posix_capture_shell(&user_shell) {
        user_shell.clone()
    } else {
        "/bin/sh".to_string()
    };

    // Print a unique marker (to skip rc-script chatter) then `exec env` so the
    // dump is the last thing on stdout. Plain POSIX `env` (NOT GNU `env -0`)
    // keeps this working on BusyBox / Alpine; we only read the `PATH=` line
    // afterwards, which is newline-free, so newline-delimited output is safe.
    const MARKER: &str = "__PANEFLOW_LOGIN_ENV_V2__";
    let script = format!("printf '%s\\n' '{MARKER}'; exec env");

    let mut cmd = Command::new(&capture_shell);
    cmd.arg("-l").arg("-i").arg("-c").arg(&script);
    if let Some(home) = dirs::home_dir() {
        // Spawn from $HOME - a sane cwd for a login shell. (We intentionally do
        // NOT prefix `cd` in the script: we only consume PATH, so per-directory
        // hooks like direnv/asdf/mise are irrelevant here.)
        cmd.current_dir(home);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and the only thing we do between
    // fork and exec. Putting the capture shell in its own session means a stray
    // rc script that opens `/dev/tty` can't grab our controlling terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            log::debug!("login-shell env: could not spawn {capture_shell:?}: {e}");
            return;
        }
    };

    let Some(stdout) = child.stdout.take() else {
        terminate_login_shell_capture(&mut child);
        let _ = child.wait();
        return;
    };

    // Wait on the child, not on stdout EOF. A background job that inherited
    // the pipe would otherwise keep `read_to_end` blocked until the deadline
    // and the timeout arm would drop a PATH line already in the buffer.
    let deadline = Instant::now() + Duration::from_secs(5);
    let captured = capture_login_shell_stdout(&mut child, stdout, deadline, MARKER.as_bytes());
    if captured.truncated {
        log::warn!(
            "login-shell env: {capture_shell:?} wrote more than {LOGIN_ENV_CAPTURE_CAP} bytes; capture truncated"
        );
    }
    if captured.timed_out {
        log::warn!(
            "login-shell env: {capture_shell:?} did not finish within 5s; keeping the inherited PATH"
        );
        return;
    }

    match captured.path {
        Some(path) if !captured_path_has_system_bin(&path) => {
            log::warn!(
                "login-shell env: PATH captured from {capture_shell:?} lacks /usr/bin and /bin ({} bytes); keeping the inherited PATH",
                path.len()
            );
        }
        Some(path) if !path.is_empty() => {
            // SAFETY: main thread, before GPUI / any worker thread is spawned.
            // The reader was joined, or detached after the PATH snapshot, and
            // does not read the environment. We import ONLY PATH - see the
            // module doc for why adopting the full login environment is unsafe.
            unsafe { std::env::set_var("PATH", &path) };
            log::info!(
                "login-shell env: adopted PATH from {capture_shell:?} ({} bytes)",
                path.len()
            );
        }
        _ => {
            log::warn!(
                "login-shell env: no PATH captured from {capture_shell:?} (unsupported shell or empty env); keeping the inherited PATH"
            );
        }
    }
}

/// Upper bound on the bytes read from the capture shell's stdout. The `PATH=`
/// line follows the marker almost immediately, so 256 KiB is plenty; the cap
/// keeps a login rc that writes continuously from growing the capture buffer
/// (and the GUI process's RSS) until the 5 s deadline fires.
#[cfg(unix)]
const LOGIN_ENV_CAPTURE_CAP: u64 = 256 * 1024;

/// Read the capture shell's stdout into a buffer, stopping after
/// [`LOGIN_ENV_CAPTURE_CAP`] bytes. The buffer can never grow past the cap, no
/// matter how much the child writes before the deadline.
///
/// Production capture does not use this. `read_to_end` returns only at EOF or
/// the cap, which is the issue #712 hang; the cap test still pins the bound.
#[cfg(all(test, unix))]
fn read_login_shell_capture<R: std::io::Read>(stdout: &mut R) -> Vec<u8> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let _ = stdout.take(LOGIN_ENV_CAPTURE_CAP).read_to_end(&mut buf);
    buf
}

/// After the capture child has exited or been killed, how long to keep the
/// reader publishing before freezing the buffer. Long enough to copy a PATH
/// line already in the pipe; a write end held open by a background job must
/// not stretch this to the 5 s deadline.
#[cfg(unix)]
const LOGIN_ENV_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// Bytes read from the capture pipe, plus the flags the reader and the main
/// thread use to meet without waiting for EOF.
#[cfg(unix)]
struct CaptureRead {
    bytes: std::sync::Mutex<Vec<u8>>,
    stop: std::sync::atomic::AtomicBool,
    done: std::sync::atomic::AtomicBool,
    /// Set only when the reader observed stdout EOF. Cap and `stop` are not EOF:
    /// treating them as one would reap a still-running child until the deadline.
    eof: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl CaptureRead {
    fn new() -> Self {
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;
        Self {
            bytes: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
            eof: AtomicBool::new(false),
        }
    }

    fn len(&self) -> usize {
        self.bytes.lock().unwrap().len()
    }

    fn has_complete_path(&self, marker: &[u8]) -> bool {
        let guard = self.bytes.lock().unwrap();
        extract_path(&guard, marker).is_some()
    }

    /// Copy the bytes out while holding the mutex. Callers adopt PATH from
    /// this snapshot, never from the live buffer the reader may still append to.
    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
}

/// Why [`wait_for_capture_child`] stopped. `Exited` means `try_wait` already
/// reaped the child; the pid must not be signalled after that.
#[cfg(unix)]
enum CaptureWait {
    Exited,
    Reaped,
    Truncated,
    TimedOut,
}

/// Result of one login-shell capture attempt. `timed_out` is set only when the
/// child was still running at the deadline and the snapshot held no complete
/// `PATH=` line; the caller then keeps the inherited PATH.
#[cfg(unix)]
struct LoginShellCapture {
    path: Option<String>,
    truncated: bool,
    timed_out: bool,
}

/// Spawn a reader on `stdout` and wait until `child` exits or `deadline`.
///
/// Bytes are published as they arrive. After the child exits, the buffer is
/// polled briefly so the reader can drain what `env` already wrote even if a
/// background job still holds the write end. The returned PATH is a snapshot
/// taken under the mutex after the reader is joined, or before it is detached
/// if the read cannot be unblocked.
#[cfg(unix)]
fn capture_login_shell_stdout<R>(
    child: &mut std::process::Child,
    mut stdout: R,
    deadline: std::time::Instant,
    marker: &[u8],
) -> LoginShellCapture
where
    R: std::io::Read + std::os::unix::io::AsRawFd + Send + 'static,
{
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let capture = Arc::new(CaptureRead::new());
    let reader = {
        let capture = Arc::clone(&capture);
        std::thread::spawn(move || {
            let eof = publish_login_shell_capture(&mut stdout, &capture);
            // Drop the read end before advertising completion so a finished
            // reader is not still holding the pipe.
            drop(stdout);
            if eof {
                capture.eof.store(true, Ordering::Release);
            }
            capture.done.store(true, Ordering::Release);
        })
    };

    let wait = wait_for_capture_child(child, deadline, &capture);
    if matches!(wait, CaptureWait::TimedOut) {
        terminate_login_shell_capture(child);
        let _ = child.wait();
    }
    wait_for_published_path(&capture, marker);
    let buf = finish_capture_reader(&capture, reader);
    let path = extract_path(&buf, marker);
    let truncated =
        matches!(wait, CaptureWait::Truncated) || buf.len() as u64 >= LOGIN_ENV_CAPTURE_CAP;
    // A complete record is adopted even if we had to kill a child that
    // outlived the deadline. The hang that keeps the inherited PATH is the
    // one with no newline-terminated `PATH=` line.
    let timed_out = matches!(wait, CaptureWait::TimedOut) && path.is_none();
    LoginShellCapture {
        path,
        truncated,
        timed_out,
    }
}

#[cfg(unix)]
fn wait_for_capture_child(
    child: &mut std::process::Child,
    deadline: std::time::Instant,
    capture: &CaptureRead,
) -> CaptureWait {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    loop {
        if capture.len() as u64 >= LOGIN_ENV_CAPTURE_CAP {
            // The child out-wrote the cap and may still be running (and may
            // ignore SIGPIPE). Stop it before waiting on it. The pid has not
            // been reaped, so this kill cannot hit a reused pid.
            terminate_login_shell_capture(child);
            let _ = child.wait();
            return CaptureWait::Truncated;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                // `try_wait` reaped the child. Do not `kill(-pid)` afterwards:
                // the pid may already have been reused.
                return CaptureWait::Exited;
            }
            Ok(None) if Instant::now() >= deadline => return CaptureWait::TimedOut,
            Ok(None) if capture.eof.load(Ordering::Acquire) => {
                // Stdout closed; the child can still be alive (issue #683).
                // Reap only until the same deadline, then kill.
                let _ = reap_login_shell_capture(child, deadline);
                return CaptureWait::Reaped;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(error) => {
                log::warn!("login-shell env: waiting on the capture child failed: {error}");
                terminate_login_shell_capture(child);
                let _ = child.wait();
                return CaptureWait::Reaped;
            }
        }
    }
}

/// Poll until a complete PATH record is visible, the reader finishes, or
/// [`LOGIN_ENV_DRAIN_GRACE`] elapses. Does not wait for EOF.
#[cfg(unix)]
fn wait_for_published_path(capture: &CaptureRead, marker: &[u8]) {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let grace = Instant::now() + LOGIN_ENV_DRAIN_GRACE;
    loop {
        if capture.has_complete_path(marker) || capture.done.load(Ordering::Acquire) {
            return;
        }
        if Instant::now() >= grace {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Stop the reader and copy the buffer. Joins when the reader exits; if a
/// holder keeps a blocking read stuck, detaches after a short grace so startup
/// does not wait on that fd. The snapshot is taken under the mutex either way.
#[cfg(unix)]
fn finish_capture_reader(capture: &CaptureRead, reader: std::thread::JoinHandle<()>) -> Vec<u8> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    capture.stop.store(true, Ordering::Release);
    // The reader drains bytes already in the pipe before it honors `stop`,
    // then exits on the following empty read. 100 ms is enough to notice
    // `stop`; a holder that keeps `read` blocked falls through to detach.
    let grace = Instant::now() + Duration::from_millis(100);
    while !capture.done.load(Ordering::Acquire) && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(5));
    }
    if capture.done.load(Ordering::Acquire) {
        let _ = reader.join();
        capture.snapshot()
    } else {
        let snapshot = capture.snapshot();
        log::warn!(
            "login-shell env: stdout reader stayed blocked after timeout; continuing startup"
        );
        // Detach. The reader still holds its `Arc`, so dropping ours cannot
        // free the buffer under it.
        drop(reader);
        snapshot
    }
}

/// Publish `stdout` into `capture` until EOF, the byte cap, or `stop` observed
/// against an empty pipe. Returns `true` only on EOF.
#[cfg(unix)]
fn publish_login_shell_capture<R>(stdout: &mut R, capture: &CaptureRead) -> bool
where
    R: std::io::Read + std::os::unix::io::AsRawFd,
{
    use std::sync::atomic::Ordering;

    // Nonblocking so `stop` can end the thread when a background job holds
    // the write end. A blocking `read` would ignore `stop` until EOF and
    // could not be joined before `set_var`.
    let nonblocking = set_login_capture_nonblocking(stdout.as_raw_fd());
    let mut chunk = [0u8; 8192];
    let mut saw_eof = false;
    // Set when a `WouldBlock` has already been seen after `stop`. The next
    // empty read means bytes that raced with `stop` have been drained.
    let mut stop_was_empty = false;
    loop {
        let filled = capture.len() as u64;
        if filled >= LOGIN_ENV_CAPTURE_CAP {
            break;
        }
        let room = ((LOGIN_ENV_CAPTURE_CAP - filled) as usize).min(chunk.len());
        if room == 0 {
            break;
        }
        match stdout.read(&mut chunk[..room]) {
            Ok(0) => {
                saw_eof = true;
                break;
            }
            Ok(n) => {
                let mut guard = capture.bytes.lock().unwrap();
                let room = (LOGIN_ENV_CAPTURE_CAP as usize).saturating_sub(guard.len());
                let n = n.min(room);
                if n == 0 {
                    break;
                }
                guard.extend_from_slice(&chunk[..n]);
                stop_was_empty = false;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if capture.stop.load(Ordering::Acquire) {
                    if stop_was_empty || !nonblocking {
                        break;
                    }
                    stop_was_empty = true;
                    continue;
                }
                stop_was_empty = false;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    saw_eof
}

#[cfg(unix)]
fn set_login_capture_nonblocking(fd: std::os::unix::io::RawFd) -> bool {
    // SAFETY: `fd` is the capture pipe's read end, owned by the reader thread
    // for this call. `F_GETFL` / `F_SETFL` only change that descriptor's flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return false;
        }
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
    }
}

/// Shells whose `-l -i -c '<posix script>'` invocation runs our capture script.
/// fish parses `printf …; exec env` fine; nushell / tcsh / csh / xonsh / elvish
/// do not, and fall back to `/bin/sh`.
#[cfg(unix)]
fn is_posix_capture_shell(shell: &str) -> bool {
    let base = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(shell);
    matches!(
        base,
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash" | "mksh" | "fish"
    )
}

#[cfg(unix)]
/// Reap `child` before `deadline`. Returns `true` when it exited on its own.
///
/// Stdout EOF is not process exit (issue #683). `Child::wait` with no
/// deadline blocks startup for as long as the login shell keeps running.
/// Past the deadline the capture process group is killed, same as the
/// reader-timeout arm, and the inherited PATH stands when the buffer never
/// held a usable `PATH=` line.
#[cfg(unix)]
fn reap_login_shell_capture(child: &mut std::process::Child, deadline: std::time::Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if std::time::Instant::now() >= deadline => {
                log::warn!(
                    "login-shell env: capture child still running after stdout closed; terminating"
                );
                terminate_login_shell_capture(child);
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(error) => {
                log::warn!("login-shell env: waiting on the capture child failed: {error}");
                terminate_login_shell_capture(child);
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn terminate_login_shell_capture(child: &mut std::process::Child) {
    let child_pid = child.id();
    if child_pid <= i32::MAX as u32 {
        let pgid = child_pid as libc::pid_t;
        // SAFETY: the child was spawned after `setsid`, so its PID is also the
        // process group id for normal rc-script descendants.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Whether the inherited PATH is launchd's stock GUI environment (every entry
/// is one of the four system dirs, or PATH is empty/unset). Anything else was
/// set explicitly and must be honoured.
#[cfg(unix)]
fn is_launchd_default_path(path: &str) -> bool {
    const LAUNCHD_DEFAULT_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .all(|dir| {
            LAUNCHD_DEFAULT_DIRS
                .iter()
                .any(|d| dir == std::path::Path::new(d))
        })
}

/// Whether a captured PATH still carries a system bin dir (`/usr/bin` or
/// `/bin`). A login profile that assigns `PATH=$HOME/bin` instead of appending
/// would otherwise strip the system dirs from the GUI process.
#[cfg(unix)]
fn captured_path_has_system_bin(path: &str) -> bool {
    std::env::split_paths(path)
        .any(|dir| dir == std::path::Path::new("/usr/bin") || dir == std::path::Path::new("/bin"))
}

/// Extract the `PATH=` value from newline-delimited `env` output that follows
/// `marker`. PATH never contains a newline, so line-splitting is safe even when
/// some other variable's value spans multiple lines.
///
/// A record counts only when it ends in `\n`. The read cap can slice through
/// `PATH=...`; adopting that fragment would replace the inherited PATH with a
/// prefix (issue #683).
#[cfg(unix)]
fn extract_path(buf: &[u8], marker: &[u8]) -> Option<String> {
    let start = find_subslice(buf, marker)? + marker.len();
    let mut rest = &buf[start..];
    while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
        let line = &rest[..end];
        rest = &rest[end + 1..];
        if let Some(path) = line.strip_prefix(b"PATH=") {
            return std::str::from_utf8(path).ok().map(str::to_string);
        }
    }
    None
}

/// First index at which `needle` occurs in `haystack`. Tiny, allocation-free.
#[cfg(unix)]
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        LOGIN_ENV_CAPTURE_CAP, capture_login_shell_stdout, captured_path_has_system_bin,
        extract_path, find_subslice, is_launchd_default_path, is_posix_capture_shell,
        read_login_shell_capture, reap_login_shell_capture,
    };
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// The capture child is its own session, matching `load_login_shell_env`,
    /// so the process-group kill cannot reach this test.
    fn spawn_session(script: &str) -> std::process::Child {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().expect("capture probe must spawn")
    }

    #[test]
    fn reap_does_not_wait_out_a_child_that_closed_stdout() {
        let mut child = spawn_session("exec >/dev/null; exec sleep 30");
        let started = Instant::now();
        let exited_alone =
            reap_login_shell_capture(&mut child, started + Duration::from_millis(200));
        let elapsed = started.elapsed();
        assert!(
            !exited_alone,
            "a child that ignores stdout close must be terminated"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "reap held startup for {elapsed:?}"
        );
        assert!(
            child.try_wait().ok().flatten().is_some(),
            "the terminated child must be reaped"
        );
    }

    #[test]
    fn reap_collects_a_child_that_already_exited() {
        let mut child = spawn_session("exit 0");
        let started = Instant::now();
        assert!(
            reap_login_shell_capture(&mut child, started + Duration::from_secs(2)),
            "a finished capture child is reaped without a kill"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "reaping an exited child must not burn the deadline"
        );
    }

    #[test]
    fn capture_read_is_capped_for_a_child_that_never_stops_writing() {
        // A login rc that writes continuously: emulate 4x the cap worth of
        // output. The capture buffer must not grow past the cap.
        let mut endless = std::io::Read::take(std::io::repeat(b'x'), 4 * LOGIN_ENV_CAPTURE_CAP);
        let buf = read_login_shell_capture(&mut endless);
        assert_eq!(
            buf.len() as u64,
            LOGIN_ENV_CAPTURE_CAP,
            "capture buffer grew past the byte cap"
        );
        // A short, well-behaved dump is still read in full.
        let mut small = &b"__M__\nPATH=/usr/bin\n"[..];
        assert_eq!(
            read_login_shell_capture(&mut small),
            b"__M__\nPATH=/usr/bin\n"
        );
    }

    #[test]
    fn find_subslice_locates_marker() {
        assert_eq!(find_subslice(b"junk__MARK__data", b"__MARK__"), Some(4));
        assert_eq!(find_subslice(b"__MARK__data", b"__MARK__"), Some(0));
        assert_eq!(find_subslice(b"no marker here", b"__MARK__"), None);
        assert_eq!(find_subslice(b"", b"__MARK__"), None);
        assert_eq!(find_subslice(b"data", b""), None);
    }

    #[test]
    fn extract_path_reads_path_line_after_marker() {
        let out = b"chatter\n__M__\nFOO=bar\nPATH=/a:/b:/c\nHOME=/h\n";
        assert_eq!(extract_path(out, b"__M__").as_deref(), Some("/a:/b:/c"));
        // No marker -> None (we never read a PATH that precedes the marker).
        assert_eq!(extract_path(b"PATH=/x", b"__M__"), None);
        // Marker present but no PATH line.
        assert_eq!(extract_path(b"__M__\nFOO=bar\n", b"__M__"), None);
    }

    #[test]
    fn extract_path_survives_multiline_var_before_path() {
        // A variable whose value contains a newline must not corrupt parsing.
        let out = b"__M__\nSCRIPT=line1\nline2\nPATH=/usr/bin\n";
        assert_eq!(extract_path(out, b"__M__").as_deref(), Some("/usr/bin"));
    }

    #[test]
    fn extract_path_rejects_an_unterminated_fragment() {
        assert_eq!(
            extract_path(b"__M__\nPATH=/usr/bin:/opt/homebrew/bin", b"__M__"),
            None,
            "a cap that slices through PATH= must not be adopted"
        );
        assert_eq!(
            extract_path(b"__M__\nPATH=/usr/bin:/opt/homebrew/bin\n", b"__M__").as_deref(),
            Some("/usr/bin:/opt/homebrew/bin")
        );
    }

    #[test]
    fn explicit_inherited_path_is_not_launchd_default() {
        // launchd's stock GUI PATH (and an unset/empty one) still triggers capture.
        assert!(is_launchd_default_path("/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(is_launchd_default_path("/usr/bin:/bin"));
        assert!(is_launchd_default_path(""));
        // A PATH that was set explicitly (e.g. `open --env PATH=...`) is kept.
        assert!(!is_launchd_default_path("/custom/bin:/usr/bin:/bin"));
        assert!(!is_launchd_default_path(
            "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin"
        ));
        assert!(!is_launchd_default_path("/custom/bin"));
    }

    #[test]
    fn captured_path_without_system_bin_is_rejected() {
        assert!(captured_path_has_system_bin(
            "/usr/bin:/bin:/usr/sbin:/sbin"
        ));
        assert!(captured_path_has_system_bin("/opt/homebrew/bin:/usr/bin"));
        assert!(captured_path_has_system_bin("/Users/me/bin:/bin"));
        // A `.zshrc` that sets PATH=$HOME/bin without appending drops /usr/bin.
        assert!(!captured_path_has_system_bin("/Users/me/bin"));
        assert!(!captured_path_has_system_bin(
            "/Users/me/bin:/opt/homebrew/bin"
        ));
        assert!(!captured_path_has_system_bin(""));
    }

    #[test]
    fn is_posix_capture_shell_classifies() {
        for s in [
            "/bin/bash",
            "/usr/bin/zsh",
            "/bin/sh",
            "/usr/bin/fish",
            "dash",
        ] {
            assert!(is_posix_capture_shell(s), "{s} should be capturable");
        }
        for s in ["/usr/bin/nu", "/bin/tcsh", "/usr/bin/xonsh", "elvish"] {
            assert!(
                !is_posix_capture_shell(s),
                "{s} should fall back to /bin/sh"
            );
        }
    }

    /// A background process keeps the capture pipe open after the writer
    /// prints a complete PATH line and exits. Adoption must not wait for
    /// stdout EOF (issue #712). The sleeper is its own session so a
    /// process-group kill of the writer does not unblock the reader.
    #[test]
    fn login_shell_env_adopts_path_when_a_background_job_holds_stdout() {
        use std::os::unix::io::FromRawFd;

        struct KillOnDrop(Option<std::process::Child>);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let Some(child) = self.0.as_mut() else {
                    return;
                };
                // Already reaped (including by `try_wait` inside the capture).
                // Killing afterwards can signal a reused pid.
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        let mut fds = [0i32; 2];
        // SAFETY: `pipe` creates a fresh pair this test owns.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        let read_fd = fds[0];
        let write_fd = fds[1];
        // SAFETY: both fds came from `pipe` just above and are still open.
        unsafe {
            libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(write_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }

        let dup_stdio = |fd: i32| {
            // SAFETY: `fd` is the open write end. `dup` yields a new owned fd,
            // which `from_raw_fd` takes.
            let duped = unsafe { libc::dup(fd) };
            assert!(duped >= 0, "dup of the capture pipe failed");
            unsafe { Stdio::from_raw_fd(duped) }
        };

        let mut sleeper_cmd = Command::new("/bin/sleep");
        sleeper_cmd.arg("60");
        sleeper_cmd
            .stdin(Stdio::null())
            .stdout(dup_stdio(write_fd))
            .stderr(Stdio::null());
        unsafe {
            sleeper_cmd.pre_exec(|| {
                // Own session: `kill(-writer_pgid)` must not reach this holder.
                libc::setsid();
                Ok(())
            });
        }
        let sleeper = KillOnDrop(Some(sleeper_cmd.spawn().expect("stdout holder must spawn")));

        let script = "printf '%s\\n' '__PANEFLOW_LOGIN_ENV_V2__'; printf '%s\\n' 'PATH=/usr/bin:/bin:/opt/homebrew/bin'";
        let mut writer_cmd = Command::new("/bin/sh");
        writer_cmd.arg("-c").arg(script);
        writer_cmd
            .stdin(Stdio::null())
            .stdout(dup_stdio(write_fd))
            .stderr(Stdio::null());
        let mut writer = KillOnDrop(Some(writer_cmd.spawn().expect("writer must spawn")));

        // SAFETY: the parent still owns `write_fd`; both children have their
        // own dups. Closing here drops the parent's write end only.
        unsafe { libc::close(write_fd) };
        // SAFETY: `read_fd` is the open read end and is not closed elsewhere.
        let stdout = unsafe { std::fs::File::from_raw_fd(read_fd) };

        let started = Instant::now();
        let captured = capture_login_shell_stdout(
            writer.0.as_mut().expect("writer"),
            stdout,
            started + Duration::from_secs(5),
            b"__PANEFLOW_LOGIN_ENV_V2__",
        );
        let elapsed = started.elapsed();

        // Reap before asserts so a failing assert cannot skip the kill.
        drop(sleeper);

        assert!(
            !captured.timed_out,
            "a background stdout holder must not force the 5s timeout"
        );
        assert_eq!(
            captured.path.as_deref(),
            Some("/usr/bin:/bin:/opt/homebrew/bin")
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "capture took {elapsed:?}; a held stdout must not wait out the deadline"
        );
    }
}
