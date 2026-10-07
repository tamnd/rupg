//! `numeric`: the value in the layout of PostgreSQL, its text and binary forms, and the typmod.
//!
//! [`Numeric`] holds what `NumericVar` holds in `src/backend/utils/adt/numeric.c`: a sign, a weight, a display scale and base-10000 digits. The binary format has the same fields, so the receive and send functions copy them. The input, the output, the rounding and the typmod check are ports of `numeric_in`, `get_str_from_var`, `round_var` and `apply_typmod`, with the same errors in the same order: a syntax error comes before a typmod error, and a typmod error comes before the check that the weight and the scale fit the format.
//!
//! A `numeric(p, s)` column with p up to 38 is a scaled integer in the engine. [`decimal_out`] and [`decimal_send`] write that integer as PostgreSQL writes the same value, with no allocation, and [`Numeric::from_decimal`] and [`Numeric::to_decimal`] convert between the two forms.
//!
//! Lifted from `crates/rudb-pgtypes/src/numeric.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::number::{digit, is_space, u64_out};
use crate::typmod::numeric_precision_scale;

const NBASE: i32 = 10000;
const HALF_NBASE: i32 = 5000;
const DEC_DIGITS: i64 = 4;
/// `NUMERIC_DSCALE_MAX`, the largest display scale that the format can hold.
const DSCALE_MAX: i64 = 0x3fff;
/// `NUMERIC_MAX_RESULT_SCALE`: the most digits that `round` and `trunc` keep after the point.
const NUMERIC_MAX_RESULT_SCALE: i32 = 2000;
/// `round_powers` in `numeric.c`.
const ROUND_POWERS: [i32; 4] = [0, 1000, 100, 10];
/// The most base-10000 digits that a scaled `i128` needs: 39 decimal digits, split at the point.
const DECIMAL_DIGITS_MAX: usize = 12;

/// The sign of a value, which also marks the special values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericSign {
    Positive,
    Negative,
    NaN,
    Infinity,
    NegativeInfinity,
}

impl NumericSign {
    /// `NUMERIC_POS`, `NUMERIC_NEG`, `NUMERIC_NAN`, `NUMERIC_PINF` and `NUMERIC_NINF`, the codes of the binary format.
    pub const fn code(self) -> u16 {
        match self {
            NumericSign::Positive => 0x0000,
            NumericSign::Negative => 0x4000,
            NumericSign::NaN => 0xc000,
            NumericSign::Infinity => 0xd000,
            NumericSign::NegativeInfinity => 0xf000,
        }
    }

    pub const fn from_code(code: u16) -> Option<NumericSign> {
        Some(match code {
            0x0000 => NumericSign::Positive,
            0x4000 => NumericSign::Negative,
            0xc000 => NumericSign::NaN,
            0xd000 => NumericSign::Infinity,
            0xf000 => NumericSign::NegativeInfinity,
            _ => return None,
        })
    }

    const fn is_finite(self) -> bool {
        matches!(self, NumericSign::Positive | NumericSign::Negative)
    }
}

/// A `numeric` value as PostgreSQL stores it. The digits have no zero at either end. A zero has no digits, a weight of 0 and a positive sign, and it keeps its display scale. A special value has only its sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Numeric {
    sign: NumericSign,
    weight: i16,
    dscale: u16,
    digits: Vec<i16>,
}

impl Numeric {
    pub const NAN: Numeric = Numeric::special(NumericSign::NaN);
    pub const INFINITY: Numeric = Numeric::special(NumericSign::Infinity);
    pub const NEGATIVE_INFINITY: Numeric = Numeric::special(NumericSign::NegativeInfinity);

    const fn special(sign: NumericSign) -> Numeric {
        Numeric { sign, weight: 0, dscale: 0, digits: Vec::new() }
    }

    pub fn sign(&self) -> NumericSign {
        self.sign
    }

    /// The power of 10000 of the first digit.
    pub fn weight(&self) -> i16 {
        self.weight
    }

    /// The number of decimal digits that the output shows after the point.
    pub fn dscale(&self) -> u16 {
        self.dscale
    }

    /// The base-10000 digits, the most significant first.
    pub fn digits(&self) -> &[i16] {
        &self.digits
    }

    /// The value `value / 10^scale`, with `scale` as its display scale, as a cast from a `numeric(p, s)` column of the engine gives it.
    pub fn from_decimal(value: i128, scale: u32) -> Numeric {
        let mut buf = [0; DECIMAL_DIGITS_MAX];
        let (digits, weight) = decimal_digits(value.unsigned_abs(), scale, &mut buf);
        let sign = if value < 0 { NumericSign::Negative } else { NumericSign::Positive };
        Numeric { sign, weight, dscale: scale as u16, digits: digits.to_vec() }
    }

    /// The value as an integer count of `10^-scale`, rounded half away from zero as `round_var` rounds. `None` for a special value or a value that does not fit in an `i128`.
    pub fn to_decimal(&self, scale: u32) -> Option<i128> {
        if !self.sign.is_finite() {
            return None;
        }
        let mut var = Var::from(self);
        var.round(i64::from(scale));
        // The power of ten of the last digit, in units of 10^-scale. Rounding can leave up to three decimal zeros below the scale in the last digit, and they are dropped before they can overflow the accumulator.
        let exp = (var.weight + 1 - var.digits.len() as i64) * DEC_DIGITS + i64::from(scale);
        let last = var.digits.len().saturating_sub(1);
        let mut acc: i128 = 0;
        for (k, &d) in var.digits.iter().enumerate() {
            let (mul, d) = match exp {
                -3..0 if k == last => (10i128.pow((4 + exp) as u32), d / 10i32.pow(-exp as u32)),
                _ => (i128::from(NBASE), d),
            };
            acc = acc.checked_mul(mul)?.checked_add(i128::from(d))?;
        }
        if exp > 0 && acc != 0 {
            acc = acc.checked_mul(10i128.checked_pow(u32::try_from(exp).ok()?)?)?;
        }
        Some(if var.sign == NumericSign::Negative { -acc } else { acc })
    }
}

impl Numeric {
    /// The value of bytes in the layout of [`crate::numeric_layout`], which is how a vector holds a value of the type `NUMERIC`.
    pub fn from_bytes(bytes: &[u8]) -> Numeric {
        let unpacked = crate::numeric_layout::decode(bytes);
        let sign = NumericSign::from_code(unpacked.sign).unwrap_or(NumericSign::NaN);
        if !sign.is_finite() {
            return Numeric::special(sign);
        }
        let sign = if unpacked.digits.is_empty() { NumericSign::Positive } else { sign };
        Numeric { sign, weight: unpacked.weight, dscale: unpacked.dscale, digits: unpacked.digits }
    }

    /// The bytes of the value in the layout of [`crate::numeric_layout`].
    pub fn to_bytes(&self) -> Vec<u8> {
        crate::numeric_layout::encode(self.sign.code(), self.weight, self.dscale, &self.digits)
    }

