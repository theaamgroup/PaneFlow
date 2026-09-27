#!/usr/bin/env bash
# Run the terminal performance benchmark and archive its result.
#
# Usage:
#   scripts/bench-terminal.sh                 # run, compare against bench/baseline.json (when present)
#   scripts/bench-terminal.sh --set-baseline  # run, then make this run the baseline
#
# The benchmark is the ignored test `terminal_pipeline_benchmark` in
# src-app/src/terminal/perf_bench.rs, built under the release profile. Each run
# writes bench/results/<stamp>-<sha>.json; the comparison table printed between
# the PANEFLOW_BENCH_TABLE markers is the shareable artifact. See bench/README.md.
set -euo pipefail

cd "$(dirname "$0")/.."

mode="run"
if [ "${1:-}" = "--set-baseline" ]; then
  mode="set-baseline"
fi

sha=$(git rev-parse --short=12 HEAD)
dirty=false
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  dirty=true
fi
stamp=$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p bench/results
# Absolute paths: cargo runs the test binary from the package directory.
root=$(pwd)
out="$root/bench/results/${stamp}-${sha}.json"

export PANEFLOW_BENCH_OUT="$out"
export PANEFLOW_BENCH_SHA="$sha"
export PANEFLOW_BENCH_DIRTY="$dirty"
export PANEFLOW_BENCH_STAMP="$stamp"
if [ -f bench/baseline.json ]; then
  export PANEFLOW_BENCH_BASELINE="$root/bench/baseline.json"
fi

cargo test --release --locked -p paneflow-app --bin paneflow \
  terminal::perf_bench::terminal_pipeline_benchmark \
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
    echo "cpu_share $cpu_share is below 0.90: the core-share probe got less than 90% of a core, so the terminal timings are inflated and every later comparison against them would read as a false improvement. Close the competing workload and run again." >&2
    exit 1
  fi
  cp "$out" bench/baseline.json
  echo "baseline: bench/baseline.json now points at $sha"
fi
