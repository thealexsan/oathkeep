use std::ffi::{c_char, c_void};

use super::super::xasync::{self, AsyncBlock, E_INVALIDARG};
use super::super::xgameruntime::{HRESULT, S_OK, put, text};
use super::ops::{
    BLOB, BlobCallback, BlobInfo, Container, ContainerCallback, Provider, This, Update, cstring,
    delete, get, give, layout, names, read, report, take,
};
use super::store::{current_named, valid_container};
use super::{
    BUFFER_TOO_SMALL, INVALID_CONTAINER_NAME, MAX_UPDATE, NO_ACCESS, UPDATE_TOO_BIG, saves_lock,
};
use crate::runtime;

static INIT: u8 = 0;
static QUOTA_CALL: u8 = 0;
static DELETE: u8 = 0;
static READ: u8 = 0;
static SUBMIT: u8 = 0;

pub(super) extern "system" fn initialize_async(
    _: This,
    _user: *const c_void,
    config: *const c_char,
    _sync_on_demand: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    let saves = runtime::title().saves();
    let result = if saves.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        Err(NO_ACCESS)
    } else {
        Provider::open(&saves, text(config).as_deref()).map(|p| xasync::bytes(give(p)))
    };
    xasync::finished_with(block, (&raw const INIT).cast(), result)
}
pub(super) extern "system" fn initialize_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *mut Provider,
) -> HRESULT {
    xasync::result(block, (&raw const INIT).cast(), out)
}
pub(super) extern "system" fn close_provider(_: This, provider: *mut Provider) {
    take(provider);
}

pub(super) extern "system" fn quota_async(
    _: This,
    provider: *const Provider,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = get(provider)
        .ok_or(E_INVALIDARG)
        .map(|p| xasync::bytes(p.remaining() as i64));
    xasync::finished_with(block, (&raw const QUOTA_CALL).cast(), result)
}
pub(super) extern "system" fn quota_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut i64,
) -> HRESULT {
    xasync::result(block, (&raw const QUOTA_CALL).cast(), out)
}

pub(super) extern "system" fn delete_async(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    let hr = match (get(provider), text(name)) {
        (Some(p), Some(name)) if valid_container(&name) => delete(&p.root, &name),
        (Some(_), Some(_)) => INVALID_CONTAINER_NAME,
        _ => E_INVALIDARG,
    };
    xasync::finished(block, (&raw const DELETE).cast(), hr)
}
pub(super) extern "system" fn delete_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const DELETE).cast(), 0, none, none.cast())
}

