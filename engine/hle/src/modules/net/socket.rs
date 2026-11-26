use std::ffi::{c_char, c_void};
use std::net::{IpAddr, SocketAddr};

use crate::modules::files::host;

use super::{
    SOCKET_ERROR, Socket, WSAENETUNREACH, WSAHOST_NOT_FOUND, loopback, loopback_name, narrow, once,
    shown, socket_addr, wide, wsa_error,
};

pub(super) const INVALID_SOCKET: Socket = !0;
const WSAEINVAL: i32 = 10022;
const WSAEADDRNOTAVAIL: i32 = 10049;
pub(super) const SIO_GET_EXTENSION_FUNCTION_POINTER: u32 = 0xC800_0006;
const SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER: u32 = 0xC800_0024;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Guid(u32, u16, u16, [u8; 8]);

pub(super) const WSAID_CONNECTEX: Guid = Guid(
    0x25a2_07b9,
    0xddf3,
    0x4660,
    [0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e],
);
const WSAID_WSASENDMSG: Guid = Guid(
    0xa441_e712,
    0x754f,
    0x43ca,
    [0x84, 0xa7, 0x0d, 0xee, 0x44, 0xcf, 0x60, 0x6d],
);

/// True for a loopback `sockaddr`; otherwise logs it and sets
/// `WSAENETUNREACH`. Other address families count as off the machine.
fn reachable(what: &'static str, addr: *const u8, len: i32) -> bool {
    match socket_addr(addr, len) {
        Some(a) if loopback(a.ip()) => true,
        a => {
            once(what, shown(a));
            wsa_error(WSAENETUNREACH);
            false
        }
    }
}

/// Connecting by name: only to loopback. A name would need a lookup
/// (`WSAHOST_NOT_FOUND`), an address a route (`WSAENETUNREACH`).
fn reachable_name(name: Option<String>) -> bool {
    match name {
        Some(name) if !loopback_name(&name) => {
            let literal = name.parse::<IpAddr>().is_ok();
            once("network connect blocked", name);
            wsa_error(if literal {
                WSAENETUNREACH
            } else {
                WSAHOST_NOT_FOUND
            });
            false
        }
        // No name: the host rejects the call itself.
        _ => true,
    }
}

// Connects.

pub extern "system" fn connect(s: Socket, name: *const u8, len: i32) -> i32 {
    if !reachable("network connect blocked", name, len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "connect": extern "system" fn(Socket, *const u8, i32) -> i32)(s, name, len)
}

pub extern "system" fn WSAConnect(
    s: Socket,
    name: *const u8,
    len: i32,
    caller_data: *const c_void,
    callee_data: *mut c_void,
    sqos: *const c_void,
    gqos: *const c_void,
) -> i32 {
    if !reachable("network connect blocked", name, len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "WSAConnect": extern "system" fn(Socket, *const u8, i32, *const c_void, *mut c_void, *const c_void, *const c_void) -> i32)(
        s,
        name,
        len,
        caller_data,
        callee_data,
        sqos,
        gqos,
    )
}

pub extern "system" fn WSAConnectByNameA(
    s: Socket,
    node: *const c_char,
    service: *const c_char,
    local_len: *mut u32,
    local: *mut c_void,
    remote_len: *mut u32,
    remote: *mut c_void,
    timeout: *const c_void,
    reserved: *mut c_void,
) -> i32 {
    if !reachable_name(narrow(node)) {
        return 0;
    }
    host!("ws2_32.dll", "WSAConnectByNameA": extern "system" fn(Socket, *const c_char, *const c_char, *mut u32, *mut c_void, *mut u32, *mut c_void, *const c_void, *mut c_void) -> i32)(
        s, node, service, local_len, local, remote_len, remote, timeout, reserved,
    )
}

pub extern "system" fn WSAConnectByNameW(
    s: Socket,
    node: *const u16,
    service: *const u16,
    local_len: *mut u32,
    local: *mut c_void,
    remote_len: *mut u32,
    remote: *mut c_void,
    timeout: *const c_void,
    reserved: *mut c_void,
) -> i32 {
    if !reachable_name(wide(node)) {
        return 0;
    }
    host!("ws2_32.dll", "WSAConnectByNameW": extern "system" fn(Socket, *const u16, *const u16, *mut u32, *mut c_void, *mut u32, *mut c_void, *const c_void, *mut c_void) -> i32)(
        s, node, service, local_len, local, remote_len, remote, timeout, reserved,
    )
}

