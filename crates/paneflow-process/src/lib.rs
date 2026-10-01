//! Bounded external-process execution shared across Paneflow crates.
//!
//! Its only dependency is `libc`, which the embedded `paneflow-shim` already
//! links, so it does not inflate the binary that ships inside the main
//! executable (EP-002, US-005).
//!
//! [`run_with_timeout`] gives non-interactive subprocesses a wall-clock deadline
//! and strict stdout/stderr capture limits. It is synchronous and is meant to
//! run on a background thread, never on the GPUI render thread.
//!
//! [`Command`] is how PaneFlow starts a non-PTY child: `posix_spawn` with
//! `POSIX_SPAWN_CLOEXEC_DEFAULT`, so the child inherits only the descriptors
//! it is given (issue #1126). [`spawn_piped`], [`run_with_timeout`] and
//! [`spawn_detached`] build on it. The spawn exclusion that kept spawns out of
//! the windows where the IPC server's sockets (issue #1115) and stdio pipes
//! (issue #1124) are not close-on-exec yet still applies to every spawn.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod posix;

pub use posix::{inheritable_descriptors, Child, Command, Stdio};

use std::error::Error;
use std::fmt;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{OnceLock, PoisonError, RwLock, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on captured stderr. stderr is diagnostics-only, so a small fixed
/// cap is enough. Exceeding it fails the run instead of returning partial data.
const STDERR_CAP: u64 = 64 * 1024;

/// How often [`run_with_timeout`] polls the child for exit. Small enough that a
/// fast command returns promptly, large enough that a multi-minute deadline
/// does not spin the CPU.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Issue #1115: exclusion between spawning a child and creating a descriptor
/// that is not close-on-exec yet.
///
/// macOS has no `SOCK_CLOEXEC`, no `accept4` and no `pipe2`, so `socket()`,
/// `accept()` and `pipe()` return descriptors that are only marked
/// `FD_CLOEXEC` by a second `fcntl`.
/// A `posix_spawn` or `fork` on another thread between the two calls copies
/// the descriptor into the child, where it survives exec for the child's
/// whole life. [`Command::start`] and [`spawn`] hold the shared side for the
/// length of the spawn call, so spawns never wait on each other. A
/// [`Command`] child can no longer copy such a descriptor (issue #1126); the
/// exclusion stays for std spawns until every one is gone.
/// [`with_spawns_excluded`] and [`try_with_spawns_excluded`] hold the
/// exclusive side across the create-then-mark window. The exclusive side is
/// only ever tried, never queued for: a queued writer would make every later
/// spawner, including the render thread, wait behind whichever spawn call is
/// slowest.
///
/// The cost runs the other way: while a spawn call is slow (a first-exec
/// Gatekeeper scan of a new binary, for example), the IPC server does not
/// accept, and new connections wait in the listen backlog until it returns.
/// That delay is the accepted trade-off for never leaking a socket.
static SPAWN_EXCLUSION: RwLock<()> = RwLock::new(());

/// How often [`with_spawns_excluded`] retries while a spawn is in flight.
const SPAWN_EXCLUSION_RETRY: Duration = Duration::from_millis(1);

/// Spawn a std `command`, never while a [`with_spawns_excluded`] or
/// [`try_with_spawns_excluded`] window is open (issue #1115).
///
/// Production code starts children with [`Command::start`] instead
/// (issue #1126). std's child inherits every descriptor that is not
/// close-on-exec, and a `pre_exec` closure makes std fork and then wait on
/// an exec-status pipe that a concurrent `posix_spawn` can copy. This stays
/// for tests that need a `pre_exec` to hold a spawn in flight.
pub fn spawn(command: &mut std::process::Command) -> io::Result<std::process::Child> {
    with_spawn_shared_side(|| command.spawn())
}

/// Run `spawn` under the shared side of [`SPAWN_EXCLUSION`].
pub(crate) fn with_spawn_shared_side<T>(spawn: impl FnOnce() -> T) -> T {
    let _shared = SPAWN_EXCLUSION
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    spawn()
}

/// Which of a child's standard streams [`spawn_piped`] connects to the parent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pipes {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

/// [`Command::start`] with a pipe to the parent for each stream `pipes`
/// selects (issue #1124).
///
/// On macOS `pipe()` returns ends that are only marked close-on-exec by a
/// separate `fcntl`. A [`Command`] child never inherits them, but a std
/// spawn in that window (a test's [`spawn`], or the PTY shell's fork) would.
///
/// This creates the pipes inside a [`with_spawns_excluded`] window and spawns
/// under the shared side as usual. The window lasts a few syscalls, so other
/// spawners, the render thread included, wait for microseconds at most.
/// Opening it, though, waits for every spawn already in flight to return:
/// while another thread's spawn call is slow (a 12-20 s first-exec
/// Gatekeeper scan, for example), this call waits as long before its own
/// child starts. Keep it off the render thread. Holding the exclusive side
/// across the whole spawn instead would make every other spawner wait on
/// this one as well.
///
/// The selected streams on `command` are reset to [`Stdio::Null`] before
/// this returns, which closes the parent's copy of the child's ends. The
/// pipes' parent ends are returned in `Child::stdin`, `stdout` and `stderr`.
pub fn spawn_piped(command: &mut Command, pipes: Pipes) -> io::Result<Child> {
    let StdioPipes {
        stdin,
        stdout,
        stderr,
    } = with_spawns_excluded(|| StdioPipes::create(pipes))?;

    let mut parent_stdin = None;
    let mut parent_stdout = None;
    let mut parent_stderr = None;
    if let Some((child_end, parent_end)) = stdin {
        command.stdin(child_end);
        parent_stdin = Some(ChildStdin::from(OwnedFd::from(parent_end)));
    }
    if let Some((parent_end, child_end)) = stdout {
        command.stdout(child_end);
        parent_stdout = Some(ChildStdout::from(OwnedFd::from(parent_end)));
    }
    if let Some((parent_end, child_end)) = stderr {
        command.stderr(child_end);
        parent_stderr = Some(ChildStderr::from(OwnedFd::from(parent_end)));
    }

    let spawned = command.start();
    // `Command` owns the child's ends until its stdio is replaced. Close
    // them now: a reader sees EOF only once every write end is closed.
    if pipes.stdin {
        command.stdin(Stdio::Null);
    }
    if pipes.stdout {
        command.stdout(Stdio::Null);
    }
    if pipes.stderr {
        command.stderr(Stdio::Null);
    }

    let mut child = spawned?;
    child.stdin = parent_stdin;
    child.stdout = parent_stdout;
    child.stderr = parent_stderr;
    Ok(child)
}

/// The pipes [`spawn_piped`] creates, each as `(read end, write end)`.
struct StdioPipes {
    stdin: Option<(PipeReader, PipeWriter)>,
    stdout: Option<(PipeReader, PipeWriter)>,
    stderr: Option<(PipeReader, PipeWriter)>,
}

impl StdioPipes {
    /// Only call inside a [`with_spawns_excluded`] window: on macOS each end
    /// is inheritable until `io::pipe` marks it close-on-exec.
    fn create(pipes: Pipes) -> io::Result<Self> {
        let pipe_if = |wanted: bool| wanted.then(io::pipe).transpose();
        let created = Self {
            stdin: pipe_if(pipes.stdin)?,
            stdout: pipe_if(pipes.stdout)?,
            stderr: pipe_if(pipes.stderr)?,
        };
        #[cfg(any(test, feature = "test-support"))]
        test_support::run_pipe_window_hook(&created);
        Ok(created)
    }
}

/// Test seam for code that races [`spawn_piped`] (issues #1124, #1127).
///
/// Compiled for this crate's tests and for any build that enables the
/// `test-support` feature through a dev-dependency: workspace test and clippy
/// runs, where feature unification also puts it in paneflow-app's dev build.
/// Release and release-min artifacts never include it.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::StdioPipes;
    use std::cell::RefCell;

    type PipeWindowHook = Box<dyn FnOnce(&[i32])>;

    thread_local! {
        /// Runs once, on this thread, inside the next `StdioPipes::create`
        /// window, with every descriptor that window created.
        static PIPE_WINDOW_HOOK: RefCell<Option<PipeWindowHook>> =
            const { RefCell::new(None) };
    }

    /// Run `hook` inside the next pipe-creation window [`super::spawn_piped`]
    /// opens on the calling thread, with the raw descriptors it created.
    /// The window stays open, with every other spawner waiting, until the
    /// hook returns.
    pub fn set_pipe_window_hook(hook: impl FnOnce(&[i32]) + 'static) {
        PIPE_WINDOW_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn run_pipe_window_hook(pipes: &StdioPipes) {
        use std::os::fd::AsRawFd;

        let Some(hook) = PIPE_WINDOW_HOOK.with(|slot| slot.borrow_mut().take()) else {
            return;
        };
        let fds: Vec<i32> = [&pipes.stdin, &pipes.stdout, &pipes.stderr]
            .into_iter()
            .flatten()
            .flat_map(|(reader, writer)| [reader.as_raw_fd(), writer.as_raw_fd()])
            .collect();
        hook(&fds);
    }
}

/// Run `create` while no [`Command::start`] or [`spawn`] call is in flight,
/// or return `None` without running it when one is.
///
/// For poll loops, such as a non-blocking `accept`: the caller retries on its
/// next tick instead of waiting.
pub fn try_with_spawns_excluded<T>(create: impl FnOnce() -> T) -> Option<T> {
    let _exclusive = match SPAWN_EXCLUSION.try_write() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => return None,
    };
    Some(create())
}

