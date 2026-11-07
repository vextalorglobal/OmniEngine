//! Phase 3: host callbacks through the C ABI, and the batch entry point's share
//! of the same guard.
//!
//! These tests are what the header's caller rules 8, 9, 10, and 11 mean in code.
//! They exercise the boundary as an external C host would: raw pointers, a
//! function registered by address, no Rust-side ownership.

use std::ffi::c_void;
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering};

use omni::*;

static LAST_ARGC: AtomicU32 = AtomicU32::new(u32::MAX);
static LAST_STATUS: AtomicU32 = AtomicU32::new(0);
/// What `omni_batch_error` returned from inside a callback. A `usize` widened to
/// u32 is enough: the answer under test is 0.
static BATCH_ERROR_LEN: AtomicU32 = AtomicU32::new(u32::MAX);

/// Read one string argument the way the header says to read it: a borrowed view
/// of `len` bytes, copied immediately, with `omni_str_copy` forbidden.
///
/// Safety: `args[0].payload.ptr` addresses `len` readable bytes owned by the
/// engine for the duration of this call. This is the documented callback
/// contract, and it holds only while the engine's own borrow is live.
unsafe fn argument_text(args: *const OmniValue) -> Vec<u8> {
    let value = unsafe { &*args };
    assert_eq!(value.tag, OMNI_STRING, "argument is not a string");
    let bytes =
        unsafe { slice::from_raw_parts(value.payload.ptr.cast::<u8>(), value.len as usize) };
    bytes.to_vec()
}

unsafe extern "C" fn square(
    _user_data: *mut c_void,
    args: *const OmniValue,
    argc: u32,
    out: *mut OmniValue,
) -> i32 {
    assert_eq!(argc, 1, "square is registered with arity 1");
    let n = unsafe { (*args).number().unwrap_or_default() };
    unsafe {
        *out = OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload { number: n * n },
        };
    }
    OMNI_HOST_OK
}

unsafe extern "C" fn shout(
    _user_data: *mut c_void,
    args: *const OmniValue,
    argc: u32,
    out: *mut OmniValue,
) -> i32 {
    assert_eq!(argc, 1, "shout is registered with arity 1");
    let bytes = unsafe { argument_text(args) };
    let text = String::from_utf8_lossy(&bytes).to_uppercase() + "!";
    let payload = text.into_bytes();
    // Returning a string is the mirror image of the argument rule: hand back
    // your own bytes and let the engine copy them before your frame is gone.
    unsafe {
        *out = OmniValue {
            tag: OMNI_STRING,
            len: payload.len() as u32,
            payload: OmniPayload {
                ptr: payload.as_ptr() as *const std::ffi::c_char,
            },
        };
    }
    // The engine reads `out` immediately, so the payload lives long enough:
    // this leak is intentional to keep the test simple without a static arena.
    std::mem::forget(payload);
    OMNI_HOST_OK
}

unsafe extern "C" fn try_reentrant(
    _user_data: *mut c_void,
    _args: *const OmniValue,
    _argc: u32,
    out: *mut OmniValue,
) -> i32 {
    LAST_ARGC.store(_argc, Ordering::SeqCst);
    let engine = _user_data as *mut OmniEngine;
    // Call back into the engine that called this function. The borrow is
    // already held mutably, so this reports re-entrancy rather than
    // aliasing a `RefMut` -- the one case a boolean flag could not catch.
    let mut result = OmniValue::default();
    let source = "1 + 1";
    let status = unsafe {
        omni_exec(
            engine,
            source.as_ptr() as *const std::ffi::c_char,
            source.len(),
            &mut result,
        )
    };
    LAST_STATUS.store(status, Ordering::SeqCst);
    unsafe {
        *out = OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload { number: 0.0 },
        };
    }
    OMNI_HOST_OK
}

