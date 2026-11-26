use std::ffi::{c_char, c_void};
use std::net::IpAddr;

use crate::modules::files::host;

use super::{
    AF_INET, AF_INET6, Handle, SOCKET_ERROR, WSAHOST_NOT_FOUND, loopback, narrow, once, shown,
    socket_addr, wide, wsa_error,
};

const WSASERVICE_NOT_FOUND: i32 = 10108;
const NI_NUMERICHOST: i32 = 0x02;
pub(super) const NI_NAMEREQD: i32 = 0x04;

/// A raw address of family `kind` (`gethostbyaddr` style).
fn raw_ip(addr: *const c_char, len: i32, kind: i32) -> Option<IpAddr> {
    let len = usize::try_from(len).ok()?;
    if addr.is_null() {
        return None;
    }
    // SAFETY: the title passes `len` readable bytes.
    let b = unsafe { std::slice::from_raw_parts(addr.cast::<u8>(), len) };
    match (u16::try_from(kind).ok()?, len) {
        (AF_INET, 4..) => Some(<[u8; 4]>::try_from(&b[..4]).ok()?.into()),
        (AF_INET6, 16..) => Some(<[u8; 16]>::try_from(&b[..16]).ok()?.into()),
        _ => None,
    }
}

/// Names answered without asking the network: none, `localhost`, IP
/// literals (connecting to one is vetted separately).
fn resolvable(name: Option<String>) -> bool {
    match name {
        Some(name)
            if !(name.is_empty()
                || name.eq_ignore_ascii_case("localhost")
                || name.parse::<IpAddr>().is_ok()) =>
        {
            once("network name lookup blocked", name);
            wsa_error(WSAHOST_NOT_FOUND);
            false
        }
        _ => true,
    }
}

/// Reverse lookups ask DNS unless the address is loopback.
fn reverse_resolvable(ip: Option<IpAddr>) -> bool {
    match ip {
        Some(ip) if loopback(ip) => true,
        ip => {
            once("network reverse lookup blocked", shown(ip));
            false
        }
    }
}

/// `getnameinfo` flags that keep a host name lookup on the machine: the
/// numeric form, as without a network, or `None` if a name is required.
fn name_info_flags(sa: *const u8, len: i32, host_wanted: bool, flags: i32) -> Option<i32> {
    if !host_wanted
        || flags & NI_NUMERICHOST != 0
        || reverse_resolvable(socket_addr(sa, len).map(|a| a.ip()))
    {
        Some(flags)
    } else if flags & NI_NAMEREQD != 0 {
        wsa_error(WSAHOST_NOT_FOUND);
        None
    } else {
        Some(flags | NI_NUMERICHOST)
    }
}

// Forward lookups.

fn blocked_lookup(result: *mut *mut c_void) -> i32 {
    if !result.is_null() {
        // SAFETY: a non-null result points to the caller's output slot.
        unsafe { result.write(std::ptr::null_mut()) };
    }
    WSAHOST_NOT_FOUND
}

pub extern "system" fn getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const c_void,
    result: *mut *mut c_void,
) -> i32 {
    if !resolvable(narrow(node)) {
        return blocked_lookup(result);
    }
    host!("ws2_32.dll", "getaddrinfo": extern "system" fn(*const c_char, *const c_char, *const c_void, *mut *mut c_void) -> i32)(
        node, service, hints, result,
    )
}

pub extern "system" fn GetAddrInfoW(
    node: *const u16,
    service: *const u16,
    hints: *const c_void,
    result: *mut *mut c_void,
) -> i32 {
    if !resolvable(wide(node)) {
        return blocked_lookup(result);
    }
    host!("ws2_32.dll", "GetAddrInfoW": extern "system" fn(*const u16, *const u16, *const c_void, *mut *mut c_void) -> i32)(
        node, service, hints, result,
    )
}

pub extern "system" fn GetAddrInfoExA(
    name: *const c_char,
    service: *const c_char,
    namespace: u32,
    provider: *const c_void,
    hints: *const c_void,
    result: *mut *mut c_void,
    timeout: *const c_void,
    overlapped: *mut c_void,
    completion: *const c_void,
    handle: *mut Handle,
) -> i32 {
    if !resolvable(narrow(name)) {
        return blocked_lookup(result);
    }
    host!("ws2_32.dll", "GetAddrInfoExA": extern "system" fn(*const c_char, *const c_char, u32, *const c_void, *const c_void, *mut *mut c_void, *const c_void, *mut c_void, *const c_void, *mut Handle) -> i32)(
        name, service, namespace, provider, hints, result, timeout, overlapped, completion, handle,
    )
}

