// Lifted from rudb crates/rudb-graph/src/wire.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! The bytes a key map takes in a section payload.
//!
//! spec/graph/03-the-file-format.md section 3.3 asks for a fixed header carrying what the build
//! observed, then the form's own payload. This module is that layout and nothing else: it does not
//! know what a section is, what an extent is or where in a file the bytes go, because
//! `rudb-native` is the crate that knows those and it is above this one.
//!
//! The header is forty bytes and not the twenty four section 3.3 quotes. Three reasons, and the
//! difference is worth naming rather than quietly absorbing. `base` has to be an `i128` because a
//! key can be a `HUGEINT` or a dictionary code and a key map that could not hold one would be a
//! key map with an exception in it. The null count has to be there because a null is not a key and
//! the row count alone does not say how many rows the column had. And section 3.3 also requires
//! the four observed facts in the header, which is where distinctness and sortedness live. Forty
//! bytes against twenty four, on the six TPC-H tables that take the identity form, is ninety six
//! bytes in total, so the fidelity that would be lost by padding it back down is worth more than
//! the bytes.
//!
//! What the header does not hold is the maximum key, because every form derives it: identity from
//! the count, dense from the range, sorted from its last stored key. A number stored twice is a
//! number that can disagree with itself.

use rupg_common::{Error, Result};

use crate::keymap::{Form, KeyMap, Observed};

/// Bytes of fixed header at the front of a key map payload.
///
/// This is what goes in a section entry's `header_bytes`.
pub const HEADER_BYTES: usize = 40;

/// The payload layout version, in case the forms ever need a second one.
///
/// A section's `kind` is `RUDBKM1\0` and the trailing `1` is this number's public face: a second
/// layout becomes `RUDBKM2\0` and an older reader ignores it by the rule in section 3.2. So this
/// byte is belt and braces rather than the mechanism, and it exists because a mismatch here is a
/// clearer error than a misparse further in.
const LAYOUT: u8 = 1;

/// A key map's bytes, ready to be split into extents and written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    /// What goes in the section entry's `flags`, which is the form.
    ///
    /// The form being in the entry rather than only in the payload is what lets a reader decide
    /// whether it wants this key map at all without reading a byte of it.
    pub flags: u32,
    /// What goes in the section entry's `header_bytes`.
    pub header_bytes: u32,
    /// The whole payload, header first.
    pub bytes: Vec<u8>,
}

/// Writes a key map's payload.
///
/// `type_tag` is the column's logical type as the format numbers it. It is passed in rather than
/// derived because the mapping from a logical type to a tag belongs to the format, and this crate
/// sits below the format on purpose. Section 3.3 wants it in the header so that a section is
/// self describing: a tool reading a payload out of a file can say what column it was built
/// against without holding the directory as well.
///
/// # Errors
///
/// If a length does not fit the width the layout gives it, which means a key map larger than the
/// format can name.
pub fn encode(map: &KeyMap, type_tag: u8) -> Result<Payload> {
    let observed = *map.observed();
    let mut bytes = Vec::with_capacity(HEADER_BYTES + map.bytes());
    bytes.extend_from_slice(&map.base().to_le_bytes());
    bytes.extend_from_slice(&observed.rows.to_le_bytes());
    bytes.extend_from_slice(&observed.nulls.to_le_bytes());
    bytes.push(map.form().tag());
    bytes.push(type_tag);
    bytes.push(u8::from(observed.distinct));
    bytes.push(u8::from(observed.sorted));
    bytes.push(LAYOUT);
    bytes.extend_from_slice(&[0; 3]);
    debug_assert_eq!(bytes.len(), HEADER_BYTES, "the key map header is forty bytes");
    map.write_body(&mut bytes)?;
    Ok(Payload {
        flags: u32::from(map.form().tag()),
        header_bytes: u32::try_from(HEADER_BYTES).map_err(|_| malformed("header overflow"))?,
        bytes,
    })
}

/// Reads a key map's payload, returning it with the type tag it was built against.
///
/// # Errors
///
/// If the payload is shorter than its header, names a form or a layout this build does not know,
/// or holds a body that does not match the lengths its header implies. Every one of those is a
/// section to drop rather than a query to fail: section 3.1 says a table with no sections answers
/// the same, so a caller's response to an error here is to ignore this key map.
pub fn decode(bytes: &[u8]) -> Result<(KeyMap, u8)> {
    if bytes.len() < HEADER_BYTES {
        return Err(malformed("a key map payload is shorter than its header"));
    }
    let base = i128::from_le_bytes(bytes[0..16].try_into().map_err(|_| torn())?);
    let rows = u64::from_le_bytes(bytes[16..24].try_into().map_err(|_| torn())?);
    let nulls = u64::from_le_bytes(bytes[24..32].try_into().map_err(|_| torn())?);
    let form = Form::from_tag(bytes[32])?;
    let type_tag = bytes[33];
    let distinct = flag(bytes[34])?;
    let sorted = flag(bytes[35])?;
    if bytes[36] != LAYOUT {
        return Err(malformed(format!("key map layout {} is not one this build knows", bytes[36])));
    }
    let observed = Observed {
        rows,
        nulls,
        distinct,
        sorted,
        min: (rows > 0).then_some(base),
        // The maximum is the form's business, because each of the three derives it differently and
        // none of them stores it. `read_body` fills it in.
        max: None,
    };
    let map = KeyMap::read_body(form, base, observed, &bytes[HEADER_BYTES..])?;
    Ok((map, type_tag))
}

