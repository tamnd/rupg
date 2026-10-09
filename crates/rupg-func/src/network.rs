//! The functions and the operators of `inet` and `cidr` from `network.c`: the text forms such as `host` and `abbrev`, the masks, the containment operators such as `<<=`, and the bit and arithmetic operators. The comparison operators are in [`crate::compare`].

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Inet, NetFamily, Value, bitncmp, bitncommon};

use crate::{Call, Kernel, bad_value};

/// The `inet` value of an argument.
fn arg(args: &[Value], n: usize) -> Result<&Inet> {
    match args.get(n) {
        Some(Value::Inet(v)) => Ok(v),
        _ => Err(bad_value()),
    }
}

/// The `int8` value of an argument.
fn int8(args: &[Value], n: usize) -> Result<i64> {
    match args.get(n) {
        Some(Value::Int8(v)) => Ok(*v),
        _ => Err(bad_value()),
    }
}

/// The text of the address with the length of the whole address, which has no mask length.
fn address(ip: &Inet) -> String {
    let mut text = String::new();
    rupg_types::net_ntop(ip, ip.family.max_bits(), &mut text);
    text
}

/// `network_host`, `host(inet)`: the address with no mask length.
fn host(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(address(arg(args, 0)?)))
}

/// `network_show`, `text(inet)` and the cast to `text`: the address and the mask length, also for a whole address.
fn show(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    let mut text = address(ip);
    if !text.contains('/') {
        text.push('/');
        text.push_str(&ip.bits.to_string());
    }
    Ok(Value::Text(text))
}

/// `inet_abbrev`: the text form of `inet`.
fn inet_abbrev(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    let mut text = String::new();
    rupg_types::net_ntop(ip, ip.bits, &mut text);
    Ok(Value::Text(text))
}

/// `cidr_abbrev`: the network part with no zero octets after it, and the mask length.
fn cidr_abbrev(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let mut text = String::new();
    rupg_types::cidr_abbrev(arg(args, 0)?, &mut text);
    Ok(Value::Text(text))
}

/// `network_masklen`.
fn masklen(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(i32::from(arg(args, 0)?.bits)))
}

/// `network_family`: 4 or 6.
fn family(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(match arg(args, 0)?.family {
        NetFamily::V4 => 4,
        NetFamily::V6 => 6,
    }))
}

/// The mask of the first `bits` bits of an address.
fn mask(bits: u8) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (n, byte) in out.iter_mut().enumerate() {
        let left = usize::from(bits).saturating_sub(n * 8).min(8);
        *byte = if left == 0 { 0 } else { 0xff << (8 - left) };
    }
    out
}

/// A value of the family of `ip` with each byte of the address from `f`.
fn each_byte(ip: &Inet, bits: u8, f: impl Fn(usize) -> u8) -> Value {
    let mut addr = [0u8; 16];
    for (n, byte) in addr.iter_mut().take(ip.family.size()).enumerate() {
        *byte = f(n);
    }
    Value::Inet(Inet { family: ip.family, bits, addr })
}

/// `network_broadcast`: the address with the bits to the right of the mask set to 1.
fn broadcast(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    let m = mask(ip.bits);
    Ok(each_byte(ip, ip.bits, |n| ip.addr[n] | !m[n]))
}

/// `network_network`: the address with the bits to the right of the mask set to 0.
fn network(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    let m = mask(ip.bits);
    Ok(each_byte(ip, ip.bits, |n| ip.addr[n] & m[n]))
}

/// `network_netmask`: the mask as a whole address.
fn netmask(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    let m = mask(ip.bits);
    Ok(each_byte(ip, ip.family.max_bits(), |n| m[n]))
}

/// `network_hostmask`: the bits to the right of the mask as a whole address.
fn hostmask(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    let m = mask(ip.bits);
    Ok(each_byte(ip, ip.family.max_bits(), |n| !m[n]))
}

/// The mask length argument of `set_masklen`: -1 is the length of the whole address.
fn new_masklen(ip: &Inet, args: &[Value]) -> Result<u8> {
    let Some(Value::Int4(bits)) = args.get(1) else { return Err(bad_value()) };
    let bits = if *bits == -1 { i32::from(ip.family.max_bits()) } else { *bits };
    u8::try_from(bits).ok().filter(|b| *b <= ip.family.max_bits()).ok_or_else(|| {
        Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid mask length: {bits}"))
    })
}

/// `inet_set_masklen`: the same address with a new mask length.
fn inet_set_masklen(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    Ok(Value::Inet(Inet { bits: new_masklen(ip, args)?, ..*ip }))
}

/// `cidr_set_masklen`: the new mask length, with the bits to the right of it set to 0.
fn cidr_set_masklen(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    Ok(Value::Inet(ip.masked(new_masklen(ip, args)?)))
}

/// `inet_to_cidr`, the cast from `inet` to `cidr`: the bits to the right of the mask set to 0.
fn to_cidr(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    Ok(Value::Inet(ip.masked(ip.bits)))
}

/// `inet_same_family`.
fn same_family(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(arg(args, 0)?.family == arg(args, 1)?.family))
}

/// `inet_merge`: the smallest network that holds both values.
fn merge(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    if a.family != b.family {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "cannot merge addresses from different families",
        ));
    }
    Ok(Value::Inet(a.masked(bitncommon(&a.addr, &b.addr, a.bits.min(b.bits)))))
}

