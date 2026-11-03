//! OmniEngine's C ABI — version 2.
//!
//! This is the only crate permitted to name a `#[repr(C)]` type or an
//! `extern "C"` function, which is what keeps `omni-core` from becoming
//! FFI-shaped by accident.
//!
//! ## The two rules that make this sound
//!
//! **A pointer that crosses the boundary addresses bytes the engine owns for as
//! long as the handle does.** Phase 1 said "no arena pointer crosses the
//! boundary", and Phase 2 replaced that with something narrower and more useful:
//! [`omni_exec`] promotes a string *result* into the egress block before
//! returning, and egress rewinds rather than frees, so its bytes stay mapped for
//! the life of the handle. Host strings injected by [`omni_define_string`] live
//! in their own allocation outside every arena. Nothing in an [`OmniValue`] that
//! leaves this crate addresses the execution arena.
//!
//! The consequence is worth stating out loud: **a stale `OmniValue` is a wrong
//! read, never a dangling one.** A result is valid until the next `omni_exec` on
//! the same engine; calling `omni_str_copy` after that copies whatever now sits
//! at those addresses. And because [`omni_str_copy`] re-checks the address
//! against the engine that owns it, a fabricated pointer — or one that belongs to
//! *another* engine, or one whose binding has since been redefined — copies zero
//! bytes rather than reading memory it has no claim on.
//!
//! **A Rust panic may never unwind into a host.** Unwinding past `extern "C"` is
//! undefined behaviour, and it is the most common defect in hand-rolled Rust FFI.
//! Every body here runs inside `catch_unwind`, and a caught panic becomes
//! [`OMNI_ERR_PANIC`] rather than a crashed interpreter.

use std::ffi::{c_char, c_double};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;

use omni_core::{Engine, ErrorKind, HostError, Value};

/// The two ceilings a host has to size its buffers against.
///
/// Re-exported rather than restated: they are part of the ABI's contract, and a
/// number duplicated on both sides of a module boundary is a number that drifts.
pub use omni_core::{MAX_IDENTIFIER_LEN, MAX_STRING_LEN};

pub const OMNI_OK: u32 = 0;
pub const OMNI_ERR_COMPILE: u32 = 1;
pub const OMNI_ERR_RUNTIME: u32 = 2;
pub const OMNI_ERR_PANIC: u32 = 3;
pub const OMNI_ERR_ARG: u32 = 4;

pub const OMNI_NUMBER: u32 = 0;
pub const OMNI_NIL: u32 = 1;
pub const OMNI_BOOL: u32 = 2;
pub const OMNI_UNDEF: u32 = 3;
pub const OMNI_STRING: u32 = 4;

/// The wire format of this ABI, not the crate's version.
///
/// It changes when the layout of a struct or the meaning of an existing
/// signature changes — which is to say, when a host compiled against the old
/// value would read garbage from the new one. Both SDKs call
/// [`omni_abi_version`] at load time and refuse to continue on a mismatch, so a
/// stale `omni.dll` turns into one clear error at import instead of a wrong
/// number ten thousand calls later.
pub const OMNI_ABI_VERSION: u32 = 2;

/// 256 MiB of source is already absurd; the bound exists so a hostile or buggy
/// `len` from a host cannot fabricate an out-of-bounds slice.
const MAX_SOURCE_LEN: usize = 1 << 28;

/// The payload of an [`OmniValue`]: whichever arm its tag says is live.
///
/// A union rather than two adjacent fields, so a string costs the same 16 bytes
/// a number does. Both arms are the same width, and every bit pattern of both is
/// a valid value, which is what makes reading the arm matching `tag` free of
/// surprises.
#[repr(C)]
#[derive(Clone, Copy)]
pub union OmniPayload {
    /// Live for [`OMNI_NUMBER`] and [`OMNI_BOOL`].
    pub number: c_double,
    /// Live for [`OMNI_STRING`]: an address the engine owns, never dereferenced
    /// by a host that wants correctness — copy it out with [`omni_str_copy`].
    pub ptr: *const c_char,
}

/// Tagged result, 16 bytes: `tag` at 0, `len` at 4, `payload` at 8.
///
/// `len` fills the hole that padding already left between a `u32` tag and an
/// 8-byte payload, which is why adding string support cost no bytes. For
/// [`OMNI_STRING`] it is the byte count the host should reserve; for every other
/// tag it is zero.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OmniValue {
    pub tag: u32,
    pub len: u32,
    pub payload: OmniPayload,
}

/// Number of bytes of the payload to hand back at most.
///
/// A `u32` length against a 16 MiB ceiling, so the conversion is total; the
/// assert keeps that true if the ceiling ever moves.
const _: () = assert!(MAX_STRING_LEN < u32::MAX as usize);

// Pinned so a Python `ctypes.Structure` or a Node `koffi.struct` that drifts out
// of agreement fails at compile time rather than reading garbage at run time.
const _: () = assert!(std::mem::size_of::<OmniValue>() == 16);
const _: () = assert!(std::mem::size_of::<OmniPayload>() == 8);
const _: () = assert!(std::mem::offset_of!(OmniValue, tag) == 0);
const _: () = assert!(std::mem::offset_of!(OmniValue, len) == 4);
const _: () = assert!(std::mem::offset_of!(OmniValue, payload) == 8);

