//! The recorder: what the host layer makes and records with D3D12, seen at
//! the D3D12 boundary (`super::super::hook`), and one frame of it written down
//! (`super::super::snap`). On with `OATHKEEP_RECORD=<folder>` only; without it
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
use super::super::{gpu, pso, snap};

use super::list_hooks::{note, note_change, rect, rects};
use super::*;

pub(super) fn table(r: &Recorder, graphics: bool, param: u32, at: u64) -> Cmd {
    let (heap, slot) = match read(&r.descriptors).gpu(at) {
        Some((h, i)) => (h.id, i),
        None => (NONE, 0),
    };
    if heap == NONE {
        r.problem("a descriptor table in no heap seen made");
    }
    Cmd::Table {
        graphics,
        param,
        heap,
        slot,
    }
}

pub(super) unsafe extern "system" fn table_compute(this: Ptr, param: u32, at: D3D12_GPU_DESCRIPTOR_HANDLE) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList_Vtbl.SetComputeRootDescriptorTable
        )(this, param, at);
        note(this, |r| table(r, false, param, at.ptr));
    }
}

pub(super) unsafe extern "system" fn table_graphics(this: Ptr, param: u32, at: D3D12_GPU_DESCRIPTOR_HANDLE) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList_Vtbl.SetGraphicsRootDescriptorTable
        )(this, param, at);
        note(this, |r| table(r, true, param, at.ptr));
    }
}

pub(super) unsafe extern "system" fn constant_compute(this: Ptr, param: u32, value: u32, offset: u32) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList_Vtbl.SetComputeRoot32BitConstant
        )(this, param, value, offset);
        note(this, |_| Cmd::Constants {
            graphics: false,
            param,
            offset,
            values: vec![value],
        });
    }
}

pub(super) unsafe extern "system" fn constant_graphics(this: Ptr, param: u32, value: u32, offset: u32) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList_Vtbl.SetGraphicsRoot32BitConstant
        )(this, param, value, offset);
        note(this, |_| Cmd::Constants {
            graphics: true,
            param,
            offset,
            values: vec![value],
        });
    }
}

pub(super) unsafe fn values(n: u32, data: *const c_void) -> Vec<u32> {
    // SAFETY: `n` values, of any alignment.
    (0..n as usize)
        .map(|i| unsafe { data.cast::<u32>().add(i).read_unaligned() })
        .collect()
}

pub(super) unsafe extern "system" fn constants_compute(
    this: Ptr,
    param: u32,
    n: u32,
    data: *const c_void,
    offset: u32,
) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList_Vtbl.SetComputeRoot32BitConstants
        )(this, param, n, data, offset);
        note(this, |_| Cmd::Constants {
            graphics: false,
            param,
            offset,
            values: values(n, data),
        });
    }
}

pub(super) unsafe extern "system" fn constants_graphics(
    this: Ptr,
    param: u32,
    n: u32,
    data: *const c_void,
    offset: u32,
) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList_Vtbl.SetGraphicsRoot32BitConstants
        )(this, param, n, data, offset);
        note(this, |_| Cmd::Constants {
            graphics: true,
            param,
            offset,
            values: values(n, data),
        });
    }
}

/// A root view's address, reported if it lies in no buffer.
pub(super) fn address(r: &Recorder, at: u64) -> Va {
    let va = r.va(at);
    if matches!(va, Va::Unknown { .. }) {
        r.problem("a GPU address in no buffer seen made");
    }
    va
}

macro_rules! root_views {
    ($($name:ident, $method:ident, $graphics:literal, $kind:ident;)*) => {$(
        pub(super) unsafe extern "system" fn $name(this: Ptr, param: u32, at: u64) {
            // SAFETY: as above.
            unsafe {
                own!(this, ID3D12GraphicsCommandList_Vtbl.$method)(this, param, at);
                note(this, |r| Cmd::RootView { graphics: $graphics, kind: slot::$kind, param, va: address(r, at) });
            }
        }
    )*};
}
root_views! {
    cbv_compute, SetComputeRootConstantBufferView, false, CBV;
    cbv_graphics, SetGraphicsRootConstantBufferView, true, CBV;
    srv_compute, SetComputeRootShaderResourceView, false, SRV;
    srv_graphics, SetGraphicsRootShaderResourceView, true, SRV;
    uav_compute, SetComputeRootUnorderedAccessView, false, UAV;
    uav_graphics, SetGraphicsRootUnorderedAccessView, true, UAV;
}

pub(super) unsafe extern "system" fn index_buffer(this: Ptr, view: *const D3D12_INDEX_BUFFER_VIEW) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.IASetIndexBuffer)(this, view);
        note(this, |r| Cmd::IndexBuffer {
            view: view
                .as_ref()
                .map(|v| (address(r, v.BufferLocation), v.SizeInBytes, v.Format.0)),
        });
    }
}