/// `SOCKET_ADDRESS`; a `SOCKET_ADDRESS_LIST` is an `i32` count, then these
/// (pointer aligned).
#[repr(C)]
struct SocketAddress {
    addr: *const u8,
    len: i32,
}

/// Every candidate must be loopback.
pub extern "system" fn WSAConnectByList(
    s: Socket,
    list: *const u8,
    local_len: *mut u32,
    local: *mut c_void,
    remote_len: *mut u32,
    remote: *mut c_void,
    timeout: *const c_void,
    reserved: *mut c_void,
) -> i32 {
    if !list.is_null() {
        // SAFETY: the title passes a `SOCKET_ADDRESS_LIST`.
        let entries = unsafe {
            let count = usize::try_from(list.cast::<i32>().read()).unwrap_or(0);
            std::slice::from_raw_parts(list.add(8).cast::<SocketAddress>(), count)
        };
        if !entries
            .iter()
            .all(|e| reachable("network connect blocked", e.addr, e.len))
        {
            return 0;
        }
    }
    host!("ws2_32.dll", "WSAConnectByList": extern "system" fn(Socket, *const u8, *mut u32, *mut c_void, *mut u32, *mut c_void, *const c_void, *mut c_void) -> i32)(
        s, list, local_len, local, remote_len, remote, timeout, reserved,
    )
}

pub extern "system" fn WSAJoinLeaf(
    s: Socket,
    name: *const u8,
    len: i32,
    caller_data: *const c_void,
    callee_data: *mut c_void,
    sqos: *const c_void,
    gqos: *const c_void,
    flags: u32,
) -> Socket {
    if !reachable("network connect blocked", name, len) {
        return INVALID_SOCKET;
    }
    host!("ws2_32.dll", "WSAJoinLeaf": extern "system" fn(Socket, *const u8, i32, *const c_void, *mut c_void, *const c_void, *const c_void, u32) -> Socket)(
        s,
        name,
        len,
        caller_data,
        callee_data,
        sqos,
        gqos,
        flags,
    )
}

/// `ConnectEx` as `WSAIoctl` hands it out: vetted, then the socket's own.
pub(super) extern "system" fn connect_ex(
    s: Socket,
    name: *const u8,
    len: i32,
    send: *const c_void,
    send_len: u32,
    sent: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    if !reachable("network connect blocked", name, len) {
        return 0;
    }
    let (mut real, mut size) = (0usize, 0u32);
    let ioctl = host!("ws2_32.dll", "WSAIoctl": extern "system" fn(Socket, u32, *const c_void, u32, *mut c_void, u32, *mut u32, *mut c_void, *const c_void) -> i32);
    let guid = WSAID_CONNECTEX;
    let found = ioctl(
        s,
        SIO_GET_EXTENSION_FUNCTION_POINTER,
        (&raw const guid).cast(),
        16,
        (&raw mut real).cast(),
        8,
        &mut size,
        std::ptr::null_mut(),
        std::ptr::null(),
    );
    if found != 0 || real == 0 {
        return 0;
    }
    // SAFETY: the provider's `ConnectEx` for this socket.
    let real = unsafe {
        std::mem::transmute::<
            usize,
            extern "system" fn(
                Socket,
                *const u8,
                i32,
                *const c_void,
                u32,
                *mut u32,
                *mut c_void,
            ) -> i32,
        >(real)
    };
    real(s, name, len, send, send_len, sent, overlapped)
}

/// The `ConnectEx` and `WSASendMsg` pointers it hands out are ours; the
/// multi-pointer query (Registered I/O, whose `RIOSendEx` takes any
/// address) is refused as by an old system. Other controls pass through.
pub extern "system" fn WSAIoctl(
    s: Socket,
    code: u32,
    input: *const c_void,
    input_len: u32,
    output: *mut c_void,
    output_len: u32,
    returned: *mut u32,
    overlapped: *mut c_void,
    completion: *const c_void,
) -> i32 {
    if code == SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER {
        wsa_error(WSAEINVAL);
        return SOCKET_ERROR;
    }
    let result = host!("ws2_32.dll", "WSAIoctl": extern "system" fn(Socket, u32, *const c_void, u32, *mut c_void, u32, *mut u32, *mut c_void, *const c_void) -> i32)(
        s, code, input, input_len, output, output_len, returned, overlapped, completion,
    );
    if result == 0 && code == SIO_GET_EXTENSION_FUNCTION_POINTER && input_len >= 16 {
        // SAFETY: the host just read a GUID from `input` and wrote a pointer to `output`.
        unsafe {
            let ours = match input.cast::<Guid>().read_unaligned() {
                WSAID_CONNECTEX => connect_ex as *const () as usize,
                WSAID_WSASENDMSG => WSASendMsg as *const () as usize,
                _ => return result,
            };
            output.cast::<usize>().write_unaligned(ours);
        }
    }
    result
}

