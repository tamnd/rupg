//! The locales of the C library, which `pg_import_system_collations` makes collations for.

use std::process::Command;

/// The locale names that `locale -a` prints, in its order. The list is empty when the command does not run, as `pg_import_system_collations` allows on a system without the command.
pub fn system_locales() -> Vec<String> {
    let Ok(output) = Command::new("locale").arg("-a").output() else { return Vec::new() };
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_string).collect()
}

/// The `CODESET` of `nl_langinfo_l` for the `LC_CTYPE` of a locale, as `pg_get_encoding_from_locale` reads it. `None` when the C library does not know the locale.
#[cfg(target_os = "linux")]
pub fn locale_codeset(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let base = std::ptr::null_mut();
    // SAFETY: `name` is a string that ends with a NUL, and a null base asks for a new locale object.
    let locale = unsafe { libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), base) };
    if locale.is_null() {
        return None;
    }
    // SAFETY: `locale` is a valid locale object until `freelocale` below.
    let codeset = unsafe { libc::nl_langinfo_l(libc::CODESET, locale) };
    let text = (!codeset.is_null()).then(|| {
        // SAFETY: `codeset` is not null and points to a string with a NUL at its end, which lives until `freelocale` below.
        unsafe { std::ffi::CStr::from_ptr(codeset) }.to_string_lossy().into_owned()
    });
    // SAFETY: `locale` came from `newlocale`, and nothing uses it after this call.
    unsafe { libc::freelocale(locale) };
    text
}

/// The `CODESET` of a locale. Only Linux is done, so the other systems give `None` and get no collations of the C library.
#[cfg(not(target_os = "linux"))]
pub fn locale_codeset(_name: &str) -> Option<String> {
    None
}

/// `gnu_get_libc_version`: the version of the GNU C library, which is the version of each collation of the C library. `None` for another C library.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn libc_version() -> Option<String> {
    // SAFETY: `gnu_get_libc_version` returns a pointer to a static string with a NUL at its end.
    let version = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) };
    Some(version.to_string_lossy().into_owned())
}

/// The version of the C library. Only the GNU C library has one.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn libc_version() -> Option<String> {
    None
}
