# OmniEngine

A custom programming language and the ecosystem around it: a Rust core with no
garbage collector, a C ABI that Python, Node, and C can call directly, and a
CLI that runs `.omni` files.

Phase 1 is a **runnable vertical slice**, not a toy skeleton. `let answer = 40 + 2`
evaluates to `42` through the same code path from four hosts — Rust, C, Python,
and Node — and every one of those claims below is covered by a test that has
been run on this machine.

```
cargo build -p omni-ffi                          # the shared library
cargo run -p omni-cli -- run examples/hello.omni # -> 42
```

## What is here

| Path | What it is |
| --- | --- |
| `crates/omni-core` | NaN-boxed 8-byte `Value`, chunked bump arena, lexer, fused parse-and-evaluate engine. Zero dependencies. |
| `crates/omni-ffi` | The C ABI: `omni.dll` / `libomni.so`, `include/omni.h`, and contract tests written as an external caller. |
| `crates/omni-cli` | `omni run <file>`, `omni eval <expr>`. Hand-rolled args, no clap. |
| `crates/omni-wasm` | Scaffolded wasm surface, mirroring the C ABI shape. Not built by default. |
| `sdks/python` | `omni` package on `ctypes` only — no pip dependencies at all. |
| `sdks/nodejs` | `omniengine` package on `koffi`. |
| `examples/` | `.omni` programs and `c/embed.c`. |
| `docs/GUIDE.md` | The initialization and execution guide, with measured numbers. |
| `docs/superpowers/` | The approved design spec and the 11-task implementation plan. |

## Try it

```bash
# Rust
cargo test --workspace                                  # 78 tests
cargo run -p omni-cli -- eval "2 + 3 * 4"               # -> 14

# C, against the hand-written header
gcc -std=c11 -Wall -Wextra -I crates/omni-ffi/include \
    examples/c/embed.c target/debug/libomni.dll.a -o embed
./embed                                                 # all checks pass

# Python (stdlib only)
PYTHONPATH=sdks/python python -c "import omni; print(omni.Engine().evaluate('40 + 2'))"

# Node
cd sdks/nodejs && npm install koffi && node -e \
  "const {Engine}=require('./index.js'); const e=new Engine(); console.log(e.evaluate('40 + 2')); e.close()"
```

Or run everything at once:

```bash
./scripts/verify-all.sh
```

## How it stays fast and small

- **No GC, ever.** Scalars are NaN-boxed into 8 bytes; everything the engine
  allocates goes into a bump arena that is bulk-freed at the start of each
  evaluation. `reset()` is O(chunks), not O(objects).
- **Zero dependencies in the core.** The compile graph stays small, and building
  this workspace fetched nothing into `~/.cargo` — the only file it touched is
  cargo's own metadata.
- **`debug = "line-tables-only"`.** Cuts multi-gigabyte MSVC PDBs on a drive
  with 7.5 GB free, while keeping stack traces readable.
- **`unsafe` is confined to two files** — `arena.rs` and the FFI — and enforced
  by a safe `Slot<T>` facade rather than by convention.

## Language status

Phase 1 understands numbers, arithmetic with precedence, parentheses,
`let` bindings scoped to one execution, and line comments. Errors report the
line they happened on, across the FFI boundary.

There are no strings, functions, control flow, or data structures yet. The
memory model that makes them cheap is already in place; the syntax for them is
Phase 2.

## File extensions

`.omni` and `.ox`. See [docs/GUIDE.md](docs/GUIDE.md#phase-4--github-linguist-and-file-extensions)
for what GitHub shows today and what it takes to get a custom language name.

## License

MIT.
