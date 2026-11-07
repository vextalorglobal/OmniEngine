#!/usr/bin/env bash
# bench.sh -- run every benchmark in this repository and put the answers side by side.
#
#   ./scripts/bench.sh [iterations]        # default 20000
#
# The three harnesses measure three different things, which is why there are
# three of them and not one:
#
#   crates/omni-core/examples/bench.rs   the engine, called from Rust
#   sdks/python/bench.py                 the engine, behind ctypes
#   sdks/nodejs/bench.js                 the engine, behind koffi
#
# Each prints its own tables and exits non-zero if a check it makes fails --
# memory that grows with the iteration count, or per-item time that climbs as
# the workload gets bigger. This script runs whichever hosts are installed,
# keeps each full report under `target/bench/`, and ends with the comparison.
#
# The comparison is the point. The gap between the Rust row and the other two is
# the price of the bridge, and a batch crossing shares exactly that cost across
# its items -- which is why the speed-up column is read from each report's batch
# table rather than guessed.
#
# A host whose tool is missing is reported as SKIP, not a failure, so this runs
# on a machine that has only cargo.

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
ROOT="$PWD"
OUT="$ROOT/target/bench"
mkdir -p "$OUT"

ITERS="${1:-20000}"

have() { command -v "$1" >/dev/null 2>&1; }

failures=0
declare -a rows=()
declare -a skips=()

# The two figures worth comparing across hosts, pulled out of a human-readable
# report. Parsing the printed tables instead of asking each harness for JSON is
# deliberate: the summary has to describe the same numbers a person reads, so a
# change to a table can only make the summary wrong in a way a person would also
# see.
#
#   $1 -- report file.
# Prints "<ns> <best>" for the `string` scenario and the best per-item batch
# speed-up. A missing field is "?" rather than a silent zero.
parse_report() {
  local report="$1"
  local ns best
  ns=$(awk '$1 == "string" { print $2; exit }' "$report")
  best=$(awk '$NF ~ /^[0-9]+(\.[0-9]+)?x$/ {
      v = $NF; sub("x", "", v);
      if (v + 0 > m) m = v + 0
    }
    END { printf "%.2f", m + 0 }' "$report")
  printf '%s %s' "${ns:-?}" "${best:-0.00}"
}

# One host: run its command in a subshell (the SDKs' commands `cd`), keep the
# report, and record a summary row.
run_host() {
  local label="$1" script="$2"
  local report="$OUT/$label.txt"
  echo "$label..."
  # shellcheck disable=SC2086 -- $script is a command line, not one argument
  if ! ( eval "$script" ) >"$report" 2>&1; then
    failures=$((failures + 1))
    grep -E 'FAIL|failed' "$report" | head -5 | sed 's/^/      /'
    echo "  FAIL  $label -- full report: $report"
    rows+=("$(printf '%-8s %12s %14s  %s' "$label" '-' '-' 'FAILED')")
    return
  fi
  local parsed ns best
  parsed=$(parse_report "$report")
  ns=${parsed%% *}
  best=${parsed##* }
  echo "  PASS  $label: ${ns} ns/exec, best batch ${best}x -- $(basename "$report")"
  rows+=("$(printf '%-8s %12s %14s  %s' "$label" "${ns} ns" "${best}x" 'checks passed')")
}

skip_host() {
  echo "  SKIP  $1 ($2)"
  skips+=("$1")
}

echo "== OmniEngine benchmarks ==   $ITERS iterations per measurement"
echo "   workspace: $ROOT"
echo

# ---------------------------------------------------------------- Rust
# Release, because a debug build measures the absence of optimisation rather
# than the engine. The example installs its own counting allocator, and it is a
# binary of its own, so nothing else in the workspace is affected.
if have cargo; then
  run_host rust "cargo run --release -q -p omni-core --example bench -- $ITERS"
else
  skip_host rust "cargo not on PATH"
fi

# ---------------------------------------------------------------- Python
# The SDK has no build step: `bench.py` imports `omni` from this tree and loads
# whatever artifact the last `cargo build -p omni-ffi` produced.
if have python; then
  run_host python "cd sdks/python && python bench.py $ITERS"
elif have py; then
  run_host python "cd sdks/python && py -3 bench.py $ITERS"
else
  skip_host python "python not on PATH"
fi

# ---------------------------------------------------------------- Node
if have node; then
  if [[ -d sdks/nodejs/node_modules/koffi ]]; then
    run_host node "cd sdks/nodejs && node bench.js $ITERS"
  else
    skip_host node "koffi not installed: cd sdks/nodejs && npm install koffi"
  fi
else
  skip_host node "node not on PATH"
fi

# ---------------------------------------------------------------- report
echo
echo "== Summary =="
echo "$(printf '%-8s %12s %14s  %s' 'host' 'string' 'best batch' 'checks')"
# ${arr[@]+...} because an empty array is an unset variable under `set -u` in
# some shells, and every host being skipped is a real case, not a bug.
for row in ${rows[@]+"${rows[@]}"}; do echo "$row"; done
for host in ${skips[@]+"${skips[@]}"}; do
  echo "$(printf '%-8s %12s %14s  %s' "$host" '-' '-' 'skipped')"
done
echo
echo "  string      nanoseconds for \`\"hello, \" + \"world\" + \"!\"\` through that host, fastest round."
echo "  best batch  per-item speed-up of the batch entry point over a loop, at its best N."
echo "              N=1 is always a loss -- one extra list is still extra work -- so this is"
echo "              the best measured case, not a promise about a particular batch size."
echo
echo "The rust row is the engine. The other rows are the engine plus their bridge, so the"
echo "difference between them is what one crossing costs in that language -- and that cost is"
echo "exactly what a batch shares across its items. On a host whose bridge is cheaper than the"
echo "work it wraps, a batch earns almost nothing, and these tables say so."
echo
echo "Full tables: $OUT/<host>.txt"
echo
if (( failures > 0 )); then
  echo "$failures host(s) failed their own checks."
  exit 1
fi
if (( ${#rows[@]} == 0 )); then
  echo "Nothing could run -- no host tool was available."
  exit 1
fi
echo "All hosts that could run, passed their checks."
