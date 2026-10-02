//! Issue #262: `send_interrupt_stop` (the mid-turn Ctrl+C `Stop` hook) had no
//! test. These drive it against a fake hook binary and assert the argv / env /
//! stdin contract, the `MAX_INFLIGHT_REAPERS` ceiling, and that a failed spawn
//! gives its reaper slot back.
//!
//! `INFLIGHT_REAPERS` is a process-global counter, so every test here holds
//! `SERIAL` for its whole body and drains the counter back to zero before
//! releasing it; otherwise a neighbour's still-running reaper skews the count.

use super::*;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

const POLL: Duration = Duration::from_millis(10);
const WAIT: Duration = Duration::from_secs(60);

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn inflight() -> usize {
    INFLIGHT_REAPERS.load(Ordering::Acquire)
}

/// Poll until `cond` holds or `WAIT` elapses; returns whether it held.
fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < WAIT {
        if cond() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    cond()
}

fn write_hook(dir: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let hook = dir.join("paneflow-ai-hook");
    std::fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = std::fs::metadata(&hook).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hook, perms).unwrap();
    hook
}

#[test]
fn interrupt_stop_hook_gets_stop_argv_interrupt_env_and_empty_payload() {
    let _guard = serial();
    assert_eq!(inflight(), 0, "counter must start drained");

    let td = tempfile::TempDir::new().unwrap();
    let out = td.path().join("observed");
    let hook = write_hook(
        td.path(),
        &format!(
            "printf 'argc=%s argv=%s\\nsource=%s\\ntool=%s\\npid=%s\\nstdin=' \
             \"$#\" \"$*\" \"$PANEFLOW_AI_EVENT_SOURCE\" \"$PANEFLOW_AI_TOOL\" \
             \"$PANEFLOW_AI_PID\" > '{tmp}'\ncat >> '{tmp}'\nmv '{tmp}' '{out}'",
            tmp = out.with_extension("tmp").display(),
            out = out.display(),
        ),
    );

    send_interrupt_stop(&hook, "claude");

    assert!(wait_until(|| out.exists()), "fake hook never ran");
    let observed = std::fs::read_to_string(&out).unwrap();
    let expected = format!(
        "argc=1 argv=Stop\nsource={}\ntool=claude\npid={}\nstdin={{}}",
        PANEFLOW_AI_EVENT_SOURCE_INTERRUPT,
        std::process::id()
    );
    assert_eq!(observed, expected);
    assert_eq!(PANEFLOW_AI_EVENT_SOURCE_ENV, "PANEFLOW_AI_EVENT_SOURCE");

    assert!(
        wait_until(|| inflight() == 0),
        "reaper must release its slot once the hook exits; inflight={}",
        inflight()
    );
}

#[test]
fn interrupt_stop_drops_stops_past_the_inflight_reaper_ceiling() {
    let _guard = serial();
    assert_eq!(inflight(), 0, "counter must start drained");

    let td = tempfile::TempDir::new().unwrap();
    let markers = td.path().join("markers");
    std::fs::create_dir(&markers).unwrap();
    let release = td.path().join("release");
    // Each hook records that it started, then wedges until released.
    let hook = write_hook(
        td.path(),
        &format!(
            // Also stop once the temp dir is gone, so a red run whose
            // cleanup removed `release` before the hook saw it still ends.
            ": > '{markers}/'\"$$\"\n\
             while [ -d '{dir}' ] && [ ! -e '{release}' ]; do sleep 0.1; done",
            markers = markers.display(),
            dir = td.path().display(),
            release = release.display(),
        ),
    );
    let started = || std::fs::read_dir(&markers).unwrap().count();
    // Release the wedged hooks even if an assertion below fails, so a red
    // run does not leave looping `sh` children behind.
    struct ReleaseOnDrop(std::path::PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"");
        }
    }
    let _release_guard = ReleaseOnDrop(release.clone());

    for _ in 0..=MAX_INFLIGHT_REAPERS {
        send_interrupt_stop(&hook, "claude");
    }

    assert_eq!(
        inflight(),
        MAX_INFLIGHT_REAPERS,
        "the stop past the ceiling must be dropped, not counted"
    );
    assert!(
        wait_until(|| started() == MAX_INFLIGHT_REAPERS),
        "expected exactly {MAX_INFLIGHT_REAPERS} hooks to start, saw {}",
        started()
    );
    // Give a leaked ninth hook time to show up before concluding it was dropped.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        started(),
        MAX_INFLIGHT_REAPERS,
        "a stop past the ceiling must not spawn a hook"
    );

    std::fs::write(&release, b"").unwrap();
    assert!(
        wait_until(|| inflight() == 0),
        "every reaper must release its slot once its hook exits; inflight={}",
        inflight()
    );

    // With the ceiling drained a fresh stop must go through again.
    send_interrupt_stop(&hook, "claude");
    assert!(
        wait_until(|| started() == MAX_INFLIGHT_REAPERS + 1),
        "a stop after the burst drains must run the hook"
    );
    assert!(wait_until(|| inflight() == 0));
}

