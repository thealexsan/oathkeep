//! The console's DirectStorage (`XCRDAPI.dll`): a factory that opens files
//! and creates request queues and status arrays the title polls. Its
//! interfaces have grown since 2020 (new interface ids, new slots); the
//! slots here are the ones the title calls, read from its call
//! sites. Slots nobody has needed yet log their offset.
//!
//! A request starts once it is submitted, not before: the title maps the
//! pages a batch is read into, and fills in their page lists, in its own
//! submit (`submit`).
//!
//! Submitted requests of every queue run on one pool of workers, as the
//! console's hardware runs them: in parallel, started in queue order,
//! queues served by weighted round robin over their priorities (about 10x
//! per level, counted in bytes), with at most the staging buffer's worth of
//! compressed source in flight. Each request reads its source (a file or
//! memory), inflates zlib data and/or unpacks BCPack data, and writes the
//! destination (an address, or physical pages). Completion is what stays
//! in order: a status entry is written once every request enqueued before
//! it on its queue has finished, with the first failure (in queue order)
//! since the previous status entry, or success.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod execute;
mod factory;
mod files;
mod queue;
mod scheduler;
pub(crate) mod stream_log;
mod trace;

#[cfg(test)]
mod tests;

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::modules::xasync::E_INVALIDARG;
use crate::modules::xgameruntime::{Guid, HRESULT, Object};
use crate::{HleModule, hle_export};

pub use factory::DStorageGetFactory;

/// The factory interface the title asks for.
pub(super) const FACTORY: Guid = Guid::new(
    0x6295_9d11,
    0xd45f,
    0x4884,
    [0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2, 0x43, 0xae],
);
pub(super) const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
pub(super) const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_HANDLE_EOF)`: the source ends early.
pub(super) const END_OF_FILE: HRESULT = 0x8007_0026_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INVALID_DATA)`: a bad compressed stream.
pub(super) const INVALID_DATA: HRESULT = 0x8007_000D_u32 as i32;

pub(super) type Out = *mut *mut c_void;

/// One of our objects with state: the runtime's object header (method
/// table first), a reference count, then `T`.
#[repr(C)]
pub(super) struct Instance<T> {
    pub(super) base: Object,
    pub(super) refs: AtomicU32,
    pub(super) state: T,
}

impl<T> Instance<T> {
    pub(super) fn create(base: Object, state: T) -> *mut c_void {
        Box::into_raw(Box::new(Instance {
            base,
            refs: AtomicU32::new(1),
            state,
        }))
        .cast()
    }
}

pub(super) extern "system" fn add_ref_instance<T>(this: *const Instance<T>) -> u32 {
    // SAFETY: an instance we created and the title still holds.
    unsafe { &*this }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

pub(super) extern "system" fn release<T>(this: *mut Instance<T>) -> u32 {
    // SAFETY: as above.
    let left = unsafe { &*this }.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference; created by `Instance::create`.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

pub(super) fn iid_text(iid: *const Guid) -> String {
    // SAFETY: the title passes a GUID (null tolerated).
    unsafe { iid.as_ref() }.map_or_else(|| "null".into(), Guid::to_string)
}

/// `HRESULT_FROM_WIN32` of a host error.
pub(super) fn from_win32(error: &std::io::Error) -> HRESULT {
    error
        .raw_os_error()
        .map_or(E_INVALIDARG, |e| (0x8007_0000 | (e as u32 & 0xffff)) as i32)
}

pub(super) fn text(name: *const c_char) -> String {
    if name.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated name from the title.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

/// A read request as the title builds it, 0x48 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Request {
    pub(super) options: u64,
    /// An address, or with `PAGE_DESTINATION` an array of 64 KiB page
    /// numbers.
    pub(super) destination: *mut c_void,
    /// With `PAGE_DESTINATION`, the low 16 bits are where in the first page
    /// the destination starts.
    pub(super) page_offset: u64,
    pub(super) destination_size: u32,
    /// Our file object, or with `MEMORY_SOURCE` an address.
    pub(super) source: *const c_void,
    pub(super) offset: u64,
    pub(super) source_size: u32,
    /// Compressed size between two decompression stages; unused here.
    pub(super) intermediate_size: u32,
    /// Matched by cancellation; the title leaves it 0.
    pub(super) tag: u64,
    pub(super) name: *const c_char,
}

// Request options.
/// Inflate zlib (RFC 1950) data.
pub(super) const ZLIB: u64 = 1;
/// Bits 1-4: decode BCPack data of this BC format number (0: none).
pub(super) const BCPACK: u64 = 0xf << 1;
pub(super) const PAGE_DESTINATION: u64 = 1 << 21;
pub(super) const MEMORY_SOURCE: u64 = 1 << 22;
// Bits 5-20 select a further transform, bit 23 a source in physical pages;
// The title's packages need neither, so requests with them fail.

pub(super) const PAGE: usize = 64 * 1024;

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dstorage",
        dlls: &["xcrdapi.dll"],
        exports: vec![hle_export!(DStorageGetFactory)],
    }
}