/// The containment test of two values of one family: the mask lengths pass `lengths`, and the first `bits` bits are equal. Values of two families are not contained.
fn contains(args: &[Value], lengths: fn(u8, u8) -> bool, bits: fn(u8, u8) -> u8) -> Result<Value> {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    Ok(Value::Bool(
        a.family == b.family
            && lengths(a.bits, b.bits)
            && bitncmp(&a.addr, &b.addr, bits(a.bits, b.bits)) == 0,
    ))
}

/// `network_sub`, `<<`: the first value is in the second and is not equal to it.
fn sub(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    contains(args, |a, b| a > b, |_, b| b)
}

/// `network_subeq`, `<<=`.
fn subeq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    contains(args, |a, b| a >= b, |_, b| b)
}

/// `network_sup`, `>>`.
fn sup(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    contains(args, |a, b| a < b, |a, _| a)
}

/// `network_supeq`, `>>=`.
fn supeq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    contains(args, |a, b| a <= b, |a, _| a)
}

/// `network_overlap`, `&&`: one value holds the other.
fn overlap(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    contains(args, |_, _| true, u8::min)
}

/// `network_cmp`, the support function of the B-tree operator class, which gives the result of the C code and not only -1, 0 or 1.
fn cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(rupg_types::network_cmp(arg(args, 0)?, arg(args, 1)?)))
}

/// `inetnot`, `~`.
fn not(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let ip = arg(args, 0)?;
    Ok(each_byte(ip, ip.bits, |n| !ip.addr[n]))
}

/// `inetand` and `inetor`: the bits of two addresses of one family, with the longer mask.
fn bitwise(args: &[Value], what: &str, f: fn(u8, u8) -> u8) -> Result<Value> {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    if a.family != b.family {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("cannot {what} inet values of different sizes"),
        ));
    }
    Ok(each_byte(a, a.bits.max(b.bits), |n| f(a.addr[n], b.addr[n])))
}

fn and(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    bitwise(args, "AND", |a, b| a & b)
}

fn or(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    bitwise(args, "OR", |a, b| a | b)
}

/// The error of an address out of range.
fn out_of_range() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "result is out of range")
}

/// `internal_inetpl`: the address plus a number, with the error of PostgreSQL when the result leaves the family.
fn plus(ip: &Inet, mut addend: i64) -> Result<Value> {
    let mut addr = [0u8; 16];
    let mut carry = 0i64;
    for n in (0..ip.family.size()).rev() {
        carry += i64::from(ip.addr[n]) + (addend & 0xff);
        addr[n] = u8::try_from(carry & 0xff).unwrap_or_default();
        carry >>= 8;
        addend = (addend & !0xff) / 0x100;
    }
    if !(addend == 0 && carry == 0 || addend == -1 && carry == 1) {
        return Err(out_of_range());
    }
    Ok(Value::Inet(Inet { addr, ..*ip }))
}

/// `inetpl`, `inet + int8`.
fn inetpl(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    plus(arg(args, 0)?, int8(args, 1)?)
}

/// `int8pl_inet`, `int8 + inet`, a function in SQL that calls `inetpl`.
pub(crate) fn int8pl_inet(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    plus(arg(args, 1)?, int8(args, 0)?)
}

/// `inetmi_int8`, `inet - int8`.
fn inetmi_int8(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    plus(arg(args, 0)?, int8(args, 1)?.wrapping_neg())
}

/// `inetmi`, `inet - inet`: the difference of the addresses as an `int8`.
fn inetmi(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    if a.family != b.family {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "cannot subtract inet values of different sizes",
        ));
    }
    let mut res = 0i64;
    let mut carry = 1i32;
    let mut byte = 0u32;
    for n in (0..a.family.size()).rev() {
        carry += i32::from(a.addr[n]) + i32::from(!b.addr[n]);
        let lobyte = carry & 0xff;
        if byte < 8 {
            res |= i64::from(lobyte) << (byte * 8);
        } else if (res < 0 && lobyte != 0xff) || (res >= 0 && lobyte != 0) {
            return Err(out_of_range());
        }
        carry >>= 8;
        byte += 1;
    }
    if carry == 0 && byte < 8 {
        res |= -1i64 << (byte * 8);
    }
    Ok(Value::Int8(res))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "network_host" => host,
        "network_show" => show,
        "inet_abbrev" => inet_abbrev,
        "cidr_abbrev" => cidr_abbrev,
        "network_masklen" => masklen,
        "network_family" => family,
        "network_broadcast" => broadcast,
        "network_network" => network,
        "network_netmask" => netmask,
        "network_hostmask" => hostmask,
        "inet_set_masklen" => inet_set_masklen,
        "cidr_set_masklen" => cidr_set_masklen,
        "inet_to_cidr" => to_cidr,
        "inet_same_family" => same_family,
        "inet_merge" => merge,
        "network_cmp" => cmp,
        "network_sub" => sub,
        "network_subeq" => subeq,
        "network_sup" => sup,
        "network_supeq" => supeq,
        "network_overlap" => overlap,
        "inetnot" => not,
        "inetand" => and,
        "inetor" => or,
        "inetpl" => inetpl,
        "inetmi_int8" => inetmi_int8,
        "inetmi" => inetmi,
        _ => return None,
    })
}