pub extern "system" fn GetAddrInfoExW(
    name: *const u16,
    service: *const u16,
    namespace: u32,
    provider: *const c_void,
    hints: *const c_void,
    result: *mut *mut c_void,
    timeout: *const c_void,
    overlapped: *mut c_void,
    completion: *const c_void,
    handle: *mut Handle,
) -> i32 {
    if !resolvable(wide(name)) {
        return blocked_lookup(result);
    }
    host!("ws2_32.dll", "GetAddrInfoExW": extern "system" fn(*const u16, *const u16, u32, *const c_void, *const c_void, *mut *mut c_void, *const c_void, *mut c_void, *const c_void, *mut Handle) -> i32)(
        name, service, namespace, provider, hints, result, timeout, overlapped, completion, handle,
    )
}

pub extern "system" fn gethostbyname(name: *const c_char) -> *mut c_void {
    if !resolvable(narrow(name)) {
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "gethostbyname": extern "system" fn(*const c_char) -> *mut c_void)(name)
}

pub extern "system" fn WSAAsyncGetHostByName(
    window: Handle,
    message: u32,
    name: *const c_char,
    buf: *mut c_char,
    buf_len: i32,
) -> Handle {
    if !resolvable(narrow(name)) {
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "WSAAsyncGetHostByName": extern "system" fn(Handle, u32, *const c_char, *mut c_char, i32) -> Handle)(
        window, message, name, buf, buf_len,
    )
}

/// Name-space queries (DNS, also network lists and Bluetooth) are refused
/// as a console without a network answers them.
// shortcut: local name spaces are refused too; pass those through if a title
// needs one.
pub extern "system" fn WSALookupServiceBeginA(
    _restrictions: *const c_void,
    _flags: u32,
    _lookup: *mut Handle,
) -> i32 {
    once("network name-space query blocked", "any".into());
    wsa_error(WSASERVICE_NOT_FOUND);
    SOCKET_ERROR
}

/// See [`WSALookupServiceBeginA`].
pub extern "system" fn WSALookupServiceBeginW(
    restrictions: *const c_void,
    flags: u32,
    lookup: *mut Handle,
) -> i32 {
    WSALookupServiceBeginA(restrictions, flags, lookup)
}

// Reverse lookups.

pub extern "system" fn gethostbyaddr(addr: *const c_char, len: i32, kind: i32) -> *mut c_void {
    if !reverse_resolvable(raw_ip(addr, len, kind)) {
        wsa_error(WSAHOST_NOT_FOUND);
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "gethostbyaddr": extern "system" fn(*const c_char, i32, i32) -> *mut c_void)(
        addr, len, kind,
    )
}

pub extern "system" fn WSAAsyncGetHostByAddr(
    window: Handle,
    message: u32,
    addr: *const c_char,
    len: i32,
    kind: i32,
    buf: *mut c_char,
    buf_len: i32,
) -> Handle {
    if !reverse_resolvable(raw_ip(addr, len, kind)) {
        wsa_error(WSAHOST_NOT_FOUND);
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "WSAAsyncGetHostByAddr": extern "system" fn(Handle, u32, *const c_char, i32, i32, *mut c_char, i32) -> Handle)(
        window, message, addr, len, kind, buf, buf_len,
    )
}

pub extern "system" fn getnameinfo(
    sa: *const u8,
    sa_len: i32,
    host: *mut c_char,
    host_len: u32,
    serv: *mut c_char,
    serv_len: u32,
    flags: i32,
) -> i32 {
    let Some(flags) = name_info_flags(sa, sa_len, !host.is_null(), flags) else {
        return WSAHOST_NOT_FOUND;
    };
    host!("ws2_32.dll", "getnameinfo": extern "system" fn(*const u8, i32, *mut c_char, u32, *mut c_char, u32, i32) -> i32)(
        sa, sa_len, host, host_len, serv, serv_len, flags,
    )
}

pub extern "system" fn GetNameInfoW(
    sa: *const u8,
    sa_len: i32,
    host: *mut u16,
    host_len: u32,
    serv: *mut u16,
    serv_len: u32,
    flags: i32,
) -> i32 {
    let Some(flags) = name_info_flags(sa, sa_len, !host.is_null(), flags) else {
        return WSAHOST_NOT_FOUND;
    };
    host!("ws2_32.dll", "GetNameInfoW": extern "system" fn(*const u8, i32, *mut u16, u32, *mut u16, u32, i32) -> i32)(
        sa, sa_len, host, host_len, serv, serv_len, flags,
    )
}
