#!/usr/bin/env bash
# fuzz.sh -- stress the lexer, parser, lowering pass, and VM.
#
#   ./scripts/fuzz.sh                    # libFuzzer when available, else the harness
#   ./scripts/fuzz.sh --deterministic    # always the in-repo harness
#
# libFuzzer needs a nightly toolchain and cargo-fuzz. Neither is installed by
# this script: a missing tool prints the command to install it and falls back to
# the deterministic harness, which needs only the pinned stable toolchain.
#
# FUZZ_SECONDS sets the per-target time for libFuzzer (default 30).

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
fuzz_dir="crates/omni-core/fuzz"
seconds="${FUZZ_SECONDS:-30}"
mode="${1:-auto}"

deterministic() {
  cargo test -p omni-core --lib fuzz_api -- --nocapture
}

if [[ "$mode" == "--deterministic" ]]; then
  deterministic
  exit $?
fi

if command -v cargo-fuzz >/dev/null 2>&1 && rustup run nightly rustc --version >/dev/null 2>&1; then
  echo "== libFuzzer: ${seconds}s per target =="
  status=0
  for target in fuzz_lex fuzz_parse fuzz_lower fuzz_vm; do
    echo "-- $target"
    (cd "$fuzz_dir" && cargo +nightly fuzz run "$target" "corpus/$target" -- -max_total_time="$seconds") || status=$?
    if (( status != 0 )); then break; fi
  done
  exit "$status"
fi

echo "cargo-fuzz or a nightly toolchain is missing; running the deterministic harness."
echo "For libFuzzer: rustup toolchain install nightly && cargo install cargo-fuzz"
deterministic
