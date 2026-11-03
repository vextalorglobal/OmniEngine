#!/usr/bin/env bash
# verify-all.sh -- prove that OmniEngine returns the same answer from every host.
#
#   ./scripts/verify-all.sh
#
# Rust is required. C, Python, and Node are checked only when the tool is on
# PATH, and a missing tool is reported as SKIP rather than a failure, so this
# script is usable on a machine that has not got the whole toolchain yet.
#
# The last four rows are Phase 2: they run one string program, one type error,
# and the ABI level through every host that is present and compare the answers
# with each other, which is the only way to catch a bridge that is internally
# consistent and externally wrong.
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

# The Phase 2 programs, defined once and used by several rows below. They are
# written here rather than inline because a shell string inside $() around a -c
# string around an Omni source literal is where a backslash quietly goes
# missing, and \u has to reach the lexer as two characters.
CROSS_PROGRAM='"hello, " + "world" + "\u{1F600}"'
CROSS_WANT=$(printf 'hello, world\xF0\x9F\x98\x80')
MIX_PROGRAM='"hi" + 42'
MIX_WANT='line 1: cannot add string to number: "hi" + 42'
# examples/strings.omni ends with a space before the escape, so its answer is
# not the same bytes as CROSS_WANT. Checked against its own expectation.
STRINGS_WANT=$(printf 'hello, world \xF0\x9F\x98\x80')

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

  # The string example exercises the paths `hello.omni` does not: escapes, the
  # egress block, and rendering a boxed value instead of a NaN-boxed scalar.
  str_out=$(cargo run -q -p omni-cli -- run examples/strings.omni 2>&1)
  if [[ "$str_out" == "$STRINGS_WANT" ]]; then pass "omni run examples/strings.omni -> $str_out"
  else fail "omni-cli strings example printed: $str_out"; fi
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
  py_count=$(printf '%s' "$py_tests" | grep -oE 'Ran [0-9]+ test' | grep -oE '[0-9]+')
  if [[ "$py_tests" == *"OK"* ]]; then pass "python SDK tests (${py_count:-?} tests)"
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

# ---------------------------------------------------- Phase 2: the bridge agreeing with itself
#
# Each suite above checks one host against itself. These rows check what no
# single suite can: that independent surfaces return the SAME BYTES. An escape
# decoded one way in Rust and another way in Node passes both test suites and is
# still a broken bridge, so every comparison here is exact rather than "it did
# not crash". The programs themselves are defined near the top of the script.
printf 'A string is the same bytes in every host... '
cross_hosts=''
cross_wrong=''
if have cargo; then
  got=$(cargo run -q -p omni-cli -- eval "$CROSS_PROGRAM" 2>&1)
  if [[ "$got" == "$CROSS_WANT" ]]; then cross_hosts+=' cli'
  else cross_wrong+=" cli='$got'"; fi