/// The batch entry point tried from inside a callback, plus the per-item error
/// reader. Both must refuse: the batch on record mid-execution is the *previous*
/// one, and answering from it would describe a call this callback is not inside.
unsafe extern "C" fn try_batch_from_callback(
    user_data: *mut c_void,
    _args: *const OmniValue,
    _argc: u32,
    out: *mut OmniValue,
) -> i32 {
    let engine = user_data as *mut OmniEngine;
    let source = "1 + 1";
    let sources = [source.as_ptr().cast::<std::ffi::c_char>()];
    let lengths = [source.len()];
    let mut value = OmniValue::default();
    let mut status = u32::MAX;
    let code = unsafe {
        omni_exec_many(
            engine,
            sources.as_ptr(),
            lengths.as_ptr(),
            1,
            &mut value,
            &mut status,
        )
    };
    LAST_STATUS.store(code, Ordering::SeqCst);
    // A refused batch must not have touched either output array.
    assert_eq!(status, u32::MAX, "a refused batch wrote a status");
    assert_eq!(value.tag, OMNI_UNDEF, "a refused batch wrote a value");
    let mut buf = [0u8; 64];
    let n = unsafe {
        omni_batch_error(
            engine,
            0,
            buf.as_mut_ptr().cast::<std::ffi::c_char>(),
            buf.len(),
        )
    };
    BATCH_ERROR_LEN.store(n as u32, Ordering::SeqCst);
    unsafe {
        *out = OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload { number: 7.0 },
        };
    }
    OMNI_HOST_OK
}

unsafe extern "C" fn try_copy_from_callback(
    _user_data: *mut c_void,
    args: *const OmniValue,
    _argc: u32,
    out: *mut OmniValue,
) -> i32 {
    // Calling `omni_str_copy` re-enters this same engine with a shared
    // borrow during the outer mutable borrow. The rule is that it reports
    // re-entrancy (and copies 0 bytes), never crashes or reads garbage.
    let mut buf = [0u8; 32];
    let engine = _user_data as *const OmniEngine;
    let status = unsafe {
        omni_str_copy(
            engine,
            args,
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    LAST_STATUS.store(status as u32, Ordering::SeqCst);
    unsafe {
        *out = OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload { number: 0.0 },
        };
    }
    OMNI_HOST_OK
}

unsafe extern "C" fn always_fails(
    _user_data: *mut c_void,
    _args: *const OmniValue,
    _argc: u32,
    out: *mut OmniValue,
) -> i32 {
    let msg: &'static [u8] = Box::leak(b"boom".to_vec().into_boxed_slice());
    unsafe {
        *out = OmniValue {
            tag: OMNI_STRING,
            len: msg.len() as u32,
            payload: OmniPayload {
                ptr: msg.as_ptr() as *const std::ffi::c_char,
            },
        };
    }
    OMNI_HOST_ERR
}

#[test]
fn a_number_callback_answers_a_script() {
    let engine = omni_engine_new();
    let name = b"square";
    unsafe {
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                1,
                Some(square),
                std::ptr::null_mut()
            ),
            OMNI_OK
        );
        assert_eq!(omni_host_fn_count(engine), 1);

        let mut out = OmniValue::default();
        let source = "square(6) + 1";
        assert_eq!(
            omni_exec(
                engine,
                source.as_ptr() as *const std::ffi::c_char,
                source.len(),
                &mut out
            ),
            OMNI_OK
        );
        assert_eq!(out.tag, OMNI_NUMBER);
        assert_eq!(out.number(), Some(37.0));

        assert_eq!(
            omni_unregister_host_fn(engine, name.as_ptr() as *const std::ffi::c_char, name.len()),
            1
        );
        assert_eq!(omni_host_fn_count(engine), 0);

        // Calling the name now is a runtime error, not a dangling callback call.
        let mut out2 = OmniValue::default();
        let source2 = "square(2)";
        assert_eq!(
            omni_exec(
                engine,
                source2.as_ptr() as *const std::ffi::c_char,
                source2.len(),
                &mut out2
            ),
            OMNI_ERR_RUNTIME
        );

        omni_engine_free(engine);
    }
}

#[test]
fn a_string_callback_reads_the_borrowed_bytes_directly() {
    let engine = omni_engine_new();
    let name = b"shout";
    unsafe {
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                1,
                Some(shout),
                std::ptr::null_mut()
            ),
            OMNI_OK
        );

        let mut out = OmniValue::default();
        let source = r#"shout("omni")"#;
        assert_eq!(
            omni_exec(
                engine,
                source.as_ptr() as *const std::ffi::c_char,
                source.len(),
                &mut out
            ),
            OMNI_OK
        );
        assert_eq!(out.tag, OMNI_STRING);
        assert_eq!(out.len, 5); // OMNI!
        let mut buf = vec![0u8; 5];
        let n = omni_str_copy(
            engine,
            &out,
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        );
        assert_eq!(n, 5);
        assert_eq!(&buf[..], b"OMNI!");

        assert_eq!(
            omni_unregister_host_fn(engine, name.as_ptr() as *const std::ffi::c_char, name.len()),
            1
        );
        omni_engine_free(engine);
    }
}

