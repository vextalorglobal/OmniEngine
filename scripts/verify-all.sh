#!/usr/bin/env bash
# verify-all.sh -- prove that OmniEngine returns the same answer from every host.
#
#   ./scripts/verify-all.sh
#
# Rust is required. C, Python, and Node are checked only when the tool is on
# PATH, and a missing tool is reported as SKIP rather than a failure, so this
# script is usable on a machine that has not got the whole toolchain yet.
#
# Later sections take one answer through every host that is present and compare
# what came back: strings and errors, a function the host owns, one crossing for
# many programs, the DevPack surfaces, and finally the update path -- a release
# check, a refused checksum, and a binary that replaces itself. That comparison
# is the only way to catch a bridge that is internally consistent and
# externally wrong.
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

# The Phase 2 and Phase 3 programs, defined once and used by several rows below.
# They are written here rather than inline because a shell string inside $()
# around a -c string around an Omni source literal is where a backslash quietly
# goes missing, and \u has to reach the lexer as two characters.
CROSS_PROGRAM='"hello, " + "world" + "\u{1F600}"'
CROSS_WANT=$(printf 'hello, world\xF0\x9F\x98\x80')
MIX_PROGRAM='"hi" + 42'
MIX_WANT='[Line 1, Col 6]: type mismatch: cannot add string to number: "hi" + 42'
# DevPack: a runtime error on the second line must say so, and the column is a
# byte column counted from the start of the line, so the operator lands on 13.
DIAG_PROGRAM=$'let a = 1\nlet b = "x" + 2'
DIAG_WANT='[Line 2, Col 13]: type mismatch: cannot add string to number: "x" + 2'
# DevPack builtins. Kept as two programs so one row can fail on strings alone
# and the other on numbers alone instead of blaming a single fused answer.
STR_BUILTIN_PROGRAM='replace(slice("  abcdef  ", 2, 5), "c", "C") + "/" + type_of(1 == 1) + "/" + type_of("x") + "/" + type_of(contains("abc", "b"))'
STR_BUILTIN_WANT='abC/boolean/string/boolean'
NUM_BUILTIN_PROGRAM='clamp(12, 0, 10) + min(1, 2) + max(3, 4) + floor(2.9) + ceil(2.1) + round(2.5) + abs(0 - 7) + pow(2, 10) + sqrt(16)'
NUM_BUILTIN_WANT='1058'
# examples/strings.omni ends with a space before the escape, so its answer is
# not the same bytes as CROSS_WANT. Checked against its own expectation.
STRINGS_WANT=$(printf 'hello, world \xF0\x9F\x98\x80')
# Phase 3. `shout` is registered by each host in its own language, and the
# answer is the same bytes: the argument crossed in, the result crossed back
# out, and the concatenation used it as a string like any other.
CALL_PROGRAM='"say: " + shout("hey")'
CALL_WANT='say: HEY!'
BOOM_PROGRAM='boom(1)'
BOOM_WANT="host function 'boom' failed"
BOOM_DETAIL='host said no'
PEEK_PROGRAM='peek(1)'
# Both SDKs supply this text themselves, because the engine is busy and the one
# thing it cannot do is render a message about being busy.
PEEK_WANT='already inside a host function'

echo "== OmniEngine cross-host verification =="
echo "   workspace: $ROOT"
echo

# ---------------------------------------------------------------- Rust
# Run the suite once into a log: piping cargo into grep under `pipefail` makes
# the pipeline inherit cargo's exit code, which used to turn a failing suite
# into a PASS row. The log is also what the totals and the tail are read from,
# so a failure cannot be hidden by a second run that got lucky.
printf 'Rust tests... '
if cargo test --workspace >"$OUT/rust.log" 2>&1; then
  total=$(grep -oE '[0-9]+ passed' "$OUT/rust.log" | awk '{s += $1} END {print s+0}')
  pass "cargo test --workspace (${total:-0} tests)"
else
  fail "cargo test --workspace"
  tail -20 "$OUT/rust.log"
fi

printf 'Deterministic fuzz harness... '
if bash scripts/fuzz.sh --deterministic >"$OUT/fuzz.log" 2>&1; then
  summary=$(grep -oE 'fuzz harness:.*' "$OUT/fuzz.log" | tail -1 | tr -d '\r')
  pass "${summary:-deterministic fuzz harness passed}"
