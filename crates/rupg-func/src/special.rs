//! The functions of the C math library that `float.c` calls and that the standard library of Rust does not have, or has with a different algorithm: `asinh`, `acosh`, `atanh`, `erf`, `erfc`, `lgamma` and `tgamma`.
//!
//! The code of `erf`, `erfc` and `lgamma` comes from the `libm` crate (MIT), which ports the msun library of FreeBSD. That code has this notice:
//!
//! Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//!
//! Developed at SunPro, a Sun Microsystems, Inc. business. Permission to use, copy, modify, and distribute this software is freely granted, provided that this notice is preserved.
//!
//! `tgamma` uses the Lanczos approximation of the `libm` crate. `sin`, `cos`, `exp`, `ln`, `ln_1p` and `powf` come from the standard library, which calls the C math library of the platform, as PostgreSQL does.

#![allow(
    clippy::unreadable_literal,
    clippy::excessive_precision,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap
)]

use std::f64::consts::LN_2 as LN2;

/// The high 32 bits of `x`.
fn high(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}

/// The low 32 bits of `x`.
fn low(x: f64) -> u32 {
    x.to_bits() as u32
}

/// `asinh`, as fdlibm computes it.
pub(crate) fn asinh(x: f64) -> f64 {
    let ix = high(x) & 0x7fff_ffff;
    if ix >= 0x7ff0_0000 {
        return x + x;
    }
    if ix < 0x3e30_0000 {
        return x;
    }
    let a = x.abs();
    let w = if ix > 0x41b0_0000 {
        a.ln() + LN2
    } else if ix > 0x4000_0000 {
        (2.0 * a + 1.0 / ((x * x + 1.0).sqrt() + a)).ln()
    } else {
        let t = x * x;
        (a + t / (1.0 + (1.0 + t).sqrt())).ln_1p()
    };
    if x > 0.0 { w } else { -w }
}

/// `acosh`, as fdlibm computes it. The caller rejects `x < 1`.
pub(crate) fn acosh(x: f64) -> f64 {
    let hx = high(x);
    if hx >= 0x41b0_0000 {
        if hx >= 0x7ff0_0000 {
            return x + x;
        }
        return x.ln() + LN2;
    }
    if hx == 0x3ff0_0000 && low(x) == 0 {
        return 0.0;
    }
    if hx > 0x4000_0000 {
        let t = x * x;
        return (2.0 * x - 1.0 / (x + (t - 1.0).sqrt())).ln();
    }
    let t = x - 1.0;
    (t + (2.0 * t + t * t).sqrt()).ln_1p()
}

/// `atanh`, as fdlibm computes it. The caller rejects `|x| >= 1`.
pub(crate) fn atanh(x: f64) -> f64 {
    let ix = high(x) & 0x7fff_ffff;
    if ix < 0x3e30_0000 {
        return x;
    }
    let a = x.abs();
    let t = if ix < 0x3fe0_0000 {
        let t = a + a;
        0.5 * (t + t * a / (1.0 - a)).ln_1p()
    } else {
        0.5 * ((a + a) / (1.0 - a)).ln_1p()
    };
    if x >= 0.0 { t } else { -t }
}

