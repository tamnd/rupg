//! The square root, the exponential, the logarithms and the powers of `numeric`. These are ports of `sqrt_var`, `exp_var`, `ln_var`, `log_var`, `power_var` and `power_var_int`, with the multiplication and the division that they use: `mul_var` keeps only the digits that the result scale needs, and `div_var` finds the quotient digits from an estimate in floating point when it does not have to be exact. The digits that these two drop change the last digits of a result, so the ports drop the same digits.

use std::cmp::Ordering;

use rupg_common::SqlState;

use super::{DEC_DIGITS, MIN_SIG_DIGITS, NBASE, Numeric, NumericSign, Var, overflow};
use crate::error::TypeError;

/// `NBASE * NBASE`: the base of the digit pairs of `mul_var` and `div_var`.
const NBASE_SQR: i64 = 100_000_000;
/// `MUL_GUARD_DIGITS`: the digits that `mul_var` computes after the result scale.
const MUL_GUARD_DIGITS: i64 = 2;
/// `DIV_GUARD_DIGITS`: the digits that `div_var` computes after the result scale when the quotient need not be exact.
const DIV_GUARD_DIGITS: i64 = 4;
/// `NUMERIC_MAX_DISPLAY_SCALE`.
const MAX_DISPLAY_SCALE: i64 = 1000;
/// `NUMERIC_WEIGHT_MAX`.
const WEIGHT_MAX: i64 = i16::MAX as i64;
/// `NUMERIC_MAX_RESULT_SCALE * 3`: `exp_var` gives zero or an overflow for an argument with a larger magnitude.
const EXP_LIMIT: f64 = 6000.0;
// The estimates of the scales use these values with 15 digits, as numeric.c has them, and not the constants of the standard library.
/// `log10(e)`.
#[allow(clippy::approx_constant)]
const LOG10_E: f64 = 0.434294481903252;
/// `log10(2)`.
#[allow(clippy::approx_constant)]
const LOG10_2: f64 = 0.301029995663981;
/// `ln(10)`.
#[allow(clippy::approx_constant)]
const LN_10: f64 = 2.302585092994046;

fn log_error(message: &str) -> TypeError {
    TypeError::new(SqlState::INVALID_ARGUMENT_FOR_LOG, message.to_string())
}

fn power_error(message: &str) -> TypeError {
    TypeError::new(SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION, message.to_string())
}

fn division_by_zero() -> TypeError {
    TypeError::new(SqlState::DIVISION_BY_ZERO, "division by zero".to_string())
}

/// The display scale for a result with `MIN_SIG_DIGITS` significant digits, no less than the scales of the arguments.
fn result_scale(dweight: i64, scales: &[i64]) -> i64 {
    let rscale = scales.iter().fold(MIN_SIG_DIGITS - dweight, |r, &s| r.max(s));
    rscale.clamp(0, MAX_DISPLAY_SCALE)
}

impl Var {
    fn zero(dscale: i64) -> Var {
        Var { sign: NumericSign::Positive, weight: 0, dscale, digits: Vec::new() }
    }

    /// A var of a small integer, as `int64_to_numericvar` gives it.
    fn int(value: i64) -> Var {
        let sign = if value < 0 { NumericSign::Negative } else { NumericSign::Positive };
        let mut rest = value.unsigned_abs();
        let mut digits = Vec::new();
        while rest > 0 {
            digits.insert(0, (rest % NBASE as u64) as i32);
            rest /= NBASE as u64;
        }
        let weight = digits.len() as i64 - 1;
        let mut var = Var { sign, weight, dscale: 0, digits };
        var.strip();
        var
    }

    /// `const_zero_point_nine`.
    fn point_nine() -> Var {
        Var { sign: NumericSign::Positive, weight: -1, dscale: 1, digits: vec![9000] }
    }

    /// `const_one_point_one`.
    fn one_point_one() -> Var {
        Var { sign: NumericSign::Positive, weight: 0, dscale: 1, digits: vec![1, 1000] }
    }

    fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// `cmp_var`: the order of two values with their signs.
    fn cmp(&self, other: &Var) -> Ordering {
        let negative = |v: &Var| !v.is_zero() && v.sign == NumericSign::Negative;
        let positive = |v: &Var| !v.is_zero() && v.sign == NumericSign::Positive;
        match (negative(self), negative(other)) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (true, true) => other.cmp_abs(self),
            (false, false) if positive(self) || positive(other) => self.cmp_abs(other),
            (false, false) => Ordering::Equal,
        }
    }

    /// `sub_var`.
    fn sub(&self, other: &Var) -> Var {
        let mut negated = other.clone();
        negated.sign = match other.sign {
            NumericSign::Positive if !other.is_zero() => NumericSign::Negative,
            _ => NumericSign::Positive,
        };
        self.add(&negated)
    }

    /// `numericvar_to_double_no_overflow`: the double nearest to the text of the value.
    fn to_f64(&self) -> f64 {
        let mut text = String::new();
        if self.sign == NumericSign::Negative {
            text.push('-');
        }
        if self.weight < 0 {
            text.push('0');
        } else {
            text.push_str(&self.digit(self.weight).to_string());
            for w in (0..self.weight).rev() {
                text.push_str(&format!("{:04}", self.digit(w)));
            }
        }
        if self.dscale > 0 {
            text.push('.');
            let end = text.len() + self.dscale as usize;
            let mut w = -1;
            while text.len() < end {
                text.push_str(&format!("{:04}", self.digit(w)));
                w -= 1;
            }
            text.truncate(end);
        }
        text.parse().unwrap_or(f64::NAN)
    }

    /// `numericvar_to_int64` for an integer: `None` when the value does not fit.
    fn to_i64(&self) -> Option<i64> {
        let mut value: i64 = 0;
        for w in (0..=self.weight).rev() {
            value = value.checked_mul(i64::from(NBASE))?.checked_add(i64::from(self.digit(w)))?;
        }
        Some(if self.sign == NumericSign::Negative { -value } else { value })
    }

    /// The digit pair `i` of the digits: two digits as one digit of base `NBASE_SQR`. The last pair of an odd number of digits has a zero after its digit.
    fn pair(&self, i: usize) -> i64 {
        let high = i64::from(self.digits[2 * i]) * i64::from(NBASE);
        high + self.digits.get(2 * i + 1).map_or(0, |&d| i64::from(d))
    }

    /// `mul_var`: the product rounded to `rscale` digits after the point. Only the digit pairs that touch the first `rscale` digits and the guard digits go into the product.
    fn mul_round(&self, other: &Var, rscale: i64) -> Var {
        let (a, b) =
            if self.digits.len() > other.digits.len() { (other, self) } else { (self, other) };
        if a.is_zero() {
            return Var::zero(rscale);
        }
        if a.digits.len() <= 6 && rscale == a.dscale + b.dscale {
            // `mul_var_short` gives the exact product.
            let mut product = a.mul(b);
            product.dscale = rscale;
            return product;
        }
        let sign = if a.sign == b.sign { NumericSign::Positive } else { NumericSign::Negative };
        let (n1, n2) = (a.digits.len() as i64, b.digits.len() as i64);
        let (mut pairs1, mut pairs2) = ((n1 + 1) / 2, (n2 + 1) / 2);
        let res_ndigits = n1 + n2;
        let mut res_pairs = res_ndigits / 2 + 1;
        let pair_offset = res_pairs - pairs1 - pairs2 + 1;
        let res_weight =
            a.weight + b.weight + 1 + 2 * res_pairs - res_ndigits - (n1 & 1) - (n2 & 1);
        let maxdigits = res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS + MUL_GUARD_DIGITS;
        res_pairs = res_pairs.min(maxdigits / 2 + 1);
        if res_pairs <= pair_offset {
            return Var::zero(rscale);
        }
        pairs1 = pairs1.min(res_pairs - pair_offset);
        pairs2 = pairs2.min(res_pairs - pair_offset);
        let right: Vec<u128> = (0..pairs2 as usize).map(|i| b.pair(i) as u128).collect();
        let mut dig = vec![0u128; res_pairs as usize];
        for i1 in 0..pairs1 {
            let left = a.pair(i1 as usize) as u128;
            if left == 0 {
                continue;
            }
            let limit = pairs2.min(res_pairs - i1 - pair_offset);
            let at = (i1 + pair_offset) as usize;
            for (slot, &r) in dig[at..].iter_mut().zip(&right[..limit as usize]) {
                *slot += left * r;
            }
        }
        let mut digits = vec![0; 2 * res_pairs as usize];
        let mut carry = 0u128;
        for (i, &d) in dig.iter().enumerate().rev() {
            let total = d + carry;
            let low = (total % NBASE_SQR as u128) as i64;
            carry = total / NBASE_SQR as u128;
            digits[2 * i] = (low / i64::from(NBASE)) as i32;
            digits[2 * i + 1] = (low % i64::from(NBASE)) as i32;
        }
        let mut product = Var { sign, weight: res_weight, dscale: 0, digits };
        product.round(rscale);
        product.strip();
        product
    }

    /// `div_var`: the quotient to `rscale` digits after the point, rounded when `round` is set and cut when it is not. A divisor of up to 12 digits, or a call with `exact` set, gives the exact quotient. A longer divisor gives the quotient that the estimates of `div_var` give.
    fn div_round(
        &self,
        other: &Var,
        rscale: i64,
        round: bool,
        exact: bool,
    ) -> Result<Var, TypeError> {
        if other.is_zero() || other.digits[0] == 0 {
            return Err(division_by_zero());
        }
        if self.is_zero() {
            return Ok(Var::zero(rscale));
        }
        if exact || other.digits.len() as i64 <= 2 * (DIV_GUARD_DIGITS + 2) {
            return Ok(self.div(other, rscale, round));
        }
        Ok(self.div_estimate(other, rscale, round))
    }

    /// The part of `div_var` that does not correct the quotient: each digit pair of the quotient is the estimate from the first two pairs of the dividend and the divisor in floating point.
    fn div_estimate(&self, other: &Var, rscale: i64, round: bool) -> Var {
        let sign =
            if self.sign == other.sign { NumericSign::Positive } else { NumericSign::Negative };
        let res_weight = self.weight - other.weight + 1;
        let mut res_ndigits = (res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS).max(1);
        if round {
            res_ndigits += 1;
        }
        res_ndigits += DIV_GUARD_DIGITS;
        let res_pairs = ((res_ndigits + 1) / 2) as usize;
        let div_pairs = res_pairs;
        let pairs1 = self.digits.len().div_ceil(2).min(div_pairs);
        let pairs2 = other.digits.len().div_ceil(2).min(div_pairs);
        let mut dividend = vec![0i64; div_pairs + 1];
        for (i, slot) in dividend.iter_mut().enumerate().take(pairs1) {
            *slot = self.pair(i);
        }
        let divisor: Vec<i64> = (0..pairs2).map(|i| other.pair(i)).collect();
        let mut fdivisor = divisor[0] as f64 * NBASE_SQR as f64;
        if pairs2 > 1 {
            fdivisor += divisor[1] as f64;
        }
        let inverse = 1.0 / fdivisor;
        let estimate = |dividend: &[i64], qi: usize| -> i32 {
            let fdividend = dividend[qi] as f64 * NBASE_SQR as f64 + dividend[qi + 1] as f64;
            let quotient = fdividend * inverse;
            if quotient >= 0.0 { quotient as i32 } else { quotient as i32 - 1 }
        };
        let limit = (i64::MAX - i64::MAX / NBASE_SQR - 1) / (NBASE_SQR - 1);
        let mut maxdiv: i64 = 1;
        for qi in 0..res_pairs {
            let mut qdigit = estimate(&dividend, qi);
            if qdigit != 0 {
                maxdiv += i64::from(qdigit).abs();
                if maxdiv > limit {
                    // Carry the digits that earlier steps changed, so that no digit overflows.
                    let mut carry = 0;
                    let mut i = (qi + pairs2 - 2).min(div_pairs - 1);
                    while i > qi {
                        let total = dividend[i] + carry;
                        carry = total.div_euclid(NBASE_SQR);
                        dividend[i] = total.rem_euclid(NBASE_SQR);
                        i -= 1;
                    }
                    dividend[qi] += carry;
                    maxdiv = 1;
                    qdigit = estimate(&dividend, qi);
                    maxdiv += i64::from(qdigit).abs();
                }
                if qdigit != 0 {
                    let stop = pairs2.min(div_pairs - qi);
                    for (slot, &d) in dividend[qi..qi + stop].iter_mut().zip(&divisor) {
                        *slot -= i64::from(qdigit) * d;
                    }
                }
            }
            // The digit can still hold a remainder: fold it into the next one. The product can overflow, and the sum then overflows back, as in C.
            dividend[qi + 1] = dividend[qi + 1].wrapping_add(dividend[qi].wrapping_mul(NBASE_SQR));
            dividend[qi] = i64::from(qdigit);
        }
        let mut digits = vec![0; 2 * res_pairs];
        let mut carry = 0;
        for i in (0..res_pairs).rev() {
            let total = dividend[i] + carry;
            carry = total.div_euclid(NBASE_SQR);
            let low = total.rem_euclid(NBASE_SQR);
            digits[2 * i] = (low / i64::from(NBASE)) as i32;
            digits[2 * i + 1] = (low % i64::from(NBASE)) as i32;
        }
        let mut quotient = Var { sign, weight: res_weight, dscale: 0, digits };
        if round {
            quotient.round(rscale);
        } else {
            quotient.trunc(rscale);
        }
        quotient.strip();
        quotient
    }

    /// `div_var_int`: the quotient by a small integer, which is exact before the rounding.
    fn div_int(&self, divisor: i64, rscale: i64) -> Var {
        self.div(&Var::int(divisor), rscale, true)
    }

    /// `sqrt_var` for a value above zero: the integer square root of the digits that the result scale needs, rounded to `rscale` digits after the point. `rscale` can be below zero.
    fn sqrt(&self, rscale: i64) -> Var {
        if self.is_zero() {
            return Var::zero(rscale);
        }
        let res_weight = self.weight.div_euclid(2);
        let res_ndigits = if rscale + 1 >= 0 {
            res_weight + 1 + (rscale + DEC_DIGITS) / DEC_DIGITS
        } else {
            res_weight + 1 - (-rscale - 1) / DEC_DIGITS
        }
        .max(1);
        let src_ndigits = (self.weight + 1 + (res_ndigits - res_weight - 1) * 2).max(1);
        let digits: Vec<i32> =
            (0..src_ndigits as usize).map(|i| self.digits.get(i).copied().unwrap_or(0)).collect();
        let mut source =
            Var { sign: NumericSign::Positive, weight: src_ndigits - 1, dscale: 0, digits };
        source.strip();
        let mut root = isqrt(&source);
        root.weight = res_weight;
        root.round(rscale);
        root.strip();
        root
    }

    /// `exp_var`: e to the power of the value, to `rscale` digits after the point.
    fn exp(&self, rscale: i64) -> Result<Var, TypeError> {
        let mut x = self.clone();
        let mut val = x.to_f64();
        if val.abs() >= EXP_LIMIT {
            if val > 0.0 {
                return Err(overflow());
            }
            return Ok(Var::zero(rscale));
        }
        let dweight = (val * LOG10_E) as i64;
        // Bring the argument near zero, so that the series converges fast.
        let mut ndiv2 = 0;
        if val.abs() > 0.01 {
            ndiv2 = 1;
            val /= 2.0;
            while val.abs() > 0.01 {
                ndiv2 += 1;
                val /= 2.0;
            }
            let local_rscale = x.dscale + ndiv2;
            x = x.div_int(1 << ndiv2, local_rscale);
        }
        let sig_digits = (1 + dweight + rscale + (ndiv2 as f64 * LOG10_2) as i64).max(0) + 8;
        let local_rscale = sig_digits - 1;
        let mut result = Var::int(1).add(&x);
        let mut elem = x.mul_round(&x, local_rscale);
        let mut ni = 2;
        elem = elem.div_int(ni, local_rscale);
        while !elem.is_zero() {
            result = result.add(&elem);
            elem = elem.mul_round(&x, local_rscale);
            ni += 1;
            elem = elem.div_int(ni, local_rscale);
        }
        for _ in 0..ndiv2 {
            let local_rscale = (sig_digits - result.weight * 2 * DEC_DIGITS).max(0);
            result = result.mul_round(&result, local_rscale);
        }
        result.round(rscale);
        Ok(result)
    }

    /// `estimate_ln_dweight`: the power of ten of the first digit of the natural logarithm. It is 0 for a value that has no logarithm.
    fn estimate_ln_dweight(&self) -> i64 {
        if self.sign != NumericSign::Positive {
            return 0;
        }
        if self.cmp(&Var::point_nine()) != Ordering::Less
            && self.cmp(&Var::one_point_one()) != Ordering::Greater
        {
            // ln(1 + x) is near x.
            let x = self.sub(&Var::int(1));
            return match x.digits.first() {
                Some(&first) => x.weight * DEC_DIGITS + f64::from(first).log10() as i64,
                None => 0,
            };
        }
        let Some(&first) = self.digits.first() else { return 0 };
        let mut digits = first;
        let mut dweight = self.weight * DEC_DIGITS;
        if let Some(&second) = self.digits.get(1) {
            digits = digits * NBASE + second;
            dweight -= DEC_DIGITS;
        }
        let ln = f64::from(digits).ln() + dweight as f64 * LN_10;
        ln.abs().log10() as i64
    }

    /// `ln_var`: the natural logarithm, to `rscale` digits after the point.
    fn ln(&self, rscale: i64) -> Result<Var, TypeError> {
        match self.cmp(&Var::zero(0)) {
            Ordering::Equal => return Err(log_error("cannot take logarithm of zero")),
            Ordering::Less => return Err(log_error("cannot take logarithm of a negative number")),
            Ordering::Greater => {}
        }
        let two = Var::int(2);
        let mut x = self.clone();
        let mut fact = two.clone();
        let mut nsqrt = 0;
        // Take square roots until the value is between 0.9 and 1.1.
        while x.cmp(&Var::point_nine()) != Ordering::Greater {
            x = x.sqrt(rscale - x.weight * DEC_DIGITS / 2 + 8);
            fact = fact.mul_round(&two, 0);
            nsqrt += 1;
        }
        while x.cmp(&Var::one_point_one()) != Ordering::Less {
            x = x.sqrt(rscale - x.weight * DEC_DIGITS / 2 + 8);
            fact = fact.mul_round(&two, 0);
            nsqrt += 1;
        }
        // The series of 0.5 * ln((1 + z) / (1 - z)) with z = (x - 1) / (x + 1).
        let local_rscale = rscale + ((nsqrt + 1) as f64 * LOG10_2) as i64 + 8;
        let one = Var::int(1);
        let mut result = x.sub(&one).div_round(&x.add(&one), local_rscale, true, false)?;
        let mut xx = result.clone();
        let x = result.mul_round(&result, local_rscale);
        let mut ni = 1;
        loop {
            ni += 2;
            xx = xx.mul_round(&x, local_rscale);
            let elem = xx.div_int(ni, local_rscale);
            if elem.is_zero() {
                break;
            }
            result = result.add(&elem);
            if elem.weight < result.weight - local_rscale * 2 / DEC_DIGITS {
                break;
            }
        }
        Ok(result.mul_round(&fact, rscale))
    }

    /// `log_var`: the logarithm of `num` in the base `self`, with the scale that `log_var` picks.
    fn log(&self, num: &Var) -> Result<Var, TypeError> {
        let ln_base_dweight = self.estimate_ln_dweight();
        let ln_num_dweight = num.estimate_ln_dweight();
        let result_dweight = ln_num_dweight - ln_base_dweight;
        let rscale = result_scale(result_dweight, &[self.dscale, num.dscale]);
        let ln_base_rscale = (rscale + result_dweight - ln_base_dweight + 8).max(0);
        let ln_num_rscale = (rscale + result_dweight - ln_num_dweight + 8).max(0);
        let ln_base = self.ln(ln_base_rscale)?;
        let ln_num = num.ln(ln_num_rscale)?;
        ln_num.div_round(&ln_base, rscale, true, false)
    }

    /// Whether the value has no digits after the point.
    fn is_integral(&self) -> bool {
        self.digits.len() as i64 <= self.weight + 1
    }

    /// `power_var`: `self` to the power of `exp`, with the scale that `power_var` picks.
    fn power(&self, exp: &Var) -> Result<Var, TypeError> {
        if exp.is_integral()
            && let Some(value) = exp.to_i64()
            && let Ok(value) = i32::try_from(value)
        {
            return self.power_int(value, exp.dscale);
        }
        if self.is_zero() {
            return Ok(Var::zero(MIN_SIG_DIGITS));
        }
        let mut base = self.clone();
        let mut negative = false;
        if self.sign == NumericSign::Negative {
            if !exp.is_integral() {
                return Err(power_error(
                    "a negative number raised to a non-integer power yields a complex result",
                ));
            }
            negative = exp.digits.len() as i64 == exp.weight + 1
                && exp.digits.last().is_some_and(|d| d & 1 == 1);
            base.sign = NumericSign::Positive;
        }
        // A first pass with about 8 digits of ln(base) estimates the weight of the result.
        let ln_dweight = base.estimate_ln_dweight();
        let local_rscale = (8 - ln_dweight).max(0);
        let ln_base = base.ln(local_rscale)?;
        let ln_num = ln_base.mul_round(exp, local_rscale);
        let mut val = ln_num.to_f64();
        if val.abs() > 2000.0 * 3.01 {
            if val > 0.0 {
                return Err(overflow());
            }
            return Ok(Var::zero(MAX_DISPLAY_SCALE));
        }
        val *= LOG10_E;
        let rscale = result_scale(val as i64, &[base.dscale, exp.dscale]);
        let sig_digits = (rscale + val as i64).max(0);
        let local_rscale = (sig_digits - ln_dweight + 8).max(0);
        let ln_base = base.ln(local_rscale)?;
        let ln_num = ln_base.mul_round(exp, local_rscale);
        let mut result = ln_num.exp(rscale)?;
        if negative && !result.is_zero() {
            result.sign = NumericSign::Negative;
        }
        Ok(result)
    }

    /// `power_var_int`: `self` to an integer power, by repeated squares.
    fn power_int(&self, exp: i32, exp_dscale: i64) -> Result<Var, TypeError> {
        // An estimate of the power of ten of the result, from the first 16 digits.
        let f = match self.digits.first() {
            Some(&first) => {
                let mut f = f64::from(first);
                let mut p = self.weight * DEC_DIGITS;
                for &d in self.digits.iter().skip(1).take(3) {
                    f = f * f64::from(NBASE) + f64::from(d);
                    p -= DEC_DIGITS;
                }
                f64::from(exp) * (f.log10() + p as f64)
            }
            None => 0.0,
        };
        if f > ((WEIGHT_MAX + 1) * DEC_DIGITS) as f64 {
            return Err(overflow());
        }
        if f + 1.0 < -(MAX_DISPLAY_SCALE as f64) {
            return Ok(Var::zero(MAX_DISPLAY_SCALE));
        }
        let rscale = result_scale(f as i64, &[self.dscale, exp_dscale]);
        match exp {
            0 => {
                let mut one = Var::int(1);
                one.dscale = rscale;
                return Ok(one);
            }
            1 => {
                let mut result = self.clone();
                result.round(rscale);
                return Ok(result);
            }
            -1 => return Var::int(1).div_round(self, rscale, true, true),
            2 => return Ok(self.mul_round(self, rscale)),
            _ => {}
        }
        if self.is_zero() {
            if exp < 0 {
                return Err(division_by_zero());
            }
            return Ok(Var::zero(rscale));
        }
        let sig_digits = 1 + rscale + f as i64 + f64::from(exp).abs().ln() as i64 + 8;
        let mut negative = exp < 0;
        let mut mask = exp.unsigned_abs();
        let mut base_prod = self.clone();
        let mut result = if mask & 1 == 1 { self.clone() } else { Var::int(1) };
        loop {
            mask >>= 1;
            if mask == 0 {
                break;
            }
            let local_rscale =
                (sig_digits - 2 * base_prod.weight * DEC_DIGITS).min(2 * base_prod.dscale).max(0);
            base_prod = base_prod.mul_round(&base_prod, local_rscale);
            if mask & 1 == 1 {
                let local_rscale = (sig_digits - (base_prod.weight + result.weight) * DEC_DIGITS)
                    .min(base_prod.dscale + result.dscale)
                    .max(0);
                result = base_prod.mul_round(&result, local_rscale);
            }
            // Past this weight the result overflows, or is zero for a negative power.
            if base_prod.weight > WEIGHT_MAX || result.weight > WEIGHT_MAX {
                if !negative {
                    return Err(overflow());
                }
                result = Var::zero(0);
                negative = false;
                break;
            }
        }
        if negative {
            return Var::int(1).div_round(&result, rscale, true, false);
        }
        result.round(rscale);
        Ok(result)
    }
}

