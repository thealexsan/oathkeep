#[cfg(windows)]
mod depth_inline;
#[cfg(windows)]
mod depth_plane;
#[cfg(windows)]
mod depth_prelude;
#[cfg(windows)]
mod host_copy;
#[cfg(windows)]
mod twins;

use std::ffi::c_void;

use super::backing::{Heap, create_heap, heap_iids_first};
use super::create::{create_committed, create_placed};
use super::footprint::{NO_FOOTPRINT, allocation_info, copyable_footprints, subresource};
use super::format::{family, footprint, texture_layout};
#[cfg(windows)]
use super::host_tex::{host_copy, host_texture_as};
#[cfg(windows)]
use super::hosted::{held_only, placed_bytes_from, want_level};
#[cfg(windows)]
use super::placed::{placed_bytes, texture_at};
use super::{AllocationInfo, BUFFER, HeapDesc, HeapProperties, KIB_64, RESOURCE_IIDS, ResourceDesc};
use crate::object::{E_INVALIDARG, HRESULT, Iid, S_OK, This, slot};

pub(super) type Release = extern "system" fn(This) -> u32;
pub(super) type Map = extern "system" fn(This, u32, *const c_void, *mut *mut u8) -> HRESULT;
pub(super) type Address = extern "system" fn(This) -> u64;

pub(super) const PROPERTIES: HeapProperties = HeapProperties {
    kind: 1,
    cpu_pages: 0,
    pool: 0,
    creation_nodes: 0,
    visible_nodes: 0,
};

pub(super) fn buffer(width: u64) -> ResourceDesc {
    ResourceDesc {
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
    }
}

pub(super) fn texture(width: u64, height: u32, format: u32, mips: u16) -> ResourceDesc {
    ResourceDesc {
        dimension: 3,
        width,
        height,
        format,
        mip_levels: mips,
        layout: 0,
        ..buffer(0)
    }
}

pub(super) fn heap(size: u64) -> This {
    let desc = HeapDesc {
        size,
        properties: PROPERTIES,
        alignment: 0,
        flags: 0,
    };
    let mut heap = std::ptr::null();
    assert_eq!(create_heap(std::ptr::null(), &desc, heap_iids_first(), &mut heap), S_OK);
    heap
}

#[test]
fn a8_unorm_is_not_in_the_r8_typeless_family() {
    assert_eq!(family(60), 60);
    for f in 61..=64 {
        assert_eq!(family(f), 60);
    }
    assert_eq!(family(65), 65);
    assert_eq!(family(59), 53);
    assert_eq!(family(66), 66);
}

#[test]
fn heaps_are_zeroed_aligned_memory() {
    let heap = heap(1 << 20);
    // SAFETY: a heap we just made.
    let memory = unsafe { &(*heap.cast::<Heap>()).memory };
    assert_eq!(memory.ptr.as_ptr() as u64 % KIB_64, 0);
    let mut back = std::mem::MaybeUninit::<HeapDesc>::uninit();
    slot::<extern "system" fn(This, *mut HeapDesc)>(heap, 0x40)(heap, back.as_mut_ptr());
    // SAFETY: written above.
    assert_eq!(unsafe { back.assume_init() }.size, 1 << 20);
    assert_eq!(slot::<Release>(heap, 0x10)(heap), 0);

    let bad = HeapDesc {
        size: 1 << 20,
        properties: PROPERTIES,
        alignment: 3,
        flags: 0,
    };
    let mut out = std::ptr::null();
    assert_eq!(create_heap(std::ptr::null(), &bad, heap_iids_first(), &mut out), E_INVALIDARG);
}

