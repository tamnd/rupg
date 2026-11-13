// Lifted from rudb crates/rudb-qc-ir/src/ty.rs at cd9f9676 (2026-10-05).
// Changes: none.

//! The value types of QIR, per `spec/compiler/06-qir.md` section 6.3.

use std::fmt;

/// A QIR value type.
///
/// Signedness lives in the opcode, not the type, the way it does in LLVM and CLIF. There is no
/// NULL type and no SQL type: a nullable SQL value is two QIR values, the data and an `i1`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, Default)]
#[repr(u8)]
pub enum Ty {
    /// No value. The result type of an instruction that only has an effect.
    #[default]
    Void = 0,
    /// A comparison result or a validity bit. One byte in memory.
    I1 = 1,
    /// An 8 bit integer.
    I8 = 2,
    /// A 16 bit integer.
    I16 = 3,
    /// A 32 bit integer.
    I32 = 4,
    /// A 64 bit integer.
    I64 = 5,
    /// A 128 bit integer: wide decimals, sum accumulators, HUGEINT and UUID.
    I128 = 6,
    /// An IEEE single.
    F32 = 7,
    /// An IEEE double.
    F64 = 8,
    /// An untyped 64 bit address.
    Ptr = 9,
    /// The 16 byte string header: a u32 length, then 12 inline bytes or a prefix and a pointer.
    Str16 = 10,
}

/// Every type, in encoding order, so that a header's five bits decode by indexing.
const ALL: [Ty; 11] = [
    Ty::Void,
    Ty::I1,
    Ty::I8,
    Ty::I16,
    Ty::I32,
    Ty::I64,
    Ty::I128,
    Ty::F32,
    Ty::F64,
    Ty::Ptr,
    Ty::Str16,
];

impl Ty {
    /// The type with this encoding, if there is one.
    #[must_use]
    pub fn from_bits(bits: u32) -> Option<Ty> {
        ALL.get(bits as usize).copied()
    }

    /// The name the text form uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Ty::Void => "void",
            Ty::I1 => "i1",
            Ty::I8 => "i8",
            Ty::I16 => "i16",
            Ty::I32 => "i32",
            Ty::I64 => "i64",
            Ty::I128 => "i128",
            Ty::F32 => "f32",
            Ty::F64 => "f64",
            Ty::Ptr => "ptr",
            Ty::Str16 => "str16",
        }
    }

    /// The type the text form names this way.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Ty> {
        ALL.iter().copied().find(|t| t.name() == name)
    }

    /// The width in bits. A `ptr` is 64 and a `str16` is 128.
    #[must_use]
    pub fn bits(self) -> u32 {
        match self {
            Ty::Void => 0,
            Ty::I1 => 1,
            Ty::I8 => 8,
            Ty::I16 => 16,
            Ty::I32 | Ty::F32 => 32,
            Ty::I64 | Ty::F64 | Ty::Ptr => 64,
            Ty::I128 | Ty::Str16 => 128,
        }
    }

    /// The width in memory, in bytes. An `i1` takes a byte.
    #[must_use]
    pub fn bytes(self) -> u32 {
        match self {
            Ty::Void => 0,
            Ty::I1 => 1,
            other => other.bits() / 8,
        }
    }

    /// Whether this is one of the integer types, `i1` included. A `ptr` is not.
    #[must_use]
    pub fn is_int(self) -> bool {
        matches!(self, Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::I128)
    }

    /// Whether this is an integer or a pointer, which is what an address or a bitwise op takes.
    #[must_use]
    pub fn is_intlike(self) -> bool {
        self.is_int() || self == Ty::Ptr
    }

    /// Whether this is `f32` or `f64`.
    #[must_use]
    pub fn is_float(self) -> bool {
        matches!(self, Ty::F32 | Ty::F64)
    }

    /// The bits a constant of this type may have set.
    #[must_use]
    pub fn mask(self) -> u128 {
        match self.bits() {
            0 => 0,
            128 => u128::MAX,
            n => (1u128 << n) - 1,
        }
    }

    /// `bits` read as a signed number of this width.
    #[must_use]
    pub fn signed(self, bits: u128) -> i128 {
        let n = self.bits();
        if n == 0 || n == 128 {
            return bits as i128;
        }
        let shift = 128 - n;
        ((bits << shift) as i128) >> shift
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Where the bytes of a `str16` that is not inline live, per section 6.3.
///
/// A static property the translator knows when it makes the value. There is no runtime tag, and
/// rule V10 of the verifier is what keeps a transient string out of state that outlives the morsel.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Class {
    /// Points into storage the snapshot pins. Every inline string is trivially persistent.
    #[default]
    Persistent,
    /// Points into a scan buffer that is valid for the current morsel only.
    Transient,
    /// Points into memory the query owns until it ends.
    Temporary,
}

impl Class {
    /// The suffix the text form puts on a `str16` result.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Class::Persistent => "persistent",
            Class::Transient => "transient",
            Class::Temporary => "temporary",
        }
    }

    /// The class the text form names this way.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Class> {
        [Class::Persistent, Class::Transient, Class::Temporary]
            .into_iter()
            .find(|c| c.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_round_trips_through_its_bits_and_its_name() {
        for t in ALL {
            assert_eq!(Ty::from_bits(t as u32), Some(t));
            assert_eq!(Ty::from_name(t.name()), Some(t));
        }
    }

    #[test]
    fn signed_reads_the_top_bit_of_the_width() {
        assert_eq!(Ty::I32.signed(0xffff_ffff), -1);
        assert_eq!(Ty::I8.signed(0x7f), 127);
        assert_eq!(Ty::I1.signed(1), -1);
        assert_eq!(Ty::I128.signed(u128::MAX), -1);
    }
}
