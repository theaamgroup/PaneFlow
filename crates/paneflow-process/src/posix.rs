//! `posix_spawn` with Apple's `POSIX_SPAWN_CLOEXEC_DEFAULT` for every non-PTY
//! child PaneFlow starts (issue #1126).
//!
//! `std::process::Command` on macOS hands a child every descriptor that is
//! not close-on-exec at the moment it spawns. macOS has no `pipe2`, no
//! `SOCK_CLOEXEC` and no `accept4`, so every pipe or socket another thread is
//! creating is inheritable for a moment, std's own exec-status pipe included.
//! A child that copies one keeps it for its whole life, and whoever waits for
//! EOF on it waits for that unrelated child to exit.
//!
//! [`Command`] closes that class from the inheriting side: the child gets
//! fds 0-2 as configured, the descriptors named with [`Command::pass_fd`] and
//! [`Command::inherit_fd`], and nothing else, whatever other threads are
//! doing. It never forks, so it never runs a `pre_exec` closure: the session,
//! process group, signal mask, signal defaults and working directory are
//! `posix_spawn` attributes and file actions instead.

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus};

/// `POSIX_SPAWN_SETSID` from `<sys/spawn.h>`. The `libc` crate declares it
/// for Linux only; the SDK defines it for macOS 10.15 and later.
const POSIX_SPAWN_SETSID: libc::c_int = 0x0400;

/// The search path `execvp` uses when `PATH` is unset (`_PATH_DEFPATH`).
const DEFAULT_SEARCH_PATH: &str = "/usr/bin:/bin";

unsafe extern "C" {
    /// `<spawn.h>`: keep `filedes` open in the child at its own number,
    /// which `POSIX_SPAWN_CLOEXEC_DEFAULT` would otherwise close.
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        filedes: libc::c_int,
    ) -> libc::c_int;
    /// `<spawn.h>` (macOS 10.15 and later): `chdir(path)` in the child before
    /// it executes the program.
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

/// Where one of the child's standard streams comes from.
#[derive(Debug, Default)]
pub enum Stdio {
    /// The parent's own descriptor at the same number. The default, as for
    /// `std::process::Command::spawn`.
    #[default]
    Inherit,
    /// `/dev/null`, opened in the child.
    Null,
    /// This descriptor. The command keeps it open until it is replaced or
    /// dropped, so a reader sees EOF only once both are gone.
    Fd(OwnedFd),
}

impl From<OwnedFd> for Stdio {
    fn from(fd: OwnedFd) -> Self {
        Self::Fd(fd)
    }
}

impl From<io::PipeReader> for Stdio {
    fn from(pipe: io::PipeReader) -> Self {
        Self::Fd(pipe.into())
    }
}

impl From<io::PipeWriter> for Stdio {
    fn from(pipe: io::PipeWriter) -> Self {
        Self::Fd(pipe.into())
    }
}

