//! Contract tests for the Phase 9 profiling surface: `OmniProfile`,
//! `omni_engine_set_profiling`, `omni_profile`, and `omni_opcode_name`.
//!
//! Written as an external consumer would: raw pointers, caller-owned output,
//! and the re-entrancy rule checked from inside a host callback. The surface
//! is additive within ABI 4, so these tests also prove the version stayed put
//! and the new struct's layout is pinned.

use std::ffi::c_void;

use omni::*;

fn engine() -> *mut OmniEngine {
    let handle = omni_engine_new();
    assert!(!handle.is_null(), "omni_engine_new returned null");
    handle
}

fn call(engine: *mut OmniEngine, source: &str) -> (u32, OmniValue) {
    let mut out = OmniValue::default();
    let code = unsafe { omni_exec(engine, source.as_ptr().cast(), source.len(), &mut out) };
    (code, out)
}

fn profile(engine: *mut OmniEngine) -> (u32, OmniProfile) {
    let mut out = OmniProfile::default();
    let code = unsafe { omni_profile(engine, &mut out) };
    (code, out)
}

#[test]
fn the_profile_reports_the_last_execution() {
    let handle = engine();
    let (code, value) = call(handle, "2 + 3");
    assert_eq!(code, OMNI_OK);
    assert_eq!(value.number(), Some(5.0));

    let (code, profile) = profile(handle);
    assert_eq!(code, OMNI_OK);
    assert_eq!(profile.size as usize, std::mem::size_of::<OmniProfile>());
    assert!(profile.opcode_count >= 3, "a real instruction set");
    // The optimizer folded `2 + 3` to one push: PUSH_FOLDED then RET.
    assert_eq!(profile.total_instructions, 2);
    let heatmap_sum: u64 = profile.opcode_calls.iter().sum();
    assert_eq!(heatmap_sum, profile.total_instructions);
    assert_eq!(profile.peak_bytes, 0, "scalars allocate nothing");
    assert!(
        profile.elapsed_ns > 0,
        "the whole-run wall clock is always recorded"
    );
    assert_eq!(profile.reserved, 0);

    // Entries past the live opcodes are always zero.
    for entry in &profile.opcode_calls[profile.opcode_count as usize..] {
        assert_eq!(*entry, 0);
    }

    unsafe { omni_engine_free(handle) };
}

