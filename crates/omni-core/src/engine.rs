//! The engine: a fused parse-and-evaluate pass over the grammar.
//!
//! There is no AST type here. That is deliberate rather than an oversight: an
//! AST invented now would be discarded when the bytecode VM lands, and inventing
//! it would also freeze a node shape before the resolver knows what it needs. The
//! parser evaluates while descending, which is enough for expressions, `let`,
//! strings, and comparison, and nothing more.
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
use std::time::{Duration, Instant};

use crate::Value;
use crate::arena::{Arena, Slot};
use crate::lexer::{BinOp, LexError, Token, TokenKind, lex_into};
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
    fn new(line: u32, column: u32) -> Pos {
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
    fn new(kind: ErrorKind, at: Pos, message: impl Into<String>) -> OmniError {
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

/// A source buffer and the tokens lexed from it, borrowed together.
///
/// Not an AST — the parser still evaluates while descending, and there is still
/// no node type. This pairs the two things every parse step needs so the token
/// vector can be *owned by the engine* and refilled per execution. A token that
/// borrowed the source would have made that vector self-referential, which is
/// the whole reason [`Token`] carries offsets instead.
struct Tokens<'a> {
    source: &'a str,
    tokens: &'a [Token],
}

impl<'a> Tokens<'a> {
    /// Resolve one token's text against the source it was lexed from.
    fn text(&self, token: Token) -> &'a str {
        token.text(self.source)
    }

    /// Where a token starts, as a line and byte column.
    fn pos(&self, token: Token) -> Pos {
        Pos::new(token.line, column_at(self.source, token.start as usize))
    }

    /// Where the token at `index` starts; the end of input for an index past it,
    /// which is what an "unexpected end" error has to point at.
    fn pos_at(&self, index: usize) -> Pos {
        match self.tokens.get(index) {
            Some(token) => self.pos(*token),
            None => self.end_pos(),
        }
    }

    fn end_pos(&self) -> Pos {
        let line = self.tokens.last().map_or(1, |token| token.line);
        let column = self
            .source
            .rsplit('\n')
            .next()
            .map_or(1, |tail| tail.len() as u32 + 1);
        Pos::new(line, column)
    }
}

/// One-based byte column of `offset` in `source`.
///
/// `offset` is always a token boundary the lexer produced, and every such
/// boundary is a `char` boundary, so slicing is safe; a caller that pairs an
/// offset with the wrong source gets column 1 rather than a panic.
fn column_at(source: &str, offset: usize) -> u32 {
    let Some(before) = source.get(..offset) else {
        return 1;
    };
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    (offset - line_start + 1) as u32
}

/// The arguments of one call, held on the stack.
///
/// `MAX_ARGS + 1` slots so the argument that breaks the limit has somewhere to
/// land before the overflow is reported — the same shape the allocating version
/// had, minus one heap allocation per call site. [`MAX_CALL_DEPTH`] bounds how
/// many of these can be live at once, which is what makes the stack cost a
/// stated number rather than an open question.
struct Args {
    buf: [Value; MAX_ARGS + 1],
    len: usize,
}

impl Args {
    fn new() -> Args {
        Args {
            buf: [Value::UNDEF; MAX_ARGS + 1],
            len: 0,
        }
    }

    fn push(&mut self, value: Value) {
        self.buf[self.len] = value;
        self.len += 1;
    }

    fn len(&self) -> usize {
        self.len
    }

