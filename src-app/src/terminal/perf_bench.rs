//! Terminal performance benchmarks.
//!
//! One ignored test that runs under the release profile and prints one JSON
//! line per metric, then a document that gathers them all.
//! `scripts/bench-terminal.sh` drives it, archives the document under
//! `bench/results/`, and compares it against `bench/baseline.json`.
//! `bench/README.md` describes the protocol and what each metric means.
//!
//! Everything here is GPU-free and, apart from the shell idle probe, PTY-free,
//! so a run measures the terminal pipeline itself: the libghostty snapshot,
//! the conversion into the neutral `Content`, the window-free layout pass,
//! the render-thread per-frame lookups, and the runtime loop's idle behavior.
//! Allocation counts come from the test binary's recording allocator
//! (`test_allocator`), which wraps the system allocator.
//!
//! The metric names and units are upstream's, so a result document from this
//! fork compares against one of theirs with the same table code. What each
//! metric measures on this tree, before the publish gate (#343) and the
//! layout memo (#344) land, is noted where it differs from upstream's
//! implementation: `publish_*` and `pipeline_corpus_mib_s` pay for a full
//! snapshot-to-`Content` conversion per frame today, `gate_trickle_publishes`
//! reads the chunk count because nothing gates the publish, and the two idle
//! probes count a fixed 10 ms tick. Those are the metrics the gate and the
//! runtime idle work are expected to move; `layout_220x60` is the one the
//! layout memo targets.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui::{Font, FontStyle, FontWeight, px};
use paneflow_terminal_ghostty as ghostty;

use super::bench_corpus::{
    CORPUS_SEED, cpu_model, deterministic_streams, percentile_duration, process_cpu_time,
};
use super::element::{CellDimensions, LayoutInputs, base_font, layout_from_snapshot};
use super::ghostty_session::{
    RUNTIME_LOOP_ITERATIONS, RUNTIME_LOOP_MESSAGES, content_from_ghostty, simulate_gate_trickle,
};
use super::pty_session::TerminalState;
use super::test_allocator::allocation_counters;
use super::types::{Content, Point};

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Whether a smaller value is the better one. Everything is a cost except
/// throughput.
#[derive(Clone, Copy)]
enum Direction {
    LowerIsBetter,
    HigherIsBetter,
}

struct Metric {
    name: &'static str,
    unit: &'static str,
    direction: Direction,
    /// The headline value: p50 for timings, the raw figure for counts.
    value: f64,
    p95: Option<f64>,
    mean: Option<f64>,
    alloc_bytes_per_iter: Option<f64>,
    allocs_per_iter: Option<f64>,
    iters: usize,
    note: &'static str,
}

impl Metric {
    fn count(name: &'static str, unit: &'static str, value: f64, note: &'static str) -> Self {
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
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "metric": self.name,
            "unit": self.unit,
            "direction": match self.direction {
                Direction::LowerIsBetter => "lower_is_better",
                Direction::HigherIsBetter => "higher_is_better",
            },
            "value": self.value,
            "p95": self.p95,
            "mean": self.mean,
            "alloc_bytes_per_iter": self.alloc_bytes_per_iter,
            "allocs_per_iter": self.allocs_per_iter,
            "iters": self.iters,
            "note": self.note,
        })
    }
}

/// Time `op` `iters` times after `warmup` unmeasured runs. The headline value
/// is the median in nanoseconds; allocations are averaged over the measured
/// iterations.
fn measure(
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
    samples.sort_unstable();
    let iters_f = iters.max(1) as f64;
    Metric {
        name,
        unit: "ns",
        direction: Direction::LowerIsBetter,
        value: percentile_duration(&samples, 50).as_nanos() as f64,
        p95: Some(percentile_duration(&samples, 95).as_nanos() as f64),
        mean: Some(total.as_nanos() as f64 / iters_f),
        alloc_bytes_per_iter: Some((bytes_after - bytes_before) as f64 / iters_f),
        allocs_per_iter: Some((calls_after - calls_before) as f64 / iters_f),
        iters,
        note,
    }
}

// ---------------------------------------------------------------------------
// Scenario builders
// ---------------------------------------------------------------------------

const SCROLLBACK_LINES: usize = 10_000;