    /// The value of an integer, with a display scale of 0, as `int8_numeric` gives it.
    pub fn from_integer(value: i128) -> Numeric {
        Numeric::from_decimal(value, 0)
    }

    /// `float8_numeric`: the value of the double printed with 15 significant digits, so that `0.1::float8::numeric` is `0.1`.
    pub fn from_f64(value: f64) -> Numeric {
        if value.is_nan() {
            return Numeric::NAN;
        }
        if value.is_infinite() {
            return if value > 0.0 { Numeric::INFINITY } else { Numeric::NEGATIVE_INFINITY };
        }
        // `%.15g` with the trailing zeros dropped from the digits. The exponent form gives the same value and the same display scale as the plain form does.
        let text = format!("{value:.14e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        let mantissa = match mantissa.contains('.') {
            true => mantissa.trim_end_matches('0').trim_end_matches('.'),
            false => mantissa,
        };
        numeric_in(&format!("{mantissa}e{exponent}"), -1).unwrap_or(Numeric::NAN)
    }

    /// `numeric_float8`: the double nearest to the text of the value.
    pub fn to_f64(&self) -> f64 {
        match self.sign {
            NumericSign::NaN => f64::NAN,
            NumericSign::Infinity => f64::INFINITY,
            NumericSign::NegativeInfinity => f64::NEG_INFINITY,
            _ => {
                let mut out = Vec::new();
                numeric_out(self, &mut out);
                std::str::from_utf8(&out)
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(f64::NAN)
            }
        }
    }

    /// The value rounded half away from zero to an integer, as the casts to the integer types round. The error names the type, as `numeric_int4` and `numeric_int8` do.
    pub fn to_integer(&self, type_name: &str) -> Result<i128, TypeError> {
        let range = || {
            TypeError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("{type_name} out of range"),
            )
        };
        match self.sign {
            NumericSign::NaN => {
                return Err(TypeError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot convert NaN to {type_name}"),
                ));
            }
            NumericSign::Infinity | NumericSign::NegativeInfinity => {
                return Err(TypeError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot convert infinity to {type_name}"),
                ));
            }
            _ => {}
        }
        self.to_decimal(0).ok_or_else(range)
    }

    /// The value with `typmod` applied, as a cast to `numeric(p, s)` gives it.
    pub fn with_typmod(&self, typmod: i32) -> Result<Numeric, TypeError> {
        if !self.sign.is_finite() {
            apply_typmod_special(self.sign, typmod)?;
            return Ok(self.clone());
        }
        let mut var = Var::from(self);
        var.apply_typmod(typmod)?;
        var.make()
    }

    /// `numeric_round`: half away from zero to `scale` digits after the point, or before it when the scale is negative.
    pub fn round(&self, scale: i32) -> Result<Numeric, TypeError> {
        self.rescaled(scale, Var::round)
    }

    /// `numeric_trunc`: the digits after `scale` digits after the point dropped.
    pub fn trunc(&self, scale: i32) -> Result<Numeric, TypeError> {
        self.rescaled(scale, Var::trunc)
    }

    /// `numeric_round` and `numeric_trunc`, which differ in how they drop the digits.
    fn rescaled(&self, scale: i32, drop: fn(&mut Var, i64)) -> Result<Numeric, TypeError> {
        if !self.sign.is_finite() {
            return Ok(self.clone());
        }
        let scale = i64::from(scale.clamp(-NUMERIC_MAX_RESULT_SCALE, NUMERIC_MAX_RESULT_SCALE));
        let mut var = Var::from(self);
        drop(&mut var, scale);
        if scale < 0 {
            var.dscale = 0;
        }
        var.make()
    }

    /// `numeric_ceil` when `up` and `numeric_floor` when not: the nearest whole number on that side.
    pub fn whole(&self, up: bool) -> Result<Numeric, TypeError> {
        let truncated = self.trunc(0)?;
        let toward = if up { NumericSign::Positive } else { NumericSign::Negative };
        if self.sign != toward || truncated.compare(self).is_eq() {
            return Ok(truncated);
        }
        let one = Numeric::from_integer(if up { 1 } else { -1 });
        truncated.add(&one)
    }

    /// `numeric_abs`.
    pub fn abs(&self) -> Numeric {
        match self.sign {
            NumericSign::Negative => self.negate(),
            NumericSign::NegativeInfinity => self.negate(),
            _ => self.clone(),
        }
    }

    /// `numeric_uminus`.
    pub fn negate(&self) -> Numeric {
        let sign = match self.sign {
            NumericSign::Positive if !self.digits.is_empty() => NumericSign::Negative,
            NumericSign::Negative => NumericSign::Positive,
            NumericSign::Infinity => NumericSign::NegativeInfinity,
            NumericSign::NegativeInfinity => NumericSign::Infinity,
            sign => sign,
        };
        Numeric { sign, ..self.clone() }
    }

    /// `numeric_add`: the display scale is the larger of the two.
    pub fn add(&self, other: &Numeric) -> Result<Numeric, TypeError> {
        if let Some(special) = special_sum(self.sign, other.sign) {
            return Ok(special);
        }
        Var::from(self).add(&Var::from(other)).make()
    }

    /// `numeric_sub`.
    pub fn sub(&self, other: &Numeric) -> Result<Numeric, TypeError> {
        self.add(&other.negate())
    }

    /// `numeric_mul`: the display scale is the sum of the two.
    pub fn mul(&self, other: &Numeric) -> Result<Numeric, TypeError> {
        use NumericSign::{Infinity, NaN, Negative, NegativeInfinity};
        let (a, b) = (self.sign, other.sign);
        if !a.is_finite() || !b.is_finite() {
            if a == NaN || b == NaN {
                return Ok(Numeric::NAN);
            }
            let zero = (a.is_finite() && self.digits.is_empty())
                || (b.is_finite() && other.digits.is_empty());
            if zero {
                return Ok(Numeric::NAN);
            }
            let negative = |sign| matches!(sign, Negative | NegativeInfinity);
            return Ok(if negative(a) != negative(b) {
                Numeric::NEGATIVE_INFINITY
            } else {
                Numeric::special(Infinity)
            });
        }
        let rscale = (i64::from(self.dscale) + i64::from(other.dscale)).min(DSCALE_MAX);
        let mut product = Var::from(self).mul(&Var::from(other));
        product.round(rscale);
        product.make()
    }

    /// `numeric_div`: the result scale is the one `select_div_scale` picks, and the last digit is rounded half away from zero.
    pub fn div(&self, other: &Numeric) -> Result<Numeric, TypeError> {
        use NumericSign::{Infinity, NaN, Negative, NegativeInfinity, Positive};
        let (a, b) = (self.sign, other.sign);
        let zero = || TypeError::new(SqlState::DIVISION_BY_ZERO, "division by zero".to_string());
        if !a.is_finite() || !b.is_finite() {
            if a == NaN || b == NaN {
                return Ok(Numeric::NAN);
            }
            if a.is_finite() {
                // A number over an infinity is zero.
                return Ok(Numeric { sign: Positive, weight: 0, dscale: 0, digits: Vec::new() });
            }
            if !b.is_finite() {
                return Ok(Numeric::NAN);
            }
            if other.digits.is_empty() {
                return Err(zero());
            }
            let negative = (a == NegativeInfinity) != (b == Negative);
            return Ok(Numeric::special(if negative { NegativeInfinity } else { Infinity }));
        }
        let (x, y) = (Var::from(self), Var::from(other));
        let rscale = select_div_scale(&x, &y);
        if y.digits.is_empty() {
            return Err(zero());
        }
        x.div(&y, rscale, true).make()
    }

    /// `numeric_mod`: the remainder of the quotient cut to an integer, with the sign of `self`.
    pub fn modulo(&self, other: &Numeric) -> Result<Numeric, TypeError> {
        let (a, b) = (self.sign, other.sign);
        if !a.is_finite() || !b.is_finite() {
            if a == NumericSign::NaN || b == NumericSign::NaN || !a.is_finite() {
                return Ok(Numeric::NAN);
            }
            // A number modulo an infinity is the number.
            return Ok(self.clone());
        }
        if other.digits.is_empty() {
            return Err(TypeError::new(SqlState::DIVISION_BY_ZERO, "division by zero".to_string()));
        }
        let (x, y) = (Var::from(self), Var::from(other));
        let mut product = y.mul(&x.div(&y, 0, false));
        product.dscale = y.dscale;
        product.sign = match product.sign {
            NumericSign::Positive if !product.digits.is_empty() => NumericSign::Negative,
            _ => NumericSign::Positive,
        };
        x.add(&product).make()
    }

    /// `cmp_numerics`: `NaN` is equal to itself and above every other value, and the display scale does not count.
    pub fn compare(&self, other: &Numeric) -> std::cmp::Ordering {
        let left = self.to_bytes();
        let right = other.to_bytes();
        crate::numeric_layout::key(&left).cmp(crate::numeric_layout::key(&right))
    }
}

