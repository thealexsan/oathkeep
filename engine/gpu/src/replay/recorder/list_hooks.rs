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

use super::device_hooks::begin;
use super::tables::list_table;
use super::*;

/// A list's recorder and state, for a call of the host layer's.
pub(super) unsafe fn of(this: Ptr) -> Option<(&'static Recorder, &'static ListState)> {
    let r = recorder()?;
    if ours() {
        return None;
    }
    // SAFETY: a list with a shim; its state is a list's.
    Some((r, unsafe { hook::state::<ListState>(this) }))
}

/// Records a command, if lists are recorded now.
pub(super) unsafe fn note(this: Ptr, cmd: impl FnOnce(&Recorder) -> Cmd) {
    // SAFETY: a list with a shim.
    if let Some((r, state)) = unsafe { of(this) }
        && r.armed.load(Ordering::Relaxed)
    {
        state.push(&cmd(r));
    }
}

pub(super) unsafe extern "system" fn list_close(this: Ptr) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let hr = own!(this, ID3D12GraphicsCommandList_Vtbl.Close)(this);
        if let Some((_, state)) = of(this) {
            let mut inner = lock(&state.inner);
            let bytes = std::mem::take(&mut inner.bytes);
            let changes = Arc::new(std::mem::take(&mut inner.changes));
            inner.closed = Some((inner.whole.then(|| Arc::new(bytes)), changes));
        }
        hr
    }
}

pub(super) unsafe extern "system" fn list_reset(this: Ptr, allocator: Ptr, pso: Ptr) -> HRESULT {
    // SAFETY: as above.
    unsafe {
        let table = hook::table(this);
        let hr = own!(this, ID3D12GraphicsCommandList_Vtbl.Reset)(this, allocator, pso);
        // A reset sets the list's table pointer anew.
        hook::again(this, table, list_table);
        if let Some((r, state)) = of(this)
            && hr.is_ok()
        {
            let mut inner = lock(&state.inner);
            inner.closed = None;
            inner.whole =
                r.armed.load(Ordering::SeqCst) && state.kind == D3D12_COMMAND_LIST_TYPE_DIRECT.0;
            begin(&mut inner, r, pso);
        }
        hr
    }
}

pub(super) unsafe extern "system" fn draw(
    this: Ptr,
    vertices: u32,
    instances: u32,
    first: u32,
    first_instance: u32,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.DrawInstanced)(
            this,
            vertices,
            instances,
            first,
            first_instance,
        );
        note(this, |_| Cmd::Draw {
            vertices,
            instances,
            first,
            first_instance,
        });
    }
}

pub(super) unsafe extern "system" fn draw_indexed(
    this: Ptr,
    indices: u32,
    instances: u32,
    first: u32,
    base: i32,
    first_instance: u32,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.DrawIndexedInstanced)(
            this,
            indices,
            instances,
            first,
            base,
            first_instance,
        );
        note(this, |_| Cmd::DrawIndexed {
            indices,
            instances,
            first,
            base,
            first_instance,
        });
    }
}

pub(super) unsafe extern "system" fn dispatch(this: Ptr, x: u32, y: u32, z: u32) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.Dispatch)(this, x, y, z);
        note(this, |_| Cmd::Dispatch { groups: [x, y, z] });
    }
}

pub(super) unsafe extern "system" fn dispatch_mesh(this: Ptr, x: u32, y: u32, z: u32) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList6_Vtbl.DispatchMesh)(this, x, y, z);
        note(this, |_| Cmd::DispatchMesh { groups: [x, y, z] });
    }
}

pub(super) unsafe extern "system" fn copy_buffer(
    this: Ptr,
    dst: Ptr,
    dst_at: u64,
    src: Ptr,
    src_at: u64,
    len: u64,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.CopyBufferRegion)(
            this, dst, dst_at, src, src_at, len,
        );
        note(this, |r| Cmd::CopyBuffer {
            dst: r.resource(dst),
            dst_at,
            src: r.resource(src),
            src_at,
            len,
        });
    }
}

