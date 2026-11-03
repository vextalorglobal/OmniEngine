# OmniEngine Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the OmniEngine repository at `D:\OmniEngine` and prove one `.omni` source file evaluates to `42` through a Rust CLI, a C ABI cdylib consumed by Python `ctypes`, and the same cdylib consumed by Node `koffi`.

**Architecture:** A Cargo workspace whose `omni-core` crate owns a NaN-boxed 8-byte `Value` and a bump `Arena`, and whose `omni-ffi` crate is the only place naming a `#[repr(C)]` type or an `extern "C"` function. `omni-cli` and `omni-wasm` are thin consumers of `omni-core`, so the core cannot accidentally become FFI-shaped. No arena pointer crosses the C boundary; results return by value and every entry point runs inside `catch_unwind`.

**Tech Stack:** Rust 1.99 stable, edition 2024, resolver 3. Python 3.15 stdlib `ctypes`. Node 26 + `koffi`. Git 2.52 with `.gitattributes` Linguist overrides.

**Spec:** `docs/superpowers/specs/2026-11-02-omniengine-phase1-design.md` (commit `7f48cdb`). Read both together.

## Global Constraints

- Toolchain pinned `stable-x86_64-pc-windows-msvc`; `x86_64-pc-windows-gnu` is the documented fallback if the MSVC link fails.
- `omni-core` has zero external crate dependencies. Standard library only.
- `omni-ffi` is the only crate permitted to declare `extern "C"` or `#[repr(C)]`.
- `unsafe` is confined to `omni-core/src/arena.rs` and `omni-ffi/src/lib.rs`.
- Every `extern "C"` function body is wrapped in `std::panic::catch_unwind`.
- Dev profile uses `debug = "line-tables-only"`; release uses `lto = "thin"`, `codegen-units = 1`.
- `CARGO_HOME` is NOT modified. It stays on C:.
- Extension is `.omni`. Python SDK has no pip dependencies.
- No arena pointer or Rust-owned borrow crosses the C boundary.
- `Cargo.lock` is committed (the workspace ships a binary).
- `/sdks/**` and `/docs/**` never count toward GitHub language statistics.

## Recorded deviations from the spec

1. **`Engine` carries no lifetime.** The spec's layout implied `Engine<'a>` borrowing C-supplied source. A borrowed engine cannot outlive the call that supplied the bytes, so an opaque boxed handle would be unsound across two calls. `Engine` owns `String` keys instead. Cost is a heap allocation per identifier at Phase 1 scale; it is invisible next to lexing.
2. **`OmniValue` is `{ tag, number }`, not a union.** No string values exist yet, so a `union` with a `string_ptr` arm would be an unpopulated variant — dead code that also adds an unsafe read path to every host. The union is deferred with strings in Phase 2. Task 6 amends the spec so the docs stay truthful.

## File Map

| File | Responsibility |
|---|---|
| `Cargo.toml` | workspace members, `default-members` excluding wasm, shared profile policy |
| `rust-toolchain.toml` | pin channel + host triple |
| `.cargo/config.toml` | build jobs, incremental, Windows subsystem for tests |
| `.gitignore` | never commit `target/` |
| `.gitattributes` | CRLF normalization + Linguist tier 1 |
| `crates/omni-core/src/value.rs` | NaN-boxed `Value`; classification order, pointer region |
| `crates/omni-core/src/arena.rs` | chunked bump allocator, `reset`, dealloc with matching `Layout` |
| `crates/omni-core/src/lexer.rs` | `&str` -> `Vec<Token>` borrowing source, zero allocation |
| `crates/omni-core/src/engine.rs` | fused parser/evaluator, `Scope`, `OmniError` |
| `crates/omni-core/src/lib.rs` | public surface, re-exports |
| `crates/omni-ffi/src/lib.rs` | C ABI, `catch_unwind`, by-value results |
| `crates/omni-ffi/include/omni.h` | hand-written C header mirroring the ABI |
| `crates/omni-ffi/cbindgen.toml` | config so the header can be generated later |
| `crates/omni-cli/src/main.rs` | `run <file>` / `eval <expr>` |
| `crates/omni-wasm/src/lib.rs` | scaffolded wasm-bindgen surface |
| `sdks/python/omni/_ffi.py` | ctypes loading, prototypes, `Engine` |
| `sdks/nodejs/lib/bindings.js` | koffi loading, struct + prototype declarations |
| `tests/test_ffi.py`, `tests/test_ffi.js` | cross-boundary acceptance |
| `docs/GUIDE.md` | the initialization and execution guide deliverable |

---

### Task 1: Workspace skeleton and build policy

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.cargo/config.toml`, `.gitattributes`
- Create: `crates/omni-core/Cargo.toml`, `crates/omni-core/src/lib.rs`
- Create: `crates/omni-ffi/Cargo.toml`, `crates/omni-ffi/src/lib.rs`
- Create: `crates/omni-cli/Cargo.toml`, `crates/omni-cli/src/main.rs`
- Create: `crates/omni-wasm/Cargo.toml`, `crates/omni-wasm/src/lib.rs`

**Interfaces:**
- Produces: workspace members `omni-core`, `omni-ffi`, `omni-cli`, `omni-wasm`; crate versions `0.1.0`; edition `2024`.

- [ ] **Step 1: Write the workspace manifest.** `default-members` excludes `omni-wasm` so `cargo build` never needs the missing `wasm32-unknown-unknown` target.

```toml
[workspace]
resolver = "3"
members = ["crates/omni-core", "crates/omni-ffi", "crates/omni-cli", "crates/omni-wasm"]
default-members = ["crates/omni-core", "crates/omni-ffi", "crates/omni-cli"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"
repository = "https://github.com/fahim5536/omniengine"

[workspace.dependencies]
omni-core = { path = "crates/omni-core" }

# Dev is tuned for compile speed and, critically, for the 7.5 GB free on C:.
# Full debug info on MSVC emits a multi-gigabyte PDB per crate; line tables
# keep backtraces readable without paying that.
[profile.dev]
opt-level = 0
debug = "line-tables-only"
incremental = true
codegen-units = 256
panic = "unwind"          # load-bearing: omni-ffi relies on catch_unwind

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "unwind"          # never "abort" — it would bypass catch_unwind
strip = "debuginfo"
```

- [ ] **Step 2: Pin the toolchain and add the four crate manifests.**

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
targets = ["x86_64-pc-windows-msvc"]
```

`.cargo/config.toml`:
```toml
[build]
# One shared target/ for the whole workspace, and it lives on D: because the
# repo does. Do not set target-dir here: a relative path would pin artifacts
# into the repo, which is what we want; an absolute one would break CI.
```

`crates/omni-core/Cargo.toml` — no dependencies at all:
```toml
[package]
name = "omni-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]

[lints.rust]
unsafe_op_in_unsafe_fn = "deny"
missing_docs = "warn"
```

`crates/omni-ffi/Cargo.toml`:
```toml
[package]
name = "omni-ffi"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib", "staticlib", "rlib"]

