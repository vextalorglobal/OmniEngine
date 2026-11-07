//! The Rust-side benchmark: nanoseconds per execution, and what each execution
//! costs the general heap.
//!
//!   cargo run --release -p omni-core --example bench
//!
//! Zero dependencies, like the crate it measures. No criterion, no harness
//! crate: `Instant` for time and a counting `GlobalAlloc` installed by this
//! binary for allocations. The counter is *off* while timing runs and *on*
//! while counting runs, so the nanoseconds reported are not the nanoseconds of
//! instrumented code — the two questions are asked in separate passes.
//!
//! Every number this prints comes from the machine it runs on. Nothing here is
//! a claim about OmniEngine in general; `docs/GUIDE.md` quotes a run, not a
//! promise.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::Instant;

use omni_core::{Engine, HostOutcome, Stats};

/// Counts what this thread asks the system allocator for, when counting is on.
///
/// Per-thread rather than global for the same reason `omni_core::counted` is:
/// a tally that moves with unrelated activity measures noise. `const`-init
/// `Cell`s register no destructor, so reading them cannot itself allocate and
/// recurse into this allocator.
struct Counting;

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
}

fn record(allocs: u64, bytes: i64) {
    ON.with(|on| {
        if on.get() {
            ALLOCS.with(|a| a.set(a.get() + allocs));
            // Gross bytes asked for, not the net of alloc minus free: a steady
            // state that allocates and frees the same buffer every execution
            // nets to zero and would hide the very cost being measured.
            BYTES.with(|b| b.set(b.get() + bytes.max(0) as u64));
            LIVE.with(|l| l.set(l.get() + bytes));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Safety: the caller upholds GlobalAlloc's contract; System is a valid
        // delegating allocator.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            record(1, layout.size() as i64);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(0, -(layout.size() as i64));
        // Safety: the matching free for an allocation System produced.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Safety: as above.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            // The delta only: System resized in place or moved the block without
            // routing the old one through Self::dealloc.
            record(1, new_size as i64 - layout.size() as i64);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One measured workload: a name, a program, and whether it needs a host
/// function registered first.
struct Scenario {
    name: &'static str,
    source: String,
    host_fn: bool,
}

fn scenarios() -> Vec<Scenario> {
    // A program long enough that the token vector, not the arithmetic, is the
    // interesting cost. 120 additions is still one line and one `let`.
    let mut wide = String::from("let total = 1");
    for _ in 0..120 {
        wide.push_str(" + 1");
    }
    wide.push_str("; total");
    vec![
        Scenario {
            name: "number",
            source: "let answer = 40 + 2; answer".into(),
            host_fn: false,
        },
        Scenario {
            name: "string",
            source: r#""hello, " + "world" + "!""#.into(),
            host_fn: false,
        },
        Scenario {
            name: "host-call",
            source: "square(6) + 1".into(),
            host_fn: true,
        },
        Scenario {
            name: "wide",
            source: wide,
            host_fn: false,
        },
    ]
}

fn prepare(s: &Scenario) -> Engine {
    let mut e = Engine::new();
    if s.host_fn {
        e.register_host_fn("square", 1, |_, args| {
            HostOutcome::Number(args[0].as_f64().unwrap_or(0.0).powi(2))
        })
        .expect("the benchmark's own registration is valid");
    }
    // Warm the blocks and the token vector before anything is measured: the
    // first execution maps a chunk, and that syscall is not a per-exec cost.
    for _ in 0..64 {
        let _ = black_box(e.exec(black_box(&s.source)));
    }
    e
}

struct Measurement {
    ns_per_exec: f64,
    allocs_per_exec: f64,
    bytes_per_exec: f64,
    /// Bytes this thread still holds after `iters` executions, minus what it
    /// held before. Zero is the no-leak claim, measured rather than asserted.
    live_delta: i64,
    stats: Stats,
}

/// Timing rounds per measurement. The reported figure is the *fastest* round,
/// not the mean: on a machine that is also running a desktop, a scheduler
/// preemption or a turbo transition only ever adds time, so the minimum is the
/// closest estimate of what the code itself costs. Averaging would report the
/// noise floor of the machine rather than the cost of the engine.
const ROUNDS: usize = 5;

fn measure(s: &Scenario, iters: usize) -> Measurement {
    let mut e = prepare(s);

    // Pass one: time, with the counter off.
    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..iters {
            let _ = black_box(e.exec(black_box(&s.source)));
        }
        let ns = start.elapsed().as_nanos() as f64 / iters as f64;
        best = best.min(ns);
    }

    // Pass two: count, with the clock ignored. The engine is already warm, so
    // the allocations counted here are the steady-state ones.
    let live_before = LIVE.with(Cell::get);
    ALLOCS.with(|a| a.set(0));
    BYTES.with(|b| b.set(0));
    ON.with(|o| o.set(true));
    for _ in 0..iters {
        let _ = black_box(e.exec(black_box(&s.source)));
    }
    ON.with(|o| o.set(false));
    let allocs = ALLOCS.with(Cell::get);
    let bytes = BYTES.with(Cell::get);
    let live_delta = LIVE.with(Cell::get) - live_before;

    Measurement {
        ns_per_exec: best,
        allocs_per_exec: allocs as f64 / iters as f64,
        bytes_per_exec: bytes as f64 / iters as f64,
        live_delta,
        stats: e.stats(),
    }
}

/// The same workload as `measure`, but delivered `batch` items at a time.
///
/// Reported per *item*, so the two tables are directly comparable: the difference
/// is what one arena reclamation and one egress rewind shared across `batch`
/// executions is worth. The result vector is the batch's one heap request, so
/// `allocs/exec` here should fall towards zero as the batch grows.
fn measure_batch(s: &Scenario, iters: usize, batch: usize) -> Measurement {
    let mut e = prepare(s);
    let sources: Vec<&str> = vec![s.source.as_str(); batch];
    for _ in 0..8 {
        let _ = black_box(e.exec_many(black_box(&sources)));
    }
    let rounds = (iters / batch).max(1);
    let items = rounds * batch;

    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..rounds {
            let _ = black_box(e.exec_many(black_box(&sources)));
        }
        best = best.min(start.elapsed().as_nanos() as f64 / items as f64);
    }

    let live_before = LIVE.with(Cell::get);
    ALLOCS.with(|a| a.set(0));
    BYTES.with(|b| b.set(0));
    ON.with(|o| o.set(true));
    for _ in 0..rounds {
        let _ = black_box(e.exec_many(black_box(&sources)));
    }
    ON.with(|o| o.set(false));
    let allocs = ALLOCS.with(Cell::get);
    let bytes = BYTES.with(Cell::get);
    let live_delta = LIVE.with(Cell::get) - live_before;

    Measurement {
        ns_per_exec: best,
        allocs_per_exec: allocs as f64 / items as f64,
        bytes_per_exec: bytes as f64 / items as f64,
        live_delta,
        stats: e.stats(),
    }
}

fn main() {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20_000);

    println!(
        "OmniEngine core benchmark — {iters} iterations x {ROUNDS} rounds per scenario, fastest round, release build"
    );
    println!(
        "{:<10} {:>10} {:>12} {:>12} {:>11} {:>10} {:>10}",
        "scenario", "ns/exec", "allocs/exec", "bytes/exec", "live-delta", "arena-B", "egress-B"
    );
    let mut leaks = 0;
    for s in scenarios() {
        let m = measure(&s, iters);
        println!(
            "{:<10} {:>10.1} {:>12.2} {:>12.1} {:>11} {:>10} {:>10}",
            s.name,
            m.ns_per_exec,
            m.allocs_per_exec,
            m.bytes_per_exec,
            m.live_delta,
            m.stats.arena_bytes_reserved,
            m.stats.egress_bytes_reserved
        );
        if m.live_delta != 0 {
            leaks += 1;
        }
    }

    // The batch claim: one reclamation and one egress rewind shared across N
    // items. Same workload, same machine, reported per item so the columns mean
    // the same thing as the table above.
    println!("\nbatch — the `string` scenario, N items per call");
    println!(
        "{:>8} {:>10} {:>12} {:>12} {:>11} {:>10} {:>9}",
        "N", "ns/item", "allocs/item", "bytes/item", "live-delta", "egress-B", "vs loop"
    );
    let bs = scenarios().into_iter().nth(1).expect("the string scenario");
    let loop_ns = measure(&bs, iters).ns_per_exec;
    for n in [1usize, 8, 64, 256] {
        let m = measure_batch(&bs, iters, n);
        println!(
            "{:>8} {:>10.1} {:>12.3} {:>12.2} {:>11} {:>10} {:>8.2}x",
            n,
            m.ns_per_exec,
            m.allocs_per_exec,
            m.bytes_per_exec,
            m.live_delta,
            m.stats.egress_bytes_reserved,
            loop_ns / m.ns_per_exec
        );
        if m.live_delta != 0 {
            leaks += 1;
        }
    }

    // The scaling claim: three sizes, one workload. Time per execution must not
    // climb with the iteration count, and the reserved blocks must not either.
    println!("\nscaling — the `string` scenario at three sizes");
    println!(
        "{:>10} {:>10} {:>12} {:>12} {:>11}",
        "iters", "ns/exec", "egress-B", "high-water", "live-delta"
    );
    let s = scenarios().into_iter().nth(1).expect("the string scenario");
    let mut base_ns = 0.0;
    let mut base_reserved = 0;
    for (i, n) in [1_000usize, 10_000, 100_000].into_iter().enumerate() {
        let m = measure(&s, n);
        println!(
            "{:>10} {:>10.1} {:>12} {:>12} {:>11}",
            n,
            m.ns_per_exec,
            m.stats.egress_bytes_reserved,
            m.stats.egress_high_water,
            m.live_delta
        );
        if i == 0 {
            base_ns = m.ns_per_exec;
            base_reserved = m.stats.egress_bytes_reserved;
        } else {
            // Ten times the work must not cost more than twice as much per
            // item, and must not reserve one extra byte.
            if m.ns_per_exec > base_ns * 2.0 {
                eprintln!(
                    "FAIL: ns/exec grew from {base_ns:.1} to {:.1} over {n} iterations",
                    m.ns_per_exec
                );
                leaks += 1;
            }
            if m.stats.egress_bytes_reserved != base_reserved {
                eprintln!(
                    "FAIL: egress reservation grew from {base_reserved} to {}",
                    m.stats.egress_bytes_reserved
                );
                leaks += 1;
            }
        }
        if m.live_delta != 0 {
            leaks += 1;
        }
    }

    if leaks > 0 {
        eprintln!("\n{leaks} benchmark check(s) failed");
        std::process::exit(1);
    }
    println!("\nall benchmark checks passed");
}
