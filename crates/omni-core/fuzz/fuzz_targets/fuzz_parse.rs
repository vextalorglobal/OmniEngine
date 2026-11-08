#![no_main]

use libfuzzer_sys::fuzz_target;

// The lexer runs again inside the parser, so this target also keeps lexer
// coverage: a panic in either stage is a finding.
fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    let _ = omni_core::fuzz_api::parse_only(&source);
});