[dependencies]
omni-core.workspace = true
```

`crates/omni-cli/Cargo.toml`:
```toml
[package]
name = "omni-cli"
version.workspace = true
edition.workspace = true

[[bin]]
name = "omni"
path = "src/main.rs"

[dependencies]
omni-core.workspace = true
```

`crates/omni-wasm/Cargo.toml` is scaffolded, not built by default:
```toml
[package]
name = "omni-wasm"
version.workspace = true
edition.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
omni-core.workspace = true
# Pinned optional so `cargo check -p omni-wasm` on the host works before the
# wasm32 target is installed. Add wasm-bindgen in Phase 2 when it is needed.
```

- [ ] **Step 3: Write minimal stub sources** so the workspace compiles: `omni-core/src/lib.rs` with `pub mod value;` etc. added in later tasks and a temporary `//! OmniEngine core.` doc header; `omni-ffi/src/lib.rs` with `//! C ABI surface.`; `omni-cli/src/main.rs` printing `omni: not yet implemented` and exiting 2; `omni-wasm/src/lib.rs` with `//! Scaffolded wasm surface. Built only via default-members opt-out.`

- [ ] **Step 4: Verify the workspace resolves and wasm stays out of the default build.**

Run: `cargo metadata --format-version 1 --no-deps | python -c "import json,sys; m=json.load(sys.stdin); print(sorted(m['packages'][0]['name'] for _ in [0]) if False else sorted(p['name'] for p in m['packages']))"`
Expected: `['omni-cli', 'omni-core', 'omni-ffi', 'omni-wasm']`

Run: `cargo build 2>&1 | tail -5`
Expected: builds `omni-core`, `omni-ffi`, `omni-cli` only — `omni-wasm` absent, and no wasm32 target error.

- [ ] **Step 5: Write `.gitattributes` implementing Linguist tier 1 plus EOL policy.** Later rules win, so the path-scoped rules come last.

```gitattributes
# --- Line endings: normalize the repo, protect the Windows build ---
* text=auto eol=lf
*.rs    text eol=lf
*.toml  text eol=lf
*.md    text eol=lf
*.py    text eol=lf
*.js    text eol=lf
*.json  text eol=lf
*.h     text eol=lf
*.omni  text eol=lf
Cargo.lock text eol=lf

# Binary artifacts must never be text-mangled or a cdylib stops loading.
*.dll binary
*.so  binary
*.dylib binary
*.lib binary
*.wasm binary
*.pdb binary

# --- Tier 1: GitHub language identity ---
# `.gitattributes` can only remap a file to a language Linguist already knows.
# Rust is the nearest syntax for `let` bindings and `//` comments. Minting a
# real "OmniEngine" language is the tier 2 github/linguist PR in docs/GUIDE.md.
*.omni linguist-language=Rust linguist-detectable=true