pub(super) unsafe fn location(r: &Recorder, l: &D3D12_TEXTURE_COPY_LOCATION) -> Loc {
    // SAFETY: a copy's valid location.
    unsafe {
        let res = r.resource(std::mem::transmute_copy::<_, Ptr>(&l.pResource));
        if l.Type == D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX {
            return Loc::Sub {
                res,
                sub: l.Anonymous.SubresourceIndex,
            };
        }
        let p = l.Anonymous.PlacedFootprint;
        let f = p.Footprint;
        Loc::Placed {
            res,
            offset: p.Offset,
            format: f.Format.0,
            size: [f.Width, f.Height, f.Depth],
            pitch: f.RowPitch,
        }
    }
}

/// A command that changes states whether lists are recorded or not.
pub(super) unsafe fn note_change(this: Ptr, cmd: impl FnOnce(&Recorder) -> Cmd) {
    // SAFETY: a list with a shim.
    if let Some((r, state)) = unsafe { of(this) } {
        let cmd = cmd(r);
        let mut inner = lock(&state.inner);
        states::changes(&cmd, &mut inner.changes);
        if inner.whole {
            cmd.put(&mut inner.bytes);
        }
    }
}

pub(super) unsafe extern "system" fn copy_texture(
    this: Ptr,
    dst: *const D3D12_TEXTURE_COPY_LOCATION,
    x: u32,
    y: u32,
    z: u32,
    src: *const D3D12_TEXTURE_COPY_LOCATION,
    part: *const D3D12_BOX,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.CopyTextureRegion)(this, dst, x, y, z, src, part);
        note_change(this, |r| Cmd::CopyTexture {
            dst: location(r, &*dst),
            at: [x, y, z],
            src: location(r, &*src),
            part: part
                .as_ref()
                .map(|b| [b.left, b.top, b.front, b.right, b.bottom, b.back]),
        });
    }
}

pub(super) unsafe extern "system" fn copy_resource(this: Ptr, dst: Ptr, src: Ptr) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.CopyResource)(this, dst, src);
        note_change(this, |r| Cmd::CopyResource {
            dst: r.resource(dst),
            src: r.resource(src),
        });
    }
}

pub(super) unsafe extern "system" fn resolve(
    this: Ptr,
    dst: Ptr,
    dst_sub: u32,
    src: Ptr,
    src_sub: u32,
    format: DXGI_FORMAT,
) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.ResolveSubresource)(
            this, dst, dst_sub, src, src_sub, format,
        );
        note(this, |r| Cmd::Resolve {
            dst: r.resource(dst),
            dst_sub,
            src: r.resource(src),
            src_sub,
            format: format.0,
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "system" fn resolve_region(
    this: Ptr,
    dst: Ptr,
    dst_sub: u32,
    x: u32,
    y: u32,
    src: Ptr,
    src_sub: u32,
    part: *const RECT,
    format: DXGI_FORMAT,
    mode: D3D12_RESOLVE_MODE,
) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList1_Vtbl.ResolveSubresourceRegion
        )(this, dst, dst_sub, x, y, src, src_sub, part, format, mode);
        note(this, |r| Cmd::ResolveRegion {
            dst: r.resource(dst),
            dst_sub,
            at: [x, y],
            src: r.resource(src),
            src_sub,
            part: part.as_ref().map(rect),
            format: format.0,
            mode: mode.0,
        });
    }
}

pub(super) fn rect(r: &RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}

pub(super) unsafe fn rects(n: u32, all: *const RECT) -> Vec<[i32; 4]> {
    if all.is_null() || n == 0 {
        return Vec::new();
    }
    // SAFETY: `n` rectangles.
    unsafe { std::slice::from_raw_parts(all, n as usize) }
        .iter()
        .map(rect)
        .collect()
}

pub(super) unsafe extern "system" fn topology(this: Ptr, kind: D3D_PRIMITIVE_TOPOLOGY) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.IASetPrimitiveTopology)(this, kind);
        note(this, |_| Cmd::Topology { kind: kind.0 });
    }
}

pub(super) unsafe extern "system" fn viewports(this: Ptr, n: u32, all: *const D3D12_VIEWPORT) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.RSSetViewports)(this, n, all);
        note(this, |_| Cmd::Viewports {
            all: (0..n as usize)
                .map(|i| all.add(i).cast::<[f32; 6]>().read())
                .collect(),
        });
    }
}

