//! The engine: lex, lower to bytecode, execute on a stack VM.
//!
//! Phase 6 replaced the fused parse-and-evaluate pass with four stages:
//!
//! ```text
//! source ──lex──▶ tokens ──parse──▶ flat AST ──lower──▶ Chunk ──execute──▶ value
//! ```
//!
//! The tree and the chunk are owned by the engine and cleared rather than
//! reallocated, so a warm program never asks the system allocator for them.
//! `run` takes both out with `mem::take` for the duration of one execution and
//! puts them back on every path, success or error.
//!
//! The VM is a stack machine whose instructions are defined in
//! [`crate::bytecode`]. It calls the same arithmetic, equality, concatenation,
//! and host-call helpers the fused evaluator used, so the string lifetime rules
//! and the divide-by-zero rule stay stated once. `omni_exec` and
//! `exec_many` call `run` exactly as before: the ABI did not change.
//!
//! ## Memory model in this file
//!
//! Three blocks, three lifetimes — see `docs/superpowers/specs` for the reasoning:
//!
//! | block | holds | reclaimed |
//! | --- | --- | --- |
//! | [`Engine::arena`] | `let` cells and every string built during one evaluation | `reset()` per `exec`: bulk-freed, pointers die |
//! | [`Engine::egress`] | the promoted copy of the *result* string | `rewind()` per `exec`: cursor resets, nothing unmaps |
//! | a host `Box<[u8]>` per [`Engine::globals`] entry | strings injected by a host | only when that name is redefined or the engine drops |
//!
//! The pivot is promotion: [`Engine::exec`] copies a string result out of the
//! arena and into egress before returning, so the `Value` a caller receives — and
//! the one kept in [`Engine::last_value`] — addresses the rewound block and never
//! the bulk-freed one. That is what makes "valid until your next `exec`" a
//! documentable promise instead of a use-after-free, and the failure mode when a
//! host ignores it is a stale read rather than a crash.
//!
//! Reading a string's bytes is the one dereference in the crate, so it lives in
//! [`crate::object`]; [`Engine::string_of`] is the safe door to it, and it is safe
//! because taking `&self` means no `exec` can run while the bytes are borrowed.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::Value;
use crate::arena::{Arena, Slot};
use crate::ast::Ast;
use crate::bytecode::{Callable, Chunk, Instr, OpCode};
use crate::json;
use crate::lexer::{BinOp, LexError, Token, lex_into};
use crate::object::{self, MAX_STRING_LEN, StrRef};

/// Longest identifier the arena binding cell can hold inline. A bump allocator
/// wants fixed-size cells, so the limit is stated as a language rule rather than
/// emerging from a capacity accident.
pub const MAX_IDENTIFIER_LEN: usize = 32;

/// Arguments one call may pass, and the widest argument buffer the FFI has to
/// build. Sixteen is generous for a host function and small enough that the
/// boundary can use a stack array instead of allocating per call.
pub const MAX_ARGS: usize = 16;

/// How deep calls may nest inside one evaluation.
///
/// The evaluator is recursive descent, so `f(g(h(…)))` consumes native stack per
/// level. The limit turns exhaustion into an error that says what happened. It is
/// counted per engine and reset with each execution, because an aborted
/// evaluation never runs its unwinding.
pub const MAX_CALL_DEPTH: u32 = 64;

/// How deep `import` may nest before the loader refuses. Modules on disk are
/// bounded by the filesystem anyway; the limit turns a runaway chain of imports
/// into a named error rather than a walk that never finishes.
pub const MAX_MODULE_DEPTH: usize = 16;

/// The category of a failure, which the FFI maps onto status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Source text the lexer could not read.
    Lex,
    /// Well-formed tokens in a shape the grammar rejects, including a bad escape.
    Parse,
    /// A program that parsed but could not be carried out.
    Runtime,
    /// An internal invariant broke. Reserved; hosts see it as a caught panic.
    Panic,
    /// A host function reported its own failure. Distinct from [`ErrorKind::Runtime`]
    /// so a caller can tell "my callback threw" from "the program divided by zero",
    /// and so an SDK's test suite cannot mistake one for the other.
    Host,
}

/// A one-based line and byte column in the source a program came from.
///
/// The column is a byte offset, not a character count: the lexer indexes bytes,
/// and a token's `start` is the same number, so nothing has to decode UTF-8 to
/// report where an error was. A line whose text is ASCII before the error reads
/// the same either way; one with a multi-byte character earlier on it reports
/// the byte the reader's editor moves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

impl Pos {
    pub(crate) fn new(line: u32, column: u32) -> Pos {
        Pos { line, column }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmniError {
    pub kind: ErrorKind,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

impl OmniError {
    pub(crate) fn new(kind: ErrorKind, at: Pos, message: impl Into<String>) -> OmniError {
        OmniError {
            kind,
            line: at.line,
            column: at.column,
            message: message.into(),
        }
    }
}

impl fmt::Display for OmniError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[Line {}, Col {}]: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for OmniError {}

impl From<LexError> for OmniError {
    fn from(e: LexError) -> OmniError {
        OmniError::new(ErrorKind::Lex, Pos::new(e.line, e.column), e.message)
    }
}

/// Why a host call was rejected. Every variant is the *caller's* fault, which is
/// what the FFI maps onto `OMNI_ERR_ARG` rather than a compile or runtime status;
/// a program that builds too long a string is a different failure entirely and
/// stays a runtime error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    /// A binding needs a name, and `""` is not one.
    EmptyName,
    /// Longer than [`MAX_IDENTIFIER_LEN`].
    NameTooLong,
    /// The payload is not UTF-8, so no host could read it back as text.
    NotUtf8,
    /// Longer than [`MAX_STRING_LEN`].
    TooLong,
    /// A host function declared more parameters than [`MAX_ARGS`] allows.
    ArityTooLarge,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::EmptyName => f.write_str("a binding needs a non-empty name"),
            HostError::NameTooLong => write!(
                f,
                "name is longer than the {MAX_IDENTIFIER_LEN}-character limit"
            ),
            HostError::NotUtf8 => f.write_str("string bytes are not valid UTF-8"),
            HostError::TooLong => write!(
                f,
                "string is longer than the {} MiB limit",
                MAX_STRING_LEN / (1024 * 1024)
            ),
            HostError::ArityTooLarge => {
                write!(f, "a host function takes at most {MAX_ARGS} arguments")
            }
        }
    }
}

impl std::error::Error for HostError {}

/// A `let` binding stored in the arena: fixed-size inline name plus the value.
#[derive(Debug, Clone, Copy)]
struct Binding {
    name: [u8; MAX_IDENTIFIER_LEN],
    len: u8,
    value: Value,
}

impl Binding {
    fn new(name: &str, value: Value) -> Option<Binding> {
        let bytes = name.as_bytes();
        if bytes.len() > MAX_IDENTIFIER_LEN {
            return None;
        }
        let mut buf = [0u8; MAX_IDENTIFIER_LEN];
        buf[..bytes.len()].copy_from_slice(bytes);
        // len is u8 and MAX_IDENTIFIER_LEN is 32, so the cast cannot truncate.
        Some(Binding {
            name: buf,
            len: bytes.len() as u8,
            value,
        })
    }

    fn name(&self) -> &str {
        std::str::from_utf8(&self.name[..self.len as usize]).unwrap_or("")
    }

    fn value(&self) -> Value {
        self.value
    }
}

/// One persistent host-injected binding.
///
/// The `owned` field exists so a value and the bytes it addresses can never
/// disagree: a string global carries its own allocation, and replacing the
/// binding replaces both at once. That pairing is what makes the FFI's ownership
/// check decidable — a superseded value addresses memory this engine no longer
/// claims, so copying it out returns 0 rather than reading what used to be there.
struct Global {
    name: String,
    value: Value,
    owned: Option<Box<[u8]>>,
}

/// One value on the VM stack.
///
/// The stack is a `Vec<Value>` owned by the engine. Clearing it per execution
/// keeps its capacity, and a value is eight bytes with nothing to drop, so the
/// push/pop traffic of a warm program never reaches the allocator.
type Stack = Vec<Value>;

/// What a host callback gives back.
///
/// Primitives only, never a [`Value`]. A `Value` is an address the *engine*
/// minted, and letting a boundary layer fabricate one would mean trusting the FFI
/// to box a pointer correctly — so the host returns bytes or a number and
/// [`Engine`] does its own boxing. That is the same rule [`Engine::define_string`]
/// already follows from the other side.
#[derive(Debug, Clone, PartialEq)]
pub enum HostOutcome {
    Number(f64),
    Bool(bool),
    Nil,
    Undefined,
    /// String payload, copied by the engine into the execution arena.
    String(Vec<u8>),
    /// The host reported failure. The payload is its message, possibly empty.
    Failure(Vec<u8>),
}

/// A host callback as the core sees it: read the engine, read the arguments,
/// hand back a primitive.
///
/// `&Engine` rather than `&mut Engine` is the whole contract. A callback may look
/// up a string's bytes because they are still on hand, and it may not allocate,
/// bind, or reset — the engine is busy running the very call that invoked it.
pub type HostCall = dyn Fn(&Engine, &[Value]) -> HostOutcome + Send;

/// One registered host function: a name, a fixed arity, and the callback.
struct HostFunction {
    name: String,
    arity: u32,
    call: Box<HostCall>,
}

/// One result of [`Engine::exec_many`]: what the item produced, or why it did
/// not.
///
/// A struct rather than a `Result` so the batch can be walked as data — the FFI
/// writes one `OmniValue` and one status per item into caller-owned arrays, and
/// an `Option<OmniError>` maps onto that without a second pass.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchItem {
    /// The promoted value, or [`Value::UNDEF`] when the item failed. A string
    /// here lives in the egress block and is readable until the next execution.
    pub value: Value,
    pub error: Option<OmniError>,
}

impl BatchItem {
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }

    pub fn into_result(self) -> Result<Value, OmniError> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.value),
        }
    }
}

/// Counters a host reads to prove the memory story from outside Rust.
///
/// Fixed-width and `Copy` because the FFI hands it across the boundary as a
/// `#[repr(C)]` struct; the field order is the wire order and is pinned there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Executions started, successful or not: one per `exec`, one per batch item.
    pub execs: u64,
    /// Wall-clock microseconds the most recent execution spent in [`Engine::run`],
    /// including lexing. A failed execution counts too: latency is latency.
    ///
    /// `u32` because the C ABI spends the four padding bytes after its `size`
    /// field on this counter; 4,294 seconds is far past any single program, and
    /// the value saturates rather than wrapping.
    pub last_exec_micros: u32,
    /// String blocks built, cumulative. The one counter that is *supposed* to grow.
    pub strings_built: u64,
    pub arena_bytes_in_use: u64,
    pub arena_bytes_reserved: u64,
    pub arena_chunks: u64,
    pub egress_bytes_in_use: u64,
    pub egress_bytes_reserved: u64,
    /// The largest egress demand between two rewinds — one execution, or one
    /// whole batch. This is the bound on a rewinding block.
    pub egress_high_water: u64,
}

