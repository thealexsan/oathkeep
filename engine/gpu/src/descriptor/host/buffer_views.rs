use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use super::host;
use super::diag::{is_texture_view, REFUSED_VIEWS, off_elements_for, twinned, typed_twin_needs_exact, view_bytes, view_home};
use super::null_views::null_buffer_view;
use super::staged::{Kept, element_size, srv_desc, uav_desc};


/// A structured buffer view (`first`, `count` elements of `stride` bytes,
/// `offset` bytes into its host copy) as a raw view of the same bytes:
/// its first dword and its dwords. Translated shaders read untyped buffers
/// as raw bytes at `element * stride + offset` (the stride from the
/// used-resource entry, or from the table by heap slot for a buffer picked
/// by index), and a raw read through a structured view gives zeros on
/// NVIDIA (reported upstream with PR #193; structured reads through raw
/// views were right there). So the host's view is raw whenever it can be:
/// the bytes must start on 16 (the host's raw view alignment) and be whole
/// dwords. A view that cannot stays structured.
pub(super) fn raw_window(offset: u64, first: u64, count: u32, stride: u32) -> Option<(u64, u32)> {
    if stride == 0 {
        return None;
    }
    let start = first.checked_mul(u64::from(stride))?.checked_add(offset)?;
    let bytes = u64::from(count) * u64::from(stride);
    (start % 16 == 0 && bytes % 4 == 0)
        .then(|| u32::try_from(bytes / 4).ok().map(|dwords| (start / 4, dwords)))
        .flatten()
}

/// Whether the host accepts a buffer view with these fields of a buffer of
/// `width` bytes. It removes the device for one it does not (view creation
/// returns nothing), and the console is laxer: the title's heap holds
/// views past their memory's end, of no elements, of typeless formats
/// without the raw flag, with strides the host has no structured buffers
/// for. Since every buffer view of the heap is made for shaders that pick
/// them by index, one such view ended the run (2026-11-24, run 10).
pub(super) fn buffer_view_ok(format: DXGI_FORMAT, first: u64, count: u32, stride: u32, raw: bool, width: u64) -> bool {
    use crate::resource::family;
    let f = format.0 as u32;
    let element = if raw {
        // raw: R32 typeless, counted in dwords, starting on 16 bytes
        if format != DXGI_FORMAT_R32_TYPELESS || stride != 0 || !first.is_multiple_of(4) {
            return false;
        }
        4
    } else if stride > 0 {
        // structured: no format, a stride the host takes
        if format != DXGI_FORMAT_UNKNOWN || stride > 2048 || !stride.is_multiple_of(4) {
            return false;
        }
        u64::from(stride)
    } else {
        // typed: a format with a size that is not typeless
        let typeless = family(f) == f && family(f + 1) == f;
        match crate::resource::element(f) {
            Some((bytes, 1)) if f != 0 && !typeless && bytes > 0 => bytes,
            _ => return false,
        }
    };
    count > 0
        && first
            .checked_add(u64::from(count))
            .and_then(|end| end.checked_mul(element))
            .is_some_and(|end| end <= width)
}

/// Raw twins ([`raw_twins`]) made since the last frame line, and those of them that
/// do not begin on 16 bytes, as a raw view must ([`move_raw`]), and those of
/// these that could not be moved there (null).
pub(crate) static TWINS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Moves the raw view at `at`, a slot of our heaps the card does not see,
/// which shows `bytes` from `start` on (GPU address), `by` bytes on: for a
/// twin whose view begins between two multiples of 16, where the host makes
/// no raw view. An AMD descriptor of a raw buffer is its address (48 bits),
/// its bytes and flags; only one that says just that is changed, and only
/// where what the handle points at is the descriptor. False: not moved.
pub(crate) fn move_raw(at: D3D12_CPU_DESCRIPTOR_HANDLE, start: u64, bytes: u32, by: u32) -> bool {
    // (Off dwords too, as the typed view it stands for begins there: a view
    // of 16 bit values at an odd one.)
    if by >= bytes {
        return false;
    }
    let mut words = [0u8; 16];
    if !crate::host::read_into(at.ptr as *const u8, &mut words) {
        return false;
    }
    let word = |i: usize| u32::from_le_bytes([words[4 * i], words[4 * i + 1], words[4 * i + 2], words[4 * i + 3]]);
    let address = u64::from(word(0)) | u64::from(word(1)) << 32;
    if address != start || word(2) != bytes || start >> 48 != 0 {
        return false;
    }
    let moved = start + u64::from(by);
    // SAFETY: this thread's own slot (it has slots of its own: the caller
    // asks only for those), read just now.
    unsafe {
        let words = at.ptr as *mut u32;
        words.write(moved as u32);
        words.add(1).write((moved >> 32) as u32);
        words.add(2).write(bytes - by);
    }
    true
}

