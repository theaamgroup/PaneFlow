//! Startup first-frame benchmark (#519, upstream `df375ba5` part 3).
//!
//! Unlike the terminal and editor suites this one launches the real release
//! binary, because the cost it measures (the GPU window, the platform text
//! system, the state the app builds before its first frame) does not exist in
//! a window-free test. The app cooperates through `startup_trace.rs`: each
//! launch gets `PANEFLOW_STARTUP_TRACE` pointing at a scratch file, records a
//! mark per launch stage, writes the timeline once the frame it was launched
//! to measure has been presented, and quits. This file turns those timelines
//! into `bench_harness` metrics: the total per scenario and one step per
//! mark, medians across launches.
//!
//! Every launch runs against a seeded `PANEFLOW_HOME` under the system temp
//! directory and its own `PANEFLOW_SOCKET_PATH`, so it never reads the
//! developer's session, never binds the installed app's socket, and never
//! writes into the real per-user directories.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::bench_harness::{Direction, Metric, process_cpu_time, publish, refuse_debug_profile};
use crate::startup_trace::OUTPUT_PATH_ENV;

const SUITE: &str = "paneflow-startup-bench";
const EXE_ENV: &str = "PANEFLOW_BENCH_EXE";
const RUNS_ENV: &str = "PANEFLOW_BENCH_STARTUP_RUNS";
const DEFAULT_RUNS: usize = 10;
/// A timed launch has a warm binary and a seeded home; anything slower than
/// this is a hang, not a slow machine.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// The warm-up launch absorbs the first-exec Gatekeeper scan of a freshly
/// linked binary (12-20 s on this machine) and the first-run helper
/// extraction into the scratch cache.
const WARMUP_TIMEOUT: Duration = Duration::from_secs(120);
const RESTORE_WORKSPACES: usize = 3;
/// How long the core-share probe spins before each scenario.
const CORE_SHARE_PROBE: Duration = Duration::from_millis(250);
const STEP_NOTE: &str =
    "wall-clock median across launches of the time between this mark and the previous one";
const TOTAL_NOTE: &str =
    "wall-clock median across launches from the first line of main to the measured frame";

#[derive(Clone, Copy)]
enum Scenario {
    /// An empty `session.json`: the app builds its default workspace (one
    /// shell in the fixture directory) before the first frame.
    Fresh,
    /// Three workspaces with one terminal each, restored one batch per frame
    /// after the first frame (issue #156); the trace ends at the frame after
    /// the last batch.
    Restore,
}

impl Scenario {
    const ALL: [Scenario; 2] = [Scenario::Fresh, Scenario::Restore];

    fn prefix(self) -> &'static str {
        match self {
            Scenario::Fresh => "fresh",
            Scenario::Restore => "restore3",
        }
    }

    fn session(self, cwd: &Path) -> paneflow_config::schema::SessionState {
        let workspaces = match self {
            Scenario::Fresh => Vec::new(),
            Scenario::Restore => (0..RESTORE_WORKSPACES)
                .map(|index| {
                    serde_json::json!({
                        "title": format!("bench-{index}"),
                        "cwd": cwd.to_string_lossy(),
                        "tabs": [{
                            "title": "",
                            "layout": {
                                "type": "pane",
                                "surfaces": [{
                                    "surface_type": "terminal",
                                    "name": "Terminal",
                                    "cwd": cwd.to_string_lossy()
                                }]
                            }
                        }]
                    })
                })
                .collect(),
        };
        let document = serde_json::json!({
            "version": paneflow_config::schema::SESSION_SCHEMA_VERSION,
            "active_workspace": 0,
            "workspaces": workspaces,
        });
        serde_json::from_value(document).expect("fixture session matches the session schema")
    }
}

struct Trace {
    profile: String,
    marks: Vec<(String, u64)>,
}

fn app_binary() -> PathBuf {
    if let Some(path) = std::env::var_os(EXE_ENV) {
        return PathBuf::from(path);
    }
    let test_binary = std::env::current_exe().expect("test binary path");
    let profile_dir = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("test binaries live under <target>/<profile>/deps");
    profile_dir.join("paneflow")
}

