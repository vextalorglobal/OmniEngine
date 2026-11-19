//! JSON for the scalar values ABI 5 can carry, and a clear refusal for the rest.
//!
//! There is no array and no object in [`crate::Value`]: the ABI hands a host a
//! sixteen-byte struct, and a collection would need a second representation,
//! an ownership rule, and an SDK decoding path. Until that exists, `json.parse`
//! accepts a number, a string, a boolean, or `null`, and says exactly why an
//! array or object is refused rather than smuggling the text through as a string.
//!
//! The parser is a single forward scan. Strings are the only allocation, because
//! a decoded escape cannot borrow the input; every other scalar is returned by
//! value.

use std::io::Write as _;

#[derive(Debug)]
pub(crate) enum Scalar {
    Number(f64),
    Str(Vec<u8>),
    Bool(bool),
    Null,
}

/// The message a caller sees for a JSON collection. Kept here so the parser and
/// the engine's error formatting cannot drift apart.
pub(crate) const COLLECTION: &str =
    "arrays and objects are not supported at ABI 5; OmniEngine has no collection value";

pub(crate) fn parse(input: &str) -> Result<Scalar, String> {
    let mut parser = Parser {
        input,
        bytes: input.as_bytes(),
        pos: 0,
    };
    parser.skip_space();
    if parser.pos == parser.bytes.len() {
        return Err("expected a JSON value".to_string());
    }
    let value = parser.value()?;
    parser.skip_space();
    if parser.pos != parser.bytes.len() {
        return Err("trailing characters after the JSON value".to_string());
    }
    Ok(value)
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_space(&mut self) {
        while matches!(
            self.bytes.get(self.pos).copied(),
            Some(b' ' | b'\t' | b'\n' | b'\r')
        ) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Scalar, String> {
        match self.bytes.get(self.pos).copied() {
            Some(b'[' | b'{') => Err(COLLECTION.to_string()),
            Some(b'"') => self.string().map(Scalar::Str),
            Some(b't') => {
                self.literal("true")?;
                Ok(Scalar::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Scalar::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Scalar::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(other) => Err(format!(
                "unexpected character '{}'",
                self.input[self.pos..]
                    .chars()
                    .next()
                    .unwrap_or(other as char)
            )),
            None => Err("expected a JSON value".to_string()),
        }
    }

    fn literal(&mut self, want: &str) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(want.as_bytes()) {
            self.pos += want.len();
            return Ok(());
        }
        let shown = self
            .input
            .get(self.pos..)
            .and_then(|rest| rest.chars().next())
            .map_or_else(|| "end of input".to_string(), |c| format!("'{c}'"));
        Err(format!("expected '{want}', found {shown}"))
    }

    fn number(&mut self) -> Result<Scalar, String> {
        let start = self.pos;
        if self.bytes[self.pos] == b'-' {
            self.pos += 1;
        }
        match self.bytes.get(self.pos).copied() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.bytes.get(self.pos).copied(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err("expected a digit in a JSON number".to_string()),
        }
        if self.bytes.get(self.pos).copied() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.bytes.get(self.pos).copied(), Some(b'0'..=b'9')) {
                return Err("a JSON fraction needs at least one digit".to_string());
            }
            while matches!(self.bytes.get(self.pos).copied(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.bytes.get(self.pos).copied(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.bytes.get(self.pos).copied(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.bytes.get(self.pos).copied(), Some(b'0'..=b'9')) {
                return Err("a JSON exponent needs at least one digit".to_string());
            }
            while matches!(self.bytes.get(self.pos).copied(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = &self.input[start..self.pos];
        let number = text
            .parse::<f64>()
            .map_err(|_| format!("'{text}' is not a JSON number"))?;
        if !number.is_finite() {
            return Err(format!("'{text}' is not a finite JSON number"));
        }
        Ok(Scalar::Number(number))
    }

    fn string(&mut self) -> Result<Vec<u8>, String> {
        self.pos += 1; // opening quote
        let mut out = Vec::new();
        loop {
            let byte = match self.bytes.get(self.pos).copied() {
                Some(byte) => byte,
                None => return Err("unterminated string".to_string()),
            };
            self.pos += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = match self.bytes.get(self.pos).copied() {
                        Some(escape) => escape,
                        None => return Err("unterminated escape".to_string()),
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => self.unicode_escape(&mut out)?,
                        other => {
                            return Err(format!("unknown escape '\\{}'", other as char));
                        }
                    }
                }
                0x00..=0x1F => {
                    return Err("a JSON string cannot contain a raw control character".to_string());
                }
                _ => out.push(byte),
            }
        }
    }

    fn unicode_escape(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        let first = self.hex4()?;
        let scalar = if (0xD800..=0xDBFF).contains(&first) {
            if self.bytes.get(self.pos).copied() != Some(b'\\')
                || self.bytes.get(self.pos + 1).copied() != Some(b'u')
            {
                return Err("a high surrogate must be followed by a low surrogate".to_string());
            }
            self.pos += 2;
            let second = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&second) {
                return Err("a high surrogate must be followed by a low surrogate".to_string());
            }
            0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
        } else if (0xDC00..=0xDFFF).contains(&first) {
            return Err("a lone low surrogate is not a Unicode scalar".to_string());
        } else {
            first
        };
        let ch = char::from_u32(scalar)
            .ok_or_else(|| "a \\u escape is above the last Unicode scalar".to_string())?;
        let mut buffer = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self
                .bytes
                .get(self.pos)
                .copied()
                .ok_or_else(|| "a \\u escape needs four hex digits".to_string())?;
            let digit = (byte as char)
                .to_digit(16)
                .ok_or_else(|| "a \\u escape needs four hex digits".to_string())?;
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }
}

/// Append one string as JSON. The caller guarantees valid UTF-8.
pub(crate) fn write_string(bytes: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &byte in bytes {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            0x00..=0x1F => {
                let _ = write!(out, "\\u{byte:04x}");
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Append a finite number. `false` means the value has no JSON spelling.
pub(crate) fn write_number(number: f64, out: &mut Vec<u8>) -> bool {
    if !number.is_finite() {
        return false;
    }
    // `Display` gives the shortest round-tripping decimal, which is also what
    // JSON wants: 42.0 becomes `42`, not `42.0`.
    let _ = write!(out, "{number}");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_parse() {
        assert!(matches!(parse("42"), Ok(Scalar::Number(n)) if n == 42.0));
        assert!(matches!(parse("-1.5e2"), Ok(Scalar::Number(n)) if n == -150.0));
        assert!(matches!(parse("true"), Ok(Scalar::Bool(true))));
        assert!(matches!(parse("false"), Ok(Scalar::Bool(false))));
        assert!(matches!(parse("null"), Ok(Scalar::Null)));
        assert!(matches!(parse("  \"hi\" "), Ok(Scalar::Str(s)) if s == b"hi"));
    }

    #[test]
    fn escapes_decode_including_surrogate_pairs() {
        assert!(matches!(parse(r#""a\nb\u0041""#), Ok(Scalar::Str(s)) if s == b"a\nbA"));
        assert!(
            matches!(parse(r#""\uD83D\uDE00""#), Ok(Scalar::Str(s)) if s == "😀".as_bytes()),
            "a surrogate pair becomes one scalar"
        );
    }

    #[test]
    fn collections_are_refused_with_the_abi_reason() {
        for source in ["[]", "{}", " [1, 2]", r#"{"a": 1}"#] {
            let error = parse(source).unwrap_err();
            assert_eq!(error, COLLECTION, "{source}");
        }
    }

    #[test]
    fn malformed_scalars_say_what_they_wanted() {
        assert!(parse("").unwrap_err().contains("expected a JSON value"));
        assert!(parse("42 x").unwrap_err().contains("trailing"));
        assert!(
            parse(r#""unterminated"#)
                .unwrap_err()
                .contains("unterminated")
        );
        assert!(parse("tru").unwrap_err().contains("expected 'true'"));
        assert!(parse("01").unwrap_err().contains("trailing"));
        assert!(parse("1e").unwrap_err().contains("exponent"));
        assert!(parse("1.").unwrap_err().contains("fraction"));
        assert!(parse(r#""\uD800""#).unwrap_err().contains("surrogate"));
        assert!(parse(r#""\q""#).unwrap_err().contains("unknown escape"));
    }

    #[test]
    fn stringification_escapes_what_json_requires() {
        let mut out = Vec::new();
        write_string(b"a\"b\\c\nd\x01", &mut out);
        assert_eq!(out, b"\"a\\\"b\\\\c\\nd\\u0001\"");
        let mut out = Vec::new();
        write_string("এ".as_bytes(), &mut out);
        assert_eq!(out, "\"এ\"".as_bytes(), "non-ASCII passes through");
    }

    #[test]
    fn numbers_are_written_the_short_way_and_infinities_are_refused() {
        let mut out = Vec::new();
        assert!(write_number(42.0, &mut out));
        assert_eq!(out, b"42");
        out.clear();
        assert!(write_number(-1.5, &mut out));
        assert_eq!(out, b"-1.5");
        assert!(!write_number(f64::INFINITY, &mut Vec::new()));
    }
}
