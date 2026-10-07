//! The typmod of each type that has one, as `RowDescription` and `pg_attribute.atttypmod` give it.
//!
//! Clients decode the typmod, so the encoding must be exact. A typmod of -1 means that the type has no modifier.
//!
//! Lifted from `crates/rudb-pgtypes/src/typmod.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

/// `VARHDRSZ`, which the typmod of the string types and of `numeric` adds for old reasons.
pub const VARHDRSZ: i32 = 4;

/// `MAX_INTERVAL_PRECISION` and `MAX_TIMESTAMP_PRECISION`.
pub const MAX_TIME_PRECISION: i32 = 6;

/// The typmod of `varchar(n)` and `bpchar(n)`.
pub const fn char_typmod(n: i32) -> i32 {
    n + VARHDRSZ
}

/// The `n` of `varchar(n)` or `bpchar(n)`, if the typmod has one.
pub const fn char_length(typmod: i32) -> Option<i32> {
    if typmod > VARHDRSZ { Some(typmod - VARHDRSZ) } else { None }
}

/// The typmod of `numeric(precision, scale)`, `make_numeric_typmod` in `numeric.c`. The scale can be negative.
pub const fn numeric_typmod(precision: i32, scale: i32) -> i32 {
    ((precision << 16) | (scale & 0x7ff)) + VARHDRSZ
}

/// The precision and the scale of a `numeric` typmod. The scale is 11 bits with a sign.
pub const fn numeric_precision_scale(typmod: i32) -> Option<(i32, i32)> {
    if typmod < VARHDRSZ {
        return None;
    }
    let packed = typmod - VARHDRSZ;
    Some(((packed >> 16) & 0xffff, ((packed & 0x7ff) ^ 1024) - 1024))
}

/// A field of an interval qualifier such as `DAY TO SECOND`. The value is the bit of the field in the range mask, from `src/include/utils/datetime.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IntervalField {
    Month = 1,
    Year = 2,
    Day = 3,
    Hour = 10,
    Minute = 11,
    Second = 12,
}

/// `INTERVAL_FULL_RANGE`, the range mask of an interval with no qualifier.
pub const INTERVAL_FULL_RANGE: i32 = 0x7fff;

/// `INTERVAL_FULL_PRECISION`, the precision of an interval with no precision.
pub const INTERVAL_FULL_PRECISION: i32 = 0xffff;

/// The range mask of a list of fields: `YEAR TO MONTH` is `[Year, Month]`.
pub fn interval_range(fields: &[IntervalField]) -> i32 {
    fields.iter().fold(0, |mask, &field| mask | 1 << field as u8)
}

/// `INTERVAL_TYPMOD`: the range mask in the upper half and the precision in the lower half.
pub const fn interval_typmod(precision: i32, range: i32) -> i32 {
    ((range & 0x7fff) << 16) | (precision & 0xffff)
}

/// The precision and the range mask of an interval typmod.
pub const fn interval_precision_range(typmod: i32) -> Option<(i32, i32)> {
    if typmod < 0 {
        return None;
    }
    Some((typmod & 0xffff, (typmod >> 16) & 0x7fff))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_typmods_match_the_table_in_the_notes() {
        assert_eq!(char_typmod(255), 259);
        assert_eq!(char_length(259), Some(255));
        assert_eq!(char_length(-1), None);
        assert_eq!(numeric_typmod(10, 2), 655366);
        assert_eq!(numeric_precision_scale(655366), Some((10, 2)));
        assert_eq!(numeric_precision_scale(numeric_typmod(5, -3)), Some((5, -3)));
        assert_eq!(numeric_precision_scale(numeric_typmod(1000, 1000)), Some((1000, 1000)));
        assert_eq!(numeric_precision_scale(-1), None);
        // `interval day to second(3)`, as `pg_attribute` stores it on the oracle.
        let range = interval_range(&[
            IntervalField::Day,
            IntervalField::Hour,
            IntervalField::Minute,
            IntervalField::Second,
        ]);
        assert_eq!(interval_typmod(3, range), 470286339);
        assert_eq!(interval_precision_range(470286339), Some((3, range)));
        assert_eq!(interval_typmod(INTERVAL_FULL_PRECISION, INTERVAL_FULL_RANGE), 0x7fff_ffff);
    }
}