/// The sum when one side is not a finite number, or `None` when both are.
fn special_sum(a: NumericSign, b: NumericSign) -> Option<Numeric> {
    use NumericSign::{Infinity, NaN, NegativeInfinity};
    match (a, b) {
        _ if a.is_finite() && b.is_finite() => None,
        (NaN, _) | (_, NaN) => Some(Numeric::NAN),
        (Infinity, NegativeInfinity) | (NegativeInfinity, Infinity) => Some(Numeric::NAN),
        (Infinity, _) | (_, Infinity) => Some(Numeric::INFINITY),
        _ => Some(Numeric::NEGATIVE_INFINITY),
    }
}

/// `NUMERIC_MIN_SIG_DIGITS`, the fewest significant digits that a quotient has.
const MIN_SIG_DIGITS: i64 = 16;

/// `select_div_scale`: enough digits after the point for 16 significant digits, and no fewer than the display scale of either side.
fn select_div_scale(x: &Var, y: &Var) -> i64 {
    let first = |v: &Var| match v.digits.iter().position(|&d| d != 0) {
        Some(at) => (v.weight - at as i64, v.digits[at]),
        None => (0, 0),
    };
    let (weight1, first1) = first(x);
    let (weight2, first2) = first(y);
    let mut qweight = weight1 - weight2;
    if first1 <= first2 {
        qweight -= 1;
    }
    let rscale = MIN_SIG_DIGITS - qweight * DEC_DIGITS;
    rscale.max(x.dscale).max(y.dscale).clamp(0, 1000)
}

/// `NumericVar`: the working form. The weight and the scale can go out of the range of the format here, and [`Var::make`] checks them at the end, as `make_result` does.
#[derive(Debug, Clone)]
struct Var {
    sign: NumericSign,
    weight: i64,
    dscale: i64,
    digits: Vec<i32>,
}

impl From<&Numeric> for Var {
    fn from(v: &Numeric) -> Var {
        Var {
            sign: v.sign,
            weight: i64::from(v.weight),
            dscale: i64::from(v.dscale),
            digits: v.digits.iter().map(|&d| i32::from(d)).collect(),
        }
    }
}

impl Var {
    /// The end of `set_var_from_str`: the value of the decimal digits `dec`, where the first digit has the power of ten `dweight`.
    fn from_decimal_digits(dec: &[u8], dweight: i64, dscale: i64) -> Var {
        let weight = if dweight >= 0 {
            (dweight + DEC_DIGITS) / DEC_DIGITS - 1
        } else {
            -((-dweight - 1) / DEC_DIGITS + 1)
        };
        // The number of decimal zeros before the first digit that align it in its NBASE digit.
        let offset = (weight + 1) * DEC_DIGITS - (dweight + 1);
        let ndigits = (dec.len() as i64 + offset + DEC_DIGITS - 1) / DEC_DIGITS;
        let at = |k: i64| match usize::try_from(k - offset) {
            Ok(k) if k < dec.len() => i32::from(dec[k]),
            _ => 0,
        };
        let digits = (0..ndigits)
            .map(|n| {
                let k = n * DEC_DIGITS;
                ((at(k) * 10 + at(k + 1)) * 10 + at(k + 2)) * 10 + at(k + 3)
            })
            .collect();
        let mut var = Var { sign: NumericSign::Positive, weight, dscale, digits };
        var.strip();
        var
    }

    /// `strip_var`.
    fn strip(&mut self) {
        let lead = self.digits.iter().take_while(|&&d| d == 0).count();
        self.digits.drain(..lead);
        self.weight -= lead as i64;
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        if self.digits.is_empty() {
            self.sign = NumericSign::Positive;
            self.weight = 0;
        }
    }

    /// `round_var`: rounds half away from zero to `rscale` digits after the point. A negative `rscale` rounds before the point.
    fn round(&mut self, rscale: i64) {
        self.dscale = rscale;
        let di = (self.weight + 1) * DEC_DIGITS + rscale;
        if di < 0 {
            self.digits.clear();
            self.weight = 0;
            self.sign = NumericSign::Positive;
            return;
        }
        let n = ((di + DEC_DIGITS - 1) / DEC_DIGITS) as usize;
        let di = (di % DEC_DIGITS) as usize;
        if n > self.digits.len() || (n == self.digits.len() && di == 0) {
            return;
        }
        let (mut i, mut carry) = if di == 0 {
            (n, i32::from(self.digits[n] >= HALF_NBASE))
        } else {
            // Round inside the last NBASE digit.
            let i = n - 1;
            let pow10 = ROUND_POWERS[di];
            let extra = self.digits[i] % pow10;
            self.digits[i] -= extra;
            let mut carry = 0;
            if extra >= pow10 / 2 {
                self.digits[i] += pow10;
                if self.digits[i] >= NBASE {
                    self.digits[i] -= NBASE;
                    carry = 1;
                }
            }
            (i, carry)
        };
        self.digits.truncate(n);
        while carry != 0 {
            if i == 0 {
                self.digits.insert(0, carry);
                self.weight += 1;
                break;
            }
            i -= 1;
            self.digits[i] += carry;
            carry = 0;
            if self.digits[i] >= NBASE {
                self.digits[i] -= NBASE;
                carry = 1;
            }
        }
    }