fn terminal(cols: usize, rows: usize) -> ghostty::DisplayTerminal {
    let size = ghostty::WindowSize::new(cols, rows, 8, 16).expect("valid benchmark grid");
    ghostty::DisplayTerminal::new(
        size,
        SCROLLBACK_LINES,
        ghostty::TerminalAppearance::default(),
    )
    .expect("libghostty must initialize")
}

/// Feed the deterministic corpus until the grid has scrolled at least twice,
/// so every row carries styled content and the scrollback is non-empty.
fn fill(terminal: &mut ghostty::DisplayTerminal, rows: usize) {
    let streams = deterministic_streams();
    let mut lines = 0usize;
    while lines < rows * 3 {
        for stream in &streams {
            terminal.feed(stream).expect("corpus must parse");
            terminal.feed(b"\x1b[0m\r\n").expect("newline must parse");
            lines += 1;
        }
    }
}

/// One line of streaming output: colored, wide enough to matter, and ending in
/// a newline so the whole viewport scrolls. This is what a build log or an
/// agent transcript looks like to the parser.
fn scroll_chunk(index: usize) -> Vec<u8> {
    format!(
        "\x1b[38;5;{}m{index:>7}\x1b[0m  streaming output line with words, numbers 0123456789 and a path src/lib.rs:42\r\n",
        index % 256
    )
    .into_bytes()
}

/// One keystroke echo on the bottom row: the cursor moves and a handful of
/// cells change, no scroll. A dirty-row tracker would have one row to report;
/// today the conversion walks the whole grid either way.
fn echo_chunk(index: usize, rows: usize) -> Vec<u8> {
    format!("\x1b[{rows};1Hprompt> {index:06}").into_bytes()
}

/// What the runtime does per frame today (`update_shared_state`): snapshot
/// the grid and convert every cell into a fresh `Content`, then install it
/// where the previous frame was. The gate port (#343) turns the conversion
/// into an in-place mirror update that recycles the previous frame's buffer,
/// which is what the `publish_*` allocation columns are meant to show.
#[derive(Default)]
struct Publisher {
    front: Option<Content>,
}

impl Publisher {
    fn publish(&mut self, terminal: &mut ghostty::DisplayTerminal) -> &Content {
        let snapshot = terminal.snapshot().expect("snapshot must succeed");
        self.front = Some(content_from_ghostty(snapshot));
        self.front.as_ref().expect("a frame was just published")
    }
}

fn bench_font() -> Font {
    Font {
        family: "test-mono".into(),
        features: gpui::FontFeatures::default(),
        fallbacks: None,
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    }
}

fn layout(content: &Content, cols: usize, rows: usize, theme: &crate::theme::TerminalTheme) {
    let state = layout_from_snapshot(LayoutInputs {
        cells: content.cells.clone(),
        cursor: None,
        selection_range: None,
        copy_mode_cursor: None,
        search_highlights: &[],
        display_offset: content.display_offset,
        history_size: content.history_size,
        desired_cols: cols,
        desired_rows: rows,
        first_visible_row: 0,
        last_visible_row: rows as i32,
        dims: CellDimensions {
            cell_width: px(8.0),
            line_height: px(16.0),
        },
        base_font: bench_font(),
        theme,
        exited: None,
        exit_signal: None,
        integrated_glyphs_enabled: true,
        color_emoji_enabled: true,
    });
    std::hint::black_box(state);
}

