use super::*;
use std::time::Duration;

#[test]
fn synthesized_hook_wait_returns_when_child_hangs() {
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("30");
    let start = std::time::Instant::now();
    run_synthesized_hook_with_deadline(cmd, Duration::from_millis(150));
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(50),
        "must actually wait for the deadline, not fail to spawn; elapsed {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "hook wait must not block for the child lifetime; elapsed {elapsed:?}"
    );
}

#[test]
fn hook_stdout_cap_is_non_zero() {
    // A cap of 0 fails the bounded run on a single stray byte. Drive a
    // one-byte child through the same helper the synthesized hooks use so
    // this is a behavioral check, not a constant assert.
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("printf x; exit 0");
    let out = paneflow_process::run_with_timeout(cmd, HOOK_NOTIFY_TIMEOUT, HOOK_STDOUT_CAP)
        .expect("a zero stdout cap fails the bounded run on a single stray byte");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"x");
}

/// A hook child that writes to stdout must still be waited on successfully.
/// With `stdout_cap = 0` this returns `Err(OutputLimitExceeded)` and SIGKILLs
/// the child's process group before the IPC notify completes.
#[test]
fn synthesized_hook_tolerates_child_stdout() {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("printf 'hook chatter'; exit 0");
    let out = paneflow_process::run_with_timeout(cmd, HOOK_NOTIFY_TIMEOUT, HOOK_STDOUT_CAP)
        .expect("a hook that writes to stdout must not fail the bounded run");
    assert!(out.status.success());
}