    /// `trunc_var`: drops the digits after `rscale` digits after the point.
    fn trunc(&mut self, rscale: i64) {
        self.dscale = rscale;
        let di = (self.weight + 1) * DEC_DIGITS + rscale;
        if di <= 0 {
            self.digits.clear();
            self.weight = 0;
            self.sign = NumericSign::Positive;
            return;
        }
        let n = ((di + DEC_DIGITS - 1) / DEC_DIGITS) as usize;
        if n <= self.digits.len() {
            self.digits.truncate(n);
            let di = (di % DEC_DIGITS) as usize;
            if di > 0 {
                self.digits[n - 1] -= self.digits[n - 1] % ROUND_POWERS[di];
            }
        }
    }

    /// `apply_typmod`: rounds to the scale of the typmod and checks the digits before the point.
    fn apply_typmod(&mut self, typmod: i32) -> Result<(), TypeError> {
        let Some((precision, scale)) = numeric_precision_scale(typmod) else {
            return Ok(());
        };
        let maxdigits = i64::from(precision - scale);
        self.round(i64::from(scale));
        self.dscale = self.dscale.max(0);
        // The weight can count leading zero digits that `make` strips later, and a zero has no weight that means anything, so find the first digit that is not zero.
        let mut ddigits = (self.weight + 1) * DEC_DIGITS;
        if ddigits > maxdigits {
            for &dig in &self.digits {
                if dig != 0 {
                    ddigits -= match dig {
                        0..10 => 3,
                        10..100 => 2,
                        100..1000 => 1,
                        _ => 0,
                    };
                    if ddigits > maxdigits {
                        let limit = match maxdigits {
                            0 => "1".to_string(),
                            n => format!("10^{n}"),
                        };
                        return Err(field_overflow(format!(
                            "A field with precision {precision}, scale {scale} must round to an absolute value less than {limit}."
                        )));
                    }
                    break;
                }
                ddigits -= DEC_DIGITS;
            }
        }
        Ok(())
    }

    /// `make_result`: strips the zeros and checks that the weight and the scale fit the format.
    fn make(mut self) -> Result<Numeric, TypeError> {
        if !self.sign.is_finite() {
            return Ok(Numeric::special(self.sign));
        }
        self.strip();
        match (i16::try_from(self.weight), u16::try_from(self.dscale)) {
            (Ok(weight), Ok(dscale)) if self.dscale <= DSCALE_MAX => Ok(Numeric {
                sign: self.sign,
                weight,
                dscale,
                digits: self.digits.iter().map(|&d| d as i16).collect(),
            }),
            _ => Err(overflow()),
        }
    }
}

impl Var {
    /// The digit with the power of 10000 `weight`, which is 0 outside the digits.
    fn digit(&self, weight: i64) -> i32 {
        usize::try_from(self.weight - weight)
            .ok()
            .and_then(|at| self.digits.get(at))
            .copied()
            .unwrap_or(0)
    }

    /// The power of 10000 of the last digit.
    fn low(&self) -> i64 {
        self.weight + 1 - self.digits.len() as i64
    }

    /// `cmp_abs`.
    fn cmp_abs(&self, other: &Var) -> std::cmp::Ordering {
        let high = self.weight.max(other.weight);
        let low = self.low().min(other.low());
        for weight in (low..=high).rev() {
            match self.digit(weight).cmp(&other.digit(weight)) {
                std::cmp::Ordering::Equal => {}
                order => return order,
            }
        }
        std::cmp::Ordering::Equal
    }

    /// `add_abs` and `sub_abs` in one: the sum of the two magnitudes, or the difference when `subtract` is set, which needs the magnitude of `self` to be the larger one.
    fn add_abs(&self, other: &Var, subtract: bool, sign: NumericSign) -> Var {
        let weight = self.weight.max(other.weight) + 1;
        let low = self.low().min(other.low());
        let mut digits = vec![0; (weight - low + 1) as usize];
        let mut carry = 0;
        for (at, w) in (low..=weight).enumerate() {
            let other = other.digit(w);
            let mut d = self.digit(w) + carry + if subtract { -other } else { other };
            carry = 0;
            if d >= NBASE {
                d -= NBASE;
                carry = 1;
            } else if d < 0 {
                d += NBASE;
                carry = -1;
            }
            let end = digits.len() - 1 - at;
            digits[end] = d;
        }
        let mut var = Var { sign, weight, dscale: self.dscale.max(other.dscale), digits };
        var.strip();
        var
    }

    /// `add_var`.
    fn add(&self, other: &Var) -> Var {
        if self.sign == other.sign {
            return self.add_abs(other, false, self.sign);
        }
        match self.cmp_abs(other) {
            std::cmp::Ordering::Less => other.add_abs(self, true, other.sign),
            _ => self.add_abs(other, true, self.sign),
        }
    }

    /// The exact product, with no rounding. The display scale is set by the caller.
    fn mul(&self, other: &Var) -> Var {
        let sign =
            if self.sign == other.sign { NumericSign::Positive } else { NumericSign::Negative };
        if self.digits.is_empty() || other.digits.is_empty() {
            return Var { sign: NumericSign::Positive, weight: 0, dscale: 0, digits: Vec::new() };
        }
        let mut acc = vec![0i64; self.digits.len() + other.digits.len()];
        for (i, &a) in self.digits.iter().enumerate() {
            for (j, &b) in other.digits.iter().enumerate() {
                acc[i + j + 1] += i64::from(a) * i64::from(b);
            }
        }
        let mut carry = 0;
        for slot in acc.iter_mut().rev() {
            let total = *slot + carry;
            *slot = total % i64::from(NBASE);
            carry = total / i64::from(NBASE);
        }
        let digits = acc.into_iter().map(|d| d as i32).collect();
        let weight = self.weight + other.weight + 1;
        let mut var = Var { sign, weight, dscale: self.dscale + other.dscale, digits };
        var.strip();
        var
    }

    /// `div_var` with `exact` set: the quotient to `rscale` digits after the point, rounded half away from zero when `round` is set and cut when it is not. The divisor is not zero.
    fn div(&self, other: &Var, rscale: i64, round: bool) -> Var {
        let sign =
            if self.sign == other.sign { NumericSign::Positive } else { NumericSign::Negative };
        // The quotient is cut one digit of 10000 below the scale and then rounded. The cut keeps every digit above it, so the rounding sees the same digit as for the exact quotient.
        let low = -((rscale + DEC_DIGITS - 1) / DEC_DIGITS + 1);
        let shift = self.low() - other.low() - low;
        let mut dividend: Vec<i64> = self.digits.iter().map(|&d| i64::from(d)).collect();
        if shift >= 0 {
            dividend.resize(dividend.len() + shift as usize, 0);
        } else {
            let keep = dividend.len().saturating_sub((-shift) as usize);
            dividend.truncate(keep);
        }
        let divisor: Vec<i64> = other.digits.iter().map(|&d| i64::from(d)).collect();
        let quotient = divide(&dividend, &divisor);
        let weight = low + quotient.len() as i64 - 1;
        let digits = quotient.into_iter().map(|d| d as i32).collect();
        let mut var = Var { sign, weight, dscale: rscale, digits };
        var.strip();
        if round {
            var.round(rscale);
        } else {
            var.trunc(rscale);
        }
        var.strip();
        if var.digits.is_empty() {
            var.sign = NumericSign::Positive;
        }
        var
    }
}

