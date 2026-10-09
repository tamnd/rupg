//! `inet` and `cidr`, an IPv4 or IPv6 address with the length of its network part.
//!
//! The text input is a port of `network_in` in `src/backend/utils/adt/network.c` with `inet_net_pton.c`. The text output is a port of `network_out` with `pg_inet_net_ntop` in `src/port/inet_net_ntop.c`, and [`cidr_abbrev`] is a port of `inet_cidr_ntop.c`. The binary forms are `network_recv` and `network_send`, and [`network_cmp`] is `network_cmp_internal`.
//!
//! The two types have the same value. The type of the expression tells if the value is a `cidr`: the output of a `cidr` always has the mask length, and the input and the receive function of a `cidr` refuse a value with bits set to the right of the mask.

use std::cmp::Ordering;

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;

/// The family of an address. IPv4 sorts before IPv6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetFamily {
    V4,
    V6,
}

impl NetFamily {
    /// The bits of an address, 32 or 128.
    pub fn max_bits(self) -> u8 {
        match self {
            NetFamily::V4 => 32,
            NetFamily::V6 => 128,
        }
    }

    /// The bytes of an address, 4 or 16.
    pub fn size(self) -> usize {
        match self {
            NetFamily::V4 => 4,
            NetFamily::V6 => 16,
        }
    }

    /// `PGSQL_AF_INET` or `PGSQL_AF_INET6`, the first byte of the binary form.
    fn code(self) -> u8 {
        match self {
            NetFamily::V4 => 2,
            NetFamily::V6 => 3,
        }
    }
}

/// A value of `inet` or `cidr`. The bytes of `addr` after the size of the family are 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Inet {
    pub family: NetFamily,
    /// The length of the network part, up to [`NetFamily::max_bits`].
    pub bits: u8,
    /// The address in network byte order.
    pub addr: [u8; 16],
}

impl Inet {
    /// The bytes of the address.
    pub fn bytes(&self) -> &[u8] {
        &self.addr[..self.family.size()]
    }

    /// `cidr_set_masklen_internal`: the value with the mask length `bits` and the bits to the right of the mask set to 0. `bits` must be valid for the family.
    #[must_use]
    pub fn masked(&self, bits: u8) -> Inet {
        let mut addr = [0u8; 16];
        let whole = usize::from(bits / 8);
        addr[..whole].copy_from_slice(&self.addr[..whole]);
        if !bits.is_multiple_of(8) {
            addr[whole] = self.addr[whole] & !(0xff >> (bits % 8));
        }
        Inet { family: self.family, bits, addr }
    }
}

/// The name of the type in the errors.
fn type_name(cidr: bool) -> &'static str {
    if cidr { "cidr" } else { "inet" }
}

/// The text input of `inet` and `cidr`. A value with a colon is IPv6. The input of `inet` takes an IPv4 address with no mask length only when it has the four octets, and the input of `cidr` takes fewer octets and finds the mask length from the class of the address.
///
/// # Errors
///
/// `22P02` for a value that is not an address, and for a `cidr` value with bits set to the right of the mask.
pub fn inet_in(text: &str, cidr: bool) -> Result<Inet, TypeError> {
    // The parsers read a C string: a 0 byte ends it.
    let mut src = text.as_bytes().to_vec();
    src.push(0);
    let family = if text.contains(':') { NetFamily::V6 } else { NetFamily::V4 };
    let mut addr = [0u8; 16];
    let bits = match (family, cidr) {
        (NetFamily::V4, false) => net_pton_v4(&src, &mut addr),
        (NetFamily::V4, true) => cidr_pton_v4(&src, &mut addr),
        (NetFamily::V6, _) => pton_v6(&src, &mut addr),
    };
    let Some(bits) = bits.and_then(|b| u8::try_from(b).ok()).filter(|b| *b <= family.max_bits())
    else {
        return Err(TypeError::syntax(type_name(cidr), text));
    };
    if cidr && !address_ok(&addr, bits, family) {
        let mut error = TypeError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid cidr value: \"{text}\""),
        );
        error.detail = Some("Value has bits set to right of mask.".into());
        return Err(error);
    }
    Ok(Inet { family, bits, addr })
}

/// The value of an ASCII digit.
fn digit(ch: u8) -> Option<i32> {
    ch.is_ascii_digit().then(|| i32::from(ch - b'0'))
}

/// The value of an ASCII hex digit in either case.
fn xdigit(ch: u8) -> Option<u32> {
    char::from(ch).to_digit(16)
}