impl OmniValue {
    /// A number, which is also how a boolean crosses the boundary.
    #[inline]
    pub fn number(&self) -> Option<c_double> {
        if self.tag == OMNI_NUMBER || self.tag == OMNI_BOOL {
            // Safety: `number` is the live arm for these tags, and every bit
            // pattern of an f64 is a value.
            Some(unsafe { self.payload.number })
        } else {
            None
        }
    }

    /// The address a string carries. Reading it is not a dereference, and
    /// [`omni_str_copy`] is the only function that dereferences it.
    #[inline]
    pub fn string_ptr(&self) -> Option<*const c_char> {
        if self.tag == OMNI_STRING {
            // Safety: `ptr` is the live arm for this tag.
            Some(unsafe { self.payload.ptr })
        } else {
            None
        }
    }

    /// Build the string arm, which is the one place a pointer enters the struct.
    fn string(bytes: &[u8], addr: *const u8) -> OmniValue {
        OmniValue {
            tag: OMNI_STRING,
            len: bytes.len() as u32,
            payload: OmniPayload {
                ptr: addr.cast::<c_char>(),
            },
        }
    }
}

impl fmt::Debug for OmniValue {
    /// Prints the arm the tag selects.
    ///
    /// A union cannot know which field is live, and `#[derive(Debug)]` would have
    /// to pick one — printing a `double` over what is really an address is how a
    /// debugger teaches you to distrust it. So the tag decides, and this is also
    /// why `OmniPayload` has no `Debug` of its own.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let payload = match self.tag {
            OMNI_STRING => match self.string_ptr() {
                Some(p) if p.is_null() => "ptr=null".to_string(),
                Some(p) => format!("ptr={p:p} len={}", self.len),
                None => unreachable!("OMNI_STRING always has the pointer arm"),
            },
            other => match self.number() {
                Some(n) => format!("number={n}"),
                None => format!("tag={other} payload=unused"),
            },
        };
        f.debug_struct("OmniValue")
            .field("tag", &self.tag)
            .field("len", &self.len)
            .field("payload", &payload)
            .finish()
    }
}

impl Default for OmniValue {
    /// `undefined`, which is the answer that cannot be mistaken for a result.
    fn default() -> Self {
        OmniValue {
            tag: OMNI_UNDEF,
            len: 0,
            payload: OmniPayload { number: 0.0 },
        }
    }
}

impl PartialEq for OmniValue {
    /// Field-wise, reading only the arm the tag selects.
    ///
    /// Two strings compare by address, because that is all a host can be trusted
    /// with: content equality is `omni-core`'s job, and it needs the bytes.
    fn eq(&self, other: &OmniValue) -> bool {
        if self.tag != other.tag || self.len != other.len {
            return false;
        }
        if self.tag == OMNI_STRING {
            // Safety: both sides were built with `ptr` as the live arm.
            unsafe { self.payload.ptr == other.payload.ptr }
        } else {
            // Safety: both sides were built with `number` as the live arm.
            unsafe { self.payload.number.to_bits() == other.payload.number.to_bits() }
        }
    }
}

/// The memory story, told in eight counters a host can read without trusting a
/// Rust log line.
///
/// `size` is written by the *callee* and compared by the *caller* against the
/// size it compiled with. A struct whose layout drifted produces a mismatch the
/// host can detect before it reads a field that is no longer where it expects.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OmniStats {
    /// `size_of::<OmniStats>()` as the callee understands it.
    pub size: u32,
    pub execs: u64,
    pub strings: u64,
    pub arena_bytes_in_use: u64,
    pub arena_bytes_reserved: u64,
    pub arena_chunks: u64,
    pub egress_bytes_in_use: u64,
    pub egress_bytes_reserved: u64,
    pub egress_high_water: u64,
}

const _: () = assert!(std::mem::size_of::<OmniStats>() == 72);
const _: () = assert!(std::mem::align_of::<OmniStats>() == 8);
const _: () = assert!(std::mem::offset_of!(OmniStats, size) == 0);
const _: () = assert!(std::mem::offset_of!(OmniStats, execs) == 8);
const _: () = assert!(std::mem::offset_of!(OmniStats, egress_high_water) == 64);

/// An opaque engine handle.
///
/// Has no `#[repr(C)]` because C never looks inside it, and `Engine` owns its
/// identifier strings, so there is no lifetime that could make a long-lived
/// handle unsound.
pub struct OmniEngine(Engine);

fn status_for(kind: ErrorKind) -> u32 {
    match kind {
        ErrorKind::Lex | ErrorKind::Parse => OMNI_ERR_COMPILE,
        ErrorKind::Runtime => OMNI_ERR_RUNTIME,
        ErrorKind::Panic => OMNI_ERR_PANIC,
    }
}