/// A boolean on disk is zero or one and nothing else.
///
/// Refusing the other two hundred and fifty four is not pedantry: a byte that is neither is a torn
/// payload, and reading it as true would make a key map claim a distinctness nobody observed,
/// which is the one thing in this layer that turns into a wrong answer rather than a slow one.
fn flag(byte: u8) -> Result<bool> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(malformed("a flag byte in a key map header is neither zero nor one")),
    }
}

fn torn() -> Error {
    malformed("a key map header is torn")
}

fn malformed(message: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid key map payload: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag the format gives `INTEGER`. Any byte does here; the codec carries it and does not
    /// interpret it, which is the whole point of it being passed in.
    const INTEGER: u8 = 4;

    fn keys(values: &[i128]) -> Vec<Option<i128>> {
        values.iter().copied().map(Some).collect()
    }

    /// Encodes, decodes, and checks that every key still resolves to the row that held it.
    ///
    /// Resolving is the assertion that matters. Comparing two `KeyMap`s field by field would pass
    /// for a payload that round trips its bytes and its arithmetic separately, and the arithmetic
    /// is the part a wrong width breaks.
    fn survives(column: &[Option<i128>]) -> KeyMap {
        let built = KeyMap::build(column).expect("build");
        let payload = encode(&built, INTEGER).expect("encode");
        assert_eq!(payload.header_bytes as usize, HEADER_BYTES);
        assert_eq!(payload.flags, u32::from(built.form().tag()));
        let (read, type_tag) = decode(&payload.bytes).expect("decode");
        assert_eq!(type_tag, INTEGER, "the type tag is carried, not interpreted");
        assert_eq!(read.form(), built.form(), "the form survives");
        assert_eq!(read.observed(), built.observed(), "the observed facts survive");
        for (rid, key) in column.iter().enumerate() {
            let Some(key) = *key else { continue };
            assert_eq!(
                read.lookup(key).expect("lookup"),
                Some(rid as u64),
                "key {key} did not survive the round trip"
            );
        }
        read
    }

    #[test]
    fn the_identity_form_round_trips_and_is_header_only() {
        let map = survives(&keys(&(1..=1000).collect::<Vec<i128>>()));
        assert_eq!(map.form(), Form::Identity);
        let payload = encode(&map, INTEGER).expect("encode");
        assert_eq!(payload.bytes.len(), HEADER_BYTES, "section 3.3: no extents beyond the header");
    }

    #[test]
    fn the_dense_form_round_trips_with_its_rank_index() {
        // The index is stored rather than rebuilt at open. Rebuilding is a pass over the bitmap,
        // and section 3.4's SF100 arithmetic has bitmaps at ninety four megabytes, so a pass is a
        // thing you notice at open time and twelve percent of the bytes is not.
        let map = survives(&keys(&(0..20_000).map(|value| value * 2).collect::<Vec<i128>>()));
        assert_eq!(map.form(), Form::Dense);
        let payload = encode(&map, INTEGER).expect("encode");
        let bitmap = 40_000 / 8;
        let body = payload.bytes.len() - HEADER_BYTES;
        assert!(body > bitmap, "the body is {body} bytes and the bitmap alone is {bitmap}");
        assert!(body < bitmap * 5 / 4, "the index costs about an eighth, not {body} over {bitmap}");
    }

    #[test]
    fn the_sorted_form_round_trips_with_both_of_its_bit_packed_arrays() {
        let map = survives(&keys(&[500, 3, 9000, 12, 7, 88, 41, 6]));
        assert_eq!(map.form(), Form::Sorted);
    }

    #[test]
    fn the_permuted_form_round_trips_with_its_bitmap_index_and_rids() {
        // Keys a third of their range, stored in an order that is not theirs, the shape of
        // `o_orderkey` on a file clustered by date.
        let column: Vec<Option<i128>> =
            (0..5_000_i128).map(|at| Some((at * 7_919 % 5_000) * 3)).collect();
        let map = survives(&column);
        assert_eq!(map.form(), Form::Permuted);
        assert_eq!(map.lookup(1).expect("lookup"), None);
    }

    #[test]
    fn a_column_with_nulls_round_trips_and_keeps_its_null_count() {
        let column = vec![Some(10), None, Some(20), None, Some(30)];
        let map = survives(&column);
        assert_eq!(map.observed().nulls, 2);
        assert_eq!(map.observed().rows, 3);
    }

    #[test]
    fn a_column_of_one_key_round_trips() {
        survives(&keys(&[42]));
    }

    #[test]
    fn negative_keys_round_trip_because_the_base_is_an_i128() {
        // The reason the header is forty bytes rather than twenty four. A base that had to fit an
        // i64 would refuse this column, and refusing a column is not something a key map gets to
        // do to a key type the engine supports.
        survives(&keys(&[i128::MIN + 1, i128::MIN + 9, i128::MIN + 4]));
    }

    #[test]
    fn an_empty_key_map_round_trips_and_resolves_nothing() {
        let built = KeyMap::build(&[]).expect("build");
        let payload = encode(&built, INTEGER).expect("encode");
        let (read, _) = decode(&payload.bytes).expect("decode");
        assert!(read.is_empty());
        assert_eq!(read.observed().min, None, "an empty map has no minimum, not a minimum of zero");
        assert_eq!(read.lookup(0).expect("lookup"), None);
    }

    #[test]
    fn a_non_distinct_column_carries_that_fact_through_the_round_trip() {
        // Section 2.3's verification is what decides whether a link gets built at all, so it has to
        // survive being written down. A payload that lost it would produce a link on a parent side
        // that is not unique, which is a wrong answer rather than a slow one.
        let built = KeyMap::build(&keys(&[5, 7, 5, 9])).expect("build");
        assert!(!built.observed().distinct);
        let payload = encode(&built, INTEGER).expect("encode");
        let (read, _) = decode(&payload.bytes).expect("decode");
        assert!(!read.observed().distinct);
        assert!(!read.observed().usable_as_parent());
    }

    #[test]
    fn a_payload_shorter_than_its_header_is_refused() {
        let built = KeyMap::build(&keys(&[1, 2, 3])).expect("build");
        let payload = encode(&built, INTEGER).expect("encode");
        for cut in [0, 1, HEADER_BYTES - 1] {
            assert!(decode(&payload.bytes[..cut]).is_err(), "a payload of {cut} bytes is refused");
        }
    }

    #[test]
    fn a_form_this_build_does_not_know_is_refused_rather_than_guessed() {
        let built = KeyMap::build(&keys(&[1, 2, 3])).expect("build");
        let mut payload = encode(&built, INTEGER).expect("encode");
        payload.bytes[32] = 9;
        let error = decode(&payload.bytes).expect_err("refused");
        assert!(error.to_string().contains("form 9"), "{error}");
    }

    #[test]
    fn a_layout_this_build_does_not_know_is_refused() {
        let built = KeyMap::build(&keys(&[1, 2, 3])).expect("build");
        let mut payload = encode(&built, INTEGER).expect("encode");
        payload.bytes[36] = LAYOUT + 1;
        let error = decode(&payload.bytes).expect_err("refused");
        assert!(error.to_string().contains("layout"), "{error}");
    }

    #[test]
    fn a_flag_byte_that_is_neither_zero_nor_one_is_refused() {
        // Reading a torn byte as true would make a key map claim a distinctness nobody observed,
        // and a link built on that claim resolves to the wrong row.
        let built = KeyMap::build(&keys(&[1, 2, 3])).expect("build");
        let mut payload = encode(&built, INTEGER).expect("encode");
        payload.bytes[34] = 2;
        assert!(decode(&payload.bytes).is_err(), "a torn distinct flag is refused");

        let mut payload = encode(&built, INTEGER).expect("encode");
        payload.bytes[35] = 0xff;
        assert!(decode(&payload.bytes).is_err(), "a torn sorted flag is refused");
    }

    #[test]
    fn a_truncated_body_is_refused_rather_than_read_past() {
        for column in [
            keys(&(0..2000).map(|value| value * 2).collect::<Vec<i128>>()),
            keys(&[500, 3, 9000, 12, 7, 88, 41, 6]),
        ] {
            let built = KeyMap::build(&column).expect("build");
            let payload = encode(&built, INTEGER).expect("encode");
            let short = &payload.bytes[..payload.bytes.len() - 1];
            assert!(decode(short).is_err(), "a truncated {:?} body is refused", built.form());
        }
    }

    #[test]
    fn a_body_where_the_header_expects_none_is_refused() {
        // The identity form's payload is its header. Trailing bytes mean the header and the body
        // disagree about which form this is, and the safe reading of a disagreement is neither.
        let built = KeyMap::build(&keys(&(1..=10).collect::<Vec<i128>>())).expect("build");
        let mut payload = encode(&built, INTEGER).expect("encode");
        assert_eq!(payload.bytes.len(), HEADER_BYTES);
        payload.bytes.push(0);
        assert!(decode(&payload.bytes).is_err());
    }
}
