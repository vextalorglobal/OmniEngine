//! The NaN-boxed OmniEngine value.
//!
//! An `f64` is a sign bit, an 11-bit exponent, and a 52-bit mantissa. When the
//! exponent is all ones and the mantissa is nonzero the value is a NaN, and
//! ordinary arithmetic can only produce *quiet* NaNs. That leaves a large region
//! of bit patterns that no computation will ever generate, and a machine that
//! never generates a pattern is free to spend it on tags.
//!
//! The consequence for callers: every scalar is eight bytes copied on the
//! stack, with no heap cell and no collector to pause for.

use std::fmt;

/// A row inside the double-precision NaN space. Because bit 51 (the quiet bit)
/// is clear here, this row is the *signaling* NaN region, which IEEE 754
/// arithmetic never produces. That is what makes it safe to spend on pointers.
const PTR_ROW: u64 = 0x7FF7_0000_0000_0000;

/// The canonical quiet NaN. Its row hosts the singleton tags.
const QNAN: u64 = 0x7FF8_0000_0000_0000;

/// 48 usable address bits, which is the whole of x86-64 user-space address
/// range on Windows and Linux.
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// The high-16 test for the pointer row.
const PTR_ROW_SHIFTED: u64 = PTR_ROW >> 48;

/// The high-32 test for the canonical-NaN row.
const QNAN_ROW_SHIFTED: u64 = QNAN >> 32;

/// Number of singleton tag slots reserved in the canonical-NaN row.
const SINGLETON_SLOTS: u64 = 16;

/// An OmniEngine value: eight bytes, copyable, no destructor.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct Value(u64);

impl Value {
    pub const NIL: Value = Value(QNAN + 1);
    pub const TRUE: Value = Value(QNAN + 2);
    pub const FALSE: Value = Value(QNAN + 3);
    pub const UNDEF: Value = Value(QNAN + 4);

    /// Box an `f64`. Any NaN pattern collapses to the canonical quiet NaN, so a
    /// payload arriving from a host can never impersonate one of the tag slots.
    #[inline]
    pub fn from_f64(n: f64) -> Value {
        if n.is_nan() {
            Value(QNAN)
        } else {
            Value(n.to_bits())
        }
    }

    /// Box a boolean as one of the two singleton tags.
    #[inline]
    pub fn bool(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    /// True when the box is in the signaling-NaN row and therefore holds a
    /// heap address rather than a number.
    ///
    /// Note what this does *not* test, because an earlier draft did: tagging
    /// pointers as `v >> 48 == 0` looks natural (x86-64 user addresses do have
    /// zero high bits) but `0.0` is exactly `0x0000_0000_0000_0000`, so every
    /// zero in a program would have been read as a null dereference. The same
    /// trap catches every denormal below roughly 2^-969.
    #[inline]
    pub fn is_pointer(self) -> bool {
        self.0 >> 48 == PTR_ROW_SHIFTED
    }

    /// True for the reserved singleton slots 1..=15 of the canonical-NaN row.
    /// Slot 0 is the double NaN itself and must remain a number.
    #[inline]
    pub fn is_singleton(self) -> bool {
        if self.0 >> 32 != QNAN_ROW_SHIFTED {
            return false;
        }
        let low = self.0 & 0xFFFF_FFFF;
        low != 0 && low < SINGLETON_SLOTS
    }

    /// Everything that matches neither tag rule is a double. This is the
    /// fallback of the classification order, not a positive test.
    #[inline]
    pub fn is_double(self) -> bool {
        !self.is_pointer() && !self.is_singleton()
    }

    /// Alias used by the evaluator: Phase 1 has only one numeric type.
    #[inline]
    pub fn is_number(self) -> bool {
        self.is_double()
    }

    /// True for the boxed double NaN.
    #[inline]
    pub fn is_nan(self) -> bool {
        self.as_f64().is_some_and(f64::is_nan)
    }

    /// Unbox, or `None` if this is a pointer or a singleton.
    #[inline]
    pub fn as_f64(self) -> Option<f64> {
        self.is_double().then(|| f64::from_bits(self.0))
    }

    #[inline]
    pub fn as_bool(self) -> Option<bool> {
        // Compare raw bits rather than through PartialEq, which would route
        // back through as_f64 for no benefit.
        match self.0 {
            x if x == Value::TRUE.0 => Some(true),
            x if x == Value::FALSE.0 => Some(false),
            _ => None,
        }
    }

    /// Box a heap address.
    ///
    /// # Safety
    ///
    /// Callers must pass an address whose top 16 bits are zero. A kernel or
    /// tagged pointer would be silently truncated to 48 bits; the debug build
    /// asserts it, the release build does not.
    #[inline]
    pub fn from_ptr(ptr: *const u8) -> Value {
        let raw = ptr as u64;
        debug_assert_eq!(raw >> 48, 0, "pointer does not fit in 48 bits");
        Value(PTR_ROW | (raw & ADDR_MASK))
    }

    #[inline]
    pub fn as_ptr(self) -> Option<*const u8> {
        // then_some, not then: the mask is a plain bit operation, so there is
        // nothing worth deferring.
        self.is_pointer()
            .then_some((self.0 & ADDR_MASK) as *const u8)
    }

    /// OmniEngine truthiness: false for `null`, `undefined`, `false`, `0`, and
    /// `NaN`; true otherwise.
    #[inline]
    pub fn truthy(self) -> bool {
        match self.as_bool() {
            Some(b) => b,
            None => self.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        }
    }

    /// The raw box, for the FFI layer and for tests.
    #[inline]
    pub fn bits(self) -> u64 {
        self.0
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self.as_f64(), other.as_f64()) {
            // Both numbers: follow IEEE, so NaN != NaN and 0.0 == -0.0.
            (Some(a), Some(b)) => a == b,
            // Both tagged: identical bits are identical values.
            (None, None) => self.0 == other.0,
            _ => false,
        }
    }
}