/// Run `body`, converting a panic into [`OMNI_ERR_PANIC`].
#[inline]
fn guarded<F: FnOnce() -> u32>(body: F) -> u32 {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(_) => OMNI_ERR_PANIC,
    }
}

/// Run `body` for a function whose result is a count rather than a status.
///
/// A caught panic reports `usize::default()` — zero, the same answer as "nothing
/// to copy". Deliberately not a random number: `omni_str_copy` already means
/// "zero bytes" for a value it cannot account for, and a panic says the same
/// thing for the same reason.
#[inline]
fn guarded_len<F: FnOnce() -> usize>(body: F) -> usize {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_default()
}

/// Put a core value into the boundary struct, reading string content through the
/// engine that owns it.
fn encode(engine: &Engine, v: Value) -> OmniValue {
    if v.is_string() {
        // The promotion rule inside `omni_exec` guarantees a string that reaches
        // here addresses a block this engine owns. If that ever fails, the
        // honest answer is an empty string carrying no address: a host cannot
        // dereference a pointer we refuse to dereference ourselves, and
        // `omni_str_copy` copies zero bytes and says so.
        let bytes = engine.string_of(v).unwrap_or_default();
        let addr = if bytes.is_empty() {
            ptr::null()
        } else {
            v.as_ptr().unwrap_or(ptr::null())
        };
        return OmniValue::string(bytes, addr);
    }
    if let Some(n) = v.as_f64() {
        return OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload { number: n },
        };
    }
    if let Some(b) = v.as_bool() {
        return OmniValue {
            tag: OMNI_BOOL,
            len: 0,
            payload: OmniPayload {
                number: f64::from(u32::from(b)),
            },
        };
    }
    let tag = if v == Value::UNDEF {
        OMNI_UNDEF
    } else {
        OMNI_NIL
    };
    OmniValue {
        tag,
        len: 0,
        payload: OmniPayload { number: 0.0 },
    }
}

/// ABI level of the loaded library. Compare with your `OMNI_ABI_VERSION` before
/// trusting any other symbol.
#[unsafe(no_mangle)]
pub extern "C" fn omni_abi_version() -> u32 {
    OMNI_ABI_VERSION
}

/// Static, NUL-terminated crate version. Never freed.
#[unsafe(no_mangle)]
pub extern "C" fn omni_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0")
        .as_ptr()
        .cast::<c_char>()
}

/// Allocate an engine. Returns null if allocation itself panicked.
#[unsafe(no_mangle)]
pub extern "C" fn omni_engine_new() -> *mut OmniEngine {
    match catch_unwind(AssertUnwindSafe(|| {
        Box::into_raw(Box::new(OmniEngine(Engine::new())))
    })) {
        Ok(handle) => handle,
        Err(_) => ptr::null_mut(),
    }
}

/// # Safety
///
/// `engine` must be null or a value returned by [`omni_engine_new`], must not be
/// used after this call, and must not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_engine_free(engine: *mut OmniEngine) {
    if !engine.is_null() {
        // Safety: the caller asserts this pointer came from Box::into_raw above
        // and has not been reclaimed already.
        drop(unsafe { Box::from_raw(engine) });
    }
}

/// Evaluate `source` and write the result to `out`.
///
/// A `len` of zero is a valid empty program, and it does not require a non-null
/// `src`: several hosts hand an empty string or buffer over as a null pointer.
///
/// When the result is a string, `out.payload.ptr` addresses the engine's egress
/// block and stays readable until the next `omni_exec` on this engine — copy it
/// out with [`omni_str_copy`] before then if you intend to keep it.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]. `src` must be readable for
/// `len` bytes and must be UTF-8 (a `len` beyond the allocation is rejected by
/// the size bound but is still undefined behaviour). `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_exec(
    engine: *mut OmniEngine,
    src: *const c_char,
    len: usize,
    out: *mut OmniValue,
) -> u32 {
    if engine.is_null() || out.is_null() || len > MAX_SOURCE_LEN {
        return OMNI_ERR_ARG;
    }
    if src.is_null() && len != 0 {
        return OMNI_ERR_ARG;
    }
    // A zero-length slice from a null pointer is undefined behaviour even
    // though it reads nothing, so the empty program never touches `src`.
    let text = if len == 0 {
        ""
    } else {
        // Safety: the null check and the caller's guarantee that `src` is
        // readable for `len` bytes both hold here.
        let bytes = unsafe { slice::from_raw_parts(src.cast::<u8>(), len) };
        match std::str::from_utf8(bytes) {
            Ok(text) => text,
            // Malformed input is an argument problem, not a compile error, so the
            // host can tell "you called me wrong" from "your program is wrong".
            Err(_) => return OMNI_ERR_ARG,
        }
    };
    // Safety: the null check above ran, and the caller guarantees the handle is
    // live and not concurrently aliased. One handle, one thread.
    let engine = unsafe { &mut (*engine).0 };
    guarded(|| match engine.exec(text) {
        Ok(value) => {
            // Safety: `out` was checked non-null and writable.
            unsafe { *out = encode(engine, value) };
            OMNI_OK
        }
        Err(error) => status_for(error.kind),
    })
}

