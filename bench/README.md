# Performance benchmarks

`bench/` holds the reproducible measurements behind PaneFlow's performance
claims. Every number published about the terminal pipeline or the time to
first frame comes from one of the suites below, run with the script described
here, and the raw result of each run is archived next to the baseline it is
compared against.

There are two suites, two baselines, and two result prefixes:

| Suite | Test | Script | Baseline | Result files |
|---|---|---|---|---|
| `paneflow-terminal-bench` | `terminal::perf_bench::terminal_pipeline_benchmark` | `scripts/bench-terminal.sh` | `bench/baseline.json` | `bench/results/<stamp>-<sha>.json` |
| `paneflow-startup-bench` | `startup_bench::startup_first_frame_benchmark` | `scripts/bench-startup.sh` | `bench/startup-baseline.json` | `bench/results/startup-<stamp>-<sha>.json` |

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

## Highlight caps

`MAX_HIGHLIGHT_BYTES` (2 MB) and `MAX_MARKDOWN_HIGHLIGHT_BYTES` (1 MB) in
`src-app/src/diff/highlighter.rs` bound tree-sitter parsing for Changes and
Review. They were set so a file at its cap holds less than 128 MiB of
tree-sitter tree: minified JSON for the single-pass cap, Markdown's second
pass for its own. The measurement that set them lived with the removed dock
code editor. The caps stay. This tree has no probe that re-measures them.

## Startup suite

The benchmark is the ignored test `startup_first_frame_benchmark` in
`src-app/src/startup_bench.rs`. Unlike the terminal suite it launches the
real release binary, because the cost it measures is the GPU window, the
platform text system, and the state the app builds before its first frame,
none of which exist in a window-free test. The app cooperates through the
startup trace in `src-app/src/startup_trace.rs`: when `PANEFLOW_STARTUP_TRACE`
names a file, the app records a mark at each stage of `main`,
`mount_paneflow_app`, and `PaneFlowApp::new`, writes the timeline as JSON
once the frame it was launched to measure has been presented, and quits. The
probe ships in release builds so the shipping profile is what gets measured;
without the variable every mark is one `OnceLock` read.

