use std::sync::atomic::{AtomicU32, Ordering};

use oathkeep_formats::texture::{Error, Layout, Swizzle, Texture};

use super::super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::super::xgameruntime::{HRESULT, S_OK};
use super::{
    ALLOW_DEPTH_STENCIL, LayoutOut, MipOut, PackedMips, ResourceDesc, SubresourceTiling, TEXTURE_3D,
};

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;

#[repr(C)]
pub struct Computer {
    pub(super) vtable: &'static Vtable,
    pub(super) refs: AtomicU32,
    pub(super) desc: ResourceDesc,
    pub(super) mips: u32,
    pub(super) texture: Texture,
}

pub(super) type This = *mut Computer;

fn computer<'a>(this: This) -> &'a Computer {
    // SAFETY: called from computer methods only.
    unsafe { &*this }
}

/// The mode a tail mip is reported in: the 256-byte one of its kind (3 for
/// the render kind, which GFX10 lacks), the depth mode itself.
fn tail_swizzle(swizzle: Swizzle) -> u32 {
    match swizzle.0 {
        2 | 6 | 10 | 18 | 22 | 26 => Swizzle::SW_256B_D.0,
        23 | 27 => 3,
        24 => swizzle.0,
        _ => Swizzle::SW_256B_S.0,
    }
}

fn mip_out(desc: &ResourceDesc, l: &Layout, i: usize) -> MipOut {
    let m = &l.mips[i];
    let bpe = u64::from(l.desc.element_bytes);
    let block = l.desc.block;
    let three_d = desc.dimension == TEXTURE_3D;
    let array = u32::from(desc.depth_or_array).max(1);
    let mut out = MipOut {
        pitch_texels: m.pitch * block,
        pitch_bytes: m.pitch * l.desc.element_bytes,
        padded: [m.pitch, m.rows, if three_d { m.depth_pitch } else { array }],
        elements: [m.width, m.height],
        texels: [((desc.width >> i) as u32).max(1), (desc.height >> i).max(1)],
        depth_or_array: if three_d { m.depth } else { array },
        samples: l.desc.samples,
        slice_depth: l.block[2],
        ..MipOut::default()
    };
    let slice = u64::from(m.pitch) * u64::from(m.rows) * bpe;
    let depth = u64::from(if three_d { m.depth_pitch } else { 1 });
    match m.tail {
        None => {
            out.offset = m.offset;
            out.slice_size = slice * u64::from(l.desc.samples);
            out.size = out.slice_size * depth;
            out.alignment = l.alignment as u32;
            out.swizzle = l.desc.swizzle.0;
        }
        Some(t) => {
            out.in_tail = 1;
            out.alignment = 256;
            out.swizzle = tail_swizzle(l.desc.swizzle);
            if l.thick {
                // A 3D tail mip is placed by position, not by offset.
                out.slice_size = l.alignment / u64::from(l.block[2]);
                out.tail_position = t.x | t.y << 11;
            } else {
                out.offset = u64::from(t.offset);
                out.slice_size = slice;
            }
            out.size = out.slice_size * depth;
        }
    }
    out
}