const ERX: f64 = 8.45062911510467529297e-01;
const EFX8: f64 = 1.02703333676410069053e+00;
const PP: [f64; 5] = [
    1.28379167095512558561e-01,
    -3.25042107247001499370e-01,
    -2.84817495755985104766e-02,
    -5.77027029648944159157e-03,
    -2.37630166566501626084e-05,
];
const QQ: [f64; 5] = [
    3.97917223959155352819e-01,
    6.50222499887672944485e-02,
    5.08130628187576562776e-03,
    1.32494738004321644526e-04,
    -3.96022827877536812320e-06,
];
const PA: [f64; 7] = [
    -2.36211856075265944077e-03,
    4.14856118683748331666e-01,
    -3.72207876035701323847e-01,
    3.18346619901161753674e-01,
    -1.10894694282396677476e-01,
    3.54783043256182359371e-02,
    -2.16637559486879084300e-03,
];
const QA: [f64; 6] = [
    1.06420880400844228286e-01,
    5.40397917702171048937e-01,
    7.18286544141962662868e-02,
    1.26171219808761642112e-01,
    1.36370839120290507362e-02,
    1.19844998467991074170e-02,
];
const RA: [f64; 8] = [
    -9.86494403484714822705e-03,
    -6.93858572707181764372e-01,
    -1.05586262253232909814e+01,
    -6.23753324503260060396e+01,
    -1.62396669462573470355e+02,
    -1.84605092906711035994e+02,
    -8.12874355063065934246e+01,
    -9.81432934416914548592e+00,
];
const SA: [f64; 8] = [
    1.96512716674392571292e+01,
    1.37657754143519042600e+02,
    4.34565877475229228821e+02,
    6.45387271733267880336e+02,
    4.29008140027567833386e+02,
    1.08635005541779435134e+02,
    6.57024977031928170135e+00,
    -6.04244152148580987438e-02,
];
const RB: [f64; 7] = [
    -9.86494292470009928597e-03,
    -7.99283237680523006574e-01,
    -1.77579549177547519889e+01,
    -1.60636384855821916062e+02,
    -6.37566443368389627722e+02,
    -1.02509513161107724954e+03,
    -4.83519191608651397019e+02,
];
const SB: [f64; 7] = [
    3.03380607434824582924e+01,
    3.25792512996573918826e+02,
    1.53672958608443695994e+03,
    3.19985821950859553908e+03,
    2.55305040643316442583e+03,
    4.74528541206955367215e+02,
    -2.24409524465858183362e+01,
];

/// The value of the polynomial with the coefficients `c`, from the lowest power, at `x`, with the scheme of Horner.
fn poly(c: &[f64], x: f64) -> f64 {
    c.iter().rev().fold(0.0, |acc, &c| acc * x + c)
}

/// `1 + x * poly(c, x)`, the denominator of a rational approximation.
fn poly1(c: &[f64], x: f64) -> f64 {
    1.0 + x * poly(c, x)
}

/// The value of the polynomial with the coefficients `c` at `x`, with the scheme of Estrin: pairs of terms, then a sum with the even powers of `x`. The C library evaluates the polynomials of `erf` in this order, and a different order can change the last bit.
fn estrin(c: &[f64], x: f64) -> f64 {
    let x2 = x * x;
    let x4 = x2 * x2;
    let powers = [1.0, x2, x4, x4 * x2, x4 * x4];
    let mut sum = 0.0;
    for (i, pair) in c.chunks(2).enumerate() {
        let term = if let [a, b] = pair { a + x * b } else { pair[0] };
        sum = if i == 0 { term } else { sum + powers[i] * term };
    }
    sum
}

/// `1 + x * c[0] + x^2 * c[1] + ...`, with the scheme of Estrin.
fn estrin1(c: &[f64], x: f64) -> f64 {
    let mut all = [0.0; 9];
    all[0] = 1.0;
    all[1..=c.len()].copy_from_slice(c);
    estrin(&all[..=c.len()], x)
}

/// `P/Q` of `erf(x) = x + x * P/Q` for `|x| < 0.84375`.
fn erf_small(x: f64) -> f64 {
    let z = x * x;
    estrin(&PP, z) / estrin1(&QQ, z)
}

/// `P1/Q1` of `erf(|x|) = ERX + P1/Q1` for `0.84375 <= |x| < 1.25`.
fn erf_middle(x: f64) -> f64 {
    let s = x.abs() - 1.0;
    estrin(&PA, s) / estrin1(&QA, s)
}

