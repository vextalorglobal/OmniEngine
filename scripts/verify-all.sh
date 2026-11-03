#!/usr/bin/env bash
# verify-all.sh -- prove that OmniEngine returns the same answer from every host.
#
#   ./scripts/verify-all.sh
#
# Rust is required. C, Python, and Node are checked only when the tool is on
# PATH, and a missing tool is reported as SKIP rather than a failure, so this
# script is usable on a machine that has not got the whole toolchain yet.
#
# Exits non-zero if anything that could run, failed.

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
ROOT="$PWD"
OUT="$ROOT/target/verify"
mkdir -p "$OUT"

failures=0
declare -a rows=()

record() { rows+=("$1"); }
pass() { record "PASS  $1"; }
skip() { record "SKIP  $1 ($2)"; }
fail() { record "FAIL  $1"; failures=$((failures + 1)); }
have() { command -v "$1" >/dev/null 2>&1; }

echo "== OmniEngine cross-host verification =="
echo "   workspace: $ROOT"
echo

# ---------------------------------------------------------------- Rust
printf 'Rust tests... '
if cargo test --workspace 2>&1 | grep -qE 'test result: FAILED|error\[|error:'; then
  fail "cargo test --workspace"
  cargo test --workspace 2>&1 | tail -20
else
  total=$(cargo test --workspace 2>&1 | grep -oE '[0-9]+ passed' | awk '{s += $1} END {print s+0}')
  pass "cargo test --workspace (${total:-0} tests)"
fi

printf 'Shared library... '
if cargo build -p omni-ffi 2>&1 | grep -qE 'error\[|error:'; then
  fail "cargo build -p omni-ffi"
else
  lib="$ROOT/target/debug/$(case "$OSTYPE" in
    msys*|cygwin*|win32*) echo omni.dll ;;
    darwin*) echo libomni.dylib ;;
    *) echo libomni.so ;;
  esac)"
  if [[ -f "$lib" ]]; then
    pass "$(basename "$lib") ($(du -h "$lib" | cut -f1))"
  else
    fail "expected artifact $lib was not produced"
  fi
fi

# ---------------------------------------------------------------- CLI
printf 'omni-cli... '
if have cargo; then
  cli_out=$(cargo run -q -p omni-cli -- run examples/hello.omni 2>&1)
  if [[ "$cli_out" == *42* ]]; then pass "omni run examples/hello.omni -> $cli_out"
  else fail "omni-cli printed: $cli_out"; fi
else
  skip "omni-cli" "cargo not on PATH"
fi

# ---------------------------------------------------------------- C
printf 'C embedding... '
if have gcc; then
  gcc_args=(-std=c11 -Wall -Wextra -I crates/omni-ffi/include examples/c/embed.c -o "$OUT/embed.exe")
  case "$OSTYPE" in
    msys*|cygwin*|win32*) gcc "${gcc_args[@]}" target/debug/libomni.dll.a 2>"$OUT/gcc.log" ;;
    darwin*)              gcc "${gcc_args[@]}" -L target/debug -lomni 2>"$OUT/gcc.log" ;;
    *)                    gcc "${gcc_args[@]}" -L target/debug -lomni -Wl,-rpath,"$ROOT/target/debug" 2>"$OUT/gcc.log" ;;
  esac
  if [[ ! -f "$OUT/embed.exe" ]]; then
    fail "gcc could not build examples/c/embed.c: $(tail -3 "$OUT/gcc.log")"
  else
    # The loader looks next to the executable, so put the artifact there.
    cp -f target/debug/omni.dll "$OUT/" 2>/dev/null || cp -f target/debug/libomni.so "$OUT/" 2>/dev/null || true
    c_out=$(cd "$OUT" && ./embed.exe 2>/dev/null || ./embed 2>/dev/null)
    if [[ "$c_out" == *"all C embedding checks passed"* ]]; then
      pass "examples/c/embed.c -> 42 via omni.h"
    else
      fail "embed output: $c_out"
    fi
  fi
else
  skip "C embedding" "gcc not on PATH"
fi

# ---------------------------------------------------------------- Python
printf 'Python SDK... '
if have python || have py; then
  PY=$(have python && echo python || echo "py -3")
  py_out=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
print(omni.Engine().evaluate("let answer = 40 + 2"))
' 2>&1)
  if [[ "$py_out" == *42.0* ]]; then pass "python ctypes -> $py_out"
  else fail "python printed: $py_out"; fi

  py_tests=$(cd sdks/python && $PY tests/test_ffi.py 2>&1 | tail -3 | tr '\n' ' ')
  if [[ "$py_tests" == *"OK"* ]]; then pass "python SDK tests"
  else fail "python SDK tests: $py_tests"; fi
else
  skip "Python SDK" "python not on PATH"
fi

# ---------------------------------------------------------------- Node
printf 'Node SDK... '
if have node; then
  if [[ ! -d sdks/nodejs/node_modules/koffi ]]; then
    skip "Node SDK" "run: cd sdks/nodejs && npm install koffi"
  else
    node_out=$(cd sdks/nodejs && node -e '
      const { Engine } = require("./index.js");
      const e = new Engine();
      console.log(e.evaluate("let answer = 40 + 2"));
      e.close();
    ' 2>&1)
    if [[ "$node_out" == *42* ]]; then pass "node koffi -> $node_out"
    else fail "node printed: $node_out"; fi

    node_tests=$(cd sdks/nodejs && npm test --silent 2>&1 | grep -E "^ℹ (tests|pass|fail)" | tr '\n' ' ')
    if [[ "$node_tests" == *"fail 0"* ]]; then pass "node SDK tests ($node_tests)"
    else fail "node SDK tests: $node_tests"; fi
  fi
else
  skip "Node SDK" "node not on PATH"
fi

# ---------------------------------------------------------------- report
echo
echo "== Results =="
for row in "${rows[@]}"; do echo "  $row"; done
echo
if (( failures > 0 )); then
  echo "$failures check(s) failed."
  exit 1
fi
echo "All checks that could run, passed."