extern "system" fn add_ref(this: This) -> u32 {
    computer(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}

extern "system" fn release(this: This) -> u32 {
    let left = computer(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: made by `Box::into_raw` in `XGCreateTextureComputer`; the
        // last reference is gone.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

extern "system" fn layout(this: This, out: *mut LayoutOut) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let c = computer(this);
    // SAFETY: the title passes room for the layout; every field is plain
    // data, so zeroes are a valid start.
    let out = unsafe {
        out.write_bytes(0, 1);
        &mut *out
    };
    out.size = c.texture.size;
    out.alignment = c.texture.alignment;
    out.mips = c.mips;
    out.planes = c.texture.planes.len() as u32;
    out.dimension = c.desc.dimension;
    // The library calls a depth plane "depth" only beside stencil or
    // without mips.
    let depth =
        c.desc.flags & ALLOW_DEPTH_STENCIL != 0 && (c.texture.planes.len() > 1 || c.mips == 1);
    for (plane, p) in out.plane.iter_mut().zip(&c.texture.planes) {
        let l = &p.layout;
        plane.usage = match (p.stencil, depth) {
            (true, _) => 8,
            (false, true) => 7,
            (false, false) => 1,
        };
        plane.size = l.size;
        plane.slice_size = l.slice_size;
        plane.offset = p.offset;
        plane.alignment = l.alignment;
        plane.element_bytes = l.desc.element_bytes;
        plane.standard_mips = l.first_tail_mip;
        plane.tail_mips = c.mips - l.first_tail_mip;
        for (i, mip) in plane.mips.iter_mut().take(l.mips.len()).enumerate() {
            *mip = mip_out(&c.desc, l, i);
        }
    }
    S_OK
}

/// The plane, its mip and array slice for a subresource index, and the
/// tiled bytes and the linear bytes the copy touches.
struct CopyArgs<'a> {
    layout: &'a Layout,
    mip: u32,
    slice: u32,
    tiled: usize,
    linear: usize,
}

fn copy_args(
    c: &Computer,
    plane: u32,
    subresource: u32,
    row: u32,
    depth: u32,
) -> Option<CopyArgs<'_>> {
    let p = c.texture.planes.get(plane as usize)?;
    let l = &p.layout;
    let (mip, slice) = (subresource % c.mips, subresource / c.mips);
    let m = l.mips.get(mip as usize)?;
    let bytes = m.width as usize * l.desc.element_bytes as usize;
    let depths = l.subresource_depth(mip) as usize;
    let linear = (depths - 1) * depth as usize + (m.height as usize - 1) * row as usize + bytes;
    Some(CopyArgs {
        layout: l,
        mip,
        slice,
        tiled: (p.offset + l.size) as usize,
        linear,
    })
}

fn result(r: Result<(), Error>) -> HRESULT {
    match r {
        Ok(()) => S_OK,
        Err(Error::Unsupported) => E_NOTIMPL,
        Err(_) => E_INVALIDARG,
    }
}

/// `(tiled base, plane, subresource, linear, row pitch, depth pitch)`.
extern "system" fn copy_into(
    this: This,
    tiled: *mut u8,
    plane: u32,
    subresource: u32,
    linear: *const u8,
    row: u32,
    depth: u32,
) -> HRESULT {
    let c = computer(this);
    let Some(a) = copy_args(c, plane, subresource, row, depth) else {
        return E_INVALIDARG;
    };
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let offset = c.texture.planes[plane as usize].offset as usize;
    // SAFETY: the title's memory holds the whole texture and its rows.
    let (tiled, linear) = unsafe {
        (
            std::slice::from_raw_parts_mut(tiled.add(offset), a.tiled - offset),
            std::slice::from_raw_parts(linear, a.linear),
        )
    };
    let r = a
        .layout
        .tile(tiled, a.mip, a.slice, linear, row as usize, depth as usize);
    if r.is_err() {
        tracing::warn!(?r, desc = ?c.desc, subresource, "copy into a texture failed");
    }
    result(r)
}

/// `(linear, plane, subresource, sample, tiled base, row pitch, depth
/// pitch)`.
#[allow(clippy::too_many_arguments)]
extern "system" fn copy_from(
    this: This,
    linear: *mut u8,
    plane: u32,
    subresource: u32,
    _sample: u32,
    tiled: *const u8,
    row: u32,
    depth: u32,
) -> HRESULT {
    let c = computer(this);
    let Some(a) = copy_args(c, plane, subresource, row, depth) else {
        return E_INVALIDARG;
    };
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let offset = c.texture.planes[plane as usize].offset as usize;
    // SAFETY: the title's memory holds the whole texture and its rows.
    let (tiled, linear) = unsafe {
        (
            std::slice::from_raw_parts(tiled.add(offset), a.tiled - offset),
            std::slice::from_raw_parts_mut(linear, a.linear),
        )
    };
    let r = a
        .layout
        .detile(tiled, a.mip, a.slice, linear, row as usize, depth as usize);
    if r.is_err() {
        tracing::warn!(?r, desc = ?c.desc, subresource, "copy out of a texture failed");
    }
    result(r)
}

const TILE: u64 = 64 << 10;