/// A child process to start with [`Command::start`].
///
/// The builder mirrors the parts of `std::process::Command` PaneFlow uses.
/// It has no `pre_exec`: whatever ran there is an attribute here.
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    /// Changes to the inherited environment; `None` removes the name.
    env: BTreeMap<OsString, Option<OsString>>,
    cwd: Option<PathBuf>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
    process_group: Option<libc::pid_t>,
    new_session: bool,
    default_signals: Vec<libc::c_int>,
    passed_fds: Vec<(OwnedFd, RawFd)>,
    inherited_fds: Vec<RawFd>,
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl Command {
    /// A command that runs `program` with no arguments, the parent's
    /// environment and working directory, and inherited stdio.
    ///
    /// A `program` without a `/` is looked up in the child's `PATH` (the
    /// parent's, unless [`Command::env`] sets one), as std does.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            stdin: Stdio::Inherit,
            stdout: Stdio::Inherit,
            stderr: Stdio::Inherit,
            process_group: None,
            new_session: false,
            default_signals: Vec::new(),
            passed_fds: Vec::new(),
            inherited_fds: Vec::new(),
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(
            key.as_ref().to_os_string(),
            Some(value.as_ref().to_os_string()),
        );
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in vars {
            self.env(key, value);
        }
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(key.as_ref().to_os_string(), None);
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn stdin(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.stdin = stdio.into();
        self
    }

    pub fn stdout(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.stdout = stdio.into();
        self
    }

    pub fn stderr(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.stderr = stdio.into();
        self
    }

    /// Put the child in process group `pgid`; `0` makes it the leader of a
    /// new group whose id is its pid, like `CommandExt::process_group`.
    pub fn process_group(&mut self, pgid: i32) -> &mut Self {
        self.process_group = Some(pgid);
        self
    }

    /// Make the child the leader of a new session (`setsid`), so it has no
    /// controlling terminal. Overrides [`Command::process_group`].
    pub fn new_session(&mut self) -> &mut Self {
        self.new_session = true;
        self
    }

    /// Reset `signal` to its default disposition in the child, for a signal
    /// the parent ignores. `SIGPIPE` is always reset, as std does. The child's
    /// signal mask always starts empty, also as std does.
    pub fn default_signal(&mut self, signal: i32) -> &mut Self {
        self.default_signals.push(signal);
        self
    }

    /// Give the child `fd` at descriptor number `child_fd` (3 or above).
    /// The command owns `fd` and closes it when dropped; keep `fd`
    /// close-on-exec, so no other child ever inherits it.
    pub fn pass_fd(&mut self, fd: OwnedFd, child_fd: RawFd) -> &mut Self {
        self.passed_fds.push((fd, child_fd));
        self
    }

    /// Keep the parent's descriptor `fd` (3 or above) open in the child at
    /// the same number, close-on-exec or not.
    pub fn inherit_fd(&mut self, fd: RawFd) -> &mut Self {
        self.inherited_fds.push(fd);
        self
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Start the child with `posix_spawn` and `POSIX_SPAWN_CLOEXEC_DEFAULT`.
    ///
    /// The child holds fds 0-2 as configured plus the descriptors named with
    /// [`Command::pass_fd`] and [`Command::inherit_fd`], and no other: a
    /// pipe or socket another thread has not marked close-on-exec yet stays
    /// in the parent. It never forks, so it never waits on an exec-status
    /// pipe another child could hold. The call itself still blocks while the
    /// kernel loads the program, which includes a first-exec Gatekeeper scan
    /// of a new binary (seconds, not microseconds).
    ///
    /// A file without a `#!` line (`ENOEXEC`) is run through `/bin/sh`, as
    /// std's `execvp` fallback does.
    ///
    /// Like `std::process::Child`, the returned handle does not reap the
    /// child when dropped: wait for it, or hand it to a thread that does.
    pub fn start(&mut self) -> io::Result<Child> {
        let image = Image::prepare(self)?;
        let mut attributes = Attributes::new()?;
        attributes.configure(self)?;
        let mut actions = FileActions::new()?;
        actions.configure(self, image.cwd.as_ref())?;
        let pid = crate::with_spawn_shared_side(|| match image.spawn(&actions, &attributes) {
            Err(error) if error.raw_os_error() == Some(libc::ENOEXEC) => {
                image.through_shell().spawn(&actions, &attributes)
            }
            spawned => spawned,
        })?;
        Ok(Child::new(pid))
    }

    /// Forget every [`Command::inherit_fd`], for a retry after one of them
    /// was closed before the spawn.
    pub fn clear_inherited_fds(&mut self) -> &mut Self {
        self.inherited_fds.clear();
        self
    }
}

/// Copies what std's `Command` exposes: program, arguments, environment
/// changes and working directory. Stdio starts inherited. A `pre_exec`
/// closure, `env_clear`, a process group or ids set on `command` are not
/// carried over.
impl From<&std::process::Command> for Command {
    fn from(command: &std::process::Command) -> Self {
        let mut converted = Self::new(command.get_program());
        converted.args(command.get_args());
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => converted.env(key, value),
                None => converted.env_remove(key),
            };
        }
        if let Some(dir) = command.get_current_dir() {
            converted.current_dir(dir);
        }
        converted
    }
}