fn set_cloexec(fd: i32, on: bool) {
    let flag = if on { libc::FD_CLOEXEC } else { 0 };
    // SAFETY: `fcntl(F_SETFD)` on a descriptor the pipe window just created.
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, flag) }, 0, "fcntl");
}

/// `dev:inode` of the open file `fd` refers to, as `stat -f '%d:%i'` prints
/// it for its stdin, so the test can match a pipe end across processes.
fn fd_identity(fd: i32) -> String {
    // SAFETY: `fstat` fills a zeroed, caller-owned `stat` buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(fd, &mut st) }, 0, "fstat({fd})");
    format!("{}:{}", st.st_dev, st.st_ino)
}

/// The stub hook names its pipes with `stat -f '%d:%i' <&N`: with no file
/// argument macOS `stat(1)` `fstat`s its stdin. (A path lookup of
/// `/dev/fd/N` would return the devfs node, not the pipe.) Check that the
/// command prints exactly what [`fd_identity`] computes for the same pipe.
fn assert_stat_command_names_pipes_like_fstat(dir: &Path) {
    use std::os::fd::AsRawFd;

    let (reader, _writer) = std::io::pipe().unwrap();
    let expected = fd_identity(reader.as_raw_fd());
    let out = dir.join("stat-control");
    let mut child = paneflow_process::Command::new("/usr/bin/stat")
        .args(["-f", "%d:%i"])
        .stdin(reader)
        .stdout(std::os::fd::OwnedFd::from(
            std::fs::File::create(&out).unwrap(),
        ))
        .start()
        .expect("spawn stat");
    assert!(child.wait().unwrap().success(), "stat -f failed");
    assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), expected);
}