// Datagram sends.

/// Without an address it sends to the connected peer, which `connect` vetted;
/// so does a stream socket whatever `to` says (Winsock ignores it there).
pub extern "system" fn sendto(
    s: Socket,
    buf: *const u8,
    len: i32,
    flags: i32,
    to: *const u8,
    to_len: i32,
) -> i32 {
    if !to.is_null() && !stream(s) && !reachable("network datagram blocked", to, to_len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "sendto": extern "system" fn(Socket, *const u8, i32, i32, *const u8, i32) -> i32)(
        s, buf, len, flags, to, to_len,
    )
}

pub extern "system" fn WSASendTo(
    s: Socket,
    buffers: *const c_void,
    count: u32,
    sent: *mut u32,
    flags: u32,
    to: *const u8,
    to_len: i32,
    overlapped: *mut c_void,
    completion: *const c_void,
) -> i32 {
    if !to.is_null() && !stream(s) && !reachable("network datagram blocked", to, to_len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "WSASendTo": extern "system" fn(Socket, *const c_void, u32, *mut u32, u32, *const u8, i32, *mut c_void, *const c_void) -> i32)(
        s, buffers, count, sent, flags, to, to_len, overlapped, completion,
    )
}

/// A connection-oriented socket, whose sends go to the peer `connect` vetted.
fn stream(s: Socket) -> bool {
    const SOL_SOCKET: i32 = 0xffff;
    const SO_TYPE: i32 = 0x1008;
    const SOCK_STREAM: i32 = 1;
    let (mut kind, mut len) = (0i32, 4i32);
    let getsockopt = host!("ws2_32.dll", "getsockopt": extern "system" fn(Socket, i32, i32, *mut i32, *mut i32) -> i32);
    getsockopt(s, SOL_SOCKET, SO_TYPE, &mut kind, &mut len) == 0 && kind == SOCK_STREAM
}

/// The start of a `WSAMSG`: the destination.
#[repr(C)]
struct MessageName {
    name: *const u8,
    len: i32,
}

pub extern "system" fn WSASendMsg(
    s: Socket,
    message: *const c_void,
    flags: u32,
    sent: *mut u32,
    overlapped: *mut c_void,
    completion: *const c_void,
) -> i32 {
    // SAFETY: the title passes a `WSAMSG` or null.
    if let Some(m) = unsafe { message.cast::<MessageName>().as_ref() }
        && !m.name.is_null()
        && !reachable("network datagram blocked", m.name, m.len)
    {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "WSASendMsg": extern "system" fn(Socket, *const c_void, u32, *mut u32, *mut c_void, *const c_void) -> i32)(
        s, message, flags, sent, overlapped, completion,
    )
}

/// All interfaces become loopback, the only one an unplugged console has;
/// any other address is not ours (`WSAEADDRNOTAVAIL`).
// shortcut: `[::]` becomes `[::1]`, so a dual-stack listener no longer hears
// 127.0.0.1; bind both families if a title relies on that.
pub extern "system" fn bind(s: Socket, name: *const u8, len: i32) -> i32 {
    let host_bind = host!("ws2_32.dll", "bind": extern "system" fn(Socket, *const u8, i32) -> i32);
    match socket_addr(name, len) {
        Some(a) if loopback(a.ip()) => host_bind(s, name, len),
        Some(a) if a.ip().is_unspecified() => {
            once("network bind narrowed to loopback", a.to_string());
            // SAFETY: `socket_addr` checked `len` readable bytes.
            let mut local = unsafe { std::slice::from_raw_parts(name, len as usize) }.to_vec();
            match a {
                SocketAddr::V4(_) => local[4..8].copy_from_slice(&[127, 0, 0, 1]),
                SocketAddr::V6(_) => local[23] = 1,
            }
            host_bind(s, local.as_ptr(), len)
        }
        a => {
            once("network bind blocked", shown(a));
            wsa_error(WSAEADDRNOTAVAIL);
            SOCKET_ERROR
        }
    }
}