/// `erfc(|x|)` for `1.25 <= |x| < 28`.
fn erfc_large(ix: u32, x: f64) -> f64 {
    let x = x.abs();
    let s = 1.0 / (x * x);
    let (r, big_s) = if ix < 0x4006_db6d {
        (estrin(&RA, s), estrin1(&SA, s))
    } else {
        (estrin(&RB, s), estrin1(&SB, s))
    };
    let z = f64::from_bits(x.to_bits() & 0xffff_ffff_0000_0000);
    (-z * z - 0.5625).exp() * ((z - x) * (z + x) + r / big_s).exp() / x
}

/// `erf`.
pub(crate) fn erf(x: f64) -> f64 {
    let ix = high(x) & 0x7fff_ffff;
    let negative = x.is_sign_negative();
    if ix >= 0x7ff0_0000 {
        return if x.is_nan() {
            x
        } else if negative {
            -1.0
        } else {
            1.0
        };
    }
    if ix < 0x3feb_0000 {
        if ix < 0x3e30_0000 {
            return 0.125 * (8.0 * x + EFX8 * x);
        }
        return x + x * erf_small(x);
    }
    let y = if ix < 0x3ff4_0000 {
        ERX + erf_middle(x)
    } else if ix < 0x4018_0000 {
        1.0 - erfc_large(ix, x)
    } else {
        1.0 - f64::MIN_POSITIVE
    };
    if negative { -y } else { y }
}

/// `erfc`.
pub(crate) fn erfc(x: f64) -> f64 {
    let ix = high(x) & 0x7fff_ffff;
    let negative = x.is_sign_negative();
    if ix >= 0x7ff0_0000 {
        return if x.is_nan() {
            x
        } else if negative {
            2.0
        } else {
            0.0
        };
    }
    if ix < 0x3feb_0000 {
        if ix < 0x3c70_0000 {
            return 1.0 - x;
        }
        let y = erf_small(x);
        if negative || ix < 0x3fd0_0000 {
            return 1.0 - (x + x * y);
        }
        return 0.5 - (x - 0.5 + x * y);
    }
    if ix < 0x3ff4_0000 {
        return if negative { 1.0 + (ERX + erf_middle(x)) } else { 1.0 - ERX - erf_middle(x) };
    }
    if ix < 0x403c_0000 {
        return if negative { 2.0 - erfc_large(ix, x) } else { erfc_large(ix, x) };
    }
    if negative { 2.0 - f64::MIN_POSITIVE } else { 0.0 }
}

const PI: f64 = std::f64::consts::PI;
const A: [f64; 12] = [
    7.72156649015328655494e-02,
    3.22467033424113591611e-01,
    6.73523010531292681824e-02,
    2.05808084325167332806e-02,
    7.38555086081402883957e-03,
    2.89051383673415629091e-03,
    1.19270763183362067845e-03,
    5.10069792153511336608e-04,
    2.20862790713908385557e-04,
    1.08011567247583939954e-04,
    2.52144565451257326939e-05,
    4.48640949618915160150e-05,
];
const TC: f64 = 1.46163214496836224576e+00;
const TF: f64 = -1.21486290535849611461e-01;
const TT: f64 = -3.63867699703950536541e-18;
const T: [f64; 15] = [
    4.83836122723810047042e-01,
    -1.47587722994593911752e-01,
    6.46249402391333854778e-02,
    -3.27885410759859649565e-02,
    1.79706750811820387126e-02,
    -1.03142241298341437450e-02,
    6.10053870246291332635e-03,
    -3.68452016781138256760e-03,
    2.25964780900612472250e-03,
    -1.40346469989232843813e-03,
    8.81081882437654011382e-04,
    -5.38595305356740546715e-04,
    3.15632070903625950361e-04,
    -3.12754168375120860518e-04,
    3.35529192635519073543e-04,
];
const U: [f64; 6] = [
    -7.72156649015328655494e-02,
    6.32827064025093366517e-01,
    1.45492250137234768737e+00,
    9.77717527963372745603e-01,
    2.28963728064692451092e-01,
    1.33810918536787660377e-02,
];
const V: [f64; 5] = [
    2.45597793713041134822e+00,
    2.12848976379893395361e+00,
    7.69285150456672783825e-01,
    1.04222645593369134254e-01,
    3.21709242282423911810e-03,
];
const S: [f64; 7] = [
    -7.72156649015328655494e-02,
    2.14982415960608852501e-01,
    3.25778796408930981787e-01,
    1.46350472652464452805e-01,
    2.66422703033638609560e-02,
    1.84028451407337715652e-03,
    3.19475326584100867617e-05,
];
const R: [f64; 6] = [
    1.39200533467621045958e+00,
    7.21935547567138069525e-01,
    1.71933865632803078993e-01,
    1.86459191715652901344e-02,
    7.77942496381893596434e-04,
    7.32668430744625636189e-06,
];
const W: [f64; 7] = [
    4.18938533204672725052e-01,
    8.33333333333329678849e-02,
    -2.77777777728775536470e-03,
    7.93650558643019558500e-04,
    -5.95187557450339963135e-04,
    8.36339918996282139126e-04,
    -1.63092934096575273989e-03,
];

