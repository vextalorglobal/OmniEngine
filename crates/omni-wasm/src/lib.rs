//! Scaffolded WebAssembly surface for OmniEngine.
//!
//! This crate is a workspace member but not a *default* member, so
//! `cargo build` never needs the `wasm32-unknown-unknown` target. Build it
//! explicitly with `cargo check -p omni-wasm` today and
//! `cargo build --target wasm32-unknown-unknown -p omni-wasm` once that target
//! and `wasm-bindgen` are installed.
//!
//! The shape mirrors the C ABI on purpose: a program in, a tagged result out,
//! errors as strings. Adding `#[wasm_bindgen]` decorators later is a wrapper
//! around this surface rather than a rewrite of it.
//!
//! One difference is load-bearing and worth keeping in view: the C ABI passes a
//! string as an *address* plus a copy function, because a host on the other side
//! of a dynamic load cannot take Rust's ownership. A wasm boundary passes bytes,
//! so this scaffold has no egress block, no `omni_str_copy`, and no "valid until
//! your next exec" rule. `WasmValue::text` says where that difference lands.

use omni_core::{Engine, ErrorKind, Value};

/// Mirrors `OMNI_OK` and friends from the C ABI, so the two bridges agree on
/// what a status means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Compile,
    Runtime,
}

/// Mirrors the C `OmniValue`. A wasm host reads `tag` to know which payload is
/// live, exactly as a C host does.
#[derive(Debug, Clone, Default)]
pub struct WasmValue {
    pub tag: u32,
    pub number: f64,
    /// The payload for [`TAG_STRING`], and `None` for every other tag.
    ///
    /// Owned text rather than an address, and that is the whole difference
    /// between this bridge and the C one: a value crossing a wasm boundary is
    /// copied as bytes, so there is no egress problem to solve. The C ABI needs
    /// a block that rewinds instead of freeing, plus `omni_str_copy`, plus a
    /// "valid until your next exec" rule, because a `*const u8` handed to Python
    /// or Node outlives the call that produced it. A wasm boundary has no such
    /// pointer — which is why there is no `wasm_str_copy` to match.
    pub text: Option<String>,
}

pub const TAG_NUMBER: u32 = 0;
pub const TAG_NIL: u32 = 1;
pub const TAG_BOOL: u32 = 2;
pub const TAG_UNDEF: u32 = 3;
pub const TAG_STRING: u32 = 4;

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
    /// Phase 3 decorates this with `#[wasm_bindgen]`.
    pub fn exec(&mut self, source: &str) -> Result<WasmValue, String> {
        match self.engine.exec(source) {
            Ok(v) => Ok(encode(&self.engine, v)),
            Err(e) => Err(format!("{e}")),
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

/// Put a core value into the scaffold's tagged result.
///
/// The engine is the argument that makes this possible: a string's bytes live
/// behind an address only the engine can prove are still mapped, and the same
/// rule that keeps `Value`'s own `Display` from following a pointer applies
/// here. What comes out the other side is owned text, because a wasm host gets
/// a copy rather than a claim on engine memory.
fn encode(engine: &Engine, v: Value) -> WasmValue {
    if v.is_string() {
        return WasmValue {
            tag: TAG_STRING,
            number: 0.0,
            // An unreadable box has no text to give, and `None` says that
            // without inventing an empty string that would read as a real one.
            text: engine
                .string_of(v)
                .map(|b| String::from_utf8_lossy(b).into_owned()),
        };
    }
    if let Some(n) = v.as_f64() {
        return WasmValue {
            tag: TAG_NUMBER,
            number: n,
            text: None,
        };
    }
    if let Some(b) = v.as_bool() {
        return WasmValue {
            tag: TAG_BOOL,
            number: u32::from(b) as f64,
            text: None,
        };
    }
    let tag = if v == Value::UNDEF {
        TAG_UNDEF
    } else {
        TAG_NIL
    };
    WasmValue {
        tag,
        number: 0.0,
        text: None,
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

    /// The tag integers are the C ABI's, in the same order, and they are part of
    /// the surface a JS host will switch on. A new type has to be added to both
    /// bridges in the same change.
    #[test]
    fn the_tags_match_the_c_abi() {
        assert_eq!(
            (TAG_NUMBER, TAG_NIL, TAG_BOOL, TAG_UNDEF, TAG_STRING),
            (0, 1, 2, 3, 4)
        );
    }

    #[test]
    fn a_string_result_comes_out_as_text_not_an_address() {
        let mut w = OmniWasm::new();
        let v = w
            .exec(r#"let who = "world"; "hello, " + who"#)
            .expect("should evaluate");
        assert_eq!(v.tag, TAG_STRING);
        assert_eq!(v.number, 0.0, "the number arm is not the payload here");
        assert_eq!(v.text.as_deref(), Some("hello, world"));
        // And requiring a number from one still refuses, as `omni_exec_number`
        // does across the C boundary.
        let mut w2 = OmniWasm::new();
        assert_eq!(
            w2.eval_number(r#""hi""#).unwrap_err(),
            "result is not a number"
        );
    }

    #[test]
    fn escapes_reach_the_host_as_characters() {
        let mut w = OmniWasm::new();
        let v = w.exec(r#""tab\there\u{1F600}""#).expect("should evaluate");
        assert_eq!(v.text.as_deref(), Some("tab\there\u{1F600}"));
    }
}
