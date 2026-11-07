#!/usr/bin/env python3
"""The Python-side benchmark: what one evaluation costs *through the SDK*.

    python sdks/python/bench.py [iterations]

The Rust benchmark in ``crates/omni-core/examples/bench.rs`` measures the
engine. This measures the bridge: the ctypes crossing, the ``bytes`` a call
builds on the way in, and the ``Result`` it builds on the way out. That is the
number a Python host actually pays, and it is never the Rust number.

Method, the same as the Rust side and for the same reasons:

* zero dependencies -- ``time``, ``gc`` and ``tracemalloc`` are stdlib;
* timing, counting and tracing happen in **separate passes**, because
  ``tracemalloc`` instruments every allocation and would report the tracer's
  own cost as the engine's;
* the reported figure is the **fastest** of ``ROUNDS`` rounds, not the mean: a
  scheduler preemption or an antivirus scan only ever adds time, so the minimum
  is the closest estimate of what the code costs and the mean is the machine's
  noise floor;
* retention is read from the engine's own counters, which are the only ones
  that see inside the DLL; ``gc`` collections per call are the host-side
  evidence that nothing accumulates.

Every number this prints belongs to the machine it ran on.
"""

from __future__ import annotations

import gc
import sys
import time
import tracemalloc
from collections.abc import Callable
from dataclasses import dataclass

import omni
from omni import _ffi

#: Rounds per measurement; the fastest one is reported. See the module doc.
ROUNDS = 5

#: Batch sizes the crossing-amortisation table walks, same as the Rust bench.
BATCH_SIZES = (1, 8, 64, 256)

#: ``(name, program, needs a host function)`` -- the three Rust workloads again.
SCENARIOS = (
    ("number", "let answer = 40 + 2; answer", False),
    ("string", '"hello, " + "world" + "!"', False),
    ("host-call", "square(6) + 1", True),
)


@dataclass(frozen=True)
class Measurement:
    """One workload, as the bridge pays for it."""

    ns_per_call: float
    gc_per_call: float
    gc_ms: float
    peak_bytes: int
    stats: omni.Stats


def prepare(source: str, wants_host_fn: bool) -> omni.Engine:
    """A warmed engine, with the callback registered if the workload needs one."""
    engine = omni.Engine()
    if wants_host_fn:
        engine.register_function("square", lambda x: x * x, arity=1)
    # Warm the arena, the egress block and ctypes' own caches: the first call
    # maps a chunk and builds a ``CFuncPtr``, and neither is a per-call cost.
    for _ in range(64):
        engine.run(source)
    return engine


def collect(work: Callable[[], object], iters: int) -> tuple[float, float]:
    """Collections per call and the seconds the collector spent on them.

    Generational GC runs only when an allocation threshold is crossed, so the
    count is a lower bound on bytes allocated and an upper bound on what the
    collector costs. Both directions matter for the batch claim, which is about
    the objects a host stops building.

    The callback shape changed in 3.14 -- it used to be handed one dict with a
    ``phase`` key and is now handed the phase and the counters separately -- so
    both are read here rather than picking one and being wrong on the other.
    """
    state = {"count": 0, "seconds": 0.0}

    def callback(*args: object) -> None:
        if len(args) == 2:
            phase, info = args
            if phase == "stop":
                state["count"] += 1
                state["seconds"] += float(info["duration"])  # type: ignore[index]
        elif args and isinstance(args[0], dict) and args[0].get("phase") == "stop":
            state["count"] += 1
            state["seconds"] += float(args[0].get("elapsed", 0.0))

    gc.callbacks.append(callback)
    try:
        for _ in range(iters):
            work()
    finally:
        gc.callbacks.remove(callback)
    return state["count"] / iters, 1000.0 * state["seconds"]


def measure(engine: omni.Engine, work: Callable[[], object], iters: int) -> Measurement:
    """Time one workload, then account for it in two more passes.

    ``work`` is the thing being measured -- a closure that performs one call and
    throws the result away. Passing it instead of naming ``run`` or ``run_many``
    keeps the loop table and the batch table comparable: same harness, same
    timing call, different work.
    """
    best = float("inf")
    for _ in range(ROUNDS):
        start = time.perf_counter_ns()
        for _ in range(iters):
            work()
        best = min(best, (time.perf_counter_ns() - start) / iters)

    gc_rate, gc_seconds = collect(work, iters)

    # Traced pass: peak host-side memory for one call. ``tracemalloc`` sees
    # Python's allocations only, which is exactly the half this harness owns --
    # the engine's side comes from ``stats`` below.
    tracemalloc.start()
    work()
    _, peak = tracemalloc.get_traced_memory()
    tracemalloc.stop()

    return Measurement(
        ns_per_call=best,
        gc_per_call=gc_rate,
        gc_ms=gc_seconds,
        peak_bytes=peak,
        stats=engine.stats(),
    )


def crossing_cost(iters: int) -> float:
    """Nanoseconds of an FFI call that does no work at all.

    ``omni_abi_version`` reads a constant and returns it, so whatever this costs
    is ctypes marshalling and the foreign frame: the part of ``ns/call`` that is
    not the engine, and the part a batch amortises over its whole list.
    """
    call = _ffi.load_library().omni_abi_version
    call()
    best = float("inf")
    for _ in range(ROUNDS):
        start = time.perf_counter_ns()
        for _ in range(iters):
            call()
        best = min(best, (time.perf_counter_ns() - start) / iters)
    return best


