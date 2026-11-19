//! A test plugin for `omni-ffi`'s native plugin loader.
//!
//! This crate is deliberately dependency-free: a real plugin must not drag the
//! engine into the host process, so every type here is an ABI copy of the
//! loader's, kept layout-identical by the loader's verification and the
//! omni-ffi test suite. It exports three functions — `double`, `add`, and
//! `crash` — so the suite can prove a good plugin works, a panicking one is
//! contained, and the engine survives both.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::{c_char, c_void};
use std::mem::size_of;
use std::ptr;

/// ABI copy of the engine's `OmniValue`: `tag` at 0, `len` at 4, payload at 8.
#[repr(C)]
pub union OmniPayload {
    pub number: f64,
    pub ptr: *const c_char,
}

#[repr(C)]
pub struct OmniValue {
    pub tag: u32,
    pub len: u32,
    pub payload: OmniPayload,
}

const OMNI_NUMBER: u32 = 0;
const OMNI_STRING: u32 = 4;
const OMNI_HOST_OK: i32 = 0;
const OMNI_HOST_ERR: i32 = 1;
/// ABI copy of the loader's constants; the loader refuses any mismatch.
const MAGIC: u32 = 0x4F4D_4E49;
const PLUGIN_ABI_VERSION: u32 = 1;
const OMNI_ABI_VERSION: u32 = 5;

/// ABI copy of the loader's `OmniPluginFn` and `OmniPluginManifest`.
#[repr(C)]
pub struct OmniPluginFn {
    pub name: *const c_char,
    pub arity: u32,
    pub reserved: u32,
    pub call: Option<PluginCall>,
}

// The fixture's statics are read-only exports, never mutated after link; the
// pointers inside them only point into the same image.
unsafe impl Sync for OmniPluginFn {}

pub type PluginCall = unsafe extern "C-unwind" fn(
    user_data: *mut c_void,
    args: *const OmniValue,
    argc: u32,
    out: *mut OmniValue,
) -> i32;

#[repr(C)]
pub struct OmniPluginManifest {
    pub magic: u32,
    pub size: u32,
    pub abi_version: u32,
    pub omni_abi: u32,
    pub name: [c_char; 32],
    pub version_major: u32,
    pub version_minor: u32,
    pub version_patch: u32,
    pub reserved: u32,
    pub function_count: u32,
    pub functions: *const OmniPluginFn,
}

// As above: `omni_plugin_manifest` is an immutable export.
unsafe impl Sync for OmniPluginManifest {}

const fn c_name<const N: usize>(text: &[u8]) -> [c_char; N] {
    let mut bytes: [c_char; N] = [0; N];
    let mut i = 0;
    while i < text.len() {
        bytes[i] = text[i] as c_char;
        i += 1;
    }
    bytes
}

static DOUBLE_NAME: &[u8] = b"double\0";
static ADD_NAME: &[u8] = b"add\0";
static CRASH_NAME: &[u8] = b"crash\0";

/// Write one number result. The only arm this fixture produces.
fn number_out(out: *mut OmniValue, value: f64) -> i32 {
    // Safety: the engine hands each plugin function a writable `out` and reads
    // it the moment the function returns.
    unsafe {
        ptr::write(
            out,
            OmniValue {
                tag: OMNI_NUMBER,
                len: 0,
                payload: OmniPayload { number: value },
            },
        );
    }
    OMNI_HOST_OK
}

/// Write one string result. `text` must point into this library's own image —
/// the engine copies it before the call returns, and the caller keeps the
/// library mapped until then.
fn string_out(out: *mut OmniValue, text: &'static [u8]) -> i32 {
    // Safety: `out` is writable, as in `number_out`.
    unsafe {
        ptr::write(
            out,
            OmniValue {
                tag: OMNI_STRING,
                len: text.len() as u32,
                payload: OmniPayload {
                    ptr: text.as_ptr().cast::<c_char>(),
                },
            },
        );
    }
    OMNI_HOST_ERR
}

/// `double(x)` -> `2x`. Arity 1.
///
/// # Safety
///
/// `args` must point at `argc` readable `OmniValue`s, `out` at one writable
/// one — the engine's call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn double(
    _user_data: *mut c_void,
    args: *const OmniValue,
    argc: u32,
    out: *mut OmniValue,
) -> i32 {
    if argc != 1 || args.is_null() {
        return OMNI_HOST_ERR;
    }
    // Safety: `args` addresses `argc` OmniValues the engine owns, readable for
    // this call's extent — the same contract a host callback holds.
    let x = unsafe { (*args).payload.number };
    number_out(out, x * 2.0)
}

/// `add(x, y)` -> `x + y`. Arity 2.
///
/// # Safety
///
/// As `double`: `args` is `argc` readable values, `out` one writable value.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn add(
    _user_data: *mut c_void,
    args: *const OmniValue,
    argc: u32,
    out: *mut OmniValue,
) -> i32 {
    if argc != 2 || args.is_null() {
        return OMNI_HOST_ERR;
    }
    // Safety: as in `double`; two OmniValues are readable at `args`.
    let x = unsafe { (*args).payload.number };
    let y = unsafe { (*args.add(1)).payload.number };
    number_out(out, x + y)
}

/// Panics — and proves a plugin must contain its own unwinds. This library
/// statically links std, so its panic runtime is its own; a panic that
/// unwound out of the DLL could not be caught by the host's runtime on
/// Windows (the exception belongs to a foreign runtime). The pattern every
/// plugin must follow: catch your own panics at the boundary and report the
/// message as a host failure. The engine's own trampoline remains the last
/// line of defense for plugins that share its runtime.
///
/// # Safety
///
/// `out` must point at one writable `OmniValue`; `args` and `argc` are
/// unused and need not be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn crash(
    _user_data: *mut c_void,
    _args: *const OmniValue,
    _argc: u32,
    out: *mut OmniValue,
) -> i32 {
    let payload =
        std::panic::catch_unwind(|| panic!("the fixture crashed on purpose")).unwrap_err();
    let text: &'static [u8] = match payload.downcast_ref::<&'static str>() {
        Some(text) => text.as_bytes(),
        None => b"the plugin panicked",
    };
    string_out(out, text)
}

static FUNCTIONS: [OmniPluginFn; 3] = [
    OmniPluginFn {
        name: DOUBLE_NAME.as_ptr().cast::<c_char>(),
        arity: 1,
        reserved: 0,
        call: Some(double),
    },
    OmniPluginFn {
        name: ADD_NAME.as_ptr().cast::<c_char>(),
        arity: 2,
        reserved: 0,
        call: Some(add),
    },
    OmniPluginFn {
        name: CRASH_NAME.as_ptr().cast::<c_char>(),
        arity: 0,
        reserved: 0,
        call: Some(crash),
    },
];

/// The one symbol the loader looks for. `dlsym`/`GetProcAddress` resolve the
/// static's address; the loader then verifies every field before trusting it.
#[unsafe(no_mangle)]
pub static omni_plugin_manifest: OmniPluginManifest = OmniPluginManifest {
    magic: MAGIC,
    size: size_of::<OmniPluginManifest>() as u32,
    abi_version: PLUGIN_ABI_VERSION,
    omni_abi: OMNI_ABI_VERSION,
    name: c_name(b"fixture"),
    version_major: 0,
    version_minor: 1,
    version_patch: 0,
    reserved: 0,
    function_count: FUNCTIONS.len() as u32,
    functions: FUNCTIONS.as_ptr(),
};