/// The bytes a buffer view the host takes ([`buffer_view_ok`]) shows as a raw
/// view (first dword, dwords, and the bytes past that dword it begins: a raw
/// view begins on 16 bytes); of a buffer of `width` bytes.
pub(super) fn twin_window(format: DXGI_FORMAT, first: u64, count: u32, stride: u32, raw: bool, width: u64) -> Option<(u64, u32, u32)> {
    use std::sync::atomic::Ordering::Relaxed;
    TWINS[0].fetch_add(1, Relaxed);
    let element = if raw {
        4
    } else if stride > 0 {
        u64::from(stride)
    } else {
        crate::resource::element(format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let (start, end) = (first * element, ((first + u64::from(count)) * element).next_multiple_of(4));
    let end = end.min(width & !3);
    let from = start & !15;
    if end <= start {
        return None;
    }
    let dwords = u32::try_from((end - from) / 4).ok()?;
    if from != start {
        TWINS[1].fetch_add(1, Relaxed);
    }
    Some((from / 4, dwords, (start - from) as u32))
}

pub(super) fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<(D3D12_CPU_DESCRIPTOR_HANDLE, bool)>,
) -> Option<(Kept, Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)>)> {
    let mut shadow = crate::shadow::find(address, len)?;
    let mut offset = shadow.offset(address);
    let unit = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let own = unsafe { &mut desc.Anonymous.Buffer };
    let start = own
        .FirstElement
        .checked_mul(unit)
        .and_then(|off| address.checked_add(off))?;
    let view_len = u64::from(own.NumElements).checked_mul(unit)?;
    let kind = (own.StructureByteStride, own.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0, desc.Format.0 as u32);
    let home = view_home(shadow.clone(), start, view_len, kind);
    if !std::sync::Arc::ptr_eq(&home, &shadow) {
        // Its own bytes: it begins where they do.
        (shadow, offset) = (home, 0);
        own.FirstElement = 0;
    }
    // A structured view as a raw view of the same bytes (see `raw_window`).
    // SAFETY: a buffer view's union member.
    let structured = unsafe { desc.Anonymous.Buffer };
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| raw_window(offset, structured.FirstElement, structured.NumElements, structured.StructureByteStride))
        .flatten();
    if let Some((first, count)) = window {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
        desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
            FirstElement: first,
            NumElements: count,
            StructureByteStride: 0,
            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
        };
    }
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    if window.is_none() {
        if element == 0 || offset % element != 0 {
            once!(
                offset,
                element,
                "buffer view not on an element of its host copy"
            );
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    let (first, count, stride) = (buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride);
    let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
    let width = shadow.width();
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "buffer view the host would refuse left null"
        );
        return None;
    }
    // The console has no component mapping; one the host does not know is the default's.
    if desc.Shader4ComponentMapping != D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING {
        desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    if let Some((twin, cpu_only)) = twin {
        let window = twin_window(desc.Format, first, count, stride, raw, width);
        if let Some((first, count, by)) = window {
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
            desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
                FirstElement: first,
                NumElements: count,
                StructureByteStride: 0,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
            };
            // SAFETY: a live buffer and a slot of our heap.
            let start = unsafe {
                host.device
                    .CreateShaderResourceView(&shadow.buffer, Some(&desc), twin);
                shadow.buffer.GetGPUVirtualAddress() + first * 4
            };
            if by > 0 && !(cpu_only && move_raw(twin, start, count.saturating_mul(4), by)) {
                TWINS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                null_buffer_view(twin);
            }
        } else {
            null_buffer_view(twin);
        }
    }
    let kept = Some(shadow.buffer.clone());
    let copy = (view_len > 0).then_some((shadow, start, view_len));
    Some((kept, copy))
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
pub(super) fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<(D3D12_CPU_DESCRIPTOR_HANDLE, bool)>,
) -> Option<(Kept, Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)>)> {
    let mut shadow = crate::shadow::find(address, len)?;
    let unit = {
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        if b.StructureByteStride > 0 {
            u64::from(b.StructureByteStride)
        } else if b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
            4
        } else {
            crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
        }
    };
    // SAFETY: a buffer view's union member.
    let own = unsafe { &mut desc.Anonymous.Buffer };
    let start = own
        .FirstElement
        .checked_mul(unit)
        .and_then(|off| address.checked_add(off))?;
    let view_len = u64::from(own.NumElements).checked_mul(unit)?;
    let kind = (own.StructureByteStride, own.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, desc.Format.0 as u32);
    let home = view_home(shadow.clone(), start, view_len, kind);
    let mut address = address;
    if !std::sync::Arc::ptr_eq(&home, &shadow) {
        // Its own bytes: it begins where they do.
        (shadow, address) = (home, start);
        own.FirstElement = 0;
    }
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let offset = shadow.offset(address);
    // A structured view as a raw view of the same bytes (see `raw_window`).
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| raw_window(offset, buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride))
        .flatten();
    if let Some((first, count)) = window {
        buffer.FirstElement = first;
        buffer.NumElements = count;
        buffer.StructureByteStride = 0;
        buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    } else {
        if element == 0 || offset % element != 0 {
            once!(offset, element, "unordered buffer view not on an element of its host copy");
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    buffer.CounterOffsetInBytes = 0;
    let (first, count, stride) = (buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride);
    let raw = buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
    if window.is_some() {
        // (Left out at first: every structured unordered access view was then
        // refused by `buffer_view_ok` and left null, runs 15 and 16.)
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
    }
    let width = shadow.width();
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "unordered buffer view the host would refuse left null"
        );
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), at)
    };
    if let Some((twin, cpu_only)) = twin {
        let window = twin_window(desc.Format, first, count, stride, raw, width);
        if let Some((first, count, by)) = window {
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
            desc.Anonymous.Buffer = D3D12_BUFFER_UAV {
                FirstElement: first,
                NumElements: count,
                StructureByteStride: 0,
                CounterOffsetInBytes: 0,
                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
            };
            // SAFETY: a live buffer and a slot of our heap.
            let start = unsafe {
                host.device
                    .CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), twin);
                shadow.buffer.GetGPUVirtualAddress() + first * 4
            };
            if by > 0 && !(cpu_only && move_raw(twin, start, count.saturating_mul(4), by)) {
                TWINS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                null_buffer_view(twin);
            }
        } else {
            null_buffer_view(twin);
        }
    }
    let kept = Some(shadow.buffer.clone());
    let copy = (view_len > 0).then_some((shadow, start, view_len));
    Some((kept, copy))
}