/// Reads the digits of a mask length from `src[i]` up to the first byte that is not a digit, and gives the length and the index after that byte. A length above 32 stays above 32.
fn mask_length(src: &[u8], mut i: usize) -> (i32, u8, usize) {
    let mut bits = 0;
    loop {
        let ch = src[i];
        i += 1;
        match digit(ch) {
            Some(n) => bits = (bits * 10 + n).min(1000),
            None => return (bits, ch, i),
        }
    }
}

/// `inet_cidr_pton_ipv4`: the input of an IPv4 `cidr`. It takes hex octets after `0x`, or 1 to 4 decimal octets, and an optional mask length. With no mask length, the class of the address gives it.
fn cidr_pton_v4(src: &[u8], dst: &mut [u8; 16]) -> Option<i32> {
    let mut written = 0usize;
    let mut i = 0usize;
    let mut ch = src[i];
    i += 1;
    if ch == b'0' && (src[i] == b'x' || src[i] == b'X') && src[i + 1].is_ascii_hexdigit() {
        let mut dirty = 0;
        let mut tmp = 0u32;
        i += 1;
        loop {
            ch = src[i];
            i += 1;
            let Some(n) = xdigit(ch) else { break };
            tmp = if dirty == 0 { n } else { (tmp << 4) | n };
            dirty += 1;
            if dirty == 2 {
                if written == 4 {
                    return None;
                }
                dst[written] = u8::try_from(tmp).ok()?;
                written += 1;
                dirty = 0;
            }
        }
        if dirty != 0 {
            if written == 4 {
                return None;
            }
            dst[written] = u8::try_from(tmp << 4).ok()?;
            written += 1;
        }
    } else if ch.is_ascii_digit() {
        loop {
            let mut tmp = 0;
            while let Some(n) = digit(ch) {
                tmp = tmp * 10 + n;
                if tmp > 255 {
                    return None;
                }
                ch = src[i];
                i += 1;
            }
            if written == 4 {
                return None;
            }
            dst[written] = u8::try_from(tmp).ok()?;
            written += 1;
            if ch == 0 || ch == b'/' {
                break;
            }
            if ch != b'.' {
                return None;
            }
            ch = src[i];
            i += 1;
            if !ch.is_ascii_digit() {
                return None;
            }
        }
    } else {
        return None;
    }
    let mut bits = -1;
    if ch == b'/' && src[i].is_ascii_digit() && written > 0 {
        let (n, end, _) = mask_length(src, i);
        if end != 0 || n > 32 {
            return None;
        }
        bits = n;
        ch = end;
    }
    if ch != 0 || written == 0 {
        return None;
    }
    let octets = i32::try_from(written).ok()? * 8;
    if bits == -1 {
        bits = match dst[0] {
            240.. => 32,
            224.. => 8,
            192.. => 24,
            128.. => 16,
            _ => 8,
        };
        bits = bits.max(octets);
        if bits == 8 && dst[0] == 224 {
            bits = 4;
        }
    }
    // The bytes after the octets are 0 already.
    Some(bits)
}

/// `inet_net_pton_ipv4`: the input of an IPv4 `inet`, 1 to 4 decimal octets and an optional mask length. With no mask length, the address must have the four octets and the length is 32.
fn net_pton_v4(src: &[u8], dst: &mut [u8; 16]) -> Option<i32> {
    let mut written = 0usize;
    let mut i = 0usize;
    let mut ch;
    loop {
        ch = src[i];
        i += 1;
        if !ch.is_ascii_digit() {
            break;
        }
        let mut tmp = 0;
        while let Some(n) = digit(ch) {
            tmp = tmp * 10 + n;
            if tmp > 255 {
                return None;
            }
            ch = src[i];
            i += 1;
        }
        if written == 4 {
            return None;
        }
        dst[written] = u8::try_from(tmp).ok()?;
        written += 1;
        if ch == 0 || ch == b'/' {
            break;
        }
        if ch != b'.' {
            return None;
        }
    }
    let mut bits = -1;
    if ch == b'/' && src[i].is_ascii_digit() && written > 0 {
        let (n, end, _) = mask_length(src, i);
        if end != 0 || n > 32 {
            return None;
        }
        bits = n;
        ch = end;
    }
    if ch != 0 {
        return None;
    }
    if bits == -1 {
        if written != 4 {
            return None;
        }
        bits = 32;
    }
    if written == 0 || usize::try_from(bits / 8).ok()? > written {
        return None;
    }
    Some(bits)
}