/// The integer quotient of two numbers in base 10000, the most significant digit first, by algorithm D of Knuth (TAOCP volume 2, section 4.3.1). The divisor has no leading zero.
fn divide(dividend: &[i64], divisor: &[i64]) -> Vec<i64> {
    let base = i64::from(NBASE);
    let n = divisor.len();
    if dividend.len() < n {
        return Vec::new();
    }
    if n == 1 {
        let d = divisor[0];
        let mut rest = 0;
        return dividend
            .iter()
            .map(|&digit| {
                let current = rest * base + digit;
                rest = current % d;
                current / d
            })
            .collect();
    }
    // Scale both so that the first digit of the divisor is at least half the base, which keeps each trial quotient at most 2 above the true digit.
    let scale = base / (divisor[0] + 1);
    let times = |digits: &[i64]| -> Vec<i64> {
        let mut out = vec![0; digits.len() + 1];
        let mut carry = 0;
        for (at, &digit) in digits.iter().enumerate().rev() {
            let total = digit * scale + carry;
            out[at + 1] = total % base;
            carry = total / base;
        }
        out[0] = carry;
        out
    };
    let mut u = times(dividend);
    let v = times(divisor);
    let v = &v[1..];
    let m = dividend.len() - n;
    let mut q = vec![0; m + 1];
    for j in 0..=m {
        let top = u[j] * base + u[j + 1];
        let mut qhat = top / v[0];
        let mut rhat = top % v[0];
        while qhat >= base || qhat * v[1] > rhat * base + u[j + 2] {
            qhat -= 1;
            rhat += v[0];
            if rhat >= base {
                break;
            }
        }
        // Take qhat times the divisor away from the window of the dividend.
        let mut borrow = 0;
        let mut carry = 0;
        for i in (0..n).rev() {
            let product = qhat * v[i] + carry;
            carry = product / base;
            let mut d = u[j + i + 1] - product % base - borrow;
            borrow = 0;
            if d < 0 {
                d += base;
                borrow = 1;
            }
            u[j + i + 1] = d;
        }
        let d = u[j] - carry - borrow;
        if d < 0 {
            // The trial digit was one too large: add the divisor back.
            qhat -= 1;
            let mut carry = 0;
            for i in (0..n).rev() {
                let total = u[j + i + 1] + v[i] + carry;
                u[j + i + 1] = total % base;
                carry = total / base;
            }
            u[j] = d + carry;
        } else {
            u[j] = d;
        }
        q[j] = qhat;
    }
    q
}

fn overflow() -> TypeError {
    TypeError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        "value overflows numeric format".to_string(),
    )
}

fn field_overflow(detail: String) -> TypeError {
    let mut error =
        TypeError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow".to_string());
    error.detail = Some(detail);
    error
}

/// `apply_typmod_special`: `NaN` fits any typmod and an infinity fits none.
fn apply_typmod_special(sign: NumericSign, typmod: i32) -> Result<(), TypeError> {
    match numeric_precision_scale(typmod) {
        Some((precision, scale)) if sign != NumericSign::NaN => Err(field_overflow(format!(
            "A field with precision {precision}, scale {scale} cannot hold an infinite value."
        ))),
        _ => Ok(()),
    }
}

/// The text input of `numeric` with the typmod of the target, -1 for none.
pub fn numeric_in(s: &str, typmod: i32) -> Result<Numeric, TypeError> {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let syntax = || TypeError::syntax("numeric", s);
    let rest_is_space = |mut i: usize| {
        while i < b.len() {
            if !is_space(b[i]) {
                return Err(syntax());
            }
            i += 1;
        }
        Ok(())
    };

    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let start = i;
    let negative = at(i) == b'-';
    if negative || at(i) == b'+' {
        i += 1;
    }

    // Each other input has a digit or a point after the sign. `NaN` cannot have a sign.
    if !at(i).is_ascii_digit() && at(i) != b'.' {
        let word = |from: usize, word: &str| {
            b.get(from..from + word.len()).is_some_and(|w| w.eq_ignore_ascii_case(word.as_bytes()))
        };
        let infinity = if negative { NumericSign::NegativeInfinity } else { NumericSign::Infinity };
        let (sign, end) = if word(start, "nan") {
            (NumericSign::NaN, start + 3)
        } else if word(i, "infinity") {
            (infinity, i + 8)
        } else if word(i, "inf") {
            (infinity, i + 3)
        } else {
            return Err(syntax());
        };
        rest_is_space(end)?;
        apply_typmod_special(sign, typmod)?;
        return Ok(Numeric::special(sign));
    }

    let base = match (at(i), at(i + 1)) {
        (b'0', b'x' | b'X') => 16,
        (b'0', b'o' | b'O') => 8,
        (b'0', b'b' | b'B') => 2,
        _ => 10,
    };
    let (mut var, end) = match base {
        10 => decimal(b, i, s)?,
        _ => non_decimal(b, i + 2, base, s)?,
    };
    if negative {
        var.sign = NumericSign::Negative;
    }
    rest_is_space(end)?;
    var.apply_typmod(typmod)?;
    var.make()
}

/// `set_var_from_str` from the first byte after the sign.
fn decimal(b: &[u8], mut i: usize, input: &str) -> Result<(Var, usize), TypeError> {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let syntax = || TypeError::syntax("numeric", input);
    let mut have_point = at(i) == b'.';
    if have_point {
        i += 1;
    }
    if !at(i).is_ascii_digit() {
        return Err(syntax());
    }
    let mut dec = Vec::with_capacity(b.len() - i);
    let mut dweight: i64 = -1;
    let mut dscale: i64 = 0;
    loop {
        match at(i) {
            c @ b'0'..=b'9' => {
                dec.push(c - b'0');
                i += 1;
                if have_point {
                    dscale += 1;
                } else {
                    dweight += 1;
                }
            }
            b'.' => {
                // A second point, or a point with an underscore after it, is an error.
                if have_point || at(i + 1) == b'_' {
                    return Err(syntax());
                }
                have_point = true;
                i += 1;
            }
            b'_' => {
                i += 1;
                if !at(i).is_ascii_digit() {
                    return Err(syntax());
                }
            }
            _ => break,
        }
    }

    if matches!(at(i), b'e' | b'E') {
        i += 1;
        let negative = at(i) == b'-';
        if negative || at(i) == b'+' {
            i += 1;
        }
        if !at(i).is_ascii_digit() {
            return Err(syntax());
        }
        let mut exponent: i64 = 0;
        loop {
            match at(i) {
                c @ b'0'..=b'9' => {
                    exponent = exponent * 10 + i64::from(c - b'0');
                    i += 1;
                    if exponent > i64::from(i32::MAX / 2) {
                        return Err(overflow());
                    }
                }
                b'_' => {
                    i += 1;
                    if !at(i).is_ascii_digit() {
                        return Err(syntax());
                    }
                }
                _ => break,
            }
        }
        if negative {
            exponent = -exponent;
        }
        dweight += exponent;
        dscale = (dscale - exponent).max(0);
    }
    Ok((Var::from_decimal_digits(&dec, dweight, dscale), i))
}

