//! The `half` of pgvector, an IEEE 754 binary16: the conversions of `src/halfutils.h` between `half` and `float`, with the bits of a `half` in a `u16`.

/// `HalfToFloat4`: the `float` of a `half`, which is exact.
pub(super) fn to_f32(bin: u16) -> f32 {
    let mut exponent = u32::from((bin & 0x7C00) >> 10);
    let mut mantissa = u32::from(bin & 0x03FF);
    // Sign
    let mut result = u32::from(bin & 0x8000) << 16;
    if exponent == 31 {
        if mantissa == 0 {
            // Infinite
            result |= 0x7F80_0000;
        } else {
            // NaN
            result |= 0x7FC0_0000;
        }
    } else if exponent == 0 {
        // Subnormal
        if mantissa != 0 {
            let mut e: i32 = -14;
            for _ in 0..10 {
                mantissa <<= 1;
                e -= 1;
                if (mantissa >> 10) % 2 == 1 {
                    mantissa &= 0x03ff;
                    break;
                }
            }
            result |= ((e + 127) as u32) << 23;
        }
    } else {
        // Normal
        exponent += 127 - 15;
        result |= exponent << 23;
    }
    result |= mantissa << 13;
    f32::from_bits(result)
}

/// `Float4ToHalfUnchecked`: the `half` nearest to a `float`, with ties to even, and an infinity for a value out of the range of `half`.
pub(super) fn from_f32(num: f32) -> u16 {
    let bin = num.to_bits();
    let mut exponent = ((bin & 0x7F80_0000) >> 23) as i32;
    let mut mantissa = (bin & 0x007F_FFFF) as i32;
    // Sign
    #[allow(clippy::cast_possible_truncation)]
    let mut result = ((bin & 0x8000_0000) >> 16) as u16;
    if num.is_infinite() {
        result |= 0x7C00;
    } else if num.is_nan() {
        result |= 0x7E00;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            result |= (mantissa >> 13) as u16;
        }
    } else if exponent > 98 {
        exponent -= 127;
        let mut s = mantissa & 0x0000_0FFF;
        // Subnormal
        if exponent < -14 {
            let diff = -exponent - 14;
            mantissa >>= diff;
            mantissa += 1 << (23 - diff);
            s |= mantissa & 0x0000_0FFF;
        }
        let mut m = mantissa >> 13;
        // Round
        let gr = (mantissa >> 12) % 4;
        if gr == 3 || (gr == 1 && s != 0) {
            m += 1;
        }
        if m == 1024 {
            m = 0;
            exponent += 1;
        }
        if exponent > 15 {
            // Infinite
            result |= 0x7C00;
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                if exponent >= -14 {
                    result |= ((exponent + 15) << 10) as u16;
                }
                result |= m as u16;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for (v, h) in [
            (1.0f32, 0x3C00u16),
            (-2.0, 0xC000),
            (0.5, 0x3800),
            (65504.0, 0x7BFF),
            (65520.0, 0x7C00),
            (6.0e-8, 0x0001),
            (2.0e-8, 0x0000),
            (0.0, 0x0000),
            (-0.0, 0x8000),
            (1.0009766, 0x3C01),
            (1.000_488_3, 0x3C00),
            (1.001_464_8, 0x3C02),
            (f32::INFINITY, 0x7C00),
        ] {
            assert_eq!(from_f32(v), h, "{v}");
        }
        assert!(to_f32(from_f32(f32::NAN)).is_nan());
        for h in 0..0x7C00u16 {
            assert_eq!(from_f32(to_f32(h)), h);
            assert_eq!(from_f32(-to_f32(h)), h | 0x8000);
        }
        assert_eq!(to_f32(0x0001), 2.0f32.powi(-24));
        assert_eq!(to_f32(0x03FF), 1023.0 * 2.0f32.powi(-24));
    }
}
