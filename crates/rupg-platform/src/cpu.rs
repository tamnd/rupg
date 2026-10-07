//! CPU feature detection and the size of the physical memory.
//!
//! The kernels have an AVX-512 path, an AVX2 path, a NEON path, a 128-bit WebAssembly path and a portable path, chosen at run time (spec/04 section 4.10). [`CpuFeatures::detect`] reads the features once. [`CpuLevel`] is the path that the kernels use. A test can ask for a lower level than the host has, so that it runs every path that the host can run on the same input (spec/21 section 21.5).

use std::fmt;
use std::sync::OnceLock;

/// A kernel path, from the most portable to the widest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CpuLevel {
    /// Plain Rust with no target feature.
    Portable,
    /// The 128-bit SIMD of WebAssembly.
    Simd128,
    /// NEON on aarch64.
    Neon,
    /// AVX2 with BMI2 and POPCNT on x86-64.
    Avx2,
    /// AVX-512 F, BW and VL on x86-64.
    Avx512,
}

impl CpuLevel {
    /// The levels in order.
    pub const ALL: [CpuLevel; 5] =
        [CpuLevel::Portable, CpuLevel::Simd128, CpuLevel::Neon, CpuLevel::Avx2, CpuLevel::Avx512];

    /// The name, as in the setting and the test output.
    pub fn name(self) -> &'static str {
        match self {
            CpuLevel::Portable => "portable",
            CpuLevel::Simd128 => "simd128",
            CpuLevel::Neon => "neon",
            CpuLevel::Avx2 => "avx2",
            CpuLevel::Avx512 => "avx512",
        }
    }

    /// The level with this name.
    pub fn from_name(name: &str) -> Option<CpuLevel> {
        CpuLevel::ALL.into_iter().find(|l| l.name() == name)
    }
}

impl fmt::Display for CpuLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The features of the host CPU that the kernels use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct CpuFeatures {
    /// x86-64 SSE4.2, which has the CRC32C instruction.
    pub sse42: bool,
    /// x86-64 POPCNT.
    pub popcnt: bool,
    /// x86-64 AVX2.
    pub avx2: bool,
    /// x86-64 BMI2, for `pext` and `pdep`.
    pub bmi2: bool,
    /// x86-64 AVX-512 F.
    pub avx512f: bool,
    /// x86-64 AVX-512 BW, for byte and word lanes.
    pub avx512bw: bool,
    /// x86-64 AVX-512 VL, for the 128-bit and 256-bit forms.
    pub avx512vl: bool,
    /// aarch64 NEON. Every aarch64 CPU that Rust supports has it.
    pub neon: bool,
    /// aarch64 CRC32, which has the CRC32C instruction.
    pub crc: bool,
    /// The WebAssembly `simd128` feature, set at compile time.
    pub simd128: bool,
}

impl CpuFeatures {
    /// The features of the host. The first call reads them and the next calls give the same value.
    pub fn detect() -> CpuFeatures {
        static FEATURES: OnceLock<CpuFeatures> = OnceLock::new();
        *FEATURES.get_or_init(CpuFeatures::read)
    }

    #[cfg(target_arch = "x86_64")]
    fn read() -> CpuFeatures {
        CpuFeatures {
            sse42: std::is_x86_feature_detected!("sse4.2"),
            popcnt: std::is_x86_feature_detected!("popcnt"),
            avx2: std::is_x86_feature_detected!("avx2"),
            bmi2: std::is_x86_feature_detected!("bmi2"),
            avx512f: std::is_x86_feature_detected!("avx512f"),
            avx512bw: std::is_x86_feature_detected!("avx512bw"),
            avx512vl: std::is_x86_feature_detected!("avx512vl"),
            ..CpuFeatures::default()
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn read() -> CpuFeatures {
        CpuFeatures {
            neon: std::arch::is_aarch64_feature_detected!("neon"),
            crc: std::arch::is_aarch64_feature_detected!("crc"),
            ..CpuFeatures::default()
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn read() -> CpuFeatures {
        CpuFeatures { simd128: cfg!(target_feature = "simd128"), ..CpuFeatures::default() }
    }

    /// True if the host can run the kernels of `level`.
    pub fn supports(&self, level: CpuLevel) -> bool {
        match level {
            CpuLevel::Portable => true,
            CpuLevel::Simd128 => self.simd128,
            CpuLevel::Neon => self.neon,
            CpuLevel::Avx2 => self.avx2 && self.bmi2 && self.popcnt,
            CpuLevel::Avx512 => {
                self.supports(CpuLevel::Avx2) && self.avx512f && self.avx512bw && self.avx512vl
            }
        }
    }

    /// The widest level that the host can run.
    pub fn best(&self) -> CpuLevel {
        CpuLevel::ALL.into_iter().rev().find(|&l| self.supports(l)).unwrap_or(CpuLevel::Portable)
    }

    /// Each level that the host can run, from the most portable.
    pub fn levels(&self) -> Vec<CpuLevel> {
        CpuLevel::ALL.into_iter().filter(|&l| self.supports(l)).collect()
    }
}

/// The size of the physical memory in bytes, if the platform tells it.
#[cfg(unix)]
pub fn physical_memory() -> Option<u64> {
    // SAFETY: sysconf reads a system value and has no pointer argument.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    // SAFETY: as above.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page <= 0 {
        return None;
    }
    u64::try_from(pages).ok()?.checked_mul(u64::try_from(page).ok()?)
}

/// The size of the physical memory in bytes, if the platform tells it.
#[cfg(not(unix))]
pub fn physical_memory() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        let host = CpuFeatures::detect();
        assert_eq!(host, CpuFeatures::detect());
        assert!(host.supports(CpuLevel::Portable));
        assert!(host.levels().contains(&host.best()));
        if cfg!(target_arch = "aarch64") {
            assert_eq!(host.best(), CpuLevel::Neon);
        }
        assert_eq!(CpuFeatures::default().best(), CpuLevel::Portable);
        let avx512 = CpuFeatures {
            avx2: true,
            bmi2: true,
            popcnt: true,
            avx512f: true,
            avx512bw: true,
            avx512vl: true,
            ..CpuFeatures::default()
        };
        assert_eq!(avx512.best(), CpuLevel::Avx512);
        assert_eq!(avx512.levels(), [CpuLevel::Portable, CpuLevel::Avx2, CpuLevel::Avx512]);
        // AVX-512 with no BW is not enough for the byte kernels.
        assert_eq!(CpuFeatures { avx512bw: false, ..avx512 }.best(), CpuLevel::Avx2);
        for level in CpuLevel::ALL {
            assert_eq!(CpuLevel::from_name(level.name()), Some(level));
        }
        assert_eq!(CpuLevel::from_name("sse2"), None);
    }

    #[test]
    fn memory() {
        if cfg!(unix) {
            let m = physical_memory().unwrap();
            assert!(m >= 256 << 20, "{m}");
        }
    }
}
