#!/usr/bin/env bash
# Run the startup first-frame benchmark and archive its result.
#
# Usage:
#   scripts/bench-startup.sh                 # run, compare against bench/startup-baseline.json (when present)
#   scripts/bench-startup.sh --set-baseline  # run, then make this run the baseline (refused from a contended run)
#   scripts/bench-startup.sh --help
#
# The benchmark is the ignored test `startup_first_frame_benchmark` in
# src-app/src/startup_bench.rs. It launches the release `paneflow` binary
# (built here first) against two seeded scratch homes with the startup trace
# in src-app/src/startup_trace.rs enabled, and turns the traced marks into
# metrics. Each run writes bench/results/startup-<stamp>-<sha>.json; the
# table printed between the PANEFLOW_BENCH_TABLE markers is the shareable
# artifact: a comparison when the baseline exists, the plain results
# otherwise. See bench/README.md.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/bench-startup.sh [--set-baseline] [--help]

Builds the release paneflow binary, runs the paneflow-startup-bench suite
against it under the release profile, writes
bench/results/startup-<stamp>-<sha>.json, then prints a Markdown table between
the PANEFLOW_BENCH_TABLE_BEGIN and PANEFLOW_BENCH_TABLE_END markers.

Every launch runs against its own seeded PANEFLOW_HOME under the system temp
directory with its own PANEFLOW_SOCKET_PATH, so the installed app may stay
open and the developer's session is never read or written. Close it anyway
before recording a baseline: it competes for the core.

Options:
  --set-baseline  Copy the fresh result over bench/startup-baseline.json.
                  Refused when the run reports a cpu_share below 0.90 (the
                  suite spins one thread before each scenario and records the
                  share of a core it got), because a contended run inflates
                  every timing it would freeze.
  --help          Print this message and exit.

Environment:
  PANEFLOW_BENCH_OUT           Result file the suite writes. Set by this script.
  PANEFLOW_BENCH_BASELINE      Baseline the table compares against. Set by this
                               script when bench/startup-baseline.json exists;
                               without it the table drops its comparison columns.
  PANEFLOW_BENCH_SHA           Short commit the result records. Set by this script.
  PANEFLOW_BENCH_DIRTY         Whether the tracked worktree is dirty.
  PANEFLOW_BENCH_STAMP         UTC stamp the result records.
  PANEFLOW_BENCH_ALLOW_DEBUG   Allow a debug-profile run, which the suite refuses.
  PANEFLOW_BENCH_EXE           App binary to launch; defaults to target/release/paneflow.
  PANEFLOW_BENCH_STARTUP_RUNS  Timed launches per scenario (default 10).
USAGE
}

cd "$(dirname "$0")/.."

mode="run"
case "${1:-}" in
  --help | -h)
    usage
    exit 0
    ;;
  --set-baseline)
    mode="set-baseline"
    ;;
  "") ;;
  *)
    usage >&2
    exit 2
    ;;
esac

sha=$(git rev-parse --short=12 HEAD)
dirty=false
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  dirty=true
fi
stamp=$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p bench/results
# Absolute paths: cargo runs the test binary from the package directory.
root=$(pwd)
out="$root/bench/results/startup-${stamp}-${sha}.json"

export PANEFLOW_BENCH_OUT="$out"
export PANEFLOW_BENCH_SHA="$sha"
export PANEFLOW_BENCH_DIRTY="$dirty"
export PANEFLOW_BENCH_STAMP="$stamp"
if [ -f bench/startup-baseline.json ]; then
  export PANEFLOW_BENCH_BASELINE="$root/bench/startup-baseline.json"
else
  echo "no baseline yet: bench/startup-baseline.json is absent, the table carries no comparison columns"
  unset PANEFLOW_BENCH_BASELINE
fi

# The suite launches this binary; the test binary is built by the next step
# under the same profile, which the suite checks against the trace it reads.
cargo build --release --locked -p paneflow-app

cargo test --release --locked -p paneflow-app --bin paneflow \
  startup_bench::startup_first_frame_benchmark \
  -- --ignored --exact --nocapture --test-threads=1

if [ ! -f "$out" ]; then
  echo "benchmark produced no result file: $out" >&2
  exit 1
fi
echo "result: $out"
if [ "$mode" = "set-baseline" ]; then
  # A run whose core-share probe read below 0.90 printed
  # PANEFLOW_BENCH_WARNING and must not become the reference every later run
  # is compared against.
  cpu_share=$(sed -n 's/^[[:space:]]*"cpu_share":[[:space:]]*\([0-9.eE+-]*\).*/\1/p' "$out" | head -n 1)
  if [ -z "$cpu_share" ]; then
    echo "the result carries no cpu_share, refusing to record a baseline from it: $out" >&2
    exit 1
  fi
  if awk "BEGIN { exit !($cpu_share < 0.9) }"; then
    echo "cpu_share $cpu_share is below 0.90: the core-share probe got less than 90% of a core, so the launch timings are inflated and every later comparison against them would read as a false improvement. Close the competing workload and run again." >&2
    exit 1
  fi
  cp "$out" bench/startup-baseline.json
  echo "baseline: bench/startup-baseline.json now points at $sha"
fi
