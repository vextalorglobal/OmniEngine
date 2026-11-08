//! Contract tests for `omni_exec_parallel`, written as an external consumer
//! would: raw pointers, caller-owned output arrays, and no Rust-side conveniences.
//!
//! The parallel entry point is additive in ABI 4. It answers the same contract
//! as `omni_exec_many` — ordered results, one status per item, one shared egress
//! rewind — and adds only the question the sequential batch cannot ask: does
//! spreading the work over several engines change any answer? The tests here that
//! matter are therefore the ones comparing the two paths, and the ones proving a
//! failing item does not stop the item next to it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use omni::*;

fn engine() -> *mut OmniEngine {
    let handle = omni_engine_new();
    assert!(!handle.is_null(), "omni_engine_new returned null");
    handle
}

fn copy_out(engine: *const OmniEngine, value: &OmniValue) -> Vec<u8> {
    assert_eq!(value.tag, OMNI_STRING, "not a string: {value:?}");
    let mut buf = vec![0u8; value.len as usize + 1];
    let n = unsafe { omni_str_copy(engine, value, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(n as u32, value.len, "copied {n} of {}", value.len);
    buf.truncate(n);
    buf
}

fn call(engine: *mut OmniEngine, source: &str) -> (u32, OmniValue) {
    let mut out = OmniValue::default();
    let code = unsafe { omni_exec(engine, source.as_ptr().cast(), source.len(), &mut out) };
    (code, out)
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

/// Run a parallel batch the way a C caller does. The pointers borrow the caller's
/// `&str`s for the duration of the call; nothing is copied into the engine.
fn batch(
    engine: *mut OmniEngine,
    sources: &[&str],
    max_threads: u32,
) -> (u32, Vec<OmniValue>, Vec<u32>) {
    let ptrs: Vec<*const std::ffi::c_char> = sources.iter().map(|s| s.as_ptr().cast()).collect();
    let lens: Vec<usize> = sources.iter().map(|s| s.len()).collect();
    let mut out = vec![poison(); sources.len().max(1)];
    let mut statuses = vec![u32::MAX; sources.len().max(1)];
    let code = unsafe {
        omni_exec_parallel(
            engine,
            ptrs.as_ptr(),
            lens.as_ptr(),
            sources.len(),
            max_threads,
            out.as_mut_ptr(),
            statuses.as_mut_ptr(),
        )
    };
    (code, out, statuses)
}

fn batch_error(engine: *const OmniEngine, index: usize) -> String {
    let mut buf = vec![0u8; 256];
    let n = unsafe { omni_batch_error(engine, index, buf.as_mut_ptr().cast(), buf.len()) };
    assert!(n < buf.len(), "the terminator must fit");
    buf.truncate(n);
    String::from_utf8(buf).unwrap()
}

#[test]
fn parallel_results_come_back_in_source_order() {
    let engine = engine();
    unsafe {
        let (code, out, statuses) = batch(
            engine,
            &[
                "1 + 1",
                "\"alpha\"",
                "2 * 21",
                "20 / 4",
                "len(\"abcd\")",
                "7 - 5",
            ],
            8,
        );
        assert_eq!(code, OMNI_OK);
        assert_eq!(statuses, vec![OMNI_OK; 6]);
        assert_eq!(out[0].number(), Some(2.0));
        assert_eq!(copy_out(engine, &out[1]), b"alpha");
        assert_eq!(out[2].number(), Some(42.0));
        assert_eq!(out[3].number(), Some(5.0));
        assert_eq!(out[4].number(), Some(4.0));
        assert_eq!(out[5].number(), Some(2.0));
        omni_engine_free(engine);
    }
}

#[test]
fn a_parallel_batch_returns_the_first_failing_status_and_runs_every_item() {
    let engine = engine();
    unsafe {
        let (code, out, statuses) = batch(engine, &["1 +", "2 + 3", "1 / 0", "20 / 4"], 4);
        assert_eq!(code, OMNI_ERR_COMPILE, "the FIRST failure, not the last");
        assert_eq!(
            statuses,
            vec![OMNI_ERR_COMPILE, OMNI_OK, OMNI_ERR_RUNTIME, OMNI_OK]
        );
        assert_eq!(out[0].tag, OMNI_UNDEF, "a failed item carries no value");
        assert_eq!(out[2].tag, OMNI_UNDEF);
        assert_eq!(out[1].number(), Some(5.0));
        assert_eq!(out[3].number(), Some(5.0), "the item after a failure ran");

        // The per-item messages survive the parallel path exactly as they do the
        // sequential one; only the execution was spread out.
        assert!(batch_error(engine, 0).starts_with("[Line 1, Col "));
        assert_eq!(batch_error(engine, 1), "");
        assert!(batch_error(engine, 2).contains("division by zero"));
        omni_engine_free(engine);
    }
}

/// One batch shares one egress rewind, so every string is readable when the call
/// returns — the property that makes the parallel path usable per item rather
/// than only per last item.
#[test]
fn every_parallel_string_result_is_readable_at_once() {
    let engine = engine();
    unsafe {
        let (code, out, _) = batch(
            engine,
            &[
                r#""a" + "1""#,
                r#""b" + "2""#,
                r#""c" + "3""#,
                r#""d" + "4""#,
            ],
            4,
        );
        assert_eq!(code, OMNI_OK);
        // Read in reverse, so a per-item rewind would be caught rather than
        // accidentally agreeing with the order the items were written in.
        assert_eq!(copy_out(engine, &out[3]), b"d4");
        assert_eq!(copy_out(engine, &out[2]), b"c3");
        assert_eq!(copy_out(engine, &out[1]), b"b2");
        assert_eq!(copy_out(engine, &out[0]), b"a1");
        omni_engine_free(engine);
    }
}

#[test]
fn a_parallel_result_expires_on_the_next_execution() {
    let engine = engine();
    unsafe {
        let (_, out, _) = batch(engine, &[r#""parallel result""#], 2);
        let held = out[0];
        let mut buf = vec![0u8; held.len as usize + 1];
        let n = omni_str_copy(engine, &held, buf.as_mut_ptr().cast(), buf.len());
        assert_eq!(&buf[..n], b"parallel result");

        call(engine, r#""a single exec""#);
        let n = omni_str_copy(engine, &held, buf.as_mut_ptr().cast(), buf.len());
        assert_ne!(
            &buf[..n],
            b"parallel result",
            "a stale read is a wrong read"
        );
        omni_engine_free(engine);
    }
}

/// `0` asks for the automatic worker count and `1` takes the sequential path.
/// Neither may change an answer.
#[test]
fn max_threads_zero_and_one_match_the_sequential_batch() {
    let engine = engine();
    unsafe {
        let sources = ["1 + 1", "1 / 0", "2 * 2", "len(\"abcd\")", "40 + 2"];
        let (auto_code, auto_out, auto_statuses) = batch(engine, &sources, 0);
        let (one_code, one_out, one_statuses) = batch(engine, &sources, 1);
        assert_eq!(auto_code, one_code);
        assert_eq!(auto_statuses, one_statuses);
        for (index, (auto, one)) in auto_out.iter().zip(&one_out).enumerate() {
            assert_eq!(auto, one, "item {index}");
        }
        assert_eq!(auto_code, OMNI_ERR_RUNTIME);
        assert_eq!(auto_out[4].number(), Some(42.0));
        omni_engine_free(engine);
    }
}

#[test]
fn a_host_function_runs_on_worker_threads() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn double(
        _user_data: *mut c_void,
        args: *const OmniValue,
        argc: u32,
        out: *mut OmniValue,
    ) -> i32 {
        assert_eq!(argc, 1);
        CALLS.fetch_add(1, Ordering::SeqCst);
        let n = unsafe { (*args).number().unwrap_or_default() };
        unsafe {
            *out = OmniValue {
                tag: OMNI_NUMBER,
                len: 0,
                payload: OmniPayload { number: n * 2.0 },
            };
        }
        OMNI_HOST_OK
    }

    let engine = engine();
    unsafe {
        let name = "double";
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr().cast(),
                name.len(),
                1,
                Some(double),
                std::ptr::null_mut(),
            ),
            OMNI_OK
        );
        let sources = vec!["double(21)"; 16];
        let (code, out, statuses) = batch(engine, &sources, 8);
        assert_eq!(code, OMNI_OK);
        assert_eq!(statuses, vec![OMNI_OK; 16]);
        assert!(out.iter().all(|value| value.number() == Some(42.0)));
        assert_eq!(CALLS.load(Ordering::SeqCst), 16, "one call per item");
        omni_engine_free(engine);
    }
}

#[test]
fn an_empty_parallel_batch_writes_nothing_and_expires_nothing() {
    let engine = engine();
    unsafe {
        let (code, held) = call(engine, "\"still current\"");
        assert_eq!(code, OMNI_OK);
        let (empty, out, statuses) = batch(engine, &[], 4);
        assert_eq!(empty, OMNI_OK);
        assert_eq!(statuses, vec![u32::MAX], "the poison survived");
        assert_eq!(out[0], poison());
        assert_eq!(copy_out(engine, &held), b"still current");

        let mut spare = poison();
        let mut spare_status = u32::MAX;
        assert_eq!(
            omni_exec_parallel(
                engine,
                std::ptr::null(),
                std::ptr::null(),
                0,
                8,
                &mut spare,
                &mut spare_status,
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
fn argument_errors_refuse_the_parallel_batch_without_writing() {
    let engine = engine();
    unsafe {
        let sources = ["1 + 1", "2 + 2"];
        let ptrs: Vec<*const std::ffi::c_char> =
            sources.iter().map(|s| s.as_ptr().cast()).collect();
        let lens = [5usize, 5];
        let mut out = [poison(); 2];
        let mut statuses = [u32::MAX; 2];
        let untouched = out;

        type Call = (
            *const *const std::ffi::c_char,
            *const usize,
            usize,
            *mut OmniValue,
            *mut u32,
        );
        let refusals: Vec<Call> = vec![
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
                omni_exec_parallel(engine, *s, *l, *n, 4, *o, *st),
                OMNI_ERR_ARG,
                "refusal {i}"
            );
        }
        assert_eq!(
            omni_exec_parallel(
                std::ptr::null_mut(),
                ptrs.as_ptr(),
                lens.as_ptr(),
                2,
                4,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_ERR_ARG
        );

        let too_long = [MAX_SOURCE_LEN + 1, 5];
        assert_eq!(
            omni_exec_parallel(
                engine,
                ptrs.as_ptr(),
                too_long.as_ptr(),
                2,
                4,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_ERR_ARG
        );

        // Malformed UTF-8 in the second item refuses the call before the first
        // item runs, whether or not a worker could have reached it first.
        let bad = b"\xff\xfe";
        let bad_ptrs = [ptrs[0], bad.as_ptr().cast::<std::ffi::c_char>()];
        let bad_lens = [5usize, 2];
        assert_eq!(
            omni_exec_parallel(
                engine,
                bad_ptrs.as_ptr(),
                bad_lens.as_ptr(),
                2,
                4,
                out.as_mut_ptr(),
                statuses.as_mut_ptr()
            ),
            OMNI_ERR_ARG
        );
        assert_eq!(out, untouched, "no refusal wrote a value");
        assert_eq!(statuses, [u32::MAX; 2], "and none wrote a status");
        omni_engine_free(engine);
    }
}

#[test]
fn a_parallel_batch_counts_items_not_crossings() {
    let engine = engine();
    unsafe {
        let mut stats = OmniStats::default();
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        let before = stats.execs;
        batch(engine, &["1", "2", "1 +", "4"], 4);
        assert_eq!(omni_debug_stats(engine, &mut stats), OMNI_OK);
        assert_eq!(stats.execs, before + 4, "four items, one crossing");
        omni_engine_free(engine);
    }
}