pub(super) unsafe extern "system" fn vertex_buffers(
    this: Ptr,
    start: u32,
    n: u32,
    views: *const D3D12_VERTEX_BUFFER_VIEW,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.IASetVertexBuffers)(this, start, n, views);
        note(this, |r| Cmd::VertexBuffers {
            start,
            count: n,
            views: (!views.is_null()).then(|| {
                let all = std::slice::from_raw_parts(views, n as usize);
                all.iter()
                    .map(|v| (address(r, v.BufferLocation), v.SizeInBytes, v.StrideInBytes))
                    .collect()
            }),
        });
    }
}

pub(super) unsafe extern "system" fn targets(
    this: Ptr,
    n: u32,
    colors: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    range: BOOL,
    depth: *const D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.OMSetRenderTargets)(
            this, n, colors, range, depth,
        );
        note(this, |r| {
            let colors = (0..n as usize)
                .map(|i| {
                    if !range.as_bool() {
                        return r.view((*colors.add(i)).ptr);
                    }
                    let all = read(&r.descriptors);
                    all.cpu((*colors).ptr)
                        .map_or(Slot::EMPTY, |(h, first)| h.get(first + i as u32))
                })
                .collect();
            Cmd::Targets {
                colors,
                depth: depth.as_ref().map(|d| r.view(d.ptr)),
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "system" fn clear_depth(
    this: Ptr,
    view: D3D12_CPU_DESCRIPTOR_HANDLE,
    flags: D3D12_CLEAR_FLAGS,
    depth: f32,
    stencil: u8,
    n: u32,
    all: *const RECT,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.ClearDepthStencilView)(
            this, view, flags, depth, stencil, n, all,
        );
        note(this, |r| Cmd::ClearDepth {
            view: r.view(view.ptr),
            flags: flags.0,
            depth,
            stencil,
            rects: rects(n, all),
        });
    }
}

pub(super) unsafe extern "system" fn clear_target(
    this: Ptr,
    view: D3D12_CPU_DESCRIPTOR_HANDLE,
    color: *const f32,
    n: u32,
    all: *const RECT,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.ClearRenderTargetView)(this, view, color, n, all);
        note(this, |r| Cmd::ClearTarget {
            view: r.view(view.ptr),
            color: color.cast::<[f32; 4]>().read(),
            rects: rects(n, all),
        });
    }
}

pub(super) unsafe extern "system" fn discard(this: Ptr, res: Ptr, region: *const D3D12_DISCARD_REGION) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.DiscardResource)(this, res, region);
        note(this, |r| Cmd::Discard {
            res: r.resource(res),
            rects: region
                .as_ref()
                .map_or(Vec::new(), |d| rects(d.NumRects, d.pRects)),
            subs: region
                .as_ref()
                .map(|d| [d.FirstSubresource, d.NumSubresources]),
        });
    }
}

pub(super) unsafe extern "system" fn begin_query(this: Ptr, heap: Ptr, kind: D3D12_QUERY_TYPE, index: u32) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.BeginQuery)(this, heap, kind, index);
        note(this, |r| Cmd::BeginQuery {
            heap: r.object(heap, "a query heap"),
            kind: kind.0,
            index,
        });
    }
}

pub(super) unsafe extern "system" fn end_query(this: Ptr, heap: Ptr, kind: D3D12_QUERY_TYPE, index: u32) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.EndQuery)(this, heap, kind, index);
        note(this, |r| Cmd::EndQuery {
            heap: r.object(heap, "a query heap"),
            kind: kind.0,
            index,
        });
    }
}

pub(super) unsafe extern "system" fn resolve_query(
    this: Ptr,
    heap: Ptr,
    kind: D3D12_QUERY_TYPE,
    first: u32,
    count: u32,
    dst: Ptr,
    at: u64,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.ResolveQueryData)(
            this, heap, kind, first, count, dst, at,
        );
        note(this, |r| Cmd::ResolveQuery {
            heap: r.object(heap, "a query heap"),
            kind: kind.0,
            first,
            count,
            dst: r.resource(dst),
            at,
        });
    }
}

pub(super) unsafe extern "system" fn indirect(
    this: Ptr,
    signature: Ptr,
    most: u32,
    args: Ptr,
    at: u64,
    count: Ptr,
    count_at: u64,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.ExecuteIndirect)(
            this, signature, most, args, at, count, count_at,
        );
        note(this, |r| Cmd::Indirect {
            signature: r.object(signature, "a command signature"),
            most,
            args: r.resource(args),
            at,
            count: r.resource(count),
            count_at,
        });
    }
}

pub(super) unsafe extern "system" fn write_immediate(
    this: Ptr,
    n: u32,
    all: *const D3D12_WRITEBUFFERIMMEDIATE_PARAMETER,
    modes: *const D3D12_WRITEBUFFERIMMEDIATE_MODE,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList2_Vtbl.WriteBufferImmediate)(this, n, all, modes);
        note(this, |r| Cmd::WriteImmediate {
            all: (0..n as usize)
                .map(|i| {
                    let p = all.add(i).read();
                    let mode = if modes.is_null() {
                        0
                    } else {
                        modes.add(i).read().0
                    };
                    (address(r, p.Dest), p.Value, mode)
                })
                .collect(),
        });
    }
}

/// The first subresources of all: what [`ALL`] means in a barrier.
const _: () = assert!(ALL == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES);

