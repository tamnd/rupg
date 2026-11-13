// Lifted from rudb crates/rudb-qc-ir/src/eval.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! What each pure opcode computes, on constant bits.
//!
//! The builder folds with this, and the interpreter runs with it, so the two cannot disagree
//! about what `ashr i8 -128, 9` is. A value is its bits in a `u128`, masked to its type's width.
//! Floats are their IEEE bits. Section 6.5 of `spec/compiler/06-qir.md` gives the semantics this
//! implements, and where it is silent the choice is written next to the code.

use crate::ir::{Op, Ty};

/// Sign-extends a value of type `ty` to an `i128`.
#[must_use]
pub fn sext(ty: Ty, bits: u128) -> i128 {
    let width = ty.bits();
    if width == 0 || width >= 128 {
        return bits as i128;
    }
    let shift = 128 - width;
    ((bits << shift) as i128) >> shift
}

/// Masks `bits` to `ty`'s width.
#[must_use]
pub fn mask(ty: Ty, bits: u128) -> u128 {
    bits & ty.mask()
}

fn f32of(bits: u128) -> f32 {
    f32::from_bits(bits as u32)
}

fn f64of(bits: u128) -> f64 {
    f64::from_bits(bits as u64)
}

/// A float of type `ty` as an `f64`, for the total order.
fn float(ty: Ty, bits: u128) -> f64 {
    if ty == Ty::F32 { f64::from(f32of(bits)) } else { f64of(bits) }
}

/// The sign bit of a float of type `ty`.
fn sign(ty: Ty) -> u128 {
    if ty == Ty::F32 { 1 << 31 } else { 1 << 63 }
}

/// The bits of a float of type `ty`.
fn unfloat(ty: Ty, x: f64) -> u128 {
    if ty == Ty::F32 { u128::from((x as f32).to_bits()) } else { u128::from(x.to_bits()) }
}

/// DuckDB's total order on floats: NaN equals itself and sorts above every number, and `-0`
/// equals `0`.
fn total_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// `10^k` for `k` up to 38.
#[must_use]
pub fn pow10(k: u32) -> i128 {
    10i128.pow(k)
}

/// One step of CRC-32C over a 64-bit word, the same as SSE4.2's `crc32 r64, r64` and Arm's
/// `crc32cx`. The result is the 32-bit CRC zero-extended.
#[must_use]
pub fn crc32c(seed: u64, word: u64) -> u64 {
    let mut crc = seed as u32;
    for byte in word.to_le_bytes() {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
    }
    u64::from(crc)
}

/// Why an instruction did not produce a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The trap form of checked arithmetic fired: overflow, a zero divisor, a conversion out of
    /// range. The instruction's error site says which.
    Trap,
    /// The opcode is not pure, or the operands are not constants of the types it takes, so there
    /// is nothing to compute here.
    NotPure,
}

