//! Deterministic chunked bump allocator.
//!
//! No garbage collector and no reference counting: memory is reclaimed in bulk
//! when the owning engine resets or drops the arena. That is the whole
//! mechanism behind "no GC bottlenecks" — allocation is a pointer increment,
//! and deallocation of a thousand objects costs one `dealloc` per chunk.
//!
//! The invariant that makes raw pointers from this arena meaningful: chunk
//! buffers are never moved or reallocated after creation. Growing the arena
//! pushes a new `Box`ed chunk into `chunks`, which may move the *table of chunk
//! pointers* but never the bytes inside any chunk.

use std::alloc::{Layout, alloc as sys_alloc, dealloc as sys_dealloc};
use std::mem;
use std::ptr;

/// Every chunk is allocated at this alignment, which is the largest alignment
/// any value OmniEngine boxes requires today.
const CHUNK_ALIGN: usize = 16;

const DEFAULT_CHUNK: usize = 64 * 1024;

const MIN_CHUNK: usize = 4096;

/// A single fixed-size block. Held as a raw allocation rather than a
/// `Box<[u8]>` on purpose: reconstructing a `Box<[u8]>` from a 16-aligned
/// allocation makes its `Drop` deallocate with `align_of::<u8>() == 1`, which is
/// undefined behaviour that most allocators get away with and some do not.
struct Chunk {
    ptr: *mut u8,
    layout: Layout,
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // Safety: `layout` is exactly the layout this pointer was produced
        // with, so size and alignment both match, as dealloc requires.
        unsafe { sys_dealloc(self.ptr, self.layout) };
    }
}

/// A bump allocator over fixed-size chunks.
pub struct Arena {
    chunks: Vec<Chunk>,
    chunk_size: usize,
    /// Bytes claimed in the newest chunk. `usize::MAX` means "no chunk yet".
    used: usize,
    allocations: usize,
}

impl Arena {
    pub fn new() -> Arena {
        Arena::with_chunk_size(DEFAULT_CHUNK)
    }

    pub fn with_chunk_size(chunk_size: usize) -> Arena {
        assert!(
            chunk_size >= MIN_CHUNK,
            "chunk_size must be at least one page"
        );
        assert!(
            chunk_size % CHUNK_ALIGN == 0,
            "chunk_size must be 16-aligned"
        );
        Arena {
            chunks: Vec::new(),
            chunk_size,
            used: usize::MAX,
            allocations: 0,
        }
    }

    fn push_chunk(&mut self) {
        let layout =
            Layout::from_size_align(self.chunk_size, CHUNK_ALIGN).expect("valid chunk layout");
        // Safety: a nonzero size with a power-of-two alignment is a valid layout.
        let ptr = unsafe { sys_alloc(layout) };
        assert!(!ptr.is_null(), "arena chunk allocation failed");
        self.chunks.push(Chunk { ptr, layout });
        self.used = 0;
    }

    /// Carve `size` bytes aligned to `align` out of the arena.
    ///
    /// The returned pointer stays valid until [`Arena::reset`] or until the
    /// arena is dropped. A request larger than one chunk minus its alignment is
    /// rejected loudly rather than silently growing the chunk size, because a
    /// hidden grow here would make pointer stability a matter of luck.
    pub fn alloc_raw(&mut self, size: usize, align: usize) -> *mut u8 {
        assert!(size > 0, "zero-sized allocation");
        assert!(align.is_power_of_two(), "align must be a power of two");
        assert!(
            align <= CHUNK_ALIGN,
            "align {align} exceeds chunk alignment"
        );
        assert!(
            size + align <= self.chunk_size,
            "request larger than the chunk; construct with a bigger chunk_size"
        );

        // Worst-case padding is align - 1, so a fresh chunk always fits a
        // request that passed the assert above.
        let fits = self.used != usize::MAX && self.used + size + align <= self.chunk_size;
        if !fits {
            self.push_chunk();
        }

        let base = self.chunks.last().expect("a chunk is present").ptr as usize;
        let raw = base + self.used;
        let aligned = (raw + align - 1) & !(align - 1);
        self.used = (aligned - base) + size;
        self.allocations += 1;
        aligned as *mut u8
    }

    /// Write `value` into the arena and return the address.
    pub fn alloc<T>(&mut self, value: T) -> *mut T {
        let p = self.alloc_raw(mem::size_of::<T>().max(1), mem::align_of::<T>().max(1)) as *mut T;
        // Safety: alloc_raw returned `size_of::<T>()` bytes aligned for T, and
        // ptr::write does not drop the uninitialised memory it overwrites.
        unsafe { ptr::write(p, value) };
        p
    }

    /// Reclaim everything but one warm chunk. Deterministic: callers can rely
    /// on the next allocation landing at the same address as the first ever did.
    pub fn reset(&mut self) {
        if self.chunks.len() > 1 {
            self.chunks.truncate(1);
        }
        if self.chunks.is_empty() {
            self.used = usize::MAX;
        } else {
            self.used = 0;
        }
        self.allocations = 0;
    }