#[test]
fn device_setup_heap_properties_and_resources_at_addresses() {
    let device = crate::device::device();
    type Custom = extern "system" fn(*mut HeapProperties, This, u32, u32) -> *mut HeapProperties;
    let mut p = PROPERTIES;
    let back = slot::<Custom>(device, 0xd0)(&mut p, device, 1, 2);
    assert_eq!(back, &raw mut p);
    assert_eq!(
        (p.kind, p.cpu_pages, p.pool, p.creation_nodes, p.visible_nodes),
        (4, 3, 1, 1, 1),
        "upload: custom, write-back, L0"
    );
    slot::<Custom>(device, 0xd0)(&mut p, device, 1, 1);
    assert_eq!(p.cpu_pages, 1, "default: not CPU-visible");

    type At = extern "system" fn(This, u64, *const ResourceDesc, u32, *const c_void, *const Iid, *mut This) -> HRESULT;
    let memory = vec![0u8; 0x1_0000];
    let address = memory.as_ptr() as u64;
    let mut r = std::ptr::null();
    let create = slot::<At>(device, 0x428);
    let desc = buffer(0x1_0000);
    let null = std::ptr::null();
    assert_eq!(create(device, address, &desc, u32::MAX, null, &RESOURCE_IIDS[0], &mut r), 0);
    assert_eq!(slot::<Address>(r, 0x58)(r), address);
    assert_eq!(slot::<Release>(r, 0x10)(r), 0);
    assert_eq!(memory[0xffff], 0);
    assert_ne!(create(device, 0, &desc, u32::MAX, null, &RESOURCE_IIDS[0], &mut r), 0);
}

#[test]
fn sizes_cover_every_mip() {
    let size = |d: &ResourceDesc| footprint(d).map(|i| i.size);
    assert_eq!(size(&buffer(256)), Some(KIB_64));
    let layout = |d: &ResourceDesc| texture_layout(d).unwrap().size;
    let rgba = texture(256, 256, 28, 0);
    assert_eq!(size(&rgba), Some(layout(&rgba).next_multiple_of(4096)));
    let bc7 = footprint(&texture(512, 512, 98, 10));
    let console = AllocationInfo { size: 0x56000, alignment: 0x1000 };
    assert_eq!(bc7, Some(console));
    assert!(layout(&rgba) >= 4 * (256 * 256 * 4) / 3, "every mip");
    assert_eq!(size(&texture(1024, 1024, 71, 1)), Some(512 << 10));
    assert_eq!(size(&texture(16, 16, 66, 1)), None, "1-bit format");
    let linear = ResourceDesc { layout: 1, ..texture(100, 60, 28, 1) };
    assert_eq!(texture_layout(&linear).unwrap().planes[0].layout.desc.swizzle.0, 0);
    let display = ResourceDesc { layout: 0x10a, ..texture(100, 60, 28, 1) };
    assert_eq!(texture_layout(&display).unwrap().planes[0].layout.desc.swizzle.0, 10);

    let mut info = AllocationInfo { size: 0, alignment: 0 };
    let descs = [buffer(100), texture(256, 256, 28, 0)];
    allocation_info(&mut info, std::ptr::null(), 0, 2, descs.as_ptr());
    assert_eq!(
        info,
        AllocationInfo { size: KIB_64 + size(&descs[1]).unwrap(), alignment: KIB_64 }
    );
    allocation_info(&mut info, std::ptr::null(), 0, 1, &texture(1, 1, 999, 1));
    assert_eq!(info.size, u64::MAX);
}

#[test]
fn upload_footprints_follow_the_pc_rules() {
    let null = std::ptr::null();
    let mut layouts = [NO_FOOTPRINT; 2];
    let (mut rows, mut sizes, mut total) = ([0; 2], [0; 2], 0);
    let mut footprints = |desc: &ResourceDesc, first, count| {
        let (l, r, s) = (layouts.as_mut_ptr(), rows.as_mut_ptr(), sizes.as_mut_ptr());
        copyable_footprints(null, desc, first, count, 0, l, r, s, &mut total);
        (layouts, rows, sizes, total)
    };
    let (l, r, s, t) = footprints(&texture(100, 60, 28, 2), 0, 2);
    assert_eq!((l[0].row_pitch, r[0], s[0]), (512, 60, 400));
    assert_eq!(l[1].offset, (512 * 59 + 400u64).next_multiple_of(512));
    assert_eq!((l[1].width, l[1].height, r[1]), (50, 30, 30));
    assert_eq!(t, l[1].offset + 256 * 29 + 200);
    let (_, r, s, _) = footprints(&texture(64, 64, 71, 1), 0, 1);
    assert_eq!((r[0], s[0]), (16, 128));
    let (_, _, _, t) = footprints(&texture(64, 64, 71, 1), 1, 1);
    assert_eq!(t, u64::MAX, "no second subresource");
}

