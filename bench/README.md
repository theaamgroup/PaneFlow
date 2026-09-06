# Performance benchmarks

`bench/` holds the reproducible measurements behind PaneFlow's performance
claims. Every number published about the terminal pipeline or the code
editor comes from one of the suites below, run with the script described
here, and the raw result of each run is archived next to the baseline it is
compared against.

There are two suites, two baselines, and two result prefixes:

| Suite | Test | Script | Baseline | Result files |
|---|---|---|---|---|
| `paneflow-terminal-bench` | `terminal::perf_bench::terminal_pipeline_benchmark` | `scripts/bench-terminal.sh` | `bench/baseline.json` | `bench/results/<stamp>-<sha>.json` |
| `paneflow-editor-bench` | `app::diff_dock::code::perf_bench::editor_pipeline_benchmark` | `scripts/bench-editor.sh` | `bench/editor-baseline.json` | `bench/results/editor-<stamp>-<sha>.json` |

Both suites share one harness, `src-app/src/bench_harness.rs`: the metric
type, the timing helpers, the JSON document, the comparison table, and the
libproc process counters (RSS, CPU time converted from Mach ticks through
`mach_timebase_info`). Allocations are counted by the test binary's one
`#[global_allocator]`, `src-app/src/terminal/test_allocator.rs`, a wrapper
around the system allocator installed for the test binary only. It counts
allocated bytes, allocation calls, and live bytes (allocations minus
deallocations), which is how a retained-memory metric can be reported at
all. Both exist only in `cfg(test)` builds.

## Terminal suite

The benchmark is the ignored test `terminal_pipeline_benchmark` in
`src-app/src/terminal/perf_bench.rs`. It exercises the terminal pipeline
without a GPU or a window: the libghostty parser and snapshot, the conversion
into the renderer's neutral `Content`, the window-free layout pass, the
per-frame lookups the render thread performs, and the runtime loop's idle
behavior. Timings are wall-clock medians; allocations are counted by the
shared allocator.

| Metric | Unit | What it captures |
|---|---|---|
| `idle_wakeups_display_per_s` | wakeups/s | Runtime loop iterations of a display-only session with nothing to do. Direct CPU cost of an idle pane. |
| `idle_wakeups_shell_per_s` | wakeups/s | Same for a live shell sitting at its prompt. Skipped when the host cannot spawn a shell. |
| `publish_scroll_220x60` | ns | One scrolled line of styled output, then snapshot plus conversion to `Content`, on a 220x60 grid where every row is dirty. |
| `publish_echo_220x60` | ns | One keystroke echo on the bottom row, then snapshot plus conversion. Only one row changed. |
| `publish_scroll_120x40` | ns | The scroll case on a 120x40 grid, the size of a typical split pane. |
| `layout_220x60` | ns | The layout pass over a full 220x60 snapshot: run batching, background rectangles, contrast checks. |
| `line_text_at_220x60` | ns | Text of one hovered row extracted from the published snapshot, the input of link detection. |
| `base_font_resolve` | ns | The base font resolution the renderer performs for every pane on every frame. |
| `active_theme_read` | ns | The theme read the layout pass performs for every pane on every frame. |
| `gate_trickle_publishes` | frames per 1000 chunks | Frames the publish gate lets through when grid changes arrive every 2 ms with the queue drained. Bounds redraw frequency on trickle output such as a build log or an agent transcript. |
| `pipeline_corpus_mib_s` | MiB/s | Parse plus snapshot plus conversion throughput over the deterministic corpus, one publish per stream. |

Every timing metric also reports p95, mean, bytes allocated per iteration, and
allocation calls per iteration. `gate_trickle_publishes` and the two idle
probes are counts, not timings. Lower is better everywhere except
`pipeline_corpus_mib_s`.

The corpus is `deterministic_streams()` in
`src-app/src/terminal/bench_corpus.rs`, seeded with `CORPUS_SEED`, so every
run parses byte-identical input.

