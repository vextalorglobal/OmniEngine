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

use std::fmt;

use crate::Value;
use crate::arena::{Arena, Slot};
use crate::lexer::{BinOp, LexError, Token, TokenKind, lex};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmniError {
    pub kind: ErrorKind,
    pub line: u32,
    pub message: String,
}

impl OmniError {
    fn new(kind: ErrorKind, line: u32, message: impl Into<String>) -> OmniError {
        OmniError {
            kind,
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for OmniError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for OmniError {}

impl From<LexError> for OmniError {
    fn from(e: LexError) -> OmniError {
        OmniError::new(ErrorKind::Lex, e.line, e.message)
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

/// Counters a host reads to prove the memory story from outside Rust.
///
/// Fixed-width and `Copy` because the FFI hands it across the boundary as a
/// `#[repr(C)]` struct; the field order is the wire order and is pinned there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// `exec` calls started, successful or not.
    pub execs: u64,
    /// String blocks built, cumulative. The one counter that is *supposed* to grow.
    pub strings_built: u64,
    pub arena_bytes_in_use: u64,
    pub arena_bytes_reserved: u64,
    pub arena_chunks: u64,
    pub egress_bytes_in_use: u64,
    pub egress_bytes_reserved: u64,
    /// The largest single execution's egress demand — the bound on a rewinding block.
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
    strings_built: u64,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            arena: Arena::new(),
            egress: Arena::new(),
            bindings: Vec::new(),
            globals: Vec::new(),
            host_fns: Vec::new(),
            call_depth: 0,
            last_value: Value::UNDEF,
            last_error: None,
            execs: 0,
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
        self.begin_execution();
        let result = self.run(source);
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

    /// Begin a fresh execution.
    ///
    /// Clears the arena and the binding list together: that pairing is the
    /// invariant [`Slot`] relies on, so neither may happen without the other. The
    /// egress block rewinds rather than resets, because pointers into it are the
    /// ones a host is allowed to still be holding.
    fn begin_execution(&mut self) {
        self.arena.reset();
        self.bindings.clear();
        self.egress.rewind();
        // An error mid-call returns through `?` and skips the decrement, so the
        // count is re-zeroed here rather than trusted to unwind cleanly.
        self.call_depth = 0;
        self.execs += 1;
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
            strings_built: self.strings_built,
            arena_bytes_in_use: u64_from(self.arena.bytes_in_use()),
            arena_bytes_reserved: u64_from(self.arena.bytes_reserved()),
            arena_chunks: u64_from(self.arena.chunk_count()),
            egress_bytes_in_use: u64_from(self.egress.bytes_in_use()),
            egress_bytes_reserved: u64_from(self.egress.bytes_reserved()),
            egress_high_water: u64_from(self.egress.high_water()),
        }
    }

    fn run(&mut self, source: &str) -> Result<Value, OmniError> {
        let tokens = lex(source)?;
        let mut index = 0usize;
        let mut last = Value::UNDEF;
        loop {
            // Statement separators are optional and non-significant.
            while self.peek_kind(&tokens, index) == Some(TokenKind::Semi) {
                self.advance(&mut index);
            }
            match self.peek_kind(&tokens, index) {
                None | Some(TokenKind::Eof) => break,
                Some(TokenKind::Let) => {
                    self.advance(&mut index);
                    let line = self.peek_line(&tokens, index);
                    let name = self.expect_ident(&tokens, &mut index)?;
                    self.expect(&tokens, &mut index, TokenKind::Eq)?;
                    let value = self.parse_expr(&tokens, &mut index)?;
                    self.bind(name, value, line)?;
                    last = value;
                }
                _ => last = self.parse_expr(&tokens, &mut index)?,
            }
        }
        Ok(last)
    }

    /// Allocate the binding's cell in the arena. A name longer than the fixed
    /// inline field is a language error, never a silent truncation — two
    /// different identifiers comparing equal would be far worse than a limit.
    fn bind(&mut self, name: &str, value: Value, line: u32) -> Result<(), OmniError> {
        let binding = Binding::new(name, value).ok_or_else(|| {
            OmniError::new(
                ErrorKind::Parse,
                line,
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
    fn parse_expr(&mut self, t: &[Token<'_>], i: &mut usize) -> Result<Value, OmniError> {
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

    fn parse_additive(&mut self, t: &[Token<'_>], i: &mut usize) -> Result<Value, OmniError> {
        let mut acc = self.parse_term(t, i)?;
        loop {
            let op = match self.peek_kind(t, *i) {
                Some(TokenKind::BinOp(BinOp::Add)) => BinOp::Add,
                Some(TokenKind::BinOp(BinOp::Sub)) => BinOp::Sub,
                _ => break,
            };
            let line = self.peek_line(t, *i);
            self.advance(i);
            let rhs = self.parse_term(t, i)?;
            acc = self.arith(acc, op, rhs, line)?;
        }
        Ok(acc)
    }

    fn parse_term(&mut self, t: &[Token<'_>], i: &mut usize) -> Result<Value, OmniError> {
        let mut acc = self.parse_factor(t, i)?;
        loop {
            let op = match self.peek_kind(t, *i) {
                Some(TokenKind::BinOp(BinOp::Mul)) => BinOp::Mul,
                Some(TokenKind::BinOp(BinOp::Div)) => BinOp::Div,
                Some(TokenKind::BinOp(BinOp::Mod)) => BinOp::Mod,
                _ => break,
            };
            let line = self.peek_line(t, *i);
            self.advance(i);
            let rhs = self.parse_factor(t, i)?;
            acc = self.arith(acc, op, rhs, line)?;
        }
        Ok(acc)
    }

    fn parse_factor(&mut self, t: &[Token<'_>], i: &mut usize) -> Result<Value, OmniError> {
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
        let value = self.parse_primary(t, i)?;
        if negate {
            let n = value.as_f64().ok_or_else(|| {
                OmniError::new(
                    ErrorKind::Runtime,
                    self.peek_line(t, *i),
                    format!(
                        "cannot apply unary '-' to {}: {}",
                        kind_of(value),
                        self.show(value)
                    ),
                )
            })?;
            return Ok(Value::from_f64(-n));
        }
        Ok(value)
    }

    fn parse_primary(&mut self, t: &[Token<'_>], i: &mut usize) -> Result<Value, OmniError> {
        let token = self.peek(t, *i).ok_or_else(|| {
            OmniError::new(
                ErrorKind::Parse,
                self.last_line(t),
                "unexpected end of input",
            )
        })?;
        match token.kind {
            TokenKind::Number => {
                self.advance(i);
                token.text.parse::<f64>().map(Value::from_f64).map_err(|_| {
                    OmniError::new(
                        ErrorKind::Parse,
                        token.line,
                        format!("'{}' is not a valid number", token.text),
                    )
                })
            }
            TokenKind::Str => {
                self.advance(i);
                // Decoding happens here, not in the lexer, so a token stays a
                // borrowed slice and lexing allocates nothing. The line number
                // survives the deferral because the token carries it.
                let bytes = decode_literal(token.text, token.line)?;
                self.new_string(&bytes, token.line, ErrorKind::Runtime)
            }
            TokenKind::Ident => {
                let name = token.text;
                self.advance(i);
                // A name followed by '(' is a call and nothing else; the same name
                // on its own is still a global or binding lookup. Position is the
                // only disambiguation, which is what lets a host register `print`
                // without reserving the identifier.
                if self.peek_kind(t, *i) == Some(TokenKind::LParen) {
                    return self.parse_call(t, i, name, token.line);
                }
                self.lookup(name).ok_or_else(|| {
                    OmniError::new(
                        ErrorKind::Runtime,
                        token.line,
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
                token.line,
                "unexpected end of input",
            )),
            other => Err(self.parse_error_kind(t, *i, other, "expected a value")),
        }
    }

    /// `name(arg, …)` with the call already resolved to a host function.
    ///
    /// Arity is checked against the registration rather than inferred from what
    /// arrived, so `len("a", "b")` names the function and both counts. The depth
    /// guard wraps argument evaluation too: `f(g(x))` nests here, and the stack
    /// the recursion spends is the thing being limited.
    fn parse_call(
        &mut self,
        t: &[Token<'_>],
        i: &mut usize,
        name: &str,
        line: u32,
    ) -> Result<Value, OmniError> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                line,
                format!(
                    "calls are nested {MAX_CALL_DEPTH} deep, the limit; a host function \
                     cannot call itself"
                ),
            ));
        }
        let index = self.find_host_fn(name).ok_or_else(|| {
            OmniError::new(
                ErrorKind::Runtime,
                line,
                format!(
                    "unknown function '{name}' (no host function is registered with that name)"
                ),
            )
        })?;
        let arity = self.host_fns[index].arity as usize;
        self.advance(i); // the '(' that made this a call

        self.call_depth += 1;
        let outcome = self.read_args(t, i, name, arity);
        self.call_depth -= 1;
        let args = outcome?;
        if args.len() != arity {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                line,
                format!(
                    "host function '{name}' takes {arity} argument{}, got {}",
                    if arity == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }
        self.invoke_host(index, name, &args, line)
    }

    /// Read `arg, arg, …` up to and including the closing paren.
    ///
    /// Split out of [`Engine::parse_call`] so the depth counter has exactly one
    /// increment and one decrement between them, with a `Result` in the middle —
    /// the shape that makes an early `?` unable to strand the count.
    fn read_args(
        &mut self,
        t: &[Token<'_>],
        i: &mut usize,
        name: &str,
        arity: usize,
    ) -> Result<Vec<Value>, OmniError> {
        let mut args = Vec::with_capacity(arity);
        if self.peek_kind(t, *i) != Some(TokenKind::RParen) {
            loop {
                // The last argument's expression is where an error most often
                // lives, so the line number moves with the cursor instead of
                // staying on the function name.
                let arg_line = self.peek_line(t, *i);
                args.push(self.parse_expr(t, i)?);
                if args.len() > MAX_ARGS {
                    return Err(OmniError::new(
                        ErrorKind::Parse,
                        arg_line,
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
        line: u32,
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
            HostOutcome::String(bytes) => self.new_string(&bytes, line, ErrorKind::Runtime),
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
                    line,
                    format!("host function '{name}' failed{detail}"),
                ))
            }
        }
    }

    /// The one place a string block enters the execution arena, and so the one
    /// place the length limit is enforced.
    fn new_string(&mut self, bytes: &[u8], line: u32, kind: ErrorKind) -> Result<Value, OmniError> {
        if bytes.len() > MAX_STRING_LEN {
            return Err(OmniError::new(
                kind,
                line,
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
    fn arith(&mut self, lhs: Value, op: BinOp, rhs: Value, line: u32) -> Result<Value, OmniError> {
        if op == BinOp::Add {
            return match (lhs.is_string(), rhs.is_string()) {
                (true, true) => self.concat(lhs, rhs, line),
                (false, false) => self.numeric(lhs, op, rhs, line),
                _ => Err(OmniError::new(
                    ErrorKind::Runtime,
                    line,
                    format!(
                        "cannot add {} to {}: {} + {}",
                        kind_of(lhs),
                        kind_of(rhs),
                        self.show(lhs),
                        self.show(rhs)
                    ),
                )),
            };
        }
        self.numeric(lhs, op, rhs, line)
    }

    /// `string + string`, newly allocated in the execution arena.
    ///
    /// A copy, not a rope: the cost of `let s = s + "x"` in a loop is quadratic
    /// and that is a known property rather than a surprise. The bytes are gathered
    /// into an owned buffer first so the arena borrow and the arena allocation do
    /// not have to overlap.
    fn concat(&mut self, lhs: Value, rhs: Value, line: u32) -> Result<Value, OmniError> {
        let joined = {
            let Some(a) = self.string_of(lhs) else {
                return Err(unreadable_string(line, lhs));
            };
            let Some(b) = self.string_of(rhs) else {
                return Err(unreadable_string(line, rhs));
            };
            let total = a.len() + b.len();
            if total > MAX_STRING_LEN {
                return Err(OmniError::new(
                    ErrorKind::Runtime,
                    line,
                    format!(
                        "concatenation would make a {total}-byte string, over the {} MiB limit",
                        MAX_STRING_LEN / (1024 * 1024)
                    ),
                ));
            }
            let mut buf = Vec::with_capacity(total);
            buf.extend_from_slice(a);
            buf.extend_from_slice(b);
            buf
        };
        self.new_string(&joined, line, ErrorKind::Runtime)
    }

    fn numeric(&self, lhs: Value, op: BinOp, rhs: Value, line: u32) -> Result<Value, OmniError> {
        let (a, b) = match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                return Err(OmniError::new(
                    ErrorKind::Runtime,
                    line,
                    format!(
                        "'{}' needs numbers on both sides (found {} and {})",
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
                return Err(OmniError::new(ErrorKind::Runtime, line, "division by zero"));
            }
            BinOp::Mod if b == 0.0 => {
                return Err(OmniError::new(
                    ErrorKind::Runtime,
                    line,
                    "remainder by zero",
                ));
            }
            BinOp::Div => a / b,
            BinOp::Mod => a % b,
        };
        Ok(Value::from_f64(n))
    }

    // -- token cursor helpers -------------------------------------------------

    fn peek<'t>(&self, t: &[Token<'t>], i: usize) -> Option<Token<'t>> {
        // Token is Copy, so the cursor hands back a value rather than a borrow
        // whose slice lifetime is entangled with the source lifetime.
        t.get(i).copied()
    }

    fn peek_kind(&self, t: &[Token<'_>], i: usize) -> Option<TokenKind> {
        t.get(i).map(|tok| tok.kind)
    }

    fn peek_line(&self, t: &[Token<'_>], i: usize) -> u32 {
        t.get(i).map_or(0, |tok| tok.line)
    }

    fn last_line(&self, t: &[Token<'_>]) -> u32 {
        t.last().map_or(1, |tok| tok.line)
    }

    fn advance(&self, i: &mut usize) {
        *i += 1;
    }

    fn expect(&self, t: &[Token<'_>], i: &mut usize, want: TokenKind) -> Result<(), OmniError> {
        match t.get(*i) {
            Some(tok) if tok.kind == want => {
                self.advance(i);
                Ok(())
            }
            Some(tok) => Err(self.parse_error_kind(t, *i, tok.kind, "unexpected token")),
            None => Err(OmniError::new(
                ErrorKind::Parse,
                self.last_line(t),
                "unexpected end of input",
            )),
        }
    }

    fn expect_ident<'s>(&self, t: &[Token<'s>], i: &mut usize) -> Result<&'s str, OmniError> {
        match t.get(*i) {
            Some(tok) if tok.kind == TokenKind::Ident => {
                let text = tok.text;
                self.advance(i);
                Ok(text)
            }
            Some(tok) => Err(OmniError::new(
                ErrorKind::Parse,
                tok.line,
                format!("expected a name, found '{}'", display_kind(tok.kind)),
            )),
            None => Err(OmniError::new(
                ErrorKind::Parse,
                self.last_line(t),
                "expected a name",
            )),
        }
    }

    fn parse_error_kind(
        &self,
        t: &[Token<'_>],
        i: usize,
        kind: TokenKind,
        context: &str,
    ) -> OmniError {
        OmniError::new(
            ErrorKind::Parse,
            self.peek_line(t, i),
            format!("{context} (found '{}')", display_kind(kind)),
        )
    }
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new()
    }
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
fn unreadable_string(line: u32, value: Value) -> OmniError {
    OmniError::new(
        ErrorKind::Runtime,
        line,
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
fn decode_literal(raw: &str, line: u32) -> Result<Vec<u8>, OmniError> {
    let bytes = raw.as_bytes();
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
                line,
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
                let (end, cp) = decode_unicode_escape(bytes, pos + 2, line)?;
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
                    line,
                    format!("unknown escape '\\{shown}' in a string literal"),
                ));
            }
        }
        pos += 2;
    }
    Ok(out)
}

/// Parse `\u{...}` starting at the byte after `u`.
///
/// Returns the index one past the closing brace and the scalar the braces name.
/// Only scalars are accepted: a surrogate or an over-large value is a parse error
/// that quotes the digits it found, because "invalid unicode escape" tells a
/// reader nothing they can act on.
fn decode_unicode_escape(bytes: &[u8], pos: usize, line: u32) -> Result<(usize, char), OmniError> {
    let parse_error = |message: String| OmniError::new(ErrorKind::Parse, line, message);
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
        assert_eq!(e.to_string(), "line 1: division by zero");
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
            e.message, r#"cannot add string to number: "hi" + 42"#,
            "the message the requester asked for, verbatim"
        );
        let e = Engine::new().exec(r#"42 + "hi""#).unwrap_err();
        assert_eq!(e.message, r#"cannot add number to string: 42 + "hi""#);
        // A boolean operand reaches the same arm. `("a" == "b")` rather than a
        // `true` literal, because the grammar has no boolean literals yet — the
        // only booleans a program can write come out of a comparison.
        let e = Engine::new().exec(r#"("a" == "b") + "hi""#).unwrap_err();
        assert_eq!(e.message, r#"cannot add boolean to string: false + "hi""#);
    }

    #[test]
    fn other_operators_report_the_types_they_found() {
        let e = Engine::new().exec(r#""a" - "b""#).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert_eq!(
            e.message,
            "'-' needs numbers on both sides (found string and string)"
        );
        let e = Engine::new().exec(r#"6 / "b""#).unwrap_err();
        assert_eq!(
            e.message,
            "'/' needs numbers on both sides (found number and string)"
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
        let one = e.new_string(b"ab", 1, ErrorKind::Runtime).unwrap();
        let two = e.new_string(b"ab", 1, ErrorKind::Runtime).unwrap();
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
        let at_the_limit = engine.new_string(&vec![b'x'; MAX_STRING_LEN], 1, ErrorKind::Runtime);
        assert!(at_the_limit.is_ok(), "the limit is inclusive");
        let over = engine
            .new_string(&vec![b'x'; MAX_STRING_LEN + 1], 1, ErrorKind::Runtime)
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
                1,
                "host function 'pi' takes 0 arguments, got 1",
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
            "host function 'square' takes 1 argument, got 2"
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
            "unknown function 'nope' (no host function is registered with that name)"
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
}
