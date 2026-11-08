# Phase 9 — Bytecode Optimization, Enterprise Profiling & Diagnostics

Status: implemented. ABI 4 unchanged (additive exports only). Language v2 unchanged.

## 1. Goals and non-goals

Goals:

1. An ahead-of-time optimizer pass between lowering and execution that performs
   constant folding and dead code elimination, making hot programs run with
   fewer instructions.
2. A per-execution profiler exposing per-opcode invocation counts (heatmaps),
   per-opcode self-time when opted in, and peak heap allocation.
3. Rust tests proving the optimized stream is shorter and faster than the raw
   one, plus a 37th verification row that exercises folding, the profiler, and
   positional error accuracy from a foreign host.

Non-goals: control-flow optimizations (the language has no branches yet),
string-concatenation folding (needs a non-span literal encoding; see §7),
cross-execution profiling aggregation, and any ABI bump. `omni_exec_parallel`
and every existing export keep their exact signatures.

## 2. The optimizer pass

New module `crates/omni-core/src/optimizer.rs`, run inside
`run_with_budget` after `bytecode::compile` and before `run_chunk`, only when
the engine's `optimize` flag is set (default **on**).

### 2.1 Constant folding

A single linear pass over the instruction stream keeps a *constant stack* that
mirrors the VM's operand stack, recording for each stack slot whether it holds
a compile-time-known number:

- `PushNumber` pushes the literal's parsed value (the span text is parsed here;
  the lexer guarantees it parses).
- The new opcode `PushFolded` carries an `f64`'s bits across its `a`/`b`
  operands and pushes that value.
- `Add`/`Sub`/`Mul`/`Div`/`Mod` with two numeric constants on top replace
  themselves **and** their two operand pushes with one `PushFolded` holding the
  result — but only when the runtime operation would succeed. `Div` and `Mod`
  with a zero divisor are left untouched, because "division by zero" is a
  runtime error and must keep firing at its own position. `Neg` folds the same
  way over one constant.
- Everything else pushes "not a constant". `Store` re-pushes its operand's
  constancy, `Pop` pops the mirror, `Call` pops its argument count — the mirror
  tracks the real stack, so folding is correct across any sequence lowering can
  produce, not just adjacent triples.

Why a new opcode instead of reusing `PushNumber`: `PushNumber` addresses a
source span, and no span of the source parses to a folded value like
`2 + 3`. `PushFolded` cannot fail at runtime — there is no text to parse —
which is exactly what makes it safe to elide in DCE.

### 2.2 Dead code elimination

A statement whose entire computation is a side-effect-free constant push and
whose value is then discarded (`PushNumber` / `PushFolded` / `PushNil` /
`PushUndef` / `PushStr` followed immediately by the statement-separating
`Pop`) is removed entirely. `PushStr` qualifies only after `decode_literal`
succeeds at compile time, so `"bad \q escape";` keeps its runtime error.

Anything that can fail or has a side effect — a call, a `Load` (undefined
variable), an `Import`, a `Store` — is never elided. Folded statements are a
special case of the same rule: `1 / 0;` does not fold (the fold must fail),
so it stays and still raises "division by zero" at its line and column.

### 2.3 Positional accuracy

Every surviving instruction keeps its own `line`/`column` unchanged; the pass
never rewrites positions. A folded constant inherits the position of the
operator that produced it, which cannot itself raise a runtime error — the
operators that can (`Div`, `Mod`) are never folded when they would. The
contract is: **for any program, the sequence of runtime errors, each with its
line and column, is identical with optimization on and off.**

### 2.4 Engine surface

- `Engine::set_optimize(bool)` / `Engine::optimize()` — copied by `fork()`.
- `Engine::last_optimize() -> OptimizeStats` — `{ folded, elided }` for the most
  recent execution, so tests and hosts can see what the pass did.
- `bytecode::disassemble` renders `PushFolded` as its numeric value.

## 3. The profiler

New `Profile` snapshot, engine-local and reset at the top of every chunk
execution (so each batch item, worker, and import child has its own):

```rust
pub struct Profile {
    pub total_instructions: u64,        // sum of the heatmap
    pub elapsed_ns: u64,                // wall clock of the chunk run
    pub peak_bytes: u64,                // arena bytes in use at run end
    pub opcode_calls: [u64; OPCODE_COUNT],
    pub opcode_ns: [u64; OPCODE_COUNT], // self time; zero unless profiling
}
```

- **Invocation counts** are recorded unconditionally — one indexed increment
  per dispatch, negligible next to the existing per-instruction gas charge.
- **Per-opcode self-time** is recorded only when `Engine::set_profiling(true)`
  is in force: the dispatch is bracketed by `Instant::now()` and the delta is
  added to that opcode's slot. Opting in costs roughly 20–30 ns per
  instruction; opting out costs nothing beyond the counts.