#[test]
fn reentry_from_a_callback_is_a_status_not_aliasing() {
    let engine = omni_engine_new();
    let name = b"try_reentrant";
    let engine_data = engine as *mut c_void;
    unsafe {
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                0,
                Some(try_reentrant),
                engine_data,
            ),
            OMNI_OK
        );

        let mut out = OmniValue::default();
        let source = "try_reentrant()";
        // Host returns 0 (OK) but inside the callback the re-entrant call got status 6.
        assert_eq!(
            omni_exec(
                engine,
                source.as_ptr() as *const std::ffi::c_char,
                source.len(),
                &mut out
            ),
            OMNI_OK
        );
        assert_eq!(out.number(), Some(0.0));
        assert_eq!(
            LAST_STATUS.load(Ordering::SeqCst),
            OMNI_ERR_REENTRANT,
            "omni_exec from inside a callback returns REENTRANT"
        );
        assert_eq!(LAST_ARGC.load(Ordering::SeqCst), 0);

        omni_unregister_host_fn(engine, name.as_ptr() as *const std::ffi::c_char, name.len());
        omni_engine_free(engine);
    }
}

/// The same guard, reached from a batch item rather than a single call. This is
/// the case Phase 4 adds: `exec_many` holds the mutable borrow for the whole
/// batch, so an item's callback must not be able to start a second one.
#[test]
fn a_batch_entry_point_is_refused_from_inside_a_callback() {
    let engine = omni_engine_new();
    let name = b"try_batch_from_callback";
    unsafe {
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                0,
                Some(try_batch_from_callback),
                engine as *mut c_void,
            ),
            OMNI_OK
        );

        // A previous batch with a failing item, so there IS a message on record
        // for index 0 of an earlier batch -- the wrong one to hand a callback.
        let bad = "1 +";
        let sources = [bad.as_ptr().cast::<std::ffi::c_char>()];
        let lengths = [bad.len()];
        let mut value = OmniValue::default();
        let mut status = 0;
        assert_eq!(
            omni_exec_many(
                engine,
                sources.as_ptr(),
                lengths.as_ptr(),
                1,
                &mut value,
                &mut status
            ),
            OMNI_ERR_COMPILE
        );

        let source = "try_batch_from_callback()";
        let mut out = [OmniValue::default(); 2];
        let mut statuses = [u32::MAX; 2];
        LAST_STATUS.store(0, Ordering::SeqCst);
        BATCH_ERROR_LEN.store(u32::MAX, Ordering::SeqCst);
        // Two items, so the guard is proven to hold for the second one too: a
        // per-item borrow that was released early would let item 1 re-enter.
        let items = [source, source];
        let item_ptrs: Vec<*const std::ffi::c_char> =
            items.iter().map(|s| s.as_ptr().cast()).collect();
        let item_lens = [source.len(), source.len()];
        assert_eq!(
            omni_exec_many(
                engine,
                item_ptrs.as_ptr(),
                item_lens.as_ptr(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_OK
        );
        assert_eq!(statuses, [OMNI_OK, OMNI_OK]);
        assert_eq!(out[0].number(), Some(7.0));
        assert_eq!(out[1].number(), Some(7.0));
        assert_eq!(
            LAST_STATUS.load(Ordering::SeqCst),
            OMNI_ERR_REENTRANT,
            "omni_exec_many from inside a callback returns REENTRANT"
        );
        assert_eq!(
            BATCH_ERROR_LEN.load(Ordering::SeqCst),
            0,
            "omni_batch_error answers 0 rather than the previous batch's message"
        );

        // The batch that just ran is now the one on record, and it succeeded, so
        // index 0 has no message either way -- ask about a failing batch instead.
        assert_eq!(
            omni_exec_many(
                engine,
                sources.as_ptr(),
                lengths.as_ptr(),
                1,
                &mut value,
                &mut status
            ),
            OMNI_ERR_COMPILE
        );
        let mut buf = [0u8; 64];
        assert!(
            omni_batch_error(
                engine,
                0,
                buf.as_mut_ptr().cast::<std::ffi::c_char>(),
                buf.len()
            ) > 0,
            "the reader still works from the ordinary path"
        );

        omni_unregister_host_fn(engine, name.as_ptr() as *const std::ffi::c_char, name.len());
        omni_engine_free(engine);
    }
}

