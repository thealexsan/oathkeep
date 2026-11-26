use std::sync::Mutex;
use super::{View, super::record_at, super::view_at};
#[cfg(feature = "diagnostics")]
use super::flips::BEFORE;
use windows::Win32::Graphics::Direct3D12::*;
use crate::host::host;
use crate::descriptor::with_view_heap;
use super::staged::{element_size, srv_desc, uav_desc};

/// Whether `view` is of a texture the title may fill itself: a texture it
/// placed in memory, or a texture object.
pub(crate) fn is_texture_view(view: &View) -> bool {
    match view {
        View::Placed(false, resource, _) => resource[0] != 1,
        View::Shader(r, _) => crate::resource::buffer_bytes(*r as crate::object::This).is_none(),
        _ => false,
    }
}

/// Buffer views left null because the host would refuse them
/// ([`buffer_view_ok`]), since the start.
pub(crate) static REFUSED_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether the host device is gone, and why (the first time it is asked
/// after that): for passes that make many views, to name the one that did
/// it.
pub(crate) fn device_removed() -> Option<String> {
    let host = host()?;
    // SAFETY: a plain query.
    let reason = unsafe { host.device.GetDeviceRemovedReason() };
    reason.err().map(|e| e.to_string())
}

/// Writes of title view-heap slots with a placed texture view and with a
/// view of a texture object, since the start (diagnostic counts).
pub(crate) static PLACED_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static OBJECT_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes per element and typed integer format bits of a buffer view, decoded
/// in a single pass over the view descriptor; `(None, 0)` if not a buffer view.
pub(super) fn view_stride_and_kind(view: &View) -> (Option<u32>, u32) {
    use oathkeep_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
    let Some((stride, raw, format)) = view_elements(view) else {
        return (None, 0);
    };
    let bytes = if stride > 0 {
        stride
    } else if raw {
        1
    } else {
        crate::resource::element(format).map_or(0, |(bytes, _)| bytes as u32)
    };
    let kind = if stride == 0 && !raw {
        match format {
            3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => VIEW_INTEGER,
            4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => VIEW_INTEGER | VIEW_SIGNED,
            _ => 0,
        }
    } else {
        0
    };
    (Some(bytes), kind)
}

/// Packed [`oathkeep_shader::emit::SLOT_STRIDES`] word for `view`.
pub(super) fn view_stride_word(view: &View) -> u32 {
    let (stride, kind) = view_stride_and_kind(view);
    stride.unwrap_or(0) | kind
}

#[cfg(test)]
pub(super) fn view_stride(view: &View) -> Option<u32> {
    view_stride_and_kind(view).0
}

#[cfg(test)]
pub(super) fn view_kind(view: &View) -> u32 {
    view_stride_and_kind(view).1
}

/// A buffer view's structure stride, whether it is raw, and its format;
/// `None` if `view` is not a buffer view.
pub(super) fn view_elements(view: &View) -> Option<(u32, bool, u32)> {
    const BUFFER: u32 = 1;
    let (words, unordered): (&[u32; 10], bool) = match view {
        View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
        View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
        View::Placed(unordered, resource, d) if resource[0] == BUFFER => {
            (d[..10].try_into().ok()?, *unordered)
        }
        _ => return None,
    };
    // SAFETY: buffer views' union members.
    Some(unsafe {
        if unordered {
            let d = uav_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, d.Format.0 as u32)
        } else {
            let d = srv_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0, d.Format.0 as u32)
        }
    })
}

/// Whether a buffer view that begins `offset` bytes into its host copy begins
/// where no host view can (or where it cannot promote to a raw view).
///
/// - Raw views require 16-byte start alignment.
/// - Structured views are promoted to raw (`raw_window`) whenever they start on
///   16 bytes because translated shaders perform raw byte loads through them,
///   and NVIDIA returns zeros for raw reads through a structured view (even when
///   `offset` is a multiple of `stride`, e.g. stride 8 at offset 8 or stride 12
///   at offset 12). Giving every non-16-byte-aligned structured view an exact
///   copy guarantees 16-byte alignment (`offset == 0`) so `raw_window` always
///   promotes it and its raw twin never becomes null.
/// - Typed views require element alignment (and 16-byte alignment when
///   `need_16_for_typed` is set for raw twins on adapters without `move_raw`).
pub(super) fn off_elements_for(offset: u64, stride: u32, raw: bool, element: u64, need_16_for_typed: bool) -> bool {
    if raw {
        !offset.is_multiple_of(16)
    } else if stride > 0 {
        !offset.is_multiple_of(16)
    } else {
        (element > 0 && !offset.is_multiple_of(element))
            || (need_16_for_typed && !offset.is_multiple_of(16))
    }
}

#[cfg(test)]
pub(super) fn off_elements(offset: u64, stride: u32, raw: bool, element: u64) -> bool {
    off_elements_for(offset, stride, raw, element, false)
}

/// Whether raw twins are active on an adapter whose CPU descriptors cannot be
/// shifted in-place by [`move_raw`] (non-AMD adapters with >2M view heaps):
/// typed views off 16 bytes then also take an aligned [`crate::shadow::exact`] copy.
pub(super) fn typed_twin_needs_exact() -> bool {
    if oathkeep_shader::emit::raw_views() == 0 {
        return false;
    }
    static NON_AMD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NON_AMD.get_or_init(|| host().is_some_and(|h| !crate::indirect::amd(&h.device)))
}

