//! Contract tests for the C ABI, written as an external consumer would: raw
//! pointers, byte buffers, and no Rust-side conveniences. These are the tests
//! that stand behind "Python and JavaScript can call OmniEngine safely".

use std::ffi::{CStr, c_char};

use omni::*;

fn engine() -> *mut OmniEngine {
    let handle = omni_engine_new();
    assert!(!handle.is_null(), "omni_engine_new returned null");
    handle
}

/// Evaluate and hand back the raw status plus value, mirroring a C caller.
fn call(engine: *mut OmniEngine, source: &str) -> (u32, OmniValue) {
    let mut out = OmniValue::default();
    let code = unsafe {
        omni_exec(
            engine,
            source.as_ptr().cast::<c_char>(),
            source.len(),
            &mut out,
        )
    };
    (code, out)
}

#[test]
fn evaluates_42_through_the_c_surface() {
    let engine = engine();
    unsafe {
        let (code, value) = call(engine, "let answer = 40 + 2");
        assert_eq!(code, OMNI_OK);
        assert_eq!(value.tag, OMNI_NUMBER);
        assert_eq!(value.number, 42.0);
        omni_engine_free(engine);
    }
}

#[test]
fn eval_number_rejects_a_non_numeric_result() {
    let engine = engine();
    unsafe {
        let mut n = 0.0;
        let src = b"3.5";
        assert_eq!(
            omni_exec_number(engine, src.as_ptr().cast::<c_char>(), src.len(), &mut n),
            OMNI_OK
        );
        assert_eq!(n, 3.5);

        // An unbound name is a runtime failure: the source parses fine, so the
        // host must be able to tell "your program is wrong at run time" from
        // "your program does not compile".
        let bad = b"nope";
        let code = omni_exec_number(engine, bad.as_ptr().cast::<c_char>(), bad.len(), &mut n);
        assert_eq!(
            code, OMNI_ERR_RUNTIME,
            "an unbound name is a runtime failure"
        );

        // A syntax failure passes the compile status straight through, and the
        // caller's `out` is left untouched rather than written with a partial.
        let mut out = 99.0_f64;
        let broken = b"let = 1";
        assert_eq!(
            omni_exec_number(
                engine,
                broken.as_ptr().cast::<c_char>(),
                broken.len(),
                &mut out
            ),
            OMNI_ERR_COMPILE,
            "a parse error is a compile failure"
        );
        assert_eq!(out, 99.0, "a failed call must not write the output");
        omni_engine_free(engine);
    }
}

/// The single most important test in the repository: a Rust panic that unwound
/// past `extern "C"` into a host is undefined behaviour.
#[test]
fn a_panicking_engine_reports_instead_of_unwinding_into_the_host() {
    let engine = engine();
    unsafe {
        assert_eq!(
            omni_debug_panic(engine),
            OMNI_ERR_PANIC,
            "panic must be caught at the boundary"
        );
        // And the engine is still usable: catching left it in a defined state.
        let (code, value) = call(engine, "1 + 1");
        assert_eq!(code, OMNI_OK);
        assert_eq!(value.number, 2.0);
        omni_engine_free(engine);
    }
}

#[test]
fn an_empty_program_is_valid_even_when_the_host_sends_a_null_pointer() {
    // Node hands an empty Buffer over as null, and Python hands b"" over as a
    // real pointer. Both must get the same answer: undef, not an argument error.
    let engine = engine();
    let mut out = OmniValue::default();
    unsafe {
        assert_eq!(
            omni_exec(engine, std::ptr::null::<c_char>(), 0, &mut out),
            OMNI_OK
        );
        assert_eq!(out.tag, OMNI_UNDEF, "an empty program has no value");

        // A non-null zero-length source behaves identically.
        let empty = b"";
        assert_eq!(
            omni_exec(engine, empty.as_ptr().cast::<c_char>(), 0, &mut out),
            OMNI_OK
        );
        assert_eq!(out.tag, OMNI_UNDEF);

        // And requiring a number from nothing is a runtime failure, not an
        // argument error, because the call itself was legal.
        let mut n = 0.0;
        assert_eq!(
            omni_exec_number(engine, std::ptr::null::<c_char>(), 0, &mut n),
            OMNI_ERR_RUNTIME
        );

        omni_engine_free(engine);
    }
}