/// `(total tiles, packed mips, tile shape in texels, in: room / out:
/// count, first, subresource tilings)`, in 64 KiB tiles of plane 0, for 2D
/// 64 KiB modes only (others fail, as on the console). The mip tail is the
/// packed part, tile 0 of each slice; tilings list the standard mips of
/// every slice in turn. With a tail the console's library lists two slices
/// at most and zeroes all but the first; so do we (the title reads slice 0
/// only).
extern "system" fn tiling(
    this: This,
    tiles: *mut u32,
    packed: *mut PackedMips,
    shape: *mut [u32; 3],
    count: *mut u32,
    first: u32,
    out: *mut SubresourceTiling,
) -> HRESULT {
    let c = computer(this);
    let l = &c.texture.planes[0].layout;
    if l.alignment != TILE {
        return E_FAIL;
    }
    if l.thick {
        return E_NOTIMPL;
    }
    let standard = l.first_tail_mip;
    let tail = standard < c.mips;
    let tail_tiles = match standard {
        0 => l.slice_size / TILE,
        s if s < c.mips => l.mips[s as usize - 1].offset / TILE,
        _ => 0,
    };
    let block = l.desc.block;
    // SAFETY: each output is null or room for its value; `out` has room
    // for `*count` entries.
    unsafe {
        if let Some(t) = tiles.as_mut() {
            *t = (l.size / TILE) as u32;
        }
        if let Some(p) = packed.as_mut() {
            *p = PackedMips {
                standard: standard as u8,
                packed: (c.mips - standard) as u8,
                tiles: tail_tiles as u32,
                start: 0,
            };
        }
        if let Some(s) = shape.as_mut() {
            *s = [l.block[0] * block, l.block[1] * block, l.block[2]];
        }
        if let Some(n) = count.as_mut() {
            let slices = if tail { l.slices.min(2) } else { l.slices };
            let wanted = (standard * slices).saturating_sub(first).min(*n);
            for i in 0..wanted {
                let (slice, mip) = ((first + i) / standard, (first + i) % standard);
                let m = &l.mips[mip as usize];
                let entry = if tail && slice > 0 {
                    SubresourceTiling {
                        width: 0,
                        height: 0,
                        depth: 0,
                        start: 0,
                    }
                } else {
                    SubresourceTiling {
                        width: m.pitch / l.block[0],
                        height: (m.rows / l.block[1]) as u16,
                        depth: 1,
                        start: ((u64::from(slice) * l.slice_size + m.offset) / TILE) as u32,
                    }
                };
                out.add(i as usize).write(entry);
            }
            *n = wanted;
        }
    }
    S_OK
}

/// Slot 0x30 `(plane, mip, x, y, z or slice, sample, bit position)`: an
/// element's byte offset from the texture's start. The title reads
/// texels of a 128x4 `4KB_S` texture on the CPU through it (title+0x5151c90);
/// an error returned as an offset made it read wild memory. 0 if outside.
extern "system" fn element_offset(
    this: This,
    plane: u32,
    mip: u32,
    x: u64,
    y: u32,
    z: u32,
    _sample: u32,
    bit: *mut u32,
) -> u64 {
    if !bit.is_null() {
        // SAFETY: the caller passes room for the bit position or null.
        unsafe { bit.write(0) };
    }
    let Some(p) = computer(this).texture.planes.get(plane as usize) else {
        return 0;
    };
    let x = u32::try_from(x).unwrap_or(u32::MAX);
    match p.layout.offset(mip, x, y, z) {
        Ok(at) => p.offset + at,
        Err(e) => {
            tracing::warn!(?e, mip, x, y, z, "texel outside its texture");
            0
        }
    }
}

/// Slots the title does not call: size, alignment, offsets, coordinates,
/// view descriptor, tiled-resource query.
extern "system" fn unused(this: This) -> HRESULT {
    tracing::warn!(desc = ?computer(this).desc, "unimplemented texture computer method");
    E_NOTIMPL
}

#[repr(C)]
pub(super) struct Vtable(pub(super) [*const (); 13]);
// SAFETY: function addresses only.
unsafe impl Sync for Vtable {}

pub(super) static VTABLE: Vtable = Vtable([
    add_ref as *const (),        // 0x00
    release as *const (),        // 0x08
    layout as *const (),         // 0x10
    unused as *const (),         // 0x18 size
    unused as *const (),         // 0x20 alignment
    unused as *const (),         // 0x28 mip offset
    element_offset as *const (), // 0x30
    unused as *const (),         // 0x38 element coordinate
    copy_into as *const (),      // 0x40
    copy_from as *const (),      // 0x48
    tiling as *const (),         // 0x50
    unused as *const (),         // 0x58 view descriptor
    unused as *const (),         // 0x60 tiled resource?
]);