/// An OmniEngine execution context.
///
/// Owns its identifier strings so it carries no lifetime: a borrowed engine
/// could not outlive the call that supplied its source, which would make an
/// opaque FFI handle unsound.
pub struct Engine {
    arena: Arena,
    egress: Arena,
    /// The token buffer, refilled by every execution and never freed while the
    /// engine lives. Its capacity is the high-water mark of the largest program
    /// this engine has run, which is the same bounded-reuse story the egress
    /// block tells: a warm engine lexes without touching the system allocator.
    tokens: Vec<Token>,
    /// The flat tree, cleared and refilled per execution. Its node capacity is
    /// the high-water mark of the largest program this engine has compiled.
    ast: Ast,
    /// The lowered instructions and identifier bytes, reused the same way.
    chunk: Chunk,
    /// Operand stack for the VM, cleared per execution.
    stack: Stack,
    /// Lowering work stack. The compiler walks this instead of the native call
    /// stack, so a program whose expression spine is thousands deep cannot
    /// overflow the thread it runs on. Reused like the other buffers.
    tasks: Vec<crate::bytecode::Task>,
    /// Gathering space for one concatenation or JSON string, reused like the
    /// token buffer. `string_of` borrows the engine and `new_string` needs it
    /// mutably, so the two slices have to be gathered somewhere before the
    /// arena is touched; keeping it means a warm engine concatenates without
    /// asking the system allocator for a buffer per `+`.
    scratch: Vec<u8>,
    /// Arena-resident `let` bindings for the current execution.
    bindings: Vec<Slot<Binding>>,
    /// Persistent host-injected bindings.
    globals: Vec<Global>,
    /// Host functions, addressed by name at call sites.
    ///
    /// A fourth block with a fourth lifetime: it survives every execution and is
    /// released only by `unregister_host_fn` or the engine's own drop. That is
    /// what makes `arena.reset()` safe to run while a call is on the stack — the
    /// callback's own identity is not in the arena.
    host_fns: Vec<HostFunction>,
    /// Live `parse_call` frames, guarded against [`MAX_CALL_DEPTH`]. Kept as
    /// engine state because the fused parser owned it here; the AST parser uses
    /// its own counter, so this is always zero between executions.
    call_depth: u32,
    /// The directory `import` may read from. `None` means imports are disabled.
    module_root: Option<PathBuf>,
    /// Canonical paths of modules currently loading, innermost last. Cycles are
    /// detected against this stack; it is empty between top-level executions.
    loading_modules: Vec<PathBuf>,
    /// Namespaces imported during the current execution, for a diagnostic that
    /// names the module rather than the flattened `ns.name`.
    namespaces: Vec<String>,
    /// Exports of the modules imported this execution, keyed by `ns.name`.
    ///
    /// These do not live in [`Binding`] cells because a flattened name can be
    /// twice [`MAX_IDENTIFIER_LEN`] plus a dot, and widening every `let` cell for
    /// a case only imports create would trade the common path's memory for the
    /// rare one. The table is cleared with the bindings it shadows.
    module_bindings: Vec<(String, Value)>,
    /// Directories `fs.read_text` may read. Empty means everything is denied.
    fs_read_roots: Vec<PathBuf>,
    /// Directories `fs.write_text` may write. Empty means everything is denied.
    fs_write_roots: Vec<PathBuf>,
    last_value: Value,
    last_error: Option<OmniError>,
    execs: u64,
    last_exec_micros: u32,
    strings_built: u64,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            arena: Arena::new(),
            egress: Arena::new(),
            tokens: Vec::new(),
            ast: Ast::new(),
            chunk: Chunk::new(),
            stack: Vec::new(),
            tasks: Vec::new(),
            scratch: Vec::new(),
            bindings: Vec::new(),
            globals: Vec::new(),
            host_fns: Vec::new(),
            call_depth: 0,
            module_root: None,
            loading_modules: Vec::new(),
            namespaces: Vec::new(),
            module_bindings: Vec::new(),
            fs_read_roots: Vec::new(),
            fs_write_roots: Vec::new(),
            last_value: Value::UNDEF,
            last_error: None,
            execs: 0,
            last_exec_micros: 0,
            strings_built: 0,
        }
    }

    /// Run a program, returning the value of its last statement.
    ///
    /// A string result is promoted to the egress block on the way out, so what a
    /// caller holds after this returns is addressed in memory that `exec` rewinds
    /// rather than frees. Everything else this function hands back is eight bytes
    /// with no memory behind it.
    pub fn exec(&mut self, source: &str) -> Result<Value, OmniError> {
        self.reclaim();
        self.execs += 1;
        let started = Instant::now();
        let result = self.run(source);
        self.last_exec_micros = elapsed_micros(started.elapsed());
        match result {
            Ok(v) => {
                let v = self.promote(v);
                self.last_value = v;
                self.last_error = None;
                Ok(v)
            }
            Err(e) => {
                self.last_error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Run several programs in one call, sharing one arena reclamation and one
    /// egress rewind between them.
    ///
    /// The point is amortisation, not convenience: a host crossing the FFI once
    /// per expression pays for the crossing and for the reset every time, and a
    /// batch pays for both once. Each item still gets the arena back to where the
    /// batch started, so item *N* cannot see item *N-1*'s `let` bindings — a batch
    /// is a sequence of independent executions, not one long program.
    ///
    /// The egress block rewinds **once, before the first item**. Every string
    /// result is promoted into it and stays readable until the next `exec` or
    /// `exec_many`, so all *N* results can be copied out after the call returns.
    /// That is the trade the batch makes: reserved egress bytes are bounded by
    /// one batch's total demand rather than by one execution's.
    ///
    /// A failing item does not stop the batch. Every element of `sources` gets an
    /// element in the result, in order, holding either a value or an error.
    /// `last_value` and `last_error` end up exactly as if the final item had been
    /// passed to [`Engine::exec`] alone.
    ///
    /// An empty slice is a no-op that reclaims nothing, so a host polling an empty
    /// queue does not expire the results it is still holding.
    pub fn exec_many(&mut self, sources: &[&str]) -> Vec<BatchItem> {
        if sources.is_empty() {
            return Vec::new();
        }
        self.reclaim();
        let mark = self.arena.mark();
        let mut items = Vec::with_capacity(sources.len());
        for source in sources {
            self.arena.reset_to(mark);
            self.bindings.clear();
            self.module_bindings.clear();
            self.namespaces.clear();
            self.call_depth = 0;
            self.execs += 1;
            let started = Instant::now();
            let result = self.run(source);
            self.last_exec_micros = elapsed_micros(started.elapsed());
            items.push(match result {
                Ok(v) => {
                    let v = self.promote(v);
                    self.last_value = v;
                    self.last_error = None;
                    BatchItem {
                        value: v,
                        error: None,
                    }
                }
                Err(e) => {
                    self.last_error = Some(e.clone());
                    BatchItem {
                        value: Value::UNDEF,
                        error: Some(e),
                    }
                }
            });
        }
        items
    }

    /// Give back everything the previous execution claimed.
    ///
    /// Clears the arena and the binding list together: that pairing is the
    /// invariant [`Slot`] relies on, so neither may happen without the other. The
    /// egress block rewinds rather than resets, because pointers into it are the
    /// ones a host is allowed to still be holding.
    ///
    /// Shared by `exec` and `exec_many` so there is one place that decides what
    /// survives a crossing and one place that can get it wrong.
    fn reclaim(&mut self) {
        self.arena.reset();
        self.bindings.clear();
        self.egress.rewind();
        self.namespaces.clear();
        self.module_bindings.clear();
        // An error mid-call returns through `?` and skips the decrement, so the
        // count is re-zeroed here rather than trusted to unwind cleanly.
        self.call_depth = 0;
    }

    /// Copy an arena-resident string result into the egress block.
    ///
    /// Three cases pass through untouched: not a string (nothing to promote), a
    /// host string (already outside every block that resets), and a string already
    /// in egress (which cannot happen mid-`exec`, but is cheap to be sure of).
    fn promote(&mut self, value: Value) -> Value {
        let Some(bytes) = StrRef::checked(value, &self.arena).map(|r| r.as_bytes()) else {
            return value;
        };
        // Safety-free copy: `build` takes `&mut self.egress` while `bytes` borrows
        // `self.arena`, and those are different fields, so the borrow checker
        // allows exactly the aliasing this needs and none more.
        let p = object::build(&mut self.egress, bytes);
        self.strings_built += 1;
        Value::from_ptr(p)
    }

    /// The most recent failure, if the last `exec` failed.
    pub fn last_error(&self) -> Option<&OmniError> {
        self.last_error.as_ref()
    }

    /// The value produced by the last successful `exec`.
    pub fn last_value(&self) -> Value {
        self.last_value
    }

    /// Read-only view of the execution arena, for accounting and tests.
    pub fn arena(&self) -> &Arena {
        &self.arena
    }

    /// Read-only view of the host-facing egress block.
    pub fn egress(&self) -> &Arena {
        &self.egress
    }

    /// Inject a binding that survives across executions. This is how a host
    /// exposes its own functions and constants to scripts.
    ///
    /// Redefining a name replaces the binding rather than shadowing it, so a host
    /// that defines the same constant in a loop does not grow a vector forever —
    /// and releasing the old allocation here is what turns the stale value a host
    /// might still hold into an unreadable one, which is the safe answer.
    pub fn define(&mut self, name: &str, value: Value) {
        self.set_global(name, value, None);
    }

    /// Inject a string a host owns, readable by scripts as `name`.
    ///
    /// The bytes are copied, so the caller's buffer may be freed or reused the
    /// moment this returns. Rejects a bad name, a non-UTF-8 payload, and an
    /// over-long string — all of them the host's mistake, none of them a program's.
    pub fn define_string(&mut self, name: &str, bytes: &[u8]) -> Result<(), HostError> {
        if name.is_empty() {
            return Err(HostError::EmptyName);
        }
        if name.len() > MAX_IDENTIFIER_LEN {
            return Err(HostError::NameTooLong);
        }
        if bytes.len() > MAX_STRING_LEN {
            return Err(HostError::TooLong);
        }
        std::str::from_utf8(bytes).map_err(|_| HostError::NotUtf8)?;
        let owned = object::build_boxed(bytes);
        // The box's own address is the value, so the binding and its bytes are
        // installed by the same write and cannot be seen apart.
        let value = Value::from_ptr(owned.as_ptr());
        self.set_global(name, value, Some(owned));
        self.strings_built += 1;
        Ok(())
    }

    fn set_global(&mut self, name: &str, value: Value, owned: Option<Box<[u8]>>) {
        match self.globals.iter_mut().find(|g| g.name == name) {
            Some(existing) => {
                existing.value = value;
                existing.owned = owned;
            }
            None => self.globals.push(Global {
                name: name.to_string(),
                value,
                owned,
            }),
        }
    }

    /// Register a function a host owns, callable from scripts as `name(...)`.
    ///
    /// Redefining a name replaces the callback, mirroring [`Engine::define`] — a
    /// host that registers in a setup loop must not grow a vector forever. The
    /// engine keeps only the `Box<dyn>`; whatever C address it wraps stays the
    /// caller's problem, which is why [`omni-ffi`] refuses to release it and the
    /// SDKs own the unregister path.
    ///
    /// A registered name and a global of the same name can coexist: `foo` reads
    /// the global and `foo(1)` calls the function, because position decides.
    pub fn register_host_fn<F>(&mut self, name: &str, arity: u32, call: F) -> Result<(), HostError>
    where
        F: Fn(&Engine, &[Value]) -> HostOutcome + Send + 'static,
    {
        if name.is_empty() {
            return Err(HostError::EmptyName);
        }
        if name.len() > MAX_IDENTIFIER_LEN {
            return Err(HostError::NameTooLong);
        }
        if arity as usize > MAX_ARGS {
            return Err(HostError::ArityTooLarge);
        }
        let entry = HostFunction {
            name: name.to_string(),
            arity,
            call: Box::new(call),
        };
        match self.host_fns.iter_mut().find(|h| h.name == name) {
            Some(existing) => *existing = entry,
            None => self.host_fns.push(entry),
        }
        Ok(())
    }

    /// Drop the host function called `name`, reporting whether one was there.
    pub fn unregister_host_fn(&mut self, name: &str) -> bool {
        let before = self.host_fns.len();
        self.host_fns.retain(|h| h.name != name);
        self.host_fns.len() != before
    }

    /// How many host functions are registered.
    pub fn host_fn_count(&self) -> usize {
        self.host_fns.len()
    }

    fn find_host_fn(&self, name: &str) -> Option<usize> {
        self.host_fns.iter().rposition(|h| h.name == name)
    }

    /// The language's own equality, which `Value`'s `PartialEq` cannot implement:
    /// strings compare by content, and that needs the engine's proof that the
    /// bytes are still on hand.
    ///
    /// Across types this is `false` — including `undefined == null`, because they
    /// are different values and pretending otherwise is someone else's mistake.
    /// Numbers keep IEEE rules, so `NaN == NaN` stays false and `0.0 == -0.0`
    /// stays true. A string box whose bytes cannot be read equals nothing, since
    /// the alternative is inventing a comparison against garbage.
    pub fn eq_values(&self, lhs: Value, rhs: Value) -> bool {
        if lhs.is_string() && rhs.is_string() {
            return match (self.string_of(lhs), self.string_of(rhs)) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
        }
        lhs == rhs
    }

    /// The language's truthiness, complete: scalars as `Value::truthy` defines
    /// them, plus the string rule that lives behind an address.
    ///
    /// A non-empty string is true, an empty one false. A box whose bytes cannot be
    /// read is false rather than a panic — a stale pointer is a wrong answer, and
    /// a wrong answer from a diagnostic path beats a crash in one.
    pub fn truthy(&self, value: Value) -> bool {
        if value.is_string() {
            return self.string_of(value).is_some_and(|b| !b.is_empty());
        }
        value.truthy()
    }

    /// The bytes of a string value, wherever this engine keeps them: the
    /// execution arena, the egress block, or a host binding's allocation.
    ///
    /// `None` for a non-string and for a string this engine cannot prove it owns,
    /// which is the whole expiry rule expressed as a type.
    pub fn string_of(&self, value: Value) -> Option<&[u8]> {
        if let Some(r) = StrRef::checked(value, &self.arena) {
            return Some(r.as_bytes());
        }
        if let Some(r) = StrRef::checked(value, &self.egress) {
            return Some(r.as_bytes());
        }
        let addr = value.as_ptr()? as usize;
        self.globals
            .iter()
            .find_map(|g| g.owned.as_deref().and_then(|b| object::read_boxed(b, addr)))
    }

    /// Text a program or a diagnostic should show: the *content* of a string,
    /// unquoted, and the ordinary rendering of everything else.
    ///
    /// Invalid UTF-8 cannot come out of a string this engine built, so the lossy
    /// conversion is a replacement for a panic that would otherwise fire inside
    /// error reporting.
    pub fn render(&self, value: Value) -> String {
        match self.string_of(value) {
            Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            None => value.to_string(),
        }
    }

    /// The same value the way a *literal* is written, quotes and escapes intact,
    /// which is what a type error needs to point at: `"hi" + 42` is unambiguous
    /// where `hi + 42` is not.
    fn show(&self, value: Value) -> String {
        match self.string_of(value) {
            Some(bytes) => object::quote(bytes),
            None => value.to_string(),
        }
    }

    /// Counters for `omni_debug_stats` and for the tests that use the claim.
    pub fn stats(&self) -> Stats {
        let u64_from = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        Stats {
            execs: self.execs,
            last_exec_micros: self.last_exec_micros,
            strings_built: self.strings_built,
            arena_bytes_in_use: u64_from(self.arena.bytes_in_use()),
            arena_bytes_reserved: u64_from(self.arena.bytes_reserved()),
            arena_chunks: u64_from(self.arena.chunk_count()),
            egress_bytes_in_use: u64_from(self.egress.bytes_in_use()),
            egress_bytes_reserved: u64_from(self.egress.bytes_reserved()),
            egress_high_water: u64_from(self.egress.high_water()),
        }
    }

    /// Lex, parse, lower, and run, handing every owned buffer back afterwards.
    ///
    /// The five buffers are taken out of `self` so the parser and the lowering
    /// pass can borrow the engine immutably — they need to know which call
    /// names resolve — while `&mut self` runs the VM. They go back on the error
    /// path too: a bad program must not cost the next good one a reallocation.
    fn run(&mut self, source: &str) -> Result<Value, OmniError> {
        let mut tokens = std::mem::take(&mut self.tokens);
        let mut ast = std::mem::take(&mut self.ast);
        let mut chunk = std::mem::take(&mut self.chunk);
        let mut stack = std::mem::take(&mut self.stack);
        let mut tasks = std::mem::take(&mut self.tasks);

        let result = (|| -> Result<Value, OmniError> {
            lex_into(source, &mut tokens)?;
            ast.parse_into(source, &tokens)?;
            crate::bytecode::compile(&ast, source, &*self, &mut chunk, &mut tasks)?;
            self.run_chunk(&chunk, source, &mut stack)
        })();

        self.tokens = tokens;
        self.ast = ast;
        self.chunk = chunk;
        self.stack = stack;
        self.tasks = tasks;
        result
    }

    /// Execute one chunk. The stack is the engine's, cleared but not freed.
    fn run_chunk(
        &mut self,
        chunk: &Chunk,
        source: &str,
        stack: &mut Stack,
    ) -> Result<Value, OmniError> {
        stack.clear();
        let mut ip = 0usize;
        while ip < chunk.code.len() {
            let instr = chunk.code[ip];
            let at = instr.pos();
            ip += 1;
            match instr.op {
                OpCode::PushNumber => {
                    let text = literal_span(source, instr);
                    let number = text.parse::<f64>().map_err(|_| {
                        OmniError::new(
                            ErrorKind::Parse,
                            at,
                            format!("'{text}' is not a valid number"),
                        )
                    })?;
                    stack.push(Value::from_f64(number));
                }
                OpCode::PushStr => {
                    let bytes = decode_literal(literal_span(source, instr), at)?;
                    let value = self.new_string(&bytes, at, ErrorKind::Runtime)?;
                    stack.push(value);
                }
                OpCode::PushNil => stack.push(Value::NIL),
                OpCode::PushUndef => stack.push(Value::UNDEF),
                OpCode::Load => {
                    let name = chunk.name(instr);
                    match self.lookup(name) {
                        Some(value) => stack.push(value),
                        None => return Err(self.undefined_name(name, at)),
                    }
                }
                OpCode::Store => {
                    let value = top(stack, at)?;
                    let name = chunk.name(instr);
                    self.bind(name, value, at)?;
                }
                OpCode::Pop => {
                    pop(stack, at)?;
                }
                OpCode::Add => self.binary(stack, BinOp::Add, at)?,
                OpCode::Sub => self.binary(stack, BinOp::Sub, at)?,
                OpCode::Mul => self.binary(stack, BinOp::Mul, at)?,
                OpCode::Div => self.binary(stack, BinOp::Div, at)?,
                OpCode::Mod => self.binary(stack, BinOp::Mod, at)?,
                OpCode::Eq => {
                    let rhs = pop(stack, at)?;
                    let lhs = pop(stack, at)?;
                    stack.push(Value::bool(self.eq_values(lhs, rhs)));
                }
                OpCode::Ne => {
                    let rhs = pop(stack, at)?;
                    let lhs = pop(stack, at)?;
                    stack.push(Value::bool(!self.eq_values(lhs, rhs)));
                }
                OpCode::Neg => {
                    let value = pop(stack, at)?;
                    let number = value.as_f64().ok_or_else(|| {
                        OmniError::new(
                            ErrorKind::Runtime,
                            at,
                            format!(
                                "type mismatch: cannot apply unary '-' to {}: {}",
                                kind_of(value),
                                self.show(value)
                            ),
                        )
                    })?;
                    stack.push(Value::from_f64(-number));
                }
                OpCode::Jump => {
                    ip = instr.a as usize;
                }
                OpCode::JumpIfFalse => {
                    let value = pop(stack, at)?;
                    if !self.truthy(value) {
                        ip = instr.a as usize;
                    }
                }
                OpCode::Call => {
                    let name = chunk.name(instr);
                    let count = instr.c as usize;
                    let start = stack.len().checked_sub(count).ok_or_else(|| {
                        OmniError::new(ErrorKind::Panic, at, "the VM stack underflowed in a call")
                    })?;
                    let Some((callable, arity)) = self.resolve_call(name) else {
                        return Err(OmniError::new(
                            ErrorKind::Runtime,
                            at,
                            format!(
                                "unknown function '{name}' \
                                 (no host function or builtin has that name)"
                            ),
                        ));
                    };
                    if count != arity {
                        return Err(OmniError::new(
                            ErrorKind::Runtime,
                            at,
                            format!(
                                "Mismatched arity in call '{name}': takes {arity} argument{}, got {count}",
                                if arity == 1 { "" } else { "s" }
                            ),
                        ));
                    }
                    let result = match callable {
                        Callable::Host(index) => self.invoke_host(index, name, &stack[start..], at),
                        Callable::Builtin(which) => {
                            self.invoke_builtin(which, name, &stack[start..], at)
                        }
                    };
                    let value = result?;
                    stack.truncate(start);
                    stack.push(value);
                }
                OpCode::Ret => return Ok(pop(stack, at).unwrap_or(Value::UNDEF)),
                OpCode::Import => {
                    let bytes = decode_literal(literal_span(source, instr), at)?;
                    let request = std::str::from_utf8(&bytes).map_err(|_| {
                        OmniError::new(ErrorKind::Runtime, at, "import needs a UTF-8 module name")
                    })?;
                    self.import_module(request, at)?;
                    stack.push(Value::UNDEF);
                }
            }
        }
        // A chunk with no `Ret` — hand-built by a test or a future tool — ends
        // with its top of stack as the result.
        Ok(stack.pop().unwrap_or(Value::UNDEF))
    }

    fn binary(&mut self, stack: &mut Stack, op: BinOp, at: Pos) -> Result<(), OmniError> {
        let rhs = pop(stack, at)?;
        let lhs = pop(stack, at)?;
        let value = self.arith(lhs, op, rhs, at)?;
        stack.push(value);
        Ok(())
    }

    /// The error for a name the VM could not load.
    ///
    /// A dotted name whose prefix is a namespace imported this execution gets a
    /// message about visibility rather than a flattened variable name, because
    /// that is the mistake the reader made: the module exists, the export does
    /// not.
    fn undefined_name(&self, name: &str, at: Pos) -> OmniError {
        if let Some(dot) = name.find('.') {
            let base = &name[..dot];
            let member = &name[dot + 1..];
            if self.namespaces.iter().any(|namespace| namespace == base) {
                return OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!("module '{base}' has no public export '{member}'"),
                );
            }
        }
        OmniError::new(
            ErrorKind::Runtime,
            at,
            format!("undefined variable '{name}'"),
        )
    }

    fn resolve_call(&self, name: &str) -> Option<(Callable, usize)> {
        if let Some(index) = self.find_host_fn(name) {
            return Some((Callable::Host(index), self.host_fns[index].arity as usize));
        }
        builtin_for(name).map(|(which, arity)| (Callable::Builtin(which), arity))
    }

    /// Does a call name resolve? The lowering pass asks before it walks the
    /// arguments, matching the fused parser's error order.
    pub(crate) fn is_callable(&self, name: &str) -> bool {
        self.find_host_fn(name).is_some() || builtin_for(name).is_some()
    }

    /// Allocate the binding's cell in the arena. A name longer than the fixed
    /// inline field is a language error, never a silent truncation — two
    /// different identifiers comparing equal would be far worse than a limit.
    fn bind(&mut self, name: &str, value: Value, at: Pos) -> Result<(), OmniError> {
        let binding = Binding::new(name, value).ok_or_else(|| {
            OmniError::new(
                ErrorKind::Parse,
                at,
                format!(
                    "identifier is longer than the {MAX_IDENTIFIER_LEN}-character limit \
                     ({} characters)",
                    name.len()
                ),
            )
        })?;
        self.bindings.push(Slot::new(&mut self.arena, binding));
        Ok(())
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        // Later bindings shadow earlier ones, and script bindings shadow both
        // module exports and host ones.
        self.bindings
            .iter()
            .rev()
            .map(|s| s.get())
            .find(|b| b.name() == name)
            .map(|b| b.value())
            .or_else(|| {
                self.module_bindings
                    .iter()
                    .rev()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| *value)
            })
            .or_else(|| {
                self.globals
                    .iter()
                    .rev()
                    .find(|g| g.name == name)
                    .map(|g| g.value)
            })
    }

    // -- modules --------------------------------------------------------------

    /// Allow `import` to resolve modules under `root`.
    ///
    /// The root is canonicalized once, here, so every later containment check
    /// compares a canonical path to a canonical root. A host that passes a
    /// relative path and one that passes an absolute path to the same directory
    /// get the same permission.
    pub fn set_module_root(&mut self, root: impl AsRef<Path>) -> std::io::Result<()> {
        self.module_root = Some(root.as_ref().canonicalize()?);
        Ok(())
    }

    /// Turn imports off. The loader fails closed, so this restores the default.
    pub fn clear_module_root(&mut self) {
        self.module_root = None;
    }

    /// Load one module and bind its public exports as `namespace.name`.
    ///
    /// The module runs in a fresh child engine. That is the isolation rule: a
    /// module gets the builtins and the parent's filesystem permissions, but not
    /// the parent's host globals or callbacks, and its bindings reach the parent
    /// only through the export copy below.
    fn import_module(&mut self, request: &str, at: Pos) -> Result<(), OmniError> {
        if request.is_empty() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                "import needs a module name",
            ));
        }
        let Some(root) = self.module_root.clone() else {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                "import is disabled: this engine has no module root",
            ));
        };
        if self.loading_modules.len() >= MAX_MODULE_DEPTH {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("imports are nested {MAX_MODULE_DEPTH} deep, the limit"),
            ));
        }
        let mut relative = PathBuf::from(request);
        if relative.extension().is_none() {
            relative.set_extension("omni");
        }
        let path = root.join(&relative).canonicalize().map_err(|_| {
            OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("cannot import '{request}': no such module under the module root"),
            )
        })?;
        // One containment check covers absolute requests, `..` escapes, and
        // symlinks that point out of the tree, because the target is canonical.
        if !path.starts_with(&root) {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("import '{request}' escapes the module root"),
            ));
        }
        if !path.is_file() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("cannot import '{request}': not a file"),
            ));
        }
        if let Some(start) = self.loading_modules.iter().position(|p| p == &path) {
            let mut chain: Vec<String> = self.loading_modules[start..]
                .iter()
                .filter_map(|p| module_stem(p))
                .collect();
            chain.push(module_stem(&path).unwrap_or_else(|| request.to_string()));
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("circular import: {}", chain.join(" -> ")),
            ));
        }
        let Some(namespace) = module_stem(&path) else {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("import '{request}' has no module name"),
            ));
        };
        if !is_identifier(&namespace) {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("module name '{namespace}' is not a valid identifier"),
            ));
        }
        let length = std::fs::metadata(&path)
            .map_err(|e| module_io_error(&namespace, &e, at))?
            .len();
        if length > MAX_STRING_LEN as u64 {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!(
                    "module '{namespace}' is {length} bytes, over the {} MiB limit",
                    MAX_STRING_LEN / (1024 * 1024)
                ),
            ));
        }
        let bytes = std::fs::read(&path).map_err(|e| module_io_error(&namespace, &e, at))?;
        let source = std::str::from_utf8(&bytes).map_err(|_| {
            OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("module '{namespace}' is not UTF-8 text"),
            )
        })?;

        let mut child = Engine::new();
        child.module_root = Some(root);
        child.fs_read_roots = self.fs_read_roots.clone();
        child.fs_write_roots = self.fs_write_roots.clone();
        child.loading_modules = self.loading_modules.clone();
        child.loading_modules.push(path);
        child.exec(source).map_err(|mut error| {
            error.message = format!("in module '{namespace}': {}", error.message);
            error
        })?;

        let exported: Vec<(String, Value)> = child
            .bindings
            .iter()
            .map(|slot| slot.get())
            .filter(|binding| !binding.name().starts_with('_') && !binding.name().contains('.'))
            .map(|binding| (binding.name().to_string(), binding.value()))
            .collect();
        for (name, value) in exported {
            let value = if value.is_string() {
                // The child's arena dies with it, so bytes cross by copy. This is
                // the one place a module can hand the parent memory, and it hands
                // over a parent-owned copy rather than an address into the child.
                let copied = match child.string_of(value) {
                    Some(bytes) => bytes.to_vec(),
                    None => return Err(unreadable_string(at, value)),
                };
                self.new_string(&copied, at, ErrorKind::Runtime)?
            } else {
                value
            };
            self.module_bindings
                .push((format!("{namespace}.{name}"), value));
        }
        self.namespaces.push(namespace);
        Ok(())
    }

    // -- sandboxed filesystem -------------------------------------------------

    /// Allow `fs.read_text` to read files under `root`, canonicalized now.
    pub fn allow_fs_read(&mut self, root: impl AsRef<Path>) -> std::io::Result<()> {
        let root = root.as_ref().canonicalize()?;
        if !self.fs_read_roots.contains(&root) {
            self.fs_read_roots.push(root);
        }
        Ok(())
    }

    /// Allow `fs.write_text` to write files under `root`, canonicalized now.
    pub fn allow_fs_write(&mut self, root: impl AsRef<Path>) -> std::io::Result<()> {
        let root = root.as_ref().canonicalize()?;
        if !self.fs_write_roots.contains(&root) {
            self.fs_write_roots.push(root);
        }
        Ok(())
    }

    fn read_allowed(&self, request: &str, at: Pos) -> Result<Vec<u8>, OmniError> {
        if self.fs_read_roots.is_empty() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                "fs.read_text is disabled: this engine has no filesystem read permission",
            ));
        }
        let target = Path::new(request).canonicalize().map_err(|e| {
            OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("cannot read '{request}': {e}"),
            )
        })?;
        if !self
            .fs_read_roots
            .iter()
            .any(|root| target.starts_with(root))
        {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("path '{request}' is outside the allowed read roots"),
            ));
        }
        if !target.is_file() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("cannot read '{request}': not a regular file"),
            ));
        }
        let length = std::fs::metadata(&target)
            .map_err(|e| {
                OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!("cannot read '{request}': {e}"),
                )
            })?
            .len();
        if length > MAX_STRING_LEN as u64 {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!(
                    "file '{request}' is {length} bytes, over the {} MiB limit",
                    MAX_STRING_LEN / (1024 * 1024)
                ),
            ));
        }
        let bytes = std::fs::read(&target).map_err(|e| {
            OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("cannot read '{request}': {e}"),
            )
        })?;
        if std::str::from_utf8(&bytes).is_err() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("file '{request}' is not UTF-8 text"),
            ));
        }
        Ok(bytes)
    }

    fn write_allowed(&self, request: &str, text: &[u8], at: Pos) -> Result<(), OmniError> {
        if self.fs_write_roots.is_empty() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                "fs.write_text is disabled: this engine has no filesystem write permission",
            ));
        }
        let target = Path::new(request);
        // A new file cannot be canonicalized, so its parent is the part that has
        // to be proven inside a root. An existing target is checked whole, which
        // also catches a symlinked file pointing out of the sandbox.
        let checked = match target.canonicalize() {
            Ok(existing) => existing,
            Err(_) => {
                let Some(name) = target.file_name() else {
                    return Err(OmniError::new(
                        ErrorKind::Runtime,
                        at,
                        format!("path '{request}' does not name a file"),
                    ));
                };
                let parent = target.parent().unwrap_or(Path::new("."));
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                parent
                    .canonicalize()
                    .map_err(|e| {
                        OmniError::new(
                            ErrorKind::Runtime,
                            at,
                            format!("cannot write '{request}': {e}"),
                        )
                    })?
                    .join(name)
            }
        };
        if !self
            .fs_write_roots
            .iter()
            .any(|root| checked.starts_with(root))
        {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("path '{request}' is outside the allowed write roots"),
            ));
        }
        std::fs::write(&checked, text).map_err(|e| {
            OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("cannot write '{request}': {e}"),
            )
        })
    }

    // -- JSON -----------------------------------------------------------------

    fn json_parse(&mut self, name: &str, args: &[Value], at: Pos) -> Result<Value, OmniError> {
        let scalar = {
            let bytes = self.string_arg(name, args, 0, at)?;
            let text = std::str::from_utf8(bytes).map_err(|_| {
                OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!("type mismatch: {name}() needs a UTF-8 string"),
                )
            })?;
            json::parse(text).map_err(|message| {
                OmniError::new(ErrorKind::Runtime, at, format!("{name}: {message}"))
            })?
        };
        match scalar {
            json::Scalar::Number(number) => Ok(Value::from_f64(number)),
            json::Scalar::Str(bytes) => self.new_string(&bytes, at, ErrorKind::Runtime),
            json::Scalar::Bool(flag) => Ok(Value::bool(flag)),
            json::Scalar::Null => Ok(Value::NIL),
        }
    }

    fn json_stringify(&mut self, name: &str, args: &[Value], at: Pos) -> Result<Value, OmniError> {
        let value = args[0];
        let mut buf = std::mem::take(&mut self.scratch);
        let result = (|| {
            buf.clear();
            if let Some(bytes) = self.string_of(value) {
                json::write_string(bytes, &mut buf);
            } else if let Some(flag) = value.as_bool() {
                buf.extend_from_slice(if flag { b"true" } else { b"false" });
            } else if let Some(number) = value.as_f64() {
                if !json::write_number(number, &mut buf) {
                    return Err(OmniError::new(
                        ErrorKind::Runtime,
                        at,
                        format!("{name}: {number} is not a finite JSON number"),
                    ));
                }
            } else if value == Value::NIL {
                buf.extend_from_slice(b"null");
            } else {
                return Err(OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!("{name}: undefined is not a JSON value"),
                ));
            }
            self.new_string(&buf, at, ErrorKind::Runtime)
        })();
        self.scratch = buf;
        result
    }

    // -- call helpers ---------------------------------------------------------

    /// Run one host callback and box its answer.
    ///
    /// The two borrows are both shared, which is the point: the callback may read
    /// through `&Engine` — that is how it gets a string argument's bytes — and may
    /// not touch anything here. Nothing mutable is aliased while foreign code runs,
    /// so a panic in the engine around the call unwinds only through Rust frames.
    fn invoke_host(
        &mut self,
        index: usize,
        name: &str,
        args: &[Value],
        at: Pos,
    ) -> Result<Value, OmniError> {
        let outcome = {
            let host = &self.host_fns[index];
            (host.call)(&*self, args)
        };
        match outcome {
            HostOutcome::Number(n) => Ok(Value::from_f64(n)),
            HostOutcome::Bool(b) => Ok(Value::bool(b)),
            HostOutcome::Nil => Ok(Value::NIL),
            HostOutcome::Undefined => Ok(Value::UNDEF),
            HostOutcome::String(bytes) => self.new_string(&bytes, at, ErrorKind::Runtime),
            HostOutcome::Failure(message) => {
                // The host's own text, if it sent any. A callback that fails with
                // nothing to say still produces a message that names the function,
                // because "host function failed" without the name cannot be acted on.
                let shown = String::from_utf8_lossy(&message);
                let detail = if shown.is_empty() {
                    String::new()
                } else {
                    format!(": {shown}")
                };
                Err(OmniError::new(
                    ErrorKind::Host,
                    at,
                    format!("host function '{name}' failed{detail}"),
                ))
            }
        }
    }

    /// Run one of the standard helpers.
    ///
    /// Every numeric builtin reads its arguments through the same door as `+`,
    /// so a program that passes a string to `abs` gets the same shaped message
    /// it gets from `"a" * 2`, pointing at the call site rather than at the
    /// argument's own expression. String helpers write into the engine's scratch
    /// buffer and hand it to `new_string`, which keeps the warm path allocation
    /// free in the same way `+` does.
    fn invoke_builtin(
        &mut self,
        which: Builtin,
        name: &str,
        args: &[Value],
        at: Pos,
    ) -> Result<Value, OmniError> {
        match which {
            Builtin::Abs => Ok(Value::from_f64(self.number_arg(name, args, 0, at)?.abs())),
            Builtin::Floor => Ok(Value::from_f64(self.number_arg(name, args, 0, at)?.floor())),
            Builtin::Ceil => Ok(Value::from_f64(self.number_arg(name, args, 0, at)?.ceil())),
            Builtin::Round => Ok(Value::from_f64(self.number_arg(name, args, 0, at)?.round())),
            Builtin::Pow => {
                let base = self.number_arg(name, args, 0, at)?;
                let exponent = self.number_arg(name, args, 1, at)?;
                let n = base.powf(exponent);
                // The engine rejects the infinities it can name (division by
                // zero) rather than passing them off as answers. `pow` is the
                // one builtin that can reach an infinity from finite inputs,
                // so it is the one that has to say so.
                if !n.is_finite() {
                    return Err(OmniError::new(
                        ErrorKind::Runtime,
                        at,
                        format!("{name}() has no finite result for {base} and {exponent}"),
                    ));
                }
                Ok(Value::from_f64(n))
            }
            Builtin::Sqrt => {
                let x = self.number_arg(name, args, 0, at)?;
                if x < 0.0 {
                    return Err(OmniError::new(
                        ErrorKind::Runtime,
                        at,
                        format!("{name}() needs a number that is not negative (got {x})"),
                    ));
                }
                Ok(Value::from_f64(x.sqrt()))
            }
            Builtin::Min => {
                let a = self.number_arg(name, args, 0, at)?;
                let b = self.number_arg(name, args, 1, at)?;
                Ok(Value::from_f64(a.min(b)))
            }
            Builtin::Max => {
                let a = self.number_arg(name, args, 0, at)?;
                let b = self.number_arg(name, args, 1, at)?;
                Ok(Value::from_f64(a.max(b)))
            }
            Builtin::Clamp => {
                let value = self.number_arg(name, args, 0, at)?;
                let low = self.number_arg(name, args, 1, at)?;
                let high = self.number_arg(name, args, 2, at)?;
                if low > high {
                    return Err(OmniError::new(
                        ErrorKind::Runtime,
                        at,
                        format!(
                            "clamp() needs a lower bound that is not above the upper one \
                             (got {low} and {high})"
                        ),
                    ));
                }
                Ok(Value::from_f64(value.max(low).min(high)))
            }
            Builtin::Len => {
                let bytes = self.string_arg(name, args, 0, at)?;
                Ok(Value::from_f64(bytes.len() as f64))
            }
            Builtin::Trim => self.map_string(name, args, at, |text, out| {
                out.extend_from_slice(text.trim().as_bytes());
            }),
            Builtin::Upper => self.map_string(name, args, at, |text, out| {
                for ch in text.chars() {
                    for mapped in ch.to_uppercase() {
                        push_char(out, mapped);
                    }
                }
            }),
            Builtin::Lower => self.map_string(name, args, at, |text, out| {
                for ch in text.chars() {
                    for mapped in ch.to_lowercase() {
                        push_char(out, mapped);
                    }
                }
            }),
            Builtin::Contains => {
                let haystack = self.string_arg(name, args, 0, at)?;
                let needle = self.string_arg(name, args, 1, at)?;
                let found = needle.is_empty() || find_bytes(haystack, needle).is_some();
                Ok(Value::bool(found))
            }
            Builtin::Replace => {
                let mut buf = std::mem::take(&mut self.scratch);
                let result = (|| {
                    let haystack = self.string_arg(name, args, 0, at)?;
                    let old = self.string_arg(name, args, 1, at)?;
                    let new = self.string_arg(name, args, 2, at)?;
                    buf.clear();
                    if old.is_empty() {
                        // Inserting between every character is a footgun with
                        // no common production use; an empty needle leaves the
                        // text alone instead of multiplying it.
                        buf.extend_from_slice(haystack);
                    } else {
                        let mut rest = haystack;
                        while let Some(index) = find_bytes(rest, old) {
                            buf.extend_from_slice(&rest[..index]);
                            buf.extend_from_slice(new);
                            rest = &rest[index + old.len()..];
                        }
                        buf.extend_from_slice(rest);
                    }
                    self.new_string(&buf, at, ErrorKind::Runtime)
                })();
                self.scratch = buf;
                result
            }
            Builtin::Slice => {
                let mut buf = std::mem::take(&mut self.scratch);
                let result = (|| {
                    let bytes = self.string_arg(name, args, 0, at)?;
                    buf.clear();
                    let text = std::str::from_utf8(bytes).map_err(|_| {
                        OmniError::new(
                            ErrorKind::Runtime,
                            at,
                            format!("type mismatch: {name}() needs a UTF-8 string"),
                        )
                    })?;
                    // Character offsets, not byte offsets: a byte cut can land
                    // inside a multi-byte character and produce a string no
                    // host can decode. `len()` still reports bytes, because
                    // that is the storage a caller allocates for.
                    let chars = text.chars().count();
                    let start = self.count_arg(name, args, 1, at)?.min(chars);
                    let end = self.count_arg(name, args, 2, at)?.min(chars).max(start);
                    let begin = char_offset(text, start);
                    let stop = char_offset(text, end);
                    buf.extend_from_slice(&text.as_bytes()[begin..stop]);
                    self.new_string(&buf, at, ErrorKind::Runtime)
                })();
                self.scratch = buf;
                result
            }
            Builtin::TypeOf => {
                Ok(self.new_string(kind_of(args[0]).as_bytes(), at, ErrorKind::Runtime)?)
            }
            Builtin::FsReadText => {
                let path = {
                    let bytes = self.string_arg(name, args, 0, at)?;
                    utf8_path(name, bytes, at)?
                };
                let bytes = self.read_allowed(&path, at)?;
                self.new_string(&bytes, at, ErrorKind::Runtime)
            }
            Builtin::FsWriteText => {
                let path = {
                    let bytes = self.string_arg(name, args, 0, at)?;
                    utf8_path(name, bytes, at)?
                };
                let text = self.string_arg(name, args, 1, at)?;
                self.write_allowed(&path, text, at)?;
                Ok(Value::NIL)
            }
            Builtin::JsonParse => self.json_parse(name, args, at),
            Builtin::JsonStringify => self.json_stringify(name, args, at),
        }
    }

    /// One argument as a character offset: finite, truncated, and clamped at
    /// zero. A negative or absurd index gets a defined answer (the start or the
    /// end) rather than an error, because that is how the range reads.
    fn count_arg(
        &self,
        name: &str,
        args: &[Value],
        index: usize,
        at: Pos,
    ) -> Result<usize, OmniError> {
        let n = self.number_arg(name, args, index, at)?;
        if !n.is_finite() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("{name}() needs a finite index for argument {}", index + 1),
            ));
        }
        if n <= 0.0 {
            return Ok(0);
        }
        Ok(n.trunc() as usize)
    }

    /// One argument as a number, or the message a type mismatch deserves.
    fn number_arg(
        &self,
        name: &str,
        args: &[Value],
        index: usize,
        at: Pos,
    ) -> Result<f64, OmniError> {
        let value = args[index];
        value.as_f64().ok_or_else(|| {
            OmniError::new(
                ErrorKind::Runtime,
                at,
                format!(
                    "type mismatch: {name}() needs a number for argument {}, found {}: {}",
                    index + 1,
                    kind_of(value),
                    self.show(value)
                ),
            )
        })
    }

    /// One argument as a string's bytes, or the message a type mismatch deserves.
    ///
    /// A string this engine cannot read is reported as the expiry it is rather
    /// than as "not a string", because that is the mistake the caller made.
    fn string_arg<'v>(
        &'v self,
        name: &str,
        args: &[Value],
        index: usize,
        at: Pos,
    ) -> Result<&'v [u8], OmniError> {
        let value = args[index];
        if let Some(bytes) = self.string_of(value) {
            return Ok(bytes);
        }
        Err(if value.is_string() {
            unreadable_string(at, value)
        } else {
            OmniError::new(
                ErrorKind::Runtime,
                at,
                format!(
                    "type mismatch: {name}() needs a string for argument {}, found {}: {}",
                    index + 1,
                    kind_of(value),
                    self.show(value)
                ),
            )
        })
    }

    /// Rebuild a string argument through `map`, reusing the scratch buffer.
    ///
    /// `map` sees the decoded text and writes UTF-8 into the buffer. Taking the
    /// buffer out first is what lets `string_arg` borrow the engine while the
    /// result is still built with `&mut self`.
    fn map_string(
        &mut self,
        name: &str,
        args: &[Value],
        at: Pos,
        map: impl FnOnce(&str, &mut Vec<u8>),
    ) -> Result<Value, OmniError> {
        let mut buf = std::mem::take(&mut self.scratch);
        let result = (|| {
            let bytes = self.string_arg(name, args, 0, at)?;
            let text = std::str::from_utf8(bytes).map_err(|_| {
                OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!("type mismatch: {name}() needs a UTF-8 string"),
                )
            })?;
            buf.clear();
            map(text, &mut buf);
            self.new_string(&buf, at, ErrorKind::Runtime)
        })();
        self.scratch = buf;
        result
    }

    /// The one place a string block enters the execution arena, and so the one
    /// place the length limit is enforced.
    fn new_string(&mut self, bytes: &[u8], at: Pos, kind: ErrorKind) -> Result<Value, OmniError> {
        if bytes.len() > MAX_STRING_LEN {
            return Err(OmniError::new(
                kind,
                at,
                format!(
                    "string is {} bytes, over the {} MiB limit",
                    bytes.len(),
                    MAX_STRING_LEN / (1024 * 1024)
                ),
            ));
        }
        let p = object::build(&mut self.arena, bytes);
        self.strings_built += 1;
        Ok(Value::from_ptr(p))
    }

    /// One arithmetic site, so the divide-by-zero rule and the typed-`+` rule are
    /// each stated once.
    ///
    /// Zero division is an error rather than IEEE infinity, because an `inf`
    /// returning through the FFI would be indistinguishable from a successful
    /// computation, and a silent wrong answer is worse than a loud failure.
    fn arith(&mut self, lhs: Value, op: BinOp, rhs: Value, at: Pos) -> Result<Value, OmniError> {
        if op == BinOp::Add {
            return match (lhs.is_string(), rhs.is_string()) {
                (true, true) => self.concat(lhs, rhs, at),
                (false, false) => self.numeric(lhs, op, rhs, at),
                _ => Err(OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!(
                        "type mismatch: cannot add {} to {}: {} + {}",
                        kind_of(lhs),
                        kind_of(rhs),
                        self.show(lhs),
                        self.show(rhs)
                    ),
                )),
            };
        }
        self.numeric(lhs, op, rhs, at)
    }

    /// `string + string`, newly allocated in the execution arena.
    ///
    /// A copy, not a rope: the cost of `let s = s + "x"` in a loop is quadratic
    /// and that is a known property rather than a surprise. The bytes are
    /// gathered into the engine's scratch buffer first, because `string_of`
    /// borrows the engine and `new_string` needs it mutably. The buffer goes back
    /// afterwards with its capacity intact, so the next `+` reuses it instead of
    /// asking the system allocator for one per concatenation.
    fn concat(&mut self, lhs: Value, rhs: Value, at: Pos) -> Result<Value, OmniError> {
        let mut buf = std::mem::take(&mut self.scratch);
        let result = self.concat_into(lhs, rhs, at, &mut buf);
        self.scratch = buf;
        result
    }

    fn concat_into(
        &mut self,
        lhs: Value,
        rhs: Value,
        at: Pos,
        buf: &mut Vec<u8>,
    ) -> Result<Value, OmniError> {
        // Scoped so both shared borrows end before the arena is claimed.
        {
            let Some(a) = self.string_of(lhs) else {
                return Err(unreadable_string(at, lhs));
            };
            let Some(b) = self.string_of(rhs) else {
                return Err(unreadable_string(at, rhs));
            };
            let total = a.len() + b.len();
            if total > MAX_STRING_LEN {
                return Err(OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!(
                        "concatenation would make a {total}-byte string, over the {} MiB limit",
                        MAX_STRING_LEN / (1024 * 1024)
                    ),
                ));
            }
            buf.clear();
            buf.reserve(total);
            buf.extend_from_slice(a);
            buf.extend_from_slice(b);
        }
        self.new_string(buf, at, ErrorKind::Runtime)
    }

    fn numeric(&self, lhs: Value, op: BinOp, rhs: Value, at: Pos) -> Result<Value, OmniError> {
        let (a, b) = match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                return Err(OmniError::new(
                    ErrorKind::Runtime,
                    at,
                    format!(
                        "type mismatch: '{}' needs numbers on both sides (found {} and {})",
                        op.symbol(),
                        kind_of(lhs),
                        kind_of(rhs)
                    ),
                ));
            }
        };
        let n = match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div if b == 0.0 => {
                return Err(OmniError::new(ErrorKind::Runtime, at, "division by zero"));
            }
            BinOp::Mod if b == 0.0 => {
                return Err(OmniError::new(ErrorKind::Runtime, at, "remainder by zero"));
            }
            BinOp::Div => a / b,
            BinOp::Mod => a % b,
        };
        Ok(Value::from_f64(n))
    }
}