/// Runtime loop iterations per second while a session has nothing to do.
fn idle_wakeups_per_second(settle: Duration, window: Duration) -> f64 {
    std::thread::sleep(settle);
    let before = RUNTIME_LOOP_ITERATIONS.load(Ordering::Relaxed);
    let started = Instant::now();
    std::thread::sleep(window);
    let after = RUNTIME_LOOP_ITERATIONS.load(Ordering::Relaxed);
    (after - before) as f64 / started.elapsed().as_secs_f64()
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

fn publish_scenarios(metrics: &mut Vec<Metric>) -> Content {
    let mut sample = None;
    for (cols, rows) in [(220usize, 60usize), (120, 40)] {
        let mut term = terminal(cols, rows);
        fill(&mut term, rows);
        let mut publisher = Publisher::default();
        let mut index = 0usize;
        let name = if cols == 220 {
            "publish_scroll_220x60"
        } else {
            "publish_scroll_120x40"
        };
        metrics.push(measure(
            name,
            "one scrolled line fed, then snapshot plus conversion to the neutral Content (every row dirty)",
            20,
            300,
            || {
                index += 1;
                term.feed(&scroll_chunk(index)).expect("chunk must parse");
                std::hint::black_box(publisher.publish(&mut term));
            },
        ));
        if cols == 220 {
            let mut index = 0usize;
            metrics.push(measure(
                "publish_echo_220x60",
                "one keystroke echo on the bottom row, then snapshot plus conversion (one row dirty)",
                20,
                300,
                || {
                    index += 1;
                    term.feed(&echo_chunk(index, rows)).expect("chunk must parse");
                    std::hint::black_box(publisher.publish(&mut term));
                },
            ));
            // Taken once the scenario is over, so holding a frame never
            // skews the allocation columns of the frames after it.
            sample = publisher.front.take();
        }
    }
    sample.expect("the 220x60 scenario publishes at least once")
}

fn layout_scenario(metrics: &mut Vec<Metric>, content: &Content) {
    let theme = crate::theme::paneflow_dark();
    metrics.push(measure(
        "layout_220x60",
        "window-free layout pass over a full 220x60 snapshot: batched runs, background rects, contrast",
        10,
        200,
        || layout(content, 220, 60, &theme),
    ));
}

fn line_text_scenario(metrics: &mut Vec<Metric>) {
    let (cols, rows) = (220usize, 60usize);
    let state = TerminalState::new_display_only(rows, cols);
    let mut text = String::new();
    for index in 0..rows * 2 {
        text.push_str(&format!(
            "{index:>5} a line with a link https://example.com/{index} and a path src/main.rs:{index}\n"
        ));
    }
    state.write_output(text.as_bytes());
    let backend = state.session_backend();
    let point = Point::new((rows / 2) as i32, 3);
    metrics.push(measure(
        "line_text_at_220x60",
        "text of one hovered row extracted from the published snapshot (link detection input)",
        20,
        500,
        || {
            std::hint::black_box(backend.line_text_at(point));
        },
    ));
}

fn render_thread_lookups(metrics: &mut Vec<Metric>) {
    metrics.push(measure(
        "base_font_resolve",
        "the base Font the renderer resolves for every pane on every frame",
        100,
        20_000,
        || {
            std::hint::black_box(base_font());
        },
    ));
    metrics.push(measure(
        "active_theme_read",
        "the theme read the layout pass makes for every pane on every frame",
        100,
        20_000,
        || {
            std::hint::black_box(crate::theme::active_theme());
        },
    ));
}

fn gate_scenario(metrics: &mut Vec<Metric>) {
    let publishes = simulate_gate_trickle(Duration::from_millis(2), 1_000);
    metrics.push(Metric::count(
        "gate_trickle_publishes",
        "frames per 1000 chunks",
        publishes as f64,
        "grid changes arriving every 2 ms with the queue drained: frames the publish gate lets through",
    ));
}

fn idle_scenarios(metrics: &mut Vec<Metric>) {
    {
        let display = TerminalState::new_display_only(24, 80);
        let rate = idle_wakeups_per_second(Duration::from_millis(200), Duration::from_secs(1));
        metrics.push(Metric::count(
            "idle_wakeups_display_per_s",
            "wakeups/s",
            rate,
            "runtime loop iterations per second of a display-only session with nothing to do",
        ));
        drop(display);
        std::thread::sleep(Duration::from_millis(200));
    }
    let cwd = std::env::current_dir().ok();
    match TerminalState::new(cwd, 1, 1, Some((80, 24)), None, None) {
        Ok(shell) => {
            // A shell takes its own time to print a prompt (a login zsh with
            // a heavy rc file can need a second or two). Wait for the PTY to
            // have been silent long enough, within a bound, before counting.
            let silent_for = Duration::from_millis(1_500);
            let give_up = Instant::now() + Duration::from_secs(15);
            let mut last_change = Instant::now();
            let mut last_bytes = shell.processed_output_bytes_for_test();
            while last_change.elapsed() < silent_for && Instant::now() < give_up {
                std::thread::sleep(Duration::from_millis(100));
                let bytes = shell.processed_output_bytes_for_test();
                if bytes != last_bytes {
                    last_bytes = bytes;
                    last_change = Instant::now();
                }
            }
            let bytes_before = shell.processed_output_bytes_for_test();
            let messages_before = RUNTIME_LOOP_MESSAGES.load(Ordering::Relaxed);
            let rate = idle_wakeups_per_second(Duration::ZERO, Duration::from_secs(1));
            let bytes = shell.processed_output_bytes_for_test() - bytes_before;
            let messages = RUNTIME_LOOP_MESSAGES.load(Ordering::Relaxed) - messages_before;
            println!(
                "PANEFLOW_BENCH_NOTE idle shell: {bytes} PTY bytes parsed, {messages} messages during the window"
            );
            metrics.push(Metric::count(
                "idle_wakeups_shell_per_s",
                "wakeups/s",
                rate,
                "runtime loop iterations per second of a live shell session sitting at its prompt",
            ));
            drop(shell);
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(error) => {
            println!("PANEFLOW_BENCH_SKIP idle_wakeups_shell_per_s: {error}");
        }
    }
}

fn pipeline_scenario(metrics: &mut Vec<Metric>) {
    const ROUNDS: usize = 10;
    let (cols, rows) = (220usize, 60usize);
    let mut term = terminal(cols, rows);
    fill(&mut term, rows);
    let streams = deterministic_streams();
    let bytes = streams.iter().map(Vec::len).sum::<usize>() * ROUNDS;
    let mut publisher = Publisher::default();
    let mut publishes = 0usize;
    let (bytes_before, calls_before) = allocation_counters();
    let started = Instant::now();
    for _ in 0..ROUNDS {
        for stream in &streams {
            term.feed(stream).expect("corpus must parse");
            std::hint::black_box(publisher.publish(&mut term));
            publishes += 1;
        }
    }
    let wall = started.elapsed();
    let (bytes_after, calls_after) = allocation_counters();
    let throughput = bytes as f64 / wall.as_secs_f64() / (1024.0 * 1024.0);
    metrics.push(Metric {
        name: "pipeline_corpus_mib_s",
        unit: "MiB/s",
        direction: Direction::HigherIsBetter,
        value: throughput,
        p95: None,
        mean: Some(wall.as_nanos() as f64 / publishes as f64),
        alloc_bytes_per_iter: Some((bytes_after - bytes_before) as f64 / publishes as f64),
        allocs_per_iter: Some((calls_after - calls_before) as f64 / publishes as f64),
        iters: publishes,
        note: "corpus streams fed one per batch with a publish after each: parse plus snapshot plus conversion throughput",
    });
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

fn document(metrics: &[Metric]) -> serde_json::Value {
    let generated_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    serde_json::json!({
        "schema": 1,
        "suite": "paneflow-terminal-bench",
        "generated_unix": generated_unix,
        "stamp": env_or("PANEFLOW_BENCH_STAMP", "unknown"),
        "git_sha": env_or("PANEFLOW_BENCH_SHA", "unknown"),
        "git_dirty": env_or("PANEFLOW_BENCH_DIRTY", "unknown"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": cpu_model(),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "corpus_seed": format!("0x{CORPUS_SEED:016x}"),
        "metrics": metrics.iter().map(Metric::to_json).collect::<Vec<_>>(),
    })
}

fn format_value(value: f64, unit: &str) -> String {
    match unit {
        "ns" if value >= 1_000_000.0 => format!("{:.2} ms", value / 1_000_000.0),
        "ns" if value >= 1_000.0 => format!("{:.1} us", value / 1_000.0),
        "ns" => format!("{value:.0} ns"),
        "MiB/s" => format!("{value:.1} MiB/s"),
        _ => format!("{value:.0} {unit}"),
    }
}

fn format_bytes(value: f64) -> String {
    if value >= 1024.0 * 1024.0 {
        format!("{:.2} MiB", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.1} KiB", value / 1024.0)
    } else {
        format!("{value:.0} B")
    }
}

/// Markdown table of this run against a baseline document, ready to paste.
fn comparison_table(current: &[Metric], baseline: &serde_json::Value) -> String {
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
                format_value(metric.value, metric.unit),
                metric
                    .alloc_bytes_per_iter
                    .map(format_bytes)
                    .unwrap_or_else(|| "n/a".into()),
            ));
            continue;
        };
        let before = previous
            .get("value")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(f64::NAN);
        let before_alloc = previous
            .get("alloc_bytes_per_iter")
            .and_then(serde_json::Value::as_f64);
        let change = if before.is_finite() && before > 0.0 {
            let ratio = match metric.direction {
                Direction::LowerIsBetter => before / metric.value,
                Direction::HigherIsBetter => metric.value / before,
            };
            let percent = (metric.value - before) / before * 100.0;
            format!("{percent:+.1}% ({ratio:.2}x)")
        } else {
            "n/a".to_owned()
        };
        table.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            metric.name,
            format_value(before, metric.unit),
            format_value(metric.value, metric.unit),
            change,
            before_alloc
                .map(format_bytes)
                .unwrap_or_else(|| "n/a".into()),
            metric
                .alloc_bytes_per_iter
                .map(format_bytes)
                .unwrap_or_else(|| "n/a".into()),
        ));
    }
    table
}