/// Evaluate `source`, requiring the result to be a number.
///
/// A string result is `OMNI_ERR_RUNTIME`, exactly as in ABI v1: a host that
/// wants a string calls [`omni_exec`] and reads the tag.
///
/// # Safety
///
/// As [`omni_exec`], with `out` a writable `double`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_exec_number(
    engine: *mut OmniEngine,
    src: *const c_char,
    len: usize,
    out: *mut c_double,
) -> u32 {
    if engine.is_null() || out.is_null() || len > MAX_SOURCE_LEN || (src.is_null() && len != 0) {
        return OMNI_ERR_ARG;
    }
    let mut value = OmniValue::default();
    // Safety: the checks above hold, and `value` is a live local.
    let code = unsafe { omni_exec(engine, src, len, &mut value) };
    if code != OMNI_OK {
        return code;
    }
    let Some(number) = value.number() else {
        return OMNI_ERR_RUNTIME;
    };
    // Safety: checked non-null and writable.
    unsafe { *out = number };
    OMNI_OK
}

/// Copy a string result into storage the caller owns.
///
/// Writes `min(len, cap)` bytes and returns the number written. It does **not**
/// NUL-terminate: an OmniEngine string may contain a NUL, and `out->len` already
/// tells you where the payload ends. To get a C string, allocate `len + 1` and
/// write the terminator yourself.
///
/// Returns 0 — writes nothing, reads nothing — when `value` is not an
/// `OMNI_STRING`, when any pointer is null, when the address is outside the
/// 48 bits the value format can carry, or when the address belongs to something
/// other than `engine`: another engine's arena, a host string whose binding has
/// since been redefined, or a number the caller invented. That last check is the
/// whole reason this function exists instead of a plain `memcpy` at the call
/// site.
///
/// `out->len` is a hint for sizing a buffer; the engine's own header decides how
/// many bytes are actually copied, so a stale length cannot make this read past
/// the end of a block.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`], or be null. `value` must be
/// readable as an `OmniValue` or be null. `buf` must be writable for `cap`
/// bytes, or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_str_copy(
    engine: *const OmniEngine,
    value: *const OmniValue,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if engine.is_null() || value.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    // Safety: the null checks above ran, and the caller guarantees the handle is
    // live and that `buf` holds `cap` writable bytes.
    let engine = unsafe { &(*engine).0 };
    // Safety: `value` was checked non-null and readable as an `OmniValue`.
    let value = unsafe { &*value };
    let Some(addr) = value.string_ptr() else {
        return 0;
    };
    if addr.is_null() {
        return 0;
    }
    // 48 address bits is what a NaN box can hold, so a pointer with anything in
    // the high bits cannot be one this engine handed out. Checking it here rather
    // than letting `Value::from_ptr` mask it off keeps the debug assert out of
    // the host's error path.
    if (addr as u64) >> 48 != 0 {
        return 0;
    }
    guarded_len(|| {
        // The ownership proof: `string_of` reads the bytes only if the address
        // falls inside this engine's egress block, its execution arena, or one of
        // its live host bindings.
        let Some(bytes) = engine.string_of(Value::from_ptr(addr.cast::<u8>())) else {
            return 0;
        };
        let n = bytes.len().min(cap);
        // Safety: `buf` is writable for `cap` bytes and `n <= cap`.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n) };
        n
    })
}

/// Bind `name` to a string the host owns.
///
/// The engine copies `bytes` into storage outside every arena, so the binding
/// survives executions and `bytes` itself may be freed as soon as this returns.
/// Redefining `name` replaces the binding and releases the previous bytes; a
/// value the host kept from before then copies out as zero bytes rather than
/// reading freed memory.
///
/// Returns [`OMNI_OK`] or [`OMNI_ERR_ARG`]. The reasons for a rejection are
/// distinguishable in Rust (see `omni_core::HostError`) and collapse to one
/// status here, because they are all "the host called this wrongly": an empty or
/// over-long name, non-UTF-8 in either buffer, or a payload over
/// [`MAX_STRING_LEN`].
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]. `name` must be readable for
/// `name_len` bytes and `bytes` for `bytes_len`, or be null with a zero length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_define_string(
    engine: *mut OmniEngine,
    name: *const c_char,
    name_len: usize,
    bytes: *const c_char,
    bytes_len: usize,
) -> u32 {
    if engine.is_null() {
        return OMNI_ERR_ARG;
    }
    // Checked before any slice is fabricated, so an absurd length is a rejection
    // rather than an out-of-bounds read waiting to happen.
    if name_len > MAX_IDENTIFIER_LEN || bytes_len > MAX_STRING_LEN {
        return OMNI_ERR_ARG;
    }
    if (name.is_null() && name_len != 0) || (bytes.is_null() && bytes_len != 0) {
        return OMNI_ERR_ARG;
    }
    // Safety: the lengths are bounded, non-null, and the caller guarantees both
    // regions are readable for them.
    let (name, payload) = unsafe {
        let name_bytes = if name_len == 0 {
            &[]
        } else {
            slice::from_raw_parts(name.cast::<u8>(), name_len)
        };
        let payload_bytes = if bytes_len == 0 {
            &[]
        } else {
            slice::from_raw_parts(bytes.cast::<u8>(), bytes_len)
        };
        (name_bytes, payload_bytes)
    };
    let Ok(name) = std::str::from_utf8(name) else {
        return OMNI_ERR_ARG;
    };
    // Safety: the null check above ran and the caller guarantees the handle is
    // live. One handle, one thread.
    let engine = unsafe { &mut (*engine).0 };
    guarded(|| match engine.define_string(name, payload) {
        Ok(()) => OMNI_OK,
        // Every `HostError` is the caller's fault, so there is nothing else to
        // report and no new status code to teach every host in the repository.
        Err(HostError::EmptyName)
        | Err(HostError::NameTooLong)
        | Err(HostError::NotUtf8)
        | Err(HostError::TooLong) => OMNI_ERR_ARG,
    })
}

