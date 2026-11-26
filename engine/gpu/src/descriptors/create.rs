//! View and sampler creation, descriptor copying, and deferred texture-slot tracking.

use super::increment;
use super::view::{VIEW_TAG, View, words_for};
#[cfg(windows)]
use super::{Heap, SHADER_HEAPS, mirror, view};
use crate::d3d12::object::This;

/// Title slots (their handles) written with a view of a texture object
/// that had no host copy then, by texture. A title texture gets its host
/// copy on first use (`resource.rs` `Hosted`), and the title
/// writes each texture's views into its view heap when it creates the
/// texture: making those host slots would make every host copy at once.
/// They are made when the texture is: translated shaders index the heap
/// directly, so a slot is not always made again by the draw that reads it.
#[cfg(windows)]
static WAITING: std::sync::Mutex<std::collections::BTreeMap<usize, Vec<u64>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Textures given their host copy since [`make_waiting`] last ran.
#[cfg(windows)]
static MADE_LATE: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
pub(super) fn forget_waiting(resource: usize) {
    WAITING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&resource);
}

/// Title texture `resource` was given its host copy (called under that
/// texture's own lock: nothing else is locked here).
#[cfg(windows)]
pub(crate) fn texture_made(resource: usize) {
    MADE_LATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(resource);
}

/// The slot at `handle` was written with a view of texture `resource`,
/// which has no host copy: it waits for one.
#[cfg(windows)]
pub(in crate::descriptors) fn wait_for(resource: usize, handle: u64) {
    /// A texture with more slots waiting than this is in use for all we
    /// know (its views are being copied about): its host copy is made.
    const MOST: usize = 64;
    let many = {
        let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
        let slots = waiting.entry(resource).or_default();
        if !slots.contains(&handle) {
            slots.push(handle);
        }
        slots.len() > MOST
    };
    if many {
        // Makes it, and notes it made.
        let _ = crate::d3d12::resource::host_copy(resource as This);
    } else if !crate::d3d12::resource::host_waits(resource as This) {
        // Made by another thread since the slot was looked at: that making
        // may have been taken by `make_waiting` before this slot was noted.
        texture_made(resource);
    }
}

/// Makes the host slots that waited for a texture's host copy, for the
/// textures made since the last call. Called before a submission runs, by
/// the thread that submits: a slot is read when a list runs.
#[cfg(windows)]
pub(crate) fn make_waiting() {
    let made = std::mem::take(&mut *MADE_LATE.lock().unwrap_or_else(|e| e.into_inner()));
    if made.is_empty() {
        return;
    }
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    for resource in made {
        let handles = WAITING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&resource);
        for handle in handles.into_iter().flatten() {
            // Only if the slot still holds a view of that texture: the
            // title may have written it since (and a view of another
            // texture without a host copy must go on waiting).
            // (Looked up first: the handle is read only while its heap is
            // there.)
            // (The heap stays while the lock is held: `mirror`.)
            let Some(&(_, _, at)) = heaps.iter().find(|&&(start, end, _)| (start..end).contains(&handle)) else {
                continue;
            };
            // SAFETY: a registered heap, live while the lock is held.
            let heap = unsafe { &*(at as *const Heap) };
            let Some(index) = heap.index(handle) else {
                continue;
            };
            let same = match view(handle) {
                Some(View::Shader(r, _) | View::Unordered(r, _, _)) => r == resource,
                _ => false,
            };
            if same && let Some(Some(host)) = heap.host.get() {
                host.mirror(index, handle, true);
            }
        }
    }
}

pub(in crate::descriptors) fn record(handle: u64, view: View) {
    if handle == 0 {
        return;
    }
    let [i, generation] = words_for(view);
    // SAFETY: the handle is a descriptor slot, at least 16 bytes.
    unsafe { (handle as *mut [u32; 3]).write_unaligned([VIEW_TAG, i, generation]) };
    #[cfg(windows)]
    mirror(handle);
}

/// A description of `N` words, if the title passes one.
fn words<const N: usize>(desc: *const [u32; N]) -> Option<[u32; N]> {
    // SAFETY: the title passes a description or null.
    (!desc.is_null()).then(|| unsafe { desc.read_unaligned() })
}

