#![no_main]

use libfuzzer_sys::fuzz_target;

// Everything the parser rejects stops here; everything it accepts reaches the
// lowering pass, including the call-resolution and arity checks.
fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    let _ = omni_core::fuzz_api::lower_only(&source);
});