    pub fn allocations(&self) -> usize {
        self.allocations
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Bytes handed out across all live chunks.
    pub fn bytes_in_use(&self) -> usize {
        if self.chunks.is_empty() || self.used == usize::MAX {
            return 0;
        }
        (self.chunks.len() - 1) * self.chunk_size + self.used.min(self.chunk_size)
    }

    /// Total bytes reserved from the system, including unused headroom.
    pub fn bytes_reserved(&self) -> usize {
        self.chunks.len() * self.chunk_size
    }
}

impl Default for Arena {
    fn default() -> Arena {
        Arena::new()
    }
}

// Safety: an Arena is the sole owner of its chunks, so moving it between threads
// transfers unique ownership and is sound. This says nothing about *sharing* one
// arena across threads: an `&mut` alias over a handle handed to a host is a data
// race, and single-thread-per-engine is the documented host contract.
unsafe impl Send for Arena {}

/// A typed cell allocated inside an [`Arena`].
///
/// This exists so `unsafe` stays in this file. The engine reads bindings through
/// `Slot`, and the one rule that keeps it sound is outside the type: every
/// collection of `Slot`s must be dropped before the arena that owns their memory
/// is `reset()` or dropped. `Engine::begin_execution` upholds it by clearing its
/// binding list in the same step as the arena reset, so the two can never be
/// reordered.
pub struct Slot<T> {
    ptr: ptr::NonNull<T>,
}

impl<T: Copy> Slot<T> {
    pub fn new(arena: &mut Arena, value: T) -> Slot<T> {
        let p = arena.alloc(value);
        // Safety: alloc_raw asserts its allocation succeeded, so `p` is non-null,
        // correctly aligned, and points into a live chunk.
        Slot {
            ptr: unsafe { ptr::NonNull::new_unchecked(p) },
        }
    }

    pub fn get(&self) -> T {
        // Safety: see the type's documented rule.
        unsafe { *self.ptr.as_ptr() }
    }

    pub fn set(&mut self, value: T) {
        // Safety: as above; T is Copy so the overwritten value needs no drop.
        unsafe { ptr::write(self.ptr.as_ptr(), value) };
    }
}

impl<T> Clone for Slot<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Slot<T> {}

// Safety: same reasoning as Arena — a Slot is a view into memory the Arena owns
// and moves with it.
unsafe impl<T> Send for Slot<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    #[test]
    fn allocations_are_aligned() {
        let mut a = Arena::with_chunk_size(4096);
        for align in [1usize, 2, 4, 8, 16] {
            for _ in 0..64 {
                let p = a.alloc_raw(7, align);
                assert_eq!(p as usize % align, 0, "align {align} violated");
            }
        }
    }

    #[test]
    fn reset_is_deterministic_and_reuses_memory() {
        let mut a = Arena::new();
        let first = a.alloc::<u64>(1);
        let _ = a.alloc::<u64>(2);
        a.reset();
        assert_eq!(a.allocations(), 0);
        assert_eq!(a.bytes_in_use(), 0);
        // Same address after reset: this is a bump pointer, not a pool.
        assert_eq!(a.alloc::<u64>(3), first);
    }

    #[test]
    fn reset_before_any_allocation_is_a_no_op() {
        let mut a = Arena::new();
        a.reset();
        assert_eq!(a.bytes_in_use(), 0);
        assert_eq!(a.chunk_count(), 0);
        assert!(!a.alloc::<u64>(9).is_null());
    }

    #[test]
    fn crosses_into_a_new_chunk_without_invalidating_earlier_pointers() {
        let mut a = Arena::with_chunk_size(4096);
        let p = a.alloc::<u64>(0xDEAD_BEEF);
        for _ in 0..1000 {
            let _ = a.alloc::<u64>(0);
        }
        assert!(a.chunk_count() > 1, "expected chunk growth");
        // Safety: p points into chunk 0, which is neither moved nor freed.
        assert_eq!(unsafe { *p }, 0xDEAD_BEEF, "chunk growth moved data");
    }

    #[test]
    fn typed_alloc_writes_the_value() {
        let mut a = Arena::new();
        let p = a.alloc(Value::from_f64(42.0));
        // Safety: freshly allocated and written by alloc.
        assert_eq!(unsafe { *p }, Value::from_f64(42.0));
    }

    #[test]
    fn accounting_tracks_live_bytes() {
        let mut a = Arena::with_chunk_size(4096);
        assert_eq!(a.bytes_reserved(), 0);
        let _ = a.alloc_raw(100, 8);
        assert_eq!(a.bytes_reserved(), 4096);
        assert!(a.bytes_in_use() >= 100);
        assert_eq!(a.allocations(), 1);
    }

    #[test]
    #[should_panic(expected = "larger than the chunk")]
    fn oversized_request_is_rejected_not_silently_grown() {
        let mut a = Arena::with_chunk_size(4096);
        a.alloc_raw(8192, 8);
    }

    #[test]
    #[should_panic(expected = "align must be a power of two")]
    fn non_power_of_two_align_is_rejected() {
        let mut a = Arena::new();
        a.alloc_raw(8, 3);
    }

    #[test]
    #[should_panic(expected = "at least one page")]
    fn tiny_chunks_are_rejected_at_construction() {
        Arena::with_chunk_size(64);
    }
}