/// The integer square root of an integer above zero, by the steps of Newton from a value above the root.
fn isqrt(n: &Var) -> Var {
    let len = n.weight + 1;
    let (top, shift) = if len % 2 == 1 {
        (i64::from(n.digit(n.weight)), (len - 1) / 2)
    } else {
        (
            i64::from(n.digit(n.weight)) * i64::from(NBASE) + i64::from(n.digit(n.weight - 1)),
            (len - 2) / 2,
        )
    };
    let mut x = Var::int((top as f64).sqrt() as i64 + 1);
    x.weight += shift;
    let two = Var::int(2);
    loop {
        let y = x.add(&n.div(&x, 0, false)).div(&two, 0, false);
        if y.cmp_abs(&x) != Ordering::Less {
            return x;
        }
        x = y;
    }
}

impl Numeric {
    fn var(&self) -> Var {
        Var::from(self)
    }

    fn is_zero(&self) -> bool {
        self.sign.is_finite() && self.digits.is_empty()
    }

    /// The sign as `numeric_sign_internal` gives it: -1, 0 or 1. `NaN` is not given here.
    fn signum(&self) -> i32 {
        match self.sign {
            NumericSign::Negative | NumericSign::NegativeInfinity => -1,
            _ if self.is_zero() => 0,
            _ => 1,
        }
    }

