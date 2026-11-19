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
pub mod ast;
pub mod bytecode;
pub mod engine;
/// Hidden entry points for fuzz binaries. Not part of the supported API.
#[doc(hidden)]
pub mod fuzz_api;
pub mod json;
pub mod lexer;
pub mod object;
pub mod optimizer;
pub mod update;
pub mod value;

/// Test-only: wraps `System` and tallies live bytes per thread, so a test can
/// prove the engine leaves nothing on the general heap. See the module doc.
#[cfg(test)]
pub mod counted;

#[cfg(test)]
#[global_allocator]
static COUNTED: counted::Counting = counted::Counting;

pub use arena::{Arena, Mark};
pub use bytecode::{OPCODE_COUNT, opcode_name};
pub use engine::{
    BatchItem, CPU_LIMIT_MESSAGE, DEFAULT_STEP_LIMIT, Engine, ErrorKind, HostCall, HostError,
    HostOutcome, MAX_ARGS, MAX_CALL_DEPTH, MAX_IDENTIFIER_LEN, MAX_MODULE_DEPTH, MAX_NESTING,
    MAX_STACK_DEPTH, MEMORY_LIMIT_MESSAGE, OmniError, Profile, Stats,
};
pub use lexer::{BinOp, LexError, MAX_SOURCE_LEN, Token, TokenKind, lex, lex_into};
pub use object::{MAX_STRING_LEN, StrRef};
pub use optimizer::OptimizeStats;
pub use value::Value;

/// OmniEngine's language semantics version, not the crate's. Bumped when a
/// `.omni` program that used to evaluate would evaluate differently.
///
/// Phase 2 (strings, `==`, `!=`) does not bump it: no Phase 1 program changes
/// meaning, because `==` and string literals did not parse at all and `+` on two
/// numbers is unchanged.
///
/// Phase 6 does, because `import` is now a keyword: a program that used
/// `import` as a variable name stops lexing. No other program that parsed at
/// version 1 changes meaning — the VM is an implementation of the same
/// semantics, not a revision of them.
pub const LANGUAGE_VERSION: u32 = 2;
