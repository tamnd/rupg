//! The functions of the C math library that PostgreSQL calls and that the standard library of Rust computes with code of its own. `float.c` calls `cbrt` and `tgamma` of the C library, and the Rust versions can give a different last bit. So on a platform with a C library, rupg calls the same functions as the oracle on the same machine. WebAssembly has no C library, and the caller uses its own code there.

#[cfg(any(unix, windows))]
mod c {
    // SAFETY: `cbrt` and `tgamma` are pure functions of the C99 math library. They read only their argument, and they are defined for each `f64`, including NaN and the infinities.
    unsafe extern "C" {
        // The `compiler_builtins` crate of Rust defines its own `cbrt` on Linux, and the linker uses it in place of the one of glibc. glibc gives the same function the name `cbrtf64` too, and Rust does not define that name.
        #[cfg_attr(all(target_os = "linux", target_env = "gnu"), link_name = "cbrtf64")]
        pub(super) safe fn cbrt(x: f64) -> f64;
        pub(super) safe fn tgamma(x: f64) -> f64;
    }
}

/// `cbrt` of the C library.
#[cfg(any(unix, windows))]
#[must_use]
pub fn cbrt(x: f64) -> f64 {
    c::cbrt(x)
}

/// `tgamma` of the C library: the gamma function. A pole gives an infinity or NaN, and a value that is too large gives infinity.
#[cfg(any(unix, windows))]
#[must_use]
pub fn tgamma(x: f64) -> f64 {
    c::tgamma(x)
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn exact_values() {
        assert_eq!(cbrt(64.0), 4.0);
        assert_eq!(cbrt(-8.0), -2.0);
        assert_eq!(tgamma(5.0), 24.0);
        assert!(tgamma(f64::NAN).is_nan());
        assert_eq!(tgamma(f64::INFINITY), f64::INFINITY);
    }
}
