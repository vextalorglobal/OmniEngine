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
    BinOp(BinOp),
    Eq,
    LParen,
    RParen,
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
    fn new(line: u32, column: u32, message: String) -> LexError {
        LexError {
            line,
            column,
            message,
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
            b'=' => {
                tokens.push(Token {
                    kind: TokenKind::Eq,
                    text: "=",
                    line,
                });
                pos += 1;
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
}
