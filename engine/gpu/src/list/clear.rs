use super::This;
#[cfg(windows)]
use super::{ensure_state, host_list, with_state};

#[repr(C)]
pub(super) struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

pub(super) extern "system" fn clear_rtv(
    this: This,
    handle: u64,
    color: *const [f32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        let (Some(list), Some(record)) = (host_list(this), crate::descriptor::rtv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        // SAFETY: the title passes four floats (null is tolerated).
        let Some(color) = (unsafe { color.as_ref() }) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const RENDER_TARGET: u32 = 0x4;
        let rtv = host.rtv(Some(texture), record.desc().as_ref());
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            if let Some((_, before)) = list.transition(record.resource, RENDER_TARGET) {
                use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
                list.list.ResourceBarrier(&[crate::host::transition(
                    texture,
                    State(before as i32),
                    State(RENDER_TARGET as i32),
                )]);
            }
            list.list.ClearRenderTargetView(rtv, color, rects);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, handle, color, count, rects);
}

/// `ClearDepthStencilView(handle, flags, depth, stencil, count, rects)`;
/// flags: 1 depth, 2 stencil.
pub(super) extern "system" fn clear_dsv(
    this: This,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_CLEAR_FLAGS;
        let (Some(list), Some(record)) = (host_list(this), crate::descriptor::dsv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const DEPTH_WRITE: u32 = 0x10;
        // A depth texture shaders also write: what they wrote, first (the
        // clear may be of one slice or of some rectangles).
        list.twin_use(record.resource, false, true);
        ensure_state(list, record.resource, DEPTH_WRITE);
        if crate::resource::depth_and_unordered(record.resource) {
            crate::resource::DEPTH_UNORDERED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let dsv = host.dsv(texture, record.depth_desc().as_ref());
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            list.list
                .ClearDepthStencilView(dsv, D3D12_CLEAR_FLAGS(flags as i32 & 3), depth, stencil, rects)
        };
    }
    #[cfg(not(windows))]
    let _ = (this, handle, flags, depth, stencil, count, rects);
}

/// Persistent CPU-only and shader-visible CBV/SRV/UAV ring heaps dedicated to
/// `ClearUnorderedAccessViewUint` / `ClearUnorderedAccessViewFloat`.
#[cfg(windows)]
struct UavClearHeaps {
    cpu_heap: windows::Win32::Graphics::Direct3D12::ID3D12DescriptorHeap,
    gpu_heap: windows::Win32::Graphics::Direct3D12::ID3D12DescriptorHeap,
    cpu_start: usize,
    gpu_cpu_start: usize,
    gpu_gpu_start: u64,
    step: u32,
    next: std::sync::atomic::AtomicU32,
}

#[cfg(windows)]
impl UavClearHeaps {
    const SLOTS: u32 = 4096;

    fn get() -> Option<&'static Self> {
        static HEAPS: std::sync::OnceLock<Option<UavClearHeaps>> = std::sync::OnceLock::new();
        HEAPS
            .get_or_init(|| {
                use windows::Win32::Graphics::Direct3D12::*;
                let host = crate::host::host()?;
                let make = |flags| -> Option<ID3D12DescriptorHeap> {
                    let desc = D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: Self::SLOTS,
                        Flags: flags,
                        NodeMask: 0,
                    };
                    // SAFETY: plain heap creation on the live host device.
                    unsafe { host.device.CreateDescriptorHeap(&desc) }.ok()
                };
                let cpu_heap = make(D3D12_DESCRIPTOR_HEAP_FLAG_NONE)?;
                let gpu_heap = make(D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)?;
                // SAFETY: live descriptor heaps.
                let (cpu_start, gpu_cpu_start, gpu_gpu_start, step) = unsafe {
                    (
                        cpu_heap.GetCPUDescriptorHandleForHeapStart().ptr,
                        gpu_heap.GetCPUDescriptorHandleForHeapStart().ptr,
                        gpu_heap.GetGPUDescriptorHandleForHeapStart().ptr,
                        host.device
                            .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
                    )
                };
                Some(UavClearHeaps {
                    cpu_heap,
                    gpu_heap,
                    cpu_start,
                    gpu_cpu_start,
                    gpu_gpu_start,
                    step,
                    next: std::sync::atomic::AtomicU32::new(0),
                })
            })
            .as_ref()
    }

    fn next_pair(
        &self,
    ) -> (
        windows::Win32::Graphics::Direct3D12::D3D12_CPU_DESCRIPTOR_HANDLE,
        windows::Win32::Graphics::Direct3D12::D3D12_CPU_DESCRIPTOR_HANDLE,
        windows::Win32::Graphics::Direct3D12::D3D12_GPU_DESCRIPTOR_HANDLE,
    ) {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_GPU_DESCRIPTOR_HANDLE,
        };
        let slot = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % Self::SLOTS;
        let off = usize::try_from(slot).unwrap_or(0) * usize::try_from(self.step).unwrap_or(0);
        let gpu_off = u64::from(slot) * u64::from(self.step);
        let _ = &self.cpu_heap;
        (
            D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.cpu_start + off },
            D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.gpu_cpu_start + off },
            D3D12_GPU_DESCRIPTOR_HANDLE { ptr: self.gpu_gpu_start + gpu_off },
        )
    }
}