The metric names, units, and result schema are upstream's, so a result from
this fork compares against an upstream document with the same table code.
The committed baseline is the run before the September 2026 terminal
performance work (#343, #344): there was no publish gate yet, so
`gate_trickle_publishes` read the chunk count (1000), the `publish_*` and
`pipeline_corpus_mib_s` metrics paid for a full snapshot-to-`Content`
conversion per frame, and both idle probes counted a fixed 10 ms runtime
tick. Later runs show the cumulative change.

`PANEFLOW_BENCH_SKIP_IDLE=1` skips the two idle probes, which spend several
seconds waiting for a shell to settle; the timed scenarios run first either
way, so the probes never disturb them.

## Editor suite

The benchmark is the ignored test `editor_pipeline_benchmark` in
`src-app/src/app/diff_dock/code/perf_bench.rs`. It exercises the diff dock's
code editor without a GPU or a window: the rope document, the tree-sitter
parse and highlight query, the run resolution the diff view shares, the
UTF-16 conversions the input handler makes, the external-reload path, and
the platform shaper.

| Metric | Unit | What it captures |
|---|---|---|
| `open_300kb_highlighted` | ns | A 300 KB Rust file opened: rope build, longest-line measure, tree-sitter parse, and an explicit query of the first 60-row viewport. |
| `open_3_7mb` | ns | A 3.7 MB Rust file opened past the 300 KB highlight cap, covering the rope build and the source-string longest-line scan. |
| `open_markdown_injected` | ns | A 64 KB Markdown file opened, the only corpus that runs a second grammar pass through the inline injection. |
| `keystroke_to_runs` | ns | Render-thread work of one inserted character at a pseudo-random row of 300 KB of Rust: splice, incremental parse, highlight requery. Deferred parses run outside the timer; the `apply_parsed` requery they trigger is inside it. Its `p95` column is the keystroke latency target. |
| `viewport_query_60_rows` | ns | The highlight query for one 60-row viewport, the work a viewport-bounded requery would do per frame. |
| `unclosed_comment_close_ui` | ns | Render-thread work of closing an unterminated block comment at the top of the file, which re-tokenizes the whole document. |
| `resolve_runs_3750` | ns | `resolve_runs` over 3 750 captures taken from a 10 000-character minified JSON line, the shape the diff view shares. |
| `byte_to_utf16_eof` | ns | One byte offset converted to a UTF-16 offset at the end of a 3.7 MB document, two to four times per keystroke through `EntityInputHandler`. |
| `to_disk_string_3_7mb` | ns | The whole 3.7 MB document rendered to the string a save writes. |
| `theme_switch` | ns | A theme change on 300 KB of Rust, which today requeries the whole document on the render thread. |
| `shape_cold_60_rows` | ns | Sixty never-seen ASCII rows of 100 characters shaped with the editor monospace font, the cold-cache cost of one scrolled viewport. |
| `shape_warm_60_rows` | ns | The same sixty rows shaped again, the warm-cache cost the line-layout cache serves on a second frame. |
| `reload_200_retained_bytes` | bytes | Live allocated bytes a tab still holds after 200 external reloads of a 2 MB file: document, highlighter, and undo history. |

The corpus is `src-app/src/app/diff_dock/code/bench_corpus.rs`, seeded with
`EDITOR_CORPUS_SEED`. It is generated, never read from the repository's own
sources, so a run is byte-identical everywhere: synthetic Rust sized to 295 KB
(under the 300 KB highlight cap), 2 MB, and 3.7 MB (about 110 000 lines); a
single-line minified JSON document of exactly 10 000 characters; and Markdown
carrying both inline and fenced code so the injection pass has work to do.

These are the metrics the editor performance ports that follow this harness
(#426 to #430) are expected to move; each of those lands with its
`perf_bench.rs` half and re-records the baseline in the same pull request.

### The shaping probe

`shape_cold_60_rows` and `shape_warm_60_rows` decide whether an ASCII glyph
grid for the editor is worth building. **The threshold is 1.0 ms cold per 60
rows on the reference machine.** Below it, `shape_line` is not what makes
scrolling expensive and the grid stays unbuilt; at or above it, the grid path
is worth its complexity. Like every other timing metric, both are stored in
nanoseconds and rendered by the table in milliseconds once they pass 1 ms, so
the threshold reads as `1.00 ms` in the table and `1000000.0` in the
document.

The probe deliberately does not use GPUI's `TestAppContext`. That context
installs `NoopTextSystem`, a stub that returns synthetic metrics for every
font, so a measurement taken through it would describe the stub and not the
platform shaper the editor actually pays for. The probe instead resolves the
real platform text system through `gpui_platform::current_platform(true)`
(the `font-kit` feature this fork requires on macOS) and shapes through a
`WindowTextSystem` built on it. When that platform cannot be created, or
when it shapes a zero-width line because no real font is available, both
metrics are reported as unavailable through `PANEFLOW_BENCH_SKIP` lines,
remain in the JSON with `available: false` and a null value, and the suite
carries on. `PANEFLOW_BENCH_SKIP_SHAPE=1` skips the probe outright with the
same unavailable result.

`reload_200_retained_bytes` allocates and retains several hundred megabytes by
design, which is the defect it measures. It runs last, after the timed
scenarios, so it never inflates them.

## Scroll frame scenario

The editor suite runs without a window, so it cannot say what one wheel notch
costs when terminals share the frame. That number comes from a separate
ignored test, `layout::render::tests::editor_scroll_frame_by_pane_count`:

```bash
cargo test -p paneflow-app --release -- --ignored layout::render --test-threads=1
```

`--test-threads=1` is load-bearing: the scenario shares the render_content
timing probe with the eight-pane input-to-paint gate in the same module, and
two of them running at once would count each other's snapshots.

It opens the 300 KB Rust corpus in a `CodeView` docked to the right of the pane
grid, fills every terminal pane with `deterministic_streams()` and lets them go
idle, places the caret at the top and scrolls away from it, then dispatches 120
`ScrollWheelEvent` notches of `Lines(3)` spaced 8 ms apart. It repeats that for
0, 2 and 6 terminal panes and prints one JSON line carrying
`scroll_frame_p50_us_panes_N` and `scroll_frame_p95_us_panes_N` for each N,
computed from GPUI's `dirty_to_draw_duration` over at least 100 frames per
configuration. `render_content_lock_samples_panes_N` counts one terminal
snapshot per pane per traced frame, which is the witness that every terminal is
repainted by a scroll that only moved the editor. A configuration that cannot
build its panes is reported with `scroll_frame_available_panes_N: false` and the
others still run.

**The measurement is relative, not absolute.** `TestAppContext` installs
`NoopTextSystem`, so no platform shaping is included: the numbers compare
configurations against each other and never bound the real cost of a frame.
The absolute cost is read from a release profile of the running application.

`terminal_share_p50_panes_6` and `terminal_share_p95_panes_6` are the fraction
of a six-pane scroll frame that disappears at zero panes. That share is the
decision gate for caching the terminal panes behind `ViewElement::cached`
(#429): it is worth doing only if the share reaches 0.30, and below that the
measured value is recorded and the idea is dropped.

## Running

```bash
scripts/bench-terminal.sh
scripts/bench-editor.sh
```

`scripts/bench-editor.sh --help` prints the options and the environment
variables both suites honor.

Each script builds the `paneflow` test binary under the release profile,
records the short commit SHA, whether the worktree is dirty, and a UTC stamp,
then writes its result under `bench/results/`. The run always prints a
Markdown table between the `PANEFLOW_BENCH_TABLE_BEGIN` and
`PANEFLOW_BENCH_TABLE_END` markers: a comparison table when the suite's
baseline exists, and the same table without its comparison columns when it
does not (`bench-editor.sh` says `no baseline yet` first). That table is the
artifact to share.

`--set-baseline` copies the fresh result over the suite's baseline. The
committed terminal baseline is measured on the Apple Silicon development
machine at the commit before the September 2026 terminal performance work
(#343, #344), with nothing else running; its `git_sha` and `cpu` fields say
which commit and machine. The editor baseline is recorded the same way, with
the PaneFlow app closed: the running app is enough to trip the CPU-share
check below.

`scripts/bench-editor.sh` refuses `--set-baseline` when the run reports a
`cpu_share` below 0.90: a contended run inflates every timing it would freeze,
and every later comparison against it would read as a false improvement. Close
the competing workload and run again.

**A change that moves a metric updates the baseline in the same pull request.**
A baseline older than the code it is compared against turns every table into
fiction.

Both suites refuse to run under the debug profile, which would measure the
compiler rather than the code, and exit non-zero with an explicit message. Set
`PANEFLOW_BENCH_ALLOW_DEBUG=1` to override while developing a suite itself.

Neither suite runs in CI. They are local artifacts compared against a local
baseline, which is what makes the comparison meaningful.

## Fairness rules

A comparison is only meaningful between runs on the same machine, at the same
grid sizes, with the same corpus seed, and both built under the release
profile. The result document records OS, architecture, CPU model, profile,
seed, and commit so that a mismatched comparison is visible. Close heavy
applications before a run; the medians are robust to a stray interruption,
the p95 values are not.

Two runs of the same commit differ by a few percent on the microsecond
metrics. Treat a change below 5% as noise unless the allocation columns, which
are deterministic, moved with it.

The run measures its own CPU share over the timed scenarios (process CPU time
divided by wall time, recorded as `cpu_share` in the result). The scenarios
are single-threaded and never sleep, so an uncontended run reports close to
1.0. Process CPU time comes from libproc, whose figures are Mach ticks and are
converted through `mach_timebase_info`. A run that prints
`PANEFLOW_BENCH_WARNING` got less than 90% of a core: something else was
competing (a `cargo build` in another worktree is enough, and so is the
installed PaneFlow app with agents running), its timings are inflated, and it
should not be published as a comparison.

## Reading the table

`Change` is the relative move of the headline value and, in parentheses, the
speedup: baseline over now for costs, now over baseline for throughput. A
timing that halved reads `-50.0% (2.00x)`. `Alloc/iter` columns show bytes
allocated per iteration and are exact. A metric the host could not measure
reads `unavailable`.

## Result schema

```json
{
  "schema": 1,
  "suite": "paneflow-terminal-bench",
  "generated_unix": 0,
  "stamp": "20260903T120000Z",
  "git_sha": "90e63281abcd",
  "git_dirty": "false",
  "os": "macos",
  "arch": "aarch64",
  "cpu": "macos-aarch64",
  "profile": "release",
  "corpus_seed": "0x...",
  "cpu_share": 0.99,
  "metrics": [
    {
      "metric": "publish_scroll_220x60",
      "unit": "ns",
      "direction": "lower_is_better",
      "available": true,
      "value": 0.0,
      "p95": 0.0,
      "mean": 0.0,
      "alloc_bytes_per_iter": 0.0,
      "allocs_per_iter": 0.0,
      "iters": 300,
      "note": "..."
    }
  ]
}
```

The editor suite writes the same document with `"suite":
"paneflow-editor-bench"` and its own `corpus_seed`. A metric with
`"available": false` carries a null `value`.