    /// `numeric_sqrt`.
    pub fn sqrt(&self) -> Result<Numeric, TypeError> {
        match self.sign {
            NumericSign::NegativeInfinity => {
                return Err(power_error("cannot take square root of a negative number"));
            }
            NumericSign::NaN | NumericSign::Infinity => return Ok(self.clone()),
            _ => {}
        }
        let arg = self.var();
        if arg.cmp(&Var::zero(0)) == Ordering::Less {
            return Err(power_error("cannot take square root of a negative number"));
        }
        let sweight = arg.weight * DEC_DIGITS / 2 + 1;
        let rscale = result_scale(sweight, &[arg.dscale]);
        arg.sqrt(rscale).make()
    }

    /// `numeric_exp`.
    pub fn exp(&self) -> Result<Numeric, TypeError> {
        match self.sign {
            NumericSign::NegativeInfinity => return Var::zero(0).make(),
            NumericSign::NaN | NumericSign::Infinity => return Ok(self.clone()),
            _ => {}
        }
        let arg = self.var();
        let val = (arg.to_f64() * LOG10_E).clamp(-2000.0, 2000.0);
        let rscale = result_scale(val as i64, &[arg.dscale]);
        arg.exp(rscale)?.make()
    }

    /// `numeric_ln`.
    pub fn ln(&self) -> Result<Numeric, TypeError> {
        match self.sign {
            NumericSign::NegativeInfinity => {
                return Err(log_error("cannot take logarithm of a negative number"));
            }
            NumericSign::NaN | NumericSign::Infinity => return Ok(self.clone()),
            _ => {}
        }
        let arg = self.var();
        let rscale = result_scale(arg.estimate_ln_dweight(), &[arg.dscale]);
        arg.ln(rscale)?.make()
    }