/// The literal text an instruction's span addresses.
fn literal_span(source: &str, instr: Instr) -> &str {
    let start = instr.a as usize;
    source
        .get(start..start.saturating_add(instr.b as usize))
        .unwrap_or("")
}

/// Pop one value, reporting a compiler invariant break rather than panicking.
fn pop(stack: &mut Stack, at: Pos) -> Result<Value, OmniError> {
    stack
        .pop()
        .ok_or_else(|| OmniError::new(ErrorKind::Panic, at, "the VM stack underflowed"))
}

/// The value a `Store` keeps: `let x = 1 + 2` evaluates to three.
fn top(stack: &Stack, at: Pos) -> Result<Value, OmniError> {
    stack
        .last()
        .copied()
        .ok_or_else(|| OmniError::new(ErrorKind::Panic, at, "the VM stack underflowed"))
}

/// The module's namespace: its file stem without the extension.
fn module_stem(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_owned)
}

/// Whether a module stem can be written as an identifier, and therefore read as
/// `stem.export`. The check mirrors the lexer's ASCII identifier rule rather than
/// duplicating `MAX_IDENTIFIER_LEN`, because the binding table is keyed by string.
fn is_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// A path argument as UTF-8 text. Filesystem APIs take `&str` here because the
/// language's strings are UTF-8 and a non-UTF-8 path would have no spelling.
fn utf8_path(name: &str, bytes: &[u8], at: Pos) -> Result<String, OmniError> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| {
        OmniError::new(
            ErrorKind::Runtime,
            at,
            format!("type mismatch: {name}() needs a UTF-8 path"),
        )
    })
}