/// `sin(pi * x)` for `x > 2^-100`. The sign is arbitrary if the value is 0.
fn sin_pi(x: f64) -> f64 {
    let x = 2.0 * (x * 0.5 - (x * 0.5).floor());
    let n = ((x * 4.0) as i32 + 1).div_euclid(2);
    let x = (x - f64::from(n) * 0.5) * PI;
    match n {
        1 => x.cos(),
        2 => (-x).sin(),
        3 => -x.cos(),
        _ => x.sin(),
    }
}

/// `lgamma`: the logarithm of the absolute value of the gamma function. A pole gives infinity.
pub(crate) fn lgamma(x: f64) -> f64 {
    let ix = high(x) & 0x7fff_ffff;
    if ix >= 0x7ff0_0000 {
        return x * x;
    }
    if ix < (0x3ff - 70) << 20 {
        return -x.abs().ln();
    }
    let negative = x.is_sign_negative();
    let x = x.abs();
    let mut nadj = 0.0;
    if negative {
        let t = sin_pi(x);
        if t == 0.0 {
            return f64::INFINITY;
        }
        nadj = (PI / (t.abs() * x)).ln();
    }
    let r = if (ix == 0x3ff0_0000 || ix == 0x4000_0000) && low(x) == 0 {
        0.0
    } else if ix < 0x4000_0000 {
        let (mut r, y, i) = if ix <= 0x3fec_cccc {
            let r = -x.ln();
            if ix >= 0x3fe7_6944 {
                (r, 1.0 - x, 0)
            } else if ix >= 0x3fcd_a661 {
                (r, x - (TC - 1.0), 1)
            } else {
                (r, x, 2)
            }
        } else if ix >= 0x3ffb_b4c3 {
            (0.0, 2.0 - x, 0)
        } else if ix >= 0x3ff3_b4c4 {
            (0.0, x - TC, 1)
        } else {
            (0.0, x - 1.0, 2)
        };
        match i {
            0 => {
                let z = y * y;
                let p1 = A[0] + z * (A[2] + z * (A[4] + z * (A[6] + z * (A[8] + z * A[10]))));
                let p2 = z * (A[1] + z * (A[3] + z * (A[5] + z * (A[7] + z * (A[9] + z * A[11])))));
                r += y * p1 + p2 - 0.5 * y;
            }
            1 => {
                let z = y * y;
                let w = z * y;
                let p1 = T[0] + w * (T[3] + w * (T[6] + w * (T[9] + w * T[12])));
                let p2 = T[1] + w * (T[4] + w * (T[7] + w * (T[10] + w * T[13])));
                let p3 = T[2] + w * (T[5] + w * (T[8] + w * (T[11] + w * T[14])));
                r += TF + (z * p1 - (TT - w * (p2 + y * p3)));
            }
            _ => {
                let p1 = y * poly(&U, y);
                let p2 = poly1(&V, y);
                r += -0.5 * y + p1 / p2;
            }
        }
        r
    } else if ix < 0x4020_0000 {
        let i = x as i32;
        let y = x - f64::from(i);
        let p = y * poly(&S, y);
        let q = poly1(&R, y);
        let mut r = 0.5 * y + p / q;
        let mut z = 1.0;
        for k in (2..i).rev() {
            z *= y + f64::from(k);
        }
        if i >= 3 {
            r += z.ln();
        }
        r
    } else if ix < 0x4390_0000 {
        let t = x.ln();
        let z = 1.0 / x;
        let y = z * z;
        let w = W[0] + z * (W[1] + y * (W[2] + y * (W[3] + y * (W[4] + y * (W[5] + y * W[6])))));
        (x - 0.5) * (t - 1.0) + w
    } else {
        x * (x.ln() - 1.0)
    };
    if negative { nadj - r } else { r }
}

