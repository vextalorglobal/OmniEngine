//! OmniEngine core: the value representation, memory model, and evaluator.
//!
//! This crate knows nothing about C, Python, or Node. The only crate permitted
//! to name a `#[repr(C)]` type or an `extern "C"` function is `omni-ffi`, which
//! keeps the core from becoming FFI-shaped by accident.
//!
//! ## Design in three sentences
//!
//! Values are eight-byte NaN boxes, so scalars are copied rather than allocated
//! and there is nothing for a collector to trace. Compound values live in a
//! chunked bump [`Arena`] that is reclaimed in bulk when the [`Engine`] resets
//! it, which is deterministic rather than scheduled. The cost of both choices
//! is that no object graph outlives its arena, so nothing here may hand a
//! pointer across a foreign boundary.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod arena;
pub mod engine;
pub mod lexer;
pub mod value;

pub use arena::Arena;
pub use engine::{Engine, ErrorKind, OmniError};
pub use lexer::{BinOp, LexError, Token, TokenKind, lex};
pub use value::Value;

/// OmniEngine's language semantics version, not the crate's. Bumped when a
/// `.omni` program that used to evaluate would evaluate differently.
pub const LANGUAGE_VERSION: u32 = 1;
