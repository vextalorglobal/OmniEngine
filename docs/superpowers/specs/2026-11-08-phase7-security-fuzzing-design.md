# Phase 7 — Hardened Security, Continuous Fuzzing, Zero Regression

**Status:** implemented
**Scope:** `omni-core` only. ABI 4, language version 2, and every host SDK
stay byte-compatible; the new limits are Rust-level policy like the existing
module and filesystem permissions.

## 1. The problem

Phase 6 gave the VM a real instruction set and a module loader. Two resource
classes were still unbounded:

- **Time.** `Jump` can target a lower instruction index, so a hand-built chunk
  (or a future `while`) can loop forever. A host running untrusted source has no
  way to say "stop after N instructions".
- **Memory.** Every operand push grew the `Vec<Value>` without a cap. A chunk
  that pushes in a loop grows until the allocator fails — a process abort, not a
  positioned error.
- **Native stack.** Parentheses recurse through `expr → primary → expr`. Call
  nesting had `MAX_CALL_DEPTH`; grouping did not, so `((((…1…))))` overflowed
  the thread stack during parsing.

The sandbox also had one fail-open edge: `fs.write_text` canonicalized the
parent of a path that failed to canonicalize. A *dangling symlink* inside the
root therefore passed the containment check and `fs::write` followed it to a
target outside the sandbox.

Finally, none of the pipeline was fuzzable from outside the crate: `lex_into`
was public, but parsing, lowering, and VM decoding were crate-private.

## 2. Execution budget (gas)

`Budget` is created once per top-level execution and holds a hard instruction
count:

```text
used >= limit  →  [Line L, Col C]: execution exceeded the step limit of N instructions
```

- `Engine::set_step_limit(u64)` sets the budget; `0` disables the check.
- `DEFAULT_STEP_LIMIT = 16_777_216` (2^24) instructions. It is far above any
  realistic program and still converts an infinite loop into a failure in well
  under a second in release builds.
- The budget is charged before every dispatched instruction, so the position is
  the instruction that would have run next.
- `exec` and each `exec_many` item create a fresh budget. A module does **not**:
  `import` passes the parent's `&mut Budget` into the child engine's internal
  `exec_with_budget`, so `parent + modules` share one budget. Without that, a
  parent could import the same expensive module repeatedly and reset the count
  each time.
- The limit is policy, not ABI. C, Python, and Node hosts keep working with the
  default; exposing a setter through the FFI would be its own ABI revision.

Determinism: the budget is a counter, not wall-clock, so a given program fails
at the same instruction with the same message on every host and machine.

## 3. Operand-stack bound

`MAX_STACK_DEPTH = 65_536` values (512 KiB). Every push in `run_chunk` goes
through one helper that checks the length first and returns a positioned
`Runtime` error instead of growing the vector:

```text
[Line L, Col C]: the VM stack reached its 65536-value limit
```

The compiler cannot exceed this bound for well-formed source: arguments are
capped at `MAX_ARGS` and calls at `MAX_CALL_DEPTH`. The check exists for chunks
built by hand, by a fuzzer, or by a future tool.

## 4. Parser nesting bound

`MAX_NESTING = 128` counts `expr()` entries. The counter is incremented on entry
and always decremented, including on the error path:

```text
[Line L, Col C]: expressions are nested 128 deep, the limit
```

This bounds the parser's native recursion independently of `MAX_CALL_DEPTH`.
The two limits compose additively (at worst ~192 expression levels), which is
comfortably inside a 1 MiB thread stack.

## 5. Filesystem fail-closed rule

`write_allowed` now branches on `symlink_metadata`, which does not follow the
final component:

| Path state | Decision |
| --- | --- |
| Entry exists (file, directory, or symlink — including dangling) | `canonicalize()` must succeed; failure is the error, and containment is checked on the canonical target |
| Nothing exists (`NotFound`) | Canonicalize the parent, append the new file name, check containment |
| Any other metadata error | Refuse with the OS error |

There is no fallback from a failed canonicalization of an existing entry. A
dangling symlink to `outside/secret` resolves nowhere, so the write is refused
and the outside file is never touched. `read_allowed` already required
canonicalization and needed no change. Both directories remain deny-by-default
when their root lists are empty.

JSON is unchanged in behavior — the scalar parser is a bounded forward scan —
and is now covered by the fuzz corpus, including escape and surrogate paths.

## 6. Fuzz surface

A hidden module, `fuzz_api`, exposes the four stages to a fuzz binary in another
crate:

| Function | Stage |
| --- | --- |
| `lex_only(&str)` | lexer |
| `parse_only(&str)` | lexer + AST parser |
| `lower_only(&str)` | lexer + parser + bytecode lowering |
| `execute(&str)` | the whole engine, including the VM |
| `execute_bytes(&[u8])` | instruction decoder: 8 bytes → one `Instr`, executed over the same bytes as source |

`execute_bytes` maps every byte to a valid opcode (`byte % 20`) so no input is
wasted on an out-of-range discriminant, and every span is read through
`str::get`, so a corrupt index is an empty literal rather than a panic.

### cargo-fuzz project

`crates/omni-core/fuzz` is its own workspace with `libfuzzer-sys` and four
targets: `fuzz_lex`, `fuzz_parse`, `fuzz_lower`, `fuzz_vm`. It is deliberately
outside the root workspace so `cargo build`/`cargo test` on a stable toolchain
never need nightly or a network fetch.

### Deterministic harness

`cargo fuzz` needs nightly, which the project's pinned toolchain does not
provide. So the repository also ships a dependency-free harness: a xorshift64
PRNG mutates a fixed corpus (insert, delete, flip, duplicate bytes), and each
input runs through all four stages under `catch_unwind`. It asserts:

1. no panic in any stage;
2. every source-driven error has `line >= 1` and `column >= 1`;
3. the same input produces the same result twice (determinism);
4. arbitrary 8-byte words never panic the decoder.

`scripts/fuzz.sh --deterministic` runs it; without the flag the script prefers
`cargo-fuzz` and reports the missing tool rather than installing it.
`verify-all.sh` gains a 35th row that runs the deterministic harness.

## 7. Compatibility and rollback

- ABI 4, `omni.h`, `OmniValue`, and both SDKs are untouched.
- `LANGUAGE_VERSION` stays 2: no source that parsed before changes meaning,
  except a program with more than 2^24 dynamic instructions or 128 nested
  parentheses, which now fails with a named error instead of hanging or
  overflowing.
- Existing modules, fs roots, and JSON behavior are unchanged for permitted
  inputs.
- The fuzz project and script are additive; deleting them leaves the engine and
  its tests untouched.

## 8. Verification

- New Rust tests: infinite jump hits the budget, push storm hits the stack cap,
  module + parent share one budget, 192 nested parens is a parse error while 100
  succeed, dangling symlink write is refused (Unix), and the mutation harness.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, the workspace suite, the
  Python and Node suites, and `./scripts/verify-all.sh` must stay green with the
  new row.
