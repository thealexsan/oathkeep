//! Heaps, committed and placed resources, and resource sizes.
//!
//! Heaps and resources are host memory and a resource's GPU address is its
//! CPU address, as on the console; textures also get a host copy, and
//! buffers a host copy when a draw reads them (`shadow`).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

#[cfg(windows)]
mod accounting;
mod backing;
#[cfg(windows)]
mod budget;
mod create;
mod footprint;
mod format;
#[cfg(windows)]
mod host_tex;
#[cfg(windows)]
mod hosted;
#[cfg(windows)]
mod placed;
#[cfg(windows)]
mod sweep;
#[cfg(test)]
mod tests;
#[cfg(windows)]
mod twin;

use std::ffi::c_void;

use crate::d3d12::object::{Iid, Object, RESOURCE, S_OK, Table, This, guid};

#[cfg(windows)]
pub(crate) use accounting::card_accounting;
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) use accounting::{census, targets_census};
#[allow(unused_imports)]
pub use backing::{
    AllocRoutine, FreeRoutine, ManyPageMapper, PageMapper, use_allocator, use_many_page_mapper,
    use_page_mapper,
};
#[allow(unused_imports)]
pub(crate) use backing::{Memory, create_heap, many_page_mapper, page_mapper};
#[cfg(windows)]
pub(crate) use budget::{room, room_lately, targets_room_kept};
pub(crate) use create::{
    create_at_address, create_committed, create_committed3, create_placed, custom_heap_properties,
    register_page_pool, unregister_page_pool,
};
#[allow(unused_imports)]
pub(crate) use footprint::{
    AllocationInfo1, PlacedFootprint, ResourceDesc1, allocation_info, allocation_info2,
    copyable_footprints,
};
#[allow(unused_imports)]
pub(crate) use format::{
    DEPTH_FORMATS, SRGB, element, family, pc_format, target_format, texture_layout, view_format,
};
#[cfg(windows)]
pub(crate) use format::stands_in;
#[cfg(windows)]
#[allow(unused_imports)]
pub(crate) use host_tex::{
    HOST_TEXTURES, OnDemand, first_shown, host_copy, host_resource, host_tiled, host_transition,
    is_resource, made_copy,
};
#[cfg(windows)]
pub(crate) use hosted::{
    HELD, Holds, HostTexture, KINDS, alias_of, held_only, placed_bytes_from, read_from,
    title_fills, want_level,
};
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) use hosted::host_made;
#[cfg(not(windows))]
#[allow(unused_imports)]
pub(crate) use hosted_non_win::{first_mip, read_from};
#[cfg(windows)]
pub(crate) use hosted::{first_mip, host_waits};
#[cfg(windows)]
#[allow(unused_imports)]
pub(crate) use placed::{placed_bytes, placed_filler_twins, texture_at, texture_kind, title_texture};
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) use placed::{placed_textures, texture_at_made, texture_held};
#[cfg(windows)]
pub(crate) use sweep::{SWEPT_US, copy_state, let_go_idle, placed_known};
#[cfg(windows)]
pub(crate) use twin::{
    DEPTH_UNORDERED, PLANES, TWINS, Twin, depth_and_unordered, twin, twin_plane_resource,
    twin_resource,
};
#[cfg(not(windows))]
pub(crate) use twin_non_win::DEPTH_UNORDERED;

#[cfg(not(windows))]
mod hosted_non_win {
    pub(crate) fn first_mip(view: &[u32]) -> u32 {
        const TEXTURE_2D_VIEW: u32 = 4;
        match *view {
            [_, TEXTURE_2D_VIEW, _, _, first, ..] => first.min(15),
            _ => 0,
        }
    }

    pub(crate) fn read_from(layout: &oathkeep_formats::texture::Layout, first: u32) -> u32 {
        if layout.slices != 1 || layout.mips.is_empty() {
            return 0;
        }
        let first = first.min(layout.first_tail_mip).min(layout.mips.len() as u32 - 1);
        if layout.bytes_from(first) < layout.size { first } else { 0 }
    }
}

#[cfg(not(windows))]
mod twin_non_win {
    pub(crate) static DEPTH_UNORDERED: [std::sync::atomic::AtomicU64; 4] =
        [const { std::sync::atomic::AtomicU64::new(0) }; 4];
}