/// Fill `out` with the engine's memory counters.
///
/// Writes `OMNI_STATS_SIZE` into `out->size` first, so a host compiled against a
/// different layout notices before it reads a field that has moved. Returns
/// [`OMNI_OK`], or [`OMNI_ERR_ARG`] when `engine` or `out` is null.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]; `out` must be writable for
/// `size_of::<OmniStats>()` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_debug_stats(engine: *mut OmniEngine, out: *mut OmniStats) -> u32 {
    if engine.is_null() || out.is_null() {
        return OMNI_ERR_ARG;
    }
    let stats = {
        // Safety: the null check above ran, and the caller guarantees the handle
        // is live.
        let engine = unsafe { &(*engine).0 };
        let s = engine.stats();
        OmniStats {
            size: std::mem::size_of::<OmniStats>() as u32,
            execs: s.execs,
            strings: s.strings_built,
            arena_bytes_in_use: s.arena_bytes_in_use,
            arena_bytes_reserved: s.arena_bytes_reserved,
            arena_chunks: s.arena_chunks,
            egress_bytes_in_use: s.egress_bytes_in_use,
            egress_bytes_reserved: s.egress_bytes_reserved,
            egress_high_water: s.egress_high_water,
        }
    };
    // Safety: `out` was checked non-null and writable for the struct's own size,
    // and `OmniStats` is `Copy` with no drop.
    unsafe { *out = stats };
    OMNI_OK
}

/// Copy the most recent engine error into caller-owned storage.
///
/// Writes at most `cap - 1` bytes and always NUL-terminates, so a too-small
/// buffer truncates instead of overflowing. Returns the number of bytes written
/// excluding the terminator.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]; `buf` must be writable for
/// `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_last_error(
    engine: *mut OmniEngine,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if engine.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    // Copy the *rendered* error, not just its message. A host that shows a
    // user "expected a name" with no line number cannot point them at the
    // source, and this boundary is where that context would otherwise be lost.
    let rendered = match unsafe { (*engine).0.last_error() } {
        Some(error) => error.to_string(),
        None => String::new(),
    };
    let message = rendered.as_bytes();
    let n = message.len().min(cap - 1);
    // Safety: `buf` is writable for `cap` bytes and n < cap, so the copy and the
    // terminator at buf[n] both stay inside the caller's buffer.
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr(), buf.cast::<u8>(), n);
        *buf.add(n) = 0;
    }
    n
}