def measure_scaling(engine: omni.Engine, work: Callable[[], object], iters: int) -> tuple[float, omni.Stats]:
    """Fastest of ``ROUNDS`` single passes, with the engine's count checked in each.

    A separate helper for one reason: :func:`measure` runs the workload six
    times (five rounds, one counting pass, one traced pass), and the scaling
    check below needs ``execs`` to say exactly ``iters`` -- so this pass times
    and counts together, and the count is a guard rail rather than a footnote.
    """
    best = float("inf")
    for _ in range(ROUNDS):
        before = engine.stats().execs
        start = time.perf_counter_ns()
        for _ in range(iters):
            work()
        elapsed = time.perf_counter_ns() - start
        counted = engine.stats().execs - before
        if counted != iters:
            raise AssertionError(f"the engine counted {counted} of {iters} executions")
        best = min(best, elapsed / iters)
    return best, engine.stats()


def main() -> int:
    iters = int(sys.argv[1]) if len(sys.argv) > 1 else 20_000
    failures = 0

    print(
        f"OmniEngine Python SDK benchmark -- {iters} iterations x {ROUNDS} rounds per scenario, "
        "fastest round"
    )
    print(f"library: {omni.library_file_name()}, ABI {omni.OMNI_ABI_VERSION}, Python {sys.version.split()[0]}")

    print(
        f"\n{'scenario':<10} {'ns/call':>9} {'gc/call':>9} {'gc-ms':>8} {'peak-B':>8} "
        f"{'arena-B':>9} {'egress-B':>9}"
    )
    loop_ns: dict[str, float] = {}
    for name, source, host_fn in SCENARIOS:
        engine = prepare(source, host_fn)
        m = measure(engine, lambda: engine.run(source), iters)
        loop_ns[name] = m.ns_per_call
        print(
            f"{name:<10} {m.ns_per_call:>9.1f} {m.gc_per_call:>9.3f} {m.gc_ms:>8.1f} "
            f"{m.peak_bytes:>8} {m.stats.arena_bytes_reserved:>9} {m.stats.egress_bytes_reserved:>9}"
        )
        engine.close()

    print(f"\nempty crossing -- omni_abi_version(): {crossing_cost(iters * 5):.1f} ns/call")

    # The batch claim, on the workload that builds the most Python objects: one
    # crossing for N programs, reported per item so the two tables compare.
    string_source = SCENARIOS[1][1]
    engine = prepare(string_source, False)
    print("\nbatch -- the `string` scenario, N items per crossing (per item)")
    print(
        f"{'N':>5} {'ns/item':>10} {'gc/item':>9} {'gc-ms':>8} {'peak-B/xing':>12} "
        f"{'egress-B':>9} {'vs loop':>9}"
    )
    reservations = set()
    for size in BATCH_SIZES:
        programs = [string_source] * size
        for _ in range(8):
            engine.run_many(programs)
        rounds = max(iters // size, 1)
        # One `work()` is one crossing here, so the per-item figures come out of
        # the per-crossing ones by dividing by N. Peak memory is deliberately
        # left per crossing: a batch holds its whole result list at once.
        m = measure(engine, lambda: engine.run_many(programs), rounds)
        reservations.add(m.stats.egress_bytes_reserved)
        print(
            f"{size:>5} {m.ns_per_call / size:>10.1f} {m.gc_per_call / size:>9.3f} {m.gc_ms:>8.1f} "
            f"{m.peak_bytes:>12} {m.stats.egress_bytes_reserved:>9} "
            f"{loop_ns['string'] / (m.ns_per_call / size):>8.2f}x"
        )
    if len(reservations) != 1:
        print(f"FAIL: egress reservation moved between batch sizes: {sorted(reservations)}", file=sys.stderr)
        failures += 1
    engine.close()

    # The scaling claim: ten times the work may not cost more per item, and must
    # not reserve one extra byte. Both halves are checked, not just printed.
    print("\nscaling -- the `string` scenario at three sizes")
    engine = prepare(string_source, False)
    base_ns = 0.0
    base_reserved = 0
    for n in (1_000, 10_000, 100_000):
        ns, stats = measure_scaling(engine, lambda: engine.run(string_source), n)
        print(
            f"{n:>8} {ns:>10.1f} ns/call egress {stats.egress_bytes_reserved:>9} "
            f"high-water {stats.egress_high_water:>9} execs {stats.execs}"
        )
        if n == 1_000:
            base_ns, base_reserved = ns, stats.egress_bytes_reserved
        elif ns > base_ns * 2.0:
            print(f"FAIL: ns/call grew from {base_ns:.1f} to {ns:.1f} at {n}", file=sys.stderr)
            failures += 1
        if stats.egress_bytes_reserved != base_reserved:
            print(f"FAIL: egress reservation grew to {stats.egress_bytes_reserved} at {n}", file=sys.stderr)
            failures += 1
    engine.close()

    print("\nall benchmark checks passed" if failures == 0 else f"\n{failures} benchmark check(s) failed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
