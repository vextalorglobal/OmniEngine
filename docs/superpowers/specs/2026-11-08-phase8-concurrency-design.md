# Phase 8 — Multi-Threading & Concurrent Execution Contexts

Status: implemented
Date: 2026-11-08
Language version: 2 (unchanged)
C ABI: 4 (unchanged; one additive export)

## 1. The claim

An `Engine` is a complete, self-contained execution context. It owns its arena,
egress block, token/AST/chunk buffers, bindings, globals, host-function registry,
module root, and module cache. Nothing it uses lives in a `static`, a
thread-local, or a process-wide map. Therefore:

- `Engine: Send` — an engine can be moved to another thread.
- `Engine: Sync` — many threads may hold `&Engine` at once and only read.
- `&mut Engine` remains the exclusive right to execute, bind, or reset.

Concurrency across engines is therefore the default, and the only shared mutable
state in the whole runtime is whatever a host deliberately installs through a
callback. The parallel batch API spawns one forked engine per worker; no two
threads ever touch the same engine.

## 2. Why `Sync` is sound for the arena types

`Arena` is the sole owner of its chunks. Moving it (`Send`) transfers unique
ownership. Sharing `&Arena` is safe because:

1. Every mutating method takes `&mut self`, which cannot be obtained while any
   `&Arena` exists (normal Rust aliasing).
2. `reset`/`reset_to`/`rewind` only run through `&mut Engine`.
3. The only shared reads are `bytes_in_use`, `bytes_reserved`, `high_water`,
   `chunk_count`, and the internal `StrRef` bounds check — all pure reads of
   immutable-while-shared state.

`Slot<T>` is a non-owning typed view into an arena allocation. `Slot::get` is a
read; `Slot::set` requires `&mut self`. Sharing `&Slot<T>` therefore cannot
write. `T: Sync` makes the read safe to perform from another thread, and the
lifetime rule (all slots die before their arena resets) is unchanged and already
enforced by `Engine::reclaim`.

Both impls are unconditional only in the `Send` direction, where they were
already present. The new ones are:

```rust
unsafe impl Sync for Arena {}
unsafe impl<T: Sync> Sync for Slot<T> {}
```

## 3. Host functions

`HostCall` becomes `dyn Fn(&Engine, &[Value]) -> HostOutcome + Send + Sync`.
Registration stores it as `Arc<HostCall>` so `fork()` can clone the registry
without cloning the closure itself.

This is a real strengthening of the host contract, not a formality:

- A callback may now be invoked concurrently on several forked engines.
- Anything the callback captures (including FFI `user_data`) must be safe for
  that. The C header states it explicitly.
- The engine still calls host functions with `&Engine`, never `&mut Engine`, so
  a callback cannot mutate the context it observes.

For the C ABI, `HostContext` wraps `*mut c_void`. It gains `unsafe impl Sync`
under the same documented rule as `Send`: the pointee stays alive for the
registration's lifetime and is safe to use from every thread that can run a
callback for this engine. `omni_exec_parallel` can only invoke callbacks in
parallel if the host opted into that contract.

One host-specific limit is documented rather than engineered away. Koffi, the
Node FFI layer, queues a JavaScript callback to the main thread, and
`omni_exec_parallel` is synchronous, so invoking a Node host function from a
worker would deadlock: the main thread is blocked inside the very call that is
waiting for the callback. The Node SDK therefore takes the sequential path
whenever a host function is registered, and says so in `runParallel`'s contract.
Python's ctypes callback is different: it re-acquires the GIL from the worker
thread while `RawEngine` has released it for the call, so the worker path is
supported there. Rust callbacks are `Send + Sync` by their signature.

## 4. `Engine::fork`

`fork(&self) -> Engine` clones configuration, not history:

| Copied | Reset |
| --- | --- |
| module root, fs read/write roots | arena, egress |
| step limit | tokens, AST, chunk, stack, tasks, scratch |
| globals (strings re-materialized into host-owned boxes) | `let` bindings, module bindings, namespaces |
| host-function registry (`Arc` clones) | loading stack, diagnostics, counters |
| module cache (independent map, shared immutable source) | `last_value`, `last_error` |

Re-materializing globals matters: a worker must not hold a `Value` pointing at
the parent's `Box<[u8]>` while its own `globals` table cannot account for it.
The bytes are copied into the worker's own allocation, so `string_of` answers
identically and the parent's binding may be redefined without affecting the
worker. A global whose bytes the parent can no longer prove it owns is copied as
an inert scalar value; it reads as a stale value, never as a dereference.

