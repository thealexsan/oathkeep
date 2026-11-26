//! The recorder: what the host layer makes and records with D3D12, seen at
//! the D3D12 boundary (`super::super::hook`), and one frame of it written down
//! (`super::super::snapshot`). On with `OATHKEEP_RECORD=<folder>` only; without it
//! nothing is hooked.
//!
//! From the device's creation on it keeps what objects were made from and
//! what each descriptor slot holds, and follows the states of textures.
//! Lists are recorded only while it is armed: a few frames ahead of the one
//! captured, since the title records lists before they run.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR};

use super::super::hook::{self, method, noted_entry, orig};
use super::super::states::{self, Change, States};
use super::super::wire::{self, ALL, Barrier, Cmd, Loc, NONE, Part, Place, Slot, Va, Wire, slot};
use super::super::{gpu, pso, snapshot};

use super::device_hooks::made;
use super::*;

pub(super) unsafe extern "system" fn create_heap(
    this: Ptr,
    desc: *const D3D12_HEAP_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateHeap)(this, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            let id = r.id();
            lock(&r.heaps).insert(id, heap_of(id, &*desc));
            r.ids.set(p as usize, id);
        }
        hr
    }
}

pub(super) fn heap_of(id: u32, d: &D3D12_HEAP_DESC) -> wire::Heap {
    wire::Heap {
        id,
        size: d.SizeInBytes,
        props: gpu::from_props(&d.Properties),
        alignment: d.Alignment,
        flags: d.Flags.0,
    }
}

pub(super) unsafe fn clear_of(clear: *const D3D12_CLEAR_VALUE) -> Option<[u32; 5]> {
    const _: () = assert!(size_of::<D3D12_CLEAR_VALUE>() == 20);
    // SAFETY: null or a clear value, of 20 bytes.
    (!clear.is_null()).then(|| unsafe { clear.cast::<[u32; 5]>().read() })
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "system" fn create_committed(
    this: Ptr,
    props: *const D3D12_HEAP_PROPERTIES,
    flags: D3D12_HEAP_FLAGS,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
    clear: *const D3D12_CLEAR_VALUE,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateCommittedResource)(
            this, props, flags, desc, state, clear, iid, out,
        );
        if let Some((r, p)) = made(hr, out) {
            let place = Place::Committed {
                props: gpu::from_props(&*props),
                flags: flags.0,
            };
            r.add(p, place, state.0, clear_of(clear));
        }
        hr
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "system" fn create_placed(
    this: Ptr,
    heap: Ptr,
    offset: u64,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
    clear: *const D3D12_CLEAR_VALUE,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreatePlacedResource)(
            this, heap, offset, desc, state, clear, iid, out,
        );
        if let Some((r, p)) = made(hr, out) {
            // One opened from memory was not seen made.
            let heap = r.ids.get(heap as usize).unwrap_or_else(|| {
                let id = r.id();
                let d = borrow::<ID3D12Heap>(heap).GetDesc();
                r.problem("a heap opened from existing memory: a replay makes one like it");
                lock(&r.heaps).insert(id, heap_of(id, &d));
                r.ids.set(heap as usize, id);
                id
            });
            r.add(p, Place::Placed { heap, offset }, state.0, clear_of(clear));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_cbv(
    this: Ptr,
    desc: *const D3D12_CONSTANT_BUFFER_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CreateConstantBufferView)(this, desc, at);
        if let Some(r) = recorder().filter(|_| !ours()) {
            r.set_view(
                at.ptr,
                &Slot::new(
                    slot::CBV,
                    NONE,
                    NONE,
                    desc.cast(),
                    size_of::<D3D12_CONSTANT_BUFFER_VIEW_DESC>(),
                ),
            );
        }
    }
}

pub(super) unsafe extern "system" fn create_srv(
    this: Ptr,
    res: Ptr,
    desc: *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CreateShaderResourceView)(this, res, desc, at);
        if let Some(r) = recorder().filter(|_| !ours()) {
            let id = r.resource(res);
            r.set_view(
                at.ptr,
                &Slot::new(
                    slot::SRV,
                    id,
                    NONE,
                    desc.cast(),
                    size_of::<D3D12_SHADER_RESOURCE_VIEW_DESC>(),
                ),
            );
        }
    }
}

pub(super) unsafe extern "system" fn create_uav(
    this: Ptr,
    res: Ptr,
    counter: Ptr,
    desc: *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CreateUnorderedAccessView)(this, res, counter, desc, at);
        if let Some(r) = recorder().filter(|_| !ours()) {
            let (id, counter) = (r.resource(res), r.resource(counter));
            r.set_view(
                at.ptr,
                &Slot::new(
                    slot::UAV,
                    id,
                    counter,
                    desc.cast(),
                    size_of::<D3D12_UNORDERED_ACCESS_VIEW_DESC>(),
                ),
            );
        }
    }
}

