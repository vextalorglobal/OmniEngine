# OmniEngine Phase 4 — Batch Execution, Measured Performance, ABI v4

Date: 2026-11-07
Status: executed as written below
Supersedes: `docs/GUIDE.md` sections 3.2 (the arena is now checkpointed), 3.4 (the
engine has a batch entry point), 4.2 and 4.3 (ABI v4 and two new exports); the
"performance" numbers older documentation carried are replaced by the measured
tables in §6, which is the whole point of this phase

## 0. What this phase is, and what it is not

Phases 1–3 built an engine and a bridge that carries one program per crossing.
Phase 4 makes the bridge carry several, and then measures — on this machine,
with no harness crate and no third-party dependency — what one execution
actually costs from each host. After this phase, every performance claim in the
documentation is a quoted run, not a promise.

It is **not** any of the things a "performance phase" often quietly becomes:

- **Not parallelism.** A batch runs its items sequentially. What it amortises
  is the crossing, the arena reclamation, and the egress rewind — the three
  per-execution costs that exist even when one program is run. There is no
  thread pool, no work queue, and no claim that items run concurrently.
- **Not compilation.** No bytecode, no JIT, no hot-path tiering. The engine
  still lexes and evaluates every program every time; the lexer work is what
  the token-vector reuse in §7 makes allocation-free, not skip-able.
- **Not a guarantee.** The tables in §6 belong to the machine that printed
  them. The only claims made about the engine in general are the structural
  ones — flat reservations, zero retained bytes — which are properties of the
  code, not of the hardware.

Requested, condensed from the phase brief:

1. `omni_exec_many` in core and FFI: several expressions in one crossing,
   amortising FFI overhead and arena reset costs, with the re-entrancy guard,
   egress preservation, and exception handling sound during a batch.
2. Performance work on the serialization and conversion paths between Rust,
   Python (`ctypes`), and Node (`koffi`): fewer redundant allocations, fewer
   cache misses across repeated cycles.
3. A benchmarking harness measuring nanoseconds per execution, allocation
   counts, and crossing costs across all three hosts, proving linear scaling
   with no leak over thousands of iterations.
4. SDK wrappers (`exec_many` / `runMany`) and documentation of the measured
   numbers and usage guidance.

Five shapes in the brief were corrected before implementation, each recorded
here so the difference is a decision rather than a drift.

- **`OmniStats.execs` counts items, not crossings.** A counter that counts
  crossings cannot verify the linearity claim — "100,000 executions cost the
  same per execution as 1,000" needs the count of executions, and a batch of 8
  paid for in one crossing is 8 of them. ABI v4 is the version where the
  counter's meaning changed, because a v3 host reading it as "number of
  `omni_exec` calls" would draw a correct-looking and wrong conclusion.
- **A batch has two error channels.** Malformed *calls* — a null array, a count
  over `MAX_BATCH`, a source over `MAX_SOURCE_LEN`, bad UTF-8 — are refused
  whole: the return status carries the refusal and nothing is written to the
  outputs. Failing *items* are data: every element of the status array is
  written, the return status is the first item's failure, and the per-item
  message is available from `omni_batch_error(engine, index, …)`. One channel
  could not serve both, because "refused" means there is no item 0 to ask
  about, and "item 2 failed" is not a reason to refuse items 0 and 1.
- **`omni_batch_error` answers only the current batch.** Its currency is the
  same rule the results live under: until the next `omni_exec` or
  `omni_exec_many` on that engine. After that it returns zero bytes rather
  than a stale message, because an index is meaningless once the batch it
  indexed into has been reclaimed.
- **ABI v4 changes no struct layout.** The bump exists because a v3 SDK cannot
  declare the two new exports and would read the changed counter semantics
  wrongly. Both SDKs already read the library's own ABI answer at load time
  and refuse a mismatch, so the bump is the guard rail, not a format change.
- **The honest headline is "the crossing is cheap", not "the batch is fast".**
  Measured before any optimisation decision: an empty crossing costs ~240 ns
  through ctypes and ~36 ns through koffi, while one wrapped execution costs
  several microseconds. A batch can only amortise the crossing, so it can only
  ever shave the smaller term. The spec records the modest time win and the
  unconditional allocation win (§6) instead of inventing a speed-up the
  machine does not show.

## 1. The core: `Engine::exec_many`

```rust
pub struct BatchItem { pub value: Value, pub error: Option<OmniError> }
pub fn exec_many(&mut self, sources: &[&str]) -> Vec<BatchItem>;
```

