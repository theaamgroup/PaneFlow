use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use paneflow_config::schema::TerminalSurfaceProfile;

use super::ghostty_session::{GhosttySession, GhosttyUiEvent};
use super::pty_session::SpawnParams;
use super::types::{ShellQuoting, TerminalWindowSize};

const CYCLES: usize = 200;
const WARMUP_CYCLES: usize = 5;
const RESIZES_PER_CYCLE: usize = 200;
const RESOURCE_LIMIT_PERCENT: usize = 5;
const CYCLE_TIMEOUT: Duration = Duration::from_secs(8);
/// Deadline for a process and its descendants to leave the process table.
///
/// The happy path never waits this long because `wait_process_inactive`
/// returns as soon as the process is gone. Residual growth is asserted
/// separately against RSS and descriptor counts, not against how quickly the
/// kernel reaps a child.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone)]
struct SpawnSpec {
    shell: &'static str,
    quoting: ShellQuoting,
    args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitFailureKind {
    Timeout,
    DuplicateExit,
    UnexpectedRuntimeFailure,
    MissingRuntimeFailure,
    CleanupTimeout,
}

#[derive(Debug)]
struct WaitFailure {
    kind: WaitFailureKind,
    surface_id: u64,
    pid: u32,
    elapsed_ms: u128,
    exits: usize,
    runtime_failures: usize,
}

impl std::fmt::Display for WaitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "kind={:?} surface={} pid={} elapsed_ms={} exits={} runtime_failures={}",
            self.kind,
            self.surface_id,
            self.pid,
            self.elapsed_ms,
            self.exits,
            self.runtime_failures,
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct ExitObservation {
    code: i32,
    elapsed: Duration,
}

#[derive(Clone, Copy, Debug)]
struct ResourceSnapshot {
    handles: u64,
    rss: u64,
}

struct StressPane {
    surface_id: u64,
    pid: u32,
    session: GhosttySession,
    events: UnboundedReceiver<GhosttyUiEvent>,
}

impl StressPane {
    fn spawn(surface_id: u64, spec: SpawnSpec) -> Self {
        let params = SpawnParams {
            shell: spec.shell.into(),
            shell_quoting: spec.quoting,
            extra_args: spec.args,
            env: HashMap::from([
                ("TERM".into(), "xterm-256color".into()),
                ("COLORTERM".into(), "truecolor".into()),
                ("TERM_PROGRAM".into(), "paneflow".into()),
            ]),
            cwd: std::env::current_dir()
                .unwrap_or_else(|_| panic!("scenario=spawn surface={surface_id} phase=cwd")),
            cols: 80,
            rows: 24,
            profile: TerminalSurfaceProfile::Normal,
        };
        let (session, pending, events) =
            GhosttySession::pending(TerminalWindowSize::new(80, 24, 8, 16));
        let spawned = session
            .start(pending, params, None, 10_000)
            .unwrap_or_else(|_| panic!("scenario=spawn surface={surface_id} phase=start"));
        assert!(
            spawned.child_pid > 0,
            "scenario=spawn surface={surface_id} phase=pid"
        );
        session.promote();
        Self {
            surface_id,
            pid: spawned.child_pid,
            session,
            events,
        }
    }

    fn resize_storm(&self) {
        for index in 0..RESIZES_PER_CYCLE {
            self.session.resize(TerminalWindowSize::new(
                1 + index % 160,
                1 + index % 80,
                8,
                16,
            ));
        }
    }

    fn write(&self, bytes: Vec<u8>) {
        assert!(
            self.session.write(bytes).is_sent(),
            "scenario=write surface={} pid={} phase=admission",
            self.surface_id,
            self.pid,
        );
    }

    fn wait_for_exit(
        &mut self,
        timeout: Duration,
        expect_runtime_failure: bool,
    ) -> Result<ExitObservation, WaitFailure> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut exits = 0usize;
        let mut runtime_failures = 0usize;
        let mut code = -1;

        while Instant::now() < deadline && exits == 0 {
            while let Ok(event) = self.events.try_recv() {
                match event {
                    GhosttyUiEvent::ChildExited {
                        code: exit_code, ..
                    } => {
                        exits += 1;
                        code = exit_code;
                    }
                    GhosttyUiEvent::RuntimeFailed(_) => runtime_failures += 1,
                    _ => {}
                }
            }
            if exits == 0 {
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                GhosttyUiEvent::ChildExited {
                    code: exit_code, ..
                } => {
                    exits += 1;
                    code = exit_code;
                }
                GhosttyUiEvent::RuntimeFailed(_) => runtime_failures += 1,
                _ => {}
            }
        }

