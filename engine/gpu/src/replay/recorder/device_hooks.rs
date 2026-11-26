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

use super::tables::{library_table, list_table, queue_table};
use super::*;

/// The recorder, for a call that is the host layer's and not our own, which
/// succeeded.
pub(super) fn made(hr: HRESULT, out: Out) -> Option<(&'static Recorder, Ptr)> {
    if hr.is_err() || out.is_null() || ours() {
        return None;
    }
    // SAFETY: the call's answer.
    let p = unsafe { *out };
    (!p.is_null()).then_some(())?;
    Some((recorder()?, p))
}

pub(super) unsafe extern "system" fn create_queue(
    this: Ptr,
    desc: *const D3D12_COMMAND_QUEUE_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: the caller's arguments, passed on; then a new queue no other
    // thread knows.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateCommandQueue)(this, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            let kind = (*desc).Type.0;
            let state = Box::leak(Box::new(QueueState { kind }));
            hook::install(p, (&raw const *state).cast(), queue_table);
            lock(&r.queues).push((ID3D12CommandQueue::clone(&borrow(p)), kind));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_list(
    this: Ptr,
    nodes: u32,
    kind: D3D12_COMMAND_LIST_TYPE,
    allocator: Ptr,
    pso: Ptr,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateCommandList)(
            this, nodes, kind, allocator, pso, iid, out,
        );
        if let Some((r, p)) = made(hr, out) {
            let state = Arc::new(ListState {
                kind: kind.0,
                inner: Mutex::default(),
            });
            // Every interface of it we hook, and the one it is submitted as.
            static ONE: OnceLock<bool> = OnceLock::new();
            let iids = [
                ID3D12CommandList::IID,
                ID3D12GraphicsCommandList1::IID,
                ID3D12GraphicsCommandList2::IID,
                ID3D12GraphicsCommandList4::IID,
                ID3D12GraphicsCommandList6::IID,
            ];
            let same = *ONE.get_or_init(|| {
                iids.iter()
                    .all(|iid| query(p, iid).is_none_or(|q| q.as_raw() == p))
            });
            let mut all = vec![p];
            if !same {
                all.extend(
                    iids.iter()
                        .filter_map(|iid| query(p, iid))
                        .map(|q| q.as_raw()),
                );
            }
            {
                let mut inner = lock(&state.inner);
                inner.whole =
                    r.armed.load(Ordering::SeqCst) && kind == D3D12_COMMAND_LIST_TYPE_DIRECT;
                begin(&mut inner, r, pso);
            }
            for q in all {
                // The state's address is kept for good, as the table is.
                hook::install(q, Arc::into_raw(state.clone()).cast(), list_table);
                write(&r.lists).insert(q as usize, state.clone());
                r.tell_gone(
                    &*borrow::<ID3D12CommandList>(q),
                    Gone::List { ptr: q as usize },
                );
            }
        }
        hr
    }
}

/// A list is open again, with pipeline `pso`.
pub(super) fn begin(inner: &mut ListInner, r: &Recorder, pso: Ptr) {
    inner.bytes = Vec::new();
    inner.changes.clear();
    if inner.whole && !pso.is_null() {
        Cmd::Pipeline {
            pso: r.object(pso, "a pipeline"),
        }
        .put(&mut inner.bytes);
    }
}