/// Seed the scratch home the way the app expects to find it: the session,
/// an empty config, and a fixed window size, all under the config root's
/// `APP_SUBDIR` namespace. The window size pins the GPU surface the frame
/// is measured on so a persisted resize cannot move the numbers.
fn seed_home(home: &Path, scenario: Scenario, cwd: &Path) {
    let dirs = paneflow_config::loader::user_dirs_under(home);
    let session_path = paneflow_config::loader::session_path_in(&dirs);
    let config_dir = session_path
        .parent()
        .expect("session path has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("fixture config dir");
    let session = serde_json::to_string_pretty(&scenario.session(cwd)).expect("session json");
    std::fs::write(&session_path, session).expect("fixture session");
    std::fs::write(paneflow_config::loader::config_path_in(&dirs), "{}\n").expect("fixture config");
    std::fs::write(
        config_dir.join("window-state.json"),
        "{\"width\":1400.0,\"height\":900.0}\n",
    )
    .expect("fixture window state");
}

fn parse_trace(text: &str) -> Trace {
    let document: serde_json::Value = serde_json::from_str(text).expect("trace is JSON");
    let marks = document["marks"]
        .as_array()
        .expect("trace carries a marks array")
        .iter()
        .map(|mark| {
            (
                mark["name"].as_str().expect("mark name").to_owned(),
                mark["at_us"].as_u64().expect("mark offset"),
            )
        })
        .collect();
    Trace {
        profile: document["profile"].as_str().unwrap_or("unknown").to_owned(),
        marks,
    }
}

struct Launch<'a> {
    exe: &'a Path,
    home: &'a Path,
    socket: &'a Path,
    cwd: &'a Path,
}

