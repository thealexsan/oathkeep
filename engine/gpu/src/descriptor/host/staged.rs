use std::sync::Mutex;
use windows::Win32::Graphics::Direct3D12::*;
use super::{View, host};
use super::diag::{twinned, view_bytes, view_elements, view_home};

/// and two of samplers (`which` 0 or 1): descriptors are made there and
/// copied whole into the heaps it does see. Made in place they are seen half
/// written: an RX 7900 XTX saw a texture of no size in 4 % of its reads of
/// slots whose views a thread made over and over (the driver clears the
/// descriptor and fills it step by step), and none in 8,500 million with the
/// same descriptors copied, three times as fast
/// (`indirect::tests::descriptors_written_under_readers`). Title threads and
/// our passes rewrite slots while submissions run whose shaders pick them;
/// three device losses of 2026-11-25 (play-1125-0203, n02, n03) were reads of
/// memory no view names. `OATHKEEP_VIEW_COPY=0`: made in place, as before.
pub(super) fn staged(sampler: bool, which: u32) -> Option<D3D12_CPU_DESCRIPTOR_HANDLE> {
    /// Threads at a time that have slots; more make views in place.
    const THREADS: u32 = 4096;
    struct Stage {
        starts: [usize; 2],
        steps: [usize; 2],
        _heaps: [ID3D12DescriptorHeap; 2],
    }
    // SAFETY: descriptor heaps are free-threaded; each thread has slots of
    // its own.
    unsafe impl Send for Stage {}
    // SAFETY: as above.
    unsafe impl Sync for Stage {}
    static STAGE: std::sync::OnceLock<Option<Stage>> = std::sync::OnceLock::new();
    /// Places given back, and how many were ever given out.
    static PLACES: Mutex<(Vec<u32>, u32)> = Mutex::new((Vec::new(), 0));
    struct Place(Option<u32>);
    impl Drop for Place {
        fn drop(&mut self) {
            if let Some(place) = self.0 {
                PLACES.lock().unwrap_or_else(|e| e.into_inner()).0.push(place);
            }
        }
    }
    thread_local! {
        static PLACE: Place = {
            let mut places = PLACES.lock().unwrap_or_else(|e| e.into_inner());
            Place(places.0.pop().or_else(|| {
                (places.1 < THREADS).then(|| {
                    places.1 += 1;
                    places.1 - 1
                })
            }))
        };
    }
    let stage = STAGE
        .get_or_init(|| {
            if std::env::var_os("OATHKEEP_VIEW_COPY").is_some_and(|v| v == "0") {
                return None;
            }
            let host = host()?;
            let make = |kind: D3D12_DESCRIPTOR_HEAP_TYPE| -> Option<ID3D12DescriptorHeap> {
                // SAFETY: plain heap creation.
                unsafe {
                    host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: 2 * THREADS,
                        ..Default::default()
                    })
                }
                .inspect_err(|e| tracing::warn!(error = %e, "no heap to make views in: they are made in place"))
                .ok()
            };
            let kinds = [D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER];
            let heaps = [make(kinds[0])?, make(kinds[1])?];
            // SAFETY: live heaps, plain queries.
            let (starts, steps) = unsafe {
                (
                    [0, 1].map(|i: usize| heaps[i].GetCPUDescriptorHandleForHeapStart().ptr),
                    kinds.map(|kind| host.device.GetDescriptorHandleIncrementSize(kind) as usize),
                )
            };
            Some(Stage { starts, steps, _heaps: heaps })
        })
        .as_ref()?;
    // (A thread that is ending has none any more.)
    let place = PLACE.try_with(|p| p.0).ok().flatten()?;
    let kind = usize::from(sampler);
    Some(D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: stage.starts[kind] + (2 * place + which) as usize * stage.steps[kind],
    })
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
pub(super) fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

pub(super) fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
pub(super) fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The address a shader finds in a descriptor of title memory: where a
/// buffer view starts, or the ray-tracing structure of a structure's view
/// (no resource: the view's description holds the address, and the shader
/// walks the structure from there itself).
pub(crate) fn view_address(view: &View) -> Option<u64> {
    const STRUCTURE: u32 = D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32;
    match view {
        View::Shader(_, Some(desc)) if desc[1] == STRUCTURE => {
            Some(u64::from(desc[4]) | u64::from(desc[5]) << 32)
        }
        _ => buffer_range(view).map(|range| range.0),
    }
}

/// The title memory the host slot of buffer view `view` is made on
/// (address, bytes): the whole buffer the view is of, which is what
/// [`make`] finds the host copy by. [`buffer_range`] is the part of it the
/// view reads.
pub(crate) fn buffer_extent(view: &View) -> Option<(u64, u64)> {
    match view {
        View::Shader(r, Some(_)) | View::Unordered(r, _, Some(_)) => {
            let (data, len) = crate::resource::buffer_bytes(*r as crate::object::This)?;
            Some((data as u64, len))
        }
        View::Placed(_, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
        )),
        _ => None,
    }
}

/// Whether a buffer view is brought up to date in the host copy its slot
/// shows ([`buffer_copy`]). On by default; `OATHKEEP_VIEW_EXTENT=0` looks the
/// copy up by the view's range alone, as before.
fn view_extent() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_VIEW_EXTENT").is_none_or(|v| v != "0"))
}

/// The host copy buffer view `view` reads, and the title range (address,
/// bytes) it reads there; `None` for other views and empty ranges.
///
/// The copy is the one the view's slot shows: [`make`] finds it by the
/// whole buffer the view is of ([`buffer_extent`]). Looked up by the range
/// the view reads, as it was, the copy is another one wherever the two
/// lookups differ: for memory outside buffer objects, a view of a part of a
/// buffer that crosses a window line (the buffer's window is two cells, the
/// part's one), and for buffer objects, a part that also lies in a smaller
/// buffer object. The slot then showed a copy nothing brought up to date,
/// and what shaders wrote through it was looked for in the other copy.
pub(crate) fn buffer_copy(view: &View) -> Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)> {
    if let &View::Constant(address, size) = view {
        let len = u64::from(size.min(64 << 10));
        if len == 0 {
            return None;
        }
        let shadow = crate::shadow::find(address, len)?;
        if !shadow.can_bind_cbv(address, size)
            && let Some(aligned) = crate::shadow::find_aligned_cbv(address, len)
            && aligned.can_bind_cbv(address, size)
        {
            return Some((aligned, address, len));
        }
        return Some((shadow, address, len));
    }
    let (address, len) = buffer_range(view)?;
    if len == 0 {
        return None;
    }
    if view_extent()
        && let Some((base, width)) = buffer_extent(view)
        // (The console is lax: a view may begin past its buffer's end.)
        && address >= base
        && address - base < width.max(1)
        && let Some(shadow) = crate::shadow::find(base, width)
    {
        return Some((view_home(shadow, address, len, view_elements(view)?), address, len));
    }
    let shadow = crate::shadow::find(address, len)?;
    Some((view_home(shadow, address, len, view_elements(view)?), address, len))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host resource a slot made by [`make`] views, if any.
pub(super) type Kept = Option<ID3D12Resource>;