#[test]
fn depth_stencil_footprints_follow_the_pc_plane_order() {
    let expected = [
        (512, 39, 16, 8),
        (4_608, 39, 8, 4),
        (6_656, 39, 16, 8),
        (10_752, 39, 8, 4),
        (12_800, 60, 16, 8),
        (16_896, 60, 8, 4),
        (18_944, 60, 16, 8),
        (23_040, 60, 8, 4),
    ];
    for format in (19..=22).chain(44..=47) {
        let desc = ResourceDesc { depth_or_array_size: 2, ..texture(16, 8, format, 2) };
        let mut layouts = [NO_FOOTPRINT; 8];
        let (mut rows, mut sizes, mut total) = ([0; 8], [0; 8], 0);
        copyable_footprints(
            std::ptr::null(),
            &desc,
            0,
            8,
            512,
            layouts.as_mut_ptr(),
            rows.as_mut_ptr(),
            sizes.as_mut_ptr(),
            &mut total,
        );
        assert_eq!(layouts.map(|l| (l.offset, l.format, l.width, l.height)), expected, "format {format}");
        assert!(layouts.iter().all(|l| l.depth == 1 && l.row_pitch == 512), "format {format}");
        assert_eq!(rows, [8, 4, 8, 4, 8, 4, 8, 4], "format {format}");
        assert_eq!(sizes, [64, 32, 64, 32, 16, 8, 16, 8], "format {format}");
        assert_eq!(total, 24_072, "format {format}");
        assert!(subresource(&desc, 8).is_none(), "format {format}");
    }
}

#[cfg(windows)]
#[test]
fn reserved_host_texture_skips_committed_pool() {
    use windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE;
    let Some(host) = crate::host::host() else {
        return;
    };
    let desc = texture(512, 512, 98, 10);
    let Some(tex) = host_texture_as(&desc, 0, std::ptr::null(), true) else {
        return;
    };
    // SAFETY: live texture.
    let got = unsafe { tex.GetDesc() };
    assert_eq!(got.Layout, D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE);
    assert!(
        crate::tiles::Tiled::make(&host.device, &tex, 10).is_some(),
        "reserved texture must expose valid tile layout to Tiled::make"
    );
}

#[test]
fn committed_and_placed_resources_map_at_their_gpu_address() {
    let null = std::ptr::null();
    let mut committed = null;
    let hr = create_committed(
        null,
        &PROPERTIES,
        0,
        &buffer(256),
        0,
        null.cast(),
        &RESOURCE_IIDS[0],
        &mut committed,
    );
    assert_eq!(hr, S_OK);
    let mut data = std::ptr::null_mut();
    assert_eq!(slot::<Map>(committed, 0x40)(committed, 0, null.cast(), &mut data), S_OK);
    assert_eq!(slot::<Address>(committed, 0x58)(committed), data as u64);
    // SAFETY: the buffer is at least 256 zeroed bytes.
    assert_eq!(unsafe { *data.add(255) }, 0);
    assert_eq!(slot::<Release>(committed, 0x10)(committed), 0);

    let heap = heap(4 * KIB_64);
    let place = |offset, desc: &ResourceDesc, out: &mut This| {
        create_placed(null, heap, offset, desc, 0, null.cast(), &RESOURCE_IIDS[0], out)
    };
    let mut placed = null;
    assert_eq!(place(KIB_64, &buffer(KIB_64), &mut placed), S_OK);
    // SAFETY: a heap we made.
    let base = unsafe { (*heap.cast::<Heap>()).memory.ptr.as_ptr() } as u64;
    assert_eq!(slot::<Address>(placed, 0x58)(placed), base + KIB_64);
    let mut other = null;
    assert_eq!(place(4 * KIB_64, &buffer(1), &mut other), E_INVALIDARG, "past the end");
    assert_eq!(place(100, &buffer(1), &mut other), E_INVALIDARG, "misaligned");
    assert_eq!(slot::<Release>(heap, 0x10)(heap), 1);
    assert_eq!(slot::<Release>(placed, 0x10)(placed), 0);
}