The fork is a full execution context, so an embedder may keep it, move it to a
long-lived worker thread, and reuse it for many batches.

## 5. Module cache

Imports were re-read from disk on every execution. Phase 8 adds an engine-local
cache:

```rust
struct CachedModule {
    length: u64,
    modified: Option<SystemTime>,
    source: Arc<str>,
}
module_cache: HashMap<PathBuf, CachedModule>
```

- Keyed by the canonical path the loader already computes.
- Invalidated by comparing length and modification time on every import.
- Bounded at 64 entries; a 65th distinct module clears the map rather than
  growing without limit.
- Never `static`: two engines, including forked workers, have independent maps.
  `fork()` clones the map, which shares only immutable `Arc<str>` sources.

The security checks (canonical containment, file type, size, UTF-8) still run
on every import before a cache lookup or a read. A cached source is only reused
after metadata says the file is unchanged.

## 6. `Engine::exec_parallel`

```rust
pub fn exec_parallel(&mut self, sources: &[&str], max_threads: usize) -> Vec<BatchItem>
```

- Empty input returns an empty vector and reclaims nothing, like `exec_many`.
- `max_threads == 0` means automatic: `available_parallelism`, still bounded by
  the item count. Explicit values are clamped to `1..=min(items, 64)`.
- One worker yields exactly `exec_many` semantics (same reclamation, same order,
  same `last_value`/`last_error`).
- Otherwise `std::thread::scope` spawns `N` workers. Each owns an
  `Engine::fork()`; a shared `AtomicUsize` hands out source indices. There is no
  persistent global pool and no scheduler state that outlives the call.
- Each item gets a fresh `Budget` from the fork's `step_limit`; imports share
  that item's budget exactly as in sequential execution. Gas is therefore
  thread-isolated and deterministic for a given item.
- A panic inside one worker item is caught with `catch_unwind` and reported as
  an `ErrorKind::Panic` item. It does not poison the batch, the engine, or the
  process.
- Results are reassembled by index, so ordering matches `sources`. A failing item
  does not stop its neighbours.
- String results are copied out of the worker's arena/egress into the parent's
  egress after the threads join. All results are readable together until the next
  execution on the parent, exactly like `exec_many`.
- Parent counters aggregate all items: `execs` increases once per item,
  `strings_built` sums worker and promotion work, and `last_value`/`last_error`
  describe the final source, matching a sequential loop.

Thread count is a throughput knob, not a correctness knob. The same sources,
globals, host registry, and step limits produce the same per-item answers in any
order.

## 7. C ABI: `omni_exec_parallel`

```c
uint32_t omni_exec_parallel(
    OmniEngine *engine,
    const char *const *sources,
    const size_t *lengths,
    size_t count,
    uint32_t max_threads,
    OmniValue *out,
    uint32_t *statuses);
```

- Synchronous. It returns when every item is done; the caller does not poll.
- Writes one `OmniValue` and one status per item, in source order.
- Per-item errors are retained for `omni_batch_error`, replacing the previous
  batch exactly as `omni_exec_many` does.
- `max_threads == 0` selects the engine's automatic choice.
- Validation matches `omni_exec_many`: null checks, `MAX_BATCH`, per-source
  `MAX_SOURCE_LEN`, UTF-8 of every source before anything runs.
- ABI 4 does not move. The export is additive; SDKs check for it and fall back
  to `omni_exec_many` when an older library is loaded.

The handle itself remains single-thread-at-a-time through its checked borrow.
`omni_exec_parallel` is the supported way to use several cores from one handle.
Callbacks registered on that handle may run on worker threads; `user_data` must
be thread-safe for that reason.

## 8. Verification

- Compile-time assertions: `Engine: Send + Sync`, `HostCall: Send + Sync`.
- 50+ threads synchronized on a barrier, each moving a pre-forked engine and
  running a distinct program under a strict gas limit; every answer is checked.
- Concurrent `&Engine` reads from several threads (strategy/stats/render).
- Parallel batch: ordering, per-item errors, gas exhaustion isolation, string
  results all readable together, host callback invocation, module import from
  workers.
- Module cache: hit (file read once), invalidation after rewrite, bound.
- FFI contract tests for nulls, empty batches, bad UTF-8, status/error mapping.
- Python and Node SDK tests for `run_parallel`, including the fallback path.
- `scripts/verify-all.sh` row 36: parallel throughput across every present host
  and the zero-data-race guarantee proved by the Rust stress test.

## 9. Non-goals

- No work-stealing scheduler, persistent pool, or `async`.
- No shared mutable heap between engines.
- No change to language semantics, ABI structs, or existing export signatures.