/// Reports the container if it exists; a missing one is no error, the
/// callback just does not run.
pub(super) extern "system" fn container_info(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> HRESULT {
    let (Some(p), Some(name), Some(callback)) = (get(provider), text(name), callback) else {
        return E_INVALIDARG;
    };
    if !valid_container(&name) {
        return INVALID_CONTAINER_NAME;
    }
    report(&p.root, |n| n == name, context, callback);
    S_OK
}

pub(super) extern "system" fn enumerate_containers(
    _: This,
    provider: *const Provider,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> HRESULT {
    let (Some(p), Some(callback)) = (get(provider), callback) else {
        return E_INVALIDARG;
    };
    report(&p.root, |_| true, context, callback);
    S_OK
}

/// The container exists once an update to it is submitted.
pub(super) extern "system" fn create_container(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    out: *mut *mut Container,
) -> HRESULT {
    let (Some(p), Some(name)) = (get(provider), text(name)) else {
        return E_INVALIDARG;
    };
    if !valid_container(&name) {
        return INVALID_CONTAINER_NAME;
    }
    put(out, give(p.container(&name)))
}
pub(super) extern "system" fn close_container(_: This, container: *mut Container) {
    take(container);
}

pub(super) extern "system" fn enumerate_blobs(
    _: This,
    container: *const Container,
    prefix: *const c_char,
    context: *mut c_void,
    callback: Option<BlobCallback>,
) -> HRESULT {
    let (Some(c), Some(callback)) = (get(container), callback) else {
        return E_INVALIDARG;
    };
    let prefix = text(prefix).unwrap_or_default();
    let blobs = {
        let _saves = saves_lock();
        let version = current_named(&c.dir, &c.name).ok().flatten();
        version.map(|v| v.sizes()).unwrap_or_default()
    };
    for (name, size) in blobs.into_iter().filter(|(n, _)| n.starts_with(&prefix)) {
        let name = cstring(&name);
        let info = BlobInfo {
            name: name.as_ptr(),
            size: size as u32,
        };
        if !callback(&info, context) {
            break;
        }
    }
    S_OK
}

pub(super) extern "system" fn read_blobs(
    _: This,
    container: *const Container,
    blob_names: *const *const c_char,
    count: *mut u32,
    size: usize,
    out: *mut u8,
) -> HRESULT {
    let Some(c) = get(container) else {
        return E_INVALIDARG;
    };
    // SAFETY: the title passes its count.
    let wanted = unsafe { count.as_ref() }.copied().unwrap_or(0);
    let blobs = match read(c, names(blob_names, wanted)) {
        Ok(blobs) => blobs,
        Err(hr) => return hr,
    };
    let bytes = layout(&blobs, out as usize);
    if out.is_null() || size < bytes.len() {
        return BUFFER_TOO_SMALL;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { out.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
    put(count, blobs.len() as u32)
}

/// Read at once; the result holds the layout with pointers relative to the
/// title's buffer, fixed up when the title takes it.
pub(super) extern "system" fn read_async(
    _: This,
    container: *const Container,
    blob_names: *const *const c_char,
    count: u32,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = match get(container) {
        Some(c) => read(c, names(blob_names, count)).map(|blobs| layout(&blobs, 0)),
        None => Err(E_INVALIDARG),
    };
    xasync::finished_with(block, (&raw const READ).cast(), result)
}
pub(super) extern "system" fn read_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    out: *mut u8,
    count: *mut u32,
) -> HRESULT {
    let mut used = 0;
    let hr = xasync::XAsyncGetResult(block, (&raw const READ).cast(), size, out.cast(), &mut used);
    // The documented error for a small buffer; the result stays for a
    // retry with a larger one.
    if hr == xasync::E_NOT_SUFFICIENT_BUFFER {
        return BUFFER_TOO_SMALL;
    }
    if hr < 0 {
        return hr;
    }
    // The first name follows the blob array, so its offset gives the count.
    let blobs = if used == 0 {
        0
    } else {
        // SAFETY: the result holds at least one blob entry.
        unsafe { out.cast::<usize>().read_unaligned() / BLOB }
    };
    for i in 0..blobs {
        for field in [0, 16] {
            // SAFETY: within the blob array just written.
            unsafe {
                let p = out.add(i * BLOB + field).cast::<usize>();
                p.write_unaligned(p.read_unaligned() + out as usize);
            }
        }
    }
    put(count, blobs as u32)
}

pub(super) extern "system" fn create_update(
    _: This,
    container: *const Container,
    display: *const c_char,
    out: *mut *mut Update,
) -> HRESULT {
    let Some(c) = get(container) else {
        return E_INVALIDARG;
    };
    put(
        out,
        give(Update {
            name: c.name.clone(),
            dir: c.dir.clone(),
            display: text(display),
            changes: Vec::new(),
        }),
    )
}
pub(super) extern "system" fn close_update(_: This, update: *mut Update) {
    take(update);
}

pub(super) extern "system" fn write_blob(
    _: This,
    update: *mut Update,
    name: *const c_char,
    data: *const u8,
    size: usize,
) -> HRESULT {
    // SAFETY: a handle we gave out; the title does not use it concurrently.
    let (Some(update), Some(name)) = (unsafe { update.as_mut() }, text(name)) else {
        return E_INVALIDARG;
    };
    if name.is_empty() || (data.is_null() && size > 0) {
        return E_INVALIDARG;
    }
    if size > MAX_UPDATE {
        return UPDATE_TOO_BIG;
    }
    let data = if size == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `size` bytes.
        unsafe { std::slice::from_raw_parts(data, size) }.to_vec()
    };
    update.changes.push((name, Some(data)));
    S_OK
}

pub(super) extern "system" fn delete_blob(
    _: This,
    update: *mut Update,
    name: *const c_char,
) -> HRESULT {
    // SAFETY: as in `write_blob`.
    match (unsafe { update.as_mut() }, text(name)) {
        (Some(update), Some(name)) if !name.is_empty() => {
            update.changes.push((name, None));
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub(super) extern "system" fn submit_async(
    _: This,
    update: *const Update,
    block: *mut AsyncBlock,
) -> HRESULT {
    let hr = get(update).map_or(E_INVALIDARG, Update::submit);
    xasync::finished(block, (&raw const SUBMIT).cast(), hr)
}
pub(super) extern "system" fn submit_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const SUBMIT).cast(), 0, none, none.cast())
}
