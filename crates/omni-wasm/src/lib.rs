//! Scaffolded WebAssembly surface for OmniEngine.
//!
//! This crate is a workspace member but not a *default* member, so
//! `cargo build` never needs the `wasm32-unknown-unknown` target. Build it
//! explicitly with `cargo check -p omni-wasm` today and
//! `cargo build --target wasm32-unknown-unknown -p omni-wasm` once that target
//! and `wasm-bindgen` are installed.
//!
//! The shape mirrors the C ABI on purpose: a number in, a tagged result out,
//! errors as strings. Adding `#[wasm_bindgen]` decorators later is a wrapper
//! around this surface rather than a rewrite of it.

use omni_core::{Engine, ErrorKind, Value};

/// Mirrors `OMNI_OK` and friends from the C ABI, so the two bridges agree on
/// what a status means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Compile,
    Runtime,
}

/// Mirrors the C `OmniValue`. A wasm host reads `tag` to know how to interpret
/// `number`, exactly as a C host does.
#[derive(Debug, Clone, Copy, Default)]
pub struct WasmValue {
    pub tag: u32,
    pub number: f64,
}

pub const TAG_NUMBER: u32 = 0;
pub const TAG_NIL: u32 = 1;
pub const TAG_BOOL: u32 = 2;
pub const TAG_UNDEF: u32 = 3;

/// One engine per wasm instance. The instance boundary replaces the opaque
/// handle the C ABI uses.
#[derive(Default)]
pub struct OmniWasm {
    engine: Engine,
}

impl OmniWasm {
    pub fn new() -> OmniWasm {
        OmniWasm {
            engine: Engine::new(),
        }
    }

    /// Evaluate `source` and return the tagged result, or an error string.
    /// Phase 2 decorates this with `#[wasm_bindgen]`.
    pub fn exec(&mut self, source: &str) -> Result<WasmValue, String> {
        match self.engine.exec(source) {
            Ok(v) => Ok(encode(v)),
            Err(e) => Err(format!("{}", e)),
        }
    }

    pub fn eval_number(&mut self, source: &str) -> Result<f64, String> {
        match self.exec(source) {
            Ok(v) if v.tag == TAG_NUMBER => Ok(v.number),
            Ok(_) => Err("result is not a number".to_string()),
            Err(e) => Err(e),
        }
    }

    /// The status code a C host would have seen, kept for parity testing.
    pub fn status_of(error: &str) -> Status {
        if error.contains("division by zero") || error.contains("undefined variable") {
            Status::Runtime
        } else {
            Status::Compile
        }
    }
}

fn encode(v: Value) -> WasmValue {
    if let Some(n) = v.as_f64() {
        WasmValue {
            tag: TAG_NUMBER,
            number: n,
        }
    } else if let Some(b) = v.as_bool() {
        WasmValue {
            tag: TAG_BOOL,
            number: u32::from(b) as f64,
        }
    } else if v == Value::UNDEF {
        WasmValue {
            tag: TAG_UNDEF,
            number: 0.0,
        }
    } else {
        WasmValue {
            tag: TAG_NIL,
            number: 0.0,
        }
    }
}

/// Maps an [`omni_core::ErrorKind`] onto the C status integers. Present so the
/// wasm and C bridges cannot silently disagree once the wasm build goes live.
pub const fn status_code(kind: ErrorKind) -> u32 {
    match kind {
        ErrorKind::Lex | ErrorKind::Parse => 1,
        ErrorKind::Runtime => 2,
        ErrorKind::Panic => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_evaluates_the_vertical_slice() {
        let mut w = OmniWasm::new();
        let v = w.exec("let answer = 40 + 2").expect("should evaluate");
        assert_eq!(v.tag, TAG_NUMBER);
        assert_eq!(v.number, 42.0);
    }

    #[test]
    fn errors_come_back_as_text() {
        let mut w = OmniWasm::new();
        let e = w.exec("1 / 0").unwrap_err();
        assert!(e.contains("division by zero"), "got {e}");
        assert_eq!(OmniWasm::status_of(&e), Status::Runtime);
    }

    #[test]
    fn non_numbers_are_rejected_by_eval_number() {
        let mut w = OmniWasm::new();
        assert_eq!(w.eval_number("7").unwrap(), 7.0);
    }

    #[test]
    fn status_codes_match_the_c_abi() {
        assert_eq!(status_code(ErrorKind::Parse), 1);
        assert_eq!(status_code(ErrorKind::Runtime), 2);
        assert_eq!(status_code(ErrorKind::Panic), 3);
    }
}
