//! The lexer: `&str` in, borrowed token slices out.
//!
//! Token text is a `&'a str` slice of the caller's source buffer rather than an
//! owned `String`, so lexing a program allocates for the token vector and for
//! nothing else. That is a deliberate consequence of the memory model: the
//! source outlives the tokens produced from it inside a single `Engine::exec`.

use std::fmt;

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
    Ident,
    Number,
    /// A string literal's *contents*, quotes removed, escapes not yet decoded.
    ///
    /// Two decisions live in that one variant. Keeping the raw slice means lexing
    /// still allocates nothing but the token vector, which is the whole reason the
    /// lexer is allocation-free. And deferring decoding means a bad escape is
    /// reported where the value and the line number are both at hand — evaluation
    /// — rather than in a second pass over text that has already been accepted.
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
    /// Accepted and ignored. Optional in OmniEngine, present so a C-style
    /// semicolon is not an error.
    Semi,
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokenKind,
    /// Borrows the source buffer.
    pub text: &'a str,
    /// 1-based line number for diagnostics.
    pub line: u32,
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

/// Tokenize a whole program.
///
/// Always appends [`TokenKind::Eof`], so the parser never has to special-case
/// the end of input. Newlines are insignificant in the Phase 1 grammar and are
/// skipped, with line numbers tracked for diagnostics.
pub fn lex(source: &str) -> Result<Vec<Token<'_>>, LexError> {
    let bytes = source.as_bytes();
    let mut tokens: Vec<Token<'_>> = Vec::new();
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
                tokens.push(Token {
                    kind: TokenKind::EqEq,
                    text: "==",
                    line,
                });
                pos += 2;
            }
            b'=' => {
                tokens.push(Token {
                    kind: TokenKind::Eq,
                    text: "=",
                    line,
                });
                pos += 1;
            }
            b'!' if bytes.get(pos + 1) == Some(&b'=') => {
                tokens.push(Token {
                    kind: TokenKind::NotEq,
                    text: "!=",
                    line,
                });
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
                let text = source
                    .get(start..pos)
                    .ok_or_else(|| LexError::new(open_line, open_column, "invalid UTF-8"))?;
                tokens.push(Token {
                    kind: TokenKind::Str,
                    text,
                    line: open_line,
                });
                pos += 1; // step over the closing quote
            }
            b'+' => {
                tokens.push(Token {
                    kind: TokenKind::BinOp(BinOp::Add),
                    text: "+",
                    line,
                });
                pos += 1;
            }
            b'-' => {
                tokens.push(Token {
                    kind: TokenKind::BinOp(BinOp::Sub),
                    text: "-",
                    line,
                });
                pos += 1;
            }
            b'*' => {
                tokens.push(Token {
                    kind: TokenKind::BinOp(BinOp::Mul),
                    text: "*",
                    line,
                });
                pos += 1;
            }
            // Reached only when the comment guard above did not match, because
            // match arms are tried in order: `//` is a comment, a bare `/` is
            // division.
            b'/' => {
                tokens.push(Token {
                    kind: TokenKind::BinOp(BinOp::Div),
                    text: "/",
                    line,
                });
                pos += 1;
            }
            b'%' => {
                tokens.push(Token {
                    kind: TokenKind::BinOp(BinOp::Mod),
                    text: "%",
                    line,
                });
                pos += 1;
            }
            b'(' => {
                tokens.push(Token {
                    kind: TokenKind::LParen,
                    text: "(",
                    line,
                });
                pos += 1;
            }
            b')' => {
                tokens.push(Token {
                    kind: TokenKind::RParen,
                    text: ")",
                    line,
                });
                pos += 1;
            }
            b',' => {
                tokens.push(Token {
                    kind: TokenKind::Comma,
                    text: ",",
                    line,
                });
                pos += 1;
            }
            b';' => {
                tokens.push(Token {
                    kind: TokenKind::Semi,
                    text: ";",
                    line,
                });
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
                let text = source
                    .get(start..pos)
                    .ok_or_else(|| LexError::new(line, column, "invalid UTF-8".to_string()))?;
                tokens.push(Token {
                    kind: TokenKind::Number,
                    text,
                    line,
                });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = pos;
                while matches!(bytes[pos], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
                    pos += 1;
                    if pos == bytes.len() {
                        break;
                    }
                }
                let text = source
                    .get(start..pos)
                    .ok_or_else(|| LexError::new(line, column, "invalid UTF-8".to_string()))?;
                let kind = if text == "let" {
                    TokenKind::Let
                } else {
                    TokenKind::Ident
                };
                tokens.push(Token { kind, text, line });
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

    tokens.push(Token {
        kind: TokenKind::Eof,
        text: "",
        line,
    });
    Ok(tokens)
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

    /// The zero-allocation claim, made checkable: a token's text points into the
    /// caller's buffer instead of being copied out.
    #[test]
    fn token_text_is_a_borrow_not_a_copy() {
        let src = String::from("let x = 1");
        let toks = lex(&src).unwrap();
        let ident = toks.iter().find(|t| t.kind == TokenKind::Ident).unwrap();
        // Safety: offset 4 is inside `src` ("let x = 1"), so the resulting
        // pointer is in bounds — which is precisely the claim under test.
        let expected = unsafe { src.as_ptr().add(4) };
        assert!(std::ptr::eq(ident.text.as_ptr(), expected));
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
            .map(|t| t.text.to_string())
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
    /// what lets a token stay a borrowed slice, and it means the raw backslashes
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
    fn a_string_token_borrows_the_source_like_every_other_token() {
        let src = String::from("let s = \"hi\"");
        let toks = lex(&src).unwrap();
        let s = toks.iter().find(|t| t.kind == TokenKind::Str).unwrap();
        // Safety: byte offset 9 is the `h` inside the literal — `let s = "` is
        // nine bytes — so the pointer is in bounds, and the claim under test is
        // that the token *is* that pointer rather than a copy of it.
        assert!(std::ptr::eq(s.text.as_ptr(), unsafe {
            src.as_ptr().add(9)
        }));
        assert_eq!(s.text, "hi");
    }
}