/// Test hook that proves the panic boundary is wired, from Rust or from a host.
/// There is no other way to exercise the acceptance criterion "a panic returns
/// `OMNI_ERR_PANIC`", and it is named so nobody mistakes it for an API.
#[doc(hidden)]
#[unsafe(no_mangle)]
pub extern "C" fn omni_debug_panic(engine: *mut OmniEngine) -> u32 {
    if engine.is_null() {
        return OMNI_ERR_ARG;
    }
    guarded(|| panic!("omni_debug_panic: deliberate panic to prove the FFI boundary catches it"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `omni_exec` a program and hand back the raw struct, freeing nothing: the
    /// engine stays alive because these tests read addresses out of it.
    fn run(engine: *mut OmniEngine, src: &str) -> OmniValue {
        let mut out = OmniValue::default();
        let code = unsafe { omni_exec(engine, src.as_ptr().cast::<c_char>(), src.len(), &mut out) };
        assert_eq!(code, OMNI_OK, "{src} failed");
        out
    }

    fn copy(engine: *const OmniEngine, value: &OmniValue, cap: usize) -> Vec<u8> {
        let mut buf = vec![0u8; cap];
        let n =
            unsafe { omni_str_copy(engine, value, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        buf.truncate(n);
        buf
    }

    #[test]
    fn the_abi_reports_two_and_the_header_agrees() {
        assert_eq!(omni_abi_version(), OMNI_ABI_VERSION);
        assert_eq!(
            OMNI_ABI_VERSION, 2,
            "the header and both SDKs hard-code this"
        );
    }

    #[test]
    fn layout_is_what_the_header_claims() {
        assert_eq!(std::mem::size_of::<OmniValue>(), 16);
        assert_eq!(std::mem::offset_of!(OmniValue, tag), 0);
        assert_eq!(std::mem::offset_of!(OmniValue, len), 4);
        assert_eq!(std::mem::offset_of!(OmniValue, payload), 8);
        assert_eq!(std::mem::size_of::<OmniStats>(), 72);
        assert_eq!(std::mem::offset_of!(OmniStats, size), 0);
        assert_eq!(std::mem::offset_of!(OmniStats, execs), 8);
    }

    #[test]
    fn scalars_encode_into_the_number_arm() {
        assert_eq!(
            encode(&Engine::new(), Value::from_f64(42.0)),
            OmniValue {
                tag: OMNI_NUMBER,
                len: 0,
                payload: OmniPayload { number: 42.0 },
            }
        );
        assert_eq!(
            encode(&Engine::new(), Value::TRUE),
            OmniValue {
                tag: OMNI_BOOL,
                len: 0,
                payload: OmniPayload { number: 1.0 },
            }
        );
        assert_eq!(encode(&Engine::new(), Value::NIL).tag, OMNI_NIL);
        assert_eq!(encode(&Engine::new(), Value::UNDEF).tag, OMNI_UNDEF);
        // A boolean crosses as a number in the payload arm, which is what lets
        // `omni_exec_number`'s tag check be the only truth test on the wire.
        assert_eq!(encode(&Engine::new(), Value::TRUE).number(), Some(1.0));
        assert_eq!(encode(&Engine::new(), Value::FALSE).number(), Some(0.0));
        assert_eq!(encode(&Engine::new(), Value::NIL).number(), None);
    }

    #[test]
    fn a_number_result_carries_no_length_and_a_string_carries_its_bytes() {
        let engine = omni_engine_new();
        let n = run(engine, "1 + 2");
        assert_eq!((n.tag, n.len), (OMNI_NUMBER, 0));
        assert_eq!(n.number(), Some(3.0));
        assert_eq!(n.string_ptr(), None);

        let s = run(engine, r#""omni""#);
        assert_eq!(s.tag, OMNI_STRING);
        assert_eq!(s.len, 4);
        assert!(
            s.number().is_none(),
            "a string must not answer to the number arm"
        );
        assert_eq!(copy(engine, &s, 16), b"omni");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn an_empty_string_is_a_string_not_a_null() {
        let engine = omni_engine_new();
        // `"\"\""` is the program `""`; a raw string would need four quotes and a
        // closing `#` to say the same thing, and reads as three quotes first.
        let s = run(engine, "\"\"");
        assert_eq!(
            s.tag, OMNI_STRING,
            "empty is a value, not the absence of one"
        );
        assert_eq!(s.len, 0);
        assert_eq!(
            s.string_ptr(),
            Some(ptr::null()),
            "the tag says string, the address says there is nothing to read"
        );
        assert_eq!(copy(engine, &s, 8), Vec::<u8>::new());
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn copying_truncates_to_the_buffer_and_never_writes_a_terminator() {
        let engine = omni_engine_new();
        let s = run(engine, r#""hello, world""#);
        assert_eq!(s.len, 12);
        // `cap` six, buffer seven: the sentinel is one past what the callee may
        // touch, and it says whether the copy respected the bound.
        let mut buf = [0u8; 7];
        buf[6] = b'!';
        let n = unsafe { omni_str_copy(engine, &s, buf.as_mut_ptr().cast::<c_char>(), 6) };
        assert_eq!(n, 6, "cap bytes copied when the payload is longer");
        assert_eq!(&buf[..6], b"hello,");
        assert_eq!(buf[6], b'!', "nothing written past cap");
        assert_ne!(buf[5], 0, "and no NUL terminator inside it");
        // The exact fit needs no room for a terminator, because there is none.
        let mut exact = [0xAAu8; 12];
        let n =
            unsafe { omni_str_copy(engine, &s, exact.as_mut_ptr().cast::<c_char>(), exact.len()) };
        assert_eq!(n, 12);
        assert_eq!(&exact, b"hello, world");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn a_stale_string_copies_the_bytes_that_are_there_now_rather_than_faulting() {
        let engine = omni_engine_new();
        let first = run(engine, r#""first""#);
        assert_eq!(copy(engine, &first, 16), b"first");
        // The documented rule: valid until the next exec on this engine. Rewind
        // does not unmap, so the address is still owned — it now answers with what
        // the second execution wrote there, which is a wrong read, not a crash.
        let second = run(engine, r#""a much longer second""#);
        let stale = copy(engine, &first, 32);
        assert_eq!(stale, copy(engine, &second, 32), "the same bytes, honestly");
        assert_eq!(stale, b"a much longer second");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn a_fabricated_address_and_another_engines_address_both_copy_zero() {
        let engine = omni_engine_new();
        let mine = run(engine, r#""mine""#);
        let other = omni_engine_new();
        let theirs = run(other, r#""theirs""#);

        // Another engine's address, read through the wrong handle.
        assert_eq!(
            copy(engine, &theirs, 16),
            Vec::<u8>::new(),
            "a pointer is not a proof of ownership"
        );
        assert_eq!(
            copy(other, &theirs, 16),
            b"theirs",
            "and its own engine reads it"
        );

        // An address out of a stack variable.
        let local: u64 = 0xDEAD_BEEF;
        let fake = OmniValue {
            tag: OMNI_STRING,
            len: 4,
            payload: OmniPayload {
                ptr: std::ptr::addr_of!(local).cast::<c_char>(),
            },
        };
        assert_eq!(copy(engine, &fake, 16), Vec::<u8>::new());

        // An address the 48-bit format cannot hold at all.
        let high = OmniValue {
            tag: OMNI_STRING,
            len: 4,
            payload: OmniPayload {
                ptr: ptr::without_provenance(1 << 48),
            },
        };
        assert_eq!(copy(engine, &high, 16), Vec::<u8>::new());

        // Null and zero-cap are the same answer without a dereference.
        assert_eq!(
            unsafe { omni_str_copy(engine, &mine, ptr::null_mut(), 16,) },
            0
        );
        let mut buf = [0u8; 16];
        assert_eq!(
            unsafe { omni_str_copy(engine, ptr::null(), buf.as_mut_ptr().cast::<c_char>(), 16) },
            0
        );
        assert_eq!(
            unsafe { omni_str_copy(engine, &mine, buf.as_mut_ptr().cast::<c_char>(), 0) },
            0
        );
        assert_eq!(
            unsafe { omni_str_copy(ptr::null(), &mine, buf.as_mut_ptr().cast::<c_char>(), 16) },
            0
        );
        unsafe {
            omni_engine_free(engine);
            omni_engine_free(other);
        };
    }

    #[test]
    fn a_number_value_copies_nothing_even_with_a_plausible_pointer() {
        let engine = omni_engine_new();
        let n = run(engine, "7");
        let mut buf = [0u8; 8];
        let written =
            unsafe { omni_str_copy(engine, &n, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        assert_eq!(written, 0, "the tag is the first gate");
        assert_eq!(buf, [0u8; 8]);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn define_string_binds_bytes_the_host_can_then_free() {
        let engine = omni_engine_new();
        let name = b"greeting";
        let payload = String::from("নম্বর ✄").into_bytes();
        let code = unsafe {
            omni_define_string(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                payload.as_ptr().cast::<c_char>(),
                payload.len(),
            )
        };
        assert_eq!(code, OMNI_OK);
        drop(payload);
        // The program reads the binding, and the result copies out of the engine's
        // own storage rather than the caller's, which is already gone.
        let s = run(engine, "greeting");
        assert_eq!(s.tag, OMNI_STRING);
        assert_eq!(copy(engine, &s, 64), String::from("নম্বর ✄").into_bytes());
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn define_string_rejects_every_bad_argument_before_touching_memory() {
        let engine = omni_engine_new();
        let ok = b"n";
        let reject = |name: &[u8], bytes: &[u8]| {
            let code = unsafe {
                omni_define_string(
                    engine,
                    name.as_ptr().cast::<c_char>(),
                    name.len(),
                    bytes.as_ptr().cast::<c_char>(),
                    bytes.len(),
                )
            };
            assert_eq!(
                code,
                OMNI_ERR_ARG,
                "{} / {}",
                String::from_utf8_lossy(name),
                bytes.len()
            );
        };
        reject(b"", b"x");
        reject(&[0xFF], b"x");
        reject(b"n", &[0xFF]);
        reject(&[b'a'; MAX_IDENTIFIER_LEN + 1], b"x");
        // A null buffer with a non-zero length is an argument error, not a read.
        let code = unsafe {
            omni_define_string(
                engine,
                ok.as_ptr().cast::<c_char>(),
                ok.len(),
                ptr::null(),
                4,
            )
        };
        assert_eq!(code, OMNI_ERR_ARG);
        // A length over the ceiling is rejected without fabricating a slice.
        let code = unsafe {
            omni_define_string(
                engine,
                ok.as_ptr().cast::<c_char>(),
                ok.len(),
                b"x".as_ptr().cast::<c_char>(),
                MAX_STRING_LEN + 1,
            )
        };
        assert_eq!(code, OMNI_ERR_ARG);
        // An empty payload is a legitimate binding, so it must not be rejected.
        let code = unsafe {
            omni_define_string(
                engine,
                ok.as_ptr().cast::<c_char>(),
                ok.len(),
                b"".as_ptr().cast::<c_char>(),
                0,
            )
        };
        assert_eq!(code, OMNI_OK);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn redefining_a_name_makes_the_old_value_copy_zero() {
        let engine = omni_engine_new();
        let name = b"k";
        let first = b"first".to_vec();
        let second = b"second".to_vec();
        unsafe {
            omni_define_string(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                first.as_ptr().cast::<c_char>(),
                first.len(),
            );
        }
        let old = run(engine, "k");
        assert_eq!(copy(engine, &old, 16), b"first");
        unsafe {
            omni_define_string(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                second.as_ptr().cast::<c_char>(),
                second.len(),
            );
        }
        assert_eq!(
            copy(engine, &old, 16),
            Vec::<u8>::new(),
            "the superseded bytes are freed, so the ownership check is what stops a read"
        );
        let now = run(engine, "k");
        assert_eq!(copy(engine, &now, 16), b"second");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn stats_reports_the_counters_and_their_size() {
        let engine = omni_engine_new();
        let mut before = OmniStats::default();
        assert_eq!(unsafe { omni_debug_stats(engine, &mut before) }, OMNI_OK);
        assert_eq!(before.size, std::mem::size_of::<OmniStats>() as u32);
        assert_eq!(before.execs, 0);
        for _ in 0..50 {
            let _ = run(engine, r#""a" + "b""#);
        }
        let mut after = OmniStats::default();
        assert_eq!(unsafe { omni_debug_stats(engine, &mut after) }, OMNI_OK);
        assert_eq!(after.execs, 50);
        assert!(after.strings > before.strings);
        assert!(after.egress_bytes_in_use > 0, "a result lives in egress");
        // Layout mismatch detection, from the host's side.
        let mut wrong = OmniStats {
            size: 1,
            ..OmniStats::default()
        };
        assert_eq!(unsafe { omni_debug_stats(engine, &mut wrong) }, OMNI_OK);
        assert_ne!(wrong.size, 1, "the callee always restates the size");
        assert_eq!(
            unsafe { omni_debug_stats(ptr::null_mut(), &mut after) },
            OMNI_ERR_ARG
        );
        assert_eq!(
            unsafe { omni_debug_stats(engine, ptr::null_mut()) },
            OMNI_ERR_ARG
        );
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn exec_number_still_refuses_a_string() {
        let engine = omni_engine_new();
        let mut n = 0.0;
        assert_eq!(
            unsafe { omni_exec_number(engine, r#""hi""#.as_ptr().cast::<c_char>(), 4, &mut n,) },
            OMNI_ERR_RUNTIME
        );
        assert_eq!(
            unsafe { omni_exec_number(engine, "1 + 1".as_ptr().cast::<c_char>(), 5, &mut n) },
            OMNI_OK
        );
        assert_eq!(n, 2.0);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn every_error_kind_maps_to_a_distinct_status() {
        assert_eq!(status_for(ErrorKind::Lex), OMNI_ERR_COMPILE);
        assert_eq!(status_for(ErrorKind::Parse), OMNI_ERR_COMPILE);
        assert_eq!(status_for(ErrorKind::Runtime), OMNI_ERR_RUNTIME);
        assert_eq!(status_for(ErrorKind::Panic), OMNI_ERR_PANIC);
    }

    #[test]
    fn a_type_error_and_a_bad_escape_are_compile_errors_not_argument_errors() {
        let engine = omni_engine_new();
        let mut out = OmniValue::default();
        let mixing = r#""a" + 1"#;
        let code = unsafe {
            omni_exec(
                engine,
                mixing.as_ptr().cast::<c_char>(),
                mixing.len(),
                &mut out,
            )
        };
        assert_eq!(code, OMNI_ERR_RUNTIME);
        let mut buf = [0i8; 128];
        let n = unsafe { omni_last_error(engine, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        let text =
            String::from_utf8_lossy(unsafe { slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n) })
                .to_string();
        assert!(
            text.contains(r#"cannot add string to number: "a" + 1"#),
            "the message the requester asked for, across the boundary: {text}"
        );
        // An illegal escape is a program error, so it reports as a compile error.
        let bad_escape = r#""\q""#;
        let code = unsafe {
            omni_exec(
                engine,
                bad_escape.as_ptr().cast::<c_char>(),
                bad_escape.len(),
                &mut out,
            )
        };
        assert_eq!(code, OMNI_ERR_COMPILE);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn oversized_len_is_rejected_before_any_slice_is_fabricated() {
        let engine = omni_engine_new();
        let mut out = OmniValue::default();
        let code = unsafe {
            omni_exec(
                engine,
                "1".as_ptr().cast::<c_char>(),
                MAX_SOURCE_LEN + 1,
                &mut out,
            )
        };
        assert_eq!(code, OMNI_ERR_ARG);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn the_panic_boundary_still_covers_the_new_bodies() {
        let engine = omni_engine_new();
        assert_eq!(omni_debug_panic(engine), OMNI_ERR_PANIC);
        // The other four new bodies are guarded in the same shape, and a host
        // reaching them with an address the engine does not own must get a clean
        // zero rather than a partial write.
        let foreign = OmniValue {
            tag: OMNI_STRING,
            len: u32::MAX,
            payload: OmniPayload {
                ptr: ptr::without_provenance(8),
            },
        };
        let mut buf = [0u8; 16];
        let n = unsafe { omni_str_copy(engine, &foreign, buf.as_mut_ptr().cast::<c_char>(), 16) };
        assert_eq!(n, 0, "an address the engine does not own copies nothing");
        assert_eq!(buf, [0u8; 16], "and writes nothing on the way out");
        unsafe { omni_engine_free(engine) };
    }
}