/// `getbits`: a mask length from 0 to 128 with no leading zero, up to the end of the string.
fn getbits(src: &[u8]) -> Option<i32> {
    let mut val = 0;
    let mut n = 0;
    for &ch in src.iter().take_while(|ch| **ch != 0) {
        let d = digit(ch)?;
        if n != 0 && val == 0 {
            return None;
        }
        n += 1;
        val = val * 10 + d;
        if val > 128 {
            return None;
        }
    }
    (n != 0).then_some(val)
}

/// `getv4`: the IPv4 part at the end of an IPv6 address, with an optional mask length. It writes up to 4 octets.
fn getv4(src: &[u8], dst: &mut [u8], bits: &mut i32) -> bool {
    let mut written = 0usize;
    let mut val = 0;
    let mut n = 0;
    for (i, &ch) in src.iter().enumerate() {
        if ch == 0 {
            break;
        }
        if let Some(d) = digit(ch) {
            if n != 0 && val == 0 {
                return false;
            }
            n += 1;
            val = val * 10 + d;
            if val > 255 {
                return false;
            }
            continue;
        }
        if ch == b'.' || ch == b'/' {
            if written > 3 {
                return false;
            }
            dst[written] = u8::try_from(val).unwrap_or(0);
            written += 1;
            if ch == b'/' {
                return match getbits(&src[i + 1..]) {
                    Some(b) => {
                        *bits = b;
                        true
                    }
                    None => false,
                };
            }
            val = 0;
            n = 0;
            continue;
        }
        return false;
    }
    if n == 0 || written > 3 {
        return false;
    }
    dst[written] = u8::try_from(val).unwrap_or(0);
    true
}

/// `inet_cidr_pton_ipv6`: an IPv6 address of up to 8 groups of hex digits, with `::` for a run of zero groups, an optional IPv4 part at the end and an optional mask length. With no mask length, the length is 128.
fn pton_v6(src: &[u8], dst: &mut [u8; 16]) -> Option<i32> {
    const END: usize = 16;
    let mut tmp = [0u8; 16];
    let mut tp = 0usize;
    let mut colon: Option<usize> = None;
    let mut i = 0usize;
    if src[0] == b':' {
        i = 1;
        if src[1] != b':' {
            return None;
        }
    }
    let mut curtok = i;
    let mut saw_xdigit = false;
    let mut val = 0u32;
    let mut digits = 0;
    let mut bits = -1;
    loop {
        let ch = src[i];
        i += 1;
        if ch == 0 {
            break;
        }
        if let Some(n) = xdigit(ch) {
            val = (val << 4) | n;
            digits += 1;
            if digits > 4 {
                return None;
            }
            saw_xdigit = true;
            continue;
        }
        if ch == b':' {
            curtok = i;
            if !saw_xdigit {
                if colon.is_some() {
                    return None;
                }
                colon = Some(tp);
                continue;
            } else if src[i] == 0 || tp + 2 > END {
                return None;
            }
            tmp[tp..tp + 2].copy_from_slice(&u16::try_from(val).ok()?.to_be_bytes());
            tp += 2;
            saw_xdigit = false;
            digits = 0;
            val = 0;
            continue;
        }
        if ch == b'.' && tp + 4 <= END && getv4(&src[curtok..], &mut tmp[tp..tp + 4], &mut bits) {
            tp += 4;
            saw_xdigit = false;
            break;
        }
        if ch == b'/'
            && let Some(b) = getbits(&src[i..])
        {
            bits = b;
            break;
        }
        return None;
    }
    if saw_xdigit {
        if tp + 2 > END {
            return None;
        }
        tmp[tp..tp + 2].copy_from_slice(&u16::try_from(val).ok()?.to_be_bytes());
        tp += 2;
    }
    if bits == -1 {
        bits = 128;
    }
    if let Some(colon) = colon {
        if tp == END {
            return None;
        }
        let n = tp - colon;
        for k in 1..=n {
            tmp[END - k] = tmp[colon + n - k];
            tmp[colon + n - k] = 0;
        }
        tp = END;
    }
    if tp != END {
        return None;
    }
    *dst = tmp;
    Some(bits)
}

/// `addressOK`: true when no bit to the right of the mask is set.
fn address_ok(addr: &[u8; 16], bits: u8, family: NetFamily) -> bool {
    if bits == family.max_bits() {
        return true;
    }
    let mut byte = usize::from(bits / 8);
    let mut mask = if bits == 0 { 0xff } else { 0xffu8 >> (bits % 8) };
    while byte < family.size() {
        if addr[byte] & mask != 0 {
            return false;
        }
        mask = 0xff;
        byte += 1;
    }
    true
}