#[test]
fn string_copy_from_a_callback_copies_nothing() {
    let engine = omni_engine_new();
    let name = b"try_copy_from_callback";
    let engine_data = engine as *mut c_void;
    unsafe {
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                1,
                Some(try_copy_from_callback),
                engine_data,
            ),
            OMNI_OK
        );

        let mut out = OmniValue::default();
        let source = r#"try_copy_from_callback("hello")"#;
        assert_eq!(
            omni_exec(
                engine,
                source.as_ptr() as *const std::ffi::c_char,
                source.len(),
                &mut out
            ),
            OMNI_OK
        );
        // `omni_str_copy` is a read, and a read borrows the RefCell. Because a
        // callback runs inside a mutable borrow, the shared borrow fails and the
        // size_t return is 0: the host reads the borrowed bytes instead, and the
        // ABI gives it a defined result for the mistake rather than a fault.
        assert_eq!(
            LAST_STATUS.load(Ordering::SeqCst),
            0,
            "omni_str_copy returns 0 copied bytes"
        );

        omni_unregister_host_fn(engine, name.as_ptr() as *const std::ffi::c_char, name.len());
        omni_engine_free(engine);
    }
}

#[test]
fn host_failure_is_carried_as_data_not_unwound() {
    let engine = omni_engine_new();
    let name = b"always_fails";
    unsafe {
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                0,
                Some(always_fails),
                std::ptr::null_mut()
            ),
            OMNI_OK
        );

        let mut out = OmniValue::default();
        let source = "always_fails()";
        let code = omni_exec(
            engine,
            source.as_ptr() as *const std::ffi::c_char,
            source.len(),
            &mut out,
        );
        assert_eq!(code, OMNI_ERR_HOST, "host error is a status, not a crash");

        let mut buf = [0u8; 64];
        let n = omni_last_error(engine, buf.as_mut_ptr() as *mut std::ffi::c_char, buf.len());
        let msg = String::from_utf8_lossy(&buf[..n]);
        assert!(msg.contains("boom"), "message was {msg}");

        assert_eq!(
            omni_unregister_host_fn(engine, name.as_ptr() as *const std::ffi::c_char, name.len()),
            1
        );
        omni_engine_free(engine);
    }
}

#[test]
fn registration_validates_its_arguments_before_calling() {
    let engine = omni_engine_new();
    unsafe {
        // A null engine.
        assert_eq!(
            omni_register_host_fn(
                std::ptr::null_mut(),
                b"s".as_ptr() as *const std::ffi::c_char,
                1,
                1,
                Some(square),
                std::ptr::null_mut()
            ),
            OMNI_ERR_ARG
        );
        // A null callback.
        let name = b"s";
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                1,
                None,
                std::ptr::null_mut()
            ),
            OMNI_ERR_ARG
        );
        // Arity over the ABI's limit.
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr() as *const std::ffi::c_char,
                name.len(),
                (MAX_ARGS + 1) as u32,
                Some(square),
                std::ptr::null_mut()
            ),
            OMNI_ERR_ARG
        );
        // A malformed UTF-8 name.
        assert_eq!(
            omni_register_host_fn(
                engine,
                [0xFF, 0xFE].as_ptr() as *const std::ffi::c_char,
                2,
                1,
                Some(square),
                std::ptr::null_mut()
            ),
            OMNI_ERR_ARG
        );

        omni_engine_free(engine);
    }
}

#[test]
fn null_engine_unregister_reports_zero() {
    unsafe {
        assert_eq!(
            omni_unregister_host_fn(
                std::ptr::null_mut(),
                b"s".as_ptr() as *const std::ffi::c_char,
                1
            ),
            0
        );
        assert_eq!(omni_host_fn_count(std::ptr::null()), 0);
    }
}