/// Device method.
pub(crate) extern "system" fn create_cbv(_: This, desc: *const [u32; 4], handle: u64) {
    if let Some([lo, hi, size, _]) = words(desc) {
        let address = u64::from(lo) | u64::from(hi) << 32;
        record(handle, View::Constant(address, size));
    }
}

/// Device method.
pub(crate) extern "system" fn create_srv(
    _: This,
    resource: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    record(handle, View::Shader(resource as usize, words(desc)));
}

/// Device method.
pub(crate) extern "system" fn create_uav(
    _: This,
    resource: This,
    counter: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    let view = View::Unordered(resource as usize, counter as usize, words(desc));
    record(handle, view);
}

/// Device methods 0x230 and 0x238: shader resource and unordered access
/// views of memory the title placed itself, `(resource desc, view desc,
/// handle)`.
pub(crate) extern "system" fn create_placed_srv(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (words(resource), words(desc)) {
        record(handle, View::Placed(false, resource, desc));
    }
}

pub(crate) extern "system" fn create_placed_uav(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (words(resource), words(desc)) {
        record(handle, View::Placed(true, resource, desc));
    }
}

/// Device method.
pub(crate) extern "system" fn create_sampler(_: This, desc: *const [u32; 13], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::Sampler(desc));
    }
}

/// Device method 0x1e8.
pub(crate) extern "system" fn create_sampler_x(_: This, desc: *const [u32; 27], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::ConsoleSampler(desc));
    }
}

/// Device method: `count` descriptors of `kind` from `src` to `dst`.
pub(crate) extern "system" fn copy_simple(_: This, count: u32, dst: u64, src: u64, kind: u32) {
    let Some(step) = increment(kind).map(u64::from) else {
        return;
    };
    let len = u64::from(count) * step;
    if dst != 0 && src != 0 {
        // SAFETY: both ranges are descriptor slots the title owns.
        #[cfg(windows)]
        unsafe {
            crate::memory::title_write::put(dst as *mut u8, src as *const u8, len as usize)
        };
        // SAFETY: as above.
        #[cfg(not(windows))]
        unsafe {
            std::ptr::copy(src as *const u8, dst as *mut u8, len as usize)
        };
        #[cfg(windows)]
        for i in 0..u64::from(count) {
            mirror(dst + i * step);
        }
    }
}

/// Device method: ranges of descriptors (start and count each) gathered
/// from the sources and scattered to the destinations, in order.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) extern "system" fn copy(
    _: This,
    dst_ranges: u32,
    dst_starts: *const u64,
    dst_sizes: *const u32,
    src_ranges: u32,
    src_starts: *const u64,
    src_sizes: *const u32,
    kind: u32,
) {
    let Some(step) = increment(kind) else {
        return;
    };
    // Each range as (start, count); a null size array means one descriptor
    // per range.
    let ranges = |n: u32, starts: *const u64, sizes: *const u32| -> Vec<(u64, u32)> {
        (0..n as usize)
            .map(|i| {
                // SAFETY: the title passes `n` starts and sizes (or null sizes).
                unsafe {
                    (
                        *starts.add(i),
                        if sizes.is_null() { 1 } else { *sizes.add(i) },
                    )
                }
            })
            .collect()
    };
    if dst_starts.is_null() || src_starts.is_null() {
        return;
    }
    let src = ranges(src_ranges, src_starts, src_sizes);
    let mut dst = ranges(dst_ranges, dst_starts, dst_sizes).into_iter();
    let (mut to, mut room) = dst.next().unwrap_or((0, 0));
    for (mut from, mut left) in src {
        while left > 0 {
            while room == 0 {
                let Some(next) = dst.next() else { return };
                (to, room) = next;
            }
            let n = left.min(room);
            copy_simple(std::ptr::null(), n, to, from, kind);
            let bytes = u64::from(n) * u64::from(step);
            (to, from, room, left) = (to + bytes, from + bytes, room - n, left - n);
        }
    }
}