impl From<f64> for Value {
    #[inline]
    fn from(n: f64) -> Value {
        Value::from_f64(n)
    }
}

impl From<bool> for Value {
    #[inline]
    fn from(b: bool) -> Value {
        Value::bool(b)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_f64() {
            Some(n) if n.is_nan() => f.write_str("NaN"),
            Some(n) if n.is_infinite() => {
                f.write_str(if n > 0.0 { "Infinity" } else { "-Infinity" })
            }
            // Integral values print without a trailing `.0`, which is what a
            // script author expects from `40 + 2`.
            Some(n) if n == n.trunc() && n.abs() < 9.007_199_254_740_992e15 => {
                write!(f, "{}", n as i64)
            }
            Some(n) => write!(f, "{n}"),
            None if self.as_bool() == Some(true) => f.write_str("true"),
            None if self.as_bool() == Some(false) => f.write_str("false"),
            None if self.0 == Value::NIL.0 => f.write_str("null"),
            None if self.0 == Value::UNDEF.0 => f.write_str("undefined"),
            None => f.write_str("<object>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f64_round_trips() {
        for n in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            42.0,
            1e308,
            -1e308,
            3.5,
            f64::MIN,
            f64::MAX,
        ] {
            let v = Value::from_f64(n);
            assert!(v.is_double(), "{n} must stay a double");
            assert_eq!(v.as_f64(), Some(n));
        }
    }

    /// Regression guard for the encoding bug: a pointer tag of `v >> 48 == 0`
    /// is the exact bit pattern of 0.0 and of every denormal, so zero would
    /// have been misread as a heap address.
    #[test]
    fn zero_is_never_a_pointer() {
        assert!(!Value::from_f64(0.0).is_pointer());
        assert!(!Value::from_f64(-0.0).is_pointer());
        assert!(!Value::from_f64(f64::from_bits(1)).is_pointer()); // smallest denormal
        assert_eq!(Value::from_f64(0.0).as_f64(), Some(0.0));
        assert_eq!(
            Value::from_f64(f64::from_bits(1)).as_f64(),
            Some(f64::from_bits(1))
        );
    }

    #[test]
    fn infinities_stay_doubles() {
        assert!(Value::from_f64(f64::INFINITY).is_double());
        assert!(Value::from_f64(f64::NEG_INFINITY).is_double());
        assert_eq!(Value::from_f64(f64::INFINITY).as_f64(), Some(f64::INFINITY));
    }

    #[test]
    fn incoming_nan_is_normalised_and_still_a_number() {
        let v = Value::from_f64(f64::NAN);
        assert!(v.is_number());
        assert!(v.is_nan());
        assert!(!v.is_pointer());
        assert!(!v.is_singleton());
        // A weird quiet NaN payload from a host must not become a tag.
        let sneaky = Value::from_f64(f64::from_bits(0x7FF8_0000_0000_0009));
        assert!(sneaky.is_nan(), "raw tag-shaped NaN must normalise");
        assert!(!sneaky.is_singleton());
    }

    #[test]
    fn singletons_are_not_numbers() {
        for s in [Value::NIL, Value::UNDEF, Value::TRUE, Value::FALSE] {
            assert!(s.is_singleton());
            assert!(!s.is_double());
            assert_eq!(s.as_f64(), None);
        }
        assert_ne!(Value::TRUE, Value::FALSE);
        assert_eq!(Value::NIL, Value::NIL);
        assert_eq!(Value::TRUE.as_bool(), Some(true));
        assert_eq!(Value::NIL.as_bool(), None);
    }

    #[test]
    fn pointer_tag_and_untag() {
        let mut x: u64 = 7;
        let p = std::ptr::addr_of_mut!(x).cast::<u8>();
        let v = Value::from_ptr(p);
        assert!(v.is_pointer());
        assert!(!v.is_double());
        assert!(!v.is_number());
        assert_eq!(v.as_ptr(), Some(p.cast_const()));
    }

    #[test]
    fn value_is_eight_bytes_and_copy() {
        assert_eq!(std::mem::size_of::<Value>(), 8);
        let a = Value::from_f64(1.0);
        let b = a; // Copy, not a move: the zero-overhead claim in one line.
        assert_eq!(a, b);
    }

    #[test]
    fn ieee_equality_semantics_are_preserved() {
        assert_eq!(Value::from_f64(0.0), Value::from_f64(-0.0));
        assert_ne!(Value::from_f64(f64::NAN), Value::from_f64(f64::NAN));
        assert_ne!(Value::from_f64(1.0), Value::TRUE);
    }

    #[test]
    fn truthiness_matches_script_conventions() {
        assert!(!Value::from_f64(0.0).truthy());
        assert!(!Value::from_f64(f64::NAN).truthy());
        assert!(!Value::NIL.truthy());
        assert!(!Value::FALSE.truthy());
        assert!(Value::from_f64(1.0).truthy());
        assert!(Value::TRUE.truthy());
    }

    #[test]
    fn display_matches_number_formatting() {
        assert_eq!(Value::from_f64(42.0).to_string(), "42");
        assert_eq!(Value::from_f64(40.0 + 2.0).to_string(), "42");
        assert_eq!(Value::from_f64(2.5).to_string(), "2.5");
        assert_eq!(Value::TRUE.to_string(), "true");
        assert_eq!(Value::NIL.to_string(), "null");
        assert_eq!(Value::UNDEF.to_string(), "undefined");
        assert_eq!(Value::from_f64(f64::NAN).to_string(), "NaN");
    }
}