#[cfg(windows)]
enum UavClearValues {
    Uint([u32; 4]),
    Float([f32; 4]),
}

#[cfg(windows)]
fn uav_desc_from_words(
    words: &[u32; 10],
) -> windows::Win32::Graphics::Direct3D12::D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes matching D3D12_UNORDERED_ACCESS_VIEW_DESC layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<windows::Win32::Graphics::Direct3D12::D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

#[cfg(windows)]
fn clear_uav(
    this: This,
    gpu_handle: u64,
    cpu_handle: u64,
    resource: This,
    values: UavClearValues,
    count: u32,
    rects: *const Rect,
) {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_UNKNOWN};
    const UNORDERED_ACCESS: u32 = 0x8;
    let (Some(list), Some(host), Some(heaps)) =
        (host_list(this), crate::host::host(), UavClearHeaps::get())
    else {
        return;
    };
    let rects: &[windows::Win32::Foundation::RECT] = if count > 0 && !rects.is_null() {
        // SAFETY: the title passes `count` RECTs in host layout.
        unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
    } else {
        &[]
    };
    let view = crate::descriptor::view(cpu_handle).or_else(|| crate::descriptor::view(gpu_handle));
    let raw_words: Option<[u32; 10]> = match &view {
        Some(crate::descriptor::View::Unordered(_, _, desc)) => *desc,
        Some(crate::descriptor::View::Placed(true, _, desc)) => desc[..10].try_into().ok(),
        _ => None,
    };

    let (target, desc, owned_keep): (
        ID3D12Resource,
        Option<D3D12_UNORDERED_ACCESS_VIEW_DESC>,
        bool,
    ) = if let Some(texture) = crate::resource::host_resource(resource) {
        let mut desc = raw_words.as_ref().map(uav_desc_from_words);
        // If this is a depth texture with a colour UAV twin, clear the twin.
        let (target, is_twin) = if let Some(twin) = crate::resource::twin_resource(resource)
            .or_else(|| crate::resource::twin_plane_resource(resource))
        {
            desc = None;
            (twin.clone(), true)
        } else {
            (texture.clone(), false)
        };
        if desc.as_ref().is_some_and(|d| d.Format == DXGI_FORMAT_UNKNOWN) {
            desc = None;
        }
        list.twin_use(resource, is_twin, true);
        if !is_twin {
            crate::texture::card_writes(list, resource, &target, None);
            ensure_state(list, resource, UNORDERED_ACCESS);
        }
        crate::texture::host_written(resource);
        (target, desc, false)
    } else if let Some((ptr, len)) = crate::resource::buffer_bytes(resource) {
        if len < 4 {
            return;
        }
        let Some((shadow, addr, bytes)) = view
            .as_ref()
            .and_then(crate::descriptor::host::buffer_copy)
            .or_else(|| {
                let s = crate::shadow::find(ptr as u64, len)?;
                Some((s, ptr as u64, len))
            })
            .filter(|&(_, _, bytes)| bytes >= 4)
        else {
            return;
        };
        let off = shadow.offset(addr);
        let mut desc = raw_words.as_ref().map(uav_desc_from_words);
        let use_raw = desc.as_ref().is_none_or(|d| {
            d.ViewDimension != D3D12_UAV_DIMENSION_BUFFER
                || unsafe { d.Anonymous.Buffer.StructureByteStride > 0 }
                || d.Format == DXGI_FORMAT_UNKNOWN
        });
        let desc = if use_raw {
            let elements = (bytes / 4).clamp(1, u64::from(u32::MAX)) as u32;
            D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        FirstElement: off / 4,
                        NumElements: elements,
                        StructureByteStride: 0,
                        CounterOffsetInBytes: 0,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    },
                },
            }
        } else {
            let mut d = desc.take().unwrap();
            let elem = if unsafe { d.Anonymous.Buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 } {
                4
            } else {
                crate::resource::element(d.Format.0 as u32).map_or(4, |(b, _)| b.max(1))
            };
            let base_off = shadow.offset(ptr as u64);
            // SAFETY: ViewDimension == BUFFER.
            unsafe {
                d.Anonymous.Buffer.FirstElement =
                    d.Anonymous.Buffer.FirstElement.saturating_add(base_off / elem);
                d.Anonymous.Buffer.CounterOffsetInBytes = 0;
            }
            d
        };
        shadow.refresh(list, addr, bytes);
        shadow.gpu_writes(list, addr, bytes);
        let buf = shadow.buffer.clone();
        (buf, Some(desc), true)
    } else {
        return;
    };

    let (cpu_only, gpu_cpu, gpu_gpu) = heaps.next_pair();
    if owned_keep {
        list.keep(&target);
    }
    list.keep(&heaps.gpu_heap);
    // SAFETY: live device, open list, freshly created matching CPU/GPU UAV descriptors.
    unsafe {
        host.device.CreateUnorderedAccessView(
            &target,
            None::<&ID3D12Resource>,
            desc.as_ref().map(std::ptr::from_ref),
            cpu_only,
        );
        host.device.CreateUnorderedAccessView(
            &target,
            None::<&ID3D12Resource>,
            desc.as_ref().map(std::ptr::from_ref),
            gpu_cpu,
        );
        list.list.SetDescriptorHeaps(&[Some(heaps.gpu_heap.clone())]);
        let is_raw = desc
            .as_ref()
            .is_some_and(|d| d.Format == DXGI_FORMAT_R32_TYPELESS);
        match values {
            UavClearValues::Uint(v) => {
                list.list
                    .ClearUnorderedAccessViewUint(gpu_gpu, cpu_only, &target, &v, rects);
            }
            UavClearValues::Float(v) if is_raw => {
                let bits = v.map(f32::to_bits);
                list.list
                    .ClearUnorderedAccessViewUint(gpu_gpu, cpu_only, &target, &bits, rects);
            }
            UavClearValues::Float(v) => {
                list.list
                    .ClearUnorderedAccessViewFloat(gpu_gpu, cpu_only, &target, &v, rects);
            }
        }
    }
    // Invalidate cached host descriptor heaps/root signatures so the next draw
    // or dispatch rebinds the title's heaps cleanly (and re-clamps indirect
    // buffers if a buffer UAV was cleared).
    with_state(this, |list, s| {
        s.replay(list);
        if owned_keep {
            s.copied();
        }
    });
}

