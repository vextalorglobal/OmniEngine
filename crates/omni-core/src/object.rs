//! The one place that turns a NaN box's address back into bytes.
//!
//! `Value` can say *that* a box holds an address; only this module says what it
//! addresses. Keeping the dereference here means the crate has one auditable
//! unsafe seam for strings instead of an `unsafe` block wherever a string is
//! printed, compared, or copied out — the same trick [`crate::arena::Slot`] uses
//! for the allocator.
//!
//! ## The byte layout
//!
//! ```text
//! offset 0        u32   len       bytes of payload
//! offset 4        u32   canary    always zero; a non-zero read means confusion
//! offset 8        [u8; len]       UTF-8 payload, which may contain interior NULs
//! offset 8 + len  u8    0         terminator, for callers that want one
//! ```
//!
//! The length lives with the string rather than in the box because a NaN box has
//! 48 usable bits and a pointer uses all of them. The terminator exists for C
//! hosts' convenience, never as the end marker: `len` decides where a string
//! ends, precisely so `"\u{0}"` is a legal one-byte string.

use crate::Value;
use crate::arena::Arena;
use std::fmt::Write;

/// Bytes of header before the payload: `len` plus the canary.
pub(crate) const STR_HEADER: usize = 8;

/// A string block is read as `u32`s, so it needs four bytes of alignment; eight
/// is what the engine asks for, so payloads start on a tidy boundary too.
pub(crate) const STR_ALIGN: usize = 8;

/// The largest string OmniEngine will build or accept. Bounds the egress block
/// and turns an accidental O(n²) concatenation loop into a loud failure instead
/// of a quiet one that consumes the machine.
pub const MAX_STRING_LEN: usize = 16 * 1024 * 1024;

/// A borrowed view of one engine-owned string.
///
/// The lifetime parameter is not decoration: a `StrRef` can only be built from a
/// borrow of the [`Arena`] that owns the bytes, so the bytes cannot be reset,
/// dropped, or bumped past while the view exists.
#[derive(Clone, Copy)]
pub struct StrRef<'a> {
    bytes: &'a [u8],
}

impl<'a> StrRef<'a> {
    /// Interpret `value` as a string held by `arena`.
    ///
    /// `None` unless the box is in the pointer row, its canary is clean, its
    /// length is plausible, and the whole span — payload *and* terminator — sits
    /// inside one live block.
    ///
    /// Note what this does *not* catch: an address into a [`Arena::rewind`]ed or
    /// reused block passes every test and yields the bytes still sitting there.
    /// That is the documented expiry rule — a stale string is a wrong read, never
    /// a crash — and it is why this function can be safe.
    pub fn checked(value: Value, arena: &'a Arena) -> Option<StrRef<'a>> {
        let addr = value.as_ptr()? as usize;
        let len = payload_len(arena.peek_u32s(addr)?)?;
        // Asking for header + payload + terminator proves nothing in the span can
        // run up against the end of a block.
        let block = arena.span(addr, STR_HEADER + len + 1)?;
        Some(StrRef {
            bytes: &block[STR_HEADER..STR_HEADER + len],
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The payload bytes, including any interior NULs.
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// `None` when the bytes are not UTF-8. Only reachable from a caller that
    /// bypassed the boundary checks — the lexer decodes into `char`s and
    /// `omni_define_string` validates on the way in — so it is an `Option` and not
    /// a `panic`.
    pub fn as_str(&self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes).ok()
    }

    /// Copy the bytes out, so a caller can hold them past the expiry rule.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.bytes.to_vec()
    }
}

impl std::fmt::Debug for StrRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.as_str() {
            Some(text) => write!(f, "{text:?}"),
            None => write!(f, "{:?} bytes (invalid utf-8)", self.len()),
        }
    }
}

/// Write the whole block — header, payload, terminator — at `p`.
///
/// # Safety
///
/// `p` must address `STR_HEADER + src.len() + 1` bytes of live, writable memory.
/// Making this an `unsafe fn` rather than a safe one that hides a dereference is
/// the point: every caller states where those bytes came from.
///
/// The header goes in through unaligned writes on purpose: the same function has
/// to fill a block inside an 8-aligned arena *and* one inside a `Box<[u8]>`, whose
/// alignment is 1, and a typed write through an under-aligned pointer is undefined
/// behaviour that x86 quietly permits and other targets do not.
unsafe fn write_at(p: *mut u8, src: &[u8]) {
    // Safety: the contract above, plus `src` being a slice that cannot alias a
    // block in an arena the caller just allocated from.
    unsafe {
        std::ptr::write_unaligned(p.cast::<u32>(), src.len() as u32);
        std::ptr::write_unaligned(p.cast::<u32>().add(1), 0);
        let payload = p.add(STR_HEADER);
        std::ptr::copy_nonoverlapping(src.as_ptr(), payload, src.len());
        *payload.add(src.len()) = 0;
    }
}