/// A child started by [`Command::start`]: `std::process::Child` without the
/// parts PaneFlow does not use, since std cannot wrap a pid it did not spawn.
#[derive(Debug)]
pub struct Child {
    pid: libc::pid_t,
    status: Option<ExitStatus>,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Child {
    fn new(pid: libc::pid_t) -> Self {
        Self {
            pid,
            status: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    /// Reap the child if it has exited, without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        self.wait_pid(libc::WNOHANG)
    }

    /// Close `stdin`, then block until the child exits and reap it.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        if let Some(status) = self.status {
            return Ok(status);
        }
        match self.wait_pid(0)? {
            Some(status) => Ok(status),
            None => Err(io::Error::other("waitpid returned without a status")),
        }
    }

    /// `SIGKILL` the child. Once it has been reaped this does nothing, so it
    /// never signals a reused pid.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        // SAFETY: `kill` takes two scalars. `pid` is this handle's unreaped
        // child, so it cannot name another process yet.
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn wait_pid(&mut self, options: libc::c_int) -> io::Result<Option<ExitStatus>> {
        let mut raw = 0;
        loop {
            // SAFETY: `raw` is a valid out-pointer for the status word, and
            // `pid` names this handle's own child.
            let reaped = unsafe { libc::waitpid(self.pid, &mut raw, options) };
            if reaped == self.pid {
                let status = ExitStatus::from_raw(raw);
                self.status = Some(status);
                return Ok(Some(status));
            }
            if reaped == 0 {
                return Ok(None);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Map a `posix_spawn*` return value, which is an errno value rather than
/// `-1` with `errno` set, to a result.
fn check(code: libc::c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

fn c_string(bytes: Vec<u8>) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a program, argument, path or environment entry contains a NUL byte",
        )
    })
}

/// Everything `posix_spawn` reads, built before the call so a bad argument
/// fails before any spawn state exists.
struct Image {
    path: CString,
    cwd: Option<CString>,
    // Own the strings the pointer arrays below point into.
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    argv_ptrs: Vec<*mut libc::c_char>,
    envp_ptrs: Vec<*mut libc::c_char>,
}

impl Image {
    fn prepare(command: &Command) -> io::Result<Self> {
        let path = c_string(resolve_program(command)?.into_os_string().into_vec())?;
        let cwd = match &command.cwd {
            Some(dir) => Some(c_string(dir.as_os_str().as_bytes().to_vec())?),
            None => None,
        };
        let argv = std::iter::once(&command.program)
            .chain(&command.args)
            .map(|arg| c_string(arg.as_bytes().to_vec()))
            .collect::<io::Result<Vec<_>>>()?;
        let envp = child_environment(command)?;
        Ok(Self::new(path, cwd, argv, envp))
    }

    fn new(path: CString, cwd: Option<CString>, argv: Vec<CString>, envp: Vec<CString>) -> Self {
        let pointers = |strings: &[CString]| {
            strings
                .iter()
                .map(|string| string.as_ptr().cast_mut())
                .chain(std::iter::once(std::ptr::null_mut()))
                .collect::<Vec<_>>()
        };
        Self {
            path,
            cwd,
            argv_ptrs: pointers(&argv),
            envp_ptrs: pointers(&envp),
            _argv: argv,
            _envp: envp,
        }
    }

    /// `execvp`'s `ENOEXEC` fallback: `/bin/sh <path> <args...>`.
    fn through_shell(&self) -> Self {
        let argv = [c"sh".to_owned(), self.path.clone()]
            .into_iter()
            .chain(self._argv.iter().skip(1).cloned())
            .collect();
        Self::new(
            c"/bin/sh".to_owned(),
            self.cwd.clone(),
            argv,
            self._envp.clone(),
        )
    }

