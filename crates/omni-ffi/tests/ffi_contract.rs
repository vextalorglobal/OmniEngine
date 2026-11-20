//! Contract tests for the C ABI, written as an external consumer would: raw
//! pointers, byte buffers, and no Rust-side conveniences. These are the tests
//! that stand behind "Python and JavaScript can call OmniEngine safely" — and
//! because they go through the public surface only, a rule they check is a rule a
//! host can actually rely on.
//!
//! The Phase 2 rules under test are the shape of `OmniValue`, that a string
//! crosses by copy out of storage the engine owns, and that an address this
//! engine cannot account for yields zero bytes rather than a fault.

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
        assert_eq!(value.number(), Some(42.0));
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
        assert_eq!(value.number(), Some(2.0));
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
        assert!(
            text.contains("[Line 1, Col 3]"),
            "text lost its location: {text}"
        );

        // A four-byte buffer holds three bytes plus a NUL rather than writing
        // past the caller's allocation.
        let mut tiny = [0 as c_char; 4];
        assert_eq!(omni_last_error(engine, tiny.as_mut_ptr(), tiny.len()), 3);
        assert_eq!(tiny[3], 0);
        assert_eq!(CStr::from_ptr(tiny.as_ptr()).to_str().unwrap(), "[Li");

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

// ---------------------------------------------------------------------------
// ABI v2: strings across the boundary.
// ---------------------------------------------------------------------------

