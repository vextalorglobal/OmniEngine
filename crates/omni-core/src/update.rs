//! Release manifests and version comparison — the pure half of the updater.
//!
//! This module answers one question and reads one document. It performs no
//! I/O, opens no socket, and depends on nothing outside `std`: the engine
//! library has no business owning an HTTP client, and a host that already has
//! one should not be made to carry a second.
//!
//! The document is the release manifest the updater fetches:
//!
//! ```json
//! {
//!   "version": "0.2.0",
//!   "abi": 4,
//!   "assets": {
//!     "win32-x64": { "url": "https://…/omni-win32-x64.exe", "sha256": "…" }
//!   }
//! }
//! ```
//!
//! [`parse_manifest`] validates it, [`check_update`] compares it with the
//! running build, and [`platform_key`] names the asset a host on this platform
//! should take. Fetching, checksum verification and replacing a binary belong
//! to the CLI, which has a process to install into; this module is shared with
//! the C ABI so every host reaches the same verdict from the same bytes.
//!
//! The JSON reader is small on purpose. A serialisation crate would be the
//! only dependency in the workspace, pulled in for three fields of a document
//! the engine itself never consults.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

/// Largest manifest this module will parse. A release manifest is a few
/// kilobytes; the bound exists so a hostile or misconfigured endpoint cannot
/// make a host allocate without limit.
pub const MAX_MANIFEST_LEN: usize = 1 << 20;

/// A semantic version, enough of one to order two engine releases.
///
/// Build metadata (`+…`) is ignored, as SemVer requires: it names a build, not
/// a precedence. A leading `v` is accepted because release tags carry one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Dot-separated pre-release identifiers, absent on a release.
    pub pre: Option<String>,
}

