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
//!
//! ## The two ways to give memory back
//!
//! [`Arena::reset`] is reclamation: every chunk but one warm chunk is returned to
//! the system, so any pointer into it is dangling. It is correct for a block
//! whose tenants are all dropped in the same step — the execution arena and the
//! engine's binding list.
//!
//! [`Arena::rewind`] is reuse: the bump cursor returns to the start of the first
//! chunk and **no memory is unmapped**. A pointer handed out before a rewind
//! still addresses live, owned bytes containing whatever the next allocations
//! leave in them. That is the whole reason strings that face a foreign host are
//! promoted into a rewound block: the failure mode for a caller that ignores the
//! expiry rule becomes a stale read rather than a use-after-free.
//!
//! [`Arena::mark`] and [`Arena::reset_to`] are the third thing a batch needs:
//! reclaim *back to* a checkpoint instead of back to the beginning, so a run of
//! executions can keep one warm chunk and one egress rewind between them.

use std::alloc::{Layout, alloc as sys_alloc, dealloc as sys_dealloc};
use std::mem;
use std::ptr;

/// Every chunk is allocated at this alignment, which is the largest alignment
/// any value OmniEngine boxes requires today.
const CHUNK_ALIGN: usize = 16;

const DEFAULT_CHUNK: usize = 64 * 1024;

const MIN_CHUNK: usize = 4096;

/// A single block. Held as a raw allocation rather than a `Box<[u8]>` on
/// purpose: reconstructing a `Box<[u8]>` from a 16-aligned allocation makes its
/// `Drop` deallocate with `align_of::<u8>() == 1`, which is undefined behaviour
/// that most allocators get away with and some do not.
struct Chunk {
    ptr: *mut u8,
    layout: Layout,
    /// Only meaningful for an oversize block: `true` once [`Arena::rewind`] has
    /// declared every previous claim on it dead, so the next large request can
    /// reuse the mapping instead of asking the system for another one. Bump
    /// chunks are reused by the cursor walking them in order, not by a flag, and
    /// always carry `false`.
    free: bool,
}

impl Chunk {
    #[inline]
    fn size(&self) -> usize {
        self.layout.size()
    }

    /// True when the whole span `addr..addr + len` lies inside this block.
    ///
    /// Written to avoid overflow rather than to look tidy: a hostile `len` near
    /// `usize::MAX` must fail the bound, not wrap through it.
    #[inline]
    fn contains(&self, addr: usize, len: usize) -> bool {
        let start = self.ptr as usize;
        if addr < start {
            return false;
        }
        let offset = addr - start;
        offset <= self.layout.size() && self.layout.size() - offset >= len
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // Safety: `layout` is exactly the layout this pointer was produced
        // with, so size and alignment both match, as dealloc requires.
        unsafe { sys_dealloc(self.ptr, self.layout) };
    }
}

/// A checkpoint of an arena's bump cursor: how many chunks it held, which one it
/// was filling, and how far into that one it had reached.
///
/// Oversize blocks are deliberately not part of it. [`Arena::reset_to`] reclaims
/// them all, exactly as [`Arena::reset`] does, because a count cannot record
/// *which* large blocks were spare and which were live — restoring the count
/// alone would either leak a claim or hand a live block's bytes to the next
/// large request. A batch checkpoints right after a reset, when there are none.
///
/// The rule that keeps a mark honest, like [`Slot`]'s: it belongs to the arena
/// that produced it and is only meaningful while that arena is alive. Restoring
/// it reclaims everything allocated since, so any pointer handed out after the
/// mark is dangling. A mark taken from a *different* arena, or one whose chunks
/// have since been reset away, is clamped to what this arena actually holds —
/// the worst outcome is a wasted chunk, never an out-of-bounds cursor, because
/// [`Arena::alloc_raw`] re-checks the fit before every allocation.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    chunks: usize,
    current: usize,
    used: usize,
}

