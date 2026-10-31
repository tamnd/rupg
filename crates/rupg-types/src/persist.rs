//! The byte form of a [`Value`], so that the rows of a table can be kept in the `.rupg` file.

use rupg_common::Result;
use rupg_common::persist::{Persist, Reader, corrupt};
use rupg_common::{persist_enum, persist_struct};

use crate::jsonpath::{jsonpath_deparse, jsonpath_parse};
use crate::{
    AnyArray, Array, ArrayDim, Inet, Interval, JsonPath, Lexeme, NetFamily, QueryItem, Range,
    Record, Value,
};

persist_enum!(Value {
    0 => Null,
    1 => Bool(b),
    2 => Int2(n),
    3 => Int4(n),
    4 => Int8(n),
    5 => Float4(f),
    6 => Float8(f),
    7 => Numeric(n),
    8 => Oid(n),
    9 => Char(c),
    10 => Text(s),
    11 => Bytea(b),
    12 => Uuid(u),
    13 => Date(d),
    14 => Time(t),
    15 => TimeTz(t, z),
    16 => Timestamp(t),
    17 => TimestampTz(t),
    18 => Interval(i),
    19 => Inet(i),
    20 => Array(a),
    21 => Record(r),
    22 => Range(r),
    23 => Multirange(r),
    24 => TsVector(l),
    25 => TsQuery(q),
    26 => JsonPath(p),
    27 => AnyArray(a),
});

persist_struct!(AnyArray { elem, text });
persist_struct!(Record { types, values, names });
persist_struct!(Array<Value> { dims, values });
persist_struct!(ArrayDim { len, lower });
persist_struct!(Range { flags, lower, upper });
persist_struct!(Lexeme { word, positions });
persist_struct!(Interval { time, day, month });
persist_struct!(Inet { family, bits, addr });
persist_enum!(NetFamily { 0 => V4, 1 => V6 });
persist_enum!(QueryItem {
    0 => Operand { word, weight, prefix },
    1 => Operator { op, distance, left },
});

/// A path is kept as its text form. The regular expressions of the path were checked when the path was made, so they are not checked again.
impl Persist for JsonPath {
    fn put(&self, out: &mut Vec<u8>) {
        let mut text = Vec::new();
        jsonpath_deparse(self, &mut text);
        text.put(out);
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        let text = String::take(input)?;
        jsonpath_parse(&text, false, &|_, _| Ok(()))
            .map_err(|_| corrupt("a jsonpath does not parse"))
    }
}

#[cfg(test)]
mod tests {
    use rupg_common::persist::{from_bytes, to_bytes};

    use super::*;
    use crate::numeric::numeric_in;

    #[test]
    fn values_come_back_from_their_bytes() {
        let path = jsonpath_parse(
            "strict $.a[1 to 3] ? (@ like_regex \"x.*\" flag \"i\")",
            false,
            &|_, _| Ok(()),
        )
        .unwrap();
        let values = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int2(-3),
            Value::Int4(7),
            Value::Int8(i64::MIN),
            Value::Float4(1.5),
            Value::Float8(f64::NAN),
            Value::Numeric(numeric_in("-12345.678900", -1).unwrap()),
            Value::Numeric(numeric_in("NaN", -1).unwrap()),
            Value::Oid(26),
            Value::Char(b'r'),
            Value::Text("héllo".into()),
            Value::Bytea(vec![0, 255, 7]),
            Value::Uuid([9; 16]),
            Value::Date(-1),
            Value::TimeTz(5, -3600),
            Value::TimestampTz(1),
            Value::Interval(Interval { time: 1, day: 2, month: 3 }),
            Value::Inet(Inet { family: NetFamily::V6, bits: 64, addr: [1; 16] }),
            Value::Array(Box::new(Array {
                dims: vec![ArrayDim { len: 2, lower: 1 }],
                values: vec![Some(Value::Int4(1)), None],
            })),
            Value::Record(Box::new(Record::new(vec![23, 25], vec![Value::Int4(1), Value::Null]))),
            Value::Range(Box::new(Range {
                flags: 2,
                lower: Value::Int4(1),
                upper: Value::Int4(9),
            })),
            Value::Multirange(vec![Range {
                flags: 0,
                lower: Value::Date(1),
                upper: Value::Date(4),
            }]),
            Value::TsVector(vec![Lexeme { word: "cat".into(), positions: vec![1, 0xc002] }]),
            Value::TsQuery(vec![
                QueryItem::Operator { op: 2, distance: 0, left: 2 },
                QueryItem::Operand { word: "a".into(), weight: 8, prefix: true },
                QueryItem::Operand { word: "b".into(), weight: 0, prefix: false },
            ]),
            Value::JsonPath(Box::new(path)),
            Value::AnyArray(Box::new(AnyArray { elem: 23, text: "{1,2}".into() })),
        ];
        let back: Vec<Value> = from_bytes(&to_bytes(&values)).unwrap();
        assert_eq!(format!("{back:?}"), format!("{values:?}"));
    }

    #[test]
    fn a_bad_tag_gives_an_error() {
        assert!(from_bytes::<Value>(&[200]).is_err());
    }
}
