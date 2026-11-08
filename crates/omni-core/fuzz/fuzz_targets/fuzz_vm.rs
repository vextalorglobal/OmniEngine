#![no_main]

use libfuzzer_sys::fuzz_target;

// Two shapes in one target: source through the whole engine, and raw bytes
// decoded into instructions (eight bytes per `Instr`) over the same bytes as
// source text. The second is what reaches `run_chunk` with spans and jump
// targets the compiler would never emit; the step limit and stack cap are what
// keep those inputs bounded.
fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    let _ = omni_core::fuzz_api::execute(&source);
    let _ = omni_core::fuzz_api::execute_bytes(data);
});
