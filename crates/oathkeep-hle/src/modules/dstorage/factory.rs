use std::ffi::c_char;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicI32, AtomicU64};

use crate::modules::xasync::E_INVALIDARG;
use crate::modules::xgameruntime::{Guid, HRESULT, Methods, Object, S_OK, add_ref, missing, put};

use super::files::{FILE_VTABLE, File, Files, Io};
use super::queue::{QUEUE_VTABLE, Queue, STATUS_VTABLE, StatusArray};
use super::scheduler::{Lane, scheduler};
use super::trace::WALLS_FILES;
use super::{E_NOINTERFACE, E_OUTOFMEMORY, FACTORY, Instance, Out, from_win32, iid_text, stream_log, text};

static FACTORY_OBJECT: Object = Object::new("dstorage factory", &FACTORY_VTABLE);

pub extern "system" fn DStorageGetFactory(iid: *const Guid, out: Out) -> HRESULT {
    // SAFETY: the title passes a GUID (null tolerated).
    match unsafe { iid.as_ref() } {
        Some(&iid) if iid == FACTORY => {
            put(out, std::ptr::from_ref(&FACTORY_OBJECT).cast_mut().cast())
        }
        iid => {
            tracing::warn!(iid = ?iid.map(Guid::to_string), "DStorageGetFactory: unknown interface");
            E_NOINTERFACE
        }
    }
}

// The factory.

type Factory = *const Object;

/// Opens `path` for reading; the file object owns the handle.
extern "system" fn open_file(_: Factory, path: *const u16, iid: *const Guid, out: Out) -> HRESULT {
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const OPEN_EXISTING: u32 = 3;
    if path.is_null() {
        return E_INVALIDARG;
    }
    // Goes through `files`, so paths on our virtual drives work too.
    let handle = crate::modules::files::CreateFileW(
        path,
        GENERIC_READ,
        FILE_SHARE_READ,
        std::ptr::null(),
        OPEN_EXISTING,
        0,
        std::ptr::null_mut(),
    );
    // SAFETY: the title passes a NUL-terminated path.
    let name = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(path, len))
    };
    if handle as isize == -1 {
        let error = std::io::Error::last_os_error();
        tracing::warn!(path = name, %error, "dstorage: opening a file failed");
        return from_win32(&error);
    }
    let kept = name.clone();
    tracing::info!(path = name, iid = iid_text(iid), "dstorage: file opened");
    // SAFETY: a handle we just opened and nobody else owns.
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let files = Files::new(file, Io::chosen());
    let file = Instance::create(Object::new("dstorage file", &FILE_VTABLE), File { files });
    if stream_log::on() {
        stream_log::line(format_args!("F\t{:x}\t{kept}", file as usize));
    }
    WALLS_FILES.lock().unwrap_or_else(|e| e.into_inner()).push((file as usize, kept));
    put(out, file)
}

/// `(capacity, name, iid, out)`.
extern "system" fn create_status_array(
    _: Factory,
    capacity: u32,
    name: *const c_char,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    tracing::info!(
        capacity,
        name = text(name),
        iid = iid_text(iid),
        "dstorage: status array"
    );
    let entries = (0..capacity).map(|_| AtomicI32::new(S_OK)).collect();
    let written = (0..capacity).map(|_| AtomicU64::new(0)).collect();
    let array = Instance::create(
        Object::new("dstorage status array", &STATUS_VTABLE),
        StatusArray { entries, written },
    );
    put(out, array)
}

/// The queue's description: as in 2020 (source type, capacity, priority,
/// name), plus a field the title sets to 1.
#[repr(C)]
pub(super) struct QueueDesc {
    pub(super) source_type: u64,
    pub(super) capacity: u16,
    pub(super) priority: i8,
    pub(super) name: *const c_char,
    pub(super) unknown: u64,
}

extern "system" fn create_queue(
    _: Factory,
    desc: *const QueueDesc,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    // SAFETY: the title passes its description (null tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let name = text(desc.name);
    tracing::info!(
        source = desc.source_type,
        capacity = desc.capacity,
        priority = desc.priority,
        name,
        unknown = desc.unknown,
        iid = iid_text(iid),
        "dstorage: queue"
    );
    let Some(scheduler) = scheduler() else {
        return E_OUTOFMEMORY;
    };
    let mut lanes = scheduler.lock();
    let id = lanes.next_id;
    lanes.next_id += 1;
    let lane = Lane::new(desc.priority, desc.capacity);
    lanes.lanes.insert(id, lane);
    drop(lanes);
    put(
        out,
        Instance::create(Object::new("dstorage queue", &QUEUE_VTABLE), Queue { id }),
    )
}

/// The title asks for 32 MiB and stops if this fails. Give PC worker threads
/// enough staging headroom (default 256 MiB) so multi-MB RPF chunks don't
/// starve the remaining cores.
extern "system" fn set_staging_buffer_size(_: Factory, size: u32) -> HRESULT {
    static FLOOR_MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let floor = *FLOOR_MB.get_or_init(|| {
        std::env::var("OATHKEEP_DSTORAGE_STAGING_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(256)
    }) << 20;
    tracing::info!(size, floor, "dstorage: staging buffer size");
    if let Some(scheduler) = scheduler() {
        scheduler.lock().staging = u64::from(size).max(floor);
    }
    S_OK
}

static FACTORY_VTABLE: Methods<16> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),            // 0x18 (2020: queue creation)
    open_file as *const (),               // 0x20
    create_status_array as *const (),     // 0x28
    missing::<6> as *const (),            // 0x30 (2020: CPU affinity)
    missing::<7> as *const (),            // 0x38 (2020: debug flags)
    set_staging_buffer_size as *const (), // 0x40
    missing::<9> as *const (),
    create_queue as *const (), // 0x50
    missing::<11> as *const (),
    missing::<12> as *const (),
    missing::<13> as *const (),
    missing::<14> as *const (),
    missing::<15> as *const (),
]);