/// Run `create` while no [`Command::start`] or [`spawn`] call is in flight,
/// waiting for in-flight spawns to return first. Keep `create` short: spawns
/// wait while it runs. Never call it on the render thread: an in-flight
/// spawn can take seconds (a first-exec Gatekeeper scan).
///
/// `create` must also mark every descriptor it opens close-on-exec before it
/// returns, as `interprocess` and `std` do. Since issue #1126 only a std
/// spawn ([`spawn`], or the PTY shell's fork) could still copy one that is
/// not marked yet.
pub fn with_spawns_excluded<T>(create: impl FnOnce() -> T) -> T {
    loop {
        match SPAWN_EXCLUSION.try_write() {
            Ok(_exclusive) => return create(),
            Err(TryLockError::Poisoned(poisoned)) => {
                let _exclusive = poisoned.into_inner();
                return create();
            }
            Err(TryLockError::WouldBlock) => thread::sleep(SPAWN_EXCLUSION_RETRY),
        }
    }
}

/// Output from a bounded process run.
///
/// `stdout` and `stderr` are complete. A process that exceeds either capture
/// limit fails with [`ProcError::OutputLimitExceeded`] instead of returning
/// partial data as a successful result.
#[derive(Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Captured subprocess stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl fmt::Display for OutputStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdout => f.write_str("stdout"),
            Self::Stderr => f.write_str("stderr"),
        }
    }
}

/// Why a bounded run did not produce a normal [`BoundedOutput`].
#[derive(Debug)]
pub enum ProcError {
    /// The child could not be spawned.
    Spawn(io::Error),
    /// A configured capture limit cannot be represented safely on this target.
    InvalidOutputLimit(u64),
    /// The internal supervisor could not be prepared or reached an invalid
    /// lifecycle state.
    Supervision(io::Error),
    /// A dedicated stream reader thread could not be started.
    ReaderSpawn {
        stream: OutputStream,
        source: io::Error,
    },
    /// Polling the child's status failed.
    Wait(io::Error),
    /// Capturing one of the child's streams failed.
    Read {
        stream: OutputStream,
        source: io::Error,
    },
    /// The child produced more bytes than the configured capture limit.
    OutputLimitExceeded { stream: OutputStream, cap: u64 },
    /// The deadline elapsed before the child and its inherited pipes completed.
    /// The child tree was terminated best-effort and cleanup was detached so the
    /// caller is released by the deadline.
    Timeout,
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcError::Spawn(e) => write!(f, "failed to spawn process: {e}"),
            ProcError::InvalidOutputLimit(cap) => {
                write!(f, "capture limit {cap} cannot be represented safely")
            }
            ProcError::Supervision(e) => write!(f, "process supervision failed: {e}"),
            ProcError::ReaderSpawn { stream, source } => {
                write!(f, "failed to start {stream} reader: {source}")
            }
            ProcError::Wait(e) => write!(f, "failed to poll process status: {e}"),
            ProcError::Read { stream, source } => {
                write!(f, "failed to capture process {stream}: {source}")
            }
            ProcError::OutputLimitExceeded { stream, cap } => {
                write!(f, "process {stream} exceeded its {cap}-byte capture limit")
            }
            ProcError::Timeout => write!(f, "process exceeded its deadline; termination requested"),
        }
    }
}

impl Error for ProcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ProcError::Spawn(e) | ProcError::Supervision(e) | ProcError::Wait(e) => Some(e),
            ProcError::ReaderSpawn { source, .. } | ProcError::Read { source, .. } => Some(source),
            ProcError::InvalidOutputLimit(_)
            | ProcError::OutputLimitExceeded { .. }
            | ProcError::Timeout => None,
        }
    }
}

/// Run `cmd` to completion under a wall-clock `deadline`, capturing at most
/// `stdout_cap` bytes of stdout (and a small fixed cap of stderr).
///
/// The deadline starts after the child is successfully spawned. Process creation
/// itself is owned by the OS and may still block on platform-level executable
/// lookup or antivirus hooks. Before that, creating the capture pipes waits
/// for every other spawn already in flight (see [`spawn_piped`]); that wait
/// is not counted against the deadline either.
///
/// - stdin is `/dev/null` so the child can never block waiting on a prompt.
/// - stdout/stderr are read on dedicated threads; exceeding either cap closes
///   the pipe, terminates the run, and returns [`ProcError::OutputLimitExceeded`].
/// - the child leads its own process group; every error path terminates that
///   group best-effort before cleanup is detached.
pub fn run_with_timeout(
    cmd: std::process::Command,
    deadline: Duration,
    stdout_cap: u64,
) -> Result<BoundedOutput, ProcError> {
    run_with_timeout_capped(cmd, deadline, stdout_cap, STDERR_CAP)
}

/// [`run_with_timeout`] that writes `stdin` to the child and closes the pipe
/// (EOF) so plumbing such as `git cat-file --batch` can read a request set.
pub fn run_with_timeout_stdin(
    cmd: std::process::Command,
    stdin: &[u8],
    deadline: Duration,
    stdout_cap: u64,
) -> Result<BoundedOutput, ProcError> {
    run_bounded(cmd, Some(stdin), deadline, stdout_cap, STDERR_CAP)
}