One reclamation before the first item (`reclaim()` — the single place that
pairs the arena reset with the binding-list clear, extracted from `exec` so
there is exactly one implementation of "what survives a crossing"), then one
`run` per source with three steps between items: `arena.reset_to(mark)` back
to the checkpoint taken at batch start, `bindings.clear()`, and
`call_depth = 0`. Each item is therefore exactly one `exec`'s worth of state —
item *N* cannot see item *N−1*'s `let` — and a batch is a sequence of
independent executions, never one long program.

The egress block rewinds **once, before the first item**. Every string result
is promoted into it and stays readable until the next `exec` or `exec_many`,
so all *N* results are live at once and a host can copy them out after the
call returns. That is the trade the batch makes, stated rather than hidden:
reserved egress bytes are bounded by one batch's total demand instead of one
execution's, which §6 shows on this machine as a high-water mark of 6,142
bytes for a 256-item string batch against a reservation that never moves.

A failing item does not stop the batch: every element of `sources` gets an
element of the result, in order. `last_value` and `last_error` end exactly as
if the final item had been passed to `exec` alone. An empty slice reclaims
nothing, so polling an empty queue does not expire results the caller is
still holding.

## 2. The arena: marks, checkpoints, and the chunk walk

Three changes to `arena.rs`, each load-bearing and each testable:

- **`Arena::mark` / `Arena::reset_to`.** A `Mark` records which chunk the bump
  cursor was filling and how far into it, so a batch can reclaim *back to* a
  checkpoint instead of back to the beginning — one warm chunk and one egress
  rewind serve all N items. Oversize blocks are deliberately not in a mark:
  a count cannot record which large blocks were spare and which were live, so
  `reset_to` reclaims them all, exactly as `reset` does. A mark from another
  arena, or one whose chunks were since reset away, is clamped — the worst
  outcome is a wasted chunk, never an out-of-bounds cursor.
- **The chunk walk.** `alloc_raw` used to map a fresh chunk whenever the
  cursor's current chunk did not fit; a multi-chunk arena would therefore map
  two fresh chunks per cycle forever. The cursor now walks chunks already
  mapped — all chunks are one size, so a request that did not fit the
  exhausted one fits the next — and a rewound arena stays bounded by its
  largest cycle rather than by how many cycles have run. This is the change
  that made "reservation flat across 100,000 executions" true; before it,
  the scaling table could not have been written.
- **`current` + `used` cursor state.** Where the arena used to track only
  "bytes claimed in the newest chunk", it now tracks which chunk and how far,
  because the walk needs both.

The `Slot<T>` facade and its invariant are unchanged: every collection of
slots must be dropped before the arena is reset, and `reclaim()` clears the
bindings vector in the same call, so a caller cannot break it.

## 3. The FFI: `omni_exec_many`, `omni_batch_error`, ABI v4

```c
#define OMNI_ABI_VERSION 4
#define OMNI_MAX_BATCH 4096

uint32_t omni_exec_many(OmniEngine *engine,
                        const char *const *sources, const size_t *lengths,
                        size_t count, OmniValue *out, uint32_t *statuses);
size_t   omni_batch_error(const OmniEngine *engine, size_t index,
                          char *buf, size_t cap);
```

`count == 0` returns `OMNI_OK` and writes nothing — an empty batch is a no-op,
not an error. A call-shaped problem (null `sources`/`lengths`/`statuses` with
`count > 0`, a null element, a length over `MAX_SOURCE_LEN`, `count` over
`OMNI_MAX_BATCH`, or source bytes that are not UTF-8) refuses the whole call:
nothing is written to `out` or `statuses`, and the status is
`OMNI_ERR_ARG`. A program-shaped problem (a compile error, a runtime error, a
host function that fails) writes a status per item and the call returns the
first item's failure.

The new header rules, in the header's own words: a batch is N independent
executions sharing one egress rewind (rule 11); the results are readable
until the next `omni_exec` or `omni_exec_many` (rule 1, extended); a call
from inside a host callback is refused with `OMNI_ERR_REENTRANT`, and rule 9
now names `omni_batch_error` as the one message source whose currency ends at
the next execution rather than at the next call to itself.

`omni_batch_error(index, buf, cap)` copies the engine text for item `index` of
the most recent batch and returns the byte count; it returns 0 for an index
that has no message (an item that succeeded, or an index past the batch).

## 4. The SDKs

**Python (`omni/_ffi.py`, `omni/__init__.py`).** The raw layer grows
`exec_many(sources) -> (status, values, statuses)` and
`batch_error(index, cap=4096) -> str`. The wrapper layer adds:

- `Engine.run_many(sources) -> list[Result]` — raises `OmniError` naming the
  first failing item (`item 1 of 3: line 1: …`); string results are copied
  out before return, so the list is ordinary Python data and nothing expires.
- `Engine.try_run_many(sources) -> list[tuple[int, Result | None, str]]` —
  the per-item shape of `try_run`; a failing item carries its own status and
  its own message, which `last_error` cannot offer after a batch.

The allocation work in §7 (reused ctypes arrays, reused string buffer) lives
in this layer.

**Node (`lib/bindings.js`, `index.js`).** The bindings declare the two new
prototypes with every batch parameter as `void *` (koffi accepts typed-array
memory for pointer parameters, which is what lets the SDK hand over
`BigUint64Array`s it owns). The wrapper adds `runMany` / `tryRunMany` with
the same contracts, a scratch that grows by doubling to `MAX_BATCH` and is
reused across calls, and `MAX_BATCH` exported. Both SDKs refuse an over-long
list locally, before the crossing, with the SDK's own message.

## 5. The benchmark harnesses

Three files, one per host, deliberately **not** one file with three backends:
each measures the thing its host can see, with its host's own instruments.

- `crates/omni-core/examples/bench.rs` — the engine, release build, zero
  dependencies. A counting `GlobalAlloc` installed by the binary records
  allocations, bytes, and live-byte deltas; the counter is **off** during
  timing rounds and **on** during counting rounds, so nanoseconds are never
  the nanoseconds of instrumented code. Scenarios: `number`, `string`,
  `host-call`, `wide` (120 additions, to make the token vector the
  interesting cost); plus a batch table at N = 1/8/64/256 and a scaling pass
  at 1,000/10,000/100,000 iterations.
- `sdks/python/bench.py` — the engine behind ctypes. Timing with
  `perf_counter`, GC accounting with the `gc` callbacks, transient memory
  with `tracemalloc` peaks, retention with the engine's own counters. Handles
  both callback shapes (Python 3.15 changed the signature).
- `sdks/nodejs/bench.js` — the engine behind koffi. Timing with
  `hrtime.bigint`, GC accounting with a `PerformanceObserver` on `type: 'gc'`,
  retention with the engine's counters. Deliberately **no** heap-bytes
  column: V8 collects in the middle of any loop this long, so a
  before/after `heapUsed` delta reports when the collector ran rather than
  what the code allocated (measured here: it came out negative). The leak
  claim rides on the engine's counters, which are the only ones that see
  inside the DLL.

Shared method, so the tables mean the same thing everywhere: five rounds,
fastest reported (a scheduler preemption only ever adds time); warm-up before
any measurement; the scaling claim checked per timed pass against
`stats().execs`, so a round that somehow skipped executions fails loudly; and
a non-zero exit on any violation — a benchmark that prints a leak and exits
0 would be a decoration.

`scripts/bench.sh` runs whichever hosts are present, keeps each full report
under `target/bench/`, and prints the side-by-side summary: the `string`
scenario per host, and the best per-item batch speed-up, read from the same
text a person reads.

## 6. Measured results

Canonical run: 20,000 iterations × 5 rounds, fastest round, this machine,
`./scripts/bench.sh` (full tables in `target/bench/`).

```
host            ns/exec   best batch
rust              269.5        0.99x      (engine only, release)
python          10062.0        1.24x      (ctypes; empty crossing 239.1 ns)
node             8489.9        1.38x      (koffi;  empty crossing  35.8 ns)
```

Rust detail: `number` 218.4, `string` 269.5, `host-call` 194.2, `wide`
6,417.6 ns/exec; **0.00 allocs/exec in every scenario** (the warm token
vector and checkpointed arena leave the steady state allocation-free);
live-delta 0. Batch per item: N=1 0.66×, N=8 0.92×, N=64 0.99×, N=256
0.98× of a loop — and `allocs/item` falls from 1.000 to 0.004, which is the
batch's real Rust win: one arena reclamation and one egress rewind shared
across 256 executions.

Python detail: `number` 6,310.7, `string` 10,062.0, `host-call` 10,108.3
ns/call; gc/call 0.000 (the loop provokes no collections); transient peak
522–1,166 B. Batch: 0.60× / 1.09× / 1.24× / 0.96× at N=1/8/64/256 — the
256-row's dip below 1.0 is machine noise (this is a laptop; the same cell
measured 1.34× on another run), and the table says so rather than hiding it.
At the raw ctypes layer, before the wrapper's conversion work: 4,558.4
ns/call for a loop, 3,987.0 ns/item for an 8-batch — the crossing is not
where the wrapper's microseconds go.