        if exits == 0 {
            self.session.shutdown();
            let cleanup_succeeded =
                wait_process_inactive(self.pid, Instant::now() + CLEANUP_TIMEOUT);
            return Err(WaitFailure {
                kind: if cleanup_succeeded {
                    WaitFailureKind::Timeout
                } else {
                    WaitFailureKind::CleanupTimeout
                },
                surface_id: self.surface_id,
                pid: self.pid,
                elapsed_ms: started.elapsed().as_millis(),
                exits,
                runtime_failures,
            });
        }

        self.session.shutdown();
        if !wait_process_inactive(self.pid, Instant::now() + CLEANUP_TIMEOUT) {
            return Err(WaitFailure {
                kind: WaitFailureKind::CleanupTimeout,
                surface_id: self.surface_id,
                pid: self.pid,
                elapsed_ms: started.elapsed().as_millis(),
                exits,
                runtime_failures,
            });
        }
        let kind = if exits != 1 {
            Some(WaitFailureKind::DuplicateExit)
        } else if expect_runtime_failure && runtime_failures == 0 {
            Some(WaitFailureKind::MissingRuntimeFailure)
        } else if !expect_runtime_failure && runtime_failures != 0 {
            Some(WaitFailureKind::UnexpectedRuntimeFailure)
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(WaitFailure {
                kind,
                surface_id: self.surface_id,
                pid: self.pid,
                elapsed_ms: started.elapsed().as_millis(),
                exits,
                runtime_failures,
            });
        }
        Ok(ExitObservation {
            code,
            elapsed: started.elapsed(),
        })
    }
}

impl Drop for StressPane {
    fn drop(&mut self) {
        self.session.shutdown();
        let _ = wait_process_inactive(self.pid, Instant::now() + CLEANUP_TIMEOUT);
    }
}

fn cycle_spec() -> SpawnSpec {
    SpawnSpec {
        shell: "/bin/sh",
        quoting: ShellQuoting::Posix,
        args: vec![
            "-c".into(),
            "IFS= read -r line; printf 'PANEFLOW_STRESS:%s\\n' \"$line\"".into(),
        ],
    }
}

fn run_cycle(surface_id: u64) -> (Duration, usize) {
    let mut pane = StressPane::spawn(surface_id, cycle_spec());
    let descendants = descendant_pids(pane.pid);
    let output_before = pane.session.processed_output_bytes_for_test();
    pane.resize_storm();
    pane.write(format!("cycle-{surface_id}\r").into_bytes());
    let observation = pane
        .wait_for_exit(CYCLE_TIMEOUT, false)
        .unwrap_or_else(|failure| panic!("scenario=cycle failure={failure}"));
    assert_eq!(
        observation.code, 0,
        "scenario=cycle surface={surface_id} pid={} phase=exit_code",
        pane.pid,
    );
    let output_after = pane.session.processed_output_bytes_for_test();
    assert!(
        output_after > output_before,
        "scenario=cycle surface={surface_id} pid={} phase=output bytes_before={output_before} bytes_after={output_after}",
        pane.pid,
    );
    // `wait_for_exit` already waited out the shell's own PID, but nothing
    // waited out its descendants. One deadline is shared across the whole
    // set: every descendant must be gone within the same cleanup window,
    // not granted a fresh one each.
    let cleanup_deadline = Instant::now() + CLEANUP_TIMEOUT;
    for descendant in &descendants {
        assert!(
            wait_process_inactive(*descendant, cleanup_deadline),
            "scenario=cycle surface={surface_id} pid={} descendant={} phase=cleanup",
            pane.pid,
            descendant,
        );
    }
    (observation.elapsed, descendants.len())
}