/// [`run_with_timeout`] with an explicit stderr capture cap.
fn run_with_timeout_capped(
    cmd: std::process::Command,
    deadline: Duration,
    stdout_cap: u64,
    stderr_cap: u64,
) -> Result<BoundedOutput, ProcError> {
    run_bounded(cmd, None, deadline, stdout_cap, stderr_cap)
}

/// `cmd` contributes its program, arguments, environment changes and working
/// directory (see `From<&std::process::Command>` for [`Command`]).
fn run_bounded(
    cmd: std::process::Command,
    stdin: Option<&[u8]>,
    deadline: Duration,
    stdout_cap: u64,
    stderr_cap: u64,
) -> Result<BoundedOutput, ProcError> {
    let stdout_cap = validate_capture_cap(stdout_cap)?;
    let stderr_cap = validate_capture_cap(stderr_cap)?;

    let mut cmd = Command::from(&cmd);
    if stdin.is_none() {
        cmd.stdin(Stdio::Null);
    }
    let pipes = Pipes {
        stdin: stdin.is_some(),
        stdout: true,
        stderr: true,
    };

    cmd.process_group(0);
    // Prepare the reaper before spawning the child. Once a process exists,
    // every error path can hand it to this already-running thread without
    // risking a late thread-spawn failure or blocking the caller's deadline.
    let cleanup = spawn_cleanup_worker()?;
    let child = spawn_piped(&mut cmd, pipes).map_err(ProcError::Spawn)?;
    let start = Instant::now();
    let mut process = RunningProcess::new(child, cleanup);

    // Hand the pipe ends to reader threads before polling: if we polled while
    // the child filled a ~64 KiB pipe buffer it would block on write and we'd
    // kill a child that was not actually hung.
    let stdout_pipe = process
        .child_mut()?
        .stdout
        .take()
        .ok_or_else(|| supervision_error("stdout capture pipe unavailable after spawn"))?;
    let stderr_pipe = process
        .child_mut()?
        .stderr
        .take()
        .ok_or_else(|| supervision_error("stderr capture pipe unavailable after spawn"))?;
    if let Some(bytes) = stdin {
        let stdin_pipe = process
            .child_mut()?
            .stdin
            .take()
            .ok_or_else(|| supervision_error("stdin pipe unavailable after spawn"))?;
        spawn_stdin_writer(stdin_pipe, bytes.to_vec())?;
    }

    let (reader_tx, reader_rx) = mpsc::channel();
    process.attach_reader(reader_rx);
    spawn_bounded_reader(
        stdout_pipe,
        stdout_cap,
        OutputStream::Stdout,
        reader_tx.clone(),
    )?;
    spawn_bounded_reader(stderr_pipe, stderr_cap, OutputStream::Stderr, reader_tx)?;

    let mut capture = CaptureState::default();
    let status = loop {
        drain_ready_reader_messages(process.reader()?, &mut capture)?;
        match process.child_mut()?.try_wait().map_err(ProcError::Wait)? {
            Some(status) => break status,
            None => {
                let Some(sleep_for) = poll_sleep_duration(start, deadline) else {
                    return Err(ProcError::Timeout);
                };
                thread::sleep(sleep_for);
            }
        }
    };

    while !capture.is_complete() {
        let remaining = remaining_until(start, deadline).unwrap_or(Duration::ZERO);
        match process.reader()?.recv_timeout(remaining) {
            Ok(message) => capture.record(message)?,
            Err(RecvTimeoutError::Timeout) => return Err(ProcError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(supervision_error(
                    "output readers disconnected before reporting both streams",
                ));
            }
        }
    }

    let (stdout, stderr) = capture.finish()?;
    process.complete();

    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn poll_sleep_duration(start: Instant, deadline: Duration) -> Option<Duration> {
    let elapsed = start.elapsed();
    if elapsed >= deadline {
        None
    } else {
        Some((deadline - elapsed).min(POLL_INTERVAL))
    }
}

fn remaining_until(start: Instant, deadline: Duration) -> Option<Duration> {
    let elapsed = start.elapsed();
    if elapsed >= deadline {
        None
    } else {
        Some(deadline - elapsed)
    }
}

fn validate_capture_cap(cap: u64) -> Result<usize, ProcError> {
    if cap.checked_add(1).is_none() {
        return Err(ProcError::InvalidOutputLimit(cap));
    }
    usize::try_from(cap).map_err(|_| ProcError::InvalidOutputLimit(cap))
}

fn supervision_error(message: &'static str) -> ProcError {
    ProcError::Supervision(io::Error::other(message))
}

struct RunningProcess {
    child: Option<Child>,
    tree: ProcessTree,
    reader: Option<Receiver<ReaderMessage>>,
    cleanup: mpsc::Sender<CleanupResources>,
}

impl RunningProcess {
    fn new(child: Child, cleanup: mpsc::Sender<CleanupResources>) -> Self {
        Self {
            tree: ProcessTree::for_child(&child),
            child: Some(child),
            reader: None,
            cleanup,
        }
    }

    fn child_mut(&mut self) -> Result<&mut Child, ProcError> {
        self.child
            .as_mut()
            .ok_or_else(|| supervision_error("child already consumed"))
    }

    fn attach_reader(&mut self, reader: Receiver<ReaderMessage>) {
        self.reader = Some(reader);
    }

    fn reader(&self) -> Result<&Receiver<ReaderMessage>, ProcError> {
        self.reader
            .as_ref()
            .ok_or_else(|| supervision_error("reader channel not attached"))
    }

    /// The child exited and both streams were captured: release it without
    /// the teardown `Drop` runs on every early return.
    fn complete(mut self) {
        self.child = None;
    }

    fn terminate_and_detach(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        self.tree.terminate(&mut child);
        send_cleanup(&self.cleanup, child, self.reader.take());
    }
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        self.terminate_and_detach();
    }
}

struct CleanupResources {
    child: Child,
    reader: Option<Receiver<ReaderMessage>>,
}

fn spawn_cleanup_worker() -> Result<mpsc::Sender<CleanupResources>, ProcError> {
    let (sender, receiver) = mpsc::channel::<CleanupResources>();
    thread::Builder::new()
        .name("paneflow-process-cleanup".to_string())
        .spawn(move || {
            let Ok(mut resources) = receiver.recv() else {
                return;
            };
            let _ = resources.child.wait();
            if let Some(reader) = resources.reader {
                while reader.recv().is_ok() {}
            }
        })
        .map_err(ProcError::Supervision)?;
    Ok(sender)
}

fn send_cleanup(
    sender: &mpsc::Sender<CleanupResources>,
    child: Child,
    reader: Option<Receiver<ReaderMessage>>,
) {
    let resources = CleanupResources { child, reader };
    if let Err(mpsc::SendError(mut resources)) = sender.send(resources) {
        // The worker only disconnects if it panics. Never replace the caller's
        // wall-clock bound with a synchronous wait in that exceptional path.
        let _ = resources.child.try_wait();
    }
}

struct ProcessTree {
    #[cfg(unix)]
    pid: u32,
}

impl ProcessTree {
    fn for_child(child: &Child) -> Self {
        Self {
            #[cfg(unix)]
            pid: child.id(),
        }
    }

    fn terminate(&self, child: &mut Child) {
        #[cfg(unix)]
        kill_process_group(self.pid);
        let _ = child.kill();
    }
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    const SIGKILL: i32 = 9;

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    if let Ok(pid) = i32::try_from(pid) {
        let _ = unsafe { kill(-pid, SIGKILL) };
    }
}

#[derive(Debug)]
enum ReaderFailure {
    Read(io::Error),
    LimitExceeded { cap: u64 },
}