pub(super) unsafe extern "system" fn create_rtv(
    this: Ptr,
    res: Ptr,
    desc: *const D3D12_RENDER_TARGET_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CreateRenderTargetView)(this, res, desc, at);
        if let Some(r) = recorder().filter(|_| !ours()) {
            let id = r.resource(res);
            r.set_view(
                at.ptr,
                &Slot::new(
                    slot::RTV,
                    id,
                    NONE,
                    desc.cast(),
                    size_of::<D3D12_RENDER_TARGET_VIEW_DESC>(),
                ),
            );
        }
    }
}

pub(super) unsafe extern "system" fn create_dsv(
    this: Ptr,
    res: Ptr,
    desc: *const D3D12_DEPTH_STENCIL_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CreateDepthStencilView)(this, res, desc, at);
        if let Some(r) = recorder().filter(|_| !ours()) {
            let id = r.resource(res);
            r.set_view(
                at.ptr,
                &Slot::new(
                    slot::DSV,
                    id,
                    NONE,
                    desc.cast(),
                    size_of::<D3D12_DEPTH_STENCIL_VIEW_DESC>(),
                ),
            );
        }
    }
}

pub(super) unsafe extern "system" fn create_sampler(
    this: Ptr,
    desc: *const D3D12_SAMPLER_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    const _: () = assert!(size_of::<D3D12_SAMPLER_DESC>() == 52);
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CreateSampler)(this, desc, at);
        if let Some(r) = recorder().filter(|_| !ours()) {
            r.set_view(
                at.ptr,
                &Slot::new(slot::SAMPLER, NONE, NONE, desc.cast(), 52),
            );
        }
    }
}

pub(super) unsafe extern "system" fn copy_descriptors_simple(
    this: Ptr,
    n: u32,
    to: D3D12_CPU_DESCRIPTOR_HANDLE,
    from: D3D12_CPU_DESCRIPTOR_HANDLE,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CopyDescriptorsSimple)(this, n, to, from, kind);
        if let Some(r) = recorder().filter(|_| !ours()) {
            r.copy_views(to.ptr, from.ptr, n);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "system" fn copy_descriptors(
    this: Ptr,
    n_to: u32,
    to: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    to_sizes: *const u32,
    n_from: u32,
    from: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    from_sizes: *const u32,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    // SAFETY: as above; the arrays have as many entries as the counts say,
    // and null sizes mean one descriptor each.
    unsafe {
        own!(this, ID3D12Device_Vtbl.CopyDescriptors)(
            this, n_to, to, to_sizes, n_from, from, from_sizes, kind,
        );
        let Some(r) = recorder().filter(|_| !ours()) else {
            return;
        };
        let step =
            own!(this, ID3D12Device_Vtbl.GetDescriptorHandleIncrementSize)(this, kind) as usize;
        let size = |sizes: *const u32, i: u32| {
            if sizes.is_null() {
                1
            } else {
                *sizes.add(i as usize)
            }
        };
        let (mut i, mut j, mut done_to, mut done_from) = (0, 0, 0, 0);
        while i < n_to && j < n_from {
            let n = (size(to_sizes, i) - done_to).min(size(from_sizes, j) - done_from);
            if n > 0 {
                let a = (*to.add(i as usize)).ptr + done_to as usize * step;
                let b = (*from.add(j as usize)).ptr + done_from as usize * step;
                r.copy_views(a, b, n);
            }
            done_to += n;
            done_from += n;
            if done_to >= size(to_sizes, i) {
                (i, done_to) = (i + 1, 0);
            }
            if done_from >= size(from_sizes, j) {
                (j, done_from) = (j + 1, 0);
            }
        }
    }
}

pub(super) unsafe extern "system" fn execute(this: Ptr, n: u32, lists: *const Ptr) {
    // SAFETY: `n` closed lists, as D3D12 asks; the state is the queue's.
    unsafe {
        let go = || own!(this, ID3D12CommandQueue_Vtbl.ExecuteCommandLists)(this, n, lists);
        let Some(r) = recorder().filter(|_| !ours()) else {
            return go();
        };
        let kind = hook::state::<QueueState>(this).kind;
        if kind != D3D12_COMMAND_LIST_TYPE_DIRECT.0 {
            snapshot::other_queue(r, n);
            return go();
        }
        let all = std::slice::from_raw_parts(lists, n as usize);
        snapshot::submit(r, &borrow::<ID3D12CommandQueue>(this), all, go);
    }
}