pub(super) unsafe extern "system" fn create_library(
    this: Ptr,
    blob: *const c_void,
    size: usize,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device1_Vtbl.CreatePipelineLibrary)(this, blob, size, iid, out);
        if let Some((_, p)) = made(hr, out) {
            let mut all = vec![p];
            let newer = query(p, &ID3D12PipelineLibrary1::IID);
            all.extend(newer.as_ref().map(|q| q.as_raw()));
            for q in all {
                hook::install(q, std::ptr::null(), library_table);
            }
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_graphics(
    this: Ptr,
    desc: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above; the description is valid, as it was accepted.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateGraphicsPipelineState)(this, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            r.pipeline(p, pso::graphics(&*desc, &mut Keeper(r)));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_compute(
    this: Ptr,
    desc: *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateComputePipelineState)(this, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            r.pipeline(p, pso::compute(&*desc, &mut Keeper(r)));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_stream(
    this: Ptr,
    desc: *const D3D12_PIPELINE_STATE_STREAM_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device2_Vtbl.CreatePipelineState)(this, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            r.pipeline(p, pso::stream(&*desc, &mut Keeper(r)));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn load_graphics(
    this: Ptr,
    name: PCWSTR,
    desc: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr =
            own!(this, ID3D12PipelineLibrary_Vtbl.LoadGraphicsPipeline)(this, name, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            r.pipeline(p, pso::graphics(&*desc, &mut Keeper(r)));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn load_compute(
    this: Ptr,
    name: PCWSTR,
    desc: *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr =
            own!(this, ID3D12PipelineLibrary_Vtbl.LoadComputePipeline)(this, name, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            r.pipeline(p, pso::compute(&*desc, &mut Keeper(r)));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn load_stream(
    this: Ptr,
    name: PCWSTR,
    desc: *const D3D12_PIPELINE_STATE_STREAM_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12PipelineLibrary1_Vtbl.LoadPipeline)(this, name, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            r.pipeline(p, pso::stream(&*desc, &mut Keeper(r)));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_root(
    this: Ptr,
    nodes: u32,
    blob: *const c_void,
    size: usize,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr =
            own!(this, ID3D12Device_Vtbl.CreateRootSignature)(this, nodes, blob, size, iid, out);
        if let Some((r, p)) = made(hr, out) {
            // D3D12 may answer with an object it made before.
            let known = r
                .ids
                .get(p as usize)
                .filter(|id| lock(&r.roots).contains_key(id));
            let bytes = std::slice::from_raw_parts(blob.cast::<u8>(), size).to_vec();
            let id = known.unwrap_or_else(|| r.id());
            lock(&r.roots).insert(id, Arc::new(bytes));
            r.ids.set(p as usize, id);
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_signature(
    this: Ptr,
    desc: *const D3D12_COMMAND_SIGNATURE_DESC,
    root: Ptr,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateCommandSignature)(this, desc, root, iid, out);
        if let Some((r, p)) = made(hr, out) {
            let id = r.id();
            let d = &*desc;
            let args: Vec<[u32; 4]> = std::slice::from_raw_parts(
                d.pArgumentDescs.cast::<[u32; 4]>(),
                d.NumArgumentDescs as usize,
            )
            .to_vec();
            // Arguments that are GPU addresses lie in buffers; they are not
            // rewritten for a replay.
            let addresses = [
                D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW,
                D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW,
                D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW,
                D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW,
                D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW,
            ];
            if args
                .iter()
                .any(|a| addresses.iter().any(|t| t.0 as u32 == a[0]))
            {
                r.problem("a command signature with GPU addresses among its arguments: a replay reads the recorded addresses");
            }
            let root = if root.is_null() {
                NONE
            } else {
                r.object(root, "a root signature")
            };
            lock(&r.signatures).insert(
                id,
                wire::Signature {
                    id,
                    stride: d.ByteStride,
                    args,
                    root,
                },
            );
            r.ids.set(p as usize, id);
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_queries(
    this: Ptr,
    desc: *const D3D12_QUERY_HEAP_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateQueryHeap)(this, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            let id = r.id();
            lock(&r.queries).insert(
                id,
                wire::QueryHeap {
                    id,
                    kind: (*desc).Type.0,
                    count: (*desc).Count,
                },
            );
            r.ids.set(p as usize, id);
        }
        hr
    }
}

pub(super) unsafe extern "system" fn create_descriptors(
    this: Ptr,
    desc: *const D3D12_DESCRIPTOR_HEAP_DESC,
    iid: *const GUID,
    out: Out,
) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12Device_Vtbl.CreateDescriptorHeap)(this, desc, iid, out);
        if let Some((r, p)) = made(hr, out) {
            let heap = borrow::<ID3D12DescriptorHeap>(p);
            let d = &*desc;
            let visible = d.Flags.0 & D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE.0 != 0;
            let cpu = heap.GetCPUDescriptorHandleForHeapStart().ptr;
            let gpu = if visible {
                heap.GetGPUDescriptorHandleForHeapStart().ptr
            } else {
                0
            };
            let step = own!(this, ID3D12Device_Vtbl.GetDescriptorHandleIncrementSize)(this, d.Type)
                as usize;
            let id = r.id();
            write(&r.descriptors).add(Arc::new(Heap::new(id, d, cpu, gpu, step.max(1))));
            r.ids.set(p as usize, id);
            r.tell_gone(
                &*heap,
                Gone::Heap {
                    id,
                    ptr: p as usize,
                },
            );
        }
        hr
    }
}