/// A bump allocator over fixed-size chunks, with an escape hatch for objects
/// larger than a chunk.
pub struct Arena {
    chunks: Vec<Chunk>,
    /// Blocks too large for a chunk, each in an allocation of its own exact
    /// size. Kept out of `chunks` so the bump cursor never has to think about
    /// them, while pointer stability still holds because nothing here is ever
    /// moved.
    oversize: Vec<Chunk>,
    chunk_size: usize,
    /// Index of the chunk the cursor is filling. Always the last one, except
    /// after a [`Arena::rewind`] of a multi-chunk arena: the cursor goes back to
    /// chunk 0 and walks the chunks already mapped before asking for another.
    /// Without that walk, a cycle needing two chunks would map two fresh ones
    /// every time and `bytes_reserved` would grow without bound.
    current: usize,
    /// Bytes claimed in the chunk at `current`. `usize::MAX` means "no chunk yet".
    used: usize,
    /// Bytes held by *all* oversize blocks, spare ones included. This is what the
    /// system has given us and therefore what `bytes_reserved` reports.
    oversize_held: usize,
    /// Bytes currently claimed by live oversize blocks — `oversize_held` minus any
    /// spare awaiting reuse after a rewind.
    oversize_live: usize,
    allocations: usize,
    high_water: usize,
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
            oversize: Vec::new(),
            chunk_size,
            current: 0,
            used: usize::MAX,
            oversize_held: 0,
            oversize_live: 0,
            allocations: 0,
            high_water: 0,
        }
    }

    fn push_chunk(&mut self) {
        let layout =
            Layout::from_size_align(self.chunk_size, CHUNK_ALIGN).expect("valid chunk layout");
        // Safety: a nonzero size with a power-of-two alignment is a valid layout.
        let ptr = unsafe { sys_alloc(layout) };
        assert!(!ptr.is_null(), "arena chunk allocation failed");
        self.chunks.push(Chunk {
            ptr,
            layout,
            free: false,
        });
        self.current = self.chunks.len() - 1;
        self.used = 0;
    }

    /// Carve `size` bytes aligned to `align` out of the arena.
    ///
    /// The returned pointer stays valid until [`Arena::reset`] or until the
    /// arena is dropped; [`Arena::rewind`] keeps it valid. A request larger than
    /// the chunk size gets its own exact-size block rather than a panic or a
    /// silently grown chunk — a 16 MiB string is a legitimate program, and a
    /// hidden grow would make pointer stability a matter of luck.
    pub fn alloc_raw(&mut self, size: usize, align: usize) -> *mut u8 {
        assert!(size > 0, "zero-sized allocation");
        assert!(align.is_power_of_two(), "align must be a power of two");
        assert!(
            align <= CHUNK_ALIGN,
            "align {align} exceeds chunk alignment"
        );

        // Worst-case padding is align - 1, so a request that fits the chunk
        // always fits a fresh one.
        if size + align > self.chunk_size {
            return self.alloc_oversize(size, align);
        }

        let fits = self.used != usize::MAX && self.used + size + align <= self.chunk_size;
        if !fits {
            // A chunk already mapped by an earlier cycle is preferred to a new
            // one: every chunk is `chunk_size`, so the request that failed to fit
            // the exhausted one fits the next, and a rewound arena stays bounded
            // by its largest cycle rather than by how many cycles ran.
            if self.current + 1 < self.chunks.len() {
                self.current += 1;
                self.used = 0;
            } else {
                self.push_chunk();
            }
        }

        let base = self.chunks[self.current].ptr as usize;
        let raw = base + self.used;
        let aligned = (raw + align - 1) & !(align - 1);
        self.used = (aligned - base) + size;
        self.allocations += 1;
        self.track_high_water();
        aligned as *mut u8
    }

    /// One allocation, one block — unless a spare from an earlier [`rewind`](Arena::rewind)
    /// is big enough, in which case that mapping is reused.
    ///
    /// The reuse is not an optimisation, it is what makes a rewound block
    /// *bounded*: an arena that rewinds forever, handing out a fresh 100 KiB block
    /// to each cycle, would grow `bytes_reserved` without limit and contradict the
    /// one-machine-high-water story the egress model tells.
    fn alloc_oversize(&mut self, size: usize, align: usize) -> *mut u8 {
        // Round up to the chunk alignment so `contains` and the accounting both see
        // a whole number of aligned units, and so the pointer we hand back can
        // never be short of the requested alignment.
        let rounded = size.next_multiple_of(CHUNK_ALIGN.max(align));
        let mut spare: Option<usize> = None;
        for (idx, chunk) in self.oversize.iter().enumerate() {
            if chunk.free
                && chunk.size() >= rounded
                && spare.is_none_or(|best| self.oversize[best].size() > chunk.size())
            {
                spare = Some(idx);
            }
        }
        if let Some(idx) = spare {
            let size = self.oversize[idx].size();
            self.oversize[idx].free = false;
            self.oversize_live += size;
            self.allocations += 1;
            self.track_high_water();
            return self.oversize[idx].ptr;
        }
        let layout = Layout::from_size_align(rounded, CHUNK_ALIGN.max(align))
            .expect("oversize layout is valid: nonzero and power-of-two aligned");
        // Safety: a nonzero size with a power-of-two alignment is a valid layout.
        let ptr = unsafe { sys_alloc(layout) };
        assert!(!ptr.is_null(), "arena oversize allocation failed");
        self.oversize.push(Chunk {
            ptr,
            layout,
            free: false,
        });
        self.oversize_held += rounded;
        self.oversize_live += rounded;
        self.allocations += 1;
        self.track_high_water();
        ptr
    }

    fn track_high_water(&mut self) {
        self.high_water = self.high_water.max(self.bytes_in_use());
    }

    /// Write `value` into the arena and return the address.
    pub fn alloc<T>(&mut self, value: T) -> *mut T {
        let p = self.alloc_raw(mem::size_of::<T>().max(1), mem::align_of::<T>().max(1)) as *mut T;
        // Safety: alloc_raw returned `size_of::<T>()` bytes aligned for T, and
        // ptr::write does not drop the uninitialised memory it overwrites.
        unsafe { ptr::write(p, value) };
        p
    }

    /// Reclaim everything but one warm chunk. Deterministic: callers can rely on
    /// the next allocation landing at the same address as the first ever did.
    ///
    /// This *invalidates every pointer*, so it may only be used for a block
    /// whose tenants are dropped in the same step. See the module header.
    pub fn reset(&mut self) {
        self.reset_to(Mark {
            chunks: self.chunks.len().min(1),
            current: 0,
            used: 0,
        });
    }

    /// Record the bump cursor, for [`Arena::reset_to`].
    ///
    /// Takes nothing and allocates nothing: three `usize`s copied out. That is
    /// what makes it cheap enough to sit at the head of a batch and be restored
    /// before every item. `Copy` for the same reason — one mark, many restores.
    pub fn mark(&self) -> Mark {
        Mark {
            chunks: self.chunks.len(),
            current: self.current,
            used: self.used,
        }
    }

    /// Reclaim every chunk allocated since `mark`, and the cursor back to where
    /// it stood. Chunks the mark still names are kept, so the memory the batch
    /// warmed stays mapped and the next allocation lands where it did before.
    /// Oversize blocks are all reclaimed, as by [`Arena::reset`]; see [`Mark`].
    ///
    /// Like `reset` this invalidates pointers handed out after the mark, and like
    /// `reset` it leaves [`Arena::high_water`] alone: the watermark describes the
    /// largest cycle ever run, not this one.
    pub fn reset_to(&mut self, mark: Mark) {
        self.chunks.truncate(mark.chunks.min(self.chunks.len()));
        self.oversize.clear();
        self.oversize_held = 0;
        self.oversize_live = 0;
        // A mark taken before the first chunk existed carries `usize::MAX`;
        // restoring it means "nothing claimed yet", not "this chunk is full".
        let used = if mark.used == usize::MAX {
            0
        } else {
            mark.used.min(self.chunk_size)
        };
        self.used = if self.chunks.is_empty() {
            usize::MAX
        } else {
            used
        };
        self.current = mark.current.min(self.chunks.len().saturating_sub(1));
        self.allocations = 0;
    }

    /// Rewind the bump cursor to zero and free nothing.
    ///
    /// Pointers handed out before this call stay inside live, owned memory for
    /// the rest of the arena's life; the contents may be overwritten by later
    /// allocations. Use it for a block whose pointers face a foreign host.
    ///
    /// Oversize blocks are not unmapped either — they become spares, which is the
    /// difference between "a stale string is a wrong read" and "a stale string is
    /// a segfault", and the reason a long-running host's reserved bytes flatten out
    /// instead of climbing.
    pub fn rewind(&mut self) {
        if !self.chunks.is_empty() {
            self.current = 0;
            self.used = 0;
        }
        self.allocations = 0;
        for chunk in &mut self.oversize {
            chunk.free = true;
        }
        self.oversize_live = 0;
    }

    /// True when the whole span `addr..addr + len` lies inside one live block.
    ///
    /// Checking the span rather than the first byte is what makes
    /// [`crate::object::StrRef::checked`] able to hand back a borrowed slice: a
    /// pointer that starts inside a chunk but whose length runs off the end is
    /// rejected, not clipped.
    pub fn owns_span(&self, addr: usize, len: usize) -> bool {
        self.chunks.iter().any(|c| c.contains(addr, len))
            || self.oversize.iter().any(|c| c.contains(addr, len))
    }

    /// `len` bytes at `addr`, borrowed from this arena, or `None` when the span
    /// is not inside one live block.
    pub(crate) fn span(&self, addr: usize, len: usize) -> Option<&[u8]> {
        if !self.owns_span(addr, len) {
            return None;
        }
        // Safety: owns_span proved the range is inside a block this arena holds
        // allocated, and no `&mut Arena` can exist while this `&Arena` does, so
        // nothing can bump or free the bytes out from under the returned slice.
        Some(unsafe { std::slice::from_raw_parts(addr as *const u8, len) })
    }

    /// Read two `u32`s without assuming the memory was ever written.
    ///
    /// This is the reason the header peek is sound. A hostile address can land in
    /// a chunk nobody ever wrote, and reading uninitialised memory through a
    /// typed pointer is undefined behaviour. Going through `MaybeUninit` makes
    /// the read itself defined, and `assume_init` on the result is sound because
    /// every bit pattern of `u32` is a valid `u32` — there is no such thing as an
    /// invalid integer to be initialized "wrong".
    pub(crate) fn peek_u32s(&self, addr: usize) -> Option<[u32; 2]> {
        use std::mem::MaybeUninit;
        if !self.owns_span(addr, 8) {
            return None;
        }
        let p = addr as *const MaybeUninit<u32>;
        // Safety: owns_span proved eight readable bytes, `read_unaligned` imposes
        // no alignment requirement, and reading into MaybeUninit places no
        // initialisation obligation on the source.
        Some(unsafe {
            [
                p.read_unaligned().assume_init(),
                p.add(1).read_unaligned().assume_init(),
            ]
        })
    }

    /// True when `addr` lies inside a block this arena currently owns.
    ///
    /// This is what turns "the caller handed me a stale or fabricated pointer"
    /// into a checkable condition rather than a dereference. It is deliberately
    /// conservative: after [`Arena::reset`] a freed address is not owned, while
    /// after [`Arena::rewind`] a reused address still is.
    pub fn owns(&self, addr: usize) -> bool {
        self.owns_span(addr, 1)
    }

    pub fn allocations(&self) -> usize {
        self.allocations
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Blocks that did not fit a chunk and took an allocation of their own.
    pub fn oversize_count(&self) -> usize {
        self.oversize.len()
    }

    /// Bytes handed out across all live blocks: the bump cursor's claim plus every
    /// oversize block that has not been released by a rewind.
    pub fn bytes_in_use(&self) -> usize {
        let bump = if self.chunks.is_empty() || self.used == usize::MAX {
            0
        } else {
            (self.current * self.chunk_size) + self.used.min(self.chunk_size)
        };
        bump + self.oversize_live
    }

    /// Total bytes reserved from the system, including unused headroom and spares
    /// kept for reuse. This is the number that must stop growing for the memory
    /// story to be true.
    pub fn bytes_reserved(&self) -> usize {
        self.chunks.iter().map(Chunk::size).sum::<usize>() + self.oversize_held
    }

    /// The most bytes ever handed out, which is what a rewound block is bounded
    /// by: not by how many executions ran, but by the largest one.
    pub fn high_water(&self) -> usize {
        self.high_water
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
/// is `reset()` or dropped. `Engine::reclaim` upholds it by clearing its
/// binding list in the same step as the arena reset, so the two can never be
/// reordered. `Engine::exec_many` upholds it per item, against `reset_to`.
///
/// `Slot` is for the *reset* block. A `Value` that addresses the rewound egress
/// block is not a `Slot`; it is read through [`crate::object::StrRef`], under the
/// different and weaker rule stated there.
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
    use crate::object::{StrRef, build};

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

    /// What a batch is built on: the checkpoint keeps the memory the batch
    /// warmed, and every item starts allocating at the address the first one did.
    #[test]
    fn reset_to_a_mark_reuses_the_block_the_mark_named() {
        let mut a = Arena::with_chunk_size(4096);
        let kept = a.alloc::<u64>(7);
        let mark = a.mark();
        let claimed = a.bytes_in_use();
        let next = a.alloc::<u64>(0);
        let reserved = a.bytes_reserved();
        for round in 0..8 {
            a.reset_to(mark);
            assert_eq!(a.allocations(), 0, "round {round}");
            assert_eq!(
                a.bytes_in_use(),
                claimed,
                "round {round}: only what the mark named is still claimed"
            );
            assert_eq!(
                a.bytes_reserved(),
                reserved,
                "round {round}: nothing mapped"
            );
            assert!(
                a.owns(kept as usize),
                "round {round}: the marked block lives"
            );
            assert_eq!(a.alloc::<u64>(round), next, "round {round}: same address");
        }
    }

    /// A mark taken on a cold arena names no chunks, so restoring it gives the
    /// chunk back too. This is the first batch's cost: one chunk per item until
    /// an item leaves one mapped, after which every later batch reuses it.
    #[test]
    fn a_mark_taken_before_any_allocation_reclaims_the_chunk_it_did_not_name() {
        let mut a = Arena::with_chunk_size(4096);
        let mark = a.mark();
        let first = a.alloc::<u64>(1);
        a.reset_to(mark);
        assert_eq!(a.chunk_count(), 0);
        assert_eq!(a.bytes_reserved(), 0);
        assert_eq!(a.bytes_in_use(), 0);
        assert!(!a.owns(first as usize));
        assert!(
            !a.alloc::<u64>(2).is_null(),
            "and the arena is still usable"
        );
    }

    #[test]
    fn reset_to_reclaims_the_chunks_and_oversize_blocks_added_since() {
        let mut a = Arena::with_chunk_size(4096);
        let kept = a.alloc::<u64>(1);
        let mark = a.mark();
        for _ in 0..1000 {
            let _ = a.alloc::<u64>(0);
        }
        let big = a.alloc_raw(20_000, 8);
        assert!(a.chunk_count() > 1);
        assert_eq!(a.oversize_count(), 1);
        a.reset_to(mark);
        assert_eq!(a.chunk_count(), 1, "the marked chunk stays, the rest go");
        assert_eq!(a.oversize_count(), 0);
        assert_eq!(a.bytes_reserved(), 4096);
        assert_eq!(a.bytes_in_use(), 8);
        assert!(a.owns(kept as usize));
        assert!(!a.owns(big as usize));
    }

    /// The clamp, stated as a test rather than trusted: a mark whose cursor sits
    /// beyond the end of this arena's chunk must not be believed, or the next
    /// allocation would write past the block.
    #[test]
    fn a_mark_from_a_larger_arena_is_clamped_not_believed() {
        let mut big = Arena::with_chunk_size(8192);
        let _ = big.alloc_raw(5000, 8);
        let foreign = big.mark();
        let mut small = Arena::with_chunk_size(4096);
        let _ = small.alloc::<u64>(0);
        small.reset_to(foreign);
        assert_eq!(small.chunk_count(), 1, "the block it already held survives");
        let p = small.alloc::<u64>(1);
        assert!(
            small.owns(p as usize),
            "the cursor was clamped, not trusted"
        );
        assert_eq!(
            small.chunk_count(),
            2,
            "a full chunk grows, it does not overflow"
        );
    }

    /// Oversize is not checkpointed, so a large block is reclaimed even when the
    /// mark was taken while it sat there as a spare. Stated because the
    /// alternative — restoring a block count — cannot tell spare from live.
    #[test]
    fn reset_to_reclaims_oversize_even_when_the_mark_predates_the_rewind() {
        let mut a = Arena::with_chunk_size(4096);
        let payload = vec![0x41u8; 100_000];
        let first = build(&mut a, &payload);
        a.rewind();
        let mark = a.mark();
        assert_eq!(build(&mut a, &payload), first, "the spare was reused");
        a.reset_to(mark);
        assert_eq!(a.oversize_count(), 0);
        assert_eq!(a.bytes_reserved(), 0);
        assert!(!a.owns(first as usize));
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

    /// The gap that had to close before any string test could exist: a request
    /// bigger than a chunk used to panic. A 16 MiB literal is a program.
    #[test]
    fn a_request_larger_than_a_chunk_gets_its_own_block() {
        let mut a = Arena::with_chunk_size(4096);
        let payload = vec![0xABu8; 300_000];
        let p = build(&mut a, &payload);
        assert_eq!(a.chunk_count(), 0, "the bump chunks were never needed");
        assert_eq!(a.oversize_count(), 1);
        let read = StrRef::checked(Value::from_ptr(p), &a).expect("a string was built");
        assert_eq!(read.as_bytes(), payload.as_slice());
        assert!(a.bytes_reserved() >= 300_000);
    }

    #[test]
    fn oversize_and_bump_blocks_are_both_owned_and_distinct() {
        let mut a = Arena::with_chunk_size(4096);
        let small = a.alloc::<u64>(7) as usize;
        let big = a.alloc_raw(20_000, 8) as usize;
        assert!(a.owns(small));
        assert!(a.owns(big));
        assert!(!a.owns(0x1));
        assert!(!a.owns(small + a.bytes_reserved() * 2));
    }

    #[test]
    fn reset_frees_oversize_but_rewind_does_not() {
        let mut a = Arena::with_chunk_size(4096);
        let p = a.alloc_raw(20_000, 8);
        assert!(a.bytes_reserved() >= 20_000);
        a.rewind();
        assert_eq!(a.allocations(), 0);
        assert!(
            a.owns(p as usize),
            "rewind must not unmap anything: that is its whole purpose"
        );
        assert!(a.bytes_reserved() >= 20_000, "rewind freed the block");
        assert_eq!(
            a.bytes_in_use(),
            0,
            "released memory must read as released, even though it stays mapped"
        );
        a.reset();
        assert!(!a.owns(p as usize), "reset does reclaim oversize blocks");
        assert_eq!(a.oversize_count(), 0);
        assert_eq!(a.bytes_in_use(), 0);
    }

    /// What keeps a rewinding arena honest. Without this reuse, a host that
    /// `exec`s a 100 KiB string in a loop would reserve another block every time
    /// and the "bounded by one execution's high-water mark" claim would be false
    /// in exactly the case it was written for.
    #[test]
    fn a_rewound_oversize_block_is_reused_instead_of_reallocated() {
        let mut a = Arena::with_chunk_size(4096);
        let payload = vec![0x41u8; 100_000];
        let first = build(&mut a, &payload);
        let reserved = a.bytes_reserved();
        for round in 0..8 {
            a.rewind();
            let again = build(&mut a, &payload);
            assert_eq!(again, first, "round {round}: the spare block is the answer");
            assert_eq!(a.oversize_count(), 1, "round {round}: no new block");
            assert_eq!(
                a.bytes_reserved(),
                reserved,
                "round {round}: nothing new mapped"
            );
            assert!(
                a.bytes_in_use() >= 100_000,
                "the live block is counted live"
            );
        }
    }

    /// The same claim for the bump path, which a batch is what makes reachable:
    /// eight 1500-byte results in one cycle need four 4 KiB chunks, and the next
    /// cycle must walk those four rather than map four more. A cursor that only
    /// ever filled the newest chunk grew `bytes_reserved` by a whole cycle every
    /// time, which is a leak wearing a rewind's clothes.
    #[test]
    fn a_rewound_multi_chunk_cycle_reuses_every_chunk_it_mapped() {
        let mut a = Arena::with_chunk_size(4096);
        let cycle =
            |a: &mut Arena| -> Vec<*mut u8> { (0..8).map(|_| a.alloc_raw(1500, 8)).collect() };
        let first = cycle(&mut a);
        let reserved = a.bytes_reserved();
        let claimed = a.bytes_in_use();
        let chunks = a.chunk_count();
        assert_eq!(chunks, 4, "two 1500-byte claims fit one 4096-byte chunk");
        for round in 0..8 {
            a.rewind();
            assert_eq!(a.bytes_in_use(), 0, "round {round}: nothing claimed");
            let again = cycle(&mut a);
            assert_eq!(again, first, "round {round}: the same addresses, in order");
            assert_eq!(a.chunk_count(), chunks, "round {round}: no new chunk");
            assert_eq!(
                a.bytes_reserved(),
                reserved,
                "round {round}: nothing mapped"
            );
            assert_eq!(a.bytes_in_use(), claimed, "round {round}: same demand");
        }
    }

    #[test]
    fn a_spare_block_too_small_for_the_request_is_not_misused() {
        let mut a = Arena::with_chunk_size(4096);
        let small = build(&mut a, &vec![0u8; 10_000]);
        a.rewind();
        let big = build(&mut a, &vec![0u8; 200_000]);
        assert_ne!(small, big, "a 10 KiB spare cannot hold a 200 KiB string");
        assert_eq!(a.oversize_count(), 2);
        // The smaller one is still free, so the next small request gets it back.
        a.rewind();
        assert_eq!(build(&mut a, &vec![0u8; 10_000]), small);
    }

    /// The safety property the egress model is built on, stated at the level of
    /// the allocator rather than the engine.
    #[test]
    fn a_pointer_survives_a_rewind_and_can_be_read() {
        let mut a = Arena::with_chunk_size(4096);
        let p = build(&mut a, b"still here");
        a.rewind();
        // rewind unmapped nothing, so the address is still owned and readable.
        let read = StrRef::checked(Value::from_ptr(p), &a).expect("owned");
        assert_eq!(read.as_bytes(), b"still here");
    }

    #[test]
    fn high_water_records_the_largest_cycle_not_the_total() {
        let mut a = Arena::with_chunk_size(4096);
        for _ in 0..10 {
            let _ = a.alloc_raw(100, 8);
        }
        let peak = a.high_water();
        a.rewind();
        let _ = a.alloc_raw(8, 8);
        assert!(a.bytes_in_use() < peak);
        assert_eq!(
            a.high_water(),
            peak,
            "high water is a watermark, not a running total"
        );
    }

    #[test]
    #[should_panic(expected = "at least one page")]
    fn tiny_chunks_are_rejected_at_construction() {
        Arena::with_chunk_size(64);
    }

    #[test]
    #[should_panic(expected = "align must be a power of two")]
    fn non_power_of_two_align_is_rejected() {
        let mut a = Arena::new();
        a.alloc_raw(8, 3);
    }
}
