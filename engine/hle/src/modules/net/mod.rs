//! A console without a network cable: the title reaches loopback only.
//!
//! Every way out of Winsock and WinHTTP a title can import or look up by
//! name is vetted here; the rest of both DLLs is the host's own.
//!
//! * Connects (also by name, by list, `ConnectEx` from `WSAIoctl`) and
//!   datagram sends to anything but 127.0.0.0/8 and ::1 fail with
//!   `WSAENETUNREACH`.
//! * Forward lookups other than `localhost` and IP literals fail with
//!   `WSAHOST_NOT_FOUND`; reverse lookups of other addresses come back
//!   numeric, or fail if a name is required, as with no network.
//! * `WinHttpConnect` to a host that is not loopback fails with
//!   `ERROR_WINHTTP_CANNOT_CONNECT`, so no request (or WebSocket) handle can
//!   exist for one. Proxies are off and proxy auto-detection (WPAD, PAC
//!   downloads) fails with `ERROR_WINHTTP_AUTODETECTION_FAILED`.
//! * A bind to all interfaces is narrowed to loopback, so nothing on the
//!   network can connect in.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

mod lookup;
mod socket;
mod winhttp;

#[cfg(test)]
mod tests;

pub use lookup::{
    GetAddrInfoExA, GetAddrInfoExW, GetAddrInfoW, GetNameInfoW, WSAAsyncGetHostByAddr,
    WSAAsyncGetHostByName, WSALookupServiceBeginA, WSALookupServiceBeginW, getaddrinfo,
    gethostbyaddr, gethostbyname, getnameinfo,
};
pub use socket::{
    WSAConnect, WSAConnectByList, WSAConnectByNameA, WSAConnectByNameW, WSAIoctl, WSAJoinLeaf,
    WSASendMsg, WSASendTo, bind, connect, sendto,
};
pub use winhttp::{
    WinHttpConnect, WinHttpDetectAutoProxyConfigUrl, WinHttpGetProxyForUrl,
    WinHttpGetProxyForUrlEx, WinHttpGetProxyForUrlEx2, WinHttpGetProxySettingsEx, WinHttpOpen,
    WinHttpSetOption,
};

use std::ffi::{CStr, c_char, c_void};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;

use crate::modules::files::host;
use crate::{HleExport, HleModule, hle_export};

const SOCKET_ERROR: i32 = -1;
const WSAENETUNREACH: i32 = 10051;
const WSAHOST_NOT_FOUND: i32 = 11001;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;

type Socket = usize;
type Handle = *mut c_void;

/// Logs `what` for `destination` the first time (no payload, ever).
fn once(what: &'static str, destination: String) {
    static SEEN: Mutex<Vec<(&str, String)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.iter().any(|(w, d)| *w == what && *d == destination) {
        tracing::info!(%destination, "{what}");
        seen.push((what, destination));
    }
}

fn wsa_error(code: i32) {
    host!("ws2_32.dll", "WSASetLastError": extern "system" fn(i32))(code);
}

fn narrow(text: *const c_char) -> Option<String> {
    (!text.is_null()).then(|| {
        // SAFETY: the title passes a NUL-terminated string.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    })
}

fn wide(text: *const u16) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!text.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    })
}

/// An IPv4 or IPv6 `sockaddr` of `len` bytes.
fn socket_addr(addr: *const u8, len: i32) -> Option<SocketAddr> {
    let len = usize::try_from(len).ok()?;
    if addr.is_null() || len < 4 {
        return None;
    }
    // SAFETY: the title passes `len` readable bytes.
    let b = unsafe { std::slice::from_raw_parts(addr, len) };
    let port = u16::from_be_bytes([b[2], b[3]]);
    match u16::from_le_bytes([b[0], b[1]]) {
        AF_INET if len >= 16 => Some((<[u8; 4]>::try_from(&b[4..8]).ok()?, port).into()),
        AF_INET6 if len >= 28 => Some((<[u8; 16]>::try_from(&b[8..24]).ok()?, port).into()),
        _ => None,
    }
}

fn loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// `localhost` or a loopback literal (bracketed IPv6 too).
fn loopback_name(name: &str) -> bool {
    let bare = name.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost") || bare.parse().is_ok_and(loopback)
}

fn shown(addr: Option<impl ToString>) -> String {
    addr.map_or_else(|| "not an IP address".into(), |a| a.to_string())
}

/// Winsock imports by ordinal; the Winsock 1.1 numbers are fixed.
fn ordinal(name: &'static str, f: usize) -> HleExport {
    HleExport { name, address: f }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "net",
        dlls: &["ws2_32.dll", "winhttp.dll"],
        exports: vec![
            hle_export!(connect),
            hle_export!(WSAConnect),
            hle_export!(WSAConnectByNameA),
            hle_export!(WSAConnectByNameW),
            hle_export!(WSAConnectByList),
            hle_export!(WSAJoinLeaf),
            hle_export!(WSAIoctl),
            hle_export!(sendto),
            hle_export!(WSASendTo),
            hle_export!(WSASendMsg),
            hle_export!(bind),
            hle_export!(getaddrinfo),
            hle_export!(GetAddrInfoW),
            hle_export!(GetAddrInfoExA),
            hle_export!(GetAddrInfoExW),
            hle_export!(gethostbyname),
            hle_export!(WSAAsyncGetHostByName),
            hle_export!(WSALookupServiceBeginA),
            hle_export!(WSALookupServiceBeginW),
            hle_export!(gethostbyaddr),
            hle_export!(WSAAsyncGetHostByAddr),
            hle_export!(getnameinfo),
            hle_export!(GetNameInfoW),
            hle_export!(WinHttpConnect),
            hle_export!(WinHttpOpen),
            hle_export!(WinHttpSetOption),
            hle_export!(WinHttpDetectAutoProxyConfigUrl),
            hle_export!(WinHttpGetProxyForUrl),
            hle_export!(WinHttpGetProxyForUrlEx),
            hle_export!(WinHttpGetProxyForUrlEx2),
            hle_export!(WinHttpGetProxySettingsEx),
            ordinal("#2", bind as *const () as usize),
            ordinal("#4", connect as *const () as usize),
            ordinal("#20", sendto as *const () as usize),
            ordinal("#51", gethostbyaddr as *const () as usize),
            ordinal("#52", gethostbyname as *const () as usize),
            ordinal("#102", WSAAsyncGetHostByAddr as *const () as usize),
            ordinal("#103", WSAAsyncGetHostByName as *const () as usize),
        ],
    }
}