/// A unary opcode on a value of type `ty` whose result has type `to`. `to` differs from `ty`
/// for conversions and the string header operations.
///
/// # Errors
///
/// [`Fault::Trap`] when a trapping conversion is out of range, [`Fault::NotPure`] for an opcode
/// this does not compute.
pub fn unary(op: Op, ty: Ty, to: Ty, a: u128) -> Result<u128, Fault> {
    let w = ty.bits();
    let r = match op {
        Op::Neg => a.wrapping_neg(),
        Op::Not => !a,
        Op::Clz => u128::from(a.leading_zeros() - (128 - w)),
        Op::Ctz => u128::from(a.trailing_zeros().min(w)),
        Op::Popcnt => u128::from(a.count_ones()),
        Op::Bswap => a.swap_bytes() >> (128 - w),
        Op::SnegT => {
            let x = sext(ty, a);
            if x == sext(ty, 1 << (w - 1)) {
                return Err(Fault::Trap);
            }
            x.wrapping_neg() as u128
        }
        Op::Sext => sext(ty, a) as u128,
        Op::Zext | Op::Trunc => a,
        Op::Sitof => unfloat(to, sext(ty, a) as f64),
        Op::Uitof => unfloat(to, a as f64),
        Op::FtosiT => {
            // DuckDB casts a double to an integer with `nearbyint`, which rounds half to even.
            let x = float(ty, a).round_ties_even();
            let lo = -(2f64.powi(to.bits() as i32 - 1));
            if x.is_nan() || x < lo || x >= -lo {
                return Err(Fault::Trap);
            }
            (x as i128) as u128
        }
        Op::Fext => unfloat(to, f64::from(f32of(a))),
        Op::Ftrunc => unfloat(to, f64of(a)),
        Op::Bitcast => a,
        // Negation and the absolute value change the sign bit and nothing else, as IEEE 754 says, so a NaN keeps its payload and stays signaling. Through `f64` an `f32` NaN would come back quiet in a debug build and not in a release build.
        Op::Fneg => a ^ sign(ty),
        Op::Fabs => a & !sign(ty),
        Op::Fsqrt => {
            if ty == Ty::F32 {
                u128::from(f32of(a).sqrt().to_bits())
            } else {
                u128::from(f64of(a).sqrt().to_bits())
            }
        }
        Op::StrLen => a & 0xffff_ffff,
        Op::StrW0 => a & u128::from(u64::MAX),
        Op::StrW1 | Op::StrPtr => a >> 64,
        Op::StrInl => u128::from(a & 0xffff_ffff <= 12),
        _ => return Err(Fault::NotPure),
    };
    Ok(r & to.mask())
}