const GMHALF: f64 = 5.524680040776729583740234375;
const SNUM: [f64; 13] = [
    23531376880.410759688572007674451636754734846804940,
    42919803642.649098768957899047001988850926355848959,
    35711959237.355668049440185451547166705960488635843,
    17921034426.037209699919755754458931112671403265390,
    6039542586.3520280050642916443072979210699388420708,
    1439720407.3117216736632230727949123939715485786772,
    248874557.86205415651146038641322942321632125127801,
    31426415.585400194380614231628318205362874684987640,
    2876370.6289353724412254090516208496135991145378768,
    186056.26539522349504029498971604569928220784236328,
    8071.6720023658162106380029022722506138218516325024,
    210.82427775157934587250973392071336271166969580291,
    2.5066282746310002701649081771338373386264310793408,
];
const SDEN: [f64; 13] = [
    0.0,
    39916800.0,
    120543840.0,
    150917976.0,
    105258076.0,
    45995730.0,
    13339535.0,
    2637558.0,
    357423.0,
    32670.0,
    1925.0,
    66.0,
    1.0,
];

/// The rational function of the Lanczos approximation for a positive `x`.
fn lanczos(x: f64) -> f64 {
    let (mut num, mut den) = (0.0, 0.0);
    if x < 8.0 {
        for i in (0..SNUM.len()).rev() {
            num = num * x + SNUM[i];
            den = den * x + SDEN[i];
        }
    } else {
        for i in 0..SNUM.len() {
            num = num / x + SNUM[i];
            den = den / x + SDEN[i];
        }
    }
    num / den
}

/// `tgamma`: the gamma function. A pole gives NaN, and a value that is too large gives infinity.
pub(crate) fn tgamma(x: f64) -> f64 {
    let ix = high(x) & 0x7fff_ffff;
    let negative = x.is_sign_negative();
    if ix >= 0x7ff0_0000 {
        return x + f64::INFINITY;
    }
    if ix < (0x3ff - 54) << 20 {
        return 1.0 / x;
    }
    if x == x.floor() {
        if negative {
            return f64::NAN;
        }
        if x <= 23.0 {
            let mut f = 1.0;
            for k in 2..x as u32 {
                f *= f64::from(k);
            }
            return f;
        }
    }
    if ix >= 0x4067_0000 {
        if negative {
            return if x.floor() * 0.5 == (x * 0.5).floor() { 0.0 } else { -0.0 };
        }
        return f64::INFINITY;
    }
    let absx = x.abs();
    let mut y = absx + GMHALF;
    let mut dy = if absx > GMHALF { y - absx - GMHALF } else { y - GMHALF - absx };
    let mut z = absx - 0.5;
    let mut r = lanczos(absx) * (-y).exp();
    if negative {
        r = -PI / (sin_pi(absx) * absx * r);
        dy = -dy;
        z = -z;
    }
    r += dy * (GMHALF + 0.5) * r / y;
    let z = y.powf(0.5 * z);
    y = r * z * z;
    y
}
