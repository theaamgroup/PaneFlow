//! The harness the performance benchmarks share (#425).
//!
//! Both suites, the terminal pipeline (`terminal/perf_bench.rs`) and the code
//! editor (`app/diff_dock/code/perf_bench.rs`), print the same JSON document
//! and the same Markdown comparison table, so the metric type, the timing
//! helpers, the process counters and the reporting live here once. The
//! layout gates in `layout/render.rs` and the Ghostty stress scenarios read
//! the percentile and process-counter helpers from here too.
//!
//! Allocation counts come from the test binary's one `#[global_allocator]`,
//! `terminal/test_allocator.rs`: a crate may install a single one, and the
//! Kitty decoder test shares it, so this module re-exports its counters
//! rather than installing a second wrapper.
//!
//! Everything here is `cfg(test)`: the module is only compiled into the test
//! binary.

use std::time::{Duration, Instant};

pub(crate) use crate::terminal::test_allocator::{allocation_counters, live_bytes};

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Whether a smaller value is the better one. Everything is a cost except
/// throughput.
#[derive(Clone, Copy)]
pub(crate) enum Direction {
    LowerIsBetter,
    HigherIsBetter,
}

pub(crate) struct Metric {
    pub(crate) name: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) direction: Direction,
    /// The headline value: p50 for timings, the raw figure for counts.
    pub(crate) value: f64,
    pub(crate) p95: Option<f64>,
    pub(crate) mean: Option<f64>,
    pub(crate) alloc_bytes_per_iter: Option<f64>,
    pub(crate) allocs_per_iter: Option<f64>,
    pub(crate) iters: usize,
    pub(crate) note: &'static str,
    /// `false` when the scenario could not run on this host (no platform
    /// text system, a skip flag). The metric stays in the document with a
    /// null value so a baseline comparison never silently loses a row.
    pub(crate) available: bool,
}

impl Metric {
    pub(crate) fn count(
        name: &'static str,
        unit: &'static str,
        value: f64,
        note: &'static str,
    ) -> Self {
        Self {
            name,
            unit,
            direction: Direction::LowerIsBetter,
            value,
            p95: None,
            mean: None,
            alloc_bytes_per_iter: None,
            allocs_per_iter: None,
            iters: 1,
            note,
            available: true,
        }
    }

    pub(crate) fn unavailable(name: &'static str, unit: &'static str, note: &'static str) -> Self {
        Self {
            name,
            unit,
            direction: Direction::LowerIsBetter,
            value: 0.0,
            p95: None,
            mean: None,
            alloc_bytes_per_iter: None,
            allocs_per_iter: None,
            iters: 0,
            note,
            available: false,
        }
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "metric": self.name,
            "unit": self.unit,
            "direction": match self.direction {
                Direction::LowerIsBetter => "lower_is_better",
                Direction::HigherIsBetter => "higher_is_better",
            },
            "available": self.available,
            "value": self.available.then_some(self.value),
            "p95": self.p95,
            "mean": self.mean,
            "alloc_bytes_per_iter": self.alloc_bytes_per_iter,
            "allocs_per_iter": self.allocs_per_iter,
            "iters": self.iters,
            "note": self.note,
        })
    }
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

fn from_samples(
    name: &'static str,
    note: &'static str,
    samples: &mut [Duration],
    total: Duration,
    allocated: (u64, u64),
    iters: usize,
) -> Metric {
    samples.sort_unstable();
    let iters_f = iters.max(1) as f64;
    Metric {
        name,
        unit: "ns",
        direction: Direction::LowerIsBetter,
        value: percentile_duration(samples, 50).as_nanos() as f64,
        p95: Some(percentile_duration(samples, 95).as_nanos() as f64),
        mean: Some(total.as_nanos() as f64 / iters_f),
        alloc_bytes_per_iter: Some(allocated.0 as f64 / iters_f),
        allocs_per_iter: Some(allocated.1 as f64 / iters_f),
        iters,
        note,
        available: true,
    }
}

/// Time `op` `iters` times after `warmup` unmeasured runs. The headline value
/// is the median in nanoseconds; allocations are averaged over the measured
/// iterations.
pub(crate) fn measure(
    name: &'static str,
    note: &'static str,
    warmup: usize,
    iters: usize,
    mut op: impl FnMut(),
) -> Metric {
    for _ in 0..warmup {
        op();
    }
    let mut samples = Vec::with_capacity(iters);
    let (bytes_before, calls_before) = allocation_counters();
    let started = Instant::now();
    for _ in 0..iters {
        let iteration = Instant::now();
        op();
        samples.push(iteration.elapsed());
    }
    let total = started.elapsed();
    let (bytes_after, calls_after) = allocation_counters();
    from_samples(
        name,
        note,
        &mut samples,
        total,
        (bytes_after - bytes_before, calls_after - calls_before),
        iters,
    )
}

