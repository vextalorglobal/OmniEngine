# OmniEngine

A custom programming language and the ecosystem around it: a Rust core with no
garbage collector, a C ABI that Python, Node, and C can call directly, and a
CLI that runs `.omni` files.

This is a **runnable engine**, not a toy skeleton. `let answer = 40 + 2`
evaluates to `42`, and `"hello, " + "world"` evaluates to `hello, world`, through
the same code path from four hosts — Rust, C, Python, and Node — and a script can
call *back* into whichever host embeds it. One crossing can carry several
programs at once (`run_many` / `runMany` / `omni_exec_many`), and the performance
numbers in this repository are measured runs, not promises: `scripts/bench.sh`
prints them for this machine. Every claim below is covered by a test that has
been run on this machine.

## Get started in 60 seconds

Every path ends at the same engine and the same bytes.

**Python** — standard library only; release wheels carry the engine.

```bash
pip install omniengine
python -c "import omni; print(omni.Engine().evaluate('40 + 2'))"   # 42.0
```

**Node** — one native dependency (`koffi`); the package carries the engine.

```bash
npm install omniengine koffi
node -e "const {Engine}=require('omniengine'); const e=new Engine(); console.log(e.evaluate('40 + 2')); e.close()"
```

**C** — download the archive for your platform from the
[releases page](https://github.com/fahim5536/OmniEngine/releases), then:

```bash
# Windows archive: MSVC import library
cl /std:c11 /I include examples\c\embed.c lib\omni.dll.lib
set PATH=%CD%\lib;%PATH% && embed.exe      # all embedding checks pass

# Linux / macOS archive: link the shared library directly
cc -std=c11 -I include examples/c/embed.c -L lib -lomni \
   -Wl,-rpath,'$ORIGIN/lib' -o embed
./embed
```

The Windows archive is built with MSVC, so its import library is
`lib\omni.dll.lib`. This machine builds with the GNU toolchain, where the same
example links against `lib\libomni.dll.a` — that is the C path tested in the
checkout (`examples/c/embed.c`).

**Rust / CLI** — the workspace is the source of truth:

```bash
cargo build -p omni-ffi
cargo run -p omni-cli -- run examples/hello.omni    # -> 42
cargo run -p omni-cli -- eval '"omni" + " " + "engine"'  # -> omni engine
```

> The first `v0.1.0` release is built by
> [`.github/workflows/release.yml`](.github/workflows/release.yml) but has not
> been published yet. Until it is, use the checkout commands under
> [Verify the checkout](#verify-the-checkout); both SDKs also install from their
> directories with `pip install sdks/python` and `npm install ./sdks/nodejs`.

## What is here

| Path | What it is |
| --- | --- |
| `crates/omni-core` | NaN-boxed 8-byte scalars, length-delimited strings, a lexer, and a fused parse-and-evaluate engine. Zero dependencies. |
| `crates/omni-ffi` | The C ABI: `omni.dll` / `libomni.so`, `include/omni.h`, and contract tests written as an external caller. |
| `crates/omni-cli` | `omni run <file>`, `omni eval <expr>`. Hand-rolled args, no clap. |
| `crates/omni-wasm` | Scaffolded wasm surface, mirroring the C ABI shape. Not built by default. |
| `sdks/python` | `omni` package on `ctypes` only — no pip dependencies at all. |
| `sdks/nodejs` | `omniengine` package on `koffi`. |
| `examples/` | `.omni` programs and `c/embed.c`. |
| `scripts/package.py` | One native archive per platform: library, import library, header, example, docs. |
| `.github/workflows/release.yml` | Tag-driven build of five platforms, platform wheels, npm tarball, PyPI and GitHub Release. |
| `docs/GUIDE.md` | The initialization and execution guide, with measured numbers. |
| `docs/RELEASING.md` | The release runbook: versions, secrets, artefacts, rollback. |
| `docs/RELEASE-0.1.0.md` | The 0.1.0 release notes: what was verified, and the license. |
| `docs/superpowers/` | The approved design specs and the implementation plan. |

## Verify the checkout

```bash
# Rust
cargo test --workspace                                  # 238 tests
cargo run -p omni-cli -- eval '"omni" + " " + "engine"'  # -> omni engine
cargo run --release -p omni-core --example bench         # the engine, measured

# C, against the hand-written header — built beside the library it loads
gcc -std=c11 -Wall -Wextra -I crates/omni-ffi/include \
    examples/c/embed.c target/debug/libomni.dll.a -o target/debug/embed
cd target/debug && ./embed           # all checks pass

# Python (stdlib only)
PYTHONPATH=sdks/python python -c "import omni; print(omni.Engine().string('\"omni engine\"'))"
# -> omni engine      (evaluate('40 + 2') gives 42.0)

# Node
cd sdks/nodejs && npm install koffi && node -e \
  "const {Engine}=require('./index.js'); const e=new Engine(); console.log(e.string('\"omni engine\"')); e.close()"
```

Or run everything at once:

```bash
./scripts/verify-all.sh   # the same bytes from every host, ABI level, memory, batch
./scripts/bench.sh        # ns/exec, crossing cost, batch speed-up -- measured
python scripts/check-versions.py   # one version, one ABI across every surface
python scripts/package.py          # a clean native archive for this platform
```

It checks that the same bytes come out of the same program on every host, that
all four surfaces agree on the ABI level, that string results do not grow memory
over 2,000 executions, that a host function called from a script answers
identically in Python and Node, and that a batch crossing answers, fails, counts,
and refuses identically in both. It also checks the DevPack surface across hosts:
the math and string builtins, a second-line error rendered as
`[Line 2, Col 13]`, `eval_str` / `eval` returning native values, and a non-zero
`last_exec_micros`. Tools that are missing are reported as `SKIP`, not as a pass.

## The standard toolkit

Eleven builtins need no host registration: `abs`, `min`, `max`, `clamp`,
`floor`, `ceil`, `round`, `len`, `trim`, `upper`, `lower`.

```python
from omni import Engine

with Engine() as engine:
    engine.eval_str("clamp(12, 0, 10) + min(1, 2) + max(3, 4)")  # 15
    engine.eval_str('upper(trim("  omni  "))')                    # 'OMNI'
    engine.eval_str('len("héllo")')                               # 6, bytes
    engine.stats().last_exec_micros                               # microseconds
```

```js
const engine = new Engine();
engine.eval('clamp(12, 0, 10) + min(1, 2) + max(3, 4)');  // 15
engine.eval('upper(trim("  omni  "))');                    // 'OMNI'
engine.eval('1 == 1');                                     // true
engine.stats().lastExecMicros;                             // microseconds
```

`eval_str` / `eval` hand back native primitives rather than a wrapper: Python
gets `int` for an integral number and `float` otherwise; Node gets `number`,
`string`, `boolean`, or `null`. A runtime failure raises `OmniError` / throws,
and the message carries the position — `[Line 2, Col 13]: type mismatch: cannot
add string to number: "x" + 2` — with a byte column that is identical in every
host.

## Calling back into the host

A script's `name(args)` call enters foreign code. The host binds it by name, with
an arity the engine checks before the callback is ever entered:

```python
engine.register_function("shout", lambda s: s.upper() + "!", arity=1)
engine.string('"say: " + shout("hey")')      # say: HEY!
```

```js
engine.registerFunction('shout', (s) => s.toUpperCase() + '!', { arity: 1 });
engine.string('"say: " + shout("hey")');     // say: HEY!
```

Numbers, booleans, strings and `nil` cross in both directions. A Python exception
or a JavaScript `throw` becomes `[Line 1, Col 1]: host function 'shout' failed:
<message>`, because an exception unwinding through the Rust frames that
called it is undefined behaviour — the same reason a callback that calls back into
its own engine gets `OMNI_ERR_REENTRANT` instead of a second mutable reference.
See [docs/GUIDE.md](docs/GUIDE.md#47-host-callbacks-the-bridge-running-backwards)
and `examples/c/embed.c` for the C surface.

## Batching: several programs, one crossing

A loop of `run` pays the FFI crossing and the arena reclamation once per program;
a batch pays both once for the whole list:

```python
engine.run_many(["1 + 1", '"two"', "shout('hey')"])
# [Result(2.0), Result('two'), Result('HEY!')]
```

```js
engine.runMany(['1 + 1', '"two"', "shout('hey')"]);
// [{ tag: 'number', value: 2 }, { tag: 'string', value: 'two' }, ...]
```

Each item is an independent program — item *N* cannot read item *N−1*'s `let` —
and a failing item does not stop the rest: `try_run_many` / `tryRunMany` return
one `(status, result, message)` per item, with the engine's text for exactly that
item. All N string results stay readable until the next execution, so a batch's
egress is bounded by its total demand, not one item's.

The measured usage guidance: a batch of one is strictly a loss, and the win
saturates fast — the crossing itself costs ~240 ns through ctypes and ~36 ns
through koffi, while one wrapped execution costs several microseconds, so a batch
can only amortise the smaller term. On this machine, batches earned 1.2–1.4× per
item at their best N in Python and Node, and in Rust the win is unconditional but
in allocations: 0.00 per execution, 0.004 per *item* inside a 256-batch. Run
`./scripts/bench.sh` for the machine you are on.

## How it stays fast and small

- **No GC, ever.** Scalars are NaN-boxed into 8 bytes; everything the engine
  allocates goes into a bump arena that is bulk-freed at the start of each
  evaluation. `reset()` is O(chunks), not O(objects), and a batch reclaims
  back to a checkpoint instead of back to the beginning.
- **A warm engine lexes without allocating.** The token vector is reused across
  executions — measured, 0.00 allocations per execution in every benchmark
  scenario, and 0.004 per item inside a 256-batch.
- **Reservations are flat by construction.** The arena cursor walks chunks it
  already mapped instead of asking the system for new ones, so 100,000
  executions reserve exactly what the first one did — checked by the harness,
  not just by the claim.
- **Three lifetimes, three blocks.** The exec arena is freed every evaluation.
  Strings handed to a host live in a separate egress block that *rewinds*
  instead of freeing, so a stale pointer reads the newest bytes rather than
  unmapped memory. Host-supplied strings sit in their own allocation, owned by
  the engine and unchanged by any evaluation.
- **Nothing leaves the engine as a pointer a host must trust.** The result struct
  is written into storage the caller owns, and its payload arm is an integer no
  host dereferences. `omni_str_copy` fills a buffer you passed in and validates
  the address against the engine's own registry, so a fabricated or superseded
  pointer copies zero bytes instead of reading somebody else's heap.
- **A callback cannot alias the engine.** The handle holds its state in a
  `RefCell` and every entry point opens it with `try_borrow_mut()`, so a host
  function that calls back into the engine that called it gets
  `OMNI_ERR_REENTRANT` — a defined status — where the unchecked shape would be
  two mutable references and a crash blamed on the host.
- **Zero dependencies in the core.** The compile graph stays small, and building
  this workspace fetched nothing into `~/.cargo` — the only file it touched is
  cargo's own metadata.
- **`debug = "line-tables-only"`.** Cuts multi-gigabyte MSVC PDBs on a drive
  with 6.5 GB free, while keeping stack traces readable.
- **`unsafe` is four files, and they are named.** In the core it is `arena.rs`
  (the bump pointer), `object.rs` (one `write_at`, the only place a string block
  is written), and `counted.rs` (the `GlobalAlloc` trait requires it); the fourth
  is the FFI. Everything else is safe Rust by construction.

## Language status

The engine understands numbers, arithmetic with precedence, parentheses,
`let` bindings scoped to one execution, line comments, and eleven builtins
(`abs`, `min`, `max`, `clamp`, `floor`, `ceil`, `round`, `len`, `trim`, `upper`,
`lower`). Strings are length-delimited: literals with `\n`, `\r`, `\t`, `\"`,
`\\` and `\u{…}` escapes, `+` concatenation that refuses a mixed-type program by
naming both operands, and `==` / `!=` by content. A host can define a string
global from outside the language, and can bind functions a script calls by name —
up to 16 arguments, up to 64 calls deep.

Errors report a 1-based line and byte column — `[Line 2, Col 13]: …` — and that
position survives the FFI boundary into C, Python, and Node. `stats()` reports
the last execution's wall-clock time in microseconds.

What is still missing: indexing, slicing, string methods and interpolation;
ordering comparisons on strings; boolean, `nil`, and `undefined` literals;
user-defined functions, control flow, and collections. The memory model and the
call machinery that make those cheap are already in place — the syntax is the
next increment.

## File extensions

`.omni` and `.ox`. See [docs/GUIDE.md](docs/GUIDE.md#5-phase-4--github-linguist-and-file-extensions)
for what GitHub shows today and what it takes to get a custom language name.

## License

Vextalor Global Public & Integration License (VGPIL) 1.0 — see [LICENSE](LICENSE).

OmniEngine is owned by Tamjid Abdur Rahman and was engineered by Vextalor
Global. You may integrate, run, and embed it — including in commercial
applications and inside Python, Node, and C packages — free of charge. You may
not sell or re-license the raw source or core binary as a standalone or
rebranded product, and attribution and this license must stay intact.

Code published before this license was applied (through commit `9aab27b`) was
released under MIT; those grants are not retroactively withdrawn.
