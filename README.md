# OmniEngine

A custom programming language and the ecosystem around it: a Rust core with no
garbage collector, a C ABI that Python, Node, and C can call directly, and a
CLI that runs `.omni` files.

This is a **runnable engine**, not a toy skeleton. `let answer = 40 + 2`
evaluates to `42`, and `"hello, " + "world"` evaluates to `hello, world`, through
the same code path from four hosts — Rust, C, Python, and Node — and a script can
call *back* into whichever host embeds it. Every claim below is covered by a test
that has been run on this machine.

```
cargo build -p omni-ffi                          # the shared library
cargo run -p omni-cli -- run examples/hello.omni # -> 42
cargo run -p omni-cli -- run examples/strings.omni  # -> hello, world 😀
```

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
| `docs/GUIDE.md` | The initialization and execution guide, with measured numbers. |
| `docs/superpowers/` | The approved design specs and the implementation plan. |

## Try it

```bash
# Rust
cargo test --workspace                                  # 187 tests
cargo run -p omni-cli -- eval '"omni" + " " + "engine"'  # -> omni engine

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
./scripts/verify-all.sh
```

It checks that the same bytes come out of the same program on every host, that
all four surfaces agree on the ABI level, that string results do not grow memory
over 2,000 executions, and that a host function called from a script answers
identically in Python and Node. Tools that are missing are reported as `SKIP`, not
as a pass.

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
or a JavaScript `throw` becomes `host function 'shout' failed: <message>` at the
line of the call, because an exception unwinding through the Rust frames that
called it is undefined behaviour — the same reason a callback that calls back into
its own engine gets `OMNI_ERR_REENTRANT` instead of a second mutable reference.
See [docs/GUIDE.md](docs/GUIDE.md#47-host-callbacks-the-bridge-running-backwards)
and `examples/c/embed.c` for the C surface.

## How it stays fast and small

- **No GC, ever.** Scalars are NaN-boxed into 8 bytes; everything the engine
  allocates goes into a bump arena that is bulk-freed at the start of each
  evaluation. `reset()` is O(chunks), not O(objects).
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
  with 7.6 GB free, while keeping stack traces readable.
- **`unsafe` is four files, and they are named.** In the core it is `arena.rs`
  (the bump pointer), `object.rs` (one `write_at`, the only place a string block
  is written), and `counted.rs` (the `GlobalAlloc` trait requires it); the fourth
  is the FFI. Everything else is safe Rust by construction.

## Language status

The engine understands numbers, arithmetic with precedence, parentheses,
`let` bindings scoped to one execution, and line comments. Strings are
length-delimited: literals with `\n`, `\r`, `\t`, `\"`, `\\` and `\u{…}`
escapes, `+` concatenation that refuses a mixed-type program by naming both
operands, and `==` / `!=` by content. A host can define a string global from
outside the language, and can bind functions a script calls by name — up to 16
arguments, up to 64 calls deep.

Errors report the line they happened on, and that line number survives the FFI
boundary into Python and Node.

What is still missing: string length, indexing, slicing, methods and
interpolation; ordering comparisons on strings; boolean, `nil`, and `undefined`
literals; user-defined functions, control flow, and collections. The memory model
and the call machinery that make those cheap are already in place — the syntax is
the next increment.

## File extensions

`.omni` and `.ox`. See [docs/GUIDE.md](docs/GUIDE.md#5-phase-4--github-linguist-and-file-extensions)
for what GitHub shows today and what it takes to get a custom language name.

## License

MIT.
