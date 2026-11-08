//! The native plugin loader, tested two ways: the `verify_bytes` door fed with
//! hostile byte-slice manifests — every refusal check, one at a time — and the
//! real fixture library loaded through the engine's FFI, proving a good plugin
//! runs, a panicking one is contained, and a bad load leaves the engine clean.

use std::ffi::c_char;
use std::path::PathBuf;

use omni::plugin::{Plugin, PluginError, verify_bytes};
use omni::{
    MAX_ARGS, MAX_IDENTIFIER_LEN, OMNI_ABI_VERSION, OMNI_ERR_ARG, OMNI_NUMBER, OMNI_OK, OmniValue,
    omni_engine_free, omni_engine_load_plugin, omni_engine_new, omni_exec, omni_host_fn_count,
    omni_plugin_count,
};
use omni_core::{Engine, Value};

/// The 80-byte manifest header and the 24-byte function entries it points at.
const MANIFEST_SIZE: usize = 80;
const ENTRY_SIZE: usize = 24;

struct FnSpec {
    name: &'static [u8],
    arity: u32,
    /// The `call` field's raw value: verification only checks null-ness, so
    /// any nonzero address stands for "a real function".
    call: usize,
}

/// Lay out a manifest byte slice — header, then function entries, then the
/// NUL-terminated names — and patch every offset into a real pointer at the
/// slice's base address, so the verifier's window checks see exactly the
/// layout the builder intended.
fn build_manifest(name: &[u8], functions: &[FnSpec]) -> Vec<u8> {
    let names_start = MANIFEST_SIZE + functions.len() * ENTRY_SIZE;
    let mut names = Vec::new();
    let mut name_offsets = Vec::with_capacity(functions.len());
    for function in functions {
        name_offsets.push(names_start + names.len());
        names.extend_from_slice(function.name);
        names.push(0);
    }
    let mut bytes = vec![0u8; names_start + names.len()];
    bytes[names_start..].copy_from_slice(&names);
    bytes[0..4].copy_from_slice(&0x4F4D_4E49u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&(MANIFEST_SIZE as u32).to_le_bytes());
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&OMNI_ABI_VERSION.to_le_bytes());
    bytes[16..16 + name.len()].copy_from_slice(name);
    bytes[64..68].copy_from_slice(&(functions.len() as u32).to_le_bytes());
    for (index, function) in functions.iter().enumerate() {
        let entry = MANIFEST_SIZE + index * ENTRY_SIZE;
        bytes[entry + 8..entry + 12].copy_from_slice(&function.arity.to_le_bytes());
        bytes[entry + 16..entry + 24].copy_from_slice(&(function.call as u64).to_le_bytes());
    }
    let base = bytes.as_ptr() as usize;
    if !functions.is_empty() {
        bytes[72..80].copy_from_slice(&((base + MANIFEST_SIZE) as u64).to_le_bytes());
    }
    for (index, offset) in name_offsets.iter().enumerate() {
        let entry = MANIFEST_SIZE + index * ENTRY_SIZE;
        bytes[entry..entry + 8].copy_from_slice(&((base + offset) as u64).to_le_bytes());
    }
    bytes
}

const ONE: [FnSpec; 1] = [FnSpec {
    name: b"double",
    arity: 1,
    call: 1,
}];

#[test]
fn a_well_formed_manifest_verifies() {
    assert_eq!(verify_bytes(&build_manifest(b"empty", &[])), Ok(()));
    assert_eq!(verify_bytes(&build_manifest(b"fixture", &ONE)), Ok(()));
}

