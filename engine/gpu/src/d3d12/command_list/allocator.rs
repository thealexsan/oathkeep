use crate::d3d12::object::{
    self, ALLOCATOR, HRESULT, Iid, Object, S_OK, Table, This, guid,
};
use crate::d3d12::resource::PAGEABLE_IIDS;

use super::List;

pub(super) const ALLOCATOR_IIDS: [Iid; 4] = [
    guid(
        0x6102dee4,
        0xaf59,
        0x4b09,
        [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Allocator {
    object: Object,
    kind: u32,
}

pub(crate) extern "system" fn create_allocator(
    _: This,
    kind: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    let allocator = Box::new(Allocator {
        object: Object::new(&ALLOCATOR_FUNCTIONS),
        kind,
    });
    object::hand_out(ALLOCATOR, allocator, iid, &ALLOCATOR_IIDS, out)
}

extern "system" fn allocator_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &ALLOCATOR_IIDS).then_some(this);
    object::answer(ALLOCATOR, iid, found, out)
}
extern "system" fn allocator_release(this: This) -> u32 {
    object::release_boxed::<Allocator>(this)
}
/// Nothing is recorded, so there is nothing to free.
extern "system" fn allocator_reset(_: This) -> HRESULT {
    S_OK
}

/// IUnknown 3, object 4, device child 1, allocator 2.
static ALLOCATOR_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<ALLOCATOR, 24>());
    f[0] = allocator_query as *const ();
    f[2] = allocator_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = allocator_reset as *const (); // 0x40
    f[9] = object::ignore as *const (); // 0x48 allocation callback
    Table(f)
};

#[cfg(windows)]
pub(super) use crate::memory::upload::Upload;

/// The memory of [`allocate_upload`] where no host GPU reads it by address.
#[cfg(not(windows))]
pub(super) struct Upload(Vec<u8>);

#[cfg(not(windows))]
impl Upload {
    fn new(len: usize) -> Option<Upload> {
        Some(Upload(vec![0; len]))
    }
    fn start(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
}

/// Newer lists' 0x4a0 `(size, alignment) -> pointer`: memory the list
/// owns until it is reset, which the CPU writes and the GPU reads at the
/// same address; null on failure. The title's BVH builder puts
/// its instance descriptions there (and wrote through our old
/// `E_NOTIMPL` as a pointer).
pub(super) extern "system" fn allocate_upload(this: This, size: u64, alignment: u64) -> *mut u8 {
    let align = usize::try_from(alignment).unwrap_or(0).max(16);
    let Some(len) = usize::try_from(size).ok().and_then(|s| s.checked_add(align)) else {
        return std::ptr::null_mut();
    };
    if this.is_null() || !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    let Some(mut block) = Upload::new(len) else {
        return std::ptr::null_mut();
    };
    let start = block.start();
    // SAFETY: the offset is below `align`, so `size` bytes fit after it.
    let at = unsafe { start.add(start.align_offset(align)) };
    // SAFETY: the title passes one of our lists.
    let uploads = unsafe { &(*this.cast::<List>()).uploads };
    uploads.lock().unwrap_or_else(|e| e.into_inner()).push(block);
    at
}