impl Version {
    /// Parse `major.minor.patch[-pre][+build]`, with an optional leading `v`.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next().unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => {
                if pre.is_empty()
                    || !pre.split('.').all(|id| {
                        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
                {
                    return None;
                }
                (core, Some(pre.to_string()))
            }
            None => (text, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                // A release outranks every pre-release of the same version.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_pre(a, b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// SemVer's pre-release ordering: numeric identifiers compare numerically and
/// rank below alphanumeric ones; a shorter set of equal identifiers ranks lower.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) => {
                let order = match (l.parse::<u64>(), r.parse::<u64>()) {
                    (Ok(ln), Ok(rn)) => ln.cmp(&rn),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => l.cmp(r),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

/// One downloadable binary in a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub url: String,
    /// Lowercase hex SHA-256 of the bytes at `url`.
    pub sha256: String,
}

/// A parsed release manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub version: String,
    pub abi: u32,
    /// Optional link to the release notes.
    pub notes: Option<String>,
    /// Platform key (`win32-x64`, `linux-arm64`, …) to its binary.
    pub assets: BTreeMap<String, ReleaseAsset>,
}

impl ReleaseManifest {
    pub fn asset_for(&self, platform: &str) -> Option<&ReleaseAsset> {
        self.assets.get(platform)
    }
}

/// Why a manifest could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// The document is larger than [`MAX_MANIFEST_LEN`].
    TooLong,
    /// The bytes are not JSON this reader accepts.
    Json(String),
    /// It is JSON, but not a release manifest.
    Manifest(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::TooLong => write!(
                f,
                "the update manifest is larger than {MAX_MANIFEST_LEN} bytes"
            ),
            UpdateError::Json(message) => write!(f, "invalid update manifest JSON: {message}"),
            UpdateError::Manifest(message) => write!(f, "invalid update manifest: {message}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// The verdict of comparing a manifest with the running build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    /// The manifest names this version or an older one.
    UpToDate,
    /// The manifest names a newer version for the same ABI.
    Available,
    /// The manifest speaks a different ABI. Its version is not comparable to
    /// this build's, so installing it would be the wrong upgrade.
    AbiMismatch,
}

impl UpdateStatus {
    /// The integer the C ABI returns.
    pub fn code(self) -> i32 {
        match self {
            UpdateStatus::UpToDate => 0,
            UpdateStatus::Available => 1,
            UpdateStatus::AbiMismatch => 2,
        }
    }

    /// A stable lowercase name for logs and SDK result objects.
    pub fn name(self) -> &'static str {
        match self {
            UpdateStatus::UpToDate => "up-to-date",
            UpdateStatus::Available => "available",
            UpdateStatus::AbiMismatch => "abi-mismatch",
        }
    }
}

/// Compare a manifest with the running engine.
///
/// An ABI mismatch is reported before the versions are compared: two builds on
/// different wire formats do not describe the same upgrade even when one
/// number is larger.
pub fn check_update(
    current_version: &str,
    current_abi: u32,
    manifest: &ReleaseManifest,
) -> UpdateStatus {
    if manifest.abi != current_abi {
        return UpdateStatus::AbiMismatch;
    }
    match (
        Version::parse(current_version),
        Version::parse(&manifest.version),
    ) {
        (Some(current), Some(latest)) if latest > current => UpdateStatus::Available,
        _ => UpdateStatus::UpToDate,
    }
}

/// The release asset key for an operating system and architecture.
///
/// The names match `scripts/make-manifest.py` and the `native/` directories
/// the SDKs bundle, so a manifest entry, a tarball and an installed package
/// all agree on what this machine is called.
pub fn platform_key(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64" | "x64" | "amd64") => Some("win32-x64"),
        ("linux", "x86_64" | "x64" | "amd64") => Some("linux-x64"),
        ("linux", "aarch64" | "arm64") => Some("linux-arm64"),
        ("macos", "x86_64" | "x64") => Some("darwin-x64"),
        ("macos", "aarch64" | "arm64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// [`platform_key`] for the running process.
pub fn host_platform() -> Option<&'static str> {
    platform_key(std::env::consts::OS, std::env::consts::ARCH)
}

/// Read a release manifest, failing closed on anything unexpected.
pub fn parse_manifest(text: &str) -> Result<ReleaseManifest, UpdateError> {
    if text.len() > MAX_MANIFEST_LEN {
        return Err(UpdateError::TooLong);
    }
    let root = JsonParser::new(text).parse()?;
    let Json::Object(map) = root else {
        return Err(UpdateError::Manifest(
            "the document is not an object".into(),
        ));
    };

    let version = match map.get("version") {
        Some(Json::String(version)) if Version::parse(version).is_some() => version.clone(),
        Some(Json::String(_)) => {
            return Err(UpdateError::Manifest(
                "'version' is not a semantic version".into(),
            ));
        }
        _ => {
            return Err(UpdateError::Manifest(
                "'version' is missing or not a string".into(),
            ));
        }
    };

    let abi = match map.get("abi") {
        Some(Json::Number(number)) => as_u32(*number)
            .ok_or_else(|| UpdateError::Manifest("'abi' is not a 32-bit integer".into()))?,
        _ => {
            return Err(UpdateError::Manifest(
                "'abi' is missing or not a number".into(),
            ));
        }
    };

    let notes = match map.get("notes") {
        Some(Json::String(notes)) => Some(notes.clone()),
        None | Some(Json::Null) => None,
        _ => return Err(UpdateError::Manifest("'notes' is not a string".into())),
    };

    let mut assets = BTreeMap::new();
    match map.get("assets") {
        None | Some(Json::Null) => {}
        Some(Json::Object(entries)) => {
            for (platform, value) in entries {
                let Json::Object(asset) = value else {
                    return Err(UpdateError::Manifest(format!(
                        "asset '{platform}' is not an object"
                    )));
                };
                let url = match asset.get("url") {
                    Some(Json::String(url)) if !url.is_empty() => url.clone(),
                    _ => {
                        return Err(UpdateError::Manifest(format!(
                            "asset '{platform}' has no 'url'"
                        )));
                    }
                };
                let sha256 = match asset.get("sha256") {
                    Some(Json::String(sha)) if is_sha256(sha) => sha.to_ascii_lowercase(),
                    _ => {
                        return Err(UpdateError::Manifest(format!(
                            "asset '{platform}' has no 64-digit hex 'sha256'"
                        )));
                    }
                };
                assets.insert(platform.clone(), ReleaseAsset { url, sha256 });
            }
        }
        _ => return Err(UpdateError::Manifest("'assets' is not an object".into())),
    }

    Ok(ReleaseManifest {
        version,
        abi,
        notes,
        assets,
    })
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn as_u32(number: f64) -> Option<u32> {
    if number.is_finite() && number.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&number)
    {
        Some(number as u32)
    } else {
        None
    }
}

/// The subset of JSON a manifest needs. Objects are ordered maps so a repeated
/// key resolves to the last value, as JSON parsers conventionally do.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

/// Nests a manifest can reach before the reader refuses. The real document is
/// two levels deep; the bound keeps a hostile one from exhausting the stack.
const MAX_DEPTH: usize = 32;

impl<'a> JsonParser<'a> {
    fn new(text: &'a str) -> Self {
        JsonParser {
            bytes: text.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    fn parse(mut self) -> Result<Json, UpdateError> {
        self.skip_whitespace();
        let value = self.value()?;
        self.skip_whitespace();
        if self.pos != self.bytes.len() {
            return Err(self.error("trailing data after the document"));
        }
        Ok(value)
    }

    fn value(&mut self) -> Result<Json, UpdateError> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => {
                self.literal(b"true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.literal(b"null")?;
                Ok(Json::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("expected a value")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn object(&mut self) -> Result<Json, UpdateError> {
        self.enter()?;
        self.pos += 1; // '{'
        let mut map = BTreeMap::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':' after an object key"));
            }
            self.pos += 1;
            let value = self.value()?;
            map.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(self.error("expected ',' or '}' in an object")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, UpdateError> {
        self.enter()?;
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']' in an array")),
            }
        }
    }

    fn enter(&mut self) -> Result<(), UpdateError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("the document nests too deeply"));
        }
        Ok(())
    }

    fn string(&mut self) -> Result<String, UpdateError> {
        if self.peek() != Some(b'"') {
            return Err(self.error("expected a string"));
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(byte) = self.peek() {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The span before an escape or the closing quote is unescaped UTF-8
            // by construction; rejecting it means the document was not UTF-8.
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| self.error("a string is not valid UTF-8"))?,
            );
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.error("a string contains a control character")),
                None => return Err(self.error("unterminated string")),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), UpdateError> {
        let Some(byte) = self.peek() else {
            return Err(self.error("unterminated escape"));
        };
        self.pos += 1;
        match byte {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.hex4()?;
                let code = if (0xD800..=0xDBFF).contains(&first) {
                    // A high surrogate must be followed by its low half, or the
                    // escape names no scalar value at all.
                    if self.peek() != Some(b'\\') {
                        return Err(self.error("unpaired surrogate in a \\u escape"));
                    }
                    self.pos += 1;
                    if self.peek() != Some(b'u') {
                        return Err(self.error("unpaired surrogate in a \\u escape"));
                    }
                    self.pos += 1;
                    let second = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&second) {
                        return Err(self.error("invalid low surrogate in a \\u escape"));
                    }
                    0x1_0000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00)
                } else if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(self.error("unpaired surrogate in a \\u escape"));
                } else {
                    u32::from(first)
                };
                match char::from_u32(code) {
                    Some(c) => out.push(c),
                    None => return Err(self.error("a \\u escape names no character")),
                }
            }
            _ => return Err(self.error("unknown escape in a string")),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u16, UpdateError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.error("truncated \\u escape"));
        }
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.bytes[self.pos] {
                b'0'..=b'9' => self.bytes[self.pos] - b'0',
                b'a'..=b'f' => self.bytes[self.pos] - b'a' + 10,
                b'A'..=b'F' => self.bytes[self.pos] - b'A' + 10,
                _ => return Err(self.error("a \\u escape is not four hex digits")),
            };
            value = value * 16 + u16::from(digit);
            self.pos += 1;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, UpdateError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let digits = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == digits {
            return Err(self.error("a number has no digits"));
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            let fraction = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == fraction {
                return Err(self.error("a number has a trailing decimal point"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exponent = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == exponent {
                return Err(self.error("a number has an empty exponent"));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("a number is not valid UTF-8"))?;
        text.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| self.error("a number is out of range"))
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), UpdateError> {
        if self.bytes[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(self.error("expected a literal value"))
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn error(&self, message: &str) -> UpdateError {
        UpdateError::Json(format!("{message} at byte {}", self.pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
        "version": "0.2.0",
        "abi": 4,
        "notes": "https://example.invalid/notes",
        "assets": {
            "win32-x64": { "url": "https://example.invalid/omni.exe", "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
            "linux-x64": { "url": "https://example.invalid/omni", "sha256": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB" }
        }
    }"#;

    #[test]
    fn versions_parse_with_an_optional_v_and_build_metadata() {
        let version = Version::parse("v1.2.3").unwrap();
        assert_eq!((version.major, version.minor, version.patch), (1, 2, 3));
        assert_eq!(version.to_string(), "1.2.3");
        assert_eq!(
            Version::parse("1.2.3+build.7").unwrap(),
            Version::parse("1.2.3").unwrap()
        );
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("1.2.x").is_none());
        assert!(Version::parse("1.2.3-").is_none());
    }

    #[test]
    fn releases_outrank_their_pre_releases_and_numbers_sort_numerically() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0",
            "1.0.1",
            "1.1.0",
        ];
        for pair in ordered.windows(2) {
            let left = Version::parse(pair[0]).unwrap();
            let right = Version::parse(pair[1]).unwrap();
            assert!(left < right, "{} should sort before {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn a_manifest_carries_its_version_abi_and_assets() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.abi, 4);
        assert_eq!(
            manifest.notes.as_deref(),
            Some("https://example.invalid/notes")
        );
        assert_eq!(manifest.assets.len(), 2);
        let windows = manifest.asset_for("win32-x64").unwrap();
        assert_eq!(windows.url, "https://example.invalid/omni.exe");
        // Checksums are normalised to lowercase so comparisons are simple.
        assert_eq!(
            manifest.asset_for("linux-x64").unwrap().sha256,
            "b".repeat(64)
        );
        assert!(manifest.asset_for("plan9-x64").is_none());
    }

    #[test]
    fn a_manifest_without_the_required_fields_is_refused() {
        assert!(parse_manifest("{}").is_err());
        assert!(parse_manifest(r#"{"version":"1.0.0"}"#).is_err());
        assert!(parse_manifest(r#"{"version":"x","abi":4}"#).is_err());
        assert!(parse_manifest(r#"{"version":"1.0.0","abi":-1}"#).is_err());
        assert!(parse_manifest(r#"{"version":"1.0.0","abi":4.5}"#).is_err());
        assert!(parse_manifest(r#"{"version":"1.0.0","abi":"4"}"#).is_err());
        assert!(parse_manifest(r#"{"version":"1.0.0","abi":4,"assets":[]}"#).is_err());
        assert!(
            parse_manifest(
                r#"{"version":"1.0.0","abi":4,"assets":{"x":{"url":"u","sha256":"short"}}}"#
            )
            .is_err()
        );
        assert!(parse_manifest(&format!("x{}", " ".repeat(MAX_MANIFEST_LEN))).is_err());
    }

    #[test]
    fn malformed_json_is_refused_rather_than_partially_read() {
        assert!(matches!(parse_manifest("{"), Err(UpdateError::Json(_))));
        assert!(matches!(
            parse_manifest("[1,2]"),
            Err(UpdateError::Manifest(_))
        ));
        assert!(parse_manifest(r#"{"version":"1.0.0","abi":4} trailing"#).is_err());
        // A lone high surrogate names no scalar value.
        assert!(parse_manifest(r#"{"version":"1.0.0","abi":4,"notes":"\ud800"}"#).is_err());
        let deep = format!("{}{}", "[".repeat(40), "]".repeat(40));
        assert!(parse_manifest(&deep).is_err());
    }

    #[test]
    fn a_pair_of_surrogates_decodes_and_the_document_stays_usable() {
        // U+1F600, the same character the string tests use.
        let manifest =
            parse_manifest(r#"{"version":"1.0.0","abi":4,"notes":"\ud83d\ude00"}"#).unwrap();
        assert_eq!(manifest.notes.as_deref(), Some("\u{1F600}"));
    }

    #[test]
    fn a_newer_version_on_the_same_abi_is_available() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        assert_eq!(check_update("0.1.0", 4, &manifest), UpdateStatus::Available);
        assert_eq!(check_update("0.2.0", 4, &manifest), UpdateStatus::UpToDate);
        assert_eq!(check_update("0.3.0", 4, &manifest), UpdateStatus::UpToDate);
        assert_eq!(check_update("1.0.0", 4, &manifest), UpdateStatus::UpToDate);
        // A newer version on another ABI is not an upgrade this build can take.
        assert_eq!(
            check_update("0.1.0", 5, &manifest),
            UpdateStatus::AbiMismatch
        );
    }

    #[test]
    fn status_codes_match_the_abi_constants() {
        assert_eq!(UpdateStatus::UpToDate.code(), 0);
        assert_eq!(UpdateStatus::Available.code(), 1);
        assert_eq!(UpdateStatus::AbiMismatch.code(), 2);
        assert_eq!(UpdateStatus::Available.name(), "available");
    }

    #[test]
    fn platform_keys_match_the_release_names() {
        assert_eq!(platform_key("windows", "x86_64"), Some("win32-x64"));
        assert_eq!(platform_key("linux", "x86_64"), Some("linux-x64"));
        assert_eq!(platform_key("linux", "aarch64"), Some("linux-arm64"));
        assert_eq!(platform_key("macos", "x86_64"), Some("darwin-x64"));
        assert_eq!(platform_key("macos", "aarch64"), Some("darwin-arm64"));
        assert_eq!(platform_key("freebsd", "x86_64"), None);
        assert!(
            host_platform().is_some(),
            "the test host is one of the five"
        );
    }
}