- **Peak heap** reuses the arena's existing accounting: allocation is monotonic
  between reclamations, so `bytes_in_use()` at the end of the run *is* the
  execution's peak. No new tracking in the allocator.
- `elapsed_ns` is always measured (one `Instant` pair per execution).

The snapshot is read through `Engine::profile()`. `fork()` resets the counters
and copies only the `profiling` flag. Imports execute on a child engine, so a
profile describes exactly the instructions the owning engine's VM dispatched —
documented, not accidental.

## 4. FFI: three additive exports

`OMNI_MAX_OPCODES = 64` bounds fixed arrays; the callee fills `opcode_count`
with the real count (21 today), so a future opcode never silently shifts the
layout:

```c
typedef struct OmniProfile {
  uint32_t size;               /* sizeof(OmniProfile), callee-written */
  uint32_t opcode_count;
  uint64_t total_instructions;
  uint64_t elapsed_ns;
  uint64_t peak_bytes;
  uint64_t reserved;           /* zero; keeps arrays 8-aligned */
  uint64_t opcode_calls[OMNI_MAX_OPCODES];
  uint64_t opcode_ns[OMNI_MAX_OPCODES];
} OmniProfile;
```

- `uint32_t omni_engine_set_profiling(OmniEngine*, uint32_t enabled)` — toggles
  per-opcode timing; obeys the one-borrow rule like every export.
- `uint32_t omni_profile(OmniEngine*, OmniProfile* out)` — takes the handle for
  *reading*, so it is refused mid-evaluation rather than racing it.
- `size_t omni_opcode_name(uint32_t index, char* buf, size_t cap)` — writes the
  mnemonic (`PUSH_NUMBER`, `ADD`, …) NUL-terminated, like `omni_last_error`;
  zero when the index is out of range. This is how a host turns heatmap
  indices into names without shipping a name table.

ABI stays **4**: the three functions are additions, and `omni_abi_version()`
still answers 4, so an older SDK's missing-symbol fallback remains the only
compatibility surface that changed (newer SDKs probe for the new symbols).

## 5. SDKs

`Engine.set_profiling(bool)` and `Engine.profile()` in both SDKs. Python's
`profile()` returns a frozen `Profile` with `total_instructions`, `elapsed_ns`,
`peak_bytes`, and `opcode_calls` / `opcode_ns` as dicts keyed by name (fetched
through `omni_opcode_name`); Node's `profile()` returns the same facts as
`totalInstructions`, `elapsedNs`, `peakBytes`, `opcodeCalls`, `opcodeNs`. A
caller sees `{'ADD': 0, 'PUSH_FOLDED': 1, 'RET': 1, ...}` rather than indices,
and the key sets are identical in both hosts. If the loaded library predates
the exports, `profile()` raises `OmniError` saying so — never a wrong answer.

## 6. Verification

- `crates/omni-core/tests/optimizer.rs` — folding correctness (chains, strings
  untouched, `1/0` unfused), DCE rules, positional-accuracy (identical error
  text and position with the pass on and off), and the required proof: the
  optimized chunk executes strictly fewer instructions and completes strictly
  faster over a repeated constant-heavy workload.
- `crates/omni-core/tests/profiler.rs` — heatmap sums to `total_instructions`,
  self-times are zero until profiling is enabled, peak bytes bound a
  string-heavy program, fork isolation.
- `crates/omni-ffi/tests/profile.rs` — struct layout, name export, toggle
  round-trip, borrow refusal.
- Row 37 of `scripts/verify-all.sh` runs both Rust suites, then a Python
  program that (a) evaluates `(2 + 3) * 4 - 8 / 2` and asserts the heatmap
  contains one `PUSH_FOLDED` — the whole expression collapsed — and **no**
  arithmetic opcodes — the fold happened — (b) asserts the heatmap sums to
  `total_instructions`, and (c) evaluates `let x = 1\nx / 0` and asserts the
  error still reports `[Line 2, Col 3]` with the division's own message. Node
  runs the identical checks.

## 7. Deliberately deferred

- **String-concatenation folding.** `"a" + "b"` still allocates at runtime.
  Folding it needs a second non-span literal encoding (decoded bytes in the
  chunk's name buffer), which this phase reserves for when boolean/nil
  literals force the same machinery for `Eq`/`Ne` results.
- **Branch pruning and jump threading.** No lowering source emits `Jump` yet;
  the pass is written to ignore them safely, and a later control-flow phase
  can extend it.
- **Per-thread aggregated profiles** across `exec_parallel` workers — each
  worker's profile is inspectable through its own engine; the parent reports
  the batch totals it already aggregates in `stats()`.