/// Issues #1127, #1136: the interrupt `Stop` hook and the exit hook never
/// inherit each other's pipes.
///
/// Each round holds the exit hook's `run_with_timeout` pipe window open,
/// with every new end inheritable (the state macOS leaves a `pipe()` in
/// until its follow-up `fcntl`), until a Ctrl+C `Stop` has been spawned
/// inside it. A `Stop` child that copied the exit hook's pipe ends would
/// keep them until the test releases it after `run_with_timeout` returns:
/// the stub reports those pipes, and the run times out instead of
/// returning `hello`.
///
/// Pipes are matched by `dev:inode`, not counted, so a pipe the test binary
/// itself inherited (a make jobserver, a pane shell) cannot fail it.
#[test]
fn interrupt_stop_and_exit_hook_never_inherit_each_others_pipes() {
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc, Mutex};

    let _guard = serial();
    assert_eq!(inflight(), 0, "counter must start drained");
    let control_dir = tempfile::TempDir::new().unwrap();
    assert_stat_command_names_pipes_like_fstat(control_dir.path());

    for round in 0..3 {
        let td = tempfile::TempDir::new().unwrap();
        let report = td.path().join("pipes");
        let release = td.path().join("release");
        // Record `fd dev:inode` for every descriptor that is a pipe, read
        // stdin to EOF as `paneflow-ai-hook Stop` does, then stay alive
        // until released (or until the temp dir is gone on a red run).
        let hook = write_hook(
            td.path(),
            &format!(
                "n=0\n: > '{tmp}'\nwhile [ \"$n\" -le 255 ]; do\n  \
                 if [ -p \"/dev/fd/$n\" ]; then\n    \
                 printf '%s %s\\n' \"$n\" \"$(/usr/bin/stat -f '%d:%i' <&\"$n\")\" \
                 >> '{tmp}'\n  fi\n  \
                 n=$((n + 1))\ndone\n\
                 mv '{tmp}' '{report}'\n\
                 cat > /dev/null\n\
                 while [ -d '{dir}' ] && [ ! -e '{release}' ]; do sleep 0.05; done",
                tmp = report.with_extension("tmp").display(),
                report = report.display(),
                dir = td.path().display(),
                release = release.display(),
            ),
        );
        struct ReleaseOnDrop(std::path::PathBuf);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.0, b"");
            }
        }
        let release_guard = ReleaseOnDrop(release.clone());

        let (window_open_tx, window_open_rx) = mpsc::channel();
        let (stop_spawned_tx, stop_spawned_rx) = mpsc::channel::<()>();
        let window_closed = Arc::new(AtomicBool::new(false));
        // `dev:inode` of each pipe end the exit hook's window created. A
        // pipe's inode on macOS is derived from its kernel address, which a
        // new pipe can reuse once the old one is freed, so the round also
        // holds a close-on-exec copy of each read end (never a write end,
        // which would hold off the run's EOF) until the stub has reported.
        let window_pipes = Arc::new(Mutex::new(Vec::new()));
        let held_read_ends = Arc::new(Mutex::new(Vec::<std::os::fd::OwnedFd>::new()));
        let exit_hook = {
            let window_closed = Arc::clone(&window_closed);
            let window_pipes = Arc::clone(&window_pipes);
            let held_read_ends = Arc::clone(&held_read_ends);
            std::thread::spawn(move || {
                paneflow_process::test_support::set_pipe_window_hook(move |fds| {
                    *window_pipes.lock().unwrap() = fds.iter().map(|&fd| fd_identity(fd)).collect();
                    // The hook lists each pipe as (read end, write end).
                    *held_read_ends.lock().unwrap() = fds
                        .iter()
                        .step_by(2)
                        .map(|&fd| {
                            // SAFETY: `F_DUPFD_CLOEXEC` on an open descriptor;
                            // the new one is owned by the returned `OwnedFd`.
                            let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                            assert!(copy >= 0, "dup read end");
                            unsafe { std::os::fd::FromRawFd::from_raw_fd(copy) }
                        })
                        .collect();
                    for &fd in fds {
                        set_cloexec(fd, false);
                    }
                    let _ = window_open_tx.send(());
                    // Open until the Stop spawn has returned; the cap only
                    // keeps a broken run from hanging.
                    let _ = stop_spawned_rx.recv_timeout(Duration::from_secs(5));
                    for &fd in fds {
                        set_cloexec(fd, true);
                    }
                    window_closed.store(true, Ordering::SeqCst);
                });
                let mut cmd = std::process::Command::new("sh");
                cmd.arg("-c").arg("printf hello");
                paneflow_process::run_with_timeout(
                    cmd,
                    crate::HOOK_NOTIFY_TIMEOUT,
                    crate::HOOK_STDOUT_CAP,
                )
            })
        };

        window_open_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("run_with_timeout must create its pipes through spawn_piped");
        send_interrupt_stop(&hook, "claude");
        // Read as soon as the Stop spawn returns: the control that it ran
        // while the exit hook's pipes were inheritable.
        let spawned_inside_window = !window_closed.load(Ordering::SeqCst);
        let _ = stop_spawned_tx.send(());
        let result = exit_hook.join().expect("exit hook thread");

        assert!(
            wait_until(|| report.exists()),
            "round {round}: stub hook never ran"
        );
        // `fd -> dev:inode` for every pipe the stub held.
        let stub_pipes: Vec<(u32, String)> = std::fs::read_to_string(&report)
            .unwrap()
            .lines()
            .map(|line| {
                let (fd, identity) = line.split_once(' ').expect("`fd dev:inode` line");
                (fd.parse().unwrap(), identity.to_string())
            })
            .collect();
        drop(release_guard);
        assert!(
            wait_until(|| inflight() == 0),
            "round {round}: reaper must release its slot once the hook exits"
        );

        let window_pipes: Vec<String> = window_pipes.lock().unwrap().clone();
        // Controls: the Stop spawn ran inside the window, the window created
        // pipes, and the stub sees its own stdin as a pipe.
        assert!(
            spawned_inside_window,
            "round {round}: the Stop spawn must run while the exit hook's pipe window is open"
        );
        assert!(!window_pipes.is_empty(), "round {round}: no window pipes");
        assert!(
            stub_pipes.iter().any(|(fd, _)| *fd == 0),
            "round {round}: the stub's stdin must be a pipe, saw {stub_pipes:?}"
        );

        let leaked: Vec<&(u32, String)> = stub_pipes
            .iter()
            .filter(|(_, identity)| window_pipes.contains(identity))
            .collect();
        assert!(
            leaked.is_empty(),
            "round {round}: the Stop hook holds the exit hook's pipe ends {leaked:?} \
             (run_with_timeout: {result:?})"
        );
        let out = result.unwrap_or_else(|error| {
            panic!(
                "round {round}: printf exited, so the exit hook's run must see EOF, but \
                 something still holds its write end: {error}"
            )
        });
        assert_eq!(out.stdout, b"hello", "round {round}");
    }
}

#[test]
fn interrupt_stop_spawn_failure_releases_its_reaper_slot() {
    let _guard = serial();
    assert_eq!(inflight(), 0, "counter must start drained");

    let td = tempfile::TempDir::new().unwrap();
    let missing = td.path().join("no-such-hook");
    assert!(!missing.exists());

    for _ in 0..=MAX_INFLIGHT_REAPERS {
        send_interrupt_stop(&missing, "claude");
    }

    assert_eq!(
        inflight(),
        0,
        "a failed spawn must give back the slot it reserved"
    );
}