fn launch_to_measured_frame(launch: &Launch<'_>, trace_path: &Path, timeout: Duration) -> Trace {
    let _ = std::fs::remove_file(trace_path);
    let mut command = Command::new(launch.exe);
    // A pane launched from a running PaneFlow carries the parent's
    // PANEFLOW_* environment (its socket, surface id, bin dir, ...). None of
    // it may leak into the measured process.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PANEFLOW_") {
            command.env_remove(name);
        }
    }
    let mut child = command
        .current_dir(launch.cwd)
        .env(paneflow_config::loader::HOME_ENV, launch.home)
        .env("PANEFLOW_SOCKET_PATH", launch.socket)
        .env(OUTPUT_PATH_ENV, trace_path)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to launch {}: {error}", launch.exe.display()));
    let started = Instant::now();
    loop {
        match child.try_wait().expect("poll the launched app") {
            Some(status) => {
                assert!(status.success(), "paneflow exited with {status}");
                break;
            }
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("paneflow did not present its measured frame within {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let text = std::fs::read_to_string(trace_path)
        .unwrap_or_else(|error| panic!("no trace at {}: {error}", trace_path.display()));
    parse_trace(&text)
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let index = (sorted.len().saturating_sub(1) * percentile) / 100;
    sorted[index.min(sorted.len().saturating_sub(1))]
}

fn metric_from_us(name: &'static str, note: &'static str, samples_us: &mut [u64]) -> Metric {
    samples_us.sort_unstable();
    let mean_us = samples_us.iter().sum::<u64>() as f64 / samples_us.len().max(1) as f64;
    Metric {
        name,
        unit: "ns",
        direction: Direction::LowerIsBetter,
        value: percentile(samples_us, 50) as f64 * 1_000.0,
        p95: Some(percentile(samples_us, 95) as f64 * 1_000.0),
        mean: Some(mean_us * 1_000.0),
        alloc_bytes_per_iter: None,
        allocs_per_iter: None,
        iters: samples_us.len(),
        note,
        available: true,
    }
}

fn metrics_from_traces(prefix: &str, traces: &[Trace]) -> Vec<Metric> {
    let reference = &traces[0];
    for (index, trace) in traces.iter().enumerate() {
        let names = |t: &Trace| {
            t.marks
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(trace),
            names(reference),
            "launch {index} produced a different mark sequence"
        );
    }
    let mut metrics = Vec::with_capacity(reference.marks.len() + 1);
    let mut totals: Vec<u64> = traces
        .iter()
        .map(|trace| trace.marks.last().map(|(_, at)| *at).unwrap_or(0))
        .collect();
    let total_name: &'static str = String::leak(format!("{prefix}_first_frame_total"));
    metrics.push(metric_from_us(total_name, TOTAL_NOTE, &mut totals));
    for (index, (name, _)) in reference.marks.iter().enumerate() {
        let mut steps: Vec<u64> = traces
            .iter()
            .map(|trace| {
                let at = trace.marks[index].1;
                let previous = if index == 0 {
                    0
                } else {
                    trace.marks[index - 1].1
                };
                at.saturating_sub(previous)
            })
            .collect();
        let metric_name: &'static str = String::leak(format!("{prefix}_step_{name}"));
        metrics.push(metric_from_us(metric_name, STEP_NOTE, &mut steps));
    }
    metrics
}

/// The share of a core this process gets while spinning one thread for
/// [`CORE_SHARE_PROBE`]. The timed work of this suite happens in a child
/// process, so the harness cannot attribute a CPU share to it; this probe
/// measures the same thing the other suites record (whether another workload
/// is competing for the core) at the moment the launches are about to run.
fn core_share_probe() -> f64 {
    let cpu_before = process_cpu_time();
    let started = Instant::now();
    let mut spin = 0u64;
    while started.elapsed() < CORE_SHARE_PROBE {
        spin = spin.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        std::hint::black_box(spin);
    }
    (process_cpu_time() - cpu_before).as_secs_f64()
        / started.elapsed().as_secs_f64().max(f64::EPSILON)
}

#[test]
#[ignore = "startup benchmark: run through scripts/bench-startup.sh"]
fn startup_first_frame_benchmark() {
    refuse_debug_profile();
    let exe = app_binary();
    assert!(
        exe.is_file(),
        "no app binary at {}; build it first or set {EXE_ENV}",
        exe.display()
    );
    let runs = std::env::var(RUNS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|runs| *runs > 0)
        .unwrap_or(DEFAULT_RUNS);
    // Short on purpose: the socket path below has to fit `sun_path`
    // (104 bytes on macOS) with `$TMPDIR` already ~50 bytes long.
    let fixture = std::env::temp_dir().join(format!("pf-sb-{}", std::process::id()));
    let cwd = fixture.join("project");
    std::fs::create_dir_all(&cwd).expect("fixture project dir");
    let socket = fixture.join("ipc.sock");
    println!("PANEFLOW_BENCH_NOTE app binary: {}", exe.display());
    println!("PANEFLOW_BENCH_NOTE fixture: {}", fixture.display());

    let mut metrics = Vec::new();
    let mut cpu_share = f64::INFINITY;
    for scenario in Scenario::ALL {
        let home = fixture.join(format!("home-{}", scenario.prefix()));
        seed_home(&home, scenario, &cwd);
        let launch = Launch {
            exe: &exe,
            home: &home,
            socket: &socket,
            cwd: &cwd,
        };
        let warmup =
            launch_to_measured_frame(&launch, &fixture.join("warmup.json"), WARMUP_TIMEOUT);
        assert!(
            warmup.profile == "release" || std::env::var_os("PANEFLOW_BENCH_ALLOW_DEBUG").is_some(),
            "the app binary is a {} build; benchmark a release build (or set PANEFLOW_BENCH_ALLOW_DEBUG=1)",
            warmup.profile
        );
        let test_profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        assert_eq!(
            warmup.profile, test_profile,
            "the app binary and this test binary must share a profile: the fixture was seeded under the {test_profile} namespace"
        );
        let share = core_share_probe();
        println!(
            "PANEFLOW_BENCH_NOTE core share before the {} launches: {share:.2}",
            scenario.prefix()
        );
        cpu_share = cpu_share.min(share);
        let traces: Vec<Trace> = (0..runs)
            .map(|run| {
                launch_to_measured_frame(
                    &launch,
                    &fixture.join(format!("run-{run}.json")),
                    RUN_TIMEOUT,
                )
            })
            .collect();
        metrics.extend(metrics_from_traces(scenario.prefix(), &traces));
    }
    let _ = std::fs::remove_dir_all(&fixture);

    println!(
        "PANEFLOW_BENCH_NOTE {runs} timed launches per scenario after one warm-up each; cpu share is the lowest core-share probe taken before a scenario, not the child's own"
    );
    if cpu_share < 0.9 {
        println!(
            "PANEFLOW_BENCH_WARNING the process only got {:.0}% of a core while timing: another workload was competing, treat the timings as inflated",
            cpu_share * 100.0
        );
    }
    publish(SUITE, 0, &metrics, cpu_share);
}

#[cfg(test)]
mod tests {
    use super::{Scenario, Trace, metrics_from_traces, parse_trace, seed_home};

    fn trace(at_us: &[(&str, u64)]) -> Trace {
        Trace {
            profile: "release".to_owned(),
            marks: at_us
                .iter()
                .map(|(name, at)| ((*name).to_owned(), *at))
                .collect(),
        }
    }

    #[test]
    fn steps_are_medians_of_consecutive_mark_differences() {
        let traces = [
            trace(&[("a", 100), ("b", 400)]),
            trace(&[("a", 120), ("b", 220)]),
            trace(&[("a", 110), ("b", 310)]),
        ];
        let metrics = metrics_from_traces("fresh", &traces);
        assert_eq!(metrics[0].name, "fresh_first_frame_total");
        assert_eq!(metrics[0].value, 310_000.0);
        assert_eq!(metrics[1].name, "fresh_step_a");
        assert_eq!(metrics[1].value, 110_000.0);
        assert_eq!(metrics[2].name, "fresh_step_b");
        assert_eq!(metrics[2].value, 200_000.0);
        assert_eq!(metrics[2].iters, 3);
    }

    #[test]
    fn parse_reads_the_probe_document() {
        let text = r#"{"schema":1,"profile":"release","total_us":9,"marks":[{"name":"x","at_us":4,"step_us":4},{"name":"y","at_us":9,"step_us":5}]}"#;
        let parsed = parse_trace(text);
        assert_eq!(parsed.profile, "release");
        assert_eq!(parsed.marks, vec![("x".to_owned(), 4), ("y".to_owned(), 9)]);
    }

    #[test]
    #[should_panic(expected = "different mark sequence")]
    fn diverging_mark_sequences_are_rejected() {
        let traces = [trace(&[("a", 1), ("b", 2)]), trace(&[("a", 1), ("c", 2)])];
        metrics_from_traces("fresh", &traces);
    }

    #[test]
    fn fixture_sessions_match_the_schema() {
        let cwd = std::path::Path::new("bench-project");
        let fresh = Scenario::Fresh.session(cwd);
        assert!(fresh.workspaces.is_empty());
        let restore = Scenario::Restore.session(cwd);
        assert_eq!(restore.workspaces.len(), super::RESTORE_WORKSPACES);
        assert!(restore.workspaces.iter().all(|ws| ws.tabs.len() == 1));
        assert!(restore.workspaces.iter().all(|ws| {
            ws.tabs[0]
                .layout
                .as_ref()
                .is_some_and(|layout| layout.leaf_count() == 1)
        }));
        let text = serde_json::to_string(&restore).unwrap();
        let back: paneflow_config::schema::SessionState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, restore);
    }

    #[test]
    fn the_seed_lands_where_the_app_reads_it() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        seed_home(&home, Scenario::Restore, temp.path());
        let dirs = paneflow_config::loader::user_dirs_under(&home);
        let session = paneflow_config::loader::session_path_in(&dirs);
        assert!(session.is_file(), "no session at {}", session.display());
        assert!(paneflow_config::loader::config_path_in(&dirs).is_file());
        assert!(
            session
                .parent()
                .unwrap()
                .join("window-state.json")
                .is_file()
        );
        // The app reads the seeded session through its own resolver once
        // PANEFLOW_HOME points at `home`: the two paths must agree.
        assert_eq!(
            paneflow_config::loader::user_dirs_from(Some(home.as_os_str())),
            Some(dirs)
        );
    }
}
