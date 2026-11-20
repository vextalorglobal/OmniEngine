//! The lexer: `&str` in, offset tokens out.
//!
//! A token records *where* its text is in the caller's buffer rather than
//! holding a slice of it. Two things follow, and both are load-bearing.
//!
//! Lexing allocates for the token vector and nothing else — no `String` per
//! identifier, no copy per literal. And because a token no longer borrows the
//! source, the vector has no lifetime: [`crate::Engine`] keeps one and refills
//! it on every execution, so a warm engine lexes without asking the system
//! allocator for anything.
//!
//! [`Token::text`] resolves an offset against the source it came from. The
//! source outlives the tokens inside a single evaluation, which is the same
//! rule the borrowed-slice version relied on, now stated once instead of being
//! carried in every signature.

use std::fmt;

/// Longest program the lexer accepts, and the reason a token's offsets are
/// `u32` rather than `usize`: 256 MiB of source is far past any program a host
/// sends, and it keeps a token at sixteen bytes instead of thirty-two. The C
/// ABI states the same number, so a host is refused at the boundary before the
/// core is asked.
pub const MAX_SOURCE_LEN: usize = 1 << 28;

/// Binary operators, already folded into one variant so the parser can test
/// precedence without matching four separate token kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Let,
    /// `import`, a statement keyword at language version 2.
    Import,
    Ident,
    Number,
    /// A string literal's *contents*, quotes removed, escapes not yet decoded.
    ///
    /// Two decisions live in that one variant. Keeping the raw offsets means
    /// lexing still allocates nothing but the token vector, which is the whole
    /// reason the lexer is allocation-free. And deferring decoding means a bad
    /// escape is reported where the value and the line number are both at hand —
    /// evaluation — rather than in a second pass over text that has already been
    /// accepted.
    Str,
    BinOp(BinOp),
    Eq,
    /// `==`, folded into one token for the same reason `BinOp` is: the parser
    /// tests for an operator, not for two punctuation marks.
    EqEq,
    /// `!=`.
    NotEq,
    LParen,
    RParen,
    /// `,` — the argument separator a call needs. Its own variant rather than a
    /// punctuation-free hack because the parser reports what it expected, and
    /// `f(1 2)` is only a useful message if the comma is a thing.
    Comma,
    /// `.` — the one qualifier mark the grammar has: `namespace.export`.
    Dot,
    /// Accepted and ignored. Optional in OmniEngine, present so a C-style
    /// semicolon is not an error.
    Semi,
    Eof,
}

/// One token: what it is, where its text is, and which line it came from.
///
/// Sixteen bytes, `Copy`, and no lifetime. The offsets are `u32` because
/// [`MAX_SOURCE_LEN`] bounds the source they index, which is what lets a whole
/// token stream be reused across executions instead of reallocated per program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offset of the text in the source buffer.
    pub start: u32,
    /// Length of that text in bytes. Zero only for `Eof`, which has no text;
    /// punctuation carries its real spelling, so [`Token::text`] is never a lie.
    pub len: u32,
    /// 1-based line number for diagnostics.
    pub line: u32,
}

impl Token {
    /// This token's text, borrowed from the source it was lexed from.
    ///
    /// An offset paired with a different source yields `""` rather than a
    /// panic: the lexer guarantees char boundaries for the source it walked
    /// (`"` and `\` are ASCII, and UTF-8 never hides one inside a multi-byte
    /// character), but this is a public type and a caller may pair it wrongly.
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        let start = self.start as usize;
        let end = start + self.len as usize;
        source.get(start..end).unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub line: u32,
    pub column: u32,
    pub message: String,
}