While tracing, the app also pumps its own frames. GPUI requests frames from
a per-window display link that macOS only starts once the window is key or
its occlusion state changes, and a benchmark launches the app from a process
that is not the active application, so macOS refuses the activation and the
window would get exactly one frame (AppKit's initial layer display) and then
none: a staged restore, one batch per frame, would stall on its first batch.
The probe therefore sends `displayLayer:` to the GPUI view every 8 ms from a
foreground task until the trace is written, which is the same path the first
frame comes through (it runs the pending next-frame callbacks, draws, and
presents with a transaction, so AppKit still paces it at the display
refresh). The launched windows may open behind the terminal running the
script; that is fine, and nothing needs to be clicked.

Two scenarios run back to back, each against its own seeded `PANEFLOW_HOME`
under the system temp directory (see `docs/user/configuration/schema.md` for
that variable) and a fixture `PANEFLOW_SOCKET_PATH`, so the launches never
read the developer's session, never bind the installed app's socket, and
never write into the real per-user directories:

| Scenario | Prefix | Home contents | Measured frame |
|---|---|---|---|
| Fresh | `fresh_` | An empty session. The app builds its default workspace, one shell in the fixture directory, before the first frame. | The first presented frame. |
| Restore | `restore3_` | A session of three workspaces with one terminal pane each, all in the fixture directory. The daily case: a restored layout whose panes spawn shells. | The frame after the last restore batch (`restored_frame`). The fork restores one batch per frame after the first frame (issue #156), so the first frame alone would show an empty root. |

The seed writes `session.json`, `paneflow.json` (`{}`) and
`window-state.json` (a fixed 1400x900 window) under the config root's
`APP_SUBDIR` namespace before the first launch. One untimed warm-up launch
per scenario absorbs the first-exec Gatekeeper scan of a freshly linked
binary and the helper extraction into the scratch cache; the timed launches
that follow (`PANEFLOW_BENCH_STARTUP_RUNS`, default 10) therefore measure a
second launch. The suite refuses a debug binary unless
`PANEFLOW_BENCH_ALLOW_DEBUG` is set, and refuses an app binary whose profile
differs from the test binary's, because the fixture is seeded under the test
binary's namespace. `PANEFLOW_BENCH_EXE` overrides the binary path, which
otherwise resolves to `paneflow` next to the test binary's profile directory.

| Metric | Unit | What it captures |
|---|---|---|
| `<scenario>_first_frame_total` | ns | From the first line of `main` to the measured frame of the app window. The headline number of each scenario. |
| `<scenario>_step_<mark>` | ns | The time between one trace mark and the previous one, one metric per mark in launch order. |

The marks, in launch order: `login_shell_env_loaded` (the login shell's PATH
adopted), `crash_reporting_ready` (the first config read and the Sentry
guard), `editor_cache_ready` (the installed-editor snapshot),
`bridge_extracted` and `ai_hook_extracted` (the two stable helper copies),
`gpui_app_ready` (platform and text system initialization inside GPUI),
`config_loaded`, `fonts_loaded` (the embedded fonts registered),
`window_requested` and `window_created` (the GPU window), `session_loaded`
(the capped `session.json` read), `ipc_server_started` (the singleton guard
and IPC thread), then either `default_workspace_built` (Fresh: the shell
spawned) or `session_restore_scheduled` (Restore), `app_fields_prepared`,
`app_state_built`, `app_mounted` (the observers and the staged restore
armed), `window_open_returned`, `first_render`, `first_render_built` (the
element tree exists; layout and paint have not run), `first_frame`, and for
Restore `session_restored` (the last batch applied) and `restored_frame`.

The mark names are the metric names, so adding a mark adds a metric and the
comparison table reports it as new. The timed work happens in a child
process, so the harness cannot attribute a core share to it; instead the
suite spins one thread for 250 ms before each scenario and records the
lowest share of a core it got as `cpu_share`. Below 0.90 it prints
`PANEFLOW_BENCH_WARNING` and `scripts/bench-startup.sh --set-baseline`
refuses the run.

## Running

```bash
scripts/bench-terminal.sh
scripts/bench-startup.sh
```

`scripts/bench-startup.sh --help` prints the options and the environment
variables the startup suite honors.

Each script builds the `paneflow` test binary under the release profile,
records the short commit SHA, whether the worktree is dirty, and a UTC stamp,
then writes its result under `bench/results/`. The run always prints a
Markdown table between the `PANEFLOW_BENCH_TABLE_BEGIN` and
`PANEFLOW_BENCH_TABLE_END` markers: a comparison table when the suite's
baseline exists, and the same table without its comparison columns when it
does not. That table is the
artifact to share.

`--set-baseline` copies the fresh result over the suite's baseline. The
committed terminal baseline is measured on the Apple Silicon development
machine at the commit before the September 2026 terminal performance work
(#343, #344), with nothing else running; its `git_sha` and `cpu` fields say
which commit and machine.

`scripts/bench-startup.sh` refuses
`--set-baseline` when the run reports a `cpu_share` below 0.90: a contended
run inflates every timing it would freeze, and every later comparison against
it would read as a false improvement. Close the competing workload and run
again. The startup baseline is measured on the same machine with the
installed PaneFlow app closed.

**A change that moves a metric updates the baseline in the same pull request.**
A baseline older than the code it is compared against turns every table into
fiction.

Both suites refuse to run under the debug profile, which would measure
the compiler rather than the code, and exit non-zero with an explicit message.
Set `PANEFLOW_BENCH_ALLOW_DEBUG=1` to override while developing a suite itself.

No suite runs in CI. They are local artifacts compared against a local
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

The terminal run measures its own CPU share over the timed
scenarios (process CPU time divided by wall time, recorded as `cpu_share` in
the result). Those scenarios are single-threaded and never sleep, so an
uncontended run reports close to 1.0. Process CPU time comes from libproc,
whose figures are Mach ticks and are converted through `mach_timebase_info`.
The startup suite cannot measure the launched child this way: its
`cpu_share` is the lowest core-share probe taken before a scenario's launches
(see the startup section above), which catches a competing workload that is
already running but not a stall that begins during the launches. For that
suite, compare each metric's `mean` with its `p95`: with ten launches the p95
is the ninth smallest sample, so a mean above it proves one launch stalled,
and such a run should not become the baseline either. A run that prints
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

The startup suite writes
`"suite": "paneflow-startup-bench"`, a `corpus_seed` of `0x0` (it has no
corpus), null allocation columns (the allocations happen in the child
process), and the core-share probe as `cpu_share`. A metric with
`"available": false` carries a null `value`.
