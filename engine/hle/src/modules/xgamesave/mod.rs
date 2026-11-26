//! XGameSave: the user's game saves, kept on the host under the title's
//! data directory, next to persistent local storage (`runtime::Title::
//! saves`), never in the cache. A provider is a folder per user and
//! service configuration, a container a folder in it.
//!
//! An update is all or nothing, as documented: each one writes a complete
//! new version of the container (`v<N>.new/`, unchanged blobs hard-linked
//! from the previous version), flushed to disk, and one directory rename
//! commits it (`v<N>`); the highest committed version is the container.
//! A crash leaves the old version or the new one, never a mix; leftovers
//! are removed on sight. Deleting a container renames it away first.
//!
//! Names are exact: a version's `manifest` holds the container's name, its
//! display name and the blob names (blob `i` is the file `i`), so case,
//! characters the host forbids, reserved device names and length never
//! reach a file name. A container's folder is its name made safe (and
//! shortened) plus a hash of the exact name, so names differing only in
//! case get different folders. Offline there is no cloud copy, so nothing
//! ever needs a sync.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;

use super::xgameruntime::{Guid, HRESULT, Interface, Methods, Object, add_ref, missing};

mod abi;
mod ops;
mod store;
#[cfg(test)]
mod tests;

use abi::*;
pub use ops::{BlobInfo, ContainerInfo};

pub const PROVIDER: Guid = Guid::new(
    0x704c_3f58,
    0xe629,
    0x4cc2,
    [0xb1, 0x97, 0x30, 0x51, 0x1b, 0x99, 0x6f, 0xe2],
);
/// As the title (2026) asks for it; every slot it calls takes
/// the public API's arguments in the same order as under the provider's
/// own id (older titles).
pub const IID_2026: Guid = Guid::new(
    0x1bff_f3af,
    0xf14a,
    0x40a3,
    [0x8e, 0x35, 0x9a, 0xda, 0x90, 0x65, 0x93, 0xf9],
);

pub static OBJECT: Object = Object::new("game save", &VTABLE);
pub const INTERFACES: &[Interface] =
    &[(PROVIDER, PROVIDER, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

// The public API's error codes.
pub const INVALID_CONTAINER_NAME: HRESULT = 0x8083_0001_u32 as i32;
pub const NO_ACCESS: HRESULT = 0x8083_0002_u32 as i32;
pub const OUT_OF_LOCAL_STORAGE: HRESULT = 0x8083_0003_u32 as i32;
pub const UPDATE_TOO_BIG: HRESULT = 0x8083_0005_u32 as i32;
pub const QUOTA_EXCEEDED: HRESULT = 0x8083_0006_u32 as i32;
pub const BUFFER_TOO_SMALL: HRESULT = 0x8083_0007_u32 as i32;
pub const BLOB_NOT_FOUND: HRESULT = 0x8083_0008_u32 as i32;

/// Per user and title, as documented.
const QUOTA: u64 = 256 << 20;
/// What one update may write in all, as documented.
const MAX_UPDATE: usize = 16 << 20;
const MANIFEST: &str = "manifest";
/// Readable part of a container's folder name, at most.
const READABLE: usize = 100;

/// Every save operation, so none sees another's half-made version.
static SAVES: Mutex<()> = Mutex::new(());

fn saves_lock() -> std::sync::MutexGuard<'static, ()> {
    SAVES.lock().unwrap_or_else(|e| e.into_inner())
}

static VTABLE: Methods<30> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),         // 0x18 initialize (blocking)
    initialize_async as *const (),     // 0x20
    initialize_result as *const (),    // 0x28
    close_provider as *const (),       // 0x30
    missing::<7> as *const (),         // 0x38 remaining quota (blocking)
    quota_async as *const (),          // 0x40
    quota_result as *const (),         // 0x48
    missing::<10> as *const (),        // 0x50 delete (blocking)
    delete_async as *const (),         // 0x58
    delete_result as *const (),        // 0x60
    container_info as *const (),       // 0x68
    enumerate_containers as *const (), // 0x70
    missing::<15> as *const (),        // 0x78 by name prefix
    create_container as *const (),     // 0x80
    close_container as *const (),      // 0x88
    missing::<18> as *const (),        // 0x90 all blob info
    enumerate_blobs as *const (),      // 0x98 by name prefix
    read_blobs as *const (),           // 0xa0
    read_async as *const (),           // 0xa8
    read_result as *const (),          // 0xb0
    create_update as *const (),        // 0xb8
    close_update as *const (),         // 0xc0
    write_blob as *const (),           // 0xc8
    delete_blob as *const (),          // 0xd0
    missing::<27> as *const (),        // 0xd8 submit (blocking)
    submit_async as *const (),         // 0xe0
    submit_result as *const (),        // 0xe8
]);