impl LexError {
    /// `impl Into<String>` rather than `String`, so a caller writing a literal
    /// does not have to say `.to_string()` five times for the same effect — and
    /// the one caller with a `format!` still compiles.
    fn new(line: u32, column: u32, message: impl Into<String>) -> LexError {
        LexError {
            line,
            column,
            message: message.into(),
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for LexError {}

/// Record one token. The casts cannot truncate: [`lex_into`] refused a source
/// longer than [`MAX_SOURCE_LEN`], which is far inside `u32`.
fn emit(tokens: &mut Vec<Token>, kind: TokenKind, start: usize, len: usize, line: u32) {
    tokens.push(Token {
        kind,
        start: start as u32,
        len: len as u32,
        line,
    });
}

/// Tokenize a whole program into a fresh vector.
///
/// The convenience form, for a caller that lexes once. [`lex_into`] is the one
/// the engine uses, because it refills a buffer the engine already owns.
pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    let mut tokens = Vec::new();
    lex_into(source, &mut tokens)?;
    Ok(tokens)
}

/// Tokenize into a buffer the caller keeps, clearing it first.
///
/// Always appends [`TokenKind::Eof`], so the parser never has to special-case
/// the end of input. Newlines are insignificant in the Phase 1 grammar and are
/// skipped, with line numbers tracked for diagnostics.
///
/// The vector's capacity survives the clear, which is the entire point: after
/// the first program of a given size, lexing costs no allocation at all.
///
/// The buffer is left empty on failure as well as on entry. An out-parameter
/// that kept the tokens scanned before a bad byte would let a caller who
/// ignored the `Result` parse half a program, so the contract is all-or-nothing.
pub fn lex_into(source: &str, tokens: &mut Vec<Token>) -> Result<(), LexError> {
    tokens.clear();
    if source.len() > MAX_SOURCE_LEN {
        return Err(LexError::new(
            1,
            1,
            format!(
                "source is {} bytes; the lexer accepts at most {MAX_SOURCE_LEN}",
                source.len()
            ),
        ));
    }
    let result = scan(source, tokens);
    if result.is_err() {
        tokens.clear();
    }
    result
}

/// Walk the source, appending tokens until it ends or a byte refuses to lex.
fn scan(source: &str, tokens: &mut Vec<Token>) -> Result<(), LexError> {
    let bytes = source.as_bytes();
    let mut pos = 0usize;
    let mut line = 1u32;
    // Byte offset one past the last newline, so column is `pos - line_start + 1`.
    let mut line_start = 0usize;

    while pos < bytes.len() {
        let b = bytes[pos];
        let column = (pos - line_start + 1) as u32;

        match b {
            b' ' | b'\t' | b'\r' => pos += 1,
            b'\n' => {
                pos += 1;
                line += 1;
                line_start = pos;
            }
            b'/' if bytes.get(pos + 1) == Some(&b'/') => {
                // Line comment: run to the newline but do not consume it, so the
                // line counter increments exactly once per physical line.
                while pos < bytes.len() && bytes[pos] != b'\n' {
                    pos += 1;
                }
            }
            // `==` before `=`: match arms are tried in order, so the two-byte
            // guard has to come first or `a == b` lexes as two binding operators.
            b'=' if bytes.get(pos + 1) == Some(&b'=') => {
                emit(tokens, TokenKind::EqEq, pos, 2, line);
                pos += 2;
            }
            b'=' => {
                emit(tokens, TokenKind::Eq, pos, 1, line);
                pos += 1;
            }
            b'!' if bytes.get(pos + 1) == Some(&b'=') => {
                emit(tokens, TokenKind::NotEq, pos, 2, line);
                pos += 2;
            }
            // A bare `!` reaches the catch-all only when it is not `!=`, and the
            // catch-all's message would be "unexpected character '!'" — true, but
            // the useful one names the operator the reader probably meant.
            b'!' => {
                return Err(LexError::new(
                    line,
                    column,
                    "unexpected character '!'; OmniEngine has '!=' but no unary '!'",
                ));
            }
            b'"' => {
                // The opening quote owns the position of every error raised here,
                // because "unterminated" points at the string, not at the byte
                // where the scan gave up.
                let open_line = line;
                let open_column = column;
                pos += 1;
                let start = pos;
                loop {
                    if pos >= bytes.len() {
                        return Err(LexError::new(
                            open_line,
                            open_column,
                            "unterminated string literal",
                        ));
                    }
                    match bytes[pos] {
                        b'"' => break,
                        b'\n' | b'\r' => {
                            // Multi-line literals are not in the grammar, so the
                            // error says so rather than echoing "unterminated"
                            // about a quote the reader can plainly see closed.
                            return Err(LexError::new(
                                open_line,
                                open_column,
                                "a string literal cannot span lines",
                            ));
                        }
                        b'\\' => {
                            // Skip the escape's body so `\"` does not close the
                            // literal. Which escapes are legal is decided on
                            // decoding; the lexer only has to find the end.
                            if pos + 1 >= bytes.len() {
                                return Err(LexError::new(
                                    open_line,
                                    open_column,
                                    "unterminated string literal",
                                ));
                            }
                            if matches!(bytes[pos + 1], b'\n' | b'\r') {
                                return Err(LexError::new(
                                    open_line,
                                    open_column,
                                    "a backslash before a newline is not a line continuation",
                                ));
                            }
                            pos += 2;
                        }
                        // Any other byte, including every continuation byte of a
                        // multi-byte character, is payload: `"` and `\` are ASCII,
                        // and UTF-8 never uses a byte >= 0x80 to imitate one.
                        _ => pos += 1,
                    }
                }
                // Both offsets are char boundaries: the scan opened just past an
                // ASCII quote and stopped on one, and every byte it stepped over
                // in between is payload it never had to interpret.
                emit(tokens, TokenKind::Str, start, pos - start, open_line);
                pos += 1; // step over the closing quote
            }
            b'+' => {
                emit(tokens, TokenKind::BinOp(BinOp::Add), pos, 1, line);
                pos += 1;
            }
            b'-' => {
                emit(tokens, TokenKind::BinOp(BinOp::Sub), pos, 1, line);
                pos += 1;
            }
            b'*' => {
                emit(tokens, TokenKind::BinOp(BinOp::Mul), pos, 1, line);
                pos += 1;
            }
            // Reached only when the comment guard above did not match, because
            // match arms are tried in order: `//` is a comment, a bare `/` is
            // division.
            b'/' => {
                emit(tokens, TokenKind::BinOp(BinOp::Div), pos, 1, line);
                pos += 1;
            }
            b'%' => {
                emit(tokens, TokenKind::BinOp(BinOp::Mod), pos, 1, line);
                pos += 1;
            }
            b'(' => {
                emit(tokens, TokenKind::LParen, pos, 1, line);
                pos += 1;
            }
            b')' => {
                emit(tokens, TokenKind::RParen, pos, 1, line);
                pos += 1;
            }
            b',' => {
                emit(tokens, TokenKind::Comma, pos, 1, line);
                pos += 1;
            }
            // Only reached when a digit has not just claimed the dot as part of
            // a fraction, because the number arm consumes `1.5` whole.
            b'.' => {
                emit(tokens, TokenKind::Dot, pos, 1, line);
                pos += 1;
            }
            b';' => {
                emit(tokens, TokenKind::Semi, pos, 1, line);
                pos += 1;
            }
            b'0'..=b'9' => {
                let start = pos;
                while bytes[pos].is_ascii_digit() {
                    pos += 1;
                    if pos == bytes.len() {
                        break;
                    }
                }
                // Fractional part.
                if bytes.get(pos).copied() == Some(b'.') {
                    // `1.` followed by a non-digit is a mistake, not a number.
                    if !matches!(bytes.get(pos + 1).copied(), Some(b'0'..=b'9')) {
                        let shown = source.get(start..pos + 1).unwrap_or(".");
                        return Err(LexError::new(
                            line,
                            column,
                            format!("number {shown} ends with a decimal point"),
                        ));
                    }
                    pos += 1;
                    while bytes[pos].is_ascii_digit() {
                        pos += 1;
                        if pos == bytes.len() {
                            break;
                        }
                    }
                }
                // Exponent part.
                if matches!(bytes.get(pos).copied(), Some(b'e' | b'E')) {
                    let after = bytes.get(pos + 1).copied();
                    let sign_len = usize::from(matches!(after, Some(b'+') | Some(b'-')));
                    let digits = pos + 1 + sign_len;
                    if !matches!(bytes.get(digits).copied(), Some(b'0'..=b'9')) {
                        let end = digits.min(bytes.len());
                        let shown = source.get(start..end).unwrap_or("e");
                        return Err(LexError::new(
                            line,
                            column,
                            format!("number {shown} has an empty exponent"),
                        ));
                    }
                    pos = digits;
                    while matches!(bytes.get(pos).copied(), Some(b'0'..=b'9')) {
                        pos += 1;
                    }
                }
                emit(tokens, TokenKind::Number, start, pos - start, line);
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = pos;
                while matches!(bytes[pos], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
                    pos += 1;
                    if pos == bytes.len() {
                        break;
                    }
                }
                // Compared as bytes: an identifier is ASCII by the scan above,
                // so deciding a keyword from any other name needs no UTF-8 walk.
                let kind = match &bytes[start..pos] {
                    b"let" => TokenKind::Let,
                    b"import" => TokenKind::Import,
                    _ => TokenKind::Ident,
                };
                emit(tokens, kind, start, pos - start, line);
            }
            other => {
                // Report the actual character, decoded from the remaining
                // source, rather than the raw byte.
                let ch = source
                    .get(pos..)
                    .and_then(|rest| rest.chars().next())
                    .map_or_else(|| format!("byte {other:#04x}"), |c| c.to_string());
                return Err(LexError::new(
                    line,
                    column,
                    format!("unexpected character '{ch}'"),
                ));
            }
        }
    }

    emit(tokens, TokenKind::Eof, bytes.len(), 0, line);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).unwrap().iter().map(|t| t.kind).collect()
    }