/// `set_var_from_non_decimal_integer_str` from the first digit after the prefix. The digits go in groups that fit in an `i64`, and the weight is checked after each group, as PostgreSQL does, so a value that is too large fails before a syntax error after it.
fn non_decimal(b: &[u8], mut i: usize, base: u64, input: &str) -> Result<(Var, usize), TypeError> {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let syntax = || TypeError::syntax("numeric", input);
    // The value so far in base-10000 digits, the least significant first.
    let mut value: Vec<i32> = Vec::new();
    let add_group = |value: &mut Vec<i32>, tmp: u64, mul: u64| {
        let mut carry = u128::from(tmp);
        for d in value.iter_mut() {
            let v = *d as u128 * u128::from(mul) + carry;
            *d = (v % NBASE as u128) as i32;
            carry = v / NBASE as u128;
        }
        while carry > 0 {
            value.push((carry % NBASE as u128) as i32);
            carry /= NBASE as u128;
        }
        if value.len() > i16::MAX as usize + 1 { Err(overflow()) } else { Ok(()) }
    };
    let first = i;
    let (mut tmp, mut mul) = (0u64, 1u64);
    loop {
        if let Some(d) = digit(at(i), base) {
            if mul > i64::MAX as u64 / base {
                add_group(&mut value, tmp, mul)?;
                (tmp, mul) = (0, 1);
            }
            tmp = tmp * base + d;
            mul *= base;
            i += 1;
        } else if at(i) == b'_' {
            i += 1;
            if digit(at(i), base).is_none() {
                return Err(syntax());
            }
        } else {
            break;
        }
    }
    if i == first {
        return Err(syntax());
    }
    add_group(&mut value, tmp, mul)?;
    value.reverse();
    let mut var = Var {
        sign: NumericSign::Positive,
        weight: value.len() as i64 - 1,
        dscale: 0,
        digits: value,
    };
    var.strip();
    Ok((var, i))
}

/// Writes four decimal digits.
fn four(dig: i32, out: &mut Vec<u8>) {
    let dig = dig as u16;
    out.extend_from_slice(&[
        b'0' + (dig / 1000) as u8,
        b'0' + (dig / 100 % 10) as u8,
        b'0' + (dig / 10 % 10) as u8,
        b'0' + (dig % 10) as u8,
    ]);
}

/// The text output of `numeric`: the digits with exactly the display scale after the point.
pub fn numeric_out(v: &Numeric, out: &mut Vec<u8>) {
    match v.sign {
        NumericSign::NaN => return out.extend_from_slice(b"NaN"),
        NumericSign::Infinity => return out.extend_from_slice(b"Infinity"),
        NumericSign::NegativeInfinity => return out.extend_from_slice(b"-Infinity"),
        NumericSign::Negative => out.push(b'-'),
        NumericSign::Positive => {}
    }
    let at = |d: i32| match usize::try_from(d) {
        Ok(d) if d < v.digits.len() => i32::from(v.digits[d]),
        _ => 0,
    };
    let weight = i32::from(v.weight);
    if weight < 0 {
        out.push(b'0');
    } else {
        // The first digit has no leading zeros.
        u64_out(at(0) as u64, out);
        for d in 1..=weight {
            four(at(d), out);
        }
    }
    let dscale = usize::from(v.dscale);
    if dscale > 0 {
        out.push(b'.');
        let end = out.len() + dscale;
        let mut d = weight + 1;
        while out.len() < end {
            four(at(d), out);
            d += 1;
        }
        out.truncate(end);
    }
}

/// `numeric_send`. PostgreSQL reads the display scale of an infinity from bits of the header that the short format uses for the scale, so it sends 32 there. Clients do not look at it, but the bytes are the same.
pub fn numeric_send(v: &Numeric, out: &mut Vec<u8>) {
    let dscale = match v.sign {
        NumericSign::Infinity | NumericSign::NegativeInfinity => 32,
        _ => v.dscale,
    };
    write_binary(v.sign, v.weight, dscale, &v.digits, out);
}

fn write_binary(sign: NumericSign, weight: i16, dscale: u16, digits: &[i16], out: &mut Vec<u8>) {
    out.reserve(8 + 2 * digits.len());
    out.extend_from_slice(&(digits.len() as u16).to_be_bytes());
    out.extend_from_slice(&weight.to_be_bytes());
    out.extend_from_slice(&sign.code().to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for &d in digits {
        out.extend_from_slice(&d.to_be_bytes());
    }
}

/// `numeric_recv`. Digits hidden by the display scale are dropped, and the typmod is applied.
pub fn numeric_recv(recv: &mut Recv<'_>, typmod: i32) -> Result<Numeric, TypeError> {
    let bad = |what: &str| {
        TypeError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            format!("invalid {what} in external \"numeric\" value"),
        )
    };
    let len = recv.i16()? as u16;
    let weight = recv.i16()?;
    let sign = NumericSign::from_code(recv.i16()? as u16).ok_or_else(|| bad("sign"))?;
    let dscale = recv.i16()? as u16;
    if i64::from(dscale) > DSCALE_MAX {
        return Err(bad("scale"));
    }
    let mut digits = Vec::new();
    for _ in 0..len {
        let d = recv.i16()?;
        if !(0..NBASE as i16).contains(&d) {
            return Err(bad("digit"));
        }
        digits.push(i32::from(d));
    }
    if !sign.is_finite() {
        apply_typmod_special(sign, typmod)?;
        return Ok(Numeric::special(sign));
    }
    let mut var = Var { sign, weight: i64::from(weight), dscale: i64::from(dscale), digits };
    var.trunc(i64::from(dscale));
    var.apply_typmod(typmod)?;
    var.make()
}

/// The base-10000 digits of `abs / 10^scale` with no zero at either end, and the weight of the first one. The digits are aligned to the decimal point, so the last group of the fraction has zeros after its last decimal digit.
fn decimal_digits(abs: u128, scale: u32, buf: &mut [i16; DECIMAL_DIGITS_MAX]) -> (&[i16], i16) {
    assert!(scale <= 38, "a decimal has at most 38 digits after the point");
    let pow = 10u128.pow(scale);
    let (mut int, mut frac) = (abs / pow, abs % pow);
    let mut at = buf.len();
    let partial = scale % 4;
    if partial != 0 {
        let q = 10u128.pow(partial);
        at -= 1;
        buf[at] = ((frac % q) * 10u128.pow(4 - partial)) as i16;
        frac /= q;
    }
    for _ in 0..scale / 4 {
        at -= 1;
        buf[at] = (frac % NBASE as u128) as i16;
        frac /= NBASE as u128;
    }
    let point = at;
    while int > 0 {
        at -= 1;
        buf[at] = (int % NBASE as u128) as i16;
        int /= NBASE as u128;
    }
    let mut weight = (point - at) as i16 - 1;
    while at < buf.len() && buf[at] == 0 {
        at += 1;
        weight -= 1;
    }
    let mut end = buf.len();
    while end > at && buf[end - 1] == 0 {
        end -= 1;
    }
    if at == end {
        weight = 0;
    }
    (&buf[at..end], weight)
}