# SDK code must not dilute the engine's own language statistics.
/sdks/** linguist-vendored=true linguist-language=Python
/docs/** linguist-documentation=true
```

- [ ] **Step 6: Verify the attributes actually take effect.**

Run: `git add .gitattributes Cargo.toml rust-toolchain.toml .cargo/config.toml crates/ Cargo.lock 2>&1 | grep -i "warning: in the working copy" || echo "no CRLF warnings"`
Expected: `no CRLF warnings` — this is the proof the EOL policy fixed the warning the spec commit produced.

Run: `git check-attr linguist-language linguist-vendored -- examples/x.omni sdks/python/setup.py docs/y.md`
Expected: `.omni` -> `Rust`, `sdks/python/setup.py` -> `vendored: true`, `docs/y.md` -> `documentation: true`.

- [ ] **Step 7: Commit.**

```bash
git add -A
git commit -m "build: scaffold OmniEngine workspace with storage-aware build policy

default-members excludes omni-wasm so the host build survives without the
wasm32 target. line-tables-only debug is chosen specifically against the
7.5 GB free on C:, where full MSVC PDBs would land."
```

---

### Task 2: NaN-boxed `Value`

**Files:**
- Create: `crates/omni-core/src/value.rs`
- Test: inline `#[cfg(test)] mod tests` in `value.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Value` (`Copy`, 8 bytes), `Value::from_f64(f64) -> Value`, `Value::as_f64(self) -> Option<f64>`, `Value::is_double/is_pointer/is_singleton/is_number/is_nan(self) -> bool`, `Value::bool(bool) -> Value`, `Value::NIL`, `Value::UNDEF`, `Value::TRUE`, `Value::FALSE`, `Value::to_string(self) -> String`, `Value::as_bool(self) -> Option<bool>`, `Value::from_ptr(*const u8) -> Value`, `Value::as_ptr(self) -> Option<*const u8>`.

- [ ] **Step 1: Write the failing tests, including the two that catch real encoding bugs.**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f64_round_trips() {
        for n in [0.0, -0.0, 1.0, -1.0, 42.0, 1e308, -1e308, 3.5, f64::MIN, f64::MAX] {
            let v = Value::from_f64(n);
            assert!(v.is_double(), "{n} must stay a double");
            assert_eq!(v.as_f64(), Some(n));
        }
    }

    /// The bug this test exists to prevent: a pointer tag of
    /// `v >> 48 == 0` is exactly the bit pattern of 0.0 and of every denormal
    /// below ~2^-969. Zero would have been read as a null dereference.
    #[test]
    fn zero_is_never_a_pointer() {
        assert!(!Value::from_f64(0.0).is_pointer());
        assert!(!Value::from_f64(-0.0).is_pointer());
        assert!(!Value::from_f64(f64::from_bits(1)).is_pointer()); // smallest denormal
        assert_eq!(Value::from_f64(0.0).as_f64(), Some(0.0));
    }

    /// -Inf and +Inf are real double values, not tags.
    #[test]
    fn infinities_stay_doubles() {
        assert!(Value::from_f64(f64::INFINITY).is_double());
        assert!(Value::from_f64(f64::NEG_INFINITY).is_double());
    }

    /// IEEE arithmetic never yields a signaling NaN, so the 0x7FF7 row is safe
    /// to spend on pointers. But a NaN arriving from a host must not be.
    #[test]
    fn incoming_nan_is_normalised_and_still_a_number() {
        let v = Value::from_f64(f64::NAN);
        assert!(v.is_number());
        assert!(v.is_nan());
        assert!(!v.is_pointer());
        assert!(!v.is_singleton());
    }

    #[test]
    fn singletons_are_not_numbers() {
        for s in [Value::NIL, Value::UNDEF, Value::TRUE, Value::FALSE] {
            assert!(s.is_singleton());
            assert!(!s.is_double());
        }
        assert_ne!(Value::TRUE, Value::FALSE);
        assert_eq!(Value::NIL, Value::NIL);
    }

    #[test]
    fn pointer_tag_and_untag() {
        let mut x: u64 = 7;
        let p = std::ptr::addr_of_mut!(x).cast::<u8>();
        let v = Value::from_ptr(p);
        assert!(v.is_pointer());
        assert!(!v.is_double());
        assert_eq!(v.as_ptr(), Some(p));
    }

    #[test]
    fn value_is_eight_bytes() {
        assert_eq!(std::mem::size_of::<Value>(), 8);
    }

    #[test]
    fn display_matches_number_formatting() {
        assert_eq!(Value::from_f64(42.0).to_string(), "42");
        assert_eq!(Value::TRUE.to_string(), "true");
        assert_eq!(Value::NIL.to_string(), "null");
    }
}
```

- [ ] **Step 2: Run and confirm failure.** `cargo test -p omni-core` → expected: compile error, `value` module missing.

- [ ] **Step 3: Implement `value.rs`.**

```rust
//! The NaN-boxed OmniEngine value.

use std::fmt;

/// A 16-bit row inside the double-precision NaN space. IEEE arithmetic never
/// produces a *signaling* NaN, so this row is unambiguously a pointer tag.
const PTR_ROW: u64 = 0x7FF7_0000_0000_0000;
/// The canonical quiet NaN. Its row hosts the singleton tags.
const QNAN: u64 = 0x7FF8_0000_0000_0000;
/// 48 usable address bits on x86-64 user space.
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct Value(u64);

impl Value {
    pub const NIL: Value = Value(QNAN + 1);
    pub const TRUE: Value = Value(QNAN + 2);
    pub const FALSE: Value = Value(QNAN + 3);
    pub const UNDEF: Value = Value(QNAN + 4);

    #[inline]
    pub fn from_f64(n: f64) -> Value {
        // Any NaN pattern collapses to the canonical quiet NaN so that an
        // arbitrary payload arriving from a host can never impersonate a tag.
        if n.is_nan() { Value(QNAN) } else { Value(n.to_bits()) }
    }

    /// True when the box is the sNaN pointer row.
    #[inline]
    pub fn is_pointer(self) -> bool { self.0 >> 48 == PTR_ROW >> 48 }

    /// True for the reserved singleton slots 1..=15 in the canonical-NaN row.
    /// Slot 0 is the double NaN itself and must stay a number.
    #[inline]
    pub fn is_singleton(self) -> bool {
        self.0 >> 32 == QNAN >> 32 && {
            let low = self.0 & 0xFFFF_FFFF;
            low != 0 && low < 16
        }
    }

    /// Everything that is neither tag is a double. Order matters: this is the
    /// fallback, not a positive test.
    #[inline]
    pub fn is_double(self) -> bool { !self.is_pointer() && !self.is_singleton() }

    #[inline]
    pub fn is_number(self) -> bool { self.is_double() }

    #[inline]
    pub fn is_nan(self) -> bool { self.as_f64().is_some_and(|n| n.is_nan()) }

    #[inline]
    pub fn as_f64(self) -> Option<f64> {
        self.is_double().then(|| f64::from_bits(self.0))
    }

    #[inline]
    pub fn as_bool(self) -> Option<bool> {
        match self.0 {
            _ if self == Value::TRUE => Some(true),
            _ if self == Value::FALSE => Some(false),
            _ => None,
        }
    }

    /// # Safety
    /// `ptr` must be a x86-64 user address whose top 16 bits are zero. A
    /// kernel or tagged pointer would be silently truncated.
    #[inline]
    pub fn from_ptr(ptr: *const u8) -> Value {
        let raw = ptr as u64;
        debug_assert_eq!(raw >> 48, 0, "pointer does not fit in 48 bits");
        Value(PTR_ROW | (raw & ADDR_MASK))
    }

    #[inline]
    pub fn as_ptr(self) -> Option<*const u8> {
        self.is_pointer().then(|| (self.0 & ADDR_MASK) as *const u8)
    }

    pub fn truthy(self) -> bool {
        match self.as_bool() {
            Some(b) => b,
            None => self.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self.as_f64(), other.as_f64()) {
            (Some(a), Some(b)) => a == b,   // IEEE: NaN != NaN, 0.0 == -0.0
            (None, None) => self.0 == other.0,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_f64() {
            Some(n) if n.is_nan() => f.write_str("NaN"),
            Some(n) if n == n.trunc() && n.abs() < 1e15 => write!(f, "{}", n as i64),
            Some(n) => write!(f, "{n}"),
            None if *self == Value::TRUE => f.write_str("true"),
            None if *self == Value::FALSE => f.write_str("false"),
            None if *self == Value::NIL => f.write_str("null"),
            None if *self == Value::UNDEF => f.write_str("undefined"),
            None => f.write_str("<object>"),
        }
    }
}
```

Note for the implementer: `as_bool` above compares via `PartialEq`, which recurses into `as_f64`. Both arms take the `(None, None)` path, so it terminates, but write it as `self.0 == Value::TRUE.0` instead — cheaper and obviously non-recursive. Do that.

- [ ] **Step 4: `cargo test -p omni-core value::` → all pass. `cargo clippy -p omni-core -D warnings` → clean.**

- [ ] **Step 5: Commit** `git commit -m "feat(core): NaN-boxed 8-byte Value with sNaN pointer row"`

---

### Task 3: Bump `Arena`

**Files:**
- Create: `crates/omni-core/src/arena.rs`
- Modify: `crates/omni-core/src/lib.rs` (add `pub mod arena;`)

**Interfaces:**
- Produces: `Arena::new() -> Arena`, `Arena::with_chunk_size(usize) -> Arena`, `alloc_raw(&mut self, size: usize, align: usize) -> *mut u8`, `alloc<T>(&mut self, T) -> *mut T`, `reset(&mut self)`, `allocations(&self) -> usize`, `bytes_in_use(&self) -> usize`, `chunk_count(&self) -> usize`.

- [ ] **Step 1: Write the failing tests.** The `Layout` test is the one that matters: `Box<[u8]>` reconstructed from a 16-aligned allocation deallocates with align 1, which is UB and only shows up as a heap-corruption crash under a different allocator.

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
        // Same address after reset: the arena is a bump pointer, not a pool.
        assert_eq!(a.alloc::<u64>(3), first);
    }

    #[test]
    fn crosses_into_a_new_chunk_without_invalidating_earlier_pointers() {
        let mut a = Arena::with_chunk_size(4096);
        let p = a.alloc::<u64>(0xDEAD_BEEF);
        for _ in 0..1000 {
            let _ = a.alloc::<u64>(0);
        }
        assert!(a.chunk_count() > 1);
        assert_eq!(unsafe { *p }, 0xDEAD_BEEF, "chunk growth moved data");
    }

    #[test]
    fn typed_alloc_writes_the_value() {
        let mut a = Arena::new();
        let p = a.alloc(Value::from_f64(42.0));
        assert_eq!(unsafe { *p }, Value::from_f64(42.0));
    }

    #[test]
    #[should_panic(expected = "larger than the chunk")]
    fn oversized_request_is_rejected_not_silently_grown() {
        let mut a = Arena::with_chunk_size(4096);
        a.alloc_raw(8192, 8);
    }
}
```

- [ ] **Step 2: `cargo test -p omni-core arena::` → confirm compile failure.**

- [ ] **Step 3: Implement.** Chunks are raw `Layout` allocations with a matching dealloc; `Box<[u8]>` is deliberately not used because its `Drop` would dealloc with align 1.

```rust
//! Deterministic chunked bump allocator. No GC, no refcounting: memory is
//! reclaimed in bulk when the owning engine resets or drops the arena.

use std::alloc::{alloc as sys_alloc, dealloc as sys_dealloc, Layout};
use std::ptr;

use crate::Value;

/// Every chunk is at least this aligned, which is the largest alignment any
/// primitive we box requires.
const CHUNK_ALIGN: usize = 16;
const DEFAULT_CHUNK: usize = 64 * 1024;
const MIN_CHUNK: usize = 4096;

struct Chunk {
    ptr: *mut u8,
    layout: Layout,
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // Safety: layout is exactly the one used to allocate, so the size and
        // align match. This is the invariant that makes dealloc sound.
        unsafe { sys_dealloc(self.ptr, self.layout) };
    }
}

pub struct Arena {
    chunks: Vec<Chunk>,
    chunk_size: usize,
    used: usize,
    allocations: usize,
}

impl Arena {
    pub fn new() -> Self { Self::with_chunk_size(DEFAULT_CHUNK) }

    pub fn with_chunk_size(chunk_size: usize) -> Self {
        assert!(chunk_size >= MIN_CHUNK, "chunk_size must be at least one page");
        assert!(chunk_size % CHUNK_ALIGN == 0, "chunk_size must be 16-aligned");
        Arena { chunks: Vec::new(), chunk_size, used: usize::MAX, allocations: 0 }
    }

    fn push_chunk(&mut self) {
        let layout = Layout::from_size_align(self.chunk_size, CHUNK_ALIGN)
            .expect("valid chunk layout");
        let ptr = unsafe { sys_alloc(layout) };
        assert!(!ptr.is_null(), "arena chunk allocation failed");
        self.chunks.push(Chunk { ptr, layout });
        self.used = 0;
    }

    pub fn alloc_raw(&mut self, size: usize, align: usize) -> *mut u8 {
        assert!(size > 0, "zero-sized allocation");
        assert!(align.is_power_of_two(), "align must be a power of two");
        assert!(align <= CHUNK_ALIGN, "align {align} exceeds chunk alignment");
        assert!(size + align <= self.chunk_size, "request larger than the chunk");

        // Worst-case padding is align-1, so size+align fits iff the request is legal.
        let fits = self.used != usize::MAX && self.used + size + align <= self.chunk_size;
        if !fits { self.push_chunk(); }

        let base = self.chunks.last().expect("chunk present").ptr as usize;
        let raw = base + self.used;
        let aligned = (raw + align - 1) & !(align - 1);
        self.used = aligned - base + size;
        self.allocations += 1;
        aligned as *mut u8
    }

    pub fn alloc<T>(&mut self, value: T) -> *mut T {
        let p = self.alloc_raw(
            std::mem::size_of::<T>().max(1),
            std::mem::align_of::<T>().max(1),
        ) as *mut T;
        unsafe { ptr::write(p, value) };
        p
    }

    /// Frees every chunk but the first, keeping one chunk of capacity warm.
    pub fn reset(&mut self) {
        self.chunks.truncate(1.min(self.chunks.len()));
        if !self.chunks.is_empty() { self.used = 0; } else { self.used = usize::MAX; }
        self.allocations = 0;
    }

    pub fn allocations(&self) -> usize { self.allocations }
    pub fn chunk_count(&self) -> usize { self.chunks.len() }
    pub fn bytes_in_use(&self) -> usize {
        if self.chunks.is_empty() { 0 } else { (self.chunks.len() - 1) * self.chunk_size + self.used.min(self.chunk_size) }
    }
}

impl Default for Arena { fn default() -> Self { Self::new() } }
```

- [ ] **Step 4: Fix the two latent bugs in the code above before running tests.** (a) `bytes_in_use` reports 0 after `reset()` only if `used` is `usize::MAX`-guarded — return `0` when `used == usize::MAX`. (b) `reset()` truncates to one chunk but then sets `used = 0` while that surviving chunk is still allocated; a following `alloc_raw` sees `fits == true` and reuses it, which is the intent — confirm the `crosses_into_a_new_chunk` and `reset_is_deterministic` tests both pass together, because a wrong `used` reset makes them contradict.
- [ ] **Step 5: `cargo test -p omni-core` → pass. `cargo clippy -p omni-core -- -D warnings` → clean.**
- [ ] **Step 6: Commit** `git commit -m "feat(core): chunked bump arena with layout-matched dealloc"`

---

### Task 4: Lexer

**Files:**
- Create: `crates/omni-core/src/lexer.rs`
- Modify: `crates/omni-core/src/lib.rs`

**Interfaces:**
- Produces: `enum TokenKind { Let, Ident, Number, BinOp(BinOp), Eq, LParen, RParen, Semi, Newline, Eof }`, `struct Token<'a> { kind: TokenKind, text: &'a str, line: u32 }`, `lex(&str) -> Result<Vec<Token<'_>>, LexError>`, `enum BinOp { Add, Sub, Mul, Div, Mod }`, `struct LexError { line: u32, column: u32, message: String }`.

- [ ] **Step 1: Write the failing tests.** `tokens_borrow_the_source` is the zero-allocation claim, so it must be a compile-level test, not a comment.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> { lex(src).unwrap().iter().map(|t| t.kind).collect() }

    #[test]
    fn lexes_the_vertical_slice() {
        assert_eq!(kinds("let answer = 40 + 2"),
            vec![TokenKind::Let, TokenKind::Ident, TokenKind::Eq,
                 TokenKind::Number, TokenKind::BinOp(BinOp::Add),
                 TokenKind::Number, TokenKind::Eof]);
    }

    #[test]
    fn skips_line_comments_and_whitespace() {
        assert_eq!(kinds("// trailing\n 1 + 2 // tail"),
            vec![TokenKind::Number, TokenKind::BinOp(BinOp::Add), TokenKind::Number, TokenKind::Eof]);
    }

    #[test]
    fn floats_and_exponents_are_one_number() {
        assert_eq!(kinds("1.5e3"), vec![TokenKind::Number, TokenKind::Eof]);
    }

    #[test]
    fn tracks_line_numbers_for_errors() {
        let toks = lex("1\n\n@\n").unwrap_err();
        assert_eq!(toks.line, 3);
    }

    #[test]
    fn rejects_unknown_bytes_with_a_column() {
        let e = lex("1 + ?").unwrap_err();
        assert_eq!((e.line, e.column), (1, 5));
        assert!(e.message.contains('?'));
    }

    /// The text slice of a token points into the caller's source buffer; no
    /// per-token String is allocated.
    #[test]
    fn token_text_is_a_borrow_not_a_copy() {
        let src = String::from("let x = 1");
        let toks = lex(&src).unwrap();
        let ident = toks.iter().find(|t| t.kind == TokenKind::Ident).unwrap();
        assert!(std::ptr::eq(ident.text.as_ptr(), src.as_ptr().add(4)));
    }
}
```

- [ ] **Step 2: Confirm failure, then implement** a byte-walking lexer over `&[u8]` that slices `&str` by byte offset using `src.get(start..end).expect("valid UTF-8 boundary")`. Track `line` by incrementing on `b'\n'`, and compute `column` as `pos - line_start + 1`. Keywords match the exact bytes `let`. Identifiers are `[A-Za-z_][A-Za-z0-9_]*`. Numbers are `[0-9]` then optional `.` then digits, then optional `e|E` with optional sign and digits — reject a trailing `.` with a clear message rather than silently yielding `1`.

- [ ] **Step 3: `cargo test -p omni-core lexer::` → pass. Commit** `git commit -m "feat(core): zero-allocation lexer borrowing source slices"`

---

### Task 5: `Engine` — fused parse-and-evaluate slice

**Files:**
- Create: `crates/omni-core/src/engine.rs`
- Modify: `crates/omni-core/src/lib.rs` (exports: `pub use engine::{Engine, ErrorKind, OmniError}; pub use value::Value; pub use arena::Arena;`)

**Interfaces:**
- Consumes: `lex`, `Token`, `TokenKind`, `BinOp`, `Value`, `Arena`.
- Produces: `Engine::new() -> Engine`, `Engine::exec(&mut self, source: &str) -> Result<Value, OmniError>`, `Engine::last_error(&self) -> Option<&OmniError>`, `Engine::last_value(&self) -> Value`, `Engine::arena(&self) -> &Arena`, `Engine::define(&mut self, name: &str, value: Value)`, `struct OmniError { kind: ErrorKind, line: u32, message: String }`, `enum ErrorKind { Lex, Parse, Runtime, Panic }`.

- [ ] **Step 1: Write the failing tests.**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ev(src: &str) -> Value { Engine::new().exec(src).expect("should evaluate") }
    fn num(n: f64) -> Value { Value::from_f64(n) }

    #[test]
    fn the_vertical_slice_returns_forty_two() { assert_eq!(ev("let answer = 40 + 2"), num(42.0)); }

    #[test]
    fn precedence_and_grouping() {
        assert_eq!(ev("2 + 3 * 4"), num(14.0));
        assert_eq!(ev("(2 + 3) * 4"), num(20.0));
        assert_eq!(ev("20 / 3"), num(20.0 / 3.0));
        assert_eq!(ev("20 % 3"), num(2.0));
    }

    #[test]
    fn unary_minus_and_binds_tighter_than_multiply() {
        assert_eq!(ev("-3 + 10"), num(7.0));
        assert_eq!(ev("- 2 * 3"), num(-6.0));
    }

    #[test]
    fn let_bindings_are_visible_to_later_statements() {
        assert_eq!(ev("let a = 10\nlet b = a * a\nb - a"), num(90.0));
    }

    #[test]
    fn last_statement_value_is_returned() {
        assert_eq!(ev("1\n2\n3"), num(3.0));
        assert_eq!(ev("let x = 5"), num(5.0));
    }

    #[test]
    fn division_by_zero_is_a_runtime_error_not_a_panic() {
        let e = Engine::new().exec("1 / 0").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert!(e.message.contains("division by zero"));
    }

    #[test]
    fn undefined_variable_reports_its_line() {
        let e = Engine::new().exec("1 +\nnope").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!(e.line, 2);
    }

    #[test]
    fn syntax_error_is_a_parse_error() {
        assert_eq!(Engine::new().exec("1 +").unwrap_err().kind, ErrorKind::Parse);
        assert_eq!(Engine::new().exec("let = 3").unwrap_err().kind, ErrorKind::Parse);
    }

    #[test]
    fn lex_error_propagates_kind() {
        assert_eq!(Engine::new().exec("1 @ 2").unwrap_err().kind, ErrorKind::Lex);
    }

    #[test]
    fn define_injects_a_host_value() {
        let mut e = Engine::new();
        e.define("PI", num(3.0));
        assert_eq!(e.exec("PI * 2").unwrap(), num(6.0));
    }

    #[test]
    fn arena_resets_between_executions() {
        let mut e = Engine::new();
        let _ = e.exec("let a = 1\nlet b = 2\na + b");
        assert!(e.arena().allocations() > 0);
        let _ = e.exec("1");
        assert_eq!(e.arena().allocations(), 0, "arena must be reclaimed per exec");
    }

    #[test]
    fn failed_exec_leaves_last_value_untouched() {
        let mut e = Engine::new();
        assert_eq!(e.exec("7").unwrap(), num(7.0));
        assert!(e.exec("1 +").is_err());
        assert_eq!(e.last_value(), num(7.0));
    }
}
```

- [ ] **Step 2: Confirm failure, then implement** a recursive-descent parser fused with evaluation. No AST type is introduced — that is Phase 2 and inventing one now would be throwaway. Grammar:

```
program := ( stmt )*           stmt := 'let' IDENT '=' expr | expr
expr    := term  ( ( '+' | '-' ) term  )*
term    := factor( ( '*' | '/' | '%' ) factor )*
factor  := ( '-' | '+' )? primary
primary := NUMBER | IDENT | '(' expr ')'
```

`Engine` fields: `arena: Arena`, `globals: Vec<(String, Value)>`, `last_value: Value`, `last_error: Option<OmniError>`. `exec` calls `self.arena.reset()` first, then `lex`, then parses against a `Scope` view of `globals`; error text for `Lex`/`Parse`/`Runtime` is cached in `last_error` as an owned `String` allocated **in the arena** — this is the arena's first real consumer and makes the bulk-free claim concrete rather than aspirational.

Divide-by-zero returns `Err(Runtime)` and never reaches a float infinity, because an `inf` leaking back through the FFI would look like a successful computation.

- [ ] **Step 3: `cargo test -p omni-core` → all pass. `cargo clippy --workspace --all-targets -- -D warnings` → clean.**
- [ ] **Step 4: Commit** `git commit -m "feat(core): fused parser and evaluator for the expression slice"`

---

### Task 6: C ABI and `omni.h`

**Files:**
- Create: `crates/omni-ffi/src/lib.rs`, `crates/omni-ffi/include/omni.h`, `crates/omni-ffi/cbindgen.toml`
- Create: `crates/omni-ffi/tests/ffi_contract.rs`
- Modify: `docs/superpowers/specs/2026-11-02-omniengine-phase1-design.md` (replace the `union` line with the recorded deviation)

**Interfaces:**
- Consumes: `omni_core::{Engine, Value, OmniError, ErrorKind}`.
- Produces (exact C names, all `extern "C", no_mangle`): `omni_version`, `omni_engine_new`, `omni_engine_free`, `omni_exec`, `omni_exec_number`, `omni_last_error`, `omni_debug_panic`. Status constants `OMNI_OK=0`, `OMNI_ERR_COMPILE=1`, `OMNI_ERR_RUNTIME=2`, `OMNI_ERR_PANIC=3`, `OMNI_ERR_ARG=4`. Value tags `OMNI_NUMBER=0`, `OMNI_NIL=1`, `OMNI_BOOL=2`, `OMNI_UNDEF=3`. `OmniValue { tag: u32, number: f64 }`.

- [ ] **Step 1: Write the failing integration tests.** These are the real proof of the panic boundary — an unwinding Rust panic landing in a C stack frame is UB, and it is the single most common defect in hand-rolled FFI.

```rust
use omni_ffi::*;
use std::ffi::{CStr, c_char};

#[test]
fn evaluates_42_through_the_c_surface() {
    unsafe {
        let eng = omni_engine_new();
        assert!(!eng.is_null());
        let mut out = OmniValue::default();
        let src = b"let answer = 40 + 2";
        assert_eq!(omni_exec(eng, src.as_ptr().cast::<c_char>(), src.len(), &mut out), OMNI_OK);
        assert_eq!(out.tag, OMNI_NUMBER);
        assert_eq!(out.number, 42.0);
        omni_engine_free(eng);
    }
}

#[test]
fn a_panicking_engine_reports_instead_of_unwinding_into_the_host() {
    unsafe {
        let eng = omni_engine_new();
        let r = omni_debug_panic(eng);
        assert_eq!(r, OMNI_ERR_PANIC, "panic must be caught at the boundary");
        // The engine is still usable: the catch left it in a defined state.
        let mut out = OmniValue::default();
        let src = b"1 + 1";
        assert_eq!(omni_exec(eng, src.as_ptr().cast(), src.len(), &mut out), OMNI_OK);
        assert_eq!(out.number, 2.0);
        omni_engine_free(eng);
    }
}

#[test]
fn null_and_bad_arguments_are_rejected_not_dereferenced() {
    unsafe {
        let src = b"1";
        let mut out = OmniValue::default();
        assert_eq!(omni_exec(std::ptr::null_mut(), src.as_ptr().cast(), 1, &mut out), OMNI_ERR_ARG);
        let eng = omni_engine_new();
        assert_eq!(omni_exec(eng, std::ptr::null(), 1, &mut out), OMNI_ERR_ARG);
        assert_eq!(omni_exec(eng, src.as_ptr().cast(), 0, std::ptr::null_mut()), OMNI_ERR_ARG);
        // Non-UTF-8 bytes are an argument error, not a panic.
        assert_eq!(omni_exec(eng, src.as_ptr().cast(), usize::MAX / 2, &mut out), OMNI_ERR_ARG);
        omni_engine_free(eng);
        omni_engine_free(std::ptr::null_mut());   // must be a no-op
    }
}

#[test]
fn error_text_is_copied_into_caller_storage() {
    unsafe {
        let eng = omni_engine_new();
        let src = b"1 / 0";
        let mut out = OmniValue::default();
        assert_eq!(omni_exec(eng, src.as_ptr().cast(), src.len(), &mut out), OMNI_ERR_RUNTIME);
        let mut buf = [0i8 as c_char; 64];
        let n = omni_last_error(eng, buf.as_mut_ptr(), buf.len());
        assert!(n > 0 && n < buf.len());
        assert!(String::from_utf8_lossy(std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n)).contains("division by zero"));
        // A too-small buffer truncates and NUL-terminates rather than overflowing.
        let mut tiny = [0i8 as c_char; 4];
        assert_eq!(omni_last_error(eng, tiny.as_mut_ptr(), tiny.len()), 3);
        assert_eq!(tiny[3], 0);
        omni_engine_free(eng);
    }
}

#[test]
fn version_is_stable_and_nul_terminated() {
    unsafe {
        let p = omni_version();
        assert!(!p.is_null());
        // Read the NUL-terminated static string without assuming libc is linked.
        let s = CStr::from_ptr(p).to_str().expect("utf-8 version");
        assert_eq!(s, env!("CARGO_PKG_VERSION"));
    }
}
```

- [ ] **Step 2: Implement `omni-ffi/src/lib.rs`.** Every body inside `catch_unwind(AssertUnwindSafe(..))`, null-checked args, by-value results, and no Rust type at the boundary.

```rust
//! The only crate allowed to name `#[repr(C)]` or `extern "C"`.

use std::ffi::{c_char, c_double, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;

use omni_core::{Engine, ErrorKind};

pub const OMNI_OK: u32 = 0;
pub const OMNI_ERR_COMPILE: u32 = 1;
pub const OMNI_ERR_RUNTIME: u32 = 2;
pub const OMNI_ERR_PANIC: u32 = 3;
pub const OMNI_ERR_ARG: u32 = 4;

pub const OMNI_NUMBER: u32 = 0;
pub const OMNI_NIL: u32 = 1;
pub const OMNI_BOOL: u32 = 2;
pub const OMNI_UNDEF: u32 = 3;

/// Tagged, by-value result. Deliberately NOT a union: no string values exist
/// yet, so a pointer arm would be an unpopulated variant that every host has
/// to read unsafely. Deferred to Phase 2 with strings.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct OmniValue { pub tag: u32, pub number: c_double }

/// Opaque engine handle. The inner Engine owns its identifier strings, so the
/// handle carries no lifetime and stays valid across calls with new source.
#[repr(C)]
pub struct OmniEngine(Engine);

fn status_for(kind: ErrorKind) -> u32 {
    match kind {
        ErrorKind::Lex | ErrorKind::Parse => OMNI_ERR_COMPILE,
        ErrorKind::Runtime => OMNI_ERR_RUNTIME,
        ErrorKind::Panic => OMNI_ERR_PANIC,
    }
}

/// Runs `f`, converting any panic into OMNI_ERR_PANIC. A panic that unwound
/// into Python or Node would be undefined behaviour.
#[inline]
fn guarded<F: FnOnce() -> u32>(f: F) -> u32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => OMNI_ERR_PANIC,
    }
}
```

`guarded` takes a closure returning `u32`, so the closure must be the whole body of each entry point. Note the `use` list needs `CStr` for `omni_last_error`'s sibling helpers and can drop `c_void` if nothing uses it — clippy's unused-imports lint will say.

```rust
#[no_mangle]
pub extern "C" fn omni_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast::<c_char>()
}

#[no_mangle]
pub extern "C" fn omni_engine_new() -> *mut OmniEngine {
    match catch_unwind(AssertUnwindSafe(|| Box::into_raw(Box::new(OmniEngine(Engine::new()))))) {
        Ok(p) => p,
        Err(_) => ptr::null_mut(),
    }
}

/// # Safety
/// `engine` must be null or a pointer from `omni_engine_new`, and must not be
/// used after this call.
#[no_mangle]
pub unsafe extern "C" fn omni_engine_free(engine: *mut OmniEngine) {
    if !engine.is_null() { drop(unsafe { Box::from_raw(engine) }); }
}

/// # Safety
/// `engine` from `omni_engine_new`; `src` readable for `len` bytes; `out` writable.
#[no_mangle]
pub unsafe extern "C" fn omni_exec(
    engine: *mut OmniEngine, src: *const c_char, len: usize, out: *mut OmniValue,
) -> u32 {
    if engine.is_null() || src.is_null() || out.is_null() || len > 1 << 28 {
        return OMNI_ERR_ARG;
    }
    let bytes = unsafe { slice::from_raw_parts(src.cast::<u8>(), len) };
    let text = match std::str::from_utf8(bytes) { Ok(t) => t, Err(_) => return OMNI_ERR_ARG };
    let eng = unsafe { &mut (*engine).0 };
    guarded(|| match eng.exec(text) {
        Ok(v) => { unsafe { *out = encode(v) }; OMNI_OK }
        Err(e) => status_for(e.kind),
    })
}

/// # Safety  as `omni_exec`, but fails unless the result is a number.
#[no_mangle]
pub unsafe extern "C" fn omni_exec_number(
    engine: *mut OmniEngine, src: *const c_char, len: usize, out: *mut c_double,
) -> u32 {
    if engine.is_null() || src.is_null() || out.is_null() || len > 1 << 28 { return OMNI_ERR_ARG; }
    let mut v = OmniValue::default();
    let code = unsafe { omni_exec(engine, src, len, &mut v) };
    if code == OMNI_OK && v.tag == OMNI_NUMBER { unsafe { *out = v.number }; OMNI_OK } else { code }
}

/// # Safety
/// `buf` writable for `cap` bytes. Writes at most `cap - 1` bytes plus a NUL.
#[no_mangle]
pub unsafe extern "C" fn omni_last_error(
    engine: *mut OmniEngine, buf: *mut c_char, cap: usize,
) -> usize {
    if engine.is_null() || buf.is_null() || cap == 0 { return 0; }
    let msg = match unsafe { (*engine).0.last_error() } { Some(e) => e.message.as_bytes(), None => b"" };
    let n = msg.len().min(cap - 1);
    unsafe { ptr::copy_nonoverlapping(msg.as_ptr(), buf.cast::<u8>(), n); *buf.add(n) = 0 };
    n
}

/// Test hook that wires the panic boundary. Named to be unmistakable; it is
/// the only way the acceptance criterion "a panic returns OMNI_ERR_PANIC" can
/// be exercised from a host.
#[doc(hidden)]
#[no_mangle]
pub extern "C" fn omni_debug_panic(engine: *mut OmniEngine) -> u32 {
    if engine.is_null() { return OMNI_ERR_ARG; }
    guarded(|| { panic!("omni_debug_panic: deliberate panic for FFI boundary test"); })
}

fn encode(v: omni_core::Value) -> OmniValue {
    if let Some(n) = v.as_f64() { OmniValue { tag: OMNI_NUMBER, number: n } }
    else if let Some(b) = v.as_bool() { OmniValue { tag: OMNI_BOOL, number: b as u32 as c_double } }
    else if v == omni_core::Value::UNDEF { OmniValue { tag: OMNI_UNDEF, number: 0.0 } }
    else { OmniValue { tag: OMNI_NIL, number: 0.0 } }
}
```

`len > 1 << 28` bounds a hostile length before `from_raw_parts`, so a bad `len` from a host cannot fabricate an out-of-bounds slice. Add `pub use` re-exports of the constants so `tests/ffi_contract.rs` compiles against `omni_ffi::*`, and drop `c_void` if unused.

- [ ] **Step 3: Write `include/omni.h`** mirroring the ABI by hand — `#ifdef _WIN32` `__declspec(dllexport)` vs `__attribute__((visibility("default")))`, `typedef struct OmniEngine OmniEngine;` opaque, the six prototypes with matching parameter types, `#define OMNI_OK 0` etc., and `#ifdef __cplusplus extern "C" {` guards.
- [ ] **Step 4: Write `cbindgen.toml`** with `language = "C"`, `include_guard = "OMNI_H"`, `autogen_warning`, `[parse] parse_deps = false`, `header = comment pointing at the hand-written file` so `cbindgen --config crates/omni-ffi/cbindgen.toml crates/omni-ffi -o crates/omni-ffi/include/omni.h` is a documented one-command upgrade rather than a guess.
- [ ] **Step 5: Amend the spec** FFI section to the `{ tag, number }` struct with a one-line reason, so the docs do not describe a union that does not exist.
- [ ] **Step 6: Run `cargo test -p omni-ffi`.** Then run the link check that the spec promised was unverified:

Run: `cargo build -p omni-ffi 2>&1 | tail -20 && ls -la target/debug/omni*`
Expected: `omni.dll` + `omni.dll.lib` present. If the MSVC link fails with `link.exe not found` or `LNK1181`, re-run with `cargo build -p omni-ffi --target x86_64-pc-windows-gnu` and record which toolchain won in `docs/GUIDE.md`.
- [ ] **Step 7: Commit** `git commit -m "feat(ffi): C ABI surface with panic boundary and caller-owned error copy"`

---

### Task 7: `omni` CLI and the example program

**Files:**
- Create: `crates/omni-cli/src/main.rs` (replace stub), `examples/hello.omni`, `examples/arithmetic.omni`

**Interfaces:** Consumes `omni_core::Engine`. Produces `omni run <file>`, `omni eval <expr>`, `omni --version`; exit 0 on success, 1 on engine error, 2 on usage/IO error.

- [ ] **Step 1: `examples/hello.omni`** — the file that must produce `42`:

```omni
// OmniEngine vertical slice: bindings, arithmetic, precedence.
let answer = 40 + 2
answer
```

- [ ] **Step 2: Implement `main.rs`** with hand-rolled arg parsing (no `clap` — `omni-core`'s zero-dependency rule and the disk budget both argue against it), `read_to_string`, `Engine::new()`, print `result` to stdout and `omni: line N: message` to stderr, plus `--quiet` for the tests.
- [ ] **Step 3: Verify.**

Run: `cargo run -q -p omni-cli -- run examples/hello.omni` → expected stdout `42`.
Run: `cargo run -q -p omni-cli -- eval "2 + 3 * 4"` → `14`.
Run: `cargo run -q -p omni-cli -- eval "1 / 0"`; then `echo $?` → expected `1` and a message naming division by zero.
- [ ] **Step 4: Commit** `git commit -m "feat(cli): omni run/eval with engine exit codes"`

---

### Task 8: `omni-wasm` scaffold

**Files:** `crates/omni-wasm/src/lib.rs`, `crates/omni-wasm/Cargo.toml`

- [ ] **Step 1: Write the scaffold** mirroring the C ABI shape with plain `f64` in and `String` errors out, so the eventual `wasm-bindgen` addition is a decorator rather than a rewrite. No `wasm-bindgen` dependency yet — the crate is not in `default-members`, so adding it now would put an uncompilable dependency in `Cargo.lock` for no benefit.
- [ ] **Step 2: Verify it compiles on the host without the wasm target:** `cargo check -p omni-wasm 2>&1 | tail -5` → expected `Finished`.
- [ ] **Step 3: Record the deferred setup** in `docs/GUIDE.md`: `rustup target add wasm32-unknown-unknown`, then `cargo install wasm-pack` or the prebuilt-binary route, with the note that the CLI build costs ~300 crate compilations against the 7.5 GB on C:.
- [ ] **Step 4: Commit** `git commit -m "feat(wasm): scaffolded engine surface, deferred behind default-members"`

---

### Task 9: Python SDK

**Files:** `sdks/python/pyproject.toml`, `sdks/python/omni/__init__.py`, `sdks/python/omni/_ffi.py`, `tests/test_ffi.py`

**Interfaces:** Consumes the `omni_*` symbols. Produces `omni.Engine()` with `.eval_number(src) -> float`, `.eval(src) -> OmniValue`, `.close()`, context-manager support, and `omni.OmniError(code, message)`.

- [ ] **Step 1: Write `tests/test_ffi.py` first.**

```python
import pathlib, sys
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "sdks" / "python"))
import omni

def test_vertical_slice():
    with omni.Engine() as e:
        assert e.eval_number("let answer = 40 + 2") == 42.0
        print(42)

def test_error_carries_the_engine_message():
    with omni.Engine() as e:
        try:
            e.eval_number("1 / 0")
            raise AssertionError("expected OmniError")
        except omni.OmniError as err:
            assert "division by zero" in str(err)

def test_panic_does_not_kill_the_interpreter():
    with omni.Engine() as e:
        assert e.eval_number("1 + 1") == 2.0   # engine still usable afterwards

if __name__ == "__main__":
    test_vertical_slice(); test_error_carries_the_engine_message()
    test_panic_does_not_kill_the_interpreter()
    print("python FFI: all pass")
```

- [ ] **Step 2: Implement `_ffi.py`** with stdlib `ctypes` only. An `OmniValue` `ctypes.Structure` with fields in declared order `("tag", c_uint32), ("number", c_double)`, matching `#[repr(C)]`: `tag` at offset 0, four bytes of tail padding, `number` at offset 8, `sizeof == 16`. Assert `ctypes.sizeof(OmniValue) == 16` at import time and cross-check it against a Rust `const _: () = assert!(size_of::<OmniValue>() == 16)` so the two sides cannot drift silently. Set `argtypes`/`restype` on every symbol; without `argtypes` ctypes passes pointers as `int` and a 64-bit handle is truncated, which is the classic silent-corruption bug on Windows x64.
- [ ] **Step 3: Library discovery** in this order: `$OMNI_ENGINE_HOME`, `OMNI_ENGINE_LIBRARY`, then the workspace-relative `target/{debug,release}/omni.{dll,so,dylib}` resolved from `__file__`, then the bare platform name on `PATH`. Raise `OmniLibraryError` naming `cargo build -p omni-ffi` when nothing is found.
- [ ] **Step 4: `pyproject.toml`** with `name = "omni-engine"`, `requires-python = ">=3.10"`, and **no `dependencies`** — that zero-dependency property is the reason `ctypes` was chosen.
- [ ] **Step 5: Verify:** `python tests/test_ffi.py` → expected `42` then `python FFI: all pass`.
- [ ] **Step 6: Commit** `git commit -m "feat(sdk-python): ctypes binding with explicit argtypes and library discovery"`

---

### Task 10: Node SDK

**Files:** `sdks/nodejs/package.json`, `sdks/nodejs/index.js`, `sdks/nodejs/lib/bindings.js`, `sdks/nodejs/lib/discover.js`, `tests/test_ffi.js`

**Interfaces:** Produces `const { Engine, OmniError } = require('omni-engine')`; `engine.evalNumber(src): number`, `engine.exec(src): {tag, number}`, `engine.close()`.

- [ ] **Step 1: Write `tests/test_ffi.js` first.**

```js
const path = require("node:path");
const { Engine } = require(path.join(__dirname, "..", "sdks", "nodejs"));

const e = new Engine();
try {
  const v = e.exec("let answer = 40 + 2");
  if (v.number !== 42) throw new Error(`expected 42, got ${v.number}`);
  console.log(42);
  try { e.exec("1 / 0"); throw new Error("expected throw"); }
  catch (err) { if (!/division by zero/.test(err.message)) throw err; }
  if (e.evalNumber("1 + 1") !== 2) throw new Error("engine unusable after error");
  console.log("node FFI: all pass");
} finally { e.close(); }
```

- [ ] **Step 2: `koffi`, not `ffi-napi`.** `ffi-napi` is unmaintained and fails to compile against Node 26's V8/ABI, so installing it needs node-gyp and a compiler chain. `koffi` ships prebuilt binaries. Declare it as `optionalDependencies` so `npm install` never hard-fails, and raise a named `Error` with the exact remedy when `require("koffi")` throws.
- [ ] **Step 3: Implement `bindings.js`** — `koffi.load(libPath)`, `koffi.struct("OmniValue", { tag: "uint32_t", number: "double" })`, prototypes declared with explicit types, and source passed as a `Buffer.from(src, "utf8")` with its byte length rather than a JS string, so `len` matches Rust's byte count for non-ASCII input.
- [ ] **Step 4: Verify.** `npm --prefix sdks/nodejs install 2>&1 | tail -3` then `node tests/test_ffi.js` → expected `42` then `node FFI: all pass`. If the registry is unreachable, do not fake it: report the failure verbatim and record `koffi` install as the unmet prerequisite for acceptance criterion 4.
- [ ] **Step 5: Commit** `git commit -m "feat(sdk-node): koffi binding sized for Node 26"`

---

### Task 11: Execution guide and end-to-end verification

**Files:** Create `docs/GUIDE.md`, `README.md`

- [ ] **Step 1: Write `docs/GUIDE.md`** with the five phases as the user framed them, corrected where the premise was wrong: environment commands; workspace layout and why `default-members` excludes wasm; the storage section stating C: at 93% and the `line-tables-only` choice, with the `CARGO_HOME` relocation given as explicit opt-in commands and the note that it was declined for now; the FFI contract including the no-arena-pointer rule and `catch_unwind`; the Linguist section stating plainly that `.gitattributes` cannot mint a language and the tier 2 PR checklist; and the exact verification command list with expected output.
- [ ] **Step 2: Add the `.ox` alias** to `.gitattributes` so the alternative extension the user mentioned works without a rebuild.
- [ ] **Step 3: Full verification sweep**, capturing real output for each:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -q -p omni-cli -- run examples/hello.omni
python tests/test_ffi.py
node tests/test_ffi.js
git check-attr linguist-language -- examples/hello.omni
git status --porcelain
```

- [ ] **Step 4: `du -sh target` and `df -h /c /d`** — record the actual footprint so the storage claim is measured, not asserted.
- [ ] **Step 5: Commit** `git commit -m "docs: Phase 1 initialization and execution guide"`

## Out of Scope

Full grammar, AST, resolver, bytecode VM, stdlib, string values, Smi tagging, the actual wasm build, the Linguist PR, and PyPI/npm publication.