    #[test]
    fn lexes_the_vertical_slice() {
        assert_eq!(
            kinds("let answer = 40 + 2"),
            vec![
                TokenKind::Let,
                TokenKind::Ident,
                TokenKind::Eq,
                TokenKind::Number,
                TokenKind::BinOp(BinOp::Add),
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn skips_line_comments_and_whitespace() {
        assert_eq!(
            kinds("// leading\n 1 + 2 // tail"),
            vec![
                TokenKind::Number,
                TokenKind::BinOp(BinOp::Add),
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn floats_and_exponents_are_one_number() {
        assert_eq!(kinds("1.5e3"), vec![TokenKind::Number, TokenKind::Eof]);
        assert_eq!(kinds("1.5e-3"), vec![TokenKind::Number, TokenKind::Eof]);
        assert_eq!(kinds("2E10"), vec![TokenKind::Number, TokenKind::Eof]);
        assert_eq!(
            kinds("1 + 2; 3"),
            vec![
                TokenKind::Number,
                TokenKind::BinOp(BinOp::Add),
                TokenKind::Number,
                TokenKind::Semi,
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn operators_and_parens_are_distinct_tokens() {
        assert_eq!(
            kinds("(1)%2"),
            vec![
                TokenKind::LParen,
                TokenKind::Number,
                TokenKind::RParen,
                TokenKind::BinOp(BinOp::Mod),
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn division_is_an_operator_not_a_comment() {
        // The regression this guards: a missing bare-'/' arm made every
        // division lex as "unexpected character '/'".
        assert_eq!(
            kinds("8 / 2"),
            vec![
                TokenKind::Number,
                TokenKind::BinOp(BinOp::Div),
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
        assert_eq!(kinds("8 //2"), vec![TokenKind::Number, TokenKind::Eof]);
        assert_eq!(
            kinds("8 / /2"),
            vec![
                TokenKind::Number,
                TokenKind::BinOp(BinOp::Div),
                TokenKind::BinOp(BinOp::Div),
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn identifiers_may_contain_digits_and_underscores() {
        // `let2` is one identifier, not `let` followed by `2`.
        assert_eq!(kinds("let2"), vec![TokenKind::Ident, TokenKind::Eof]);
        assert_eq!(kinds("_a1"), vec![TokenKind::Ident, TokenKind::Eof]);
    }

    #[test]
    fn tracks_line_numbers_for_errors() {
        let e = lex("1\n\n@\n").unwrap_err();
        assert_eq!(e.line, 3);
    }

    #[test]
    fn rejects_unknown_bytes_with_a_column() {
        let e = lex("1 + ?").unwrap_err();
        assert_eq!((e.line, e.column), (1, 5));
        assert!(e.message.contains('?'), "message was {e}");
    }

    #[test]
    fn rejects_malformed_numbers_with_context() {
        let e = lex("1.").unwrap_err();
        assert!(e.message.contains("decimal point"), "message was {e}");
        let e = lex("1e+").unwrap_err();
        assert!(e.message.contains("exponent"), "message was {e}");
    }

    #[test]
    fn comments_at_end_of_input_do_not_panic() {
        assert_eq!(kinds("1 //"), vec![TokenKind::Number, TokenKind::Eof]);
        assert_eq!(kinds(""), vec![TokenKind::Eof]);
    }

    /// The lexer walks bytes, so a multi-byte comment is the case where "skip to
    /// the next newline" could split a character or lose track of the line. The
    /// first non-English users of this language will hit it immediately.
    #[test]
    fn non_ascii_comments_lex_and_keep_line_count() {
        assert_eq!(
            kinds("// নম্বর\n42"),
            vec![TokenKind::Number, TokenKind::Eof]
        );
        assert_eq!(
            kinds("// ✓ ok — 1 + 2\n7"),
            vec![TokenKind::Number, TokenKind::Eof]
        );
        // An error on the line after a multi-byte comment is still line 2.
        let e = lex("// ✓ ok\n@").unwrap_err();
        assert_eq!(e.line, 2, "line counting broke on a multi-byte comment");
    }

    #[test]
    fn unicode_comments_keep_following_token_offsets_byte_aligned() {
        let source = "// বাংলা — 日本語\n42";
        let tokens = lex(source).unwrap();
        assert_eq!(tokens[0].start as usize, source.find("42").unwrap());
        assert_eq!(tokens[0].text(source), "42");
    }

    #[test]
    fn a_non_ascii_byte_is_reported_decoded_with_its_column() {
        // Identifiers are ASCII by design, so this is the error path a non-English
        // source file hits first: it must name the character, not a byte.
        let e = lex("1 + ই").unwrap_err();
        assert_eq!(
            (e.line, e.column),
            (1, 5),
            "column is a byte offset, and ই begins at byte 5"
        );
        assert!(e.message.contains('ই'), "message was {e}");
    }

    /// The zero-copy claim, made checkable: resolving a token's offset lands
    /// inside the caller's buffer rather than in a copy the lexer made.
    #[test]
    fn token_text_resolves_into_the_callers_buffer() {
        let src = String::from("let x = 1");
        let toks = lex(&src).unwrap();
        let ident = toks.iter().find(|t| t.kind == TokenKind::Ident).unwrap();
        assert_eq!((ident.start, ident.len), (4, 1));
        // No unsafe needed any more: the offset is data, and `text` borrows the
        // source it is handed. Offset 4 is the `x` in "let x = 1".
        assert!(std::ptr::eq(
            ident.text(&src).as_ptr(),
            src.as_bytes()[4..].as_ptr()
        ));
    }

    /// Sixteen bytes is the number the reuse story rests on: a token stream is
    /// half the size it was when every token carried a fat pointer, so twice as
    /// many fit in a cache line.
    #[test]
    fn a_token_is_sixteen_bytes_and_carries_no_lifetime() {
        assert_eq!(std::mem::size_of::<Token>(), 16);
        fn owns_no_borrow(_: Token) {}
        let t = lex("1").unwrap()[0];
        owns_no_borrow(t);
        assert_eq!(t.text("1"), "1");
    }

    #[test]
    fn punctuation_and_eof_resolve_to_their_spelling() {
        let src = "a == b != c;";
        let toks = lex(src).unwrap();
        let texts: Vec<&str> = toks.iter().map(|t| t.text(src)).collect();
        assert_eq!(texts, vec!["a", "==", "b", "!=", "c", ";", ""]);
    }

    /// The claim that makes a warm engine allocation-free: refilling a buffer
    /// keeps its capacity, and a shorter program leaves no stale tokens behind.
    #[test]
    fn lex_into_reuses_capacity_and_clears_the_previous_program() {
        let mut buf = Vec::new();
        lex_into("1 + 2 + 3 + 4 + 5", &mut buf).unwrap();
        let capacity = buf.capacity();
        assert!(capacity >= 10);
        assert_eq!(buf.len(), 10, "five numbers, four operators, an Eof");

        lex_into("7", &mut buf).unwrap();
        assert_eq!(buf.capacity(), capacity, "the buffer was reallocated");
        assert_eq!(buf.len(), 2, "a number and an Eof, nothing left over");
        assert_eq!(buf[0].text("7"), "7");
        assert_eq!(buf[1].kind, TokenKind::Eof);

        // A failed lex must not leave half a program for the next caller to read.
        assert!(lex_into("1 + ?", &mut buf).is_err());
        assert!(buf.is_empty());
    }

    /// The invariant every `as u32` in `emit` rests on. Proving it by comparison
    /// costs nothing; proving it by allocating a 256 MiB source would cost more
    /// than the guard is worth, and the ABI already tests refusal at its own
    /// boundary with the same number.
    #[test]
    fn the_source_limit_fits_the_offsets_that_index_it() {
        assert!(u32::try_from(super::MAX_SOURCE_LEN).is_ok());
    }

    #[test]
    fn records_lines_on_every_token() {
        let toks = lex("1\n2\n\n3").unwrap();
        let lines: Vec<u32> = toks.iter().map(|t| t.line).collect();
        assert_eq!(lines, vec![1, 2, 4, 4]);
    }

    fn body(src: &str) -> String {
        lex(src)
            .unwrap()
            .iter()
            .find(|t| t.kind == TokenKind::Str)
            .map(|t| t.text(src).to_string())
            .expect("no string token")
    }

    #[test]
    fn a_string_literal_is_one_token_holding_its_contents() {
        assert_eq!(kinds(r#""hi""#), vec![TokenKind::Str, TokenKind::Eof]);
        assert_eq!(body(r#""hi""#), "hi");
        assert_eq!(body("\"\""), "");
        assert_eq!(body("\"two words\""), "two words");
    }

    /// The lexer finds the end of a literal; it does not interpret it. That is
    /// what lets a token stay a pair of offsets, and it means the raw backslashes
    /// survive to be decoded by the evaluator, where the line number is known.
    #[test]
    fn escapes_are_scanned_over_not_decoded() {
        assert_eq!(body(r#""a\"b""#), r#"a\"b"#);
        assert_eq!(body(r#""\\""#), r#"\\"#);
        assert_eq!(body(r#""tab\there""#), r"tab\there");
        assert_eq!(body(r#""\u{41}A""#), r"\u{41}A");
        // An escape the grammar does not have still lexes, because only
        // evaluation can say which escapes are legal and report the line.
        assert_eq!(body(r#""\q""#), r"\q");
    }

    #[test]
    fn a_literal_never_spans_lines_and_says_where_it_opened() {
        let e = lex("\"a\nb\"").unwrap_err();
        assert_eq!((e.line, e.column), (1, 1), "position is the opening quote");
        assert!(e.message.contains("cannot span lines"), "{e}");

        let e = lex("1 +\n  \"oops\n").unwrap_err();
        assert_eq!((e.line, e.column), (2, 3), "the opening quote of line 2");

        let e = lex("\"unterminated").unwrap_err();
        assert!(e.message.contains("unterminated"), "{e}");

        let e = lex("\"trailing backslash\\").unwrap_err();
        assert!(e.message.contains("unterminated"), "{e}");

        // A backslash-newline would otherwise swallow the newline and lose the
        // line count, so it is refused rather than half-supported.
        let e = lex("\"a\\\nb\"").unwrap_err();
        assert!(e.message.contains("line continuation"), "{e}");
    }

    #[test]
    fn double_equals_is_one_token_and_a_single_equals_still_binds() {
        assert_eq!(
            kinds("1 == 2"),
            vec![
                TokenKind::Number,
                TokenKind::EqEq,
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("x = 1"),
            vec![
                TokenKind::Ident,
                TokenKind::Eq,
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("1 != 2"),
            vec![
                TokenKind::Number,
                TokenKind::NotEq,
                TokenKind::Number,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn a_bare_bang_explains_what_it_wanted_to_be() {
        let e = lex("1 ! 2").unwrap_err();
        assert!(e.message.contains("!='"), "message was {e}");
        assert_eq!((e.line, e.column), (1, 3));
    }

    #[test]
    fn non_ascii_bytes_inside_a_literal_are_payload() {
        assert_eq!(body("\"✄ ok\""), "✄ ok");
        assert_eq!(body("\"নম্বর\""), "নম্বর");
        // And the token after a multi-byte literal is still on the right line.
        let toks = lex("\"✄\"\n\"🎯\"").unwrap();
        assert_eq!(toks.len(), 3, "two literals and an Eof");
        assert_eq!(toks[0].line, 1);
        assert_eq!(toks[1].line, 2);
        assert_eq!(toks[2].line, 2, "Eof carries the last line");
    }

    #[test]
    fn a_string_token_points_into_the_source_like_every_other_token() {
        let src = String::from("let s = \"hi\"");
        let toks = lex(&src).unwrap();
        let s = toks.iter().find(|t| t.kind == TokenKind::Str).unwrap();
        // `let s = "` is nine bytes, so the contents start at offset 9.
        assert_eq!((s.start, s.len), (9, 2));
        assert!(std::ptr::eq(
            s.text(&src).as_ptr(),
            src.as_bytes()[9..].as_ptr()
        ));
        assert_eq!(s.text(&src), "hi");
    }
}