    fn as_slice(&self) -> &[Value] {
        &self.buf[..self.len]
    }
}

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
    /// Gathering space for one concatenation, reused like the token buffer.
    /// `string_of` borrows the engine and `new_string` needs it mutably, so the
    /// two slices have to be gathered somewhere before the arena is touched;
    /// this is that somewhere, and keeping it means a warm engine concatenates
    /// without asking the system allocator for a buffer per `+`.
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
    /// Live `parse_call` frames, guarded against [`MAX_CALL_DEPTH`].
    call_depth: u32,
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
            scratch: Vec::new(),
            bindings: Vec::new(),
            globals: Vec::new(),
            host_fns: Vec::new(),
            call_depth: 0,
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

    /// Lex into the engine's own buffer, evaluate, and hand the buffer back.
    ///
    /// The take-and-restore is what lets a `Vec` owned by `self` be parsed by
    /// methods that need `&mut self`: the buffer is a local for the duration of
    /// the evaluation, so no borrow of `self` overlaps it. It goes back on the
    /// error path too — a bad program must not cost the next good one a
    /// reallocation.
    fn run(&mut self, source: &str) -> Result<Value, OmniError> {
        let mut tokens = std::mem::take(&mut self.tokens);
        let result = self.run_tokens(source, &mut tokens);
        self.tokens = tokens;
        result
    }

    fn run_tokens(&mut self, source: &str, tokens: &mut Vec<Token>) -> Result<Value, OmniError> {
        lex_into(source, tokens)?;
        let t = Tokens { source, tokens };
        let mut index = 0usize;
        let mut last = Value::UNDEF;
        loop {
            // Statement separators are optional and non-significant.
            while self.peek_kind(&t, index) == Some(TokenKind::Semi) {
                self.advance(&mut index);
            }
            match self.peek_kind(&t, index) {
                None | Some(TokenKind::Eof) => break,
                Some(TokenKind::Let) => {
                    self.advance(&mut index);
                    let name_pos = t.pos_at(index);
                    let name = self.expect_ident(&t, &mut index)?;
                    self.expect(&t, &mut index, TokenKind::Eq)?;
                    let value = self.parse_expr(&t, &mut index)?;
                    self.bind(name, value, name_pos)?;
                    last = value;
                }
                _ => last = self.parse_expr(&t, &mut index)?,
            }
        }
        Ok(last)
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
        // Later bindings shadow earlier ones, and script bindings shadow host ones.
        self.bindings
            .iter()
            .rev()
            .map(|s| s.get())
            .find(|b| b.name() == name)
            .map(|b| b.value())
            .or_else(|| {
                self.globals
                    .iter()
                    .rev()
                    .find(|g| g.name == name)
                    .map(|g| g.value)
            })
    }

    /// Comparison sits one level below `+`, so `"a" + "b" == "ab"` parses as
    /// `("a" + "b") == "ab"` and not as the nonsense the old two-level grammar
    /// would have made of it.
    fn parse_expr(&mut self, t: &Tokens<'_>, i: &mut usize) -> Result<Value, OmniError> {
        let mut acc = self.parse_additive(t, i)?;
        loop {
            let want_equal = match self.peek_kind(t, *i) {
                Some(TokenKind::EqEq) => true,
                Some(TokenKind::NotEq) => false,
                _ => break,
            };
            self.advance(i);
            let rhs = self.parse_additive(t, i)?;
            let same = self.eq_values(acc, rhs);
            acc = Value::bool(if want_equal { same } else { !same });
        }
        Ok(acc)
    }

    fn parse_additive(&mut self, t: &Tokens<'_>, i: &mut usize) -> Result<Value, OmniError> {
        let mut acc = self.parse_term(t, i)?;
        loop {
            let op = match self.peek_kind(t, *i) {
                Some(TokenKind::BinOp(BinOp::Add)) => BinOp::Add,
                Some(TokenKind::BinOp(BinOp::Sub)) => BinOp::Sub,
                _ => break,
            };
            let at = t.pos_at(*i);
            self.advance(i);
            let rhs = self.parse_term(t, i)?;
            acc = self.arith(acc, op, rhs, at)?;
        }
        Ok(acc)
    }

    fn parse_term(&mut self, t: &Tokens<'_>, i: &mut usize) -> Result<Value, OmniError> {
        let mut acc = self.parse_factor(t, i)?;
        loop {
            let op = match self.peek_kind(t, *i) {
                Some(TokenKind::BinOp(BinOp::Mul)) => BinOp::Mul,
                Some(TokenKind::BinOp(BinOp::Div)) => BinOp::Div,
                Some(TokenKind::BinOp(BinOp::Mod)) => BinOp::Mod,
                _ => break,
            };
            let at = t.pos_at(*i);
            self.advance(i);
            let rhs = self.parse_factor(t, i)?;
            acc = self.arith(acc, op, rhs, at)?;
        }
        Ok(acc)
    }

    fn parse_factor(&mut self, t: &Tokens<'_>, i: &mut usize) -> Result<Value, OmniError> {
        let mut negate = false;
        loop {
            match self.peek_kind(t, *i) {
                Some(TokenKind::BinOp(BinOp::Sub)) => {
                    negate = !negate;
                    self.advance(i);
                }
                Some(TokenKind::BinOp(BinOp::Add)) => {
                    self.advance(i);
                }
                _ => break,
            }
        }
        let operand_pos = t.pos_at(*i);
        let value = self.parse_primary(t, i)?;
        if negate {
            let n = value.as_f64().ok_or_else(|| {
                OmniError::new(
                    ErrorKind::Runtime,
                    operand_pos,
                    format!(
                        "type mismatch: cannot apply unary '-' to {}: {}",
                        kind_of(value),
                        self.show(value)
                    ),
                )
            })?;
            return Ok(Value::from_f64(-n));
        }
        Ok(value)
    }

    fn parse_primary(&mut self, t: &Tokens<'_>, i: &mut usize) -> Result<Value, OmniError> {
        let token = self.peek(t, *i).ok_or_else(|| {
            OmniError::new(ErrorKind::Parse, t.pos_at(*i), "unexpected end of input")
        })?;
        match token.kind {
            TokenKind::Number => {
                self.advance(i);
                t.text(token)
                    .parse::<f64>()
                    .map(Value::from_f64)
                    .map_err(|_| {
                        OmniError::new(
                            ErrorKind::Parse,
                            t.pos(token),
                            format!("'{}' is not a valid number", t.text(token)),
                        )
                    })
            }
            TokenKind::Str => {
                self.advance(i);
                // Decoding happens here, not in the lexer, so a token stays a
                // pair of offsets and lexing allocates nothing. The position
                // survives the deferral because the token carries both numbers.
                let at = t.pos(token);
                let bytes = decode_literal(t.text(token), at)?;
                self.new_string(&bytes, at, ErrorKind::Runtime)
            }
            TokenKind::Ident => {
                let name = t.text(token);
                self.advance(i);
                // A name followed by '(' is a call and nothing else; the same name
                // on its own is still a global or binding lookup. Position is the
                // only disambiguation, which is what lets a host register `print`
                // without reserving the identifier.
                if self.peek_kind(t, *i) == Some(TokenKind::LParen) {
                    return self.parse_call(t, i, name, t.pos(token));
                }
                self.lookup(name).ok_or_else(|| {
                    OmniError::new(
                        ErrorKind::Runtime,
                        t.pos(token),
                        format!("undefined variable '{name}'"),
                    )
                })
            }
            TokenKind::LParen => {
                self.advance(i);
                let inner = self.parse_expr(t, i)?;
                self.expect(t, i, TokenKind::RParen)?;
                Ok(inner)
            }
            TokenKind::Eof => Err(OmniError::new(
                ErrorKind::Parse,
                t.pos(token),
                "unexpected end of input",
            )),
            other => Err(self.parse_error_kind(t, *i, other, "expected a value")),
        }
    }

    /// `name(arg, …)` with the call already resolved to a host function or one
    /// of the standard helpers.
    ///
    /// Arity is checked against the registration or the builtin's definition
    /// rather than inferred from what arrived, so `len("a", "b")` names the
    /// function and both counts. A host registration wins over a builtin of the
    /// same name, which is what lets a host instrument `len` without the language
    /// reserving it. The depth guard wraps argument evaluation too: `f(g(x))`
    /// nests here, and the stack the recursion spends is the thing being limited.
    fn parse_call(
        &mut self,
        t: &Tokens<'_>,
        i: &mut usize,
        name: &str,
        at: Pos,
    ) -> Result<Value, OmniError> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!(
                    "calls are nested {MAX_CALL_DEPTH} deep, the limit; a host function \
                     cannot call itself"
                ),
            ));
        }
        let host = self.find_host_fn(name);
        let builtin = builtin_for(name);
        if host.is_none() && builtin.is_none() {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!("unknown function '{name}' (no host function or builtin has that name)"),
            ));
        }
        let arity = match host {
            Some(index) => self.host_fns[index].arity as usize,
            None => builtin.expect("checked above").1,
        };
        self.advance(i); // the '(' that made this a call

        self.call_depth += 1;
        let outcome = self.read_args(t, i, name);
        self.call_depth -= 1;
        let args = outcome?;
        if args.len() != arity {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!(
                    "Mismatched arity in call '{name}': takes {arity} argument{}, got {}",
                    if arity == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }
        match host {
            Some(index) => self.invoke_host(index, name, args.as_slice(), at),
            None => {
                let (which, _) = builtin.expect("checked above");
                self.invoke_builtin(which, name, args.as_slice(), at)
            }
        }
    }

    /// Read `arg, arg, …` up to and including the closing paren.
    ///
    /// Split out of [`Engine::parse_call`] so the depth counter has exactly one
    /// increment and one decrement between them, with a `Result` in the middle —
    /// the shape that makes an early `?` unable to strand the count.
    ///
    /// The arity is not a parameter: the buffer is sized by [`MAX_ARGS`] alone,
    /// so a call costs the same stack whether the host takes one argument or
    /// sixteen, and nothing is allocated to find out which.
    fn read_args(&mut self, t: &Tokens<'_>, i: &mut usize, name: &str) -> Result<Args, OmniError> {
        let mut args = Args::new();
        if self.peek_kind(t, *i) != Some(TokenKind::RParen) {
            loop {
                // The last argument's expression is where an error most often
                // lives, so the position moves with the cursor instead of
                // staying on the function name.
                let arg_pos = t.pos_at(*i);
                args.push(self.parse_expr(t, i)?);
                if args.len() > MAX_ARGS {
                    return Err(OmniError::new(
                        ErrorKind::Parse,
                        arg_pos,
                        format!("a call takes at most {MAX_ARGS} arguments"),
                    ));
                }
                match self.peek_kind(t, *i) {
                    Some(TokenKind::Comma) => self.advance(i),
                    Some(TokenKind::RParen) | None => break,
                    Some(kind) => {
                        return Err(self.parse_error_kind(
                            t,
                            *i,
                            kind,
                            &format!("expected ',' or ')' in the call to '{name}'"),
                        ));
                    }
                }
            }
        }
        self.expect(t, i, TokenKind::RParen)?;
        Ok(args)
    }

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

    // -- token cursor helpers -------------------------------------------------

    fn peek(&self, t: &Tokens<'_>, i: usize) -> Option<Token> {
        // Token is Copy and carries no lifetime, so the cursor hands back a
        // value: sixteen bytes the caller may keep past the next peek.
        t.tokens.get(i).copied()
    }

    fn peek_kind(&self, t: &Tokens<'_>, i: usize) -> Option<TokenKind> {
        t.tokens.get(i).map(|tok| tok.kind)
    }

    fn advance(&self, i: &mut usize) {
        *i += 1;
    }

    fn expect(&self, t: &Tokens<'_>, i: &mut usize, want: TokenKind) -> Result<(), OmniError> {
        match t.tokens.get(*i) {
            Some(tok) if tok.kind == want => {
                self.advance(i);
                Ok(())
            }
            Some(tok) => Err(self.parse_error_kind(t, *i, tok.kind, "unexpected token")),
            None => Err(OmniError::new(
                ErrorKind::Parse,
                t.end_pos(),
                "unexpected end of input",
            )),
        }
    }

    fn expect_ident<'s>(&self, t: &Tokens<'s>, i: &mut usize) -> Result<&'s str, OmniError> {
        match t.tokens.get(*i) {
            Some(tok) if tok.kind == TokenKind::Ident => {
                let text = t.text(*tok);
                self.advance(i);
                Ok(text)
            }
            Some(tok) => Err(OmniError::new(
                ErrorKind::Parse,
                t.pos(*tok),
                format!("expected a name, found '{}'", display_kind(tok.kind)),
            )),
            None => Err(OmniError::new(
                ErrorKind::Parse,
                t.end_pos(),
                "expected a name",
            )),
        }
    }

    fn parse_error_kind(
        &self,
        t: &Tokens<'_>,
        i: usize,
        kind: TokenKind,
        context: &str,
    ) -> OmniError {
        OmniError::new(
            ErrorKind::Parse,
            t.pos_at(i),
            format!("{context} (found '{}')", display_kind(kind)),
        )
    }
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
enum Builtin {
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

fn display_kind(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Let => "'let'",
        TokenKind::Ident => "a name",
        TokenKind::Number => "a number",
        TokenKind::Str => "a string",
        TokenKind::Eq => "'='",
        TokenKind::EqEq => "'=='",
        TokenKind::NotEq => "'!='",
        TokenKind::LParen => "'('",
        TokenKind::RParen => "')'",
        TokenKind::Comma => "','",
        TokenKind::Semi => "';'",
        TokenKind::Eof => "end of input",
        TokenKind::BinOp(op) => op.symbol(),
    }
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
}
