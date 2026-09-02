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
            ": > '{markers}/'\"$$\"\nwhile [ ! -e '{release}' ]; do sleep 0.1; done",
            markers = markers.display(),
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