/// Accumulates only the segments of an iteration the scenario chooses to
/// time, so setup and the work that runs off the render thread stay out of
/// the figure.
#[derive(Default)]
pub(crate) struct SegmentTimer {
    elapsed: Duration,
    bytes: u64,
    calls: u64,
}

impl SegmentTimer {
    pub(crate) fn time<R>(&mut self, op: impl FnOnce() -> R) -> R {
        let (bytes_before, calls_before) = allocation_counters();
        let started = Instant::now();
        let out = op();
        self.elapsed += started.elapsed();
        let (bytes_after, calls_after) = allocation_counters();
        self.bytes += bytes_after - bytes_before;
        self.calls += calls_after - calls_before;
        out
    }
}

/// [`measure`] for an iteration that only times some of its segments through
/// the [`SegmentTimer`] it is handed.
pub(crate) fn measure_segments(
    name: &'static str,
    note: &'static str,
    warmup: usize,
    iters: usize,
    mut op: impl FnMut(&mut SegmentTimer),
) -> Metric {
    for _ in 0..warmup {
        op(&mut SegmentTimer::default());
    }
    let mut samples = Vec::with_capacity(iters);
    let mut total = Duration::ZERO;
    let mut bytes = 0u64;
    let mut calls = 0u64;
    for _ in 0..iters {
        let mut timer = SegmentTimer::default();
        op(&mut timer);
        samples.push(timer.elapsed);
        total += timer.elapsed;
        bytes += timer.bytes;
        calls += timer.calls;
    }
    from_samples(name, note, &mut samples, total, (bytes, calls), iters)
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

pub(crate) fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

fn document(suite: &str, corpus_seed: u64, metrics: &[Metric]) -> serde_json::Value {
    let generated_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    serde_json::json!({
        "schema": 1,
        "suite": suite,
        "generated_unix": generated_unix,
        "stamp": env_or("PANEFLOW_BENCH_STAMP", "unknown"),
        "git_sha": env_or("PANEFLOW_BENCH_SHA", "unknown"),
        "git_dirty": env_or("PANEFLOW_BENCH_DIRTY", "unknown"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": cpu_model(),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "corpus_seed": format!("0x{corpus_seed:016x}"),
        "metrics": metrics.iter().map(Metric::to_json).collect::<Vec<_>>(),
    })
}

pub(crate) fn format_value(value: f64, unit: &str) -> String {
    match unit {
        "ns" if value >= 1_000_000.0 => format!("{:.2} ms", value / 1_000_000.0),
        "ns" if value >= 1_000.0 => format!("{:.1} us", value / 1_000.0),
        "ns" => format!("{value:.0} ns"),
        "MiB/s" => format!("{value:.1} MiB/s"),
        "bytes" => format_bytes(value),
        _ => format!("{value:.0} {unit}"),
    }
}

pub(crate) fn format_bytes(value: f64) -> String {
    if value >= 1024.0 * 1024.0 {
        format!("{:.2} MiB", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.1} KiB", value / 1024.0)
    } else {
        format!("{value:.0} B")
    }
}

fn run_header() -> String {
    format!(
        "Run `{}` ({}), {} {} on {}.\n\n",
        env_or("PANEFLOW_BENCH_SHA", "unknown"),
        env_or("PANEFLOW_BENCH_STAMP", "unknown"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        cpu_model(),
    )
}

fn value_cell(metric: &Metric) -> String {
    if metric.available {
        format_value(metric.value, metric.unit)
    } else {
        "unavailable".to_owned()
    }
}

fn alloc_cell(alloc_bytes_per_iter: Option<f64>) -> String {
    alloc_bytes_per_iter
        .map(format_bytes)
        .unwrap_or_else(|| "n/a".into())
}

/// The table a run prints when the suite has no baseline yet: the same rows,
/// without the comparison columns.
pub(crate) fn results_table(current: &[Metric]) -> String {
    let mut table = run_header();
    table.push_str("No baseline to compare against.\n\n");
    table.push_str("| Metric | Now | Alloc/iter now |\n");
    table.push_str("|---|---|---|\n");
    for metric in current {
        table.push_str(&format!(
            "| `{}` | {} | {} |\n",
            metric.name,
            value_cell(metric),
            alloc_cell(metric.alloc_bytes_per_iter),
        ));
    }
    table
}

/// Markdown table of this run against a baseline document, ready to paste.
pub(crate) fn comparison_table(current: &[Metric], baseline: &serde_json::Value) -> String {
    let baseline_metrics = baseline
        .get("metrics")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let find = |name: &str| {
        baseline_metrics
            .iter()
            .find(|metric| metric.get("metric").and_then(serde_json::Value::as_str) == Some(name))
    };
    let mut table = String::new();
    table.push_str(&format!(
        "Baseline `{}` ({}) versus `{}` ({}), {} {} on {}.\n\n",
        baseline
            .get("git_sha")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        baseline
            .get("stamp")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        env_or("PANEFLOW_BENCH_SHA", "unknown"),
        env_or("PANEFLOW_BENCH_STAMP", "unknown"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        cpu_model(),
    ));
    table.push_str("| Metric | Baseline | Now | Change | Alloc/iter baseline | Alloc/iter now |\n");
    table.push_str("|---|---|---|---|---|---|\n");
    for metric in current {
        let Some(previous) = find(metric.name) else {
            table.push_str(&format!(
                "| `{}` | n/a | {} | new | n/a | {} |\n",
                metric.name,
                value_cell(metric),
                alloc_cell(metric.alloc_bytes_per_iter),
            ));
            continue;
        };
        let before = previous.get("value").and_then(serde_json::Value::as_f64);
        let before_alloc = previous
            .get("alloc_bytes_per_iter")
            .and_then(serde_json::Value::as_f64);
        let change = match before {
            Some(before) if metric.available && before > 0.0 => {
                let ratio = match metric.direction {
                    Direction::LowerIsBetter => before / metric.value,
                    Direction::HigherIsBetter => metric.value / before,
                };
                let percent = (metric.value - before) / before * 100.0;
                format!("{percent:+.1}% ({ratio:.2}x)")
            }
            _ => "n/a".to_owned(),
        };
        let before_value = before
            .map(|value| format_value(value, metric.unit))
            .unwrap_or_else(|| "unavailable".to_owned());
        table.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            metric.name,
            before_value,
            value_cell(metric),
            change,
            alloc_cell(before_alloc),
            alloc_cell(metric.alloc_bytes_per_iter),
        ));
    }
    table
}

/// Print one JSON line per metric and the whole document, write the document
/// to `PANEFLOW_BENCH_OUT` when set, then print the table between the
/// `PANEFLOW_BENCH_TABLE_*` markers: a comparison against
/// `PANEFLOW_BENCH_BASELINE` when that file reads as JSON, the plain results
/// table otherwise.
pub(crate) fn publish(suite: &str, corpus_seed: u64, metrics: &[Metric], cpu_share: f64) {
    for metric in metrics {
        println!("PANEFLOW_BENCH_METRIC {}", metric.to_json());
    }
    let mut document = document(suite, corpus_seed, metrics);
    document["cpu_share"] = serde_json::json!(cpu_share);
    println!("PANEFLOW_BENCH_DOCUMENT {document}");

    if let Some(path) = std::env::var_os("PANEFLOW_BENCH_OUT") {
        let pretty = serde_json::to_string_pretty(&document).expect("document serializes");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, pretty).expect("benchmark output must be writable");
        println!("PANEFLOW_BENCH_WRITTEN {}", path.to_string_lossy());
    }

    let baseline = std::env::var_os("PANEFLOW_BENCH_BASELINE")
        .and_then(|path| std::fs::read_to_string(&path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    println!("PANEFLOW_BENCH_TABLE_BEGIN");
    match baseline {
        Some(baseline) => print!("{}", comparison_table(metrics, &baseline)),
        None => print!("{}", results_table(metrics)),
    }
    println!("PANEFLOW_BENCH_TABLE_END");
}

// ---------------------------------------------------------------------------
// Process counters
// ---------------------------------------------------------------------------

pub(crate) fn percentile_duration(values: &[Duration], percentile: usize) -> Duration {
    let index = values.len().saturating_sub(1).saturating_mul(percentile) / 100;
    values.get(index).copied().unwrap_or_default()
}

pub(crate) fn percentile_us(values: &[Duration], percentile: usize) -> u128 {
    percentile_duration(values, percentile).as_micros()
}

fn task_all_info() -> Option<libproc::libproc::task_info::TaskAllInfo> {
    use libproc::libproc::proc_pid::pidinfo;
    use libproc::libproc::task_info::TaskAllInfo;
    pidinfo::<TaskAllInfo>(std::process::id() as i32, 0).ok()
}

pub(crate) fn resident_set_bytes() -> u64 {
    task_all_info()
        .map(|info| info.ptinfo.pti_resident_size)
        .unwrap_or(0)
}

pub(crate) fn process_cpu_time() -> Duration {
    task_all_info()
        .map(|info| {
            duration_from_mach_ticks(
                info.ptinfo
                    .pti_total_user
                    .saturating_add(info.ptinfo.pti_total_system),
            )
        })
        .unwrap_or_default()
}

/// `pti_total_user` / `pti_total_system` are Mach absolute-time ticks, not
/// nanoseconds. Convert with the kernel timebase (observed 125/3 on arm64).
fn duration_from_mach_ticks(ticks: u64) -> Duration {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: `info` is a local C-layout struct; the syscall only writes it.
    let kr = unsafe { mach_timebase_info(&mut info) };
    if kr != 0 || info.denom == 0 {
        return Duration::ZERO;
    }
    let nanos = u64::try_from(u128::from(ticks) * u128::from(info.numer) / u128::from(info.denom))
        .unwrap_or(u64::MAX);
    Duration::from_nanos(nanos)
}

pub(crate) fn cpu_model() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A debug build measures the compiler, not the code; refuse unless the suite
/// itself is being developed. The profile is read into a local so the
/// assertion is a runtime check in both profiles rather than a constant.
pub(crate) fn refuse_debug_profile() {
    let debug_profile = cfg!(debug_assertions);
    let allowed = std::env::var_os("PANEFLOW_BENCH_ALLOW_DEBUG").is_some();
    assert!(
        !debug_profile || allowed,
        "run this benchmark with cargo test --release (or set PANEFLOW_BENCH_ALLOW_DEBUG=1)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparison table is the public artifact, so its arithmetic is
    /// checked: a halved timing is a 2x improvement and reads as -50%.
    #[test]
    fn comparison_table_reports_speedups_from_the_baseline() {
        let now = [Metric {
            name: "publish_scroll_220x60",
            unit: "ns",
            direction: Direction::LowerIsBetter,
            value: 500_000.0,
            p95: None,
            mean: None,
            alloc_bytes_per_iter: Some(1024.0),
            allocs_per_iter: Some(1.0),
            iters: 1,
            note: "",
            available: true,
        }];
        let baseline = serde_json::json!({
            "git_sha": "abc",
            "stamp": "t0",
            "metrics": [{
                "metric": "publish_scroll_220x60",
                "value": 1_000_000.0,
                "alloc_bytes_per_iter": 2048.0
            }]
        });
        let table = comparison_table(&now, &baseline);
        assert!(
            table.contains(
                "| `publish_scroll_220x60` | 1.00 ms | 500.0 us | -50.0% (2.00x) | 2.0 KiB | 1.0 KiB |"
            ),
            "{table}"
        );
    }

    #[test]
    fn results_table_drops_the_comparison_columns_without_a_baseline() {
        let now = [Metric::count("open_300kb_highlighted", "ns", 1_500.0, "")];
        let table = results_table(&now);
        assert!(table.contains("No baseline to compare against."), "{table}");
        assert!(
            table.contains("| Metric | Now | Alloc/iter now |"),
            "{table}"
        );
        assert!(!table.contains("Change"), "{table}");
        assert!(
            table.contains("| `open_300kb_highlighted` | 1.5 us | n/a |"),
            "{table}"
        );
    }

    #[test]
    fn unavailable_metrics_remain_in_the_document_and_tables() {
        let metric = Metric::unavailable("shape_cold_60_rows", "ns", "no platform text system");
        let json = metric.to_json();
        assert_eq!(json["available"], false);
        assert!(json["value"].is_null());
        assert!(results_table(&[metric]).contains("| `shape_cold_60_rows` | unavailable | n/a |"));
    }

    #[test]
    fn live_bytes_tracks_a_retained_allocation() {
        const RETAINED_BYTES: usize = 16 * 1024 * 1024;
        const CONCURRENT_NOISE_MARGIN: i64 = (RETAINED_BYTES / 2) as i64;
        let before = live_bytes();
        let retained = vec![0u8; RETAINED_BYTES];
        let during = live_bytes();
        assert!(
            during - before >= CONCURRENT_NOISE_MARGIN,
            "a 16 MiB vector must dominate concurrent allocator noise: {before} -> {during}"
        );
        drop(retained);
        let after = live_bytes();
        assert!(
            during - after >= CONCURRENT_NOISE_MARGIN,
            "dropping a 16 MiB vector must dominate concurrent allocator noise: {during} -> {after}"
        );
    }

    #[test]
    fn resident_set_bytes_samples_the_live_process() {
        assert!(
            resident_set_bytes() > 0,
            "live process RSS must be greater than zero"
        );
    }

    #[test]
    fn process_cpu_time_samples_the_live_process() {
        // Burn a little user time so a freshly spawned test process is not at zero.
        let mut acc = 0u64;
        for i in 0..50_000u64 {
            acc = acc.wrapping_add(i);
        }
        std::hint::black_box(acc);
        assert!(
            process_cpu_time() > Duration::ZERO,
            "live process CPU time must be greater than zero"
        );
    }
}
