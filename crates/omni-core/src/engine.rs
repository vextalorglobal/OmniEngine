//! The engine: a fused parse-and-evaluate pass over the Phase 1 grammar.
//!
//! There is no AST type here. That is deliberate rather than an oversight: an
//! AST invented now would be discarded when the bytecode VM lands in Phase 2,
//! and inventing it would also freeze a node shape before the resolver knows
//! what it needs. The parser evaluates while descending, which is enough for
//! expressions and `let`, and nothing more.
//!
//! ## Memory model in this file
//!
//! `let` bindings are allocated in the arena and dropped wholesale at the start
//! of the next `exec`, because a language whose top-level scope is per-execution
//! has no reason to track them individually. Host bindings injected with
//! [`Engine::define`] live in a plain vector instead, since they must outlive a
//! single execution. NaN-boxed [`Value`]s are copied, so neither kind needs a
//! pointer to the value itself.

use std::fmt;

use crate::Value;
use crate::arena::{Arena, Slot};
use crate::lexer::{BinOp, LexError, Token, TokenKind, lex};

/// Longest identifier the arena binding cell can hold inline. A bump allocator
/// wants fixed-size cells, so the limit is stated as a language rule rather than
/// emerging from a capacity accident.
pub const MAX_IDENTIFIER_LEN: usize = 32;

/// The category of a failure, which the FFI maps onto status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Source text the lexer could not read.
    Lex,
    /// Well-formed tokens in a shape the grammar rejects.
    Parse,
    /// A program that parsed but could not be carried out.
    Runtime,
    /// An internal invariant broke. Reserved; hosts see it as a caught panic.
    Panic,
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

/// An OmniEngine execution context.
///
/// Owns its identifier strings so it carries no lifetime: a borrowed engine
/// could not outlive the call that supplied its source, which would make an
/// opaque FFI handle unsound.
pub struct Engine {
    arena: Arena,
    /// Arena-resident `let` bindings for the current execution.
    bindings: Vec<Slot<Binding>>,
    /// Persistent host-injected bindings.
    globals: Vec<(String, Value)>,
    last_value: Value,
    last_error: Option<OmniError>,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            arena: Arena::new(),
            bindings: Vec::new(),
            globals: Vec::new(),
            last_value: Value::UNDEF,
            last_error: None,
        }
    }

    /// Run a program, returning the value of its last statement.
    pub fn exec(&mut self, source: &str) -> Result<Value, OmniError> {
        self.begin_execution();
        let result = self.run(source);
        match result {
            Ok(v) => {
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

    /// Begin a fresh execution. Clears the arena and the binding list together:
    /// that pairing is the invariant [`Slot`] relies on, so neither may happen
    /// without the other.
    fn begin_execution(&mut self) {
        self.arena.reset();
        self.bindings.clear();
    }

    /// The most recent failure, if the last `exec` failed.
    pub fn last_error(&self) -> Option<&OmniError> {
        self.last_error.as_ref()
    }

    /// The value produced by the last successful `exec`.
    pub fn last_value(&self) -> Value {
        self.last_value
    }

    /// Read-only view of the arena, for accounting and tests.
    pub fn arena(&self) -> &Arena {
        &self.arena
    }

    /// Inject a binding that survives across executions. This is how a host
    /// exposes its own functions and constants to scripts.
    pub fn define(&mut self, name: &str, value: Value) {
        self.globals.push((name.to_string(), value));
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
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| *v)
            })
    }

    fn parse_expr(&mut self, t: &[Token<'_>], i: &mut usize) -> Result<Value, OmniError> {
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
            let n = value
                .as_f64()
                .ok_or_else(|| self.parse_error(t, *i, "cannot apply unary '-' to a non-number"))?;
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
            TokenKind::Ident => {
                self.advance(i);
                self.lookup(token.text).ok_or_else(|| {
                    OmniError::new(
                        ErrorKind::Runtime,
                        token.line,
                        format!("undefined variable '{}'", token.text),
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
            other => Err(self.parse_error_kind(t, *i, other, "expected a number, a name, or '('")),
        }
    }

    /// One arithmetic site, so the divide-by-zero rule is stated once.
    ///
    /// Zero division is an error rather than IEEE infinity, because an `inf`
    /// returning through the FFI would be indistinguishable from a successful
    /// computation, and a silent wrong answer is worse than a loud failure.
    fn arith(&self, lhs: Value, op: BinOp, rhs: Value, line: u32) -> Result<Value, OmniError> {
        let (a, b) = match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                return Err(OmniError::new(
                    ErrorKind::Runtime,
                    line,
                    format!("'{}' needs numbers on both sides", op.symbol()),
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

    fn parse_error(&self, t: &[Token<'_>], i: usize, msg: &str) -> OmniError {
        OmniError::new(ErrorKind::Parse, self.peek_line(t, i), msg.to_string())
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

fn display_kind(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Let => "'let'",
        TokenKind::Ident => "a name",
        TokenKind::Number => "a number",
        TokenKind::Eq => "'='",
        TokenKind::LParen => "'('",
        TokenKind::RParen => "')'",
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
}