/// Whether `view` has a raw twin when its host slot is made ([`raw_twins`]):
/// a buffer view or a constant buffer's.
pub(super) fn twinned(view: &View) -> bool {
    matches!(view, View::Constant(..)) || view_elements(view).is_some()
}

/// Whether views off their copy's elements get a copy of their own bytes. On by
/// default; `OATHKEEP_VIEW_BYTES=0` leaves them null, as before.
pub(super) fn view_bytes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_VIEW_BYTES").is_none_or(|v| v != "0"))
}

/// The copy the buffer view of `[start, start + len)` is made on: `shadow`, the one
/// its buffer has, or one of the view's own bytes ([`off_elements_for`]).
pub(super) fn view_home(
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    start: u64,
    len: u64,
    (stride, raw, format): (u32, bool, u32),
) -> std::sync::Arc<crate::shadow::Shadow> {
    let Some(offset) = shadow.offset_in(start).map(|_| shadow.offset(start)) else {
        return shadow;
    };
    if !view_bytes() {
        return shadow;
    }
    let element = crate::resource::element(format).map_or(0, |(bytes, _)| bytes);
    if !off_elements_for(offset, stride, raw, element, typed_twin_needs_exact()) {
        return shadow;
    }
    crate::shadow::exact(start, len).unwrap_or(shadow)
}

/// For device-removed reports: what the slots of the title's view heap that
/// the words of user data may name hold now ([`crate::host::indirects`]).
#[cfg(feature = "diagnostics")]
pub(crate) fn describe_slots(words: &[u32]) -> Vec<(usize, u32, String)> {
    with_view_heap(|host| {
        crate::host::indirects::slots(words, host.count, 6)
            .into_iter()
            .take(24)
            .map(|(word, slot)| (word, slot, host.describe_slot(slot).unwrap_or_default()))
            .collect()
    })
    .unwrap_or_default()
}

/// For `crate::book`: the bytes of up to `n` descriptors of the view heap from
/// slot `at` on (from the first twin on, where `at` lies before it and past
/// the others), as the card's heap has them: the first slot, the bytes, and
/// the slot behind the last one worth reading.
#[cfg(feature = "diagnostics")]
pub(crate) fn shown_bytes(at: u32, n: u32) -> Option<(u32, Vec<u8>, u32)> {
    with_view_heap(|host| {
        let own = host.count + host.dynamic;
        let end = if host.raw == 0 { own } else { host.raw + own };
        let at = if at >= own && at < host.raw { host.raw } else { at };
        let stop = if at < own { own } else { end };
        let n = n.min(stop.saturating_sub(at));
        if n == 0 || host.step != 32 {
            return None;
        }
        let mut bytes = vec![0u8; n as usize * 32];
        crate::host::read_into(host.cpu(at).ptr as *const u8, &mut bytes).then_some((at, bytes, end))
    })?
}

/// For `crate::book`: what our books say of a slot of the view heap.
#[cfg(feature = "diagnostics")]
pub(crate) fn slot_story(slot: u32) -> String {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::core::Interface;
    with_view_heap(|host| {
        let (index, twin) = if host.raw != 0 && slot >= host.raw { (slot - host.raw, true) } else { (slot, false) };
        let mut text = if index < host.count {
            format!("title slot {index}, made from {}", host.describe_slot(index).unwrap_or_default())
        } else {
            let taken = host.next.lock().unwrap_or_else(|e| e.into_inner()).1.get((index - host.count) as usize).copied();
            format!("dynamic slot {} (taken {taken:?})", index - host.count)
        };
        if twin {
            let made = host.twins.get(index as usize).map(|t| t.load(Relaxed));
            text.push_str(&format!(", its raw twin (made {made:?})"));
        }
        if let Some(at) = host.written.get(slot as usize) {
            text.push_str(&format!(", written last at {:.1} s", f64::from(at.load(Relaxed)) / 1e3));
        }
        let before = BEFORE.lock().unwrap_or_else(|e| e.into_inner()).get(&index).copied();
        if let Some((was, at, (started, finished), by)) = before {
            let what = match view_at(was as u32, (was >> 32) as u32) {
                _ if was == 0 => "nothing (or the neutral texture)".into(),
                None => "a view that is gone".into(),
                Some(view) => format!("{view:?}").chars().take(160).collect::<String>(),
            };
            text.push_str(&format!(
                ", changed ({by}) at {:.3} s with submissions {started} begun and {finished} finished, showed before: {what}",
                at as f64 / 1e3
            ));
        }
        let kept = host.kept_in(index);
        match kept {
            None => text.push_str(", holds nothing"),
            Some(resource) => {
                // SAFETY: a resource the slot holds.
                let d = unsafe { resource.GetDesc() };
                let gpu = if d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
                    // SAFETY: as above.
                    unsafe { resource.GetGPUVirtualAddress() }
                } else {
                    0
                };
                text.push_str(&format!(
                    ", holds {:#x}: dimension {} {}x{}x{} levels {} f{} at {gpu:#x}, {} references",
                    resource.as_raw() as usize,
                    d.Dimension.0,
                    d.Width,
                    d.Height,
                    d.DepthOrArraySize,
                    d.MipLevels,
                    d.Format.0,
                    crate::host::references(&resource)
                ));
            }
        }
        text
    })
    .unwrap_or_default()
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
pub(super) static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!("{:#x} past its end (slot {})", va - start - bytes, (va - start) / step)
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