fn module_io_error(module: &str, error: &std::io::Error, at: Pos) -> OmniError {
    OmniError::new(
        ErrorKind::Runtime,
        at,
        format!("cannot read module '{module}': {error}"),
    )
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new()
    }
}

/// Whole microseconds, saturating where the ABI's counter cannot go further.
///
/// `Duration::as_micros` is a `u128`; a single execution cannot reach the `u32`
/// ceiling in any real program, but converting with `try_from` and a fallback is
/// what makes that a stated limit rather than a silent wrap.
fn elapsed_micros(elapsed: Duration) -> u32 {
    u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX)
}

/// One of the helpers every host gets without registering anything.
///
/// A host registration of the same name wins, so these are a floor rather than a
/// reserved namespace: an embedder that wants `len` to count characters instead
/// of bytes registers its own and gets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    Abs,
    Min,
    Max,
    Clamp,
    Floor,
    Ceil,
    Round,
    Pow,
    Sqrt,
    Len,
    Trim,
    Upper,
    Lower,
    Contains,
    Replace,
    Slice,
    TypeOf,
    FsReadText,
    FsWriteText,
    JsonParse,
    JsonStringify,
}

/// The builtin a call name means, and how many arguments it takes.
///
/// The arity lives here rather than in `invoke_builtin` so the call site can
/// reject a wrong count before any argument is *used*, naming the function and
/// both counts — the same contract a host registration gets.
fn builtin_for(name: &str) -> Option<(Builtin, usize)> {
    Some(match name {
        "abs" => (Builtin::Abs, 1),
        "min" => (Builtin::Min, 2),
        "max" => (Builtin::Max, 2),
        "clamp" => (Builtin::Clamp, 3),
        "floor" => (Builtin::Floor, 1),
        "ceil" => (Builtin::Ceil, 1),
        "round" => (Builtin::Round, 1),
        "pow" => (Builtin::Pow, 2),
        "sqrt" => (Builtin::Sqrt, 1),
        "len" => (Builtin::Len, 1),
        "trim" => (Builtin::Trim, 1),
        "upper" => (Builtin::Upper, 1),
        "lower" => (Builtin::Lower, 1),
        "contains" => (Builtin::Contains, 2),
        "replace" => (Builtin::Replace, 3),
        "slice" => (Builtin::Slice, 3),
        "type_of" => (Builtin::TypeOf, 1),
        "fs.read_text" => (Builtin::FsReadText, 1),
        "fs.write_text" => (Builtin::FsWriteText, 2),
        "json.parse" => (Builtin::JsonParse, 1),
        "json.stringify" => (Builtin::JsonStringify, 1),
        _ => return None,
    })
}

/// Append one character to a byte buffer.
fn push_char(out: &mut Vec<u8>, ch: char) {
    let mut tmp = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
}

/// The byte offset of the first occurrence of `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The byte offset of the `index`-th character, or the end of the text.
fn char_offset(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(offset, _)| offset)
}

/// The name a type error uses for one value's kind. Written on `Value`'s tags
/// rather than on the engine so it needs no bytes behind an address: a message
/// about a stale string must not depend on reading it.
fn kind_of(value: Value) -> &'static str {
    if value.is_string() {
        "string"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.is_number() {
        "number"
    } else if value == Value::NIL {
        "null"
    } else if value == Value::UNDEF {
        "undefined"
    } else {
        "unknown"
    }
}

/// The error for a string box this engine cannot read: reported as the type
/// problem it looks like, because from the caller's side that is what it is.
fn unreadable_string(at: Pos, value: Value) -> OmniError {
    OmniError::new(
        ErrorKind::Runtime,
        at,
        format!(
            "this string was produced by an earlier execution and its bytes are gone; \
             re-run to get a readable one ({value})"
        ),
    )
}

/// Decode a string literal's escapes.
///
/// `\n`, `\r`, `\t`, `\"`, `\\`, and `\u{...}` with one to six hex digits. Every
/// other byte after a backslash is an error that quotes what it found, because
/// the two mistakes people actually make — `"\d"` for a digit and `"\x41"` from
/// another language — are both invisible in a message that only says "bad escape".
///
/// `\u{...}` accepts scalars only: `0xD800..=0xDFFF` is refused as a lone
/// surrogate and anything above `0x10FFFF` as out of range. There is no
/// surrogate-pair joining, because these strings are UTF-8 and a code-unit escape
/// would be a second encoding rule nobody asked for — write `"😀"` or `"\u{1F600}"`.
fn decode_literal(raw: &str, at: Pos) -> Result<Cow<'_, [u8]>, OmniError> {
    let bytes = raw.as_bytes();
    // Most literals contain no escape at all, and for those the decoded value
    // *is* the source text. Borrowing it skips both the allocation and the copy
    // into it; the arena copy in `new_string` is then the only one performed.
    if !bytes.contains(&b'\\') {
        return Ok(Cow::Borrowed(bytes));
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut pos = 0usize;
    while pos < bytes.len() {
        let b = bytes[pos];
        if b != b'\\' {
            out.push(b);
            pos += 1;
            continue;
        }
        let Some(&esc) = bytes.get(pos + 1) else {
            // The lexer rejects a literal that ends mid-escape; this is the
            // cross-module assumption, checked instead of trusted.
            return Err(OmniError::new(
                ErrorKind::Parse,
                at,
                "a string literal ends with a backslash",
            ));
        };
        match esc {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'u' => {
                // `end` is an index into `bytes`, so the caller lands on the first
                // byte after the closing brace.
                let (end, cp) = decode_unicode_escape(bytes, pos + 2, at)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(cp.encode_utf8(&mut buf).as_bytes());
                pos = end;
                continue;
            }
            other => {
                // Safe: the byte after an ASCII backslash starts a character, so
                // this slice begins on a UTF-8 boundary.
                let shown = raw[pos + 1..].chars().next().unwrap_or(other as char);
                return Err(OmniError::new(
                    ErrorKind::Parse,
                    at,
                    format!("unknown escape '\\{shown}' in a string literal"),
                ));
            }
        }
        pos += 2;
    }
    Ok(Cow::Owned(out))
}