Node detail: `number` 7,090.6, `string` 8,489.9, `host-call` 24,384.7
ns/call (the host-call row pays koffi's trampoline, which is the most
expensive bridge this repository has); gc/call ≈ 0. Batch: 0.49× / 1.11× /
1.36× / 1.38×. The egress high-water mark climbs with N (22 → 6,142 B) while
the reservation stays 65,536 — the §1 trade, visible on the wire.

Scaling, all three hosts, 1,000 → 10,000 → 100,000 executions of the `string`
scenario: Rust 225.9 → 229.8 → 240.3 ns/exec, Node 7,289 → 8,183 → 8,370,
Python 10,145–11,790 (noisy but not climbing). In every host, across all
three sizes: egress reservation 65,536 throughout, `execs` counted exactly
(±0), and the engine's retained bytes flat. That is the linearity claim,
measured rather than asserted.

The headline the three tables agree on: **the crossing costs 36–240 ns and
the wrapper costs microseconds, so a batch's time win at the SDK level is
bounded (~1.2–1.4× per item at best), while its allocation win is
unconditional — in Rust, 256 executions for ~1 allocation.** Anyone quoting
these numbers must also quote the machine: the full tables carry the library
paths, tool versions, and iteration counts.

## 7. What was optimized, and what was deliberately not

The baseline first (the repo's rule: measure, then optimise). Two core
changes came out of it:

- **Token vector reuse.** `Token` went from a `&'a str` slice to a 16-byte
  `{kind, start: u32, len: u32, line}` — no lifetime, `Copy`, offsets into
  the source buffer. The engine keeps one vector and refills it per
  execution, so a warm engine lexes without asking the system allocator for
  anything; the `wide` scenario is the regression test for this path.
  `MAX_SOURCE_LEN` (256 MiB) is what makes `u32` offsets safe, and the C ABI
  states the same number so a host is refused at the boundary first.
- **The arena chunk walk (§2).** This is the change the scaling table proves:
  before it, a cycle that needed two chunks mapped two fresh ones per
  execution and `bytes_reserved` grew without bound; after it, the reserved
  bytes are a function of the largest single cycle.

SDK-side, the same rule — measure the bridge, then cut what it measurably
builds: the Python raw layer reuses its ctypes pointer/length/status/value
arrays and its string-copy buffer across calls (fresh arrays + `list()`
conversion measured 2,346 ns/item; reused + per-element access 1,755), and
the Node wrapper reuses one batch scratch that doubles as needed. What was
deliberately **not** done, each with its reason: no parallel execution
inside a batch (a sequential batch has no synchronisation to get wrong, and
the guard's whole job is that one engine has one borrower); no bytecode or
caching layer (an expression language with no user functions has nothing to
reuse a compiled form for — the same reasoning that keeps the no-AST design
honest); no benchmark dependency (criterion would measure this machine's
criterion, and the phase's rule is that a claim must survive being read).

## 8. Verification

- Rust: 225 workspace tests, including the new batch contract suite
  (`omni-ffi/tests/batch.rs` — item isolation, egress sharing, refusal
  shapes, `omni_batch_error` currency, counter semantics) and the C embed
  harness, which now exercises `omni_exec_many` and `omni_batch_error`
  directly against the header.
- Python: 106 tests; Node: 92 tests (18 of them new, for `runMany` /
  `tryRunMany` — ordering, string copies, failing-item indexing, item
  isolation, host-function calls from batch items, empty batches, local
  refusal, re-entrancy, scratch growth, flat reservations).
- `scripts/verify-all.sh`: four new cross-host rows — the same four items
  produce the same values in Python and Node; a failing item reports the
  same statuses (0 2 0 2) and the same engine text in both; 8,000 items in
  1,000 crossings count as +8,000 executions with a flat reservation; a
  batch attempted from inside a host callback is refused with the shared
  refusal text and the engine lives on.
- `scripts/bench.sh`: all three hosts ran, all three passed their own
  checks (flat reservations, exact execution counts, no retained bytes), on
  the run whose tables §6 quotes.

## 9. What this phase leaves for later

Named, not hidden: the batch amortises the crossing but not the host-side
wrapper work, and the wrapper work is now the measured bottleneck — a
zero-copy result path (handing a host a pointer it borrows rather than
copies) is the obvious next increment, and this phase's harnesses are the
instrument it will be judged by. Also left open: batches over non-string
payloads have no fast path worth building until a host shows demand, and the
`wide` scenario exists so a future bytecode phase has a before-picture.