#[derive(Debug)]
struct ReaderMessage {
    stream: OutputStream,
    result: Result<Vec<u8>, ReaderFailure>,
}

fn spawn_stdin_writer(mut pipe: std::process::ChildStdin, bytes: Vec<u8>) -> Result<(), ProcError> {
    thread::Builder::new()
        .name("paneflow-process-stdin".to_string())
        .spawn(move || {
            let _ = pipe.write_all(&bytes);
        })
        .map(|_| ())
        .map_err(ProcError::Supervision)
}

fn spawn_bounded_reader<R>(
    pipe: R,
    cap: usize,
    stream: OutputStream,
    sender: mpsc::Sender<ReaderMessage>,
) -> Result<(), ProcError>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(format!("paneflow-process-{stream}"))
        .spawn(move || {
            let result = read_bounded(pipe, cap);
            let _ = sender.send(ReaderMessage { stream, result });
        })
        .map(|_| ())
        .map_err(|source| ProcError::ReaderSpawn { stream, source })
}

fn read_bounded<R>(mut pipe: R, cap: usize) -> Result<Vec<u8>, ReaderFailure>
where
    R: Read,
{
    let mut bytes = Vec::new();
    pipe.by_ref()
        .take((cap as u64) + 1)
        .read_to_end(&mut bytes)
        .map_err(ReaderFailure::Read)?;
    if bytes.len() > cap {
        return Err(ReaderFailure::LimitExceeded { cap: cap as u64 });
    }
    Ok(bytes)
}

#[derive(Default)]
struct CaptureState {
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
}

impl CaptureState {
    fn record(&mut self, message: ReaderMessage) -> Result<(), ProcError> {
        let bytes = match message.result {
            Ok(bytes) => bytes,
            Err(ReaderFailure::Read(source)) => {
                return Err(ProcError::Read {
                    stream: message.stream,
                    source,
                });
            }
            Err(ReaderFailure::LimitExceeded { cap }) => {
                return Err(ProcError::OutputLimitExceeded {
                    stream: message.stream,
                    cap,
                });
            }
        };
        let slot = match message.stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        };
        if slot.is_some() {
            return Err(supervision_error("reader reported the same stream twice"));
        }
        *slot = Some(bytes);
        Ok(())
    }

    fn is_complete(&self) -> bool {
        self.stdout.is_some() && self.stderr.is_some()
    }

    fn finish(mut self) -> Result<(Vec<u8>, Vec<u8>), ProcError> {
        let stdout = self
            .stdout
            .take()
            .ok_or_else(|| supervision_error("stdout reader result missing"))?;
        let stderr = self
            .stderr
            .take()
            .ok_or_else(|| supervision_error("stderr reader result missing"))?;
        Ok((stdout, stderr))
    }
}

fn drain_ready_reader_messages(
    reader: &Receiver<ReaderMessage>,
    capture: &mut CaptureState,
) -> Result<(), ProcError> {
    loop {
        match reader.try_recv() {
            Ok(message) => capture.record(message)?,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) if capture.is_complete() => return Ok(()),
            Err(TryRecvError::Disconnected) => {
                return Err(supervision_error(
                    "output readers disconnected before reporting both streams",
                ));
            }
        }
    }
}

/// How often the detached reaper re-checks the children it holds. The thread
/// blocks on the channel while it holds none, so this only costs a wake-up
/// while at least one launch is still running.
const DETACHED_REAP_INTERVAL: Duration = Duration::from_millis(500);

/// Channel to the shared detached-child reaper. `None` when the reaper thread
/// could not be started, in which case [`spawn_detached`] still starts the
/// child and leaves it unreaped rather than dropping the launch.
static DETACHED_REAPER: OnceLock<Option<mpsc::Sender<Child>>> = OnceLock::new();

/// Spawn a process Paneflow launches but never observes (an editor, a file
/// manager) and hand its exit status to a shared reaper thread.
///
/// `std::process::Child` has no reaping `Drop`: the standard library documents
/// that it "does *not* automatically wait on child processes (not even if the
/// `Child` is dropped)". On Unix, dropping the handle therefore leaves the child
/// as a zombie holding a PID slot for the parent's whole lifetime. CLI launchers
/// make that immediate: `zed .` hands the path to the already-running instance
/// over a socket and exits within milliseconds, so every launch used to leak one
/// permanent `<defunct>` entry.
///
/// This never waits synchronously: it returns as soon as the spawn itself
/// succeeds or fails, so it is safe to call from the render thread. Only spawn
/// errors are reported; the child's exit code is deliberately discarded.
///
/// The child is started with [`Command::start`] from `command`'s program,
/// arguments, environment changes and working directory, with inherited
/// stdio (issue #1126).
pub fn spawn_detached(command: &mut std::process::Command) -> io::Result<()> {
    let child = Command::from(&*command).start()?;
    // A missing or dead reaper is not worth failing the launch over: the child
    // is already running and the caller wanted it running. Dropping the handle
    // here is exactly the pre-existing behavior.
    let sender = DETACHED_REAPER.get_or_init(start_detached_reaper).as_ref();
    send_to_detached_reaper(sender, child, &mut io::stderr().lock());
    Ok(())
}

fn start_detached_reaper() -> Option<mpsc::Sender<Child>> {
    let mut diagnostic = io::stderr().lock();
    start_detached_reaper_with(
        |receiver| {
            thread::Builder::new()
                .name("paneflow-detached-reaper".to_owned())
                .spawn(move || reap_detached_children(&receiver))
        },
        &mut diagnostic,
    )
}

fn start_detached_reaper_with<F, W>(spawn: F, diagnostic: &mut W) -> Option<mpsc::Sender<Child>>
where
    F: FnOnce(Receiver<Child>) -> io::Result<thread::JoinHandle<()>>,
    W: Write,
{
    let (sender, receiver) = mpsc::channel::<Child>();
    match spawn(receiver) {
        Ok(_) => Some(sender),
        Err(error) => {
            let _ = writeln!(
                diagnostic,
                "paneflow-process warning: detached reaper thread could not be started: {error}"
            );
            None
        }
    }
}

fn send_to_detached_reaper<T, W>(sender: Option<&mpsc::Sender<T>>, child: T, diagnostic: &mut W)
where
    W: Write,
{
    match sender {
        Some(sender) => {
            if let Err(error) = sender.send(child) {
                let _ = writeln!(
                    diagnostic,
                    "paneflow-process warning: detached child could not be sent to reaper: {error}"
                );
            }
        }
        None => {
            let _ = writeln!(
                diagnostic,
                "paneflow-process warning: detached reaper is unavailable; child will not be reaped"
            );
        }
    }
}

