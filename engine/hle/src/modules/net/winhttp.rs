use std::ffi::c_void;

use windows_sys::Win32::Foundation::SetLastError;

use crate::modules::files::host;

use super::{Handle, loopback_name, once, wide};

pub(super) const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;
pub(super) const ERROR_WINHTTP_AUTODETECTION_FAILED: u32 = 12180;
const WINHTTP_ACCESS_TYPE_NO_PROXY: u32 = 1;
const WINHTTP_OPTION_PROXY: u32 = 38;

/// The one door to a server: requests, uploads and WebSockets all hang off
/// the handle this returns.
pub extern "system" fn WinHttpConnect(
    session: Handle,
    server: *const u16,
    port: u16,
    reserved: u32,
) -> Handle {
    if let Some(name) = wide(server)
        && !loopback_name(&name)
    {
        once("HTTP connection blocked", format!("{name}:{port}"));
        // SAFETY: no pointers.
        unsafe { SetLastError(ERROR_WINHTTP_CANNOT_CONNECT) };
        return std::ptr::null_mut();
    }
    host!("winhttp.dll", "WinHttpConnect": extern "system" fn(Handle, *const u16, u16, u32) -> Handle)(
        session, server, port, reserved,
    )
}

/// Sessions never use a proxy: only loopback is reachable, and a proxy
/// (named, or found by auto-detection) would carry it off the machine.
pub extern "system" fn WinHttpOpen(
    agent: *const u16,
    access: u32,
    _proxy: *const u16,
    _bypass: *const u16,
    flags: u32,
) -> Handle {
    if access != WINHTTP_ACCESS_TYPE_NO_PROXY {
        once("HTTP proxy ignored", format!("access type {access}"));
    }
    host!("winhttp.dll", "WinHttpOpen": extern "system" fn(*const u16, u32, *const u16, *const u16, u32) -> Handle)(
        agent,
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        std::ptr::null(),
        std::ptr::null(),
        flags,
    )
}

/// See [`WinHttpOpen`]: a proxy set later is accepted and not used.
pub extern "system" fn WinHttpSetOption(
    handle: Handle,
    option: u32,
    buffer: *const c_void,
    len: u32,
) -> i32 {
    if option == WINHTTP_OPTION_PROXY {
        once("HTTP proxy ignored", "WINHTTP_OPTION_PROXY".into());
        return 1;
    }
    host!("winhttp.dll", "WinHttpSetOption": extern "system" fn(Handle, u32, *const c_void, u32) -> i32)(
        handle, option, buffer, len,
    )
}

/// Proxy auto-detection asks the network (DHCP, DNS, a PAC download).
fn no_auto_proxy() -> u32 {
    once("HTTP proxy auto-detection blocked", "WPAD/PAC".into());
    ERROR_WINHTTP_AUTODETECTION_FAILED
}

pub extern "system" fn WinHttpDetectAutoProxyConfigUrl(_flags: u32, _url: *mut *mut u16) -> i32 {
    // SAFETY: no pointers.
    unsafe { SetLastError(no_auto_proxy()) };
    0
}

pub extern "system" fn WinHttpGetProxyForUrl(
    _session: Handle,
    _url: *const u16,
    _options: *const c_void,
    _info: *mut c_void,
) -> i32 {
    // SAFETY: no pointers.
    unsafe { SetLastError(no_auto_proxy()) };
    0
}

pub extern "system" fn WinHttpGetProxyForUrlEx(
    _resolver: Handle,
    _url: *const u16,
    _options: *const c_void,
    _context: usize,
) -> u32 {
    no_auto_proxy()
}

pub extern "system" fn WinHttpGetProxyForUrlEx2(
    _resolver: Handle,
    _url: *const u16,
    _options: *const c_void,
    _selection_len: u32,
    _selection: *const u8,
    _context: usize,
) -> u32 {
    no_auto_proxy()
}

pub extern "system" fn WinHttpGetProxySettingsEx(
    _resolver: Handle,
    _kind: i32,
    _params: *const c_void,
    _context: usize,
) -> u32 {
    no_auto_proxy()
}