/// `pg_inet_net_ntop`: the address, then `/bits` when `bits` is not the length of the whole address. IPv4 always has the four octets.
pub fn net_ntop(value: &Inet, bits: u8, out: &mut String) {
    use std::fmt::Write;
    match value.family {
        NetFamily::V4 => {
            let [a, b, c, d, ..] = value.addr;
            let _ = write!(out, "{a}.{b}.{c}.{d}");
            if bits != 32 {
                let _ = write!(out, "/{bits}");
            }
        }
        NetFamily::V6 => {
            ntop_v6(&value.addr, out);
            if bits != 128 {
                let _ = write!(out, "/{bits}");
            }
        }
    }
}

/// `inet_net_ntop_ipv6` with no mask length: the groups in hex, the longest run of two or more zero groups as `::`, and the last 4 bytes as an IPv4 address when the address is a mapped or compatible IPv4 address.
fn ntop_v6(src: &[u8; 16], out: &mut String) {
    use std::fmt::Write;
    let mut words = [0u32; 8];
    for (i, byte) in src.iter().enumerate() {
        words[i / 2] |= u32::from(*byte) << ((1 - (i % 2)) * 8);
    }
    // The start and the length of the longest run of zero words.
    let mut best: Option<(usize, usize)> = None;
    let mut cur: Option<(usize, usize)> = None;
    for (i, word) in words.iter().enumerate() {
        if *word == 0 {
            cur = Some(cur.map_or((i, 1), |(base, len)| (base, len + 1)));
        } else if let Some(run) = cur.take()
            && best.is_none_or(|b| run.1 > b.1)
        {
            best = Some(run);
        }
    }
    if let Some(run) = cur
        && best.is_none_or(|b| run.1 > b.1)
    {
        best = Some(run);
    }
    if best.is_some_and(|b| b.1 < 2) {
        best = None;
    }
    for (i, word) in words.iter().enumerate() {
        if let Some((base, len)) = best
            && i >= base
            && i < base + len
        {
            if i == base {
                out.push(':');
            }
            continue;
        }
        if i != 0 {
            out.push(':');
        }
        if i == 6
            && let Some((0, len)) = best
            && (len == 6 || (len == 7 && words[7] != 1) || (len == 5 && words[5] == 0xffff))
        {
            let _ = write!(out, "{}.{}.{}.{}", src[12], src[13], src[14], src[15]);
            break;
        }
        let _ = write!(out, "{word:x}");
    }
    if best.is_some_and(|(base, len)| base + len == 8) {
        out.push(':');
    }
}

/// `inet_out` and `cidr_out`: the text of `net_ntop`, and for a `cidr` the mask length when the text has none.
pub fn inet_out(value: &Inet, cidr: bool, out: &mut Vec<u8>) {
    let mut text = String::new();
    net_ntop(value, value.bits, &mut text);
    if cidr && !text.contains('/') {
        text.push('/');
        text.push_str(&value.bits.to_string());
    }
    out.extend_from_slice(text.as_bytes());
}

/// `pg_inet_cidr_ntop`, the text of `abbrev(cidr)`: only the octets or the groups of the network part, then the mask length.
pub fn cidr_abbrev(value: &Inet, text: &mut String) {
    use std::fmt::Write;
    let bits = value.bits;
    let mut out = String::new();
    match value.family {
        NetFamily::V4 => {
            if bits == 0 {
                out.push('0');
            }
            let whole = usize::from(bits / 8);
            for (n, octet) in value.addr[..whole].iter().enumerate() {
                let _ = write!(out, "{octet}");
                if n + 1 < whole {
                    out.push('.');
                }
            }
            let b = bits % 8;
            if b > 0 {
                if !out.is_empty() {
                    out.push('.');
                }
                let m = 0xffu8 << (8 - b);
                let _ = write!(out, "{}", value.addr[whole] & m);
            }
        }
        NetFamily::V6 => cidr_ntop_v6(&value.masked(bits).addr, bits, &mut out),
    }
    let _ = write!(text, "{out}/{bits}");
}