/// Allocate and fill a string block, returning its address.
///
/// The layout is defined here, not in the allocator, so `arena.rs` stays about
/// bytes and this module stays about strings.
pub(crate) fn build(arena: &mut Arena, src: &[u8]) -> *mut u8 {
    let p = arena.alloc_raw(STR_HEADER + src.len() + 1, STR_ALIGN);
    // Safety: `alloc_raw` returned exactly the bytes `write_at` writes.
    unsafe { write_at(p, src) };
    p
}

/// Allocate a string block that lives outside every arena.
///
/// This is what a host string is: bytes the engine owns for as long as the
/// binding does, at an address no arena claims. That is precisely why
/// [`StrRef::checked`] cannot read one — it proves ownership through the arena —
/// and why [`read_boxed`] exists beside it.
pub(crate) fn build_boxed(src: &[u8]) -> Box<[u8]> {
    let mut buf = vec![0u8; STR_HEADER + src.len() + 1].into_boxed_slice();
    // Safety: `buf` owns exactly the number of bytes `write_at` writes, and the
    // pointer comes from that live allocation.
    unsafe { write_at(buf.as_mut_ptr(), src) };
    buf
}

/// Read `addr` as the header of a block inside `block`, which is what a host
/// allocation looks like from the outside.
///
/// Same three tests as [`StrRef::checked`] — offset inside the allocation, clean
/// canary with a plausible length, whole span on hand — against a known-good
/// buffer instead of an arena, and written with indexed slices so the bounds
/// checking is Rust's rather than another `unsafe` block to audit.
pub(crate) fn read_boxed(block: &[u8], addr: usize) -> Option<&[u8]> {
    let start = block.as_ptr() as usize;
    if addr < start {
        return None;
    }
    let off = addr - start;
    let head = off.checked_add(STR_HEADER)?;
    if head > block.len() {
        return None;
    }
    let words = [
        u32::from_ne_bytes(block[off..off + 4].try_into().ok()?),
        u32::from_ne_bytes(block[off + 4..head].try_into().ok()?),
    ];
    let len = payload_len(words)?;
    let end = head.checked_add(len)?.checked_add(1)?;
    if end > block.len() {
        return None;
    }
    Some(&block[head..head + len])
}

/// Given the two header words, return the payload length — or `None` if this is
/// not a string block.
///
/// Shared by the arena path ([`StrRef::checked`]) and the engine's host-string
/// path so the rules cannot diverge: a dirty canary means the address held
/// something else, and a length over [`MAX_STRING_LEN`] means the same. What this
/// deliberately does *not* check is whether the bytes are on hand — each caller
/// knows the shape of what it holds, and bounds-checks against that itself.
pub(crate) fn payload_len(words: [u32; 2]) -> Option<usize> {
    let [len_word, canary] = words;
    if canary != 0 {
        return None;
    }
    usize::try_from(len_word)
        .ok()
        .filter(|len| *len <= MAX_STRING_LEN)
}

