use crate::exec::spawn_parent_death_guard;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A long-lived stand-in for the agent the guard watches over.
fn spawn_sleeping_child() -> Child {
    Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn `sleep 30` as the stand-in agent")
}

/// A parent PID that is guaranteed to differ from the real one, so the guard
/// sees a "reparent" on its first tick without anyone having to die.
fn bogus_parent_pid() -> u32 {
    // SAFETY: `getppid` is a trivial, argument-free syscall.
    let real = unsafe { libc::getppid() } as u32;
    real.wrapping_add(1)
}

/// Poll `try_wait` for up to `budget`; `None` means the child outlived it.
fn wait_for_exit(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < budget {
        if let Some(status) = child.try_wait().expect("try_wait on the stand-in child") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn parent_death_guard_sigkills_child_on_reparent() {
    let mut child = spawn_sleeping_child();
    let child_reaped = Arc::new(AtomicBool::new(false));

    spawn_parent_death_guard(child.id(), bogus_parent_pid(), Arc::clone(&child_reaped));

    // The guard polls every 500 ms; give it several ticks of slack.
    let status = wait_for_exit(&mut child, Duration::from_secs(5));
    child_reaped.store(true, Ordering::Release);
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("guard left the agent running after a detected reparent");
        }
    };
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "agent must die by SIGKILL on reparent, got {status:?}"
    );
}

#[test]
fn parent_death_guard_does_not_signal_once_child_is_reaped() {
    let mut child = spawn_sleeping_child();
    // `run_real` flips this the instant `child.wait()` returns; from then on
    // the child PID may belong to an unrelated process and must not be
    // probed or signalled, even though the parent check would fire.
    let child_reaped = Arc::new(AtomicBool::new(true));

    spawn_parent_death_guard(child.id(), bogus_parent_pid(), Arc::clone(&child_reaped));

    // Three guard ticks is enough for a faulty guard to have fired.
    let status = wait_for_exit(&mut child, Duration::from_millis(1_600));
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        status.is_none(),
        "guard must not touch an already-reaped child PID, but it exited with {status:?}"
    );
}