else
  fail "deterministic fuzz harness"
  tail -20 "$OUT/fuzz.log"
fi

printf 'Shared library... '
if cargo build -p omni-ffi >"$OUT/build.log" 2>&1; then
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
else
  fail "cargo build -p omni-ffi"
  tail -10 "$OUT/build.log"
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

# ---------------------------------------------------- Phase 3: a function the host owns
#
# A host callback is the bridge running backwards: the engine reads a script,
# hands the arguments into foreign code, and then trusts whatever that code
# writes back. Each row here checks a promise the C header makes and a single
# host cannot verify about itself -- that the answer, the failure text, and the
# refusal all come out the same on both sides of the boundary.

printf 'A host function answers the same bytes in every host... '
cb_hosts=''
cb_wrong=''
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && P="$CALL_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    e.register_function("shout", lambda s: s.upper() + "!", arity=1)
    sys.stdout.buffer.write(e.string(os.environ["P"]).encode("utf-8"))
' 2>&1)
  if [[ "$got" == "$CALL_WANT" ]]; then cb_hosts+=' python'
  else cb_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && P="$CALL_PROGRAM" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
e.registerFunction("shout", (s) => s.toUpperCase() + "!", { arity: 1 });
process.stdout.write(e.string(process.env.P));
e.close();
' 2>&1)
  if [[ "$got" == "$CALL_WANT" ]]; then cb_hosts+=' node'
  else cb_wrong+=" node='$got'"; fi
fi
if [[ -n "$cb_wrong" ]]; then
  fail "the callback answer differs:$cb_wrong"
elif [[ -n "$cb_hosts" ]]; then
  pass "an argument crossed in and a string crossed back out, identically in:$cb_hosts"
else
  skip "host callback bytes" "python and node not available"
fi

printf 'A host failure is data, and says so identically... '
boom_hosts=''
boom_wrong=''
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && P="$BOOM_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
def boom(_):
    raise ValueError("'"$BOOM_DETAIL"'")
with omni.Engine() as e:
    e.register_function("boom", boom, arity=1)
    try:
        e.run(os.environ["P"])
        print("the failing call returned no error")
    except omni.OmniError as err:
        print(str(err))
' 2>&1)
  if [[ "$got" == *"$BOOM_WANT"* && "$got" == *"$BOOM_DETAIL"* ]]; then
    boom_hosts+=' python'
  else boom_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && P="$BOOM_PROGRAM" M="$BOOM_DETAIL" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
e.registerFunction("boom", () => { throw new Error(process.env.M); }, { arity: 1 });
try { e.run(process.env.P); console.log("the failing call returned no error"); }
catch (err) { console.log(String(err)); }
e.close();
' 2>&1)
  if [[ "$got" == *"$BOOM_WANT"* && "$got" == *"$BOOM_DETAIL"* ]]; then
    boom_hosts+=' node'
  else boom_wrong+=" node='$got'"; fi
fi
if [[ -n "$boom_wrong" ]]; then
  fail "the host failure text differs or the throw escaped:$boom_wrong"
elif [[ -n "$boom_hosts" ]]; then
  pass "an exception was caught at the boundary and named in:$boom_hosts"
else
  skip "host failure text" "python and node not available"
fi