/// Parse `\u{...}` starting at the byte after `u`.
///
/// Returns the index one past the closing brace and the scalar the braces name.
/// Only scalars are accepted: a surrogate or an over-large value is a parse error
/// that quotes the digits it found, because "invalid unicode escape" tells a
/// reader nothing they can act on.
fn decode_unicode_escape(bytes: &[u8], pos: usize, at: Pos) -> Result<(usize, char), OmniError> {
    let parse_error = |message: String| OmniError::new(ErrorKind::Parse, at, message);
    if bytes.get(pos) != Some(&b'{') {
        return Err(parse_error(
            "expected '{' after '\\u' — the form is \\u{1F600}".to_string(),
        ));
    }
    let digits_start = pos + 1;
    let mut digits_end = digits_start;
    while digits_end < bytes.len()
        && digits_end - digits_start < 6
        && bytes[digits_end].is_ascii_hexdigit()
    {
        digits_end += 1;
    }
    let digits = &bytes[digits_start..digits_end];
    if digits.is_empty() {
        return Err(parse_error(
            "'\\u{}' needs at least one hex digit".to_string(),
        ));
    }
    if bytes.get(digits_end) != Some(&b'}') {
        let reason = if digits.len() == 6 {
            "'\\u{...}' takes at most six hex digits".to_string()
        } else {
            "a hex escape must end with '}'".to_string()
        };
        return Err(parse_error(reason));
    }
    let hex = std::str::from_utf8(digits)
        .map_err(|_| parse_error("internal: hex digits are ASCII".to_string()))?;
    let value = u32::from_str_radix(hex, 16)
        .map_err(|_| parse_error(format!("'\\u{{{hex}}}' is not a Unicode code point")))?;
    let end = digits_end + 1; // one past the closing brace
    let ch = char::from_u32(value).ok_or_else(|| {
        parse_error(if (0xD800..=0xDFFF).contains(&value) {
            format!(
                "'\\u{{{hex}}}' is a lone surrogate; OmniEngine strings are UTF-8, so \
                 write the character itself or its scalar value"
            )
        } else {
            format!("'\\u{{{hex}}}' is above the last Unicode scalar (U+10FFFF)")
        })
    })?;
    Ok((end, ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(src: &str) -> Value {
        Engine::new()
            .exec(src)
            .unwrap_or_else(|e| panic!("{src} failed: {e}"))
    }

    fn num(n: f64) -> Value {
        Value::from_f64(n)
    }

    /// Evaluate `src` and render the result, which is the only way a test can see
    /// a string's content — the box itself prints as `<string>` by design.
    fn text(src: &str) -> String {
        let mut e = Engine::new();
        let v = e.exec(src).unwrap_or_else(|e| panic!("{src} failed: {e}"));
        e.render(v)
    }

    #[test]
    fn the_vertical_slice_returns_forty_two() {
        assert_eq!(ev("let answer = 40 + 2"), num(42.0));
    }

    #[test]
    fn precedence_and_grouping() {
        assert_eq!(ev("2 + 3 * 4"), num(14.0));
        assert_eq!(ev("(2 + 3) * 4"), num(20.0));
        assert_eq!(ev("20 / 3"), num(20.0 / 3.0));
        assert_eq!(ev("20 % 3"), num(2.0));
        assert_eq!(ev("1 + 2 * 3 - 4 / 2"), num(5.0));
    }

    #[test]
    fn unary_minus_binds_tighter_than_multiply() {
        assert_eq!(ev("-3 + 10"), num(7.0));
        assert_eq!(ev("- 2 * 3"), num(-6.0));
        assert_eq!(ev("--5"), num(5.0));
        assert_eq!(ev("-(2 + 3)"), num(-5.0));
    }

    #[test]
    fn let_bindings_are_visible_to_later_statements() {
        assert_eq!(ev("let a = 10\nlet b = a * a\nb - a"), num(90.0));
    }

    #[test]
    fn bindings_do_not_leak_between_executions() {
        let mut e = Engine::new();
        assert_eq!(e.exec("let secret = 1").unwrap(), num(1.0));
        assert_eq!(
            e.exec("secret").unwrap_err().kind,
            ErrorKind::Runtime,
            "a per-execution scope must not remember the last program"
        );
    }

    #[test]
    fn shadowing_uses_the_latest_binding() {
        assert_eq!(ev("let x = 1\nlet x = 2\nx"), num(2.0));
    }

    #[test]
    fn semicolons_are_optional_statement_separators() {
        assert_eq!(ev("let a = 1; let b = 2; a + b"), num(3.0));
    }

    #[test]
    fn last_statement_value_is_returned() {
        assert_eq!(ev("1\n2\n3"), num(3.0));
        assert_eq!(ev("let x = 5"), num(5.0));
        assert_eq!(ev(""), Value::UNDEF);
    }

    #[test]
    fn division_by_zero_is_a_runtime_error_not_a_panic() {
        let e = Engine::new().exec("1 / 0").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert!(e.message.contains("division by zero"), "message was {e}");
        assert_eq!(
            Engine::new().exec("1 % 0").unwrap_err().message,
            "remainder by zero"
        );
    }

    #[test]
    fn undefined_variable_reports_its_line() {
        let e = Engine::new().exec("1 +\nnope").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!(e.line, 2);
        assert!(e.message.contains("nope"), "message was {e}");
    }

    #[test]
    fn syntax_errors_are_parse_errors() {
        assert_eq!(
            Engine::new().exec("1 +").unwrap_err().kind,
            ErrorKind::Parse
        );
        assert_eq!(
            Engine::new().exec("let = 3").unwrap_err().kind,
            ErrorKind::Parse
        );
        assert_eq!(
            Engine::new().exec("(1 + 2").unwrap_err().kind,
            ErrorKind::Parse
        );
        assert_eq!(
            Engine::new().exec("1 + )").unwrap_err().kind,
            ErrorKind::Parse
        );
    }

    #[test]
    fn lex_error_propagates_kind() {
        assert_eq!(
            Engine::new().exec("1 @ 2").unwrap_err().kind,
            ErrorKind::Lex
        );
    }

    #[test]
    fn overly_long_identifiers_are_rejected_not_truncated() {
        let name = "a".repeat(MAX_IDENTIFIER_LEN + 1);
        let src = format!("let {name} = 1\n{name}");
        let e = Engine::new().exec(&src).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Parse, "got {e}");
    }

    #[test]
    fn define_injects_a_persistent_host_value() {
        let mut e = Engine::new();
        e.define("PI", num(3.0));
        assert_eq!(e.exec("PI * 2").unwrap(), num(6.0));
        // Host bindings survive an execution boundary, unlike `let`.
        assert_eq!(e.exec("PI + 1").unwrap(), num(4.0));
    }

    #[test]
    fn script_bindings_shadow_host_bindings() {
        let mut e = Engine::new();
        e.define("x", num(1.0));
        assert_eq!(e.exec("let x = 9\nx").unwrap(), num(9.0));
        assert_eq!(e.exec("x").unwrap(), num(1.0));
    }

    #[test]
    fn arena_is_reclaimed_per_execution() {
        let mut e = Engine::new();
        let _ = e.exec("let a = 1\nlet b = 2\na + b");
        assert_eq!(e.arena().allocations(), 2, "one cell per let");
        let _ = e.exec("1");
        assert_eq!(
            e.arena().allocations(),
            0,
            "arena must be reclaimed per exec"
        );
    }

    #[test]
    fn failed_exec_leaves_last_value_untouched() {
        let mut e = Engine::new();
        assert_eq!(e.exec("7").unwrap(), num(7.0));
        assert!(e.exec("1 +").is_err());
        assert_eq!(e.last_value(), num(7.0));
        assert!(e.last_error().is_some());
        assert!(e.exec("8").is_ok());
        assert!(
            e.last_error().is_none(),
            "a success must clear the stale error"
        );
    }

    #[test]
    fn float_literals_and_exponents_evaluate() {
        assert_eq!(ev("1.5 * 2"), num(3.0));
        assert_eq!(ev("2e3"), num(2000.0));
        assert_eq!(ev("1 / 3"), num(1.0 / 3.0));
    }

    /// End to end, not just at the lexer: the source goes through `Engine::exec`
    /// with a multi-byte comment, which is the path a non-English program takes.
    /// Both SDKs assert the same thing from their side of the FFI.
    #[test]
    fn a_program_with_a_non_ascii_comment_evaluates() {
        assert_eq!(ev("// নম্বর\n42"), num(42.0));
        assert_eq!(ev("// টীকা\nlet x = 40\nx + 2"), num(42.0));
        let e = Engine::new().exec("// comment\n@").unwrap_err();
        assert_eq!(e.line, 2, "line number broke after a multi-byte comment");
    }

    /// Comments are free-form text in any script, but identifiers are ASCII by
    /// design — a decision, not an accident. A non-ASCII name has to fail as a
    /// lex error that names the character, never as an undefined variable.
    #[test]
    fn non_ascii_identifiers_are_a_lex_error_naming_the_character() {
        let e = Engine::new().exec("let উত্তর = 1").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Lex);
        assert!(e.message.contains('উ'), "message was {e}");
    }

    #[test]
    fn errors_render_with_a_line_prefix() {
        let e = Engine::new().exec("1 / 0").unwrap_err();
        assert_eq!(e.to_string(), "[Line 1, Col 3]: division by zero");
    }

    // -- strings --------------------------------------------------------------

    #[test]
    fn a_literal_is_a_string_value_with_its_content() {
        let mut e = Engine::new();
        let v = e.exec("\"hello\"").unwrap();
        assert!(v.is_string(), "the result must be in the pointer row");
        assert_eq!(e.string_of(v).unwrap(), b"hello");
        assert_eq!(e.render(v), "hello");
    }

    #[test]
    fn an_empty_literal_is_a_string_not_nothing() {
        let mut e = Engine::new();
        let v = e.exec("\"\"").unwrap();
        assert!(v.is_string());
        assert_eq!(e.string_of(v).unwrap(), b"");
        assert!(!e.truthy(v), "empty is falsy");
    }

    #[test]
    fn a_literal_can_be_bound_and_read_back() {
        assert_eq!(text("let greeting = \"নম্বর\"\ngreeting"), "নম্বর");
    }

    #[test]
    fn escapes_decode_to_what_they_name() {
        assert_eq!(text(r#""a\nb""#), "a\nb");
        assert_eq!(text(r#""a\tb""#), "a\tb");
        assert_eq!(text(r#""a\rb""#), "a\rb");
        assert_eq!(text(r#""a\"b""#), "a\"b");
        assert_eq!(text(r#""a\\b""#), "a\\b");
        assert_eq!(text(r#""\u{41}\u{42}\u{43}""#), "ABC");
        assert_eq!(text(r#""\u{1F600}""#), "😀");
        assert_eq!(text("\"😀\""), "😀");
    }

    #[test]
    fn a_lone_surrogate_escape_is_refused_and_says_why() {
        for src in [r#""\u{D800}""#, r#""\u{dFFF}""#, r#""\u{D83D}\u{DE00}""#] {
            let e = Engine::new().exec(src).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Parse, "{src} gave {e}");
            assert!(
                e.message.contains("surrogate"),
                "{src} message was {}",
                e.message
            );
        }
    }

    #[test]
    fn a_code_point_above_the_unicode_limit_is_refused() {
        let e = Engine::new().exec(r#""\u{110000}""#).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Parse);
        assert!(e.message.contains("10FFFF"), "message was {e}");
    }

    #[test]
    fn a_malformed_hex_escape_names_the_form_it_wanted() {
        let e = Engine::new().exec(r#""\u41""#).unwrap_err();
        assert!(e.message.contains("\\u{1F600}"), "message was {e}");
        let e = Engine::new().exec(r#""\u{}""#).unwrap_err();
        assert!(e.message.contains("at least one"), "message was {e}");
        let e = Engine::new().exec(r#""\u{1234567}""#).unwrap_err();
        assert!(e.message.contains("at most six"), "message was {e}");
        let e = Engine::new().exec(r#""\q""#).unwrap_err();
        assert!(
            e.message.contains("unknown escape '\\q'"),
            "message was {e}"
        );
        let e = Engine::new().exec(r#""\x41""#).unwrap_err();
        assert!(
            e.message.contains("unknown escape '\\x'"),
            "message was {e}"
        );
    }

    #[test]
    fn a_bad_escape_is_reported_on_the_line_the_literal_starts() {
        let e = Engine::new().exec("1 + 2\n\"ok\"\n\"bad\\q\"").unwrap_err();
        assert_eq!((e.kind, e.line), (ErrorKind::Parse, 3), "got {e}");
    }

    #[test]
    fn plus_concatenates_two_strings() {
        assert_eq!(text(r#""omni" + "field""#), "omnifield");
        assert_eq!(text(r#""a" + "b" + "c""#), "abc");
        // Escaped rather than raw: the program here starts with two quotes, and
        // `r###"" + "x"###` reads the content as `" + "x` — the closing delimiter
        // is found before the empty literal it was meant to express. A plain
        // string with `\"` says exactly which characters the lexer gets.
        assert_eq!(text("\"\" + \"x\""), "x");
        assert_eq!(text("\"x\" + \"\""), "x");
        assert_eq!(text(r#""\n" + "x""#), "\nx");
    }

    #[test]
    fn plus_still_adds_two_numbers() {
        assert_eq!(ev("1 + 2"), num(3.0));
        assert_eq!(ev("let a = 1\na + 1"), num(2.0));
    }

    #[test]
    fn mixing_a_string_and_a_number_in_addition_names_both_operands() {
        let e = Engine::new().exec(r#""hi" + 42"#).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!(
            e.message, r#"type mismatch: cannot add string to number: "hi" + 42"#,
            "the message the requester asked for, verbatim"
        );
        let e = Engine::new().exec(r#"42 + "hi""#).unwrap_err();
        assert_eq!(
            e.message,
            r#"type mismatch: cannot add number to string: 42 + "hi""#
        );
        // A boolean operand reaches the same arm. `("a" == "b")` rather than a
        // `true` literal, because the grammar has no boolean literals yet — the
        // only booleans a program can write come out of a comparison.
        let e = Engine::new().exec(r#"("a" == "b") + "hi""#).unwrap_err();
        assert_eq!(
            e.message,
            r#"type mismatch: cannot add boolean to string: false + "hi""#
        );
    }

    #[test]
    fn other_operators_report_the_types_they_found() {
        let e = Engine::new().exec(r#""a" - "b""#).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!(
            e.message,
            "type mismatch: '-' needs numbers on both sides (found string and string)"
        );
        let e = Engine::new().exec(r#"6 / "b""#).unwrap_err();
        assert_eq!(
            e.message,
            "type mismatch: '/' needs numbers on both sides (found number and string)"
        );
    }

    #[test]
    fn equality_compares_string_content_not_addresses() {
        // Two separately built blocks in one execution holding the same bytes:
        // `Value`'s PartialEq says different boxes, the language says equal. They
        // have to come from the same arena, because a fresh engine starts its bump
        // cursor at the same address every time and two identical boxes would then
        // compare equal by accident rather than by design.
        let mut e = Engine::new();
        let one = e
            .new_string(b"ab", Pos::new(1, 1), ErrorKind::Runtime)
            .unwrap();
        let two = e
            .new_string(b"ab", Pos::new(1, 1), ErrorKind::Runtime)
            .unwrap();
        assert_ne!(one, two, "the boxes are different allocations");
        assert!(e.eq_values(one, two), "and the language calls them equal");
        assert_eq!(e.exec(r#""ab" == "a" + "b""#).unwrap(), Value::TRUE);
        assert_eq!(e.exec(r#""ab" != "a" + "b""#).unwrap(), Value::FALSE);
        assert_eq!(e.exec(r#""ab" == "ba""#).unwrap(), Value::FALSE);
        assert_eq!(e.exec(r#""ab" != "ba""#).unwrap(), Value::TRUE);
    }

    /// The grammar has no `null` or `true` literal yet — that is a line in the
    /// roadmap, not a gap here — so the cross-type rules are tested where they are
    /// implemented, on values the FFI can produce even if a script cannot.
    #[test]
    fn equality_across_types_is_false_and_inequality_is_true() {
        let mut e = Engine::new();
        let s = e.exec(r#""1""#).unwrap();
        let n = e.exec("1").unwrap();
        for (lhs, rhs) in [
            (s, n),
            (n, s),
            (n, Value::NIL),
            (Value::NIL, Value::UNDEF),
            (Value::TRUE, s),
            (Value::UNDEF, s),
        ] {
            assert!(
                !e.eq_values(lhs, rhs),
                "{} and {} must not be equal",
                kind_of(lhs),
                kind_of(rhs)
            );
        }
        // And the parser agrees, for the one cross-type pair a program can write.
        assert_eq!(e.exec(r#""1" == 1"#).unwrap(), Value::FALSE);
        assert_eq!(e.exec(r#""1" != 1"#).unwrap(), Value::TRUE);
    }

    #[test]
    fn number_equality_keeps_ieee_rules() {
        let mut e = Engine::new();
        assert_eq!(e.exec("1 == 1").unwrap(), Value::TRUE);
        assert_eq!(e.exec("0 == -0").unwrap(), Value::TRUE);
        assert_eq!(
            e.exec("0.1 + 0.2 == 0.3").unwrap(),
            Value::FALSE,
            "IEEE, honestly: the classic rounding miss is not equal"
        );
        assert_eq!(e.exec("0.1 + 0.2 != 0.3").unwrap(), Value::TRUE);
        assert_eq!(e.exec("1 + 0.1 + 0.2 == 1.3").unwrap(), Value::TRUE);
        // Singletons compare by tag, so identical ones are equal and distinct ones
        // are not — reachable through the engine even without literals for them.
        assert!(e.eq_values(Value::TRUE, Value::TRUE));
        assert!(!e.eq_values(Value::TRUE, Value::FALSE));
        assert!(!e.eq_values(Value::NIL, Value::UNDEF));
        // A NaN is not equal to itself, and `Value::from_f64` normalises every NaN
        // to the canonical box — so two NaNs of different payloads arrive as the
        // same bits and still compare unequal. That is the IEEE rule surviving the
        // normalisation, not an artefact of it.
        let nan = num(f64::NAN);
        assert!(!e.eq_values(nan, nan));
        assert_eq!(
            num(f64::NAN / 2.0).bits(),
            nan.bits(),
            "a payload-carrying NaN normalises to the canonical box"
        );
        assert!(
            !e.eq_values(nan, num(f64::NAN / 2.0)),
            "identical NaN boxes must still be unequal, or `NaN != NaN` is broken"
        );
    }

    #[test]
    fn comparison_is_looser_than_addition() {
        assert_eq!(ev(r#""a" + "b" == "ab""#), Value::TRUE);
        assert_eq!(ev(r#"1 + 2 == 3"#), Value::TRUE);
        assert_eq!(ev(r#"1 + 2 == "3""#), Value::FALSE);
    }

    #[test]
    fn truthiness_of_a_string_is_emptiness() {
        let mut e = Engine::new();
        // One value per exec, read before the next exec rewinds egress: the stale
        // rule applies to these too, so the assertions are sequenced rather than
        // nested.
        let filled = e.exec("\"x\"").unwrap();
        assert!(e.truthy(filled));
        let empty = e.exec("\"\"").unwrap();
        assert!(!e.truthy(empty));
        assert!(e.truthy(num(1.0)));
        assert!(!e.truthy(num(0.0)));
        assert!(!e.truthy(Value::UNDEF));
    }

    #[test]
    fn a_stale_string_is_readable_until_overwritten_and_never_crashes() {
        // The documented expiry rule, tested rather than asserted in a comment:
        // the block rewinds, so the bytes stay mapped and the address stays
        // owned. What a caller loses is the content, not the process.
        let mut e = Engine::new();
        let first = e.exec("\"first\"").unwrap();
        assert_eq!(e.string_of(first).unwrap(), b"first");
        let _ = e.exec("\"second\"").unwrap();
        let read = e.string_of(first);
        assert!(read.is_some(), "a rewind unmaps nothing");
        assert_ne!(read.unwrap(), b"first", "and the same slot was reused");
    }

    #[test]
    fn a_result_string_lives_in_egress_not_the_execution_arena() {
        let mut e = Engine::new();
        let v = e.exec(r#""a" + "b""#).unwrap();
        let addr = v.as_ptr().unwrap() as usize;
        assert!(
            e.egress().owns(addr),
            "the value a host holds must address the rewinding block"
        );
        assert!(
            StrRef::checked(v, e.arena()).is_none(),
            "and never the block that resets"
        );
    }

    #[test]
    fn the_result_survives_a_reset_of_the_execution_arena() {
        let mut e = Engine::new();
        let v = e.exec("let x = \"keep me\"\nx + \"\"").unwrap();
        let before = e.string_of(v).unwrap().to_vec();
        // An execution that allocates heavily and fails must not disturb egress.
        let _ = e.exec("1 +").unwrap_err();
        assert_eq!(e.string_of(v).map(|b| b.to_vec()), Some(before));
    }

    #[test]
    fn define_string_injects_a_host_string_a_program_can_read() {
        let mut e = Engine::new();
        e.define_string("who", b"world").unwrap();
        assert_eq!(text_of(&mut e, r#""hello, " + who"#), "hello, world");
        // And it survives an execution boundary, which is why it is not in the arena.
        assert_eq!(text_of(&mut e, "who"), "world");
    }

    #[test]
    fn a_host_string_can_be_compared_for_content() {
        let mut e = Engine::new();
        e.define_string("greeting", b"hi").unwrap();
        assert_eq!(e.exec("greeting == \"hi\"").unwrap(), Value::TRUE);
        assert_eq!(e.exec("greeting == \"hi\" + \"\"").unwrap(), Value::TRUE);
    }

    #[test]
    fn redefining_a_host_name_replaces_it_rather_than_accumulating() {
        let mut e = Engine::new();
        for _ in 0..500 {
            e.define_string("x", b"value").unwrap();
        }
        assert_eq!(e.globals.len(), 1, "a loop must not grow the binding list");
        e.define_string("x", b"other").unwrap();
        assert_eq!(text_of(&mut e, "x"), "other");
    }

    #[test]
    fn a_superseded_host_string_is_not_readable_as_a_string() {
        let mut e = Engine::new();
        e.define_string("x", b"first").unwrap();
        let stale = e.exec("x").unwrap();
        e.define_string("x", b"second").unwrap();
        // The allocation behind `stale` is gone; the engine must not read it.
        assert!(e.string_of(stale).is_none());
        assert_eq!(text_of(&mut e, "x"), "second");
    }

    #[test]
    fn define_string_rejects_bad_arguments_with_a_distinct_reason() {
        let mut e = Engine::new();
        assert_eq!(e.define_string("", b"x").unwrap_err(), HostError::EmptyName);
        assert_eq!(
            e.define_string(&"a".repeat(MAX_IDENTIFIER_LEN + 1), b"x")
                .unwrap_err(),
            HostError::NameTooLong
        );
        assert_eq!(
            e.define_string("x", &[0xFF, 0xFE]).unwrap_err(),
            HostError::NotUtf8
        );
        assert_eq!(
            e.define_string("x", b"").unwrap(),
            (),
            "an empty payload is a legitimate string"
        );
        assert_eq!(
            e.define_string("y", &vec![b'a'; MAX_STRING_LEN + 1])
                .unwrap_err(),
            HostError::TooLong
        );
    }

    #[test]
    fn a_program_cannot_build_a_string_over_the_limit() {
        // A *program* doing it is a runtime error, not an argument error: the
        // distinction the ABI's status codes are built on. The limit is 16 MiB of
        // payload, so the block at exactly the limit is accepted and one byte more
        // is refused at the one place that enforces it.
        let mut engine = Engine::new();
        let src = format!("let big = \"{}\"", "x".repeat(1024));
        assert_eq!(
            text_of(&mut engine, &src).len(),
            1024,
            "under the limit works"
        );
        let at_the_limit = engine.new_string(
            &vec![b'x'; MAX_STRING_LEN],
            Pos::new(1, 1),
            ErrorKind::Runtime,
        );
        assert!(at_the_limit.is_ok(), "the limit is inclusive");
        let over = engine
            .new_string(
                &vec![b'x'; MAX_STRING_LEN + 1],
                Pos::new(1, 1),
                ErrorKind::Runtime,
            )
            .unwrap_err();
        assert_eq!(over.kind, ErrorKind::Runtime);
        assert!(over.message.contains("16 MiB"), "message was {over}");
    }

    #[test]
    fn string_stats_stay_flat_across_many_executions() {
        let mut e = Engine::new();
        let _ = e.exec(r#"let a = "x" + "y""#);
        let warm = e.stats();
        for _ in 0..2000 {
            let _ = e.exec(r#"let a = "x" + "yy"; a + "z" == "xyyz""#);
        }
        let after = e.stats();
        assert_eq!(after.execs, warm.execs + 2000);
        assert!(after.strings_built > warm.strings_built, "work happened");
        assert_eq!(after.arena_chunks, warm.arena_chunks, "no new chunks");
        assert_eq!(
            after.arena_bytes_reserved, warm.arena_bytes_reserved,
            "the arena stopped growing"
        );
        assert_eq!(
            after.egress_bytes_reserved, warm.egress_bytes_reserved,
            "the egress block stopped growing"
        );
    }

    #[test]
    fn a_large_string_result_reuses_one_egress_block() {
        // The case that would otherwise break the model: 100 KiB does not fit a
        // 64 KiB chunk, so every promotion wants an oversize block, and a rewind
        // that cannot reuse one would grow forever.
        let mut e = Engine::new();
        let big = "\"".to_string() + &"x".repeat(100_000) + "\"";
        let _ = e.exec(&big).unwrap();
        let warm = e.stats();
        for _ in 0..50 {
            let _ = e.exec(&big).unwrap();
        }
        let after = e.stats();
        assert_eq!(after.egress_bytes_reserved, warm.egress_bytes_reserved);
        assert_eq!(text_of(&mut e, &big).len(), 100_000);
    }

    #[test]
    fn a_non_string_result_is_not_promoted_or_copied() {
        let mut e = Engine::new();
        let v = e.exec("1 + 2").unwrap();
        assert_eq!(v, num(3.0));
        assert_eq!(e.stats().strings_built, 0, "no string was built at all");
    }

    /// The claim "zero-overhead performance", made checkable. `live()` counts this
    /// thread's bytes in the global allocator, so anything the engine fails to free
    /// shows up as a positive delta over 100,000 executions.
    #[test]
    fn a_hundred_thousand_string_executions_leak_nothing() {
        let mut e = Engine::new();
        let src = r#"let a = "hello"; a + ", " + "world" == "hello, world""#;
        for _ in 0..200 {
            let _ = e.exec(src);
        }
        let before = crate::counted::live();
        for _ in 0..100_000 {
            let v = e.exec(src).unwrap();
            assert_eq!(v, Value::TRUE);
        }
        let after = crate::counted::live();
        assert_eq!(
            after - before,
            0,
            "{} bytes left behind by 100,000 executions",
            after - before
        );
        assert_eq!(e.stats().execs, 100_200);
    }

    /// The stronger half of the same claim: not merely "nothing left behind"
    /// but "nothing asked for". A warm engine refills its token buffer, keeps its
    /// binding vector's capacity, and bumps the arena, so a numeric execution
    /// must not reach the system allocator at all. `live()` cannot show this —
    /// a buffer allocated and freed every time nets to zero — so the counter
    /// here is allocation *requests*.
    #[test]
    fn a_warm_numeric_execution_makes_no_heap_allocation() {
        let mut e = Engine::new();
        let src = "let answer = 40 + 2; answer * 1";
        for _ in 0..64 {
            e.exec(src).unwrap();
        }
        let before = crate::counted::calls();
        for _ in 0..1000 {
            assert_eq!(e.exec(src).unwrap(), num(42.0));
        }
        assert_eq!(
            crate::counted::calls(),
            before,
            "a warm numeric exec still asked the system allocator for memory"
        );
    }

    /// A literal with no escape decodes to the source text itself, so building
    /// it and promoting the result touch only the arena and the egress block.
    /// The borrow is the whole reason `decode_literal` returns a `Cow`.
    #[test]
    fn a_warm_escape_free_literal_makes_no_heap_allocation() {
        let mut e = Engine::new();
        for _ in 0..64 {
            e.exec(r#""hello, world""#).unwrap();
        }
        let before = crate::counted::calls();
        for _ in 0..1000 {
            e.exec(r#""hello, world""#).unwrap();
        }
        assert_eq!(crate::counted::calls(), before);
    }

    /// Concatenation gathers into the engine's scratch buffer, so once that
    /// buffer is wide enough for the program's longest `+`, a warm execution
    /// allocates nothing here either. Three literals and two joins is the shape
    /// the cross-host verification rows use.
    #[test]
    fn a_warm_concatenation_makes_no_heap_allocation() {
        let mut e = Engine::new();
        let src = r#""hello, " + "world" + "!""#;
        for _ in 0..64 {
            e.exec(src).unwrap();
        }
        let before = crate::counted::calls();
        for _ in 0..1000 {
            e.exec(src).unwrap();
        }
        assert_eq!(crate::counted::calls(), before);
        assert_eq!(text_of(&mut e, src), "hello, world!");
    }

    // -- batch execution ------------------------------------------------------

    #[test]
    fn a_batch_returns_one_item_per_source_in_order() {
        let mut e = Engine::new();
        let items = e.exec_many(&["1 + 1", "let x = 6; x * 7", "\"ok\""]);
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(BatchItem::is_ok));
        assert_eq!(items[0].value.as_f64(), Some(2.0));
        assert_eq!(items[1].value.as_f64(), Some(42.0));
        assert_eq!(e.render(items[2].value), "ok");
    }

    /// A batch is a sequence of independent executions, not one long program.
    /// Without the per-item `reset_to` and binding clear, item 1 would read item
    /// 0's `x` out of a chunk that has since been handed back.
    #[test]
    fn an_item_cannot_see_the_previous_items_bindings() {
        let mut e = Engine::new();
        let items = e.exec_many(&["let secret = 1; secret", "secret"]);
        assert_eq!(items[0].value.as_f64(), Some(1.0));
        let err = items[1]
            .error
            .as_ref()
            .expect("the binding did not survive");
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert!(err.message.contains("secret"), "message was {err}");
        assert_eq!(items[1].value, Value::UNDEF);
    }

    #[test]
    fn a_host_global_still_survives_every_item() {
        let mut e = Engine::new();
        e.define("answer", num(42.0));
        let items = e.exec_many(&["answer", "answer + 1", "answer"]);
        assert_eq!(
            items.iter().map(|i| i.value.as_f64()).collect::<Vec<_>>(),
            vec![Some(42.0), Some(43.0), Some(42.0)]
        );
    }

    /// The reason the egress block rewinds once per batch rather than once per
    /// item: a host that gets three string results back has to be able to read
    /// all three. Rewinding per item would leave the first two stale.
    #[test]
    fn every_string_result_of_a_batch_stays_readable_after_it_returns() {
        let mut e = Engine::new();
        let items = e.exec_many(&[r#""a" + "1""#, r#""b" + "2""#, r#""c" + "3""#]);
        let text: Vec<String> = items.iter().map(|i| e.render(i.value)).collect();
        assert_eq!(text, vec!["a1", "b2", "c3"]);
        for item in &items {
            let addr = item.value.as_ptr().unwrap() as usize;
            assert!(e.egress().owns(addr), "promoted into the rewinding block");
            assert!(StrRef::checked(item.value, e.arena()).is_none());
        }
        assert!(
            e.egress().bytes_in_use() > 0,
            "three results are live at once, not one"
        );
    }

    #[test]
    fn a_failing_item_does_not_stop_the_batch() {
        let mut e = Engine::new();
        let items = e.exec_many(&["1 +", "2 + 3", "?", "\"still runs\""]);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].error.as_ref().unwrap().kind, ErrorKind::Parse);
        assert_eq!(items[1].value.as_f64(), Some(5.0));
        assert_eq!(items[2].error.as_ref().unwrap().kind, ErrorKind::Lex);
        assert!(items[3].is_ok());
        assert_eq!(e.render(items[3].value), "still runs");
    }

    /// A host failure propagates through `?` and skips the depth decrement, so
    /// the count is re-zeroed per item. A hundred failures must not leave the
    /// next good call one step from the ceiling.
    #[test]
    fn call_depth_does_not_leak_from_a_failing_item_into_the_next() {
        let mut e = Engine::new();
        e.register_host_fn("boom", 0, |_engine, _args| {
            HostOutcome::Failure(b"no".to_vec())
        })
        .unwrap();
        add_square(&mut e);
        let mut sources: Vec<&str> = (0..MAX_CALL_DEPTH * 2).map(|_| "boom()").collect();
        sources.push("square(6)");
        let items = e.exec_many(&sources);
        assert!(
            items[..items.len() - 1]
                .iter()
                .all(|i| { i.error.as_ref().map(|e| e.kind) == Some(ErrorKind::Host) })
        );
        let last = items.last().unwrap();
        assert!(last.is_ok(), "depth leaked: {:?}", last.error);
        assert_eq!(last.value.as_f64(), Some(36.0));
    }

    #[test]
    fn an_empty_batch_is_a_no_op_that_expires_nothing() {
        let mut e = Engine::new();
        let held = e.exec("\"keep me\"").unwrap();
        let execs = e.stats().execs;
        assert!(e.exec_many(&[]).is_empty());
        assert_eq!(e.stats().execs, execs, "no execution was started");
        assert_eq!(
            e.string_of(held).map(|b| b.to_vec()),
            Some(b"keep me".to_vec()),
            "an empty queue must not rewind the block a host is still reading"
        );
    }

    #[test]
    fn a_batch_counts_one_execution_per_item() {
        let mut e = Engine::new();
        e.exec("1").unwrap();
        assert_eq!(e.exec_many(&["2", "3", "?"]).len(), 3);
        assert_eq!(
            e.stats().execs,
            4,
            "one exec plus three items, failures included"
        );
    }

    #[test]
    fn a_batch_leaves_the_last_item_state_behind() {
        let mut e = Engine::new();
        e.exec_many(&["7", "?"]);
        assert_eq!(e.last_value().as_f64(), Some(7.0));
        assert_eq!(e.last_error().unwrap().kind, ErrorKind::Lex);
        e.exec_many(&["?", "9"]);
        assert_eq!(e.last_value().as_f64(), Some(9.0));
        assert!(e.last_error().is_none(), "a later success clears the error");
    }

    /// The amortisation claim, made checkable. The result vector is the batch's
    /// one unavoidable heap request — the FFI writes into caller-owned arrays
    /// instead and pays even that zero. Everything else is warm reuse.
    #[test]
    fn a_warm_batch_allocates_only_the_vector_it_hands_back() {
        let mut e = Engine::new();
        let sources = ["let answer = 40 + 2; answer", r#""hello, " + "world""#];
        for _ in 0..64 {
            e.exec_many(&sources);
        }
        let before = crate::counted::calls();
        for _ in 0..1000 {
            let items = e.exec_many(&sources);
            assert_eq!(items.len(), 2);
        }
        assert_eq!(
            crate::counted::calls() - before,
            1000,
            "one allocation per batch, and it is the result vector"
        );
    }

    /// Bounded reuse at batch scale: the same batch run ten thousand times must
    /// reserve the same bytes, because the egress block rewinds once and reuses
    /// what the first batch mapped.
    #[test]
    fn repeated_batches_reserve_a_flat_amount_of_memory() {
        let mut e = Engine::new();
        let sources = ["let x = 1; x", r#""a" + "b" + "c""#, "square(3)"];
        add_square(&mut e);
        e.exec_many(&sources);
        let reserved = e.stats().egress_bytes_reserved;
        let high_water = e.stats().egress_high_water;
        let live_before = crate::counted::live();
        for round in 0..10_000 {
            let items = e.exec_many(&sources);
            assert!(items.iter().all(BatchItem::is_ok), "round {round}");
            assert_eq!(
                e.stats().egress_bytes_reserved,
                reserved,
                "round {round}: the block grew instead of being reused"
            );
        }
        assert_eq!(e.stats().egress_high_water, high_water);
        assert_eq!(crate::counted::live(), live_before, "the loop leaked");
    }

    fn text_of(e: &mut Engine, src: &str) -> String {
        let v = e
            .exec(src)
            .unwrap_or_else(|err| panic!("{src} failed: {err}"));
        e.render(v)
    }

    // -- host functions -------------------------------------------------------

    /// A number-times-itself callback, the smallest thing that proves a call works.
    fn add_square(e: &mut Engine) {
        e.register_host_fn("square", 1, |_engine, args| match args[0].as_f64() {
            Some(n) => HostOutcome::Number(n * n),
            None => HostOutcome::Failure(b"needs a number".to_vec()),
        })
        .unwrap();
    }

    /// Upper-cases a string argument, which is the first callback that has to read
    /// the engine's bytes rather than just its bits.
    fn add_shout(e: &mut Engine) {
        e.register_host_fn("shout", 1, |engine, args| match engine.string_of(args[0]) {
            Some(bytes) => {
                let up = String::from_utf8_lossy(bytes).to_uppercase().into_bytes();
                HostOutcome::String(up)
            }
            None => HostOutcome::Failure(b"needs a string".to_vec()),
        })
        .unwrap();
    }

    #[test]
    fn a_host_function_returns_a_number() {
        let mut e = Engine::new();
        add_square(&mut e);
        assert_eq!(e.exec("square(6)"), Ok(num(36.0)));
    }

    #[test]
    fn a_host_function_sits_in_an_expression() {
        let mut e = Engine::new();
        add_square(&mut e);
        assert_eq!(e.exec("square(6) + 1"), Ok(num(37.0)));
        assert_eq!(e.exec("square(2 + 3)"), Ok(num(25.0)));
        assert_eq!(e.exec("-square(4)"), Ok(num(-16.0)));
        // Calls bind tighter than `==`, so this compares 36 to 36, not 6 to 36.
        assert_eq!(e.exec("square(6) == 36"), Ok(Value::TRUE));
    }

    #[test]
    fn a_host_function_returns_a_string_the_engine_can_read() {
        let mut e = Engine::new();
        add_shout(&mut e);
        assert_eq!(text_of(&mut e, "shout(\"hi\")"), "HI");
        assert_eq!(text_of(&mut e, "shout(\"hi\") + \"!\""), "HI!");
        assert_eq!(e.exec("shout(\"a\") == \"A\""), Ok(Value::TRUE));
        assert_eq!(e.exec("shout(\"a\") == \"a\""), Ok(Value::FALSE));
    }

    #[test]
    fn a_host_string_result_lives_in_egress_like_any_other_result() {
        // The promotion rule cannot know about host callbacks, so it has to cover
        // them: a string returned by a host lands in the arena during the call and
        // is copied to egress on the way out, exactly like a concatenation.
        let mut e = Engine::new();
        add_shout(&mut e);
        let grown = {
            e.exec("shout(\"hi\")").unwrap();
            e.stats()
        };
        for _ in 0..50 {
            e.exec("shout(\"hi\")").unwrap();
        }
        let again = e.stats();
        assert_eq!(again.egress_bytes_reserved, grown.egress_bytes_reserved);
        assert_eq!(again.egress_high_water, grown.egress_high_water);
    }

    #[test]
    fn zero_arity_and_max_arity_calls_work() {
        let mut e = Engine::new();
        e.register_host_fn("pi", 0, |_, _| HostOutcome::Number(std::f64::consts::PI))
            .unwrap();
        e.register_host_fn("sum16", 16, |_, args| {
            let total: f64 = args.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum();
            HostOutcome::Number(total)
        })
        .unwrap();
        assert_eq!(e.exec("pi()"), Ok(num(std::f64::consts::PI)));
        assert_eq!(
            e.exec("pi(1)"),
            Err(OmniError::new(
                ErrorKind::Runtime,
                Pos::new(1, 1),
                "Mismatched arity in call 'pi': takes 0 arguments, got 1",
            ))
        );
        let src = format!(
            "sum16({})",
            (1..=16)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(e.exec(&src), Ok(num(136.0)));
    }

    #[test]
    fn a_nested_call_evaluates_inside_out() {
        let mut e = Engine::new();
        add_square(&mut e);
        e.register_host_fn("double", 1, |_, args| match args[0].as_f64() {
            Some(n) => HostOutcome::Number(n * 2.0),
            None => HostOutcome::Failure(Vec::new()),
        })
        .unwrap();
        assert_eq!(e.exec("double(square(3))"), Ok(num(18.0)));
        assert_eq!(e.exec("square(double(3))"), Ok(num(36.0)));
    }

    #[test]
    fn a_missing_comma_names_the_comma() {
        let mut e = Engine::new();
        add_square(&mut e);
        let err = e.exec("square(1 2)").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Parse);
        assert_eq!(
            err.message,
            "expected ',' or ')' in the call to 'square' (found 'a number')"
        );
    }

    #[test]
    fn an_unterminated_call_names_the_function_it_left_open() {
        let mut e = Engine::new();
        add_square(&mut e);
        let err = e.exec("square(2").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Parse);
        // The lexer ends the stream with an Eof token rather than running out, so
        // this is the call's own message and not a bare "unexpected end of input".
        assert_eq!(
            err.message,
            "expected ',' or ')' in the call to 'square' (found 'end of input')"
        );
        assert_eq!(err.line, 1);
        // The line moves with the cursor, so a call left open on line 2 reports 2.
        let err = e.exec("let unused = 1\nsquare(2").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Parse);
        assert_eq!(err.line, 2);
    }

    #[test]
    fn arity_is_the_registrations_contract() {
        let mut e = Engine::new();
        add_square(&mut e);
        let err = e.exec("square(1, 2)").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert_eq!(
            err.message,
            "Mismatched arity in call 'square': takes 1 argument, got 2"
        );
        assert_eq!(err.line, 1);
    }

    #[test]
    fn an_unregistered_name_is_a_runtime_error_that_says_so() {
        let mut e = Engine::new();
        let err = e.exec("nope(1)").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert_eq!(
            err.message,
            "unknown function 'nope' (no host function or builtin has that name)"
        );
    }

    #[test]
    fn a_host_failure_is_its_own_error_kind_and_keeps_its_message() {
        let mut e = Engine::new();
        add_square(&mut e);
        let err = e.exec("square(\"two\")").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Host);
        assert_eq!(err.message, "host function 'square' failed: needs a number");
    }

    #[test]
    fn a_host_failure_with_nothing_to_say_still_names_the_function() {
        let mut e = Engine::new();
        e.register_host_fn("quiet", 0, |_, _| HostOutcome::Failure(Vec::new()))
            .unwrap();
        let err = e.exec("quiet()").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Host);
        assert_eq!(err.message, "host function 'quiet' failed");
    }

    /// `id(id(id(1)))` with the given number of frames.
    fn nest(depth: usize) -> String {
        format!("{}1{}", "id(".repeat(depth), ")".repeat(depth))
    }

    #[test]
    fn call_nesting_stops_at_the_limit_instead_of_the_stack() {
        let mut e = Engine::new();
        e.register_host_fn("id", 1, |_, args| {
            HostOutcome::Number(args[0].as_f64().unwrap_or(0.0))
        })
        .unwrap();
        let limit = MAX_CALL_DEPTH as usize;
        assert_eq!(e.exec(&nest(limit)), Ok(num(1.0)), "at the limit");
        let err = e.exec(&nest(limit + 1)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        assert!(
            err.message.contains(&format!("nested {limit} deep")),
            "{err}"
        );
        // The counter lives per execution: an aborted nest must not strand it.
        assert_eq!(e.exec("id(1)"), Ok(num(1.0)));
        assert_eq!(e.exec(&nest(limit)), Ok(num(1.0)));
    }

    #[test]
    fn a_left_deep_spine_lowers_without_growing_the_native_stack() {
        let mut e = Engine::new();
        // The same shape as the Python SDK's long-source row, two orders of
        // magnitude larger: a left-deep `+` spine. The parser builds it in a
        // loop, and lowering walks a work stack, so the only limit is memory.
        let source = format!("1{}", " + 1".repeat(100_000));
        assert_eq!(e.exec(&source), Ok(num(100_001.0)));
    }

    #[test]
    fn more_arguments_than_the_ceiling_is_a_parse_error() {
        let mut e = Engine::new();
        add_square(&mut e);
        let src = format!("square({})", vec!["1"; MAX_ARGS + 1].join(","));
        let err = e.exec(&src).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Parse);
        assert!(
            err.message
                .contains(&format!("at most {MAX_ARGS} arguments")),
            "{err}"
        );
    }

    /// A named callback, so one item can be registered several times — a closure
    /// variable would move into the first call.
    fn nil_call(_: &Engine, _: &[Value]) -> HostOutcome {
        HostOutcome::Nil
    }

    #[test]
    fn registration_rejects_bad_names_and_arities() {
        let mut e = Engine::new();
        assert_eq!(
            e.register_host_fn("", 0, nil_call),
            Err(HostError::EmptyName)
        );
        assert_eq!(
            e.register_host_fn(&"n".repeat(MAX_IDENTIFIER_LEN + 1), 0, nil_call),
            Err(HostError::NameTooLong)
        );
        assert_eq!(
            e.register_host_fn("wide", (MAX_ARGS + 1) as u32, nil_call),
            Err(HostError::ArityTooLarge)
        );
        assert_eq!(e.host_fn_count(), 0, "a rejected name registers nothing");
    }

    #[test]
    fn a_host_function_outlives_every_execution_until_unregistered() {
        let mut e = Engine::new();
        add_square(&mut e);
        assert_eq!(e.exec("square(2)"), Ok(num(4.0)));
        // The arena reset and the egress rewind are both between these two calls;
        // the registry is the one block that does neither.
        assert_eq!(e.exec("let unused = 1"), Ok(num(1.0)));
        assert_eq!(e.exec("square(3)"), Ok(num(9.0)));
        assert_eq!(e.host_fn_count(), 1);
        assert!(e.unregister_host_fn("square"));
        assert!(!e.unregister_host_fn("square"));
        assert_eq!(e.host_fn_count(), 0);
        assert!(e.exec("square(3)").is_err());
    }

    #[test]
    fn redefining_a_name_replaces_the_callback() {
        let mut e = Engine::new();
        e.register_host_fn("which", 0, |_, _| HostOutcome::Number(1.0))
            .unwrap();
        e.register_host_fn("which", 0, |_, _| HostOutcome::Number(2.0))
            .unwrap();
        assert_eq!(e.host_fn_count(), 1);
        assert_eq!(e.exec("which()"), Ok(num(2.0)));
    }

    #[test]
    fn a_global_and_a_host_function_can_share_a_name() {
        let mut e = Engine::new();
        add_square(&mut e);
        e.define("square", num(7.0));
        assert_eq!(e.exec("square"), Ok(num(7.0)));
        assert_eq!(e.exec("square(4)"), Ok(num(16.0)));
    }

    // -- builtins -------------------------------------------------------------

    #[test]
    fn math_builtins_cover_the_standard_set() {
        assert_eq!(ev("abs(-3)"), num(3.0));
        assert_eq!(ev("abs(3)"), num(3.0));
        assert_eq!(ev("min(2, 5)"), num(2.0));
        assert_eq!(ev("max(2, 5)"), num(5.0));
        assert_eq!(ev("clamp(12, 0, 10)"), num(10.0));
        assert_eq!(ev("clamp(-1, 0, 10)"), num(0.0));
        assert_eq!(ev("clamp(4, 0, 10)"), num(4.0));
        assert_eq!(ev("floor(2.9)"), num(2.0));
        assert_eq!(ev("ceil(2.1)"), num(3.0));
        assert_eq!(ev("round(2.5)"), num(3.0));
        assert_eq!(ev("round(-2.5)"), num(-3.0), "half away from zero");
        assert_eq!(ev("pow(2, 10)"), num(1024.0));
        assert_eq!(ev("pow(9, 0.5)"), num(3.0));
        assert_eq!(ev("sqrt(16)"), num(4.0));
        assert_eq!(ev("round(sqrt(2) * sqrt(2))"), num(2.0));
        // Results are ordinary numbers and compose with the rest of the grammar.
        assert_eq!(ev("abs(-2) + max(1, 2) * floor(2.9)"), num(6.0));
    }

    #[test]
    fn sqrt_refuses_a_negative_and_pow_refuses_an_infinity() {
        let e = Engine::new().exec("sqrt(0 - 1)").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!((e.line, e.column), (1, 1), "the error points at the call");
        assert!(e.message.contains("not negative"), "{e}");

        let e = Engine::new().exec("pow(10, 400)").unwrap_err();
        assert!(e.message.contains("no finite result"), "{e}");
    }

    #[test]
    fn clamp_refuses_an_inverted_range() {
        let e = Engine::new().exec("clamp(1, 10, 0)").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!((e.line, e.column), (1, 1), "the error points at the call");
        assert!(e.message.contains("lower bound"), "message was {e}");
    }

    #[test]
    fn string_builtins_cover_the_standard_set() {
        assert_eq!(text("len(\"hello\")"), "5");
        // Bytes, not characters: "é" is two bytes in UTF-8.
        assert_eq!(text("len(\"héllo\")"), "6");
        assert_eq!(text("trim(\"  padded\\t\\n\")"), "padded");
        assert_eq!(text("upper(\"MiXeD\")"), "MIXED");
        assert_eq!(text("lower(\"MiXeD\")"), "mixed");
        // Case mapping is Unicode, so this expands rather than passing through.
        assert_eq!(text("upper(\"straße\")"), "STRASSE");
        // Helpers compose, and the inner result stays readable for the outer.
        assert_eq!(text("upper(trim(\"  hi  \"))"), "HI");
    }

    #[test]
    fn lookup_and_editing_builtins_work_on_bytes_not_surprises() {
        assert_eq!(ev("contains(\"omni engine\", \"engine\")"), Value::TRUE);
        assert_eq!(ev("contains(\"omni\", \"zz\")"), Value::FALSE);
        // The empty needle is contained in every string, including an empty one.
        assert_eq!(ev("contains(\"\", \"\")"), Value::TRUE);
        assert_eq!(text("replace(\"a-b-a\", \"-\", \"+\")"), "a+b+a");
        assert_eq!(
            text("replace(\"aaa\", \"aa\", \"b\")"),
            "ba",
            "left to right"
        );
        assert_eq!(text("replace(\"abc\", \"z\", \"y\")"), "abc");
        // An empty needle is a no-op rather than an insertion everywhere.
        assert_eq!(text("replace(\"abc\", \"\", \"x\")"), "abc");
        // Character offsets: "héllo" sliced at 1..3 is "él", not half of "é".
        assert_eq!(text("slice(\"héllo\", 1, 3)"), "él");
        assert_eq!(text("slice(\"hello\", 0, 2)"), "he");
        assert_eq!(text("slice(\"hello\", 2, 99)"), "llo", "clamped to the end");
        assert_eq!(
            text("slice(\"hello\", 3, 1)"),
            "",
            "an inverted range is empty"
        );
        assert_eq!(
            text("slice(\"hello\", 0 - 4, 2)"),
            "he",
            "a negative start clamps"
        );

        // One engine, two calls: the scratch buffer the edit builtins share must
        // not carry the previous answer into the next prefix.
        let mut e = Engine::new();
        let replaced = e.exec("replace(\"abc\", \"\", \"x\")").unwrap();
        assert_eq!(e.render(replaced), "abc");
        let sliced = e.exec("slice(\"héllo\", 1, 3)").unwrap();
        assert_eq!(e.render(sliced), "él");
    }

    #[test]
    fn type_of_names_every_kind_the_language_can_carry() {
        assert_eq!(text("type_of(2)"), "number");
        assert_eq!(text("type_of(\"x\")"), "string");
        assert_eq!(text("type_of(1 == 1)"), "boolean");
        // No literal exists for nil yet, so the host is the way to get one.
        let mut e = Engine::new();
        e.register_host_fn("nothing", 0, nil_call).unwrap();
        let value = e.exec("type_of(nothing())").unwrap();
        assert_eq!(e.render(value), "null");
    }

    #[test]
    fn string_builtins_reuse_the_scratch_buffer() {
        let mut e = Engine::new();
        let warm = e.exec("upper(\"warm\")").unwrap();
        assert_eq!(e.render(warm), "WARM");
        let warmed = e.scratch.capacity();
        for _ in 0..100 {
            let value = e.exec("upper(\"again\")").unwrap();
            assert_eq!(e.render(value), "AGAIN");
        }
        assert_eq!(e.scratch.capacity(), warmed, "no per-call growth");
    }

    #[test]
    fn builtins_report_a_type_mismatch_at_the_call_site() {
        let e = Engine::new().exec("abs(\"x\")").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!((e.line, e.column), (1, 1));
        assert!(e.message.starts_with("type mismatch: abs() needs a number"));
        let e = Engine::new().exec("trim(1)").unwrap_err();
        assert!(
            e.message
                .starts_with("type mismatch: trim() needs a string")
        );
        let e = Engine::new().exec("len(1)").unwrap_err();
        assert!(e.message.starts_with("type mismatch: len() needs a string"));
    }

    #[test]
    fn a_builtin_arity_mismatch_names_the_call() {
        let e = Engine::new().exec("abs(1, 2)").unwrap_err();
        assert_eq!(
            e.message,
            "Mismatched arity in call 'abs': takes 1 argument, got 2"
        );
        assert_eq!((e.line, e.column), (1, 1), "the error points at the call");
        let e = Engine::new().exec("min(1)").unwrap_err();
        assert_eq!(
            e.message,
            "Mismatched arity in call 'min': takes 2 arguments, got 1"
        );
    }

    #[test]
    fn a_host_registration_shadows_a_builtin_of_the_same_name() {
        let mut e = Engine::new();
        e.register_host_fn("abs", 1, |_, _| HostOutcome::Number(99.0))
            .unwrap();
        assert_eq!(e.exec("abs(-1)"), Ok(num(99.0)));
        assert!(e.unregister_host_fn("abs"));
        assert_eq!(e.exec("abs(-1)"), Ok(num(1.0)));
    }

    // -- positions and the timer ---------------------------------------------

    #[test]
    fn a_runtime_error_points_at_the_operator_on_its_own_line() {
        let e = Engine::new()
            .exec("let a = 1\nlet b = \"x\" + 2")
            .unwrap_err();
        assert_eq!((e.line, e.column), (2, 13), "the '+' on the second line");
        assert_eq!(
            e.to_string(),
            "[Line 2, Col 13]: type mismatch: cannot add string to number: \"x\" + 2"
        );
    }

    #[test]
    fn columns_count_bytes_so_multibyte_text_does_not_shift_the_operator() {
        // `"é" + 1`: the quote, two bytes of é, the closing quote, a space, then
        // the '+'. A character column would say 5; the lexer's byte column says 6.
        let e = Engine::new().exec("\"é\" + 1").unwrap_err();
        assert_eq!((e.line, e.column), (1, 6));
    }

    #[test]
    fn a_lex_error_carries_the_position_the_lexer_reported() {
        let e = Engine::new().exec("1\n\n@").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Lex);
        assert_eq!((e.line, e.column), (3, 1));
        assert_eq!(e.to_string(), "[Line 3, Col 1]: unexpected character '@'");
    }

    #[test]
    fn the_timer_saturates_instead_of_wrapping() {
        assert_eq!(elapsed_micros(Duration::from_micros(0)), 0);
        assert_eq!(elapsed_micros(Duration::from_micros(1_500)), 1_500);
        assert_eq!(elapsed_micros(Duration::from_secs(100_000)), u32::MAX);
    }

    #[test]
    fn stats_report_the_timer_of_the_most_recent_execution() {
        let mut e = Engine::new();
        assert_eq!(e.stats().last_exec_micros, 0, "nothing has run yet");
        let src = format!("\"{}\"", "x".repeat(200_000));
        e.exec(&src).unwrap();
        let micros = e.stats().last_exec_micros;
        assert!(micros > 0, "a 200 KB program takes measurable time");
        assert_eq!(micros, e.last_exec_micros);
    }

    // -- VM control flow ------------------------------------------------------

    fn run_chunk(e: &mut Engine, code: Vec<Instr>, names: &[u8]) -> Result<Value, OmniError> {
        let chunk = Chunk {
            code,
            names: names.to_vec(),
        };
        let mut stack: Stack = Vec::new();
        e.run_chunk(&chunk, "", &mut stack)
    }

    #[test]
    fn jump_skips_the_instructions_it_bounds() {
        let at = Pos::new(1, 1);
        let code = vec![
            Instr::new(OpCode::Jump, 2, 0, 0, at),
            Instr::new(OpCode::PushUndef, 0, 0, 0, at),
            Instr::new(OpCode::PushNil, 0, 0, 0, at),
            Instr::new(OpCode::Ret, 0, 0, 0, at),
        ];
        assert_eq!(
            run_chunk(&mut Engine::new(), code, b"").unwrap(),
            Value::NIL
        );
    }

    #[test]
    fn jump_if_false_takes_the_else_branch_and_falls_through_otherwise() {
        let at = Pos::new(1, 1);
        // if false: push nil; jump-if-false to the else arm.
        let false_code = vec![
            Instr::new(OpCode::PushNil, 0, 0, 0, at),
            Instr::new(OpCode::JumpIfFalse, 4, 0, 0, at),
            Instr::new(OpCode::PushUndef, 0, 0, 0, at),
            Instr::new(OpCode::Jump, 5, 0, 0, at),
            Instr::new(OpCode::PushNil, 0, 0, 0, at),
            Instr::new(OpCode::Ret, 0, 0, 0, at),
        ];
        assert_eq!(
            run_chunk(&mut Engine::new(), false_code, b"").unwrap(),
            Value::NIL
        );

        // if true: the same shape, but the condition loads a host boolean.
        let true_code = vec![
            Instr::new(OpCode::Load, 0, 3, 0, at),
            Instr::new(OpCode::JumpIfFalse, 4, 0, 0, at),
            Instr::new(OpCode::PushUndef, 0, 0, 0, at),
            Instr::new(OpCode::Jump, 5, 0, 0, at),
            Instr::new(OpCode::PushNil, 0, 0, 0, at),
            Instr::new(OpCode::Ret, 0, 0, 0, at),
        ];
        let mut e = Engine::new();
        e.define("yes", Value::bool(true));
        assert_eq!(run_chunk(&mut e, true_code, b"yes").unwrap(), Value::UNDEF);
    }

    #[test]
    fn ret_returns_the_top_of_stack_and_stops() {
        let at = Pos::new(1, 1);
        let code = vec![
            Instr::new(OpCode::PushNil, 0, 0, 0, at),
            Instr::new(OpCode::Ret, 0, 0, 0, at),
            Instr::new(OpCode::PushUndef, 0, 0, 0, at),
            Instr::new(OpCode::Ret, 0, 0, 0, at),
        ];
        assert_eq!(
            run_chunk(&mut Engine::new(), code, b"").unwrap(),
            Value::NIL
        );
    }

    #[test]
    fn an_unbalanced_chunk_reports_a_vm_fault_instead_of_panicking() {
        let at = Pos::new(1, 1);
        let code = vec![
            Instr::new(OpCode::Add, 0, 0, 0, at),
            Instr::new(OpCode::Ret, 0, 0, 0, at),
        ];
        let error = run_chunk(&mut Engine::new(), code, b"").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Panic);
    }

    // -- modules --------------------------------------------------------------

    /// A fresh, empty directory under the system temp dir. The label keeps
    /// concurrently running tests from sharing one root.
    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omni-phase6-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_module(root: &Path, name: &str, source: &str) {
        std::fs::write(root.join(format!("{name}.omni")), source).unwrap();
    }

    #[test]
    fn imports_bind_public_exports_and_hide_underscore_names() {
        let root = temp_root("exports");
        write_module(
            &root,
            "math",
            "let answer = 40 + 2\nlet name = \"omni\"\nlet _hidden = 9",
        );
        let mut e = Engine::new();
        e.set_module_root(&root).unwrap();

        let value = e
            .exec("import \"math\"\nmath.answer + len(math.name)")
            .unwrap();
        assert_eq!(value.as_f64(), Some(46.0));
        // The visibility message is available in the execution that imported the
        // module, because that is the execution whose namespace table knows it.
        let error = e.exec("import \"math\"\nmath._hidden").unwrap_err();
        assert_eq!(
            error.message,
            "module 'math' has no public export '_hidden'"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn imports_are_disabled_without_a_module_root() {
        let error = Engine::new().exec("import \"math\"").unwrap_err();
        assert_eq!(
            error.message,
            "import is disabled: this engine has no module root"
        );
    }

    #[test]
    fn a_missing_module_names_the_request() {
        let root = temp_root("missing");
        let mut e = Engine::new();
        e.set_module_root(&root).unwrap();
        let error = e.exec("import \"nope\"").unwrap_err();
        assert_eq!(
            error.message,
            "cannot import 'nope': no such module under the module root"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_import_cannot_escape_the_root() {
        let root = temp_root("escape");
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join("outside.omni"), "let x = 1").unwrap();
        let mut e = Engine::new();
        e.set_module_root(&inner).unwrap();
        let error = e.exec("import \"../outside\"").unwrap_err();
        assert_eq!(error.message, "import '../outside' escapes the module root");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_module_cannot_see_the_parents_globals_or_callbacks() {
        let root = temp_root("isolation");
        write_module(&root, "probe", "let x = secret");
        write_module(&root, "call", "secret()");
        let mut e = Engine::new();
        e.define("secret", num(7.0));
        e.register_host_fn("secret", 0, nil_call).unwrap();
        e.set_module_root(&root).unwrap();

        let error = e.exec("import \"probe\"").unwrap_err();
        assert_eq!(
            error.message,
            "in module 'probe': undefined variable 'secret'"
        );
        let error = e.exec("import \"call\"").unwrap_err();
        assert_eq!(
            error.message,
            "in module 'call': unknown function 'secret' (no host function or builtin has that name)"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_module_error_keeps_its_prefix_and_position() {
        let root = temp_root("error-prefix");
        write_module(&root, "bad", "let x = 1\nlet y = 1 / 0");
        let mut e = Engine::new();
        e.set_module_root(&root).unwrap();
        let error = e.exec("import \"bad\"").unwrap_err();
        assert_eq!(error.message, "in module 'bad': division by zero");
        assert_eq!(error.line, 2, "the module's own line survives");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_circular_import_names_the_whole_chain() {
        let root = temp_root("cycle");
        write_module(&root, "a", "import \"b\"\nlet x = 1");
        write_module(&root, "b", "import \"a\"\nlet y = 2");
        let mut e = Engine::new();
        e.set_module_root(&root).unwrap();
        let error = e.exec("import \"a\"").unwrap_err();
        // Each enclosing module adds its own prefix on the way out; the cycle
        // itself is named once, with every module on it.
        assert!(
            error.message.ends_with("circular import: a -> b -> a"),
            "{error}"
        );
        assert_eq!(error.line, 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn import_depth_is_bounded_at_sixteen() {
        let root = temp_root("depth");
        for index in 0..MAX_MODULE_DEPTH {
            write_module(
                &root,
                &format!("m{index}"),
                &format!("import \"m{}\"", index + 1),
            );
        }
        write_module(&root, &format!("m{MAX_MODULE_DEPTH}"), "let end = 1");
        let mut e = Engine::new();
        e.set_module_root(&root).unwrap();
        let error = e.exec("import \"m0\"").unwrap_err();
        assert!(
            error.message.contains(&format!(
                "imports are nested {MAX_MODULE_DEPTH} deep, the limit"
            )),
            "{error}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn module_exports_do_not_leak_between_batch_items() {
        let root = temp_root("batch");
        write_module(&root, "math", "let answer = 42");
        let mut e = Engine::new();
        e.set_module_root(&root).unwrap();
        let items = e.exec_many(&["import \"math\"\nmath.answer", "math.answer"]);
        assert!(items[0].is_ok(), "{:?}", items[0].error);
        let error = items[1].error.as_ref().expect("the second item must fail");
        assert_eq!(error.message, "undefined variable 'math.answer'");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // -- sandboxed filesystem -------------------------------------------------

    #[test]
    fn the_filesystem_fails_closed_until_a_root_is_allowed() {
        let mut e = Engine::new();
        assert_eq!(
            e.exec("fs.read_text(\"x\")").unwrap_err().message,
            "fs.read_text is disabled: this engine has no filesystem read permission"
        );
        assert_eq!(
            e.exec("fs.write_text(\"x\", \"y\")").unwrap_err().message,
            "fs.write_text is disabled: this engine has no filesystem write permission"
        );
    }

    #[test]
    fn an_allowed_root_reads_and_writes_utf8_text() {
        let root = temp_root("fs-ok");
        std::fs::write(root.join("in.txt"), "hello").unwrap();
        let mut e = Engine::new();
        e.allow_fs_read(&root).unwrap();
        e.allow_fs_write(&root).unwrap();
        e.define_string("input", root.join("in.txt").to_str().unwrap().as_bytes())
            .unwrap();
        e.define_string("output", root.join("out.txt").to_str().unwrap().as_bytes())
            .unwrap();

        let read = e.exec("fs.read_text(input)").unwrap();
        assert_eq!(e.render(read), "hello");
        assert_eq!(
            e.exec("fs.write_text(output, \"written\")").unwrap(),
            Value::NIL
        );
        assert_eq!(
            std::fs::read_to_string(root.join("out.txt")).unwrap(),
            "written"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_path_outside_every_root_is_refused_before_io() {
        let root = temp_root("fs-escape");
        let outside = std::env::temp_dir().join(format!(
            "omni-phase6-{}-fs-escape-outside.txt",
            std::process::id()
        ));
        std::fs::write(&outside, "secret").unwrap();
        let mut e = Engine::new();
        e.allow_fs_read(&root).unwrap();
        e.define_string("target", outside.to_str().unwrap().as_bytes())
            .unwrap();
        let error = e.exec("fs.read_text(target)").unwrap_err();
        assert!(
            error.message.ends_with("is outside the allowed read roots"),
            "{error}"
        );
        std::fs::remove_file(&outside).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_non_utf8_file_is_refused_because_language_strings_are_utf8() {
        let root = temp_root("fs-binary");
        std::fs::write(root.join("bin"), [0xff, 0xfe]).unwrap();
        let mut e = Engine::new();
        e.allow_fs_read(&root).unwrap();
        e.define_string("target", root.join("bin").to_str().unwrap().as_bytes())
            .unwrap();
        let error = e.exec("fs.read_text(target)").unwrap_err();
        assert!(error.message.ends_with("is not UTF-8 text"), "{error}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // -- JSON -----------------------------------------------------------------

    #[test]
    fn json_parses_the_four_scalar_kinds() {
        let mut e = Engine::new();
        assert_eq!(e.exec("json.parse(\"42\")").unwrap().as_f64(), Some(42.0));
        let value = e.exec("json.parse(\"\\\"hi\\\"\")").unwrap();
        assert_eq!(e.render(value), "hi");
        assert_eq!(e.exec("json.parse(\"true\")").unwrap(), Value::bool(true));
        assert_eq!(e.exec("json.parse(\"null\")").unwrap(), Value::NIL);
    }

    #[test]
    fn json_collections_are_refused_with_the_abi_reason() {
        let mut e = Engine::new();
        let error = e.exec("json.parse(\"[1, 2]\")").unwrap_err();
        assert_eq!(error.message, format!("json.parse: {}", json::COLLECTION));
        let error = e.exec("json.parse(\"{\\\"a\\\": 1}\")").unwrap_err();
        assert!(error.message.starts_with("json.parse: "), "{error}");
        assert!(error.message.ends_with(json::COLLECTION), "{error}");
    }

    #[test]
    fn json_stringifies_scalars_with_the_required_escapes() {
        assert_eq!(text("json.stringify(42)"), "42");
        assert_eq!(text("json.stringify(1 == 1)"), "true");
        assert_eq!(text("json.stringify(\"a\\nb\")"), "\"a\\nb\"");
        let mut e = Engine::new();
        e.define("nothing", Value::UNDEF);
        assert_eq!(
            e.exec("json.stringify(nothing)").unwrap_err().message,
            "json.stringify: undefined is not a JSON value"
        );
    }
}
