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
//! chunked bump [`arena::Arena`], which the [`Engine`] rewinds or resets rather
//! than freeing one object at a time, so reclamation is deterministic and costs
//! one `dealloc` per block. The one rule that follows is that a pointer must not
//! outlive the block that owns it — which is why [`crate::engine`] promotes every
//! string result into a block that rewinds instead of freeing, and why
//! [`crate::object`] is the only module that reads a box's address.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod arena;
pub mod engine;
pub mod lexer;
pub mod object;
pub mod value;

/// Test-only: wraps `System` and tallies live bytes per thread, so a test can
/// prove the engine leaves nothing on the general heap. See the module doc.
#[cfg(test)]
pub mod counted;

#[cfg(test)]
#[global_allocator]
static COUNTED: counted::Counting = counted::Counting;

pub use arena::Arena;
pub use engine::{
    Engine, ErrorKind, HostCall, HostError, HostOutcome, MAX_ARGS, MAX_CALL_DEPTH,
    MAX_IDENTIFIER_LEN, OmniError, Stats,
};
pub use lexer::{BinOp, LexError, Token, TokenKind, lex};
pub use object::{MAX_STRING_LEN, StrRef};
pub use value::Value;

/// OmniEngine's language semantics version, not the crate's. Bumped when a
/// `.omni` program that used to evaluate would evaluate differently.
///
/// Phase 2 (strings, `==`, `!=`) does not bump it: no Phase 1 program changes
/// meaning, because `==` and string literals did not parse at all and `+` on two
/// numbers is unchanged.
pub const LANGUAGE_VERSION: u32 = 1;