    /// `numeric_log`: the logarithm of `num` in the base `self`.
    pub fn log(&self, num: &Numeric) -> Result<Numeric, TypeError> {
        if !self.sign.is_finite() || !num.sign.is_finite() {
            if self.sign == NumericSign::NaN || num.sign == NumericSign::NaN {
                return Ok(Numeric::NAN);
            }
            if self.signum() < 0 || num.signum() < 0 {
                return Err(log_error("cannot take logarithm of a negative number"));
            }
            if self.signum() == 0 || num.signum() == 0 {
                return Err(log_error("cannot take logarithm of zero"));
            }
            if self.sign == NumericSign::Infinity {
                if num.sign == NumericSign::Infinity {
                    return Ok(Numeric::NAN);
                }
                return Var::zero(0).make();
            }
            return Ok(Numeric::INFINITY);
        }
        self.var().log(&num.var())?.make()
    }

    /// `numeric_power`: the rules of POSIX `pow` for `NaN` and the infinities.
    pub fn power(&self, exp: &Numeric) -> Result<Numeric, TypeError> {
        use NumericSign::{Infinity, NaN, NegativeInfinity};
        let one = || Var::int(1).make();
        let zero = || Var::zero(0).make();
        if !self.sign.is_finite() || !exp.sign.is_finite() {
            if self.sign == NaN {
                if exp.is_zero() {
                    return one();
                }
                return Ok(Numeric::NAN);
            }
            if exp.sign == NaN {
                if self.sign.is_finite() && self.var().cmp(&Var::int(1)) == Ordering::Equal {
                    return one();
                }
                return Ok(Numeric::NAN);
            }
            let (sign1, sign2) = (self.signum(), exp.signum());
            if sign1 == 0 && sign2 < 0 {
                return Err(power_error("zero raised to a negative power is undefined"));
            }
            if sign1 < 0 && exp.sign.is_finite() && !exp.var().is_integral() {
                return Err(power_error(
                    "a negative number raised to a non-integer power yields a complex result",
                ));
            }
            if self.sign.is_finite() && self.var().cmp(&Var::int(1)) == Ordering::Equal {
                return one();
            }
            if sign2 == 0 {
                return one();
            }
            if sign1 == 0 && sign2 > 0 {
                return zero();
            }
            if !exp.sign.is_finite() {
                let above_one = if self.sign.is_finite() {
                    let base = self.var();
                    if base.cmp(&Var::int(-1)) == Ordering::Equal {
                        return one();
                    }
                    base.cmp_abs(&Var::int(1)) == Ordering::Greater
                } else {
                    true
                };
                return if above_one == (sign2 > 0) { Ok(Numeric::INFINITY) } else { zero() };
            }
            if self.sign == Infinity {
                return if sign2 > 0 { Ok(Numeric::INFINITY) } else { zero() };
            }
            debug_assert_eq!(self.sign, NegativeInfinity);
            if sign2 < 0 {
                return zero();
            }
            let e = exp.var();
            let odd = e.digits.len() as i64 == e.weight + 1
                && e.digits.last().is_some_and(|d| d & 1 == 1);
            return Ok(if odd { Numeric::NEGATIVE_INFINITY } else { Numeric::INFINITY });
        }
        if self.signum() == 0 && exp.signum() < 0 {
            return Err(power_error("zero raised to a negative power is undefined"));
        }
        self.var().power(&exp.var())?.make()
    }
}