    fn spawn(&self, actions: &FileActions, attributes: &Attributes) -> io::Result<libc::pid_t> {
        let mut pid = 0;
        // SAFETY: every pointer is valid for the call: `path` and the
        // NULL-terminated `argv`/`envp` arrays point into strings `self`
        // owns, and both spawn objects were initialized and stay alive until
        // their owners drop after this returns.
        let code = unsafe {
            libc::posix_spawn(
                &mut pid,
                self.path.as_ptr(),
                &actions.0,
                &attributes.0,
                self.argv_ptrs.as_ptr(),
                self.envp_ptrs.as_ptr(),
            )
        };
        check(code)?;
        Ok(pid)
    }
}

/// The parent's environment with `command`'s changes applied, as
/// `NAME=value` strings.
fn child_environment(command: &Command) -> io::Result<Vec<CString>> {
    let mut vars: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    for (key, value) in &command.env {
        match value {
            Some(value) => vars.insert(key.clone(), value.clone()),
            None => vars.remove(key),
        };
    }
    vars.into_iter()
        .map(|(key, value)| {
            let mut entry = key.into_vec();
            entry.push(b'=');
            entry.extend(value.into_vec());
            c_string(entry)
        })
        .collect()
}

/// The absolute path `posix_spawn` should execute, resolved the way std's
/// spawn does: a program with a `/` is taken relative to the child's working
/// directory; a bare name is searched for in the child's `PATH`.
fn resolve_program(command: &Command) -> io::Result<PathBuf> {
    let program = Path::new(&command.program);
    if command.program.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    if command.program.as_bytes().contains(&b'/') {
        return absolute_in_child(command, program);
    }
    let search = match command.env.get(OsStr::new("PATH")) {
        Some(Some(path)) => path.clone(),
        Some(None) => OsString::from(DEFAULT_SEARCH_PATH),
        None => std::env::var_os("PATH").unwrap_or_else(|| DEFAULT_SEARCH_PATH.into()),
    };
    for dir in std::env::split_paths(&search) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let Ok(candidate) = absolute_in_child(command, &dir.join(program)) else {
            continue;
        };
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io::Error::from_raw_os_error(libc::ENOENT))
}

/// `path` as the child sees it, made absolute against the child's working
/// directory, so the result does not depend on when `posix_spawn` applies
/// the `chdir`.
fn absolute_in_child(command: &Command, path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let base = match &command.cwd {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => std::env::current_dir()?.join(dir),
        None => std::env::current_dir()?,
    };
    Ok(base.join(path))
}

fn is_executable_file(path: &Path) -> bool {
    if !path.metadata().is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

/// An initialized `posix_spawnattr_t`, destroyed on drop, so every error
/// path releases it.
struct Attributes(libc::posix_spawnattr_t);

impl Attributes {
    fn new() -> io::Result<Self> {
        let mut attributes = std::mem::MaybeUninit::uninit();
        // SAFETY: `init` writes a fresh object into the out-pointer.
        check(unsafe { libc::posix_spawnattr_init(attributes.as_mut_ptr()) })?;
        // SAFETY: `init` succeeded, so the object is initialized; `Drop`
        // destroys it exactly once.
        Ok(Self(unsafe { attributes.assume_init() }))
    }

    fn configure(&mut self, command: &Command) -> io::Result<()> {
        let mut flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
            | libc::POSIX_SPAWN_SETSIGMASK
            | libc::POSIX_SPAWN_SETSIGDEF;
        if command.new_session {
            flags |= POSIX_SPAWN_SETSID;
        } else if let Some(pgid) = command.process_group {
            flags |= libc::POSIX_SPAWN_SETPGROUP;
            // SAFETY: `self.0` is an initialized attribute object.
            check(unsafe { libc::posix_spawnattr_setpgroup(&mut self.0, pgid) })?;
        }

        // std's posix_spawn path starts every child with an empty signal
        // mask and `SIGPIPE` at its default (std itself ignores it).
        // SAFETY: `sigemptyset`/`sigaddset` write into a local set, and the
        // attribute setters copy it into the initialized object.
        unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            check(libc::posix_spawnattr_setsigmask(&mut self.0, &mask))?;

            let mut defaults: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut defaults);
            libc::sigaddset(&mut defaults, libc::SIGPIPE);
            for &signal in &command.default_signals {
                if libc::sigaddset(&mut defaults, signal) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            check(libc::posix_spawnattr_setsigdefault(&mut self.0, &defaults))?;
        }

        let flags = libc::c_short::try_from(flags)
            .map_err(|_| io::Error::other("posix_spawn flags do not fit a short"))?;
        // SAFETY: `self.0` is an initialized attribute object.
        check(unsafe { libc::posix_spawnattr_setflags(&mut self.0, flags) })
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialized in `new` and destroyed only here.
        unsafe {
            libc::posix_spawnattr_destroy(&mut self.0);
        }
    }
}