pub(super) unsafe extern "system" fn scissors(this: Ptr, n: u32, all: *const RECT) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.RSSetScissorRects)(this, n, all);
        note(this, |_| Cmd::Scissors { all: rects(n, all) });
    }
}

pub(super) unsafe extern "system" fn blend_factor(this: Ptr, factor: *const f32) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.OMSetBlendFactor)(this, factor);
        note(this, |_| Cmd::BlendFactor {
            factor: factor.cast::<[f32; 4]>().as_ref().copied(),
        });
    }
}

pub(super) unsafe extern "system" fn stencil_ref(this: Ptr, value: u32) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.OMSetStencilRef)(this, value);
        note(this, |_| Cmd::StencilRef { value });
    }
}

pub(super) unsafe extern "system" fn depth_bounds(this: Ptr, min: f32, max: f32) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList1_Vtbl.OMSetDepthBounds)(this, min, max);
        note(this, |_| Cmd::DepthBounds { min, max });
    }
}

pub(super) unsafe extern "system" fn set_pipeline(this: Ptr, pso: Ptr) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.SetPipelineState)(this, pso);
        note(this, |r| Cmd::Pipeline {
            pso: r.object(pso, "a pipeline"),
        });
    }
}

pub(super) unsafe extern "system" fn barrier(this: Ptr, n: u32, all: *const D3D12_RESOURCE_BARRIER) {
    // SAFETY: as above; a barrier's type says which of its parts holds.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.ResourceBarrier)(this, n, all);
        note_change(this, |r| {
            let all = std::slice::from_raw_parts(all, n as usize)
                .iter()
                .map(|b| match b.Type {
                    D3D12_RESOURCE_BARRIER_TYPE_ALIASING => {
                        let a = &b.Anonymous.Aliasing;
                        Barrier::Alias {
                            flags: b.Flags.0,
                            before: r
                                .resource(std::mem::transmute_copy::<_, Ptr>(&a.pResourceBefore)),
                            after: r
                                .resource(std::mem::transmute_copy::<_, Ptr>(&a.pResourceAfter)),
                        }
                    }
                    D3D12_RESOURCE_BARRIER_TYPE_UAV => Barrier::Uav {
                        flags: b.Flags.0,
                        res: r.resource(std::mem::transmute_copy::<_, Ptr>(
                            &b.Anonymous.UAV.pResource,
                        )),
                    },
                    _ => {
                        let t = &b.Anonymous.Transition;
                        Barrier::Transition {
                            flags: b.Flags.0,
                            res: r.resource(std::mem::transmute_copy::<_, Ptr>(&t.pResource)),
                            sub: t.Subresource,
                            before: t.StateBefore.0,
                            after: t.StateAfter.0,
                        }
                    }
                })
                .collect();
            Cmd::Barriers { all }
        });
    }
}

pub(super) unsafe extern "system" fn set_heaps(this: Ptr, n: u32, all: *const Ptr) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.SetDescriptorHeaps)(this, n, all);
        note(this, |r| Cmd::Heaps {
            heaps: (0..n as usize)
                .map(|i| r.object(*all.add(i), "a descriptor heap"))
                .collect(),
        });
    }
}

pub(super) unsafe extern "system" fn root_compute(this: Ptr, root: Ptr) {
    // SAFETY: as above.
    unsafe {
        own!(this, ID3D12GraphicsCommandList_Vtbl.SetComputeRootSignature)(this, root);
        note(this, |r| Cmd::RootSignature {
            graphics: false,
            root: r.object(root, "a root signature"),
        });
    }
}

pub(super) unsafe extern "system" fn root_graphics(this: Ptr, root: Ptr) {
    // SAFETY: as above.
    unsafe {
        own!(
            this,
            ID3D12GraphicsCommandList_Vtbl.SetGraphicsRootSignature
        )(this, root);
        note(this, |r| Cmd::RootSignature {
            graphics: true,
            root: r.object(root, "a root signature"),
        });
    }
}