/// `ClearUnorderedAccessViewUint(gpu_handle, cpu_handle, resource, values, count, rects)` (slot 0x1a8).
pub(super) extern "system" fn clear_uav_uint(
    this: This,
    gpu_handle: u64,
    cpu_handle: u64,
    resource: This,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        // SAFETY: the title passes four u32 values (null is tolerated).
        let Some(&vals) = (unsafe { values.as_ref() }) else {
            return;
        };
        clear_uav(
            this,
            gpu_handle,
            cpu_handle,
            resource,
            UavClearValues::Uint(vals),
            count,
            rects,
        );
    }
    #[cfg(not(windows))]
    let _ = (this, gpu_handle, cpu_handle, resource, values, count, rects);
}

/// `ClearUnorderedAccessViewFloat(gpu_handle, cpu_handle, resource, values, count, rects)` (slot 0x1b0).
pub(super) extern "system" fn clear_uav_float(
    this: This,
    gpu_handle: u64,
    cpu_handle: u64,
    resource: This,
    values: *const [f32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        // SAFETY: the title passes four f32 values (null is tolerated).
        let Some(&vals) = (unsafe { values.as_ref() }) else {
            return;
        };
        clear_uav(
            this,
            gpu_handle,
            cpu_handle,
            resource,
            UavClearValues::Float(vals),
            count,
            rects,
        );
    }
    #[cfg(not(windows))]
    let _ = (this, gpu_handle, cpu_handle, resource, values, count, rects);
}