/// The interfaces every device child also answers for.
pub(crate) const PAGEABLE_IIDS: [Iid; 3] = [
    guid(0x63ee58fb, 0x1268, 0x4835, [0x86, 0xda, 0xf0, 0x08, 0xce, 0x62, 0xf0, 0xd6]),
    guid(0x905db94b, 0xa00c, 0x4140, [0x9d, 0xf5, 0x2b, 0x64, 0xca, 0x9e, 0xa3, 0x57]),
    guid(0xc4fec28f, 0x7966, 0x4e95, [0x9f, 0x94, 0xf4, 0x31, 0xcb, 0x56, 0xc3, 0xb8]),
];
pub(super) const RESOURCE_IIDS: [Iid; 4] = [
    guid(0x696442be, 0xa72e, 0x4059, [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad]),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

pub(super) const KIB_64: u64 = 64 << 10;
pub(super) const MIB_4: u64 = 4 << 20;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapProperties {
    pub(super) kind: u32,
    pub(super) cpu_pages: u32,
    pub(super) pool: u32,
    pub(super) creation_nodes: u32,
    pub(super) visible_nodes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    pub(super) size: u64,
    pub(super) properties: HeapProperties,
    pub(super) alignment: u64,
    pub(super) flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResourceDesc {
    pub(super) dimension: u32,
    pub(super) alignment: u64,
    pub(super) width: u64,
    pub(super) height: u32,
    pub(super) depth_or_array_size: u16,
    pub(super) mip_levels: u16,
    pub(super) format: u32,
    pub(super) sample_count: u32,
    pub(super) sample_quality: u32,
    pub(super) layout: u32,
    pub(super) flags: u32,
}

#[cfg(windows)]
impl ResourceDesc {
    /// The texture of this one's levels from `base` on.
    pub(super) fn from_level(&self, base: u32) -> ResourceDesc {
        let mut d = *self;
        if base > 0 {
            d.width = (d.width >> base).max(1);
            d.height = (d.height >> base).max(1);
            d.mip_levels -= base as u16;
        }
        d
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocationInfo {
    pub(super) size: u64,
    pub(super) alignment: u64,
}

const _: () = assert!(size_of::<HeapProperties>() == 20);
const _: () = assert!(size_of::<HeapDesc>() == 48);
const _: () = assert!(size_of::<ResourceDesc>() == 56);

pub(super) const BUFFER: u32 = 1;
pub(super) const TEXTURE_3D: u32 = 4;

/// A resource's memory: its own, or a range of a heap it holds a reference
/// to.
pub(super) enum Backing {
    Committed(Memory),
    Placed { heap: This, offset: u64 },
    /// Memory the title mapped itself, at this address.
    Address(*mut u8),
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let Backing::Placed { heap, .. } = *self {
            backing::heap_release(heap);
        }
    }
}

#[repr(C)]
pub(super) struct Resource {
    pub(super) object: Object,
    pub(super) desc: ResourceDesc,
    pub(super) heap: HeapProperties,
    pub(super) heap_flags: u32,
    pub(super) backing: Backing,
    /// Textures also live on the host GPU (decision 0007); buffers do not yet.
    #[cfg(windows)]
    pub(super) host: hosted::Hosted,
}

/// A texture's console memory and description; `None` for buffers.
pub(crate) fn texture_memory(this: This) -> Option<(*const u8, ResourceDesc)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension != BUFFER).then(|| (r.data().cast_const(), r.desc))
}

/// A buffer's host memory and size; `None` for textures.
pub(crate) fn buffer_bytes(this: This) -> Option<(*mut u8, u64)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    (r.desc.dimension == BUFFER).then(|| (r.data(), r.desc.width))
}

impl Resource {
    pub(super) fn data(&self) -> *mut u8 {
        match self.backing {
            Backing::Committed(ref memory) => memory.ptr.as_ptr(),
            // SAFETY: placement checked the range lies inside the heap.
            Backing::Placed { heap, offset } => unsafe {
                (*heap.cast::<backing::Heap>()).memory.ptr.as_ptr().add(offset as usize)
            },
            Backing::Address(at) => at,
        }
    }
}

pub(super) fn resource<'a>(this: This) -> &'a Resource {
    // SAFETY: called from resource methods only.
    unsafe { &*this.cast::<Resource>() }
}

// The creation methods' own arguments, passed on.
#[allow(clippy::too_many_arguments)]
pub(super) fn hand_out_resource(
    desc: ResourceDesc,
    heap: HeapProperties,
    heap_flags: u32,
    backing: Backing,
    state: u32,
    clear: *const c_void,
    iid: *const Iid,
    out: *mut This,
) -> crate::d3d12::object::HRESULT {
    #[cfg(not(windows))]
    let _ = (state, clear);
    let resource = Box::new(Resource {
        object: Object::new(&RESOURCE_FUNCTIONS),
        desc,
        heap,
        heap_flags,
        backing,
        #[cfg(windows)]
        host: hosted::Hosted::new(state, clear),
    });
    // The host copy waits for the texture's first use (`Hosted`).
    #[cfg(windows)]
    if desc.dimension != BUFFER {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        hosted::register_texture(at, &desc, resource.data());
    }
    #[cfg(windows)]
    if desc.dimension == BUFFER {
        let at = std::ptr::from_ref::<Resource>(&resource) as usize;
        crate::memory::shadow::register(at, resource.data() as u64, desc.width);
    }
    crate::d3d12::object::hand_out(RESOURCE, resource, iid, &RESOURCE_IIDS, out)
}