#[test]
fn the_manifest_header_is_checked_field_by_field() {
    let mut bad = build_manifest(b"ok", &[]);
    bad[0..4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    assert_eq!(verify_bytes(&bad), Err(PluginError::BadMagic(0x1234_5678)));

    let mut bad = build_manifest(b"ok", &[]);
    bad[4..8].copy_from_slice(&79u32.to_le_bytes());
    assert_eq!(verify_bytes(&bad), Err(PluginError::BadSize(79)));

    let mut bad = build_manifest(b"ok", &[]);
    bad[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(verify_bytes(&bad), Err(PluginError::BadAbiVersion(2)));

    let mut bad = build_manifest(b"ok", &[]);
    bad[12..16].copy_from_slice(&(OMNI_ABI_VERSION + 1).to_le_bytes());
    assert_eq!(
        verify_bytes(&bad),
        Err(PluginError::AbiMismatch {
            plugin: OMNI_ABI_VERSION + 1,
            engine: OMNI_ABI_VERSION,
        })
    );

    assert_eq!(
        verify_bytes(&build_manifest(b"", &[])),
        Err(PluginError::Unnamed)
    );
    assert_eq!(
        verify_bytes(&build_manifest(b"fixt\xFFure", &[])),
        Err(PluginError::BadName)
    );
}

#[test]
fn the_function_table_is_checked_field_by_field() {
    let mut too_many = build_manifest(b"ok", &[]);
    too_many[64..68].copy_from_slice(&65u32.to_le_bytes());
    assert_eq!(
        verify_bytes(&too_many),
        Err(PluginError::TooManyFunctions(65))
    );

    let mut null_table = build_manifest(b"ok", &[]);
    null_table[64..68].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(
        verify_bytes(&null_table),
        Err(PluginError::BadFunctionTable)
    );

    assert_eq!(
        verify_bytes(&build_manifest(
            b"ok",
            &[FnSpec {
                name: b"double",
                arity: 1,
                call: 0,
            }]
        )),
        Err(PluginError::NullFunction {
            index: 0,
            name: "double".to_string(),
        })
    );

    assert_eq!(
        verify_bytes(&build_manifest(
            b"ok",
            &[FnSpec {
                name: b"double",
                arity: MAX_ARGS as u32 + 1,
                call: 1,
            }]
        )),
        Err(PluginError::ArityTooLarge {
            name: "double".to_string(),
            arity: MAX_ARGS as u32 + 1,
        })
    );

    assert_eq!(
        verify_bytes(&build_manifest(
            b"ok",
            &[
                FnSpec {
                    name: b"double",
                    arity: 1,
                    call: 1,
                },
                FnSpec {
                    name: b"double",
                    arity: 2,
                    call: 1,
                },
            ]
        )),
        Err(PluginError::DuplicateName("double".to_string()))
    );

    assert_eq!(
        verify_bytes(&build_manifest(
            b"ok",
            &[FnSpec {
                name: b"d\xFFuble",
                arity: 1,
                call: 1,
            }]
        )),
        Err(PluginError::BadFunctionName { index: 0 })
    );

    assert_eq!(
        verify_bytes(&build_manifest(
            b"ok",
            &[FnSpec {
                name: &[b'd'; MAX_IDENTIFIER_LEN + 1],
                arity: 1,
                call: 1,
            }]
        )),
        Err(PluginError::BadFunctionName { index: 0 })
    );
}

#[test]
fn reads_past_the_window_are_refused() {
    // A slice too short for the full header.
    let whole = build_manifest(b"ok", &ONE);
    assert_eq!(verify_bytes(&whole[..79]), Err(PluginError::Truncated));
    assert_eq!(verify_bytes(&whole[..4]), Err(PluginError::Truncated));
    assert_eq!(verify_bytes(&[]), Err(PluginError::Truncated));

    // A table pointer that escapes the window.
    let mut runaway_table = build_manifest(b"ok", &ONE);
    runaway_table[72..80].copy_from_slice(&0x1_0000_0000u64.to_le_bytes());
    assert_eq!(verify_bytes(&runaway_table), Err(PluginError::Truncated));

    // A name pointer that escapes the window.
    let mut runaway_name = build_manifest(b"ok", &ONE);
    runaway_name[80..88].copy_from_slice(&0x1_0000_0000u64.to_le_bytes());
    assert_eq!(
        verify_bytes(&runaway_name),
        Err(PluginError::BadFunctionName { index: 0 })
    );
}

/// Where the fixture cdylib lands for the profile this test binary was built
/// with — the debug path first, the release path when the suite runs under
/// `--release`. `None` when it has not been built at all.
fn fixture_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "omni_plugin_fixture.dll";
    #[cfg(all(unix, target_os = "macos"))]
    let name = "libomni_plugin_fixture.dylib";
    #[cfg(all(unix, not(target_os = "macos")))]
    let name = "libomni_plugin_fixture.so";
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    for profile in ["debug", "release"] {
        let path = root.join("target").join(profile).join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

#[test]
fn the_fixture_loads_runs_and_survives_a_panic() {
    let Some(path) = fixture_path() else {
        eprintln!("skipping: run `cargo build -p omni-plugin-fixture` first");
        return;
    };
    let plugin = Plugin::load(path.to_str().unwrap()).expect("the fixture must load");
    assert_eq!(plugin.name(), "fixture");

    let mut engine = Engine::new();
    plugin
        .register(&mut engine)
        .expect("registration must succeed");
    assert_eq!(engine.host_fn_count(), 3);

    let doubled = engine.exec("double(21)").expect("double must run");
    assert_eq!(doubled.bits(), Value::from_f64(42.0).bits());
    let summed = engine.exec("add(2, 3)").expect("add must run");
    assert_eq!(summed.bits(), Value::from_f64(5.0).bits());

    let crash = engine
        .exec("crash()")
        .expect_err("crash is a host-call failure");
    assert!(
        crash.message.contains("the fixture crashed on purpose"),
        "{crash:?}"
    );
    let again = engine
        .exec("double(5)")
        .expect("the engine must survive a plugin panic");
    assert_eq!(again.bits(), Value::from_f64(10.0).bits());

    plugin.unregister(&mut engine);
    assert_eq!(engine.host_fn_count(), 0);
    assert!(engine.exec("double(1)").is_err(), "unregister must forget");
}

#[test]
fn the_ffi_loads_runs_and_refuses_plugins() {
    let Some(path) = fixture_path() else {
        eprintln!("skipping: run `cargo build -p omni-plugin-fixture` first");
        return;
    };
    let path = path.to_str().unwrap();
    // Safety: the engine comes from omni_engine_new and is freed once.
    let engine = omni_engine_new();

    let code =
        unsafe { omni_engine_load_plugin(engine, path.as_ptr().cast::<c_char>(), path.len()) };
    assert_eq!(code, OMNI_OK, "the fixture must load through the FFI");
    assert_eq!(unsafe { omni_plugin_count(engine) }, 1);
    assert_eq!(unsafe { omni_host_fn_count(engine) }, 3);

    let source = "double(21)";
    let mut out = OmniValue::default();
    let code = unsafe {
        omni_exec(
            engine,
            source.as_ptr().cast::<c_char>(),
            source.len(),
            &mut out,
        )
    };
    assert_eq!(code, OMNI_OK);
    assert_eq!(out.tag, OMNI_NUMBER);
    assert_eq!(out.number(), Some(42.0));

    let bogus = "no-such-plugin-anywhere.dll";
    let code =
        unsafe { omni_engine_load_plugin(engine, bogus.as_ptr().cast::<c_char>(), bogus.len()) };
    assert_eq!(code, OMNI_ERR_ARG, "a missing library is refused");
    assert_eq!(unsafe { omni_plugin_count(engine) }, 1, "nothing was added");

    // The test executable is a real mapped image that exports no manifest.
    let exe = std::env::current_exe().expect("the test binary's own path");
    let exe = exe.to_str().unwrap().to_owned();
    let code = unsafe { omni_engine_load_plugin(engine, exe.as_ptr().cast::<c_char>(), exe.len()) };
    assert_eq!(code, OMNI_ERR_ARG, "a manifest-less image is refused");
    assert_eq!(unsafe { omni_plugin_count(engine) }, 1);

    unsafe { omni_engine_free(engine) };
}