#[test]
fn every_heatmap_index_names_through_omni_opcode_name() {
    let handle = engine();
    call(handle, "1 + 1");
    let (_, profile) = profile(handle);

    let mut buf = [0i8; 32];
    for index in 0..profile.opcode_count {
        let n = unsafe { omni_opcode_name(index, buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0, "opcode {index} has a mnemonic");
        let text = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(!text.is_empty());
    }

    // An unknown index writes an empty string and returns 0.
    let n = unsafe { omni_opcode_name(profile.opcode_count, buf.as_mut_ptr(), buf.len()) };
    assert_eq!(n, 0);
    assert_eq!(buf[0], 0);
    let n = unsafe { omni_opcode_name(u32::MAX, buf.as_mut_ptr(), buf.len()) };
    assert_eq!(n, 0);

    // A small buffer truncates rather than overflowing: "PUSH_FOLDED" into 5.
    let mut small = [0x7fu8; 5];
    let n = unsafe { omni_opcode_name(2, small.as_mut_ptr().cast(), small.len()) };
    assert_eq!(n, 4, "cap - 1 bytes");
    assert_eq!(&small[..4], b"PUSH");
    assert_eq!(small[4], 0);

    // Null buffer or zero capacity writes nothing.
    assert_eq!(unsafe { omni_opcode_name(2, std::ptr::null_mut(), 8) }, 0);
    assert_eq!(unsafe { omni_opcode_name(2, buf.as_mut_ptr(), 0) }, 0);

    unsafe { omni_engine_free(handle) };
}

#[test]
fn the_profiling_toggle_controls_the_nanosecond_split() {
    let handle = engine();
    call(handle, "1 + 2");
    let (_, untimed) = profile(handle);
    assert_eq!(untimed.opcode_ns.iter().sum::<u64>(), 0);

    assert_eq!(unsafe { omni_engine_set_profiling(handle, 1) }, OMNI_OK);
    let chain = format!("let x = 1\nx{}", " + 1".repeat(1_000));
    call(handle, &chain);
    let (_, timed) = profile(handle);
    let self_time: u64 = timed.opcode_ns.iter().sum();
    assert!(self_time > 0, "enabled profiling brackets every dispatch");
    assert!(
        self_time <= timed.elapsed_ns,
        "self-times live inside the whole-run wall clock"
    );

    assert_eq!(unsafe { omni_engine_set_profiling(handle, 0) }, OMNI_OK);
    call(handle, "1 + 2");
    let (_, off) = profile(handle);
    assert_eq!(off.opcode_ns.iter().sum::<u64>(), 0);

    unsafe { omni_engine_free(handle) };
}

#[test]
fn peak_bytes_cross_the_boundary() {
    let handle = engine();
    let big = "x".repeat(4 * 1024);
    let source = format!("let big = \"{big}\"\nlen(big)");
    let (code, value) = call(handle, &source);
    assert_eq!(code, OMNI_OK);
    assert_eq!(value.number(), Some(big.len() as f64));

    let (_, profile) = profile(handle);
    assert!(
        profile.peak_bytes >= big.len() as u64,
        "the literal was materialised: {}",
        profile.peak_bytes
    );
    assert_eq!(
        profile.opcode_calls.iter().sum::<u64>(),
        profile.total_instructions
    );

    unsafe { omni_engine_free(handle) };
}

/// A callback that asks the engine for its profile while the engine is
/// borrowed for the callback itself. Both reads must answer re-entrancy, and
/// the callback must still be able to finish: the verdict travels in the
/// returned number, because an assertion unwinding through Rust frames would
/// be exactly what caller rule 10 forbids.
unsafe extern "C" fn peek(
    engine_ptr: *mut c_void,
    _args: *const OmniValue,
    _argc: u32,
    out: *mut OmniValue,
) -> i32 {
    let handle = engine_ptr.cast::<OmniEngine>();
    let mut verdict = 1.0f64;
    let mut profile = OmniProfile::default();
    let code = unsafe { omni_profile(handle, &mut profile) };
    verdict *= f64::from(code == OMNI_ERR_REENTRANT);
    let code = unsafe { omni_engine_set_profiling(handle, 1) };
    verdict *= f64::from(code == OMNI_ERR_REENTRANT);
    // Safety: the callback contract hands over `out`, writable by the callee.
    unsafe {
        *out = OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload { number: verdict },
        };
    }
    OMNI_HOST_OK
}

#[test]
fn a_borrowed_handle_refuses_profile_reads() {
    let handle = engine();
    let code = unsafe {
        omni_register_host_fn(
            handle,
            b"peek".as_ptr().cast(),
            4,
            0,
            Some(peek),
            handle.cast::<c_void>(),
        )
    };
    assert_eq!(code, OMNI_OK);

    let (code, value) = call(handle, "peek()");
    assert_eq!(code, OMNI_OK);
    assert_eq!(value.number(), Some(1.0), "both reads answered re-entrancy");

    unsafe { omni_engine_free(handle) };
}

#[test]
fn null_arguments_are_refused() {
    let handle = engine();
    let mut profile = OmniProfile::default();
    assert_eq!(
        unsafe { omni_profile(std::ptr::null_mut(), &mut profile) },
        OMNI_ERR_ARG
    );
    assert_eq!(
        unsafe { omni_profile(handle, std::ptr::null_mut()) },
        OMNI_ERR_ARG
    );
    assert_eq!(
        unsafe { omni_engine_set_profiling(std::ptr::null_mut(), 1) },
        OMNI_ERR_ARG
    );
    unsafe { omni_engine_free(handle) };
}

#[test]
fn the_surface_is_additive_within_abi_six() {
    // No layout of an existing struct moved, and no signature changed: a host
    // compiled against the v6 header remains correct.
    assert_eq!(omni_abi_version(), OMNI_ABI_VERSION);
    assert_eq!(OMNI_ABI_VERSION, 6);
}