/// Writes `n` in decimal.
fn u128_out(n: u128, out: &mut Vec<u8>) {
    if let Ok(n) = u64::try_from(n) {
        return u64_out(n, out);
    }
    let mut buf = [0u8; 39];
    let mut at = buf.len();
    let mut n = n;
    while n > 0 {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[at..]);
}

/// The text output of `value / 10^scale` from a `numeric(p, s)` column of the engine. It is the text that [`numeric_out`] gives for the same value with `scale` as its display scale.
pub fn decimal_out(value: i128, scale: u32, out: &mut Vec<u8>) {
    if value < 0 {
        out.push(b'-');
    }
    let abs = value.unsigned_abs();
    if scale == 0 {
        return u128_out(abs, out);
    }
    let pow = 10u128.pow(scale);
    u128_out(abs / pow, out);
    out.push(b'.');
    let mut frac = abs % pow;
    let start = out.len();
    out.resize(start + scale as usize, b'0');
    for at in out[start..].iter_mut().rev() {
        *at = b'0' + (frac % 10) as u8;
        frac /= 10;
    }
}

/// The binary output of `value / 10^scale`. It is the bytes that [`numeric_send`] gives for the same value with `scale` as its display scale.
pub fn decimal_send(value: i128, scale: u32, out: &mut Vec<u8>) {
    let mut buf = [0; DECIMAL_DIGITS_MAX];
    let (digits, weight) = decimal_digits(value.unsigned_abs(), scale, &mut buf);
    let sign = if value < 0 { NumericSign::Negative } else { NumericSign::Positive };
    write_binary(sign, weight, scale as u16, digits, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(v: &Numeric) -> String {
        let mut out = Vec::new();
        numeric_out(v, &mut out);
        String::from_utf8(out).unwrap()
    }

    fn round_trip(input: &str, typmod: i32) -> String {
        numeric_in(input, typmod).map(|v| text(&v)).unwrap_or_else(|e| e.message)
    }

    fn value(text: &str) -> Numeric {
        numeric_in(text, -1).unwrap()
    }

    #[test]
    fn the_arithmetic_keeps_the_scales_of_postgres() {
        // The texts are the ones PostgreSQL 19 prints.
        for (a, op, b, out) in [
            ("1.0", '/', "3", "0.33333333333333333333"),
            ("10.0", '/', "4", "2.5000000000000000"),
            ("1e20", '/', "3", "33333333333333333333"),
            ("0.000001", '/', "3", "0.000000333333333333333333"),
            ("2", '/', "3", "0.66666666666666666667"),
            ("-2", '/', "3", "-0.66666666666666666667"),
            ("123456789012345678901234567890", '/', "7", "17636684144620811271604938270"),
            ("1", '/', "98765432109876543210", "0.000000000000000000010124999998860938"),
            ("99999999", '/', "0.0001", "999999990000.00000000"),
            ("0", '/', "5", "0.00000000000000000000"),
            ("1.50", '+', "2.125", "3.625"),
            ("1.50", '-', "2.125", "-0.625"),
            ("1.5", '-', "1.5", "0.0"),
            ("1.50", '*', "2.125", "3.18750"),
            ("-0.01", '*', "0", "0.00"),
            ("9999.9999", '+', "0.0001", "10000.0000"),
            ("NaN", '+', "1", "NaN"),
            ("Infinity", '-', "Infinity", "NaN"),
            ("Infinity", '*', "-2", "-Infinity"),
            ("Infinity", '*', "0", "NaN"),
            ("1", '/', "Infinity", "0"),
            ("7.5", '%', "2", "1.5"),
            ("-7.5", '%', "2", "-1.5"),
            ("7", '%', "-0.25", "0.00"),
            ("10", '%', "Infinity", "10"),
            ("-Infinity", '/', "-3", "Infinity"),
        ] {
            let (a, b) = (value(a), value(b));
            let result = match op {
                '+' => a.add(&b),
                '-' => a.sub(&b),
                '*' => a.mul(&b),
                '%' => a.modulo(&b),
                _ => a.div(&b),
            };
            assert_eq!(text(&result.unwrap()), out, "{} {op} {}", text(&a), text(&b));
        }
        assert_eq!(value("1").div(&value("0")).unwrap_err().message, "division by zero");
        assert_eq!(value("Infinity").div(&value("0")).unwrap_err().message, "division by zero");
    }

    #[test]
    fn the_long_division_agrees_with_the_integers() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let a = i128::from(next() >> (next() % 60));
            let b = i128::from(next() >> (next() % 63)).max(1);
            let quotient = Numeric::from_integer(a).div(&Numeric::from_integer(b)).unwrap();
            assert_eq!(
                quotient.with_typmod(-1).unwrap().to_decimal(0),
                Some((2 * a + b) / (2 * b)),
                "{a} / {b}"
            );
            let back = quotient.mul(&Numeric::from_integer(b)).unwrap();
            let error = back.sub(&Numeric::from_integer(a)).unwrap().to_f64().abs();
            assert!(error <= b as f64, "{a} / {b} = {}", text(&quotient));
        }
    }

    #[test]
    fn a_double_converts_with_fifteen_digits() {
        for (input, out) in [
            (0.1, "0.1"),
            (1.0 / 3.0, "0.333333333333333"),
            (1e20, "100000000000000000000"),
            (1.5e-7, "0.00000015"),
            (-2.0, "-2"),
            (123456.789, "123456.789"),
        ] {
            assert_eq!(text(&Numeric::from_f64(input)), out);
        }
        assert_eq!(value("0.33333333333333333333").to_f64(), 1.0 / 3.0);
        assert_eq!(value("2.5").to_integer("integer").unwrap(), 3);
        assert_eq!(value("-2.5").to_integer("integer").unwrap(), -3);
        assert_eq!(
            value("NaN").to_integer("integer").unwrap_err().message,
            "cannot convert NaN to integer"
        );
    }

    #[test]
    fn the_bytes_hold_the_value_and_order_it() {
        for input in ["0.00", "-123456.78", "1e-30", "NaN", "-Infinity", "33333333333333333333.3"] {
            let v = value(input);
            assert_eq!(Numeric::from_bytes(&v.to_bytes()), v);
        }
        assert_eq!(value("1.0").compare(&value("1.000")), std::cmp::Ordering::Equal);
        assert_eq!(value("-1.1").compare(&value("-1.01")), std::cmp::Ordering::Less);
    }

    #[test]
    fn the_input_keeps_the_scale_and_strips_the_zeros() {
        let v = numeric_in(" -0012.3400 ", -1).unwrap();
        assert_eq!(
            (v.sign(), v.weight(), v.dscale(), v.digits()),
            (NumericSign::Negative, 0, 4, &[12, 3400][..])
        );
        assert_eq!(text(&v), "-12.3400");
        let zero = numeric_in("-0.00", -1).unwrap();
        assert_eq!(
            (zero.sign(), zero.weight(), zero.dscale(), zero.digits()),
            (NumericSign::Positive, 0, 2, &[][..])
        );
        assert_eq!(round_trip("1e3", -1), "1000");
        assert_eq!(round_trip("1.5e-3", -1), "0.0015");
        assert_eq!(round_trip("0x_1F", -1), "31");
        assert_eq!(round_trip("-0b1_01", -1), "-5");
        assert_eq!(round_trip("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF", -1), u128::MAX.to_string());
        assert_eq!(round_trip("1e-16384", -1), "value overflows numeric format");
        assert_eq!(round_trip("1e2147483647", -1), "value overflows numeric format");
    }

    #[test]
    fn round_trunc_ceil_and_floor_keep_the_digits() {
        let text = |value: Numeric| {
            let mut out = Vec::new();
            numeric_out(&value, &mut out);
            String::from_utf8(out).unwrap()
        };
        let value = numeric_in("-123456789012345678901.555", -1).unwrap();
        assert_eq!(text(value.round(2).unwrap()), "-123456789012345678901.56");
        assert_eq!(text(value.round(-1).unwrap()), "-123456789012345678900");
        assert_eq!(text(value.trunc(1).unwrap()), "-123456789012345678901.5");
        assert_eq!(text(value.whole(true).unwrap()), "-123456789012345678901");
        assert_eq!(text(value.whole(false).unwrap()), "-123456789012345678902");
        assert_eq!(text(value.abs()), "123456789012345678901.555");
        let small = numeric_in("-0.5", -1).unwrap();
        assert_eq!(text(small.whole(true).unwrap()), "0");
        assert_eq!(text(small.whole(false).unwrap()), "-1");
        assert_eq!(text(numeric_in("9.99", -1).unwrap().round(1).unwrap()), "10.0");
        assert_eq!(text(numeric_in("5", -1).unwrap().round(1).unwrap()), "5.0");
        let nan = numeric_in("NaN", -1).unwrap();
        assert_eq!(text(nan.round(2).unwrap()), "NaN");
    }

    #[test]
    fn rounding_carries_into_a_new_digit() {
        let typmod = crate::typmod::numeric_typmod;
        assert_eq!(round_trip("9999.99995", typmod(10, 4)), "10000.0000");
        assert_eq!(round_trip("0.6", typmod(1, 0)), "1");
        assert_eq!(round_trip("-0.004", typmod(5, 2)), "0.00");
        assert_eq!(round_trip("15", typmod(3, -1)), "20");
        assert_eq!(round_trip("-5", typmod(3, -1)), "-10");
        let error = numeric_in("999.995", typmod(5, 2)).unwrap_err();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("22003", "numeric field overflow")
        );
        assert_eq!(
            error.detail.as_deref(),
            Some(
                "A field with precision 5, scale 2 must round to an absolute value less than 10^3."
            )
        );
        let error = numeric_in("0.95", typmod(1, 1)).unwrap_err();
        assert_eq!(
            error.detail.as_deref(),
            Some("A field with precision 1, scale 1 must round to an absolute value less than 1.")
        );
        let error = numeric_in("-inf", typmod(5, 2)).unwrap_err();
        assert_eq!(
            error.detail.as_deref(),
            Some("A field with precision 5, scale 2 cannot hold an infinite value.")
        );
        assert_eq!(numeric_in("NaN", typmod(5, 2)), Ok(Numeric::NAN));
    }

    #[test]
    fn the_binary_form_is_the_fields() {
        let mut out = Vec::new();
        numeric_send(&numeric_in("-12.3400", -1).unwrap(), &mut out);
        assert_eq!(out, [0, 2, 0, 0, 0x40, 0, 0, 4, 0, 12, 0x0d, 0x48]);
        let back = numeric_recv(&mut Recv::new(&out), -1).unwrap();
        assert_eq!(text(&back), "-12.3400");

        // Digits that the scale hides are dropped, as `trunc_var` drops them.
        let hidden = [0, 2, 0, 0, 0, 0, 0, 1, 0, 1, 0x13, 0x88];
        assert_eq!(text(&numeric_recv(&mut Recv::new(&hidden), -1).unwrap()), "1.5");

        let bad = |bytes: &[u8]| numeric_recv(&mut Recv::new(bytes), -1).unwrap_err().message;
        assert_eq!(
            bad(&[0, 0, 0, 0, 0x12, 0x34, 0, 0]),
            "invalid sign in external \"numeric\" value"
        );
        assert_eq!(
            bad(&[0, 0, 0, 0, 0, 0, 0x40, 0]),
            "invalid scale in external \"numeric\" value"
        );
        assert_eq!(
            bad(&[0, 1, 0, 0, 0, 0, 0, 0, 0x27, 0x10]),
            "invalid digit in external \"numeric\" value"
        );
        assert_eq!(bad(&[0, 1, 0, 0, 0, 0, 0, 0]), "insufficient data left in message");
        let mut out = Vec::new();
        numeric_send(&Numeric::NEGATIVE_INFINITY, &mut out);
        assert_eq!(out, [0, 0, 0, 0, 0xf0, 0, 0, 32]);
    }

    #[test]
    fn a_decimal_of_the_engine_writes_what_numeric_writes() {
        let values = [
            0i128,
            1,
            -1,
            5,
            -5,
            9999,
            10000,
            123456789,
            -100000000,
            i64::MAX as i128,
            i128::MAX,
            i128::MIN + 1,
        ];
        for value in values {
            for scale in [0, 1, 2, 3, 4, 5, 8, 9, 18, 19, 37, 38] {
                let v = Numeric::from_decimal(value, scale);
                let mut out = Vec::new();
                decimal_out(value, scale, &mut out);
                let decimal = String::from_utf8(out).unwrap();
                assert_eq!(decimal, text(&v), "{value} {scale}");
                assert_eq!(numeric_in(&decimal, -1), Ok(v.clone()), "{value} {scale}");
                let (mut sent, mut expected) = (Vec::new(), Vec::new());
                decimal_send(value, scale, &mut sent);
                numeric_send(&v, &mut expected);
                assert_eq!(sent, expected, "{value} {scale}");
                assert_eq!(v.to_decimal(scale), Some(value), "{value} {scale}");
            }
        }
        let v = numeric_in("-1.005", -1).unwrap();
        assert_eq!(
            (v.to_decimal(2), v.to_decimal(0), v.to_decimal(5)),
            (Some(-101), Some(-1), Some(-100500))
        );
        assert_eq!(numeric_in("1e40", -1).unwrap().to_decimal(0), None);
        assert_eq!(Numeric::NAN.to_decimal(0), None);
    }
}