#[test]
#[ignore = "terminal performance benchmark: run through scripts/bench-terminal.sh"]
fn terminal_pipeline_benchmark() {
    // A debug build measures the compiler, not the code; refuse unless the
    // suite itself is being developed.
    if cfg!(debug_assertions) && std::env::var_os("PANEFLOW_BENCH_ALLOW_DEBUG").is_none() {
        panic!(
            "run this benchmark with cargo test --release (or set PANEFLOW_BENCH_ALLOW_DEBUG=1)"
        );
    }

    let mut metrics = Vec::new();
    // The timed scenarios are single-threaded and never sleep, so the CPU
    // time the process accrues over them should match the wall clock. A
    // share well below one means the machine was busy with something else
    // and the timings of this run are inflated; the allocation columns are
    // exact regardless. `process_cpu_time` converts libproc's Mach ticks
    // through `mach_timebase_info`, so the share is a real ratio.
    let timed_started = Instant::now();
    let cpu_before = process_cpu_time();
    let sample = publish_scenarios(&mut metrics);
    layout_scenario(&mut metrics, &sample);
    line_text_scenario(&mut metrics);
    render_thread_lookups(&mut metrics);
    gate_scenario(&mut metrics);
    pipeline_scenario(&mut metrics);
    let cpu_share = (process_cpu_time() - cpu_before).as_secs_f64()
        / timed_started.elapsed().as_secs_f64().max(f64::EPSILON);
    println!("PANEFLOW_BENCH_NOTE cpu share over the timed scenarios: {cpu_share:.2}");
    if cpu_share < 0.9 {
        println!(
            "PANEFLOW_BENCH_WARNING the process only got {:.0}% of a core while timing: another workload was competing, treat the timings as inflated",
            cpu_share * 100.0
        );
    }
    // Last: tearing a shell down keeps the machine busy for a while after
    // the session is dropped (the process tree is terminated and reaped),
    // which would land on the timed scenarios if the probes ran first. The
    // probes count every runtime loop in the process, and by now every
    // earlier session has been shut down.
    if std::env::var_os("PANEFLOW_BENCH_SKIP_IDLE").is_none() {
        idle_scenarios(&mut metrics);
    }

    for metric in &metrics {
        println!("PANEFLOW_BENCH_METRIC {}", metric.to_json());
    }
    let mut document = document(&metrics);
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

    if let Some(path) = std::env::var_os("PANEFLOW_BENCH_BASELINE")
        && let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(baseline) = serde_json::from_str::<serde_json::Value>(&text)
    {
        println!("PANEFLOW_BENCH_TABLE_BEGIN");
        print!("{}", comparison_table(&metrics, &baseline));
        println!("PANEFLOW_BENCH_TABLE_END");
    }
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
    fn the_gate_simulation_never_publishes_more_than_once_per_chunk() {
        assert!(simulate_gate_trickle(Duration::from_millis(2), 100) <= 100);
    }
}