/// An initialized `posix_spawn_file_actions_t`, destroyed on drop, so every
/// error path releases it. It only records actions: no descriptor is opened
/// in the parent.
struct FileActions(libc::posix_spawn_file_actions_t);

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut actions = std::mem::MaybeUninit::uninit();
        // SAFETY: `init` writes a fresh object into the out-pointer.
        check(unsafe { libc::posix_spawn_file_actions_init(actions.as_mut_ptr()) })?;
        // SAFETY: `init` succeeded, so the object is initialized; `Drop`
        // destroys it exactly once.
        Ok(Self(unsafe { actions.assume_init() }))
    }

    /// Stdio first, then the passed descriptors, then the inherited ones, then
    /// the working directory. [`check_descriptor_plan`] refuses the plans in
    /// which, with that order, one action would replace another's source
    /// before it is copied.
    fn configure(&mut self, command: &Command, cwd: Option<&CString>) -> io::Result<()> {
        check_descriptor_plan(command)?;
        for (target, stdio) in [&command.stdin, &command.stdout, &command.stderr]
            .into_iter()
            .enumerate()
        {
            let target = target as RawFd;
            match stdio {
                // A stdio slot the parent has closed stays closed.
                Stdio::Inherit if !is_open(target) => {}
                Stdio::Inherit => self.inherit(target)?,
                Stdio::Null => self.open_null(target)?,
                Stdio::Fd(fd) => self.dup2(fd.as_raw_fd(), target)?,
            }
        }
        for (fd, target) in &command.passed_fds {
            self.dup2(fd.as_raw_fd(), *target)?;
        }
        for &fd in &command.inherited_fds {
            self.inherit(fd)?;
        }
        if let Some(cwd) = cwd {
            // SAFETY: `self.0` is initialized and `cwd` is NUL-terminated;
            // the action copies the path.
            check(unsafe { posix_spawn_file_actions_addchdir_np(&mut self.0, cwd.as_ptr()) })?;
        }
        Ok(())
    }

    fn inherit(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: `self.0` is initialized; the action records a number.
        check(unsafe { posix_spawn_file_actions_addinherit_np(&mut self.0, fd) })
    }

    /// `dup2(source, target)` in the child. A descriptor already at its
    /// target is inherited instead: `dup2` onto itself would keep its
    /// close-on-exec flag.
    fn dup2(&mut self, source: RawFd, target: RawFd) -> io::Result<()> {
        if source == target {
            return self.inherit(target);
        }
        // SAFETY: `self.0` is initialized; the action records two numbers.
        check(unsafe { libc::posix_spawn_file_actions_adddup2(&mut self.0, source, target) })
    }

    fn open_null(&mut self, target: RawFd) -> io::Result<()> {
        // SAFETY: `self.0` is initialized and the path is NUL-terminated;
        // the action copies it.
        check(unsafe {
            libc::posix_spawn_file_actions_addopen(
                &mut self.0,
                target,
                c"/dev/null".as_ptr(),
                libc::O_RDWR,
                0,
            )
        })
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: initialized in `new` and destroyed only here.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut self.0);
        }
    }
}

