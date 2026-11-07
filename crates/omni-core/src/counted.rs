//! Test-only counting allocator.
//!
//! Installed as the crate's `#[global_allocator]` under `cfg(test)` so a test can
//! ask two questions the arena's own counters cannot answer: *did this engine
//! leave any bytes on the general heap?* and *did it ask for any at all?*
//! `Arena::bytes_reserved` measures the bump blocks; it cannot see a leaked
//! `Vec`, a `String` that never dropped, or a `Box` forgotten during a refactor —
//! and it cannot see a buffer that is allocated and freed on every execution,
//! which nets to zero live bytes while still costing a malloc per call.
//!
//! The counter is per-thread, which is what makes the assertion meaningful under
//! `cargo test`'s default harness: a global tally would move with every other
//! running test, and the delta would be noise rather than a leak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Safety: the caller upholds `GlobalAlloc`'s contract, and `System` is a
        // valid delegating allocator.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            record(layout.size() as isize);
            count();
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(-(layout.size() as isize));
        // Safety: as above; this is the free that the matching alloc owns.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Safety: as above.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            // Only the delta, because `System.realloc` did not free the old block
            // through `Self::dealloc` — it resized in place or moved it.
            record(new_size as isize - layout.size() as isize);
            count();
        }
        p
    }
}

fn record(delta: isize) {
    LIVE.with(|c| c.set(c.get() + delta));
}

fn count() {
    CALLS.with(|c| c.set(c.get() + 1));
}

/// Bytes this thread has allocated and not yet freed.
pub fn live() -> isize {
    LIVE.with(Cell::get)
}

/// Allocation requests this thread has made, successful `alloc` and `realloc`
/// both counted.
///
/// The second question, and the one `live` cannot answer: a steady state that
/// allocates and frees the same buffer every execution nets to zero live bytes
/// while still paying for a malloc each time. A warm engine that claims to be
/// allocation-free has to show *this* number not moving.
pub fn calls() -> u64 {
    CALLS.with(Cell::get)
}