#[cfg(windows)]
impl Drop for Resource {
    fn drop(&mut self) {
        if self.desc.dimension != BUFFER {
            let address = std::ptr::from_ref::<Resource>(self) as usize;
            hosted::unregister_texture(address, &self.desc, self.data());
        }
        let made = self.host.made.get_mut().unwrap_or_else(|e| e.into_inner()).take();
        #[cfg(feature = "diagnostics")]
        if made.as_ref().is_some_and(|(h, _)| std::sync::Arc::strong_count(h) > 1) {
            static OUTLIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = OUTLIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    n,
                    desc = ?self.desc,
                    "a title texture was released while a list not yet reset names its host copy"
                );
            }
        }
        if let Some((host, twin)) = made {
            crate::backend::retire(host);
            if let Some(twin) = twin {
                crate::backend::retire(twin);
            }
        }
        crate::textures::texture::forget(std::ptr::from_ref::<Resource>(self) as usize);
        crate::descriptors::forget(std::ptr::from_ref::<Resource>(self) as usize);
        if self.desc.dimension == BUFFER {
            crate::memory::shadow::unregister(std::ptr::from_ref::<Resource>(self) as usize, self.data() as u64);
        }
    }
}

extern "system" fn resource_query(this: This, iid: *const Iid, out: *mut This) -> crate::d3d12::object::HRESULT {
    crate::d3d12::object::answer(RESOURCE, iid, crate::d3d12::object::is(iid, &RESOURCE_IIDS).then_some(this), out)
}
extern "system" fn resource_release(this: This) -> u32 {
    crate::d3d12::object::release_boxed::<Resource>(this)
}
extern "system" fn map(this: This, _sub: u32, _read: *const c_void, out: *mut *mut u8) -> crate::d3d12::object::HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(resource(this).data()) };
    }
    S_OK
}
extern "system" fn unmap(_: This, _sub: u32, _written: *const c_void) {}
extern "system" fn resource_desc(this: This, out: *mut ResourceDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(resource(this).desc) };
    }
}
extern "system" fn gpu_address(this: This) -> u64 {
    let r = resource(this);
    let at = r.data() as u64;
    #[cfg(feature = "diagnostics")]
    if at >> 40 != 0 {
        static ONCE: [std::sync::Once; 2] = [std::sync::Once::new(), std::sync::Once::new()];
        let backing = match r.backing {
            Backing::Committed(ref m) => format!("committed, attributes {:?}", m.attributes),
            Backing::Placed { offset, .. } => format!("placed in a heap at {offset:#x}"),
            Backing::Address(_) => "at a title address".into(),
        };
        ONCE[usize::from(r.desc.dimension == BUFFER)].call_once(|| {
            tracing::warn!(address = format_args!("{at:#x}"), backing, desc = ?r.desc, "a GPU address above 40 bits")
        });
    }
    at
}
extern "system" fn heap_properties(
    this: This,
    properties: *mut HeapProperties,
    flags: *mut u32,
) -> crate::d3d12::object::HRESULT {
    let r = resource(this);
    // SAFETY: either pointer may be null; otherwise the title passes room.
    unsafe {
        if let Some(p) = properties.as_mut() {
            *p = r.heap;
        }
        if let Some(f) = flags.as_mut() {
            *f = r.heap_flags;
        }
    }
    S_OK
}

/// IUnknown 3, object 4, device child 1, resource 10 (7 as on PC, then 3
/// console ones).
pub(super) static RESOURCE_FUNCTIONS: Table<32> = {
    let mut f = crate::d3d12::object::d3d12_object(crate::d3d12::object::missing_table::<RESOURCE, 32>());
    f[0] = resource_query as *const ();
    f[2] = resource_release as *const ();
    f[7] = crate::d3d12::object::parent_device as *const (); // 0x38
    f[8] = map as *const (); // 0x40
    f[9] = unmap as *const (); // 0x48
    f[10] = resource_desc as *const (); // 0x50
    f[11] = gpu_address as *const (); // 0x58
    f[14] = heap_properties as *const (); // 0x70
    Table(f)
};

#[cfg(test)]
pub(crate) fn test_buffer(width: u64) -> This {
    let heap = HeapProperties { kind: 1, cpu_pages: 0, pool: 0, creation_nodes: 0, visible_nodes: 0 };
    let desc = ResourceDesc {
        dimension: BUFFER,
        alignment: 0,
        width,
        height: 1,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: 0,
        sample_count: 1,
        sample_quality: 0,
        layout: 1,
        flags: 0,
    };
    let mut out = std::ptr::null();
    let hr = create_committed(std::ptr::null(), &heap, 0, &desc, 0, std::ptr::null(), &RESOURCE_IIDS[0], &mut out);
    assert_eq!(hr, S_OK);
    out
}