/// `inet_cidr_ntop_ipv6` with no mask length, into an empty `out`. `s` has the bits to the right of the mask set to 0.
fn cidr_ntop_v6(s: &[u8; 16], bits: u8, out: &mut String) {
    use std::fmt::Write;
    if bits == 0 {
        out.push_str("::");
        return;
    }
    let words = usize::from(bits).div_ceil(16).max(2);
    // The longest run of zero words, as the loop of PostgreSQL finds it.
    let (mut zero_s, mut zero_l, mut tmp_s, mut tmp_l) = (0, 0, 0, 0);
    for i in 0..words {
        if s[2 * i] | s[2 * i + 1] == 0 {
            if tmp_l == 0 {
                tmp_s = i;
            }
            tmp_l += 1;
        } else if tmp_l != 0 && zero_l < tmp_l {
            zero_s = tmp_s;
            zero_l = tmp_l;
            tmp_l = 0;
        }
    }
    if tmp_l != 0 && zero_l < tmp_l {
        zero_s = tmp_s;
        zero_l = tmp_l;
    }
    let ipv4 = zero_l != words
        && zero_s == 0
        && (zero_l == 6
            || (zero_l == 5 && s[10] == 0xff && s[11] == 0xff)
            || (zero_l == 7 && s[14] != 0 && s[15] != 1));
    for p in 0..words {
        if zero_l != 0 && p >= zero_s && p < zero_s + zero_l {
            if p == zero_s {
                out.push(':');
            }
            if p == words - 1 {
                out.push(':');
            }
            continue;
        }
        if ipv4 && p > 5 {
            out.push(if p == 6 { ':' } else { '.' });
            let _ = write!(out, "{}", s[2 * p]);
            if p != 7 || bits > 120 {
                let _ = write!(out, ".{}", s[2 * p + 1]);
            }
        } else {
            if !out.is_empty() {
                out.push(':');
            }
            let _ = write!(out, "{:x}", u32::from(s[2 * p]) * 256 + u32::from(s[2 * p + 1]));
        }
    }
}

/// The error of `network_recv`.
fn recv_error(message: String) -> TypeError {
    TypeError::new(SqlState::INVALID_BINARY_REPRESENTATION, message)
}

/// `inet_recv` and `cidr_recv`: the family, the mask length, a flag of `cidr` that the function does not read, the length of the address and the address.
///
/// # Errors
///
/// `22P03` for a bad family, mask length or address length, and for a `cidr` value with bits set to the right of the mask.
pub fn inet_recv(recv: &mut Recv<'_>, cidr: bool) -> Result<Inet, TypeError> {
    let name = type_name(cidr);
    let family = match recv.byte()? {
        2 => NetFamily::V4,
        3 => NetFamily::V6,
        _ => {
            return Err(recv_error(format!("invalid address family in external \"{name}\" value")));
        }
    };
    let bits = recv.byte()?;
    if bits > family.max_bits() {
        return Err(recv_error(format!("invalid bits in external \"{name}\" value")));
    }
    recv.byte()?;
    let size = recv.byte()?;
    if usize::from(size) != family.size() {
        return Err(recv_error(format!("invalid length in external \"{name}\" value")));
    }
    let mut addr = [0u8; 16];
    addr[..family.size()].copy_from_slice(recv.bytes(family.size())?);
    if cidr && !address_ok(&addr, bits, family) {
        let mut error = recv_error("invalid external \"cidr\" value".into());
        error.detail = Some("Value has bits set to right of mask.".into());
        return Err(error);
    }
    Ok(Inet { family, bits, addr })
}

/// `inet_send` and `cidr_send`.
pub fn inet_send(value: &Inet, cidr: bool, out: &mut Vec<u8>) {
    let size = value.family.size();
    out.extend_from_slice(&[value.family.code(), value.bits, u8::from(cidr)]);
    out.push(u8::try_from(size).unwrap_or(16));
    out.extend_from_slice(value.bytes());
}