printf 'Re-entering the engine from a callback is refused, not crashed... '
re_hosts=''
re_wrong=''
if [[ -n "${PY:-}" ]]; then
  # The callback calls back into the same handle it was called from, which is
  # the one thing the borrowed arguments make unsafe. The answer must be an
  # error, and the engine must still work after it.
  got=$(cd sdks/python && P="$PEEK_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
e = omni.Engine()
def peek(_):
    return e.evaluate("1 + 1")
e.register_function("peek", peek, arity=1)
try:
    e.run(os.environ["P"])
    text = "the re-entrant call returned no error"
except omni.OmniError as err:
    text = str(err)
print(text, "|", e.evaluate("6 * 7"))
' 2>&1)
  if [[ "$got" == *"$PEEK_WANT"* && "$got" == *"42.0"* ]]; then
    re_hosts+=' python'
  else re_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && P="$PEEK_PROGRAM" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
e.registerFunction("peek", () => e.evaluate("1 + 1"), { arity: 1 });
let text;
try { e.run(process.env.P); text = "the re-entrant call returned no error"; }
catch (err) { text = String(err); }
console.log(text, "|", e.evaluate("6 * 7"));
e.close();
' 2>&1)
  if [[ "$got" == *"$PEEK_WANT"* && "$got" == *"42"* ]]; then
    re_hosts+=' node'
  else re_wrong+=" node='$got'"; fi
fi
if [[ -n "$re_wrong" ]]; then
  fail "re-entrancy was not refused-and-survived in:$re_wrong"
elif [[ -n "$re_hosts" ]]; then
  pass "a second borrow of the same handle errors and the engine lives on in:$re_hosts"
else
  skip "re-entrancy guard" "python and node not available"
fi

printf 'The registry says the same thing from both sides... '
reg_hosts=''
reg_wrong=''
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    for name in ("a", "b", "c"):
        e.register_function(name, lambda: 1.0, arity=0)
    before = e.host_function_count
    e.unregister_function("b")
    after = e.host_function_count
    again = 1 if e.unregister_function("b") else 0
    try:
        e.run("b()")
        text = "b still answered"
    except omni.OmniError as err:
        text = str(err)
    print(before, after, again, "|", text)
' 2>&1)
  if [[ "$got" == "3 2 0 |"*"unknown function 'b'"* ]]; then
    reg_hosts+=' python'
  else reg_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && node -e '
const { Engine } = require("./index.js");
const e = new Engine();
for (const name of ["a", "b", "c"]) e.registerFunction(name, () => 1, { arity: 0 });
const before = e.hostFunctionCount;
e.unregisterFunction("b");
const after = e.hostFunctionCount;
const again = e.unregisterFunction("b") ? 1 : 0;
let text;
try { e.run("b()"); text = "b still answered"; } catch (err) { text = String(err); }
console.log(before, after, again, "|", text);
e.close();
' 2>&1)
  if [[ "$got" == "3 2 0 |"*"unknown function 'b'"* ]]; then
    reg_hosts+=' node'
  else reg_wrong+=" node='$got'"; fi
fi
if [[ -n "$reg_wrong" ]]; then
  fail "the registry disagreed:$reg_wrong"
elif [[ -n "$reg_hosts" ]]; then
  pass "three registrations, one removal, then the name is unknown in:$reg_hosts"
else
  skip "registry agreement" "python and node not available"
fi

# ---------------------------------------------------- Phase 4: one crossing, many programs
#
# A batch is N independent executions that share one crossing, one arena
# reclamation and one egress rewind. What a single-host suite can check is that
# its own wrapper is internally consistent; these rows check what it cannot --
# that the items, the per-item errors, the counter and the re-entrancy refusal
# come out identically on both sides of the bridge. The C host's batch contract
# is asserted by examples/c/embed.c above; every row here also holds because
# embed.c already passed.

printf 'A batch answers the same items in every host... '
batch_hosts=''
batch_wrong=''
if [[ -n "${PY:-}" ]]; then
  # Written through stdout.buffer, not print: the answer carries a 4-byte code
  # point, and a non-UTF-8 Python stdout would fail on it for reasons that have
  # nothing to do with the engine.
  got=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    results = e.run_many(["1 + 1", "\"hello, \" + \"world\"", "\"\\u{1F600}\" + \"!\"", "20 / 3"])
    line = "|".join(str(r.value) for r in results)
    sys.stdout.buffer.write(line.encode("utf-8"))
' 2>&1)
  # Python renders the language's doubles as Python floats: 2.0, not 2.
  if [[ "$got" == "2.0|hello, world|"*"!|6.666666666666667" ]]; then batch_hosts+=' python'
  else batch_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && node -e '
const { Engine } = require("./index.js");
const e = new Engine();
const results = e.runMany(["1 + 1", "\"hello, \" + \"world\"", "\"\\u{1F600}\" + \"!\"", "20 / 3"]);
console.log(results.map((r) => r.value).join("|"));
e.close();
' 2>&1)
  if [[ "$got" == "2|hello, world|"*"!|6.666666666666667" ]]; then batch_hosts+=' node'
  else batch_wrong+=" node='$got'"; fi
fi
if [[ -n "$batch_wrong" ]]; then
  fail "the batch answers differ:$batch_wrong"
elif [[ -n "$batch_hosts" ]]; then
  pass "four items, one crossing, the same values in:$batch_hosts"
else
  skip "batch answers" "python and node not available"
fi

printf 'A failing item names itself identically in every host... '
bfail_hosts=''
bfail_wrong=''
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    items = e.try_run_many(["1 + 1", "\"a\" + 1", "3 * 3", "nothere"])
    print(" ".join(str(s) for s, _, _ in items))
    print("|", items[1][2], "|", items[3][2])
' 2>&1)
  if [[ "$got" == "0 2 0 2"*"| [Line 1, Col 5]: type mismatch: cannot add string to number: \"a\" + 1 | [Line 1, Col 1]: undefined variable 'nothere'"* ]]; then
    bfail_hosts+=' python'
  else bfail_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && node -e '
const { Engine } = require("./index.js");
const e = new Engine();
const items = e.tryRunMany(["1 + 1", "\"a\" + 1", "3 * 3", "nothere"]);
console.log(items.map((i) => i.status).join(" "));
console.log("|", items[1].message, "|", items[3].message);
e.close();
' 2>&1)
  if [[ "$got" == "0 2 0 2"*"| [Line 1, Col 5]: type mismatch: cannot add string to number: \"a\" + 1 | [Line 1, Col 1]: undefined variable 'nothere'"* ]]; then
    bfail_hosts+=' node'
  else bfail_wrong+=" node='$got'"; fi
fi
if [[ -n "$bfail_wrong" ]]; then
  fail "the per-item failure differs:$bfail_wrong"
elif [[ -n "$bfail_hosts" ]]; then
  pass "statuses 0 2 0 2 and the engine text for items 1 and 3, identically in:$bfail_hosts"
else
  skip "batch failure text" "python and node not available"
fi

printf 'A batch counts items, not crossings, and reserves nothing extra... '
bcnt_hosts=''
bcnt_wrong=''
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    e.run_many(["6 * 7"])               # warm: the first crossing maps the blocks
    before = e.stats()
    for _ in range(1000):
        e.run_many(["6 * 7"] * 8)
    after = e.stats()
    print(before.egress_bytes_reserved, after.egress_bytes_reserved, after.execs - before.execs, sep=" ")
' 2>&1)
  read -r before after execs <<< "$got"
  if [[ "$before" == "$after" && "$execs" == "8000" ]]; then bcnt_hosts+=" python($after B)"
  else bcnt_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && node -e '
const { Engine } = require("./index.js");
const e = new Engine();
e.runMany(["6 * 7"]);
const before = e.stats();
for (let i = 0; i < 1000; i++) e.runMany(Array(8).fill("6 * 7"));
const after = e.stats();
console.log(before.egressBytesReserved, after.egressBytesReserved, after.execs - before.execs);
e.close();
' 2>&1)
  read -r before after execs <<< "$got"
  if [[ "$before" == "$after" && "$execs" == "8000" ]]; then bcnt_hosts+=" node($after B)"
  else bcnt_wrong+=" node='$got'"; fi
fi
if [[ -n "$bcnt_wrong" ]]; then
  fail "the batch counter or reservation disagreed:$bcnt_wrong"
elif [[ -n "$bcnt_hosts" ]]; then
  pass "8000 items in 1000 crossings, execs +8000, reservation flat:$bcnt_hosts"
else
  skip "batch accounting" "python and node not available"
fi

printf 'A batch inside a host callback is refused, not crashed... '
bre_hosts=''
bre_wrong=''
if [[ -n "${PY:-}" ]]; then
  # The callback borrows the engine's state; a batch crossing from inside it is
  # the same re-entrancy as a single exec, refused with the same words.
  got=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
e = omni.Engine()
def peek(_):
    return e.run_many(["1 + 1"])[0].value
e.register_function("peek", peek, arity=1)
try:
    e.run("peek(1)")
    text = "the re-entrant batch returned no error"
except omni.OmniError as err:
    text = str(err)
print(text, "|", e.evaluate("6 * 7"))
' 2>&1)
  if [[ "$got" == *"$PEEK_WANT"* && "$got" == *"42.0"* ]]; then
    bre_hosts+=' python'
  else bre_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && node -e '
const { Engine } = require("./index.js");
const e = new Engine();
e.registerFunction("peek", () => e.runMany(["1 + 1"])[0].value, { arity: 1 });
let text;
try { e.run("peek(1)"); text = "the re-entrant batch returned no error"; }
catch (err) { text = String(err); }
console.log(text, "|", e.evaluate("6 * 7"));
e.close();
' 2>&1)
  if [[ "$got" == *"$PEEK_WANT"* && "$got" == *"42"* ]]; then
    bre_hosts+=' node'
  else bre_wrong+=" node='$got'"; fi
fi
if [[ -n "$bre_wrong" ]]; then
  fail "the re-entrant batch was not refused-and-survived in:$bre_wrong"
elif [[ -n "$bre_hosts" ]]; then
  pass "a batch from a borrowed handle errors with the shared refusal text in:$bre_hosts"
else
  skip "batch re-entrancy guard" "python and node not available"
fi

# ---------------------------------------------------- Phase 5: the DevPack surfaces
#
# The builtins, the positioned diagnostics, the native-value sugar, and the
# execution timer are each asserted by a single host's suite. These rows check
# what those suites cannot: that a program built from the new builtins reduces
# to the same value everywhere, that a second-line error renders the same
# line/column text in every host, and that the two SDKs hand back values a host
# can use without unwrapping anything.

printf 'The DevPack math, string and type builtins answer the same in every host... '
builtin_hosts=''
builtin_wrong=''
if have cargo; then
  s=$(cargo run -q -p omni-cli -- eval "$STR_BUILTIN_PROGRAM" 2>&1)
  n=$(cargo run -q -p omni-cli -- eval "$NUM_BUILTIN_PROGRAM" 2>&1)
  if [[ "$s" == "$STR_BUILTIN_WANT" && "$n" == "$NUM_BUILTIN_WANT" ]]; then builtin_hosts+=' cli'
  else builtin_wrong+=" cli='$s|$n'"; fi
fi
if [[ -n "${PY:-}" ]]; then
  # eval_str folds the integral float to a Python int, so 30 prints as "30"
  # and not "30.0" while still being a value ordinary Python arithmetic takes.
  got=$(cd sdks/python && S="$STR_BUILTIN_PROGRAM" N="$NUM_BUILTIN_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    print(e.eval_str(os.environ["S"]), "|", e.eval_str(os.environ["N"]))
' 2>&1)
  if [[ "$got" == "$STR_BUILTIN_WANT | $NUM_BUILTIN_WANT" ]]; then builtin_hosts+=' python'
  else builtin_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && S="$STR_BUILTIN_PROGRAM" N="$NUM_BUILTIN_PROGRAM" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
console.log(e.eval(process.env.S), "|", e.eval(process.env.N));
e.close();
' 2>&1)
  if [[ "$got" == "$STR_BUILTIN_WANT | $NUM_BUILTIN_WANT" ]]; then builtin_hosts+=' node'
  else builtin_wrong+=" node='$got'"; fi
fi
if [[ -n "$builtin_wrong" ]]; then
  fail "the builtin answers differ:$builtin_wrong"
elif [[ -n "$builtin_hosts" ]]; then
  pass "math, string and type builtins reduce identically in:$builtin_hosts"
else
  skip "builtin answers" "no host available"
fi

printf 'A second-line error names line and byte column in every host... '
diag_hosts=''
diag_wrong=''
if have cargo; then
  got=$(cargo run -q -p omni-cli -- eval "$DIAG_PROGRAM" 2>&1)
  if [[ "$got" == *"$DIAG_WANT"* ]]; then diag_hosts+=' cli'
  else diag_wrong+=" cli='$got'"; fi
fi
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && P="$DIAG_PROGRAM" $PY -c '
import os, sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    try:
        e.run(os.environ["P"])
    except omni.OmniError as err:
        print(str(err))
' 2>&1)
  if [[ "$got" == *"$DIAG_WANT"* ]]; then diag_hosts+=' python'
  else diag_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && P="$DIAG_PROGRAM" node -e '
const { Engine } = require("./index.js");
const e = new Engine();
try { e.run(process.env.P); } catch (err) { console.log(String(err)); }
e.close();
' 2>&1)
  if [[ "$got" == *"$DIAG_WANT"* ]]; then diag_hosts+=' node'
  else diag_wrong+=" node='$got'"; fi
fi
if [[ -n "$diag_wrong" ]]; then
  fail "the position text differs:$diag_wrong"
elif [[ -n "$diag_hosts" ]]; then
  pass "line 2, column 13, and the operator's message in:$diag_hosts"
else
  skip "position text" "no host available"
fi

printf 'eval_str and eval hand back native values... '
native_hosts=''
native_wrong=''
if [[ -n "${PY:-}" ]]; then
  got=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    out = []
    for src in ("40 + 2", "\"hi\"", "1 == 1"):
        v = e.eval_str(src)
        out.append(f"{v} {type(v).__name__}")
    print("|".join(out))
' 2>&1)
  if [[ "$got" == "42 int|hi str|True bool" ]]; then native_hosts+=' python'
  else native_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && node -e '
const { Engine } = require("./index.js");
const e = new Engine();
const out = ["40 + 2", "\"hi\"", "1 == 1"].map((src) => {
  const v = e.eval(src);
  return `${v} ${typeof v}`;
});
console.log(out.join("|"));
e.close();
' 2>&1)
  if [[ "$got" == "42 number|hi string|true boolean" ]]; then native_hosts+=' node'
  else native_wrong+=" node='$got'"; fi
fi
if [[ -n "$native_wrong" ]]; then
  fail "the sugar did not return native values:$native_wrong"
elif [[ -n "$native_hosts" ]]; then
  pass "int/number, str/string, bool/boolean with no wrapper in:$native_hosts"
else
  skip "native-value sugar" "python and node not available"
fi

printf 'The last execution timer crosses as microseconds... '
timer_hosts=''
timer_wrong=''
if [[ -n "${PY:-}" ]]; then
  # A 200 KB literal spends measurable time in the lexer; a small program can
  # legitimately time 0 on a coarse clock, so the row needs a program that is
  # too big to round away.
  got=$(cd sdks/python && $PY -c '
import sys; sys.path.insert(0, ".")
import omni
with omni.Engine() as e:
    e.run("\"" + "x" * 200000 + "\"")
    s = e.stats()
    print(s.last_exec_micros > 0, s.last_exec_micros)
' 2>&1)
  read -r ok us <<< "$got"
  if [[ "$ok" == "True" && "${us:-0}" -gt 0 ]]; then timer_hosts+=" python(${us}us)"
  else timer_wrong+=" python='$got'"; fi
fi
if have node && [[ -d sdks/nodejs/node_modules/koffi ]]; then
  got=$(cd sdks/nodejs && node -e '
const { Engine } = require("./index.js");
const e = new Engine();
e.run("\"" + "x".repeat(200000) + "\"");
const s = e.stats();
console.log(s.lastExecMicros > 0, s.lastExecMicros);
e.close();
' 2>&1)
  read -r ok us <<< "$got"
  if [[ "$ok" == "true" && "${us:-0}" -gt 0 ]]; then timer_hosts+=" node(${us}us)"
  else timer_wrong+=" node='$got'"; fi
fi
if [[ -n "$timer_wrong" ]]; then
  fail "the timer did not reach the host:$timer_wrong"
elif [[ -n "$timer_hosts" ]]; then
  pass "one 200 KB execution timed, non-zero and in microseconds:$timer_hosts"
else
  skip "exec timer" "python and node not available"
fi

# ---------------------------------------------------- Phase 6: staying current
# A release check reads one manifest; an update replaces one file. Both are
# exercised against file:// fixtures so the rows say nothing about the network
# and everything about the bytes: what the exit code was, and whether the
# target is still byte-identical when verification refused.

update_exe=""
case "$OSTYPE" in
  msys*|cygwin*|win32*) update_exe=".exe" ;;
esac
OMNI_BIN="$ROOT/target/debug/omni$update_exe"
OMNIUP_BIN="$ROOT/target/debug/omniup$update_exe"
UPDATE_DIR="$OUT/update"
mkdir -p "$UPDATE_DIR"

sha_of() {
  if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Git Bash rewrites an argument that looks like an absolute path, and
# `file:///D:/...` looks like one to it: without this the engine receives
# `/d/...` and the fetch fails for a reason that has nothing to do with the
# updater. The exclusion is per-command, so nothing else in this script changes.
update_cmd() {
  MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' "$@"
}

host_platform() {
  local machine
  machine=$(uname -m 2>/dev/null || echo unknown)
  case "$OSTYPE" in
    msys*|cygwin*|win32*) echo win32-x64 ;;
    darwin*)
      case "$machine" in arm64|aarch64) echo darwin-arm64 ;; *) echo darwin-x64 ;; esac ;;
    *)
      case "$machine" in aarch64|arm64) echo linux-arm64 ;; *) echo linux-x64 ;; esac ;;
  esac
}