/// Render one string's bytes the way the language writes a literal.
///
/// Falls back to a placeholder rather than panicking, because this runs inside
/// error messages: a diagnostic that crashed while reporting a bad program is a
/// worse bug than the program.
pub(crate) fn quote(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            for ch in text.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c.is_control() => {
                        let _ = write!(out, "\\u{{{:X}}}", c as u32);
                    }
                    c => out.push(c),
                }
            }
        }
        Err(_) => out.push_str("<invalid utf-8>"),
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checked_ref_reads_its_bytes() {
        let mut arena = Arena::with_chunk_size(4096);
        let p = build(&mut arena, b"hello");
        let r = StrRef::checked(Value::from_ptr(p), &arena).expect("owned by the arena");
        assert_eq!(r.as_bytes(), b"hello");
        assert_eq!(r.as_str(), Some("hello"));
        assert_eq!(r.len(), 5);
        assert!(!r.is_empty());
        assert_eq!(r.to_bytes(), b"hello".to_vec());
    }

    #[test]
    fn an_empty_string_is_a_string() {
        let mut arena = Arena::with_chunk_size(4096);
        let p = build(&mut arena, b"");
        let r = StrRef::checked(Value::from_ptr(p), &arena).expect("built");
        assert!(r.is_empty());
        assert_eq!(r.as_bytes(), b"");
    }

    #[test]
    fn interior_nul_is_payload_not_terminator() {
        let mut arena = Arena::with_chunk_size(4096);
        let p = build(&mut arena, b"a\0b");
        let r = StrRef::checked(Value::from_ptr(p), &arena).expect("built");
        assert_eq!(r.len(), 3, "len must beat strlen");
        assert_eq!(r.as_bytes(), b"a\0b");
    }

    /// The type-confusion guard: an arbitrary live block whose first word looks
    /// like a length is rejected by the canary in the second word.
    #[test]
    fn a_non_string_block_is_not_read_as_a_string() {
        let mut arena = Arena::with_chunk_size(4096);
        let p = arena.alloc::<u64>(0xFFFF_FFFF_0000_0008);
        let r = StrRef::checked(Value::from_ptr(p.cast::<u8>()), &arena);
        assert!(r.is_none(), "the canary must reject a misread box");
    }

    #[test]
    fn a_pointer_from_elsewhere_is_rejected() {
        let arena = Arena::with_chunk_size(4096);
        let mut local = 0u8;
        let p = std::ptr::addr_of_mut!(local).cast::<u8>();
        assert!(StrRef::checked(Value::from_ptr(p), &arena).is_none());
        assert!(StrRef::checked(Value::NIL, &arena).is_none());
        assert!(StrRef::checked(Value::from_f64(42.0), &arena).is_none());
    }

    #[test]
    fn a_span_that_walks_off_the_block_is_rejected() {
        let mut arena = Arena::with_chunk_size(4096);
        let p = arena.alloc_raw(STR_HEADER, STR_ALIGN);
        // Safety: p is STR_HEADER bytes of owned, 4-aligned memory.
        unsafe {
            std::ptr::write(p.cast::<u32>(), 4096 * 4); // longer than the chunk
            std::ptr::write(p.cast::<u32>().add(1), 0); // clean canary
        }
        assert!(StrRef::checked(Value::from_ptr(p), &arena).is_none());
    }

    #[test]
    fn a_fabricated_length_is_refused_before_any_span_is_computed() {
        // u32::MAX as a length is the largest a header can claim. It must fail the
        // cap, and the caller's span check must then never run with it.
        assert_eq!(payload_len([u32::MAX, 0]), None);
        assert_eq!(payload_len([4, 0]), Some(4));
        assert_eq!(payload_len([0, 0]), Some(0));
        assert_eq!(payload_len([4, 7]), None, "dirty canary");
        assert_eq!(
            payload_len([MAX_STRING_LEN as u32, 0]),
            Some(MAX_STRING_LEN)
        );
        assert_eq!(payload_len([MAX_STRING_LEN as u32 + 1, 0]), None);
        assert_eq!(payload_len([0xFFFF_FFFF, 0xFFFF_FFFF]), None);
    }

    #[test]
    fn a_boxed_block_round_trips_through_its_own_address() {
        let boxed = build_boxed(b"host bytes");
        let v = Value::from_ptr(boxed.as_ptr());
        // The arena cannot see it, which is the whole reason `read_boxed` exists.
        let arena = Arena::with_chunk_size(4096);
        assert!(StrRef::checked(v, &arena).is_none());
        assert_eq!(
            read_boxed(&boxed, v.as_ptr().unwrap() as usize),
            Some(b"host bytes".as_slice())
        );
    }

    #[test]
    fn a_boxed_block_rejects_every_address_outside_it() {
        let boxed = build_boxed(b"payload");
        let start = boxed.as_ptr() as usize;
        assert!(read_boxed(&boxed, start - 1).is_none());
        assert!(read_boxed(&boxed, start + 1).is_none(), "unaligned header");
        assert!(read_boxed(&boxed, start + boxed.len()).is_none());
        assert!(
            read_boxed(&boxed, usize::MAX).is_none(),
            "no overflow through"
        );
        let mut other = 0u8;
        assert!(read_boxed(&boxed, std::ptr::addr_of_mut!(other) as usize).is_none());
    }

    #[test]
    fn a_boxed_empty_string_is_still_a_string() {
        let boxed = build_boxed(b"");
        let addr = boxed.as_ptr() as usize;
        assert_eq!(read_boxed(&boxed, addr), Some(b"".as_slice()));
    }

    /// The layout is one definition shared by an 8-aligned arena block and an
    /// align-1 box, so both must agree byte for byte.
    #[test]
    fn an_arena_block_and_a_boxed_block_are_the_same_bytes() {
        let mut arena = Arena::with_chunk_size(4096);
        let p = build(&mut arena, b"same");
        let boxed = build_boxed(b"same");
        // Safety: both spans are exactly STR_HEADER + 4 + 1 bytes of live memory.
        let arena_bytes = unsafe { std::slice::from_raw_parts(p, STR_HEADER + 4 + 1) }.to_vec();
        assert_eq!(arena_bytes, boxed.to_vec());
    }

    #[test]
    fn quoting_a_string_escapes_what_a_reader_would_misread() {
        assert_eq!(quote(b"hi"), "\"hi\"");
        assert_eq!(quote(br#"a"b"#), "\"a\\\"b\"");
        assert_eq!(quote(b"tab\there"), "\"tab\\there\"");
        assert_eq!(quote(br"a\\b"), r#""a\\\\b""#);
        assert_eq!(quote("✄".as_bytes()), "\"✄\"");
        assert_eq!(quote(b"\x01"), "\"\\u{1}\"");
        assert_eq!(quote(b""), "\"\"");
        assert_eq!(quote(&[0xFF]), "\"<invalid utf-8>\"");
    }
}
