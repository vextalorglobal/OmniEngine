//! OmniEngine's C ABI.
//!
//! This is the only crate permitted to name a `#[repr(C)]` type or an
//! `extern "C"` function, which is what keeps `omni-core` from becoming
//! FFI-shaped by accident.
//!
//! ## The two rules that make this sound
//!
//! **No arena pointer crosses the boundary.** `omni-core` reclaims a whole
//! arena at the start of every `exec`, so a `*const Object` handed to Python
//! would dangle on the next call. Results therefore leave as eight-byte tagged
//! values, and text leaves by being copied into storage the caller owns.
//!
//! **A Rust panic may never unwind into a host.** Unwinding past `extern "C"`
//! is undefined behaviour, and it is the most common defect in hand-rolled Rust
//! FFI. Every body here runs inside `catch_unwind`, and a caught panic becomes
//! [`OMNI_ERR_PANIC`] rather than a crashed interpreter.

use std::ffi::{c_char, c_double};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;

use omni_core::{Engine, ErrorKind, Value};

pub const OMNI_OK: u32 = 0;
pub const OMNI_ERR_COMPILE: u32 = 1;
pub const OMNI_ERR_RUNTIME: u32 = 2;
pub const OMNI_ERR_PANIC: u32 = 3;
pub const OMNI_ERR_ARG: u32 = 4;

pub const OMNI_NUMBER: u32 = 0;
pub const OMNI_NIL: u32 = 1;
pub const OMNI_BOOL: u32 = 2;
pub const OMNI_UNDEF: u32 = 3;

/// 256 MiB of source is already absurd; the bound exists so a hostile or buggy
/// `len` from a host cannot fabricate an out-of-bounds slice.
const MAX_SOURCE_LEN: usize = 1 << 28;

/// Tagged result, passed by value.
///
/// Deliberately not a `union`: no string values exist yet, so a pointer arm
/// would be a variant nothing populates that every host must still read
/// unsafely. It arrives with strings in Phase 2.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OmniValue {
    pub tag: u32,
    /// For [`OMNI_BOOL`], 0.0 or 1.0. Zero for tags that carry no payload.
    pub number: c_double,
}

// Pinned so a Python `ctypes.Structure` or a Node `koffi.struct` that drifts out
// of agreement fails at compile time rather than reading garbage at run time.
const _: () = assert!(std::mem::size_of::<OmniValue>() == 16);
const _: () = assert!(std::mem::offset_of!(OmniValue, number) == 8);

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

fn encode(v: Value) -> OmniValue {
    if let Some(n) = v.as_f64() {
        OmniValue {
            tag: OMNI_NUMBER,
            number: n,
        }
    } else if let Some(b) = v.as_bool() {
        OmniValue {
            tag: OMNI_BOOL,
            number: f64::from(u32::from(b)),
        }
    } else if v == Value::UNDEF {
        OmniValue {
            tag: OMNI_UNDEF,
            number: 0.0,
        }
    } else {
        OmniValue {
            tag: OMNI_NIL,
            number: 0.0,
        }
    }
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
            unsafe { *out = encode(value) };
            OMNI_OK
        }
        Err(error) => status_for(error.kind),
    })
}

/// Evaluate `source`, requiring the result to be a number.
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
    if value.tag != OMNI_NUMBER {
        return OMNI_ERR_RUNTIME;
    }
    // Safety: checked non-null and writable.
    unsafe { *out = value.number };
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

    #[test]
    fn layout_is_what_the_header_claims() {
        assert_eq!(std::mem::size_of::<OmniValue>(), 16);
        assert_eq!(std::mem::offset_of!(OmniValue, tag), 0);
        assert_eq!(std::mem::offset_of!(OmniValue, number), 8);
    }

    #[test]
    fn bool_and_undef_encode_without_a_number_arm() {
        assert_eq!(
            encode(Value::TRUE),
            OmniValue {
                tag: OMNI_BOOL,
                number: 1.0
            }
        );
        assert_eq!(
            encode(Value::UNDEF),
            OmniValue {
                tag: OMNI_UNDEF,
                number: 0.0
            }
        );
        assert_eq!(
            encode(Value::NIL),
            OmniValue {
                tag: OMNI_NIL,
                number: 0.0
            }
        );
        assert_eq!(
            encode(Value::from_f64(42.0)),
            OmniValue {
                tag: OMNI_NUMBER,
                number: 42.0
            }
        );
    }

    #[test]
    fn every_error_kind_maps_to_a_distinct_status() {
        assert_eq!(status_for(ErrorKind::Lex), OMNI_ERR_COMPILE);
        assert_eq!(status_for(ErrorKind::Parse), OMNI_ERR_COMPILE);
        assert_eq!(status_for(ErrorKind::Runtime), OMNI_ERR_RUNTIME);
        assert_eq!(status_for(ErrorKind::Panic), OMNI_ERR_PANIC);
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
}