as_url() {
  local path="$1"
  case "$OSTYPE" in
    msys*|cygwin*|win32*)
      # $PWD in Git Bash is a POSIX path (/d/...), which is not a URL the ABI can
      # open; cygpath -m gives the Windows spelling (D:/...) to paste after the
      # scheme.
      if have cygpath; then path=$(cygpath -m "$path"); fi
      path=$(printf '%s' "$path" | sed 's|\\|/|g')
      case "$path" in
        [A-Za-z]:/*) printf 'file:///%s' "$path" ;;
        *) printf 'file:///%s' "${path#/}" ;;
      esac
      ;;
    *) printf 'file://%s' "$path" ;;
  esac
}

# The CLI is a native program: a --target it is asked to replace must be spelled
# the way Windows spells it, not the way Git Bash does.
native_path() {
  case "$OSTYPE" in
    msys*|cygwin*|win32*) if have cygpath; then cygpath -m "$1"; else printf '%s' "$1"; fi ;;
    *) printf '%s' "$1" ;;
  esac
}

write_manifest() { # file, version, abi, asset URL, sha256
  cat >"$1" <<JSON
{
  "version": "$2",
  "abi": $3,
  "notes": "verify-all",
  "assets": {
    "$PLATFORM": { "url": "$4", "sha256": "$5" }
  }
}
JSON
}

printf 'omni --version names what it speaks... '
if have cargo; then
  if cargo build -q -p omni-cli --bins >"$OUT/cli-bins.log" 2>&1; then
    abi_level=$(sed -n 's/^#define OMNI_ABI_VERSION \([0-9][0-9]*\).*/\1/p' crates/omni-ffi/include/omni.h | head -1)
    version=$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version *=/{gsub(/[" ]/,""); split($0,p,"="); print p[2]; exit}' Cargo.toml)
    ver=$(update_cmd "$OMNI_BIN" --version 2>&1)
    if [[ "$ver" == *"omni $version"* && "$ver" == *"ABI $abi_level"* ]]; then
      pass "omni --version -> $ver"
    else
      fail "omni --version printed: $ver"
    fi
  else
    fail "cargo build -p omni-cli --bins"
    tail -5 "$OUT/cli-bins.log"
  fi
else
  skip "omni --version" "cargo not on PATH"
fi

if have cargo && [[ -x "$OMNI_BIN" && -x "$OMNIUP_BIN" && -n "${version:-}" && -n "${abi_level:-}" ]]; then
  PLATFORM=$(host_platform)
  payload="$UPDATE_DIR/payload$update_exe"
  cp -f "$OMNI_BIN" "$payload"
  # A trailing byte changes the checksum without changing the image, so a row
  # can tell "the update wrote these bytes" from "nothing happened".
  printf 'verify
' >>"$payload"
  payload_sha=$(sha_of "$payload")
  payload_url=$(as_url "$payload")
  write_manifest "$UPDATE_DIR/same.json" "$version" "$abi_level" "$payload_url" "$payload_sha"
  write_manifest "$UPDATE_DIR/newer.json" "99.0.0" "$abi_level" "$payload_url" "$payload_sha"
  write_manifest "$UPDATE_DIR/other-abi.json" "99.0.0" "$((abi_level + 1))" "$payload_url" "$payload_sha"
  write_manifest "$UPDATE_DIR/bad-sha.json" "99.0.0" "$abi_level" "$payload_url" "$(printf '0%.0s' $(seq 64))"

  printf 'a build that matches the release... '
  out=$(update_cmd "$OMNI_BIN" check-update --manifest "$(as_url "$UPDATE_DIR/same.json")" 2>&1)
  code=$?
  if [[ $code -eq 0 && "$out" == *"up to date"* ]]; then
    pass "omni check-update -> 0, $out"
  else
    fail "omni check-update on an equal version exited $code: $out"
  fi

  printf 'a newer release is offered... '
  out=$(update_cmd "$OMNI_BIN" check-update --manifest "$(as_url "$UPDATE_DIR/newer.json")" 2>&1)
  code=$?
  if [[ $code -eq 10 && "$out" == *"99.0.0"* ]]; then
    pass "omni check-update -> 10, $out"
  else
    fail "omni check-update on a newer version exited $code: $out"
  fi

  printf 'another ABI is refused, not offered... '
  out=$(update_cmd "$OMNI_BIN" check-update --manifest "$(as_url "$UPDATE_DIR/other-abi.json")" 2>&1)
  code=$?
  if [[ $code -eq 11 && "$out" == *"ABI"* ]]; then
    pass "omni check-update -> 11, $out"
  else
    fail "omni check-update on another ABI exited $code: $out"
  fi

  printf 'a wrong checksum leaves the target alone... '
  target="$UPDATE_DIR/target$update_exe"
  # The target is a different program from the payload, so the checksum row
  # can prove nothing was written rather than compare a file with itself.
  cp -f "$OMNIUP_BIN" "$target"
  before=$(sha_of "$target")
  out=$(update_cmd "$OMNIUP_BIN" update --manifest "$(as_url "$UPDATE_DIR/bad-sha.json")" --target "$(native_path "$target")" --yes 2>&1)
  code=$?
  after=$(sha_of "$target")
  if [[ $code -eq 3 && "$before" == "$after" ]]; then
    pass "omniup update -> 3, target unchanged"
  else
    fail "omniup update with a wrong checksum exited $code and target changed: $before -> $after ($out)"
  fi

  printf 'a dry run downloads nothing... '
  write_manifest "$UPDATE_DIR/dry.json" "99.0.0" "$abi_level" "$(as_url "$UPDATE_DIR/never-downloaded")" "$payload_sha"
  out=$(update_cmd "$OMNIUP_BIN" update --manifest "$(as_url "$UPDATE_DIR/dry.json")" --target "$(native_path "$target")" --dry-run --yes 2>&1)
  code=$?
  if [[ $code -eq 10 && ! -f "$UPDATE_DIR/never-downloaded" ]]; then
    pass "omniup update --dry-run -> 10, nothing fetched"
  else
    fail "omniup update --dry-run exited $code: $out"
  fi

  printf 'a verified update replaces the bytes... '
  out=$(update_cmd "$OMNIUP_BIN" update --manifest "$(as_url "$UPDATE_DIR/newer.json")" --target "$(native_path "$target")" --yes 2>&1)
  code=$?
  if [[ $code -eq 0 && "$(sha_of "$target")" == "$payload_sha" ]]; then
    pass "omniup update -> 0, target now the release bytes"
  else
    fail "omniup update exited $code: $out"
  fi
  if [[ -x "$target" ]]; then
    replaced=$(update_cmd "$target" --version 2>&1)
    if [[ "$replaced" == *"omni $version"* ]]; then
      pass "the replaced binary still runs: $replaced"
    else
      fail "the replaced binary printed: $replaced"
    fi
  fi

  printf 'a running binary updates itself... '
  self_dir="$UPDATE_DIR/self"
  mkdir -p "$self_dir"
  cp -f "$OMNI_BIN" "$self_dir/omni$update_exe"
  out=$(update_cmd "$self_dir/omni$update_exe" update --manifest "$(as_url "$UPDATE_DIR/newer.json")" --yes 2>&1)
  code=$?
  if [[ $code -eq 0 && "$(sha_of "$self_dir/omni$update_exe")" == "$payload_sha" ]]; then
    if [[ "$update_exe" == ".exe" && -f "$self_dir/omni$update_exe.bak" ]]; then
      pass "omni update -> 0, the running image is kept at omni.exe.bak"
    elif [[ "$update_exe" == "" ]]; then
      pass "omni update -> 0, the old image was released"
    else
      fail "omni update -> 0 but the Windows backup is missing"
    fi
  else
    fail "self-update exited $code: $out"
  fi
else
  skip "release check and update" "cargo or the built CLI binaries are unavailable"
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