#[cfg(windows)]
#[test]
fn texture_floor_scales_with_vram_budget() {
    use super::budget::texture_floor_bytes;
    const GIB: u64 = 1 << 30;
    assert_eq!(texture_floor_bytes(None), 2 * GIB);
    assert_eq!(texture_floor_bytes(Some(4 * GIB)), GIB);
    assert_eq!(texture_floor_bytes(Some(6 * GIB)), 6 * GIB / 4);
    assert_eq!(texture_floor_bytes(Some(8 * GIB)), 2 * GIB);
    assert_eq!(texture_floor_bytes(Some(12 * GIB)), 2 * GIB);
    assert_eq!(texture_floor_bytes(Some(24 * GIB)), 2 * GIB);
    assert_eq!(texture_floor_bytes(Some(32 * GIB)), 2 * GIB);
}

#[cfg(windows)]
#[test]
fn targets_room_limit_scales_with_vram_budget() {
    use super::budget::targets_room_limit;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    assert_eq!(targets_room_limit(None, None), 3072 * MIB);
    assert_eq!(targets_room_limit(Some(12 * GIB), None), 3072 * MIB);
    assert_eq!(targets_room_limit(Some(24 * GIB), None), 3072 * MIB);
    assert_eq!(targets_room_limit(Some(7147 * MIB), None), 7147 * MIB / 5);
    assert_eq!(targets_room_limit(Some(6 * GIB), None), 6 * GIB / 5);
    assert_eq!(targets_room_limit(Some(2 * GIB), None), 512 * MIB);
    assert_eq!(targets_room_limit(Some(7147 * MIB), Some(0)), 0);
    assert_eq!(targets_room_limit(Some(7147 * MIB), Some(2048)), 2048 * MIB);
    assert_eq!(targets_room_limit(Some(24 * GIB), Some(99999)), 8192 * MIB);
}

#[cfg(windows)]
#[test]
fn a_placed_texture_is_kept_from_the_level_viewed() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    if crate::host::host().is_none() || !held_only() {
        return;
    }
    let desc = [3u32, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0];
    let whole = placed_bytes(&desc).unwrap();
    let part = placed_bytes_from(&desc, 2).unwrap();
    assert!(part * 8 < whole, "{part} of {whole}");
    assert_eq!(placed_bytes_from(&desc, 0), Some(whole));
    // SAFETY: memory of our own that stays.
    let at = unsafe { VirtualAlloc(std::ptr::null(), whole as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    let texture = texture_at(at as u64, &desc).unwrap();
    want_level(texture, 2);
    let copy = host_copy(texture).unwrap();
    // SAFETY: a live texture.
    let d = unsafe { copy.texture.GetDesc() };
    assert_eq!((copy.base, d.Width, d.Height, d.MipLevels), (2, 64, 64, 7));
    want_level(texture, 5);
    assert_eq!(host_copy(texture).unwrap().serial, copy.serial);
    want_level(texture, 0);
    let more = host_copy(texture).unwrap();
    // SAFETY: as above.
    let d = unsafe { more.texture.GetDesc() };
    assert_eq!((more.base, d.Width, d.MipLevels), (0, 256, 9));
    assert!(more.serial != copy.serial && more.grown.load(std::sync::atomic::Ordering::Relaxed));
}