fi
if [[ -n "${PY:-}" ]]; then
  # stdout.buffer, not print: a redirected Python stdout on Windows uses the
  # ANSI code page, and a 4-byte code point would fail to encode for reasons
  # that have nothing to do with the engine.
  got=$(cd sdks/python && P="$CROSS_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    sys.stdout.buffer.write(e.string(os.environ["P"]).encode("utf-8"))
' 2>&1)
  if [[ "$got" == "$CROSS_WANT" ]]; then cross_hosts+=' python'
  else cross_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && P="$CROSS_PROGRAM" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
process.stdout.write(e.string(process.env.P));
e.close();
' 2>&1)
  if [[ "$got" == "$CROSS_WANT" ]]; then cross_hosts+=' node'
  else cross_wrong+=" node='$got'"; fi
fi
if [[ -n "$cross_wrong" ]]; then
  fail "string bytes differ in:$cross_wrong"
elif [[ -n "$cross_hosts" ]]; then
  pass "concat + \\u{...} identical in:$cross_hosts"
else
  skip "cross-host string" "no cargo, python, or node available"
fi

printf 'One type error, the same text everywhere... '
mix_hosts=''
mix_wrong=''
if have cargo; then
  got=$(cargo run -q -p omni-cli -- eval "$MIX_PROGRAM" 2>&1)
  if [[ "$got" == *"$MIX_WANT"* ]]; then mix_hosts+=' cli'
  else mix_wrong+=" cli='$got'"; fi
fi
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && P="$MIX_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    try:
        e.run(os.environ["P"])
    except omni.OmniError as err:
        print(str(err))
' 2>&1)
  if [[ "$got" == *"$MIX_WANT"* ]]; then mix_hosts+=' python'
  else mix_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && P="$MIX_PROGRAM" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
try { e.run(process.env.P); } catch (err) { console.log(String(err)); }
e.close();
' 2>&1)
  if [[ "$got" == *"$MIX_WANT"* ]]; then mix_hosts+=' node'
  else mix_wrong+=" node='$got'"; fi
fi
# embed.c prints the engine's message verbatim, so the C side can be checked
# against the same text without a second binary.
if [[ -n "${c_out:-}" ]]; then
  if [[ "$c_out" == *"$MIX_WANT"* ]]; then mix_hosts+=' c'
  else mix_wrong+=" c(see embed output above)"; fi
fi
if [[ -n "$mix_wrong" ]]; then
  fail "error text differs in:$mix_wrong"
elif [[ -n "$mix_hosts" ]]; then
  pass "the mixed-type message survives the boundary in:$mix_hosts"
else
  skip "cross-host error text" "no host available"
fi

printf 'Every surface agrees on the ABI level... '
levels=()
hdr=$(grep -oE '^#define[[:space:]]+OMNI_ABI_VERSION[[:space:]]+[0-9]+' \
  crates/omni-ffi/include/omni.h | grep -oE '[0-9]+$')
[[ -n "$hdr" ]] && levels+=("header=$hdr")
if [[ -n "${c_out:-}" ]]; then
  n=$(printf '%s\n' "$c_out" | grep -m1 -oE '\(ABI [0-9]+\)' | grep -oE '[0-9]+')
  [[ -n "$n" ]] && levels+=("C=$n")
fi
if [[ -n "${PY:-}" ]]; then
  n=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    print(omni.OMNI_ABI_VERSION, e.abi_version)
' 2>&1)
  # The SDK's own constant and the number the library reports: equal is the
  # whole contract, and a stale artifact is what makes them differ.
  [[ "$n" == *" "* ]] && levels+=("python=${n% *}/lib=${n#* }")
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  n=$(cd sdks/nodejs && node -e '
const b = require("./lib/bindings.js");
const { Engine } = require("./index.js");
const e = new Engine();
console.log(b.ABI_VERSION, e.abiVersion);
e.close();
' 2>&1)
  [[ "$n" == *" "* ]] && levels+=("node=${n% *}/lib=${n#* }")
fi
distinct=$(for v in "${levels[@]##*=}"; do printf '%s\n' "$v"; done | sort -u | wc -l)
if (( ${#levels[@]} >= 2 )) && (( distinct == 1 )); then
  pass "ABI ${levels[0]#*=} declared and read identically: ${levels[*]}"
elif (( distinct > 1 )); then
  fail "ABI levels disagree: ${levels[*]}"
else
  skip "ABI agreement" "need the header plus at least one host"
fi

printf 'String results do not grow memory... '
grow_hosts=''
grow_wrong=''
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && P="$CROSS_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    prog = os.environ["P"]
    e.run(prog)                      # warm: the first exec maps the blocks
    before = e.stats()
    for _ in range(2000):
        e.run(prog)
    after = e.stats()
    print(before.egress_bytes_reserved, after.egress_bytes_reserved,
          after.execs - before.execs, sep=" ")
' 2>&1)
  read -r before after execs <<< "$got"
  if [[ "$before" == "$after" && "$execs" == "2000" ]]; then grow_hosts+=" python($after B)"
  else grow_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && P="$CROSS_PROGRAM" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
const prog = process.env.P;
e.run(prog);
const before = e.stats();
for (let i = 0; i < 2000; i++) e.run(prog);
const after = e.stats();
console.log(before.egressBytesReserved, after.egressBytesReserved, after.execs - before.execs);
e.close();
' 2>&1)
  read -r before after execs <<< "$got"
  if [[ "$before" == "$after" && "$execs" == "2000" ]]; then grow_hosts+=" node($after B)"
  else grow_wrong+=" node='$got'"; fi
fi
if [[ -n "$grow_wrong" ]]; then
  fail "egress grew or the counters disagreed:$grow_wrong"
elif [[ -n "$grow_hosts" ]]; then
  pass "2000 string executions, one rewind each, reservation flat:$grow_hosts"
else
  skip "egress bounded from a host" "python and node not available"
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
