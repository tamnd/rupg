//! The functions of `macaddr` and `macaddr8` from `mac.c` and `mac8.c`: `trunc`, the bit operators, `macaddr8_set7bit` and the casts. A value is the bytes of the address in the bits of [`Value::Int8`]. The comparison operators are in [`crate::compare`].

use rupg_common::{Error, Result, SqlState};
use rupg_types::{self as types, Value};

use crate::{Call, Kernel, bad_value};

/// The bits of all the bytes of a `macaddr`.
const MAC6: u64 = 0xffff_ffff_ffff;

/// The kernel of a function of `macaddr` or `macaddr8` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "macaddr_trunc" => macaddr_trunc,
        "macaddr8_trunc" => macaddr8_trunc,
        "macaddr_not" => macaddr_not,
        "macaddr8_not" => macaddr8_not,
        "macaddr_and" | "macaddr8_and" => and,
        "macaddr_or" | "macaddr8_or" => or,
        "macaddr8_set7bit" => set7bit,
        "macaddrtomacaddr8" => to_macaddr8,
        "macaddr8tomacaddr" => to_macaddr,
        _ => return None,
    })
}

/// The bits of the address of an argument.
fn bits(args: &[Value], n: usize) -> Result<u64> {
    match args.get(n) {
        Some(Value::Int8(v)) => Ok(v.cast_unsigned()),
        _ => Err(bad_value()),
    }
}

fn value(bits: u64) -> Value {
    Value::Int8(bits.cast_signed())
}

/// `macaddr_trunc`: the first 3 bytes, the manufacturer, with zeros after them.
fn macaddr_trunc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(bits(args, 0)? & 0xffff_ff00_0000))
}

fn macaddr8_trunc(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(bits(args, 0)? & 0xffff_ff00_0000_0000))
}

fn macaddr_not(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(!bits(args, 0)? & MAC6))
}

fn macaddr8_not(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(!bits(args, 0)?))
}

fn and(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(bits(args, 0)? & bits(args, 1)?))
}

fn or(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(bits(args, 0)? | bits(args, 1)?))
}

/// `macaddr8_set7bit`: the 7th bit of the first byte set, which makes a modified EUI-64 for IPv6.
fn set7bit(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(bits(args, 0)? | 0x0200_0000_0000_0000))
}

fn to_macaddr8(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(value(types::macaddr_to_macaddr8(bits(args, 0)?)))
}

fn to_macaddr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    types::macaddr8_to_macaddr(bits(args, 0)?).map(value).ok_or_else(|| {
        Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            "macaddr8 data out of range to convert to macaddr",
        )
        .with_hint(
            "Only addresses that have FF and FE as values in the 4th and 5th bytes from the left, for example xx:xx:xx:ff:fe:xx:xx:xx, are eligible to be converted from macaddr8 to macaddr.",
        )
    })
}