#[test]
fn null_and_bad_arguments_are_rejected_not_dereferenced() {
    let mut out = OmniValue::default();
    let src = b"1";
    unsafe {
        assert_eq!(
            omni_exec(
                std::ptr::null_mut::<OmniEngine>(),
                src.as_ptr().cast::<c_char>(),
                1,
                &mut out
            ),
            OMNI_ERR_ARG
        );
        let engine = engine();
        assert_eq!(
            omni_exec(engine, std::ptr::null::<c_char>(), 1, &mut out),
            OMNI_ERR_ARG
        );
        assert_eq!(
            omni_exec(
                engine,
                src.as_ptr().cast::<c_char>(),
                1,
                std::ptr::null_mut::<OmniValue>()
            ),
            OMNI_ERR_ARG
        );
        // A hostile length is bounded before any slice is fabricated.
        assert_eq!(
            omni_exec(
                engine,
                src.as_ptr().cast::<c_char>(),
                usize::MAX / 2,
                &mut out
            ),
            OMNI_ERR_ARG
        );
        // Freeing null is a no-op, not a crash.
        omni_engine_free(std::ptr::null_mut::<OmniEngine>());
        omni_engine_free(engine);
    }
}

#[test]
fn malformed_utf8_is_an_argument_error() {
    let engine = engine();
    unsafe {
        let bytes = [0xFFu8, 0xFE, 0x01];
        let mut out = OmniValue::default();
        assert_eq!(
            omni_exec(
                engine,
                bytes.as_ptr().cast::<c_char>(),
                bytes.len(),
                &mut out
            ),
            OMNI_ERR_ARG
        );
        omni_engine_free(engine);
    }
}

#[test]
fn error_text_is_copied_into_caller_storage_and_truncates_safely() {
    let engine = engine();
    unsafe {
        let (code, _) = call(engine, "1 / 0");
        assert_eq!(code, OMNI_ERR_RUNTIME);

        let mut buf = [0 as c_char; 64];
        let n = omni_last_error(engine, buf.as_mut_ptr(), buf.len());
        assert!(n > 0 && n < buf.len(), "wrote {n} bytes");
        let text = CStr::from_ptr(buf.as_ptr()).to_str().unwrap();
        assert!(text.contains("division by zero"), "text was {text}");
        // The location has to survive the boundary, or a host cannot tell the
        // user which line broke.
        assert!(text.contains("line 1"), "text lost its line number: {text}");

        // A four-byte buffer holds three bytes plus a NUL rather than writing
        // past the caller's allocation.
        let mut tiny = [0 as c_char; 4];
        assert_eq!(omni_last_error(engine, tiny.as_mut_ptr(), tiny.len()), 3);
        assert_eq!(tiny[3], 0);
        assert_eq!(CStr::from_ptr(tiny.as_ptr()).to_str().unwrap(), "lin");

        omni_engine_free(engine);
    }
}

#[test]
fn a_successful_exec_clears_the_stale_error() {
    let engine = engine();
    unsafe {
        let _ = call(engine, "1 / 0");
        let _ = call(engine, "1");
        let mut buf = [0 as c_char; 32];
        assert_eq!(omni_last_error(engine, buf.as_mut_ptr(), buf.len()), 0);
        assert_eq!(buf[0], 0);
        omni_engine_free(engine);
    }
}

#[test]
fn version_is_static_and_nul_terminated() {
    unsafe {
        let text = CStr::from_ptr(omni_version())
            .to_str()
            .expect("utf-8 version");
        assert_eq!(text, env!("CARGO_PKG_VERSION"));
    }
}

#[test]
fn two_engines_do_not_share_state() {
    let a = engine();
    let b = engine();
    unsafe {
        assert_eq!(call(a, "let secret = 1").0, OMNI_OK);
        assert_eq!(
            call(b, "secret").0,
            OMNI_ERR_RUNTIME,
            "bindings must not leak across handles"
        );
        omni_engine_free(a);
        omni_engine_free(b);
    }
}
