//! Media Foundation is the host's (`mfplat.dll` passes through), except for
//! the source resolver: the host's would fetch `http`, `rtsp` and other
//! URLs over the network, past `modules::net`. Titles get a proxy that
//! opens local files only, with paths on our virtual drives rewritten to
//! their host folders, and forwards everything else to the host's resolver.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod url;

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};
use url::checked;

const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
/// What Media Foundation answers for a URL scheme it has no handler for.
const UNSUPPORTED_SCHEME: HRESULT = 0xC00D_36C3_u32 as i32;

const IID_IUNKNOWN: [u8; 16] = guid(0x0000_0000, 0x0000, 0x0000, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_RESOLVER: [u8; 16] = guid(
    0xFBE5_A32D,
    0xA497,
    0x4B61,
    [0xBB, 0x85, 0x97, 0xB1, 0xA8, 0x48, 0xA6, 0xE3],
);

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

// The proxy resolver.

/// The host's resolver, called through its method table.
#[repr(C)]
struct Host {
    vtable: *const usize,
}

impl Host {
    /// Method `n` of the host object, typed by the caller.
    unsafe fn method<F: Copy>(this: *mut Host, n: usize) -> F {
        // SAFETY: the caller names the slot's real signature; `this` is the
        // host's live resolver.
        unsafe { std::mem::transmute_copy(&*(*this).vtable.add(n)) }
    }
}

#[repr(C)]
pub struct Resolver {
    vtable: &'static Methods,
    host: *mut Host,
    refs: AtomicU32,
}

type Unknown = *mut c_void;

#[repr(C)]
struct Methods {
    query_interface:
        extern "system" fn(*mut Resolver, *const [u8; 16], *mut *mut c_void) -> HRESULT,
    add_ref: extern "system" fn(*mut Resolver) -> u32,
    release: extern "system" fn(*mut Resolver) -> u32,
    create_from_url: extern "system" fn(
        *mut Resolver,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT,
    create_from_stream: extern "system" fn(
        *mut Resolver,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT,
    begin_from_url: extern "system" fn(
        *mut Resolver,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT,
    end_from_url: extern "system" fn(*mut Resolver, Unknown, *mut u32, *mut Unknown) -> HRESULT,
    begin_from_stream: extern "system" fn(
        *mut Resolver,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT,
    end_from_stream: extern "system" fn(*mut Resolver, Unknown, *mut u32, *mut Unknown) -> HRESULT,
    cancel: extern "system" fn(*mut Resolver, Unknown) -> HRESULT,
}

static METHODS: Methods = Methods {
    query_interface,
    add_ref,
    release,
    create_from_url,
    create_from_stream,
    begin_from_url,
    end_from_url,
    begin_from_stream,
    end_from_stream,
    cancel,
};

fn host(this: *mut Resolver) -> *mut Host {
    // SAFETY: title code calls methods on a proxy we made, alive until its
    // last release.
    unsafe { (*this).host }
}

/// Only the resolver's own interfaces: anything else could hand out the
/// host object unguarded.
extern "system" fn query_interface(
    this: *mut Resolver,
    iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    let iid = unsafe { *iid };
    let found = iid == IID_IUNKNOWN || iid == IID_RESOLVER;
    // SAFETY: checked non-null.
    unsafe {
        out.write(if found {
            this.cast()
        } else {
            std::ptr::null_mut()
        })
    };
    if !found {
        return E_NOINTERFACE;
    }
    add_ref(this);
    S_OK
}

extern "system" fn add_ref(this: *mut Resolver) -> u32 {
    // SAFETY: as in `host`.
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

extern "system" fn release(this: *mut Resolver) -> u32 {
    // SAFETY: as in `host`.
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: made by `MFCreateSourceResolver`; this was the last
        // reference, to the proxy and to the host object it holds.
        let proxy = unsafe { Box::from_raw(this) };
        // SAFETY: slot 2 is `Release`.
        let release: extern "system" fn(*mut Host) -> u32 = unsafe { Host::method(proxy.host, 2) };
        release(proxy.host);
    }
    left
}

extern "system" fn create_from_url(
    this: *mut Resolver,
    url: *const u16,
    flags: u32,
    props: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let url = match checked(url) {
        Ok(url) => url,
        Err(error) => {
            if !object.is_null() {
                // SAFETY: the title's out parameter.
                unsafe { object.write(std::ptr::null_mut()) };
            }
            return error;
        }
    };
    let host = host(this);
    // SAFETY: slot 3 is `CreateObjectFromURL`.
    let f: extern "system" fn(
        *mut Host,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 3) };
    f(host, url.as_ptr(), flags, props, kind, object)
}

extern "system" fn begin_from_url(
    this: *mut Resolver,
    url: *const u16,
    flags: u32,
    props: Unknown,
    cookie: *mut Unknown,
    callback: Unknown,
    state: Unknown,
) -> HRESULT {
    let url = match checked(url) {
        Ok(url) => url,
        Err(error) => return error,
    };
    let host = host(this);
    // SAFETY: slot 5 is `BeginCreateObjectFromURL`.
    let f: extern "system" fn(
        *mut Host,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 5) };
    f(host, url.as_ptr(), flags, props, cookie, callback, state)
}

extern "system" fn create_from_stream(
    this: *mut Resolver,
    stream: Unknown,
    url: *const u16,
    flags: u32,
    props: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 4 is `CreateObjectFromByteStream`; the URL is only a
    // hint for picking a handler.
    let f: extern "system" fn(
        *mut Host,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut u32,
        *mut Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 4) };
    f(host, stream, url, flags, props, kind, object)
}

extern "system" fn end_from_url(
    this: *mut Resolver,
    result: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 6 is `EndCreateObjectFromURL`.
    let f: extern "system" fn(*mut Host, Unknown, *mut u32, *mut Unknown) -> HRESULT =
        unsafe { Host::method(host, 6) };
    f(host, result, kind, object)
}

#[allow(clippy::too_many_arguments)]
extern "system" fn begin_from_stream(
    this: *mut Resolver,
    stream: Unknown,
    url: *const u16,
    flags: u32,
    props: Unknown,
    cookie: *mut Unknown,
    callback: Unknown,
    state: Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 7 is `BeginCreateObjectFromByteStream`.
    let f: extern "system" fn(
        *mut Host,
        Unknown,
        *const u16,
        u32,
        Unknown,
        *mut Unknown,
        Unknown,
        Unknown,
    ) -> HRESULT = unsafe { Host::method(host, 7) };
    f(host, stream, url, flags, props, cookie, callback, state)
}

extern "system" fn end_from_stream(
    this: *mut Resolver,
    result: Unknown,
    kind: *mut u32,
    object: *mut Unknown,
) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 8 is `EndCreateObjectFromByteStream`.
    let f: extern "system" fn(*mut Host, Unknown, *mut u32, *mut Unknown) -> HRESULT =
        unsafe { Host::method(host, 8) };
    f(host, result, kind, object)
}

extern "system" fn cancel(this: *mut Resolver, cookie: Unknown) -> HRESULT {
    let host = host(this);
    // SAFETY: slot 9 is `CancelObjectCreation`.
    let f: extern "system" fn(*mut Host, Unknown) -> HRESULT = unsafe { Host::method(host, 9) };
    f(host, cookie)
}

/// The host's resolver behind our proxy.
pub extern "system" fn MFCreateSourceResolver(out: *mut *mut Resolver) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    static CREATE: OnceLock<Option<usize>> = OnceLock::new();
    let Some(create) =
        *CREATE.get_or_init(|| crate::host::function("mfplat.dll", "MFCreateSourceResolver"))
    else {
        return UNSUPPORTED_SCHEME;
    };
    // SAFETY: the host's `MFCreateSourceResolver(IMFSourceResolver**)`.
    let create: extern "system" fn(*mut *mut Host) -> HRESULT =
        unsafe { std::mem::transmute(create) };
    let mut host = std::ptr::null_mut();
    let hr = create(&mut host);
    if hr < 0 || host.is_null() {
        return hr;
    }
    let proxy = Box::new(Resolver {
        vtable: &METHODS,
        host,
        refs: AtomicU32::new(1),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(proxy)) };
    S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "media",
        dlls: &["mfplat.dll"],
        exports: vec![hle_export!(MFCreateSourceResolver)],
    }
}

#[cfg(test)]
mod tests;