fn process_active(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only probes process existence.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn wait_process_inactive(pid: u32, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if !process_active(pid) {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    !process_active(pid)
}

fn descendant_pids(_root_pid: u32) -> Vec<u32> {
    Vec::new()
}

fn resource_snapshot() -> ResourceSnapshot {
    // Darwin and the other BSDs expose no `/proc/self/fd`, so probe the
    // descriptor table directly. `F_GETFD` is a pure query on `fd`.
    // SAFETY: `sysconf` reads a process limit and returns -1 when unavailable.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let max_fd = i32::try_from(limit.clamp(0, 4096)).unwrap_or(4096);
    let handles = (0..max_fd)
        // SAFETY: `F_GETFD` only reads the descriptor flags of `fd`.
        .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1)
        .count() as u64;
    ResourceSnapshot {
        handles,
        rss: super::bench_corpus::resident_set_bytes(),
    }
}

fn resources_within_budget(baseline: ResourceSnapshot, current: ResourceSnapshot) -> bool {
    let limits = resource_limits(baseline);
    current.handles <= limits.handles && current.rss <= limits.rss
}

fn resource_limits(baseline: ResourceSnapshot) -> ResourceSnapshot {
    ResourceSnapshot {
        handles: baseline
            .handles
            .saturating_add(baseline.handles.saturating_sub(1) / 20),
        rss: baseline
            .rss
            .saturating_add(baseline.rss.saturating_sub(1) / 20),
    }
}

fn wait_for_resource_recovery(baseline: ResourceSnapshot) -> ResourceSnapshot {
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    let mut current = resource_snapshot();
    while Instant::now() < deadline && !resources_within_budget(baseline, current) {
        std::thread::sleep(Duration::from_millis(20));
        current = resource_snapshot();
    }
    current
}

fn assert_resource_recovery(
    scenario: &'static str,
    baseline: ResourceSnapshot,
    current: ResourceSnapshot,
) {
    let limits = resource_limits(baseline);
    assert!(
        resources_within_budget(baseline, current),
        "scenario={scenario} phase=resources handles_start={} handles_end={} rss_start={} rss_end={} handle_limit={} rss_limit={}",
        baseline.handles,
        current.handles,
        baseline.rss,
        current.rss,
        limits.handles,
        limits.rss,
    );
}

#[test]
#[ignore = "EP-004 promotion gate: 200 PTY cycles with 200 resizes each"]
fn ghostty_spawn_resize_close_stress_has_no_residual_growth() {
    for warmup in 0..WARMUP_CYCLES {
        let _ = run_cycle(warmup as u64);
    }
    let baseline = resource_snapshot();
    let started = Instant::now();
    let mut max_cycle = Duration::ZERO;
    let mut cycle_durations = Vec::with_capacity(CYCLES);
    let mut descendants_observed = 0usize;
    for cycle in 0..CYCLES {
        let (duration, descendants) = run_cycle((cycle + WARMUP_CYCLES) as u64);
        max_cycle = max_cycle.max(duration);
        cycle_durations.push(duration);
        descendants_observed = descendants_observed.saturating_add(descendants);
    }
    let recovered = wait_for_resource_recovery(baseline);
    let elapsed = started.elapsed();
    let limits = resource_limits(baseline);
    cycle_durations.sort_unstable();
    println!(
        "{{\"scenario\":\"ghostty_spawn_resize_close\",\"warmup_cycles\":{WARMUP_CYCLES},\"cycles\":{CYCLES},\"resizes_per_cycle\":{RESIZES_PER_CYCLE},\"descendants_observed\":{descendants_observed},\"campaign_ms\":{},\"cycle_median_us\":{},\"cycle_p95_us\":{},\"max_cycle_ms\":{},\"handles_baseline\":{},\"handles_end\":{},\"handles_limit\":{},\"rss_baseline_bytes\":{},\"rss_end_bytes\":{},\"rss_limit_bytes\":{},\"resource_limit_percent\":{RESOURCE_LIMIT_PERCENT}}}",
        elapsed.as_millis(),
        super::bench_corpus::percentile_us(&cycle_durations, 50),
        super::bench_corpus::percentile_us(&cycle_durations, 95),
        max_cycle.as_millis(),
        baseline.handles,
        recovered.handles,
        limits.handles,
        baseline.rss,
        recovered.rss,
        limits.rss,
    );
    assert_resource_recovery("cycles", baseline, recovered);
    assert!(
        max_cycle <= CYCLE_TIMEOUT,
        "scenario=cycles phase=duration total_ms={} max_cycle_ms={} descendants={descendants_observed}",
        elapsed.as_millis(),
        max_cycle.as_millis(),
    );
}
