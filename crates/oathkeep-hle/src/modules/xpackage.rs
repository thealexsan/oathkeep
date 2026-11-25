#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::sync::OnceLock;

use super::xasync::{self, AsyncBlock};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, put_string, text, token,
};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0xaf40_6016,
    0xe850,
    0x4aa8,
    [0xa8, 0x8d, 0x2f, 0x3d, 0xcb, 0x9d, 0xac, 0x7e],
);

pub const IID_2026: Guid = Guid::new(
    0x0b9d_556e,
    0xab70,
    0x43f4,
    [0xba, 0xa0, 0x99, 0x99, 0xac, 0x4c, 0xb0, 0x40],
);

pub static OBJECT: Object = Object::new("package", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID_2026, &OBJECT)];

pub const NO_PACKAGE: HRESULT = 0x8924_5208_u32 as i32;

pub fn identifier() -> String {
    format!("{:08X}", runtime::title().id)
}

fn is_ours(id: *const c_char) -> bool {
    text(id).is_some_and(|id| id == identifier())
}

fn package_bytes() -> u64 {
    fn size(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => size(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let exe = runtime::main_image().and_then(runtime::image_path);
        exe.as_deref().and_then(Path::parent).map_or(0, size)
    })
}

type This = *const Object;

extern "system" fn current_identifier(_: This, size: usize, buffer: *mut u8) -> HRESULT {
    put_string(&identifier(), size, buffer, std::ptr::null_mut())
}

extern "system" fn is_packaged(_: This) -> bool {
    true
}

static MONITOR: u8 = 0;
fn monitor() -> *const c_void {
    (&raw const MONITOR).cast()
}

extern "system" fn create_monitor(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _queue: *const c_void,
    out: *mut *const c_void,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, monitor())
}
extern "system" fn close_monitor(_: This, _: *const c_void) {}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub total_bytes: u64,
    pub installed_bytes: u64,
    pub launch_bytes: u64,
    pub launchable: bool,
    pub completed: bool,
}

extern "system" fn progress(_: This, _: *const c_void, out: *mut Progress) {
    let bytes = package_bytes();
    put(
        out,
        Progress {
            total_bytes: bytes,
            installed_bytes: bytes,
            launch_bytes: bytes,
            launchable: true,
            completed: true,
        },
    );
}

extern "system" fn register_progress(
    _: This,
    _monitor: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put(out, token())
}

extern "system" fn find_availability(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    out: *mut u32,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, 0)
}

extern "system" fn change_order(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

static INSTALL: u8 = 0;

extern "system" fn install_chunks_async(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _suppress_confirmation: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = if is_ours(id) {
        Ok(xasync::bytes(monitor()))
    } else {
        Err(NO_PACKAGE)
    };
    xasync::finished_with(block, (&raw const INSTALL).cast(), result)
}
extern "system" fn install_chunks_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const INSTALL).cast(), out)
}

extern "system" fn estimate_download(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    bytes: *mut u64,
    confirm: *mut bool,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    if !confirm.is_null() {
        // SAFETY: checked non-null.
        unsafe { confirm.write(false) };
    }
    put(bytes, 0)
}

extern "system" fn uninstall_chunks(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

extern "system" fn mount_path(_: This, _: *const c_void, _: usize, _: *mut u8) -> HRESULT {
    xasync::E_INVALIDARG
}
extern "system" fn close_mount(_: This, _: *const c_void) {}

extern "system" fn uninstall_package(_: This, id: *const c_char) -> bool {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package uninstall refused"
    );
    false
}

static MOUNT: u8 = 0;

extern "system" fn mount_async(_: This, id: *const c_char, block: *mut AsyncBlock) -> HRESULT {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package mount: not installed"
    );
    xasync::finished(block, (&raw const MOUNT).cast(), NO_PACKAGE)
}
extern "system" fn mount_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const MOUNT).cast(), out)
}

extern "system" fn enumerate(
    _: This,
    kind: u32,
    _scope: u32,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    tracing::debug!(kind, "packages enumerated: none");
    S_OK
}

static VTABLE: Methods<40> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    current_identifier as *const (),
    is_packaged as *const (),
    create_monitor as *const (),
    close_monitor as *const (),
    progress as *const (),
    missing::<8> as *const (),
    register_progress as *const (),
    missing::<10> as *const (),
    missing::<11> as *const (),
    find_availability as *const (),
    missing::<13> as *const (),
    change_order as *const (),
    missing::<15> as *const (),
    install_chunks_async as *const (),
    install_chunks_result as *const (),
    estimate_download as *const (),
    uninstall_chunks as *const (),
    missing::<20> as *const (),
    missing::<21> as *const (),
    missing::<22> as *const (),
    missing::<23> as *const (),
    missing::<24> as *const (),
    mount_path as *const (),
    close_mount as *const (),
    missing::<27> as *const (),
    missing::<28> as *const (),
    missing::<29> as *const (),
    missing::<30> as *const (),
    missing::<31> as *const (),
    missing::<32> as *const (),
    missing::<33> as *const (),
    uninstall_package as *const (),
    missing::<35> as *const (),
    missing::<36> as *const (),
    mount_async as *const (),
    mount_result as *const (),
    enumerate as *const (),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn the_installation_is_complete_and_alone() {
        let this: This = &OBJECT;
        type Id = extern "system" fn(This, usize, *mut u8) -> HRESULT;
        let mut id = [0u8; 33];
        assert_eq!(method::<Id>(&OBJECT, 0x18)(this, 33, id.as_mut_ptr()), S_OK);
        let len = id.iter().position(|&b| b == 0).unwrap();
        assert_eq!(std::str::from_utf8(&id[..len]).unwrap(), identifier());
        assert!(method::<extern "system" fn(This) -> bool>(&OBJECT, 0x20)(
            this
        ));

        type Find =
            extern "system" fn(This, *const c_char, u32, *const c_void, *mut u32) -> HRESULT;
        let mut availability = 9;
        let ours = format!("{}\0", identifier());
        let find = method::<Find>(&OBJECT, 0x60);
        let none = std::ptr::null();
        assert_eq!(
            find(this, ours.as_ptr().cast(), 0, none, &mut availability),
            S_OK
        );
        assert_eq!(availability, 0, "ready");
        assert_eq!(
            find(this, c"dlc".as_ptr(), 0, none, &mut availability),
            NO_PACKAGE
        );

        type Progress_ = extern "system" fn(This, *const c_void, *mut Progress);
        let mut p = Progress {
            total_bytes: 1,
            installed_bytes: 0,
            launch_bytes: 0,
            launchable: false,
            completed: false,
        };
        method::<Progress_>(&OBJECT, 0x38)(this, monitor(), &mut p);
        assert!(p.completed && p.launchable && p.installed_bytes == p.total_bytes);

        type Mount = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            method::<Mount>(&OBJECT, 0x128)(this, c"dlc".as_ptr(), &mut block),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut block, true), NO_PACKAGE);
    }
}
