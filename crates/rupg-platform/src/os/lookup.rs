//! The network interfaces and the host name lookups of the operating system, for `samehost`, `samenet` and host names in `pg_hba.conf`.
//!
//! Lifted from `crates/rudb-server/src/hba.rs` of tamnd/rudb at f5f7065a.

use std::ffi::CStr;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use rupg_common::{Error, Result, SqlState};

use super::os_text;

/// The error of a lookup, with the text of the system as the message.
fn lookup_error(text: String) -> Error {
    Error::new(SqlState::CONNECTION_EXCEPTION, text)
}

/// `gai_strerror`.
fn gai_error(code: i32) -> String {
    // SAFETY: `gai_strerror` returns a pointer to a static string for any code.
    unsafe { CStr::from_ptr(libc::gai_strerror(code)) }.to_string_lossy().into_owned()
}

/// The addresses of the interfaces of this machine with their masks, from `getifaddrs`.
pub(super) fn interfaces() -> Result<Vec<(IpAddr, IpAddr)>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `list` is a valid place for the pointer to the list.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return Err(lookup_error(os_text(&io::Error::last_os_error())));
    }
    let mut out = Vec::new();
    let mut at = list;
    while !at.is_null() {
        // SAFETY: `at` is an entry of the list that `getifaddrs` gave, which lives until `freeifaddrs` below.
        let entry = unsafe { &*at };
        // SAFETY: the address and the mask are null or valid socket addresses of the entry.
        let address = unsafe { sockaddr_ip(entry.ifa_addr) };
        if let Some(address) = address {
            // SAFETY: as above.
            let mask = unsafe { sockaddr_ip(entry.ifa_netmask) };
            let full = if address.is_ipv4() {
                IpAddr::V4(Ipv4Addr::from(u32::MAX))
            } else {
                IpAddr::V6(Ipv6Addr::from(u128::MAX))
            };
            let mask = mask.filter(|m| m.is_ipv4() == address.is_ipv4()).unwrap_or(full);
            out.push((address, mask));
        }
        at = entry.ifa_next;
    }
    // SAFETY: `list` came from `getifaddrs` and is freed once.
    unsafe { libc::freeifaddrs(list) };
    Ok(out)
}

/// The IP address of a socket address of the family `AF_INET` or `AF_INET6`.
///
/// # Safety
///
/// `address` is null or points to a valid socket address.
unsafe fn sockaddr_ip(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    // SAFETY: the caller gives a valid socket address, and the family tells its type.
    unsafe {
        match i32::from((*address).sa_family) {
            libc::AF_INET => {
                let v4 = &*address.cast::<libc::sockaddr_in>();
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
            }
            libc::AF_INET6 => {
                let v6 = &*address.cast::<libc::sockaddr_in6>();
                Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

/// The host name of an address from `getnameinfo` with `NI_NAMEREQD`.
pub(super) fn host_name(ip: IpAddr) -> Result<String> {
    let mut host = [0 as libc::c_char; 1025];
    // SAFETY: the socket address is built in full here, and `host` is valid for its length.
    let code = unsafe {
        let mut storage = std::mem::zeroed::<libc::sockaddr_storage>();
        let len = match ip {
            IpAddr::V4(v4) => {
                let sin = &mut *(&raw mut storage).cast::<libc::sockaddr_in>();
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_addr.s_addr = u32::from(v4).to_be();
                #[cfg(any(target_os = "macos", target_os = "freebsd"))]
                {
                    sin.sin_len = size_of::<libc::sockaddr_in>() as u8;
                }
                size_of::<libc::sockaddr_in>()
            }
            IpAddr::V6(v6) => {
                let sin6 = &mut *(&raw mut storage).cast::<libc::sockaddr_in6>();
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_addr.s6_addr = v6.octets();
                #[cfg(any(target_os = "macos", target_os = "freebsd"))]
                {
                    sin6.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
                }
                size_of::<libc::sockaddr_in6>()
            }
        };
        libc::getnameinfo(
            (&raw const storage).cast(),
            len as libc::socklen_t,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if code != 0 {
        return Err(lookup_error(gai_error(code)));
    }
    // SAFETY: `getnameinfo` wrote a string with its zero byte into `host`.
    Ok(unsafe { CStr::from_ptr(host.as_ptr()) }.to_string_lossy().into_owned())
}

/// The addresses of a host name from `getaddrinfo`.
pub(super) fn host_addresses(name: &str) -> Result<Vec<IpAddr>> {
    match (name, 0).to_socket_addrs() {
        Ok(addresses) => Ok(addresses.map(|a| a.ip()).collect()),
        Err(e) => {
            // The standard library puts a prefix before the text of `gai_strerror`.
            let text = e.to_string();
            let text = text.strip_prefix("failed to lookup address information: ").unwrap_or(&text);
            Err(lookup_error(text.to_string()))
        }
    }
}