/// Refuse a descriptor plan whose file actions would clobber each other:
/// a stdio `Fd` source that is a lower stdio slot, a `pass_fd` source or
/// target in 0-2 (the stdio actions run first and may have replaced it), a
/// target given twice or also inherited, a source that is another
/// `pass_fd`'s target, or an `inherit_fd` below 3.
fn check_descriptor_plan(command: &Command) -> io::Result<()> {
    let invalid = |reason: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            reason.to_owned(),
        ))
    };
    if command.inherited_fds.iter().any(|&fd| fd < 3) {
        return invalid("inherit_fd names a stdio slot");
    }
    // Slots are set in order 0, 1, 2: a source below its own slot has
    // already been replaced by then.
    for (slot, stdio) in [&command.stdin, &command.stdout, &command.stderr]
        .into_iter()
        .enumerate()
    {
        let lower_slot = match stdio {
            Stdio::Fd(fd) => usize::try_from(fd.as_raw_fd()).is_ok_and(|source| source < slot),
            Stdio::Inherit | Stdio::Null => false,
        };
        if lower_slot {
            return invalid("a stdio source is a lower stdio slot, already replaced");
        }
    }
    let targets: Vec<RawFd> = command
        .passed_fds
        .iter()
        .map(|(_, target)| *target)
        .collect();
    for (index, (fd, target)) in command.passed_fds.iter().enumerate() {
        let source = fd.as_raw_fd();
        if source < 3 || *target < 3 {
            return invalid("pass_fd uses a stdio slot");
        }
        if targets[..index].contains(target) || command.inherited_fds.contains(target) {
            return invalid("pass_fd targets a number already given to the child");
        }
        let other_target = targets
            .iter()
            .enumerate()
            .any(|(other, target)| other != index && *target == source);
        if other_target {
            return invalid("pass_fd source is another pass_fd's target");
        }
    }
    Ok(())
}

fn is_open(fd: RawFd) -> bool {
    // SAFETY: `F_GETFD` only reads the flags of `fd`, open or not.
    unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
}

/// This process's descriptors numbered 3 or above that a child would
/// inherit by default: the ones not marked close-on-exec.
///
/// A [`Command`] child inherits none of them unless told to with
/// [`Command::inherit_fd`]. Call this inside
/// [`with_spawns_excluded`](crate::with_spawns_excluded): no pipe or socket
/// the exclusion protects is half-created then, so every descriptor listed
/// was deliberately left inheritable, such as one the process itself
/// inherited from its parent.
pub fn inheritable_descriptors() -> io::Result<Vec<RawFd>> {
    let own = libc::pid_t::try_from(std::process::id())
        .map_err(|_| io::Error::other("pid does not fit pid_t"))?;
    Ok(descriptors_of(own)?
        .into_iter()
        .map(|(fd, _)| fd)
        .filter(|&fd| {
            // SAFETY: `F_GETFD` only reads the flags of `fd`.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            fd >= 3 && flags >= 0 && flags & libc::FD_CLOEXEC == 0
        })
        .collect())
}

/// `pid`'s open descriptors as `(fd, PROX_FDTYPE_*)`, the way `lsof` reads
/// them.
pub(crate) fn descriptors_of(pid: libc::pid_t) -> io::Result<Vec<(RawFd, u32)>> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks only for the size of the descriptor table.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let needed = usize::try_from(needed)
        .ok()
        .filter(|&needed| needed > 0)
        .ok_or_else(io::Error::last_os_error)?;
    // Headroom for descriptors opened between the two calls.
    let capacity = needed / entry + 64;
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
    let bytes = libc::c_int::try_from(capacity * entry)
        .map_err(|_| io::Error::other("descriptor table too large"))?;
    // SAFETY: the buffer holds `capacity` entries and the kernel writes at
    // most `bytes` bytes into it.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    let written = usize::try_from(written)
        .ok()
        .filter(|&written| written > 0)
        .ok_or_else(io::Error::last_os_error)?;
    // SAFETY: the kernel initialized `written` bytes of whole entries, at
    // most `capacity` of them.
    unsafe { fds.set_len((written / entry).min(capacity)) };
    Ok(fds
        .into_iter()
        .map(|fd| (fd.proc_fd, fd.proc_fdtype))
        .collect())
}