/// Read a string result the way a C host does: allocate, copy, and own the bytes.
fn copy_out(engine: *const OmniEngine, value: &OmniValue) -> Vec<u8> {
    assert_eq!(value.tag, OMNI_STRING, "not a string: {value:?}");
    let mut buf = vec![0u8; value.len as usize + 1];
    let n = unsafe { omni_str_copy(engine, value, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    assert_eq!(n as u32, value.len, "copied {n} of {}", value.len);
    buf.truncate(n);
    buf
}

#[test]
fn the_abi_reports_six_and_the_header_agrees() {
    assert_eq!(omni_abi_version(), 6);
    assert_eq!(omni_abi_version(), OMNI_ABI_VERSION);
}

#[test]
fn a_string_result_crosses_the_boundary_as_bytes() {
    let engine = engine();
    unsafe {
        let (code, value) = call(engine, r#"let who = "world"; "hello, " + who"#);
        assert_eq!(code, OMNI_OK);
        assert_eq!(value.tag, OMNI_STRING);
        assert_eq!(value.len, 12, "the concatenation, in bytes");
        assert_eq!(copy_out(engine, &value), b"hello, world");
        // A number must not answer to the string reader, even with a plausible
        // length left in the struct.
        let (_, n) = call(engine, "1 + 1");
        let mut buf = [0u8; 8];
        assert_eq!(
            omni_str_copy(engine, &n, buf.as_mut_ptr().cast::<c_char>(), 8),
            0
        );
        assert_eq!(buf, [0u8; 8]);
        omni_engine_free(engine);
    }
}

#[test]
fn escapes_survive_the_boundary_byte_for_byte() {
    let engine = engine();
    unsafe {
        let (code, value) = call(engine, r#""tab\there\nnewline\"\u{1F600}""#);
        assert_eq!(code, OMNI_OK);
        // The Rust side of this assertion is a *string* literal, so its `\t`,
        // `\n` and `\u{1F600}` are the decoded forms — which is exactly what the
        // engine must have produced for the bytes to match.
        assert_eq!(
            copy_out(engine, &value),
            "tab\there\nnewline\"\u{1F600}".as_bytes()
        );
        // Interior NUL is payload, not a terminator: `omni_str_copy` never writes
        // one and never stops at one, so `len` is the only end-of-string there is.
        let (code, value) = call(engine, r#""a\u{0}b""#);
        assert_eq!(code, OMNI_OK);
        assert_eq!(value.len, 3, "NUL counted, not truncated");
        assert_eq!(copy_out(engine, &value), b"a\0b");
        omni_engine_free(engine);
    }
}

/// The documented lifetime, tested from outside: a result stays readable for the
/// rest of this execution and, because egress rewinds instead of freeing, stays
/// *readable* after the next one too — it simply holds whatever was written there
/// since. That is the "wrong read, never a crash" property, asserted literally.
#[test]
fn a_string_survives_the_boundary_and_a_stale_one_yields_bytes_not_a_fault() {
    let engine = engine();
    unsafe {
        let (_, first) = call(engine, r#""first result""#);
        assert_eq!(copy_out(engine, &first), b"first result");
        let _ = call(engine, r#""a much longer second result""#);
        // Same handle, same address, next execution. `first.len` says 12; the
        // engine's own header says 27, and the copy is bounded by the caller's
        // `cap`, so a stale length can under-read but never overflow.
        let mut stale = vec![0u8; 16];
        let n = omni_str_copy(
            engine,
            &first,
            stale.as_mut_ptr().cast::<c_char>(),
            stale.len(),
        );
        assert_eq!(n, 16, "reads the live header rather than the stale length");
        assert_eq!(&stale[..], b"a much longer se".as_slice());
        omni_engine_free(engine);
    }
}

#[test]
fn a_foreign_or_fabricated_address_copies_zero_bytes() {
    let a = engine();
    let b = engine();
    unsafe {
        let (_, from_b) = call(b, r#""only in engine b""#);
        let mut buf = [0u8; 32];
        assert_eq!(
            omni_str_copy(a, &from_b, buf.as_mut_ptr().cast::<c_char>(), 32),
            0,
            "a pointer is not a proof of ownership"
        );
        assert_eq!(buf, [0u8; 32], "and nothing was written on the way out");
        assert_eq!(copy_out(b, &from_b), b"only in engine b");
        omni_engine_free(a);
        omni_engine_free(b);
    }
}

#[test]
fn a_host_string_can_be_read_by_a_program_and_redefined_without_growing() {
    let engine = engine();
    let name = b"lang";
    unsafe {
        for value in ["বাংলা", "Bengali"] {
            let bytes = value.as_bytes();
            assert_eq!(
                omni_define_string(
                    engine,
                    name.as_ptr().cast::<c_char>(),
                    name.len(),
                    bytes.as_ptr().cast::<c_char>(),
                    bytes.len(),
                ),
                OMNI_OK
            );
        }
        let (code, value) = call(engine, "lang + \"!\"");
        assert_eq!(code, OMNI_OK);
        assert_eq!(copy_out(engine, &value), b"Bengali!");
        omni_engine_free(engine);
    }
}

/// The leak claim, measured from outside Rust: ten thousand string executions
/// must not grow the reserved memory, while the string counter keeps counting.
#[test]
fn ten_thousand_string_executions_reserve_no_more_memory() {
    let engine = engine();
    unsafe {
        let source = r#"let a = "hello"; let b = "world"; a + ", " + b + "!" == "hello, world!""#;
        for _ in 0..200 {
            let _ = call(engine, source);
        }
        let mut warm = OmniStats::default();
        assert_eq!(omni_debug_stats(engine, &mut warm), OMNI_OK);
        for _ in 0..10_000 {
            let (code, value) = call(engine, source);
            assert_eq!(code, OMNI_OK);
            assert_eq!(value.tag, OMNI_BOOL);
        }
        let mut after = OmniStats::default();
        assert_eq!(omni_debug_stats(engine, &mut after), OMNI_OK);
        assert_eq!(after.execs, warm.execs + 10_000);
        assert_eq!(after.arena_chunks, warm.arena_chunks);
        assert_eq!(after.egress_bytes_reserved, warm.egress_bytes_reserved);
        assert_eq!(after.arena_bytes_reserved, warm.arena_bytes_reserved);
        assert!(
            after.strings > warm.strings,
            "the one counter that is supposed to grow"
        );
        omni_engine_free(engine);
    }
}

#[test]
fn a_larger_result_reuses_the_same_egress_reservation() {
    let engine = engine();
    unsafe {
        // Over a chunk in size, so it takes the oversize path: the block a
        // rewinding arena has to hand back rather than accumulate.
        let payload = "x".repeat(100_000);
        let source = format!("\"{payload}\"");
        let mut first_reserved = 0u64;
        for round in 0..50 {
            let (code, value) = call(engine, &source);
            assert_eq!(code, OMNI_OK);
            let bytes = copy_out(engine, &value);
            assert_eq!(bytes.len(), 100_000, "a 100 KiB string crosses intact");
            let mut stats = OmniStats::default();
            assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
            assert!(
                stats.egress_high_water as usize >= 100_000,
                "the high water marks the largest single execution"
            );
            if round == 0 {
                first_reserved = stats.egress_bytes_reserved;
            } else {
                assert_eq!(
                    stats.egress_bytes_reserved, first_reserved,
                    "round {round} reserved more than round 0: the block was reallocated"
                );
            }
        }
        omni_engine_free(engine);
    }
}

#[test]
fn a_program_too_long_string_is_a_runtime_error_not_an_argument_error() {
    let engine = engine();
    unsafe {
        // 17 MiB of literal exceeds MAX_STRING_LEN, which is the program's fault.
        let payload = "y".repeat(MAX_STRING_LEN + 1);
        let source = format!("\"{payload}\"");
        let (code, _) = call(engine, &source);
        assert_eq!(code, OMNI_ERR_RUNTIME);
        let mut buf = [0 as c_char; 128];
        let n = omni_last_error(engine, buf.as_mut_ptr(), buf.len());
        let text = CStr::from_ptr(buf.as_ptr()).to_str().unwrap();
        assert!(n > 0 && text.contains("MiB"), "text was {text}");
        // The same bytes handed to `omni_define_string` are the host's fault.
        let name = b"s";
        let code = omni_define_string(
            engine,
            name.as_ptr().cast::<c_char>(),
            name.len(),
            payload.as_ptr().cast::<c_char>(),
            payload.len(),
        );
        assert_eq!(code, OMNI_ERR_ARG);
        omni_engine_free(engine);
    }
}

#[test]
fn stats_refuse_a_null_target_and_state_their_own_size() {
    let engine = engine();
    unsafe {
        assert_eq!(omni_debug_stats(engine, std::ptr::null_mut()), OMNI_ERR_ARG);
        assert_eq!(
            omni_debug_stats(
                std::ptr::null_mut::<OmniEngine>(),
                &mut OmniStats::default()
            ),
            OMNI_ERR_ARG
        );
        let mut stats = OmniStats {
            size: 7,
            ..OmniStats::default()
        };
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        assert_eq!(stats.size, std::mem::size_of::<OmniStats>() as u32);
        omni_engine_free(engine);
    }
}