/// Hold every spawned child until it exits.
///
/// Polling beats a blocking `wait()` per child here: one long-lived launch (a
/// file manager that outlives the click, an `xdg-open` handler that `exec`s the
/// browser instead of returning) must not block the reaping of every launch
/// queued behind it. One thread serves all call sites.
fn reap_detached_children(receiver: &Receiver<Child>) {
    let mut pending: Vec<Child> = Vec::new();
    let mut connected = true;
    while connected || !pending.is_empty() {
        if pending.is_empty() {
            match receiver.recv() {
                Ok(child) => pending.push(child),
                Err(_) => return,
            }
        } else {
            match receiver.recv_timeout(DETACHED_REAP_INTERVAL) {
                Ok(child) => pending.push(child),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => connected = false,
            }
        }
        // A `try_wait` error (ECHILD, a PID already reaped elsewhere) is
        // terminal for that handle: retrying it would never succeed.
        pending.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shell wrapper so the behavior tests stay readable across platforms.
    #[cfg(unix)]
    fn sh(script: &str) -> std::process::Command {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(script);
        c
    }

    #[cfg(unix)]
    fn stdout_command() -> std::process::Command {
        sh("printf hello")
    }

    #[cfg(unix)]
    fn sleep_command() -> std::process::Command {
        sh("sleep 30")
    }

    #[cfg(unix)]
    fn set_cloexec(fd: i32, on: bool) {
        const F_SETFD: i32 = 2;
        const FD_CLOEXEC: i32 = 1;
        unsafe extern "C" {
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        }
        let flag = if on { FD_CLOEXEC } else { 0 };
        assert_eq!(unsafe { fcntl(fd, F_SETFD, flag) }, 0, "fcntl(F_SETFD)");
    }

    use super::test_support::set_pipe_window_hook;

    /// Held by the test that spawns through std and by every test that
    /// leaves a descriptor inheritable outside the exclusion, so that std
    /// child never copies it.
    static STD_SPAWN_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn std_spawn_test_lock() -> std::sync::MutexGuard<'static, ()> {
        STD_SPAWN_TEST
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Issue #1124: the pipes [`spawn_piped`] creates are never copied into
    /// a concurrent spawn's child.
    ///
    /// The hook holds the pipe-creation window open for 300 ms with every
    /// new end inheritable, the state std's macOS `Stdio::piped()` leaves a
    /// pipe in until its follow-up `fcntl` calls. If the concurrent spawn ran
    /// inside it, the 30 s `/bin/sleep` would hold the probe's stdout write
    /// end, the probe would see no EOF after `printf` exits, and the run
    /// would end in `Timeout` instead of returning `hello`.
    #[cfg(unix)]
    #[test]
    fn piped_spawn_never_leaks_its_pipes_into_a_concurrent_spawn() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (window_open_tx, window_open_rx) = mpsc::channel();
        let window_closed = Arc::new(AtomicBool::new(false));
        let prober = {
            let window_closed = Arc::clone(&window_closed);
            thread::spawn(move || {
                set_pipe_window_hook(move |fds| {
                    for &fd in fds {
                        set_cloexec(fd, false);
                    }
                    let _ = window_open_tx.send(());
                    thread::sleep(Duration::from_millis(300));
                    for &fd in fds {
                        set_cloexec(fd, true);
                    }
                    window_closed.store(true, Ordering::SeqCst);
                });
                let started = Instant::now();
                let result = run_with_timeout(sh("printf hello"), Duration::from_secs(3), 1 << 20);
                (result, started.elapsed())
            })
        };

        window_open_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("run_with_timeout must create its pipes through spawn_piped");
        // Piped as well, like a PTY guard's control channel.
        let mut holder = spawn_piped(
            Command::new("/bin/sleep")
                .arg("30")
                .stdout(Stdio::Null)
                .stderr(Stdio::Null),
            Pipes {
                stdin: true,
                ..Pipes::default()
            },
        )
        .expect("spawn /bin/sleep");
        let spawned_after_window = window_closed.load(Ordering::SeqCst);
        let (result, elapsed) = prober.join().expect("prober thread");
        let _ = holder.kill();
        let _ = holder.wait();

        let out = result.expect(
            "printf exited, so its stdout must reach EOF: the concurrent child must not \
             hold a copy of the write end",
        );
        assert_eq!(out.stdout, b"hello");
        assert!(
            elapsed < Duration::from_secs(2),
            "EOF must follow printf's exit promptly, took {elapsed:?}"
        );
        assert!(
            spawned_after_window,
            "a concurrent spawn must wait until the pipe window closes"
        );
    }

    /// The parent's copy of each child end is closed, so a spawned child's
    /// stdout reaches EOF when it exits, and the command's stdio is reset.
    #[cfg(unix)]
    #[test]
    fn spawn_piped_hands_back_parent_ends_and_closes_child_ends() {
        let mut command = Command::from(&sh("cat; printf done >&2"));
        let mut child = spawn_piped(
            &mut command,
            Pipes {
                stdin: true,
                stdout: true,
                stderr: true,
            },
        )
        .expect("spawn cat");
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(b"echoed")
            .expect("write stdin");

        // Read on threads: if a child end stayed open in the parent, the
        // read would never see EOF, and the test must fail, not hang.
        fn read_all_within(mut pipe: impl Read + Send + 'static, child: &mut Child) -> Vec<u8> {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = tx.send(pipe.read_to_end(&mut bytes).map(|_| bytes));
            });
            let read = rx.recv_timeout(Duration::from_secs(10));
            if read.is_err() {
                let _ = child.kill();
                let _ = child.wait();
            }
            read.expect("the pipe never reached EOF: a child end stayed open in the parent")
                .expect("read the pipe")
        }
        let stdout_pipe = child.stdout.take().expect("stdout pipe");
        let stdout = read_all_within(stdout_pipe, &mut child);
        let stderr_pipe = child.stderr.take().expect("stderr pipe");
        let stderr = read_all_within(stderr_pipe, &mut child);
        assert!(child.wait().expect("wait").success());
        assert_eq!(stdout, b"echoed");
        assert_eq!(stderr, b"done");

        // The reset stdio is `/dev/null`: `cat` reads EOF at once and exits.
        let mut again = command.start().expect("respawn");
        assert!(again.stdin.is_none() && again.stdout.is_none() && again.stderr.is_none());
        let (done_tx, done_rx) = mpsc::channel();
        let pid = again.id();
        thread::spawn(move || {
            let _ = done_tx.send(again.wait());
        });
        let status = done_rx.recv_timeout(Duration::from_secs(10));
        if status.is_err() {
            // SAFETY: the waiter thread has not reaped `pid` yet, so it is
            // still this test's child.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        let status = status.expect("a respawn with reset stdio must not wait on a pipe");
        assert!(status.expect("wait").success());
    }

    /// Issue #1115: a descriptor created inside a [`with_spawns_excluded`]
    /// window and marked close-on-exec before the window closes is never
    /// inherited by a concurrent [`spawn`].
    ///
    /// The window is held open for 300 ms with the descriptor inheritable, the
    /// state macOS leaves a fresh `socket()` or `accept()` descriptor in until
    /// its follow-up `fcntl`. If the spawn ran inside it, `/bin/sleep` would
    /// hold the socket for 30 s and the peer would see no EOF.
    #[cfg(unix)]
    #[test]
    fn spawn_never_inherits_a_descriptor_created_in_an_excluded_window() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier};

        let _std_spawn = std_spawn_test_lock();
        let window_open = Arc::new(Barrier::new(2));
        let window_closed = Arc::new(AtomicBool::new(false));
        let creator = {
            let window_open = Arc::clone(&window_open);
            let window_closed = Arc::clone(&window_closed);
            thread::spawn(move || {
                // Create the pair inside the window too: `socketpair` is no
                // more atomically close-on-exec than `socket`, and another
                // test's spawn must not copy it either.
                with_spawns_excluded(|| {
                    let (held, peer) = UnixStream::pair().expect("socketpair");
                    // Set before `held` can close: XNU refuses `SO_RCVTIMEO`
                    // on a socket whose peer is already gone (issue #824).
                    peer.set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("read timeout");
                    set_cloexec(held.as_raw_fd(), false);
                    window_open.wait();
                    thread::sleep(Duration::from_millis(300));
                    set_cloexec(held.as_raw_fd(), true);
                    window_closed.store(true, Ordering::SeqCst);
                    (held, peer)
                })
            })
        };

        window_open.wait();
        let mut child = spawn(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        )
        .expect("spawn /bin/sleep");
        let spawned_after_window = window_closed.load(Ordering::SeqCst);
        let (held, mut peer) = creator.join().expect("creator thread");
        drop(held);

        let mut byte = [0_u8; 1];
        let read = peer.read(&mut byte);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            matches!(read, Ok(0)),
            "the peer must see EOF once the creator drops its end, but the \
             spawned child still holds a copy: {read:?}"
        );
        assert!(
            spawned_after_window,
            "spawn must wait until the excluded window closes"
        );
    }

    #[test]
    fn bounded_reader_rejects_overflow() {
        let read = read_bounded(std::io::Cursor::new(b"abcdef".to_vec()), 3);
        assert!(matches!(read, Err(ReaderFailure::LimitExceeded { cap: 3 })));
    }

    #[test]
    fn completes_under_deadline_and_captures_stdout() {
        let out = run_with_timeout(stdout_command(), Duration::from_secs(5), 1 << 20)
            .expect("fast command should complete");
        assert!(out.status.success());
        // printf has no trailing newline on Unix; cmd `echo` would add CRLF, so
        // assert on a prefix to stay platform-tolerant.
        assert!(
            out.stdout.starts_with(b"hello"),
            "stdout was {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn sleeping_child_is_killed_at_the_deadline() {
        // A 30 s sleeper under a 150 ms deadline must return ~immediately with
        // Timeout, not block for 30 s.
        let start = Instant::now();
        let res = run_with_timeout(sleep_command(), Duration::from_millis(150), 1 << 20);
        assert!(
            matches!(res, Err(ProcError::Timeout)),
            "expected Timeout, got {res:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not wait for the child to finish on its own"
        );
    }

    /// stdout cap: a 1 MB producer under a 4 KiB cap fails as soon as the reader
    /// sees byte 4097. The process tree is terminated instead of draining an
    /// unbounded stream after its retained output is already unusable.
    #[cfg(unix)]
    #[test]
    fn stdout_cap_fails_without_oom_or_hang() {
        let start = Instant::now();
        let result = run_with_timeout(
            sh("head -c 1000000 /dev/zero"),
            Duration::from_secs(30),
            4096,
        );
        assert!(matches!(
            result,
            Err(ProcError::OutputLimitExceeded {
                stream: OutputStream::Stdout,
                cap: 4096
            })
        ));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "overflow must terminate the run promptly"
        );
    }

    #[test]
    fn stderr_cap_rejects_overflow() {
        let read = read_bounded(
            std::io::Cursor::new(vec![b'x'; 128 * 1024]),
            STDERR_CAP as usize,
        );
        assert!(matches!(
            read,
            Err(ReaderFailure::LimitExceeded { cap: STDERR_CAP })
        ));
    }

    #[test]
    fn bounded_reader_preserves_read_errors() {
        struct FailingReader;

        impl Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("forced read failure"))
            }
        }

        let read = read_bounded(FailingReader, 16);
        assert!(matches!(read, Err(ReaderFailure::Read(_))));
    }

    #[cfg(unix)]
    #[test]
    fn descendant_pipe_holder_is_bounded_by_deadline() {
        let start = Instant::now();
        let res = run_with_timeout(
            sh("(sleep 30) & printf parent-exited"),
            Duration::from_millis(200),
            1 << 20,
        );
        assert!(
            matches!(res, Err(ProcError::Timeout)),
            "expected Timeout, got {res:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not wait for a descendant that inherited stdout"
        );
    }

    #[test]
    fn nonzero_exit_status_is_reported_not_an_error() {
        let out = run_with_timeout(sh("exit 3"), Duration::from_secs(5), 1 << 20)
            .expect("a clean nonzero exit is an Output, not a ProcError");
        assert!(!out.status.success());
    }

    #[test]
    fn spawn_detached_reports_spawn_failure() {
        let err = spawn_detached(&mut std::process::Command::new(
            "paneflow-no-such-binary-4f2a",
        ))
        .expect_err("a missing binary must surface as a spawn error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn detached_reaper_failures_emit_warnings() {
        let mut diagnostic = Vec::new();
        let reaper = start_detached_reaper_with(
            |_| Err(io::Error::other("test thread spawn failure")),
            &mut diagnostic,
        );
        assert!(reaper.is_none());

        let (sender, receiver) = mpsc::channel::<()>();
        drop(receiver);
        send_to_detached_reaper(Some(&sender), (), &mut diagnostic);
        send_to_detached_reaper(None, (), &mut diagnostic);

        let diagnostic = String::from_utf8(diagnostic).expect("diagnostic must be UTF-8");
        assert!(diagnostic.contains("test thread spawn failure"));
        assert!(diagnostic.contains("could not be sent to reaper"));
        assert!(diagnostic.contains("reaper is unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn stdin_is_forwarded_and_closed() {
        let out =
            run_with_timeout_stdin(sh("cat"), b"hello batch", Duration::from_secs(5), 1 << 20)
                .expect("cat should complete after stdin EOF");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello batch");
    }

    #[cfg(unix)]
    #[test]
    fn zero_cap_rejects_any_output() {
        let result = run_with_timeout(sh("printf x"), Duration::from_secs(5), 0);
        assert!(matches!(
            result,
            Err(ProcError::OutputLimitExceeded {
                stream: OutputStream::Stdout,
                cap: 0
            })
        ));
    }

    /// `STDERR_CAP` stays 64 KiB for the nine existing callers; a chatty
    /// `setup` needs a larger stderr budget without changing that default.
    #[cfg(unix)]
    #[test]
    fn stderr_cap_is_overridable_per_call() {
        let script = r#"head -c 100000 /dev/zero | tr '\0' x >&2"#;
        let result = run_with_timeout(sh(script), Duration::from_secs(5), 4096);
        assert!(
            matches!(
                result,
                Err(ProcError::OutputLimitExceeded {
                    stream: OutputStream::Stderr,
                    ..
                })
            ),
            "default stderr cap must still fail ~100 KiB of stderr, got {result:?}"
        );
        let out = run_with_timeout_capped(sh(script), Duration::from_secs(5), 4096, 1024 * 1024)
            .expect("a 1 MiB stderr cap must accept ~100 KiB of stderr");
        assert!(out.status.success());
    }

    /// Count the caller's own children currently in state `Z`.
    ///
    /// macOS has no `/proc`, so the state comes from `ps -A -o ppid=,stat=`:
    /// two blank-headed columns, `ppid` then the state string whose first
    /// character is the process state (`Z` for a zombie).
    #[cfg(target_os = "macos")]
    fn zombie_child_count() -> usize {
        let me = std::process::id().to_string();
        // Through the spawn exclusion like every other child: a plain
        // `output()` could copy the socket another test holds inheritable.
        let mut ps = std::process::Command::new("ps");
        ps.args(["-A", "-o", "ppid=,stat="]);
        let out = run_with_timeout(ps, Duration::from_secs(10), 4 << 20)
            .expect("ps must run to count zombie children");
        assert!(
            out.status.success(),
            "ps -A -o ppid=,stat= failed with status {:?}",
            out.status
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| {
                let mut fields = line.split_whitespace();
                let ppid = fields.next();
                let state = fields.next();
                ppid == Some(me.as_str()) && state.is_some_and(|s| s.starts_with('Z'))
            })
            .count()
    }

    /// The regression this helper exists for: a child that exits immediately
    /// must not stay `<defunct>` once its handle goes out of scope.
    ///
    /// Asserting "zero zombie children" rather than a delta keeps the test
    /// immune to the transient zombies other tests in this module produce
    /// between a child's exit and its `try_wait`.
    #[cfg(target_os = "macos")]
    #[test]
    fn spawn_detached_reaps_short_lived_children() {
        for _ in 0..4 {
            spawn_detached(&mut std::process::Command::new("true"))
                .expect("`true` must be spawnable");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let zombies = zombie_child_count();
            if zombies == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "spawn_detached left {zombies} zombie children unreaped"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// A child that is killed and reaped however the test ends.
    struct KillOnDrop(Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn sleeper() -> Command {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::Null)
            .stdout(Stdio::Null)
            .stderr(Stdio::Null);
        command
    }

    fn descriptors_of_child(child: &Child) -> Vec<(i32, u32)> {
        posix::descriptors_of(child.id() as libc::pid_t).expect("list the child's descriptors")
    }

    /// Issue #1126: a [`Command`] child holds none of a pipe this process
    /// left inheritable, and a reader of that pipe sees EOF as soon as the
    /// parent closes its write end.
    ///
    /// Both ends stay not close-on-exec for the whole spawn, the state
    /// macOS leaves a fresh `pipe()` in until its follow-up `fcntl` (std's
    /// exec-status pipe and `Stdio::piped()` included). A std `posix_spawn`
    /// child copies both ends and keeps the write end for its 30 s life.
    #[test]
    fn spawned_child_holds_no_descriptor_the_parent_left_inheritable() {
        use std::os::fd::AsRawFd;

        let _std_spawn = std_spawn_test_lock();
        let (mut reader, writer) = io::pipe().expect("pipe");
        set_cloexec(reader.as_raw_fd(), false);
        set_cloexec(writer.as_raw_fd(), false);

        let child = KillOnDrop(sleeper().start().expect("spawn /bin/sleep"));
        let held = descriptors_of_child(&child.0);

        drop(writer);
        let (read_tx, read_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut byte = [0_u8; 1];
            let _ = read_tx.send(reader.read(&mut byte).map_err(|error| error.kind()));
        });
        let read = read_rx.recv_timeout(Duration::from_secs(2));

        // Its stdio is `/dev/null`, so any pipe it holds is this test's. Only
        // pipes count: a vnode `dyld` holds while `/bin/sleep` starts is not
        // a leak.
        let pipes: Vec<i32> = held
            .iter()
            .filter(|(_, kind)| *kind == libc::PROX_FDTYPE_PIPE as u32)
            .map(|(fd, _)| *fd)
            .collect();
        assert!(
            pipes.is_empty(),
            "the child holds pipe descriptors it was never given: {pipes:?} (all: {held:?})"
        );
        assert_eq!(
            read,
            Ok(Ok(0)),
            "the pipe's reader must see EOF once the parent closes the write end"
        );
    }

    /// `inheritable_descriptors` lists a descriptor left inheritable on
    /// purpose and skips one marked close-on-exec, so the shim's agent keeps
    /// the first and never gets the second.
    #[test]
    fn inheritable_descriptors_lists_only_descriptors_left_inheritable() {
        use std::os::fd::AsRawFd;

        let _std_spawn = std_spawn_test_lock();
        let (kept, closed) = io::pipe().expect("pipe");
        set_cloexec(kept.as_raw_fd(), false);
        let listed = with_spawns_excluded(inheritable_descriptors).expect("list descriptors");
        assert!(listed.contains(&kept.as_raw_fd()), "{listed:?}");
        assert!(!listed.contains(&closed.as_raw_fd()), "{listed:?}");
        assert!(listed.iter().all(|fd| *fd > 2), "{listed:?}");
    }

    /// The three stdio slots and every descriptor named with `pass_fd` or
    /// `inherit_fd` reach the child at the number asked for.
    #[test]
    fn passed_and_inherited_descriptors_reach_the_child() {
        use std::os::fd::AsRawFd;

        let (mut passed_reader, passed_writer) = io::pipe().expect("pipe");
        let (mut inherited_reader, inherited_writer) = io::pipe().expect("pipe");
        let inherited = inherited_writer.as_raw_fd();
        let target = if inherited == 5 { 6 } else { 5 };
        let (mut stdout_reader, stdout_writer) = io::pipe().expect("pipe");

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!(
                "printf passed >&{target}; printf kept >&{inherited}; printf out"
            ))
            .stdin(Stdio::Null)
            .stdout(stdout_writer)
            .stderr(Stdio::Null)
            .pass_fd(passed_writer.into(), target)
            .inherit_fd(inherited);
        let mut child = KillOnDrop(command.start().expect("spawn sh"));
        drop(command);
        drop(inherited_writer);
        let status = child.0.wait().expect("wait");

        let read = |reader: &mut io::PipeReader| {
            let mut text = String::new();
            reader.read_to_string(&mut text).expect("read");
            text
        };
        assert!(status.success(), "{status:?}");
        assert_eq!(read(&mut passed_reader), "passed");
        assert_eq!(read(&mut inherited_reader), "kept");
        assert_eq!(read(&mut stdout_reader), "out");
    }

    /// The `pre_exec` work PaneFlow used to do in the forked child is a
    /// spawn attribute now: a new session, a new process group.
    #[test]
    fn session_and_process_group_attributes_apply() {
        let mut session = sleeper();
        session.new_session();
        let session = KillOnDrop(session.start().expect("spawn session leader"));
        let mut group = sleeper();
        group.process_group(0);
        let group = KillOnDrop(group.start().expect("spawn group leader"));

        let session_pid = session.0.id() as libc::pid_t;
        let group_pid = group.0.id() as libc::pid_t;
        // SAFETY: `getsid`/`getpgid` only read the ids of a live child.
        let (sid, pgid) = unsafe { (libc::getsid(session_pid), libc::getpgid(group_pid)) };
        // SAFETY: as above.
        let own_sid = unsafe { libc::getsid(0) };
        assert_eq!(
            sid, session_pid,
            "new_session must make the child a session leader"
        );
        assert_eq!(
            pgid, group_pid,
            "process_group(0) must make the child a group leader"
        );
        // SAFETY: as above.
        let group_sid = unsafe { libc::getsid(group_pid) };
        assert_eq!(
            group_sid, own_sid,
            "a new group stays in the parent's session"
        );
    }

    /// The child starts with an empty signal mask, `SIGPIPE` at its default
    /// as std leaves it, and every `default_signal` reset. A shell started
    /// with a signal ignored cannot catch it, so `kill -SIG $$` only kills
    /// it when the disposition was reset.
    #[test]
    fn child_signal_mask_is_empty_and_default_signals_are_reset() {
        use std::os::unix::process::ExitStatusExt;

        let suicide = |signal: &str| {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(format!("kill -{signal} $$; exit 0"))
                .stdin(Stdio::Null)
                .stdout(Stdio::Null)
                .stderr(Stdio::Null);
            command
        };

        // std ignores SIGPIPE in this process.
        let status = suicide("PIPE")
            .start()
            .expect("spawn")
            .wait()
            .expect("wait");
        assert_eq!(status.signal(), Some(libc::SIGPIPE), "{status:?}");

        // A signal blocked on the spawning thread is unblocked in the child.
        let blocked = thread::spawn(move || {
            // SAFETY: changes only this short-lived thread's own mask.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGUSR1);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            }
            suicide("USR1")
                .start()
                .expect("spawn")
                .wait()
                .expect("wait")
        })
        .join()
        .expect("spawning thread");
        assert_eq!(blocked.signal(), Some(libc::SIGUSR1), "{blocked:?}");

        // A disposition the parent ignores survives exec unless reset.
        // SAFETY: SIGUSR2 is unused by the test harness; restored below.
        let previous = unsafe { libc::signal(libc::SIGUSR2, libc::SIG_IGN) };
        let ignored = suicide("USR2").start().and_then(|mut child| child.wait());
        let reset = suicide("USR2")
            .default_signal(libc::SIGUSR2)
            .start()
            .and_then(|mut child| child.wait());
        // SAFETY: restores the disposition saved above.
        unsafe { libc::signal(libc::SIGUSR2, previous) };
        let ignored = ignored.expect("spawn with SIGUSR2 ignored");
        let reset = reset.expect("spawn with SIGUSR2 reset");
        assert!(
            ignored.success(),
            "an ignored signal stays ignored: {ignored:?}"
        );
        assert_eq!(reset.signal(), Some(libc::SIGUSR2), "{reset:?}");
    }

    /// Program lookup matches std: a bare name searches the child's `PATH`,
    /// a relative path resolves against the child's working directory, and
    /// a missing program is `NotFound`.
    #[test]
    fn program_lookup_matches_std() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("paneflow-process-lookup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let stub = dir.join("paneflow-lookup-stub");
        std::fs::write(&stub, "#!/bin/sh\nexit 7\n").expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let run = |command: &mut Command| {
            command
                .stdin(Stdio::Null)
                .stdout(Stdio::Null)
                .stderr(Stdio::Null)
                .start()
                .and_then(|mut child| child.wait())
        };

        let on_path = run(Command::new("paneflow-lookup-stub").env("PATH", &dir));
        let relative = run(Command::new("./paneflow-lookup-stub").current_dir(&dir));
        let system = run(&mut Command::new("true"));
        let missing = run(&mut Command::new("paneflow-no-such-binary-4f2a"));
        let bad_cwd = run(Command::new("/usr/bin/true").current_dir(dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(on_path.expect("PATH lookup").code(), Some(7));
        assert_eq!(relative.expect("relative to cwd").code(), Some(7));
        assert!(system.expect("parent PATH lookup").success());
        assert_eq!(
            missing.expect_err("missing program").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            bad_cwd.expect_err("missing working directory").kind(),
            io::ErrorKind::NotFound
        );
    }

    /// The environment is the parent's with the command's changes applied.
    #[test]
    fn environment_changes_apply() {
        let (mut reader, writer) = io::pipe().expect("pipe");
        let home = std::env::var("HOME").unwrap_or_default();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf '%s|%s|%s' \"$PANEFLOW_SET\" \"${HOME-unset}\" \"${PANEFLOW_GONE-unset}\"")
            .env("PANEFLOW_SET", "one")
            .env("PANEFLOW_GONE", "two")
            .env_remove("PANEFLOW_GONE")
            .stdin(Stdio::Null)
            .stdout(writer)
            .stderr(Stdio::Null);
        let mut child = command.start().expect("spawn sh");
        drop(command);
        assert!(child.wait().expect("wait").success());
        let mut text = String::new();
        reader.read_to_string(&mut text).expect("read");
        assert_eq!(text, format!("one|{home}|unset"));
    }

    /// A script without a `#!` line runs through `/bin/sh` with its
    /// arguments, as std's `execvp` fallback ran it; plain `posix_spawn`
    /// fails it with `ENOEXEC`.
    #[test]
    fn script_without_a_shebang_runs_through_sh() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("paneflow-process-noshebang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("no-shebang");
        std::fs::write(&script, "printf '%s|%s|%s' \"$0\" \"$1\" \"$2\"\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let (mut reader, writer) = io::pipe().expect("pipe");
        let mut command = Command::new(&script);
        command
            .args(["first arg", "second"])
            .stdin(Stdio::Null)
            .stdout(writer)
            .stderr(Stdio::Null);
        let started = command.start().and_then(|mut child| child.wait());
        drop(command);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(started.expect("a shebang-less script must start").success());
        let mut text = String::new();
        reader.read_to_string(&mut text).expect("read");
        // `$0` is the resolved script path, as `sh <path> args...` sets it.
        assert_eq!(text, format!("{}|first arg|second", script.display()));
    }

    /// A descriptor plan whose file actions would clobber each other is
    /// refused before anything is spawned.
    ///
    /// A source below 3 has to be an `OwnedFd` at a stdio number, so those
    /// cases borrow this process's stdin (fd 0) and forget the command
    /// before any assertion, so it is never closed.
    #[test]
    fn descriptor_plans_that_clobber_each_other_are_refused() {
        use std::os::fd::{AsRawFd, FromRawFd};

        let refused_with_stdin_source = |configure: fn(&mut Command, OwnedFd)| {
            let mut command = sleeper();
            // SAFETY: fd 0 is open for the whole test run; `forget` below
            // keeps this `OwnedFd` from ever closing it.
            configure(&mut command, unsafe { OwnedFd::from_raw_fd(0) });
            let started = command.start();
            std::mem::forget(command);
            started
                .map(|mut child| {
                    let _ = child.kill();
                    let _ = child.wait();
                })
                .expect_err("plan must be refused")
                .kind()
        };
        // A `pass_fd` source below 3.
        assert_eq!(
            refused_with_stdin_source(|command, stdin| {
                command.pass_fd(stdin, 9);
            }),
            io::ErrorKind::InvalidInput
        );
        // A stdio source that is a lower slot: stdin is set up first.
        assert_eq!(
            refused_with_stdin_source(|command, stdin| {
                command.stderr(stdin);
            }),
            io::ErrorKind::InvalidInput
        );

        let fd = || -> OwnedFd { io::pipe().expect("pipe").1.into() };
        let refused = |command: &mut Command| {
            command
                .start()
                .map(|mut child| {
                    let _ = child.kill();
                    let _ = child.wait();
                })
                .expect_err("plan must be refused")
                .kind()
        };

        assert_eq!(
            refused(sleeper().pass_fd(fd(), 2)),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            refused(sleeper().inherit_fd(1)),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            refused(sleeper().pass_fd(fd(), 7).pass_fd(fd(), 7)),
            io::ErrorKind::InvalidInput
        );
        // A pass target that is also inherited.
        assert_eq!(
            refused(sleeper().inherit_fd(8).pass_fd(fd(), 8)),
            io::ErrorKind::InvalidInput
        );
        // The second source is the first target.
        let second = fd();
        let target = second.as_raw_fd();
        assert_eq!(
            refused(sleeper().pass_fd(fd(), target).pass_fd(second, 9)),
            io::ErrorKind::InvalidInput
        );
    }

    /// Inheriting a descriptor that is not open fails the spawn with
    /// `EBADF`, which the shim's agent spawn retries without inheriting.
    #[test]
    fn inheriting_a_closed_descriptor_fails_with_ebadf() {
        // Far above any descriptor this test process opens.
        const CLOSED: i32 = 4000;
        let error = sleeper()
            .inherit_fd(CLOSED)
            .start()
            .map(|mut child| {
                let _ = child.kill();
                let _ = child.wait();
            })
            .expect_err("a closed descriptor cannot be inherited");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF), "{error:?}");
    }
}
