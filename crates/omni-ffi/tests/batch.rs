//! Contract tests for the batch surface, written as an external consumer would:
//! two parallel input arrays the caller owns, two output arrays the caller owns,
//! and no Rust-side conveniences.
//!
//! Split from `ffi_contract.rs` because the batch has its own rule to stand
//! behind — header rule 11 — and the interesting cases are all about what one
//! crossing does that N crossings cannot: N results live at once, N statuses
//! written even when an early item fails, and one egress rewind shared between
//! them.

use std::ffi::{CStr, c_char};

use omni::*;

fn engine() -> *mut OmniEngine {
    let handle = omni_engine_new();
    assert!(!handle.is_null(), "omni_engine_new returned null");
    handle
}

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

fn copy_out(engine: *const OmniEngine, value: &OmniValue) -> Vec<u8> {
    assert_eq!(value.tag, OMNI_STRING, "not a string: {value:?}");
    let mut buf = vec![0u8; value.len as usize + 1];
    let n = unsafe { omni_str_copy(engine, value, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    assert_eq!(n as u32, value.len, "copied {n} of {}", value.len);
    buf.truncate(n);
    buf
}

/// Copy a value the engine no longer stands behind. The length is whatever the
/// engine's own header now says, not the `len` the stale value carries, so this
/// deliberately does not assert that the two agree — that is the property.
fn stale_copy(engine: *const OmniEngine, value: &OmniValue) -> Vec<u8> {
    let mut buf = vec![0u8; value.len as usize + 1];
    let n = unsafe { omni_str_copy(engine, value, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    buf.truncate(n);
    buf
}

/// A value with every field set to something a successful write would replace,
/// so "written nothing" is distinguishable from "written zero".
fn poison() -> OmniValue {
    OmniValue {
        tag: u32::MAX,
        len: u32::MAX,
        payload: OmniPayload { number: 12345.0 },
    }
}

/// Run a batch the way a C caller does. The pointers borrow the caller's `&str`s,
/// which is the whole lifetime contract: nothing is copied into the engine.
fn batch(engine: *mut OmniEngine, sources: &[&str]) -> (u32, Vec<OmniValue>, Vec<u32>) {
    let ptrs: Vec<*const c_char> = sources
        .iter()
        .map(|s| s.as_ptr().cast::<c_char>())
        .collect();
    let lens: Vec<usize> = sources.iter().map(|s| s.len()).collect();
    let mut out = vec![poison(); sources.len().max(1)];
    let mut statuses = vec![u32::MAX; sources.len().max(1)];
    let code = unsafe {
        omni_exec_many(
            engine,
            ptrs.as_ptr(),
            lens.as_ptr(),
            sources.len(),
            out.as_mut_ptr(),
            statuses.as_mut_ptr(),
        )
    };
    (code, out, statuses)
}

fn batch_error(engine: *const OmniEngine, index: usize) -> String {
    let mut buf = vec![0u8; 256];
    let n =
        unsafe { omni_batch_error(engine, index, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    assert!(n < buf.len(), "the terminator must fit");
    assert_eq!(buf[n], 0, "always NUL-terminated");
    String::from_utf8(buf[..n].to_vec()).expect("error text is UTF-8")
}

#[test]
fn a_batch_writes_one_value_and_one_status_per_item() {
    let engine = engine();
    unsafe {
        let (code, out, statuses) = batch(engine, &["6 * 7", "\"text\"", "1 == 1", ""]);
        assert_eq!(code, OMNI_OK);
        assert_eq!(statuses, vec![OMNI_OK; 4]);
        assert_eq!(out[0].number(), Some(42.0));
        assert_eq!(out[1].tag, OMNI_STRING);
        assert_eq!(copy_out(engine, &out[1]), b"text");
        assert_eq!(out[2].tag, OMNI_BOOL);
        assert_eq!(out[3].tag, OMNI_UNDEF, "an empty program is undefined");
        omni_engine_free(engine);
    }
}

#[test]
fn a_batch_returns_the_first_failing_status_and_still_runs_every_item() {
    let engine = engine();
    unsafe {
        let (code, out, statuses) = batch(engine, &["1 +", "2 + 3", "1 / 0", "20 / 4"]);
        assert_eq!(code, OMNI_ERR_COMPILE, "the FIRST failure, not the last");
        assert_eq!(
            statuses,
            vec![OMNI_ERR_COMPILE, OMNI_OK, OMNI_ERR_RUNTIME, OMNI_OK]
        );
        assert_eq!(out[0].tag, OMNI_UNDEF, "a failed item carries no value");
        assert_eq!(out[2].tag, OMNI_UNDEF);
        assert_eq!(out[1].number(), Some(5.0));
        assert_eq!(out[3].number(), Some(5.0), "the item after a failure ran");
        omni_engine_free(engine);
    }
}

/// The capability a loop of single calls cannot offer. Each `omni_exec` rewinds
/// egress, so only the last string would survive; one batch rewinds once, so all
/// of them do.
#[test]
fn every_string_result_of_a_batch_is_readable_at_once() {
    let engine = engine();
    unsafe {
        let (code, out, _) = batch(engine, &[r#""a" + "1""#, r#""b" + "2""#, r#""c" + "3""#]);
        assert_eq!(code, OMNI_OK);
        // Read in reverse, so a per-item rewind would be caught rather than
        // accidentally agreeing with the order the items were written in.
        assert_eq!(copy_out(engine, &out[2]), b"c3");
        assert_eq!(copy_out(engine, &out[1]), b"b2");
        assert_eq!(copy_out(engine, &out[0]), b"a1");
        omni_engine_free(engine);
    }
}

/// The other half of the same trade: the batch's results expire together, on the
/// next execution of either kind, and a stale read is still a wrong read rather
/// than a fault.
#[test]
fn a_batch_result_expires_on_the_next_execution_of_either_kind() {
    let engine = engine();
    unsafe {
        let (_, out, _) = batch(engine, &[r#""first batch result""#]);
        let held = out[0];
        assert_eq!(copy_out(engine, &held), b"first batch result");
        call(engine, r#""a single exec""#);
        assert_ne!(stale_copy(engine, &held), b"first batch result");

        let (_, out, _) = batch(engine, &[r#""second batch result""#]);
        let held = out[0];
        batch(engine, &[r#""third""#]);
        assert_ne!(stale_copy(engine, &held), b"second batch result");
        omni_engine_free(engine);
    }
}

#[test]
fn a_batch_item_cannot_read_the_previous_items_binding() {
    let engine = engine();
    unsafe {
        let (code, _, statuses) = batch(engine, &["let secret = 1; secret", "secret"]);
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert_eq!(statuses, vec![OMNI_OK, OMNI_ERR_RUNTIME]);
        assert!(batch_error(engine, 1).contains("secret"));
        omni_engine_free(engine);
    }
}

#[test]
fn a_host_global_survives_every_item_of_a_batch() {
    let engine = engine();
    unsafe {
        let name = "answer";
        let payload = "42";
        assert_eq!(
            omni_define_string(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                payload.as_ptr().cast::<c_char>(),
                payload.len(),
            ),
            OMNI_OK
        );
        let (code, out, _) = batch(engine, &["answer", r#""value: " + answer"#]);
        assert_eq!(code, OMNI_OK);
        assert_eq!(copy_out(engine, &out[0]), b"42");
        assert_eq!(copy_out(engine, &out[1]), b"value: 42");
        omni_engine_free(engine);
    }
}

#[test]
fn batch_error_names_the_item_and_a_single_exec_ends_its_currency() {
    let engine = engine();
    unsafe {
        batch(engine, &["1 + 1", "let = 2", "1 / 0"]);
        assert_eq!(batch_error(engine, 0), "", "a success has no message");
        assert!(
            batch_error(engine, 1).starts_with("line 1:"),
            "rendered, not raw"
        );
        assert!(batch_error(engine, 2).contains("division by zero"));
        assert_eq!(batch_error(engine, 3), "", "past the batch");
        assert_eq!(batch_error(engine, usize::MAX), "");

        // The engine's own last error is the final item's, which is exactly why
        // the per-item reader exists.
        let mut buf = [0 as c_char; 128];
        let n = omni_last_error(engine, buf.as_mut_ptr(), buf.len());
        let last = CStr::from_ptr(buf.as_ptr()).to_str().unwrap()[..n].to_string();
        assert_eq!(last, batch_error(engine, 2));

        call(engine, "1");
        assert_eq!(batch_error(engine, 2), "", "a single exec ends the batch");
        omni_engine_free(engine);
    }
}

#[test]
fn batch_error_truncates_rather_than_overflowing() {
    let engine = engine();
    unsafe {
        batch(engine, &["1 / 0"]);
        let mut buf = [0xAAu8.cast_signed() as c_char; 8];
        let n = omni_batch_error(engine, 0, buf.as_mut_ptr(), buf.len());
        assert_eq!(n, 7, "cap - 1 bytes, so a full return means truncated");
        assert_eq!(buf[7], 0, "and the terminator still fits");
        assert_eq!(omni_batch_error(engine, 0, buf.as_mut_ptr(), 0), 0);
        assert_eq!(omni_batch_error(engine, 0, std::ptr::null_mut(), 8), 0);
        assert_eq!(
            omni_batch_error(std::ptr::null(), 0, buf.as_mut_ptr(), 8),
            0
        );
        omni_engine_free(engine);
    }
}

#[test]
fn an_empty_batch_writes_nothing_and_expires_nothing() {
    let engine = engine();
    unsafe {
        let (code, held) = call(engine, "\"still current\"");
        assert_eq!(code, OMNI_OK);
        let (empty, out, statuses) = batch(engine, &[]);
        assert_eq!(empty, OMNI_OK);
        assert_eq!(statuses, vec![u32::MAX], "the poison survived");
        assert_eq!(out[0], poison());
        assert_eq!(copy_out(engine, &held), b"still current");
        // Null source arrays are legal when the count is zero. The output arrays
        // are not: they are required unconditionally, exactly as `omni_exec`
        // requires `out` even for an empty program.
        let mut spare = poison();
        let mut spare_status = u32::MAX;
        assert_eq!(
            omni_exec_many(
                engine,
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut spare,
                &mut spare_status
            ),
            OMNI_OK
        );
        assert_eq!(spare, poison());
        assert_eq!(spare_status, u32::MAX);
        assert_eq!(copy_out(engine, &held), b"still current");
        omni_engine_free(engine);
    }
}

#[test]
fn argument_errors_refuse_the_whole_batch_without_writing() {
    let engine = engine();
    unsafe {
        let sources = ["1 + 1", "2 + 2"];
        let ptrs: Vec<*const c_char> = sources.iter().map(|s| s.as_ptr().cast()).collect();
        let lens = [5usize, 5];
        let mut out = [poison(); 2];
        let mut statuses = [u32::MAX; 2];
        let untouched = out;

        let refusals: Vec<(
            *const *const c_char,
            *const usize,
            usize,
            *mut OmniValue,
            *mut u32,
        )> = vec![
            (
                ptrs.as_ptr(),
                lens.as_ptr(),
                MAX_BATCH + 1,
                out.as_mut_ptr(),
                statuses.as_mut_ptr(),
            ),
            (
                std::ptr::null(),
                lens.as_ptr(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr(),
            ),
            (
                ptrs.as_ptr(),
                std::ptr::null(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr(),
            ),
            (
                ptrs.as_ptr(),
                lens.as_ptr(),
                2,
                std::ptr::null_mut(),
                statuses.as_mut_ptr(),
            ),
            (
                ptrs.as_ptr(),
                lens.as_ptr(),
                2,
                out.as_mut_ptr(),
                std::ptr::null_mut(),
            ),
        ];
        for (i, (s, l, n, o, st)) in refusals.iter().enumerate() {
            assert_eq!(
                omni_exec_many(engine, *s, *l, *n, *o, *st),
                OMNI_ERR_ARG,
                "refusal {i}"
            );
        }
        assert_eq!(
            omni_exec_many(
                std::ptr::null_mut(),
                ptrs.as_ptr(),
                lens.as_ptr(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_ERR_ARG
        );
        assert_eq!(out, untouched, "no refusal wrote a value");
        assert_eq!(statuses, [u32::MAX; 2], "and none wrote a status");

        // An over-long length, and a null pointer carrying a nonzero one.
        let too_long = [MAX_SOURCE_LEN + 1, 5];
        assert_eq!(
            omni_exec_many(
                engine,
                ptrs.as_ptr(),
                too_long.as_ptr(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_ERR_ARG
        );
        let with_null = [std::ptr::null(), ptrs[1]];
        assert_eq!(
            omni_exec_many(
                engine,
                with_null.as_ptr(),
                lens.as_ptr(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_ERR_ARG
        );
        assert_eq!(statuses, [u32::MAX; 2]);

        // A null pointer with a zero length is the empty program, and is legal.
        let empty_lens = [0usize, 5];
        assert_eq!(
            omni_exec_many(
                engine,
                with_null.as_ptr(),
                empty_lens.as_ptr(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_OK
        );
        assert_eq!(out[0].tag, OMNI_UNDEF);
        assert_eq!(out[1].number(), Some(4.0));
        assert_eq!(statuses, [OMNI_OK, OMNI_OK]);

        // Malformed UTF-8 in the second item refuses the call before the first
        // item runs, so `out` still holds what the last legal batch wrote.
        let before = out;
        let bad = b"\xff\xfe";
        let bad_ptrs = [ptrs[0], bad.as_ptr().cast::<c_char>()];
        let bad_lens = [5usize, 2];
        assert_eq!(
            omni_exec_many(
                engine,
                bad_ptrs.as_ptr(),
                bad_lens.as_ptr(),
                2,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_ERR_ARG
        );
        assert_eq!(out, before, "a refused batch writes nothing at all");
        assert_eq!(statuses, [OMNI_OK, OMNI_OK]);
        omni_engine_free(engine);
    }
}

#[test]
fn execs_counts_items_not_crossings() {
    let engine = engine();
    unsafe {
        let mut stats = OmniStats::default();
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        let before = stats.execs;
        batch(engine, &["1", "2", "1 +"]);
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        assert_eq!(
            stats.execs,
            before + 3,
            "three items, one crossing, failures included"
        );
        omni_engine_free(engine);
    }
}

/// Bounded reuse at batch scale. One batch maps what one batch needs; ten
/// thousand identical batches must not map one byte more.
#[test]
fn repeated_batches_reserve_a_flat_amount_of_egress() {
    let engine = engine();
    unsafe {
        let sources = [r#""a" + "b" + "c""#; 8];
        batch(engine, &sources);
        let mut stats = OmniStats::default();
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        let reserved = stats.egress_bytes_reserved;
        let high_water = stats.egress_high_water;
        assert!(reserved > 0, "eight results are live at once");
        for _ in 0..10_000 {
            let (code, _, statuses) = batch(engine, &sources);
            assert_eq!(code, OMNI_OK);
            assert_eq!(statuses, vec![OMNI_OK; 8]);
        }
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        assert_eq!(
            stats.egress_bytes_reserved, reserved,
            "the block was reused"
        );
        assert_eq!(stats.egress_high_water, high_water);
        assert_eq!(stats.execs, 10_001 * 8);
        omni_engine_free(engine);
    }
}

/// A batch larger than one egress chunk takes the oversize path, and the spare
/// reuse that makes a rewinding block bounded has to work there too.
#[test]
fn a_batch_of_long_strings_stays_bounded() {
    let engine = engine();
    unsafe {
        // 40 KiB per item, eight items: past the 64 KiB chunk, so the batch needs
        // an oversize block that a single item would not.
        let long = format!("\"{}\"", "x".repeat(40 * 1024));
        let sources = [long.as_str(); 8];
        let (code, out, _) = batch(engine, &sources);
        assert_eq!(code, OMNI_OK);
        assert_eq!(out[7].len, 40 * 1024);
        let mut stats = OmniStats::default();
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        let reserved = stats.egress_bytes_reserved;
        assert!(
            reserved >= 8 * 40 * 1024,
            "one batch's demand, not one item's"
        );
        for _ in 0..64 {
            assert_eq!(batch(engine, &sources).0, OMNI_OK);
        }
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        assert_eq!(
            stats.egress_bytes_reserved, reserved,
            "the spare was reused"
        );
        omni_engine_free(engine);
    }
}