/// `bitncmp`: the order of the first `n` bits of two addresses. PostgreSQL compares the whole bytes with `memcmp`, which gives the difference of the first bytes that differ on the reference server, and then the bits of the last byte, which give 1 or -1.
pub fn bitncmp(l: &[u8; 16], r: &[u8; 16], n: u8) -> i32 {
    let whole = usize::from(n / 8);
    if let Some((a, b)) = l[..whole].iter().zip(&r[..whole]).find(|(a, b)| a != b) {
        return i32::from(*a) - i32::from(*b);
    }
    if n.is_multiple_of(8) {
        return 0;
    }
    let mask = 0xffu8 << (8 - n % 8);
    match (l[whole] & mask).cmp(&(r[whole] & mask)) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// `bitncommon`: the number of leading bits, up to `n`, that two addresses have in common.
pub fn bitncommon(l: &[u8; 16], r: &[u8; 16], n: u8) -> u8 {
    let mut nbits = n % 8;
    let mut byte = 0usize;
    while byte < usize::from(n / 8) {
        if l[byte] != r[byte] {
            nbits = 7;
            break;
        }
        byte += 1;
    }
    if nbits != 0 {
        let diff = u32::from(l[byte] ^ r[byte]);
        while diff >> (8 - nbits) != 0 {
            nbits -= 1;
        }
    }
    u8::try_from(byte * 8).unwrap_or(128) + nbits
}

/// `network_cmp_internal`, the result of `network_cmp`: the common bits of the network parts, then the mask lengths, then the whole addresses. IPv4 comes before IPv6. Only the sign of the result is the order.
pub fn network_cmp(a: &Inet, b: &Inet) -> i32 {
    if a.family != b.family {
        return i32::from(a.family.code()) - i32::from(b.family.code());
    }
    let order = bitncmp(&a.addr, &b.addr, a.bits.min(b.bits));
    if order != 0 {
        return order;
    }
    let order = i32::from(a.bits) - i32::from(b.bits);
    if order != 0 {
        return order;
    }
    bitncmp(&a.addr, &b.addr, a.family.max_bits())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &str, cidr: bool) -> String {
        let mut out = Vec::new();
        inet_out(&inet_in(input, cidr).unwrap(), cidr, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn inet_keeps_the_host_bits_and_omits_a_full_mask() {
        assert_eq!(text("192.168.1.5", false), "192.168.1.5");
        assert_eq!(text("192.168.1.5/24", false), "192.168.1.5/24");
        assert_eq!(text("::ffff:1.2.3.4", false), "::ffff:1.2.3.4");
        assert_eq!(text("2001:db8:0:0:1:0:0:1", false), "2001:db8::1:0:0:1");
        assert_eq!(text("::1", false), "::1");
        assert_eq!(text("1::/64", false), "1::/64");
    }

    #[test]
    fn cidr_finds_the_mask_from_the_class() {
        assert_eq!(text("10", true), "10.0.0.0/8");
        assert_eq!(text("172.16", true), "172.16.0.0/16");
        assert_eq!(text("192.168.1", true), "192.168.1.0/24");
        assert_eq!(text("224", true), "224.0.0.0/4");
        assert_eq!(text("224.0.0.0", true), "224.0.0.0/32");
        assert_eq!(text("0x0a", true), "10.0.0.0/8");
    }

    #[test]
    fn bad_values_have_the_errors_of_postgres() {
        assert_eq!(
            inet_in("1.2.3", false).unwrap_err().message,
            "invalid input syntax for type inet: \"1.2.3\""
        );
        assert_eq!(
            inet_in("1.2.3.4/33", false).unwrap_err().message,
            "invalid input syntax for type inet: \"1.2.3.4/33\""
        );
        let error = inet_in("192.168.1.5/24", true).unwrap_err();
        assert_eq!(error.message, "invalid cidr value: \"192.168.1.5/24\"");
        assert_eq!(error.detail.as_deref(), Some("Value has bits set to right of mask."));
    }

    #[test]
    fn abbrev_of_cidr_drops_the_zero_octets() {
        let mut out = String::new();
        cidr_abbrev(&inet_in("10.1.0.0/16", true).unwrap(), &mut out);
        assert_eq!(out, "10.1/16");
    }

    #[test]
    fn the_binary_form_reads_back() {
        let value = inet_in("2001:db8::/32", true).unwrap();
        let mut out = Vec::new();
        inet_send(&value, true, &mut out);
        assert_eq!(&out[..4], &[3, 32, 1, 16]);
        let mut recv = Recv::new(&out);
        assert_eq!(inet_recv(&mut recv, true), Ok(value));
    }

    #[test]
    fn the_network_part_sorts_first() {
        let a = inet_in("10.0.0.0/8", false).unwrap();
        let b = inet_in("10.0.0.1/32", false).unwrap();
        let c = inet_in("::1", false).unwrap();
        assert_eq!(network_cmp(&a, &b), -24);
        assert_eq!(network_cmp(&b, &c), -1);
        let d = inet_in("10.0.0.9", false).unwrap();
        assert_eq!(network_cmp(&d, &b), 8);
        assert_eq!(bitncommon(&a.addr, &b.addr, 8), 8);
    }
}