/// A binary opcode on two values of type `ty`. Comparisons return an `i1`, the widening
/// multiplies an `i128`.
///
/// Shift and rotate amounts are taken modulo the width, which is what x86-64 does for 32 and 64
/// bit operands and what every backend has to reproduce for the narrow ones.
///
/// # Errors
///
/// [`Fault::Trap`] when checked arithmetic overflows or divides by zero, [`Fault::NotPure`] for
/// an opcode this does not compute.
pub fn binary(op: Op, ty: Ty, a: u128, b: u128) -> Result<u128, Fault> {
    let w = ty.bits();
    let m = ty.mask();
    let (sa, sb) = (sext(ty, a), sext(ty, b));
    let signed_fits = |x: i128| sext(ty, x as u128 & m) == x;
    let amount = if w == 0 { 0 } else { (b as u32) % w };
    let r = match op {
        Op::Add => a.wrapping_add(b),
        Op::Sub => a.wrapping_sub(b),
        Op::Mul => a.wrapping_mul(b),
        Op::And => a & b,
        Op::Or => a | b,
        Op::Xor => a ^ b,
        Op::Shl => a << amount,
        Op::Lshr => a >> amount,
        Op::Ashr => (sa >> amount) as u128,
        Op::Rotl => {
            if amount == 0 {
                a
            } else {
                (a << amount) | (a >> (w - amount))
            }
        }
        Op::Rotr => {
            if amount == 0 {
                a
            } else {
                (a >> amount) | (a << (w - amount))
            }
        }
        Op::SaddT | Op::SaddOv => checked(sa.checked_add(sb), signed_fits)?,
        Op::SsubT | Op::SsubOv => checked(sa.checked_sub(sb), signed_fits)?,
        Op::SmulT | Op::SmulOv => checked(sa.checked_mul(sb), signed_fits)?,
        Op::UaddT => unsigned(a.checked_add(b), m)?,
        Op::UsubT => unsigned(a.checked_sub(b), m)?,
        Op::UmulT => unsigned(a.checked_mul(b), m)?,
        Op::SdivT => {
            if sb == 0 {
                return Err(Fault::Trap);
            }
            checked(sa.checked_div(sb), signed_fits)?
        }
        Op::SremT => {
            if sb == 0 {
                return Err(Fault::Trap);
            }
            // `MIN % -1` is 0 and not an overflow, as in DuckDB.
            if sb == -1 { 0 } else { sa.wrapping_rem(sb) as u128 }
        }
        Op::UdivT => a.checked_div(b).ok_or(Fault::Trap)?,
        Op::UremT => a.checked_rem(b).ok_or(Fault::Trap)?,
        Op::Smulw => return Ok(sa.wrapping_mul(sb) as u128),
        Op::Umulw => return Ok(a.wrapping_mul(b)),
        Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv if ty == Ty::F32 => {
            let (x, y) = (f32of(a), f32of(b));
            let z = match op {
                Op::Fadd => x + y,
                Op::Fsub => x - y,
                Op::Fmul => x * y,
                _ => x / y,
            };
            u128::from(z.to_bits())
        }
        Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv => {
            let (x, y) = (f64of(a), f64of(b));
            let z = match op {
                Op::Fadd => x + y,
                Op::Fsub => x - y,
                Op::Fmul => x * y,
                _ => x / y,
            };
            u128::from(z.to_bits())
        }
        Op::FminTot | Op::FmaxTot => {
            let less = total_cmp(float(ty, a), float(ty, b)).is_lt();
            if less == (op == Op::FminTot) { a } else { b }
        }
        Op::IcmpEq => return Ok(u128::from(a == b)),
        Op::IcmpNe => return Ok(u128::from(a != b)),
        Op::IcmpSlt => return Ok(u128::from(sa < sb)),
        Op::IcmpSle => return Ok(u128::from(sa <= sb)),
        Op::IcmpSgt => return Ok(u128::from(sa > sb)),
        Op::IcmpSge => return Ok(u128::from(sa >= sb)),
        Op::IcmpUlt => return Ok(u128::from(a < b)),
        Op::IcmpUle => return Ok(u128::from(a <= b)),
        Op::IcmpUgt => return Ok(u128::from(a > b)),
        Op::IcmpUge => return Ok(u128::from(a >= b)),
        Op::FcmpEq => return Ok(u128::from(total_cmp(float(ty, a), float(ty, b)).is_eq())),
        Op::FcmpLt => return Ok(u128::from(total_cmp(float(ty, a), float(ty, b)).is_lt())),
        Op::FcmpLe => return Ok(u128::from(total_cmp(float(ty, a), float(ty, b)).is_le())),
        Op::Crc32c => u128::from(crc32c(a as u64, b as u64)),
        Op::StrMk => return Ok((a & u128::from(u64::MAX)) | (b << 64)),
        _ => return Err(Fault::NotPure),
    };
    Ok(r & m)
}

fn checked(r: Option<i128>, fits: impl Fn(i128) -> bool) -> Result<u128, Fault> {
    match r {
        Some(x) if fits(x) => Ok(x as u128),
        _ => Err(Fault::Trap),
    }
}

fn unsigned(r: Option<u128>, m: u128) -> Result<u128, Fault> {
    match r {
        Some(x) if x & !m == 0 => Ok(x),
        _ => Err(Fault::Trap),
    }
}

/// `dup.t` and `ddown`: multiply or divide a decimal of type `ty` by `10^k`.
///
/// # Errors
///
/// [`Fault::Trap`] when `dup.t` overflows `ty`.
pub fn scale(op: Op, ty: Ty, a: u128, k: u32) -> Result<u128, Fault> {
    let x = sext(ty, a);
    let p = pow10(k.min(38));
    let r = match op {
        Op::DupT => {
            let y = x.checked_mul(p).ok_or(Fault::Trap)?;
            if sext(ty, y as u128 & ty.mask()) != y {
                return Err(Fault::Trap);
            }
            y
        }
        Op::Ddown => {
            // Rounded half away from zero.
            let q = x / p;
            let r = x % p;
            if r.unsigned_abs() * 2 >= p.unsigned_abs() { q + x.signum() } else { q }
        }
        _ => return Err(Fault::NotPure),
    };
    Ok(r as u128 & ty.mask())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_wrapping_and_shifts() {
        assert_eq!(binary(Op::Add, Ty::I8, 200, 100).unwrap(), 44);
        assert_eq!(binary(Op::Ashr, Ty::I8, 0x80, 9).unwrap(), 0xc0);
        assert_eq!(binary(Op::Rotr, Ty::I64, 1, 32).unwrap(), 1 << 32);
        assert_eq!(unary(Op::Clz, Ty::I32, Ty::I32, 1).unwrap(), 31);
        assert_eq!(unary(Op::Bswap, Ty::I16, Ty::I16, 0x1234).unwrap(), 0x3412);
    }

    #[test]
    fn checked_arithmetic_traps_where_duckdb_raises() {
        assert_eq!(binary(Op::SaddT, Ty::I32, 0x7fff_ffff, 1), Err(Fault::Trap));
        assert_eq!(binary(Op::SaddT, Ty::I32, 0xffff_ffff, 1).unwrap(), 0);
        assert_eq!(binary(Op::SdivT, Ty::I32, 0x8000_0000, 0xffff_ffff), Err(Fault::Trap));
        assert_eq!(binary(Op::SremT, Ty::I32, 0x8000_0000, 0xffff_ffff).unwrap(), 0);
        assert_eq!(binary(Op::UdivT, Ty::I64, 1, 0), Err(Fault::Trap));
        assert_eq!(unary(Op::SnegT, Ty::I8, Ty::I8, 0x80), Err(Fault::Trap));
    }

    #[test]
    fn decimal_rescale_rounds_half_away_from_zero() {
        assert_eq!(scale(Op::Ddown, Ty::I64, 15, 1).unwrap(), 2);
        assert_eq!(
            scale(Op::Ddown, Ty::I64, (-15i64) as u64 as u128, 1).unwrap(),
            (-2i64) as u64 as u128
        );
        assert_eq!(scale(Op::Ddown, Ty::I64, 14, 1).unwrap(), 1);
        assert_eq!(scale(Op::DupT, Ty::I16, 4000, 1), Err(Fault::Trap));
    }

    #[test]
    fn float_total_order_puts_nan_on_top() {
        let nan = u128::from(f64::NAN.to_bits());
        let one = u128::from(1f64.to_bits());
        assert_eq!(binary(Op::FcmpEq, Ty::F64, nan, nan).unwrap(), 1);
        assert_eq!(binary(Op::FcmpLt, Ty::F64, one, nan).unwrap(), 1);
        assert_eq!(binary(Op::FmaxTot, Ty::F64, one, nan).unwrap(), nan);
        let neg0 = u128::from((-0f64).to_bits());
        assert_eq!(binary(Op::FcmpEq, Ty::F64, neg0, 0).unwrap(), 1);
    }

    #[test]
    fn float_to_int_rounds_half_to_even_and_traps_out_of_range() {
        let f = |x: f64| u128::from(x.to_bits());
        assert_eq!(unary(Op::FtosiT, Ty::F64, Ty::I32, f(2.5)).unwrap(), 2);
        assert_eq!(unary(Op::FtosiT, Ty::F64, Ty::I32, f(3.5)).unwrap(), 4);
        assert_eq!(unary(Op::FtosiT, Ty::F64, Ty::I32, f(3e9)), Err(Fault::Trap));
        assert_eq!(unary(Op::FtosiT, Ty::F64, Ty::I32, f(f64::NAN)), Err(Fault::Trap));
    }

    #[test]
    fn crc32c_matches_the_check_value() {
        // The standard check: CRC-32C of "123456789" is 0xe3069283. Eight bytes go through the
        // word step and the ninth through the same loop by hand.
        let word = u64::from_le_bytes(*b"12345678");
        let mut crc = crc32c(0xffff_ffff, word) as u32;
        crc ^= u32::from(b'9');
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
        assert_eq!(crc ^ 0xffff_ffff, 0xe306_9283);
    }
}
