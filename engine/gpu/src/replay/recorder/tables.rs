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

use super::device_hooks::*;
use super::list_hooks::*;
use super::list_state_hooks::*;
use super::resource_hooks::*;
use super::*;

pub(super) fn device_table(t: &mut [usize; hook::SLOTS]) {
    set!(t, ID3D12Device_Vtbl.CreateCommandQueue = create_queue);
    set!(
        t,
        ID3D12Device_Vtbl.CreateGraphicsPipelineState = create_graphics
    );
    set!(
        t,
        ID3D12Device_Vtbl.CreateComputePipelineState = create_compute
    );
    set!(t, ID3D12Device_Vtbl.CreateCommandList = create_list);
    set!(
        t,
        ID3D12Device_Vtbl.CreateDescriptorHeap = create_descriptors
    );
    set!(t, ID3D12Device_Vtbl.CreateRootSignature = create_root);
    set!(t, ID3D12Device_Vtbl.CreateConstantBufferView = create_cbv);
    set!(t, ID3D12Device_Vtbl.CreateShaderResourceView = create_srv);
    set!(t, ID3D12Device_Vtbl.CreateUnorderedAccessView = create_uav);
    set!(t, ID3D12Device_Vtbl.CreateRenderTargetView = create_rtv);
    set!(t, ID3D12Device_Vtbl.CreateDepthStencilView = create_dsv);
    set!(t, ID3D12Device_Vtbl.CreateSampler = create_sampler);
    set!(t, ID3D12Device_Vtbl.CopyDescriptors = copy_descriptors);
    set!(
        t,
        ID3D12Device_Vtbl.CopyDescriptorsSimple = copy_descriptors_simple
    );
    set!(
        t,
        ID3D12Device_Vtbl.CreateCommittedResource = create_committed
    );
    set!(t, ID3D12Device_Vtbl.CreateHeap = create_heap);
    set!(t, ID3D12Device_Vtbl.CreatePlacedResource = create_placed);
    set!(t, ID3D12Device_Vtbl.CreateQueryHeap = create_queries);
    set!(
        t,
        ID3D12Device_Vtbl.CreateCommandSignature = create_signature
    );
    set!(t, ID3D12Device1_Vtbl.CreatePipelineLibrary = create_library);
    set!(t, ID3D12Device2_Vtbl.CreatePipelineState = create_stream);
    let _ = device_rest(t);
}

pub(super) fn device_rest(t: &mut [usize; hook::SLOTS]) -> &'static [(usize, &'static str)] {
    not_recorded!(t, DEVICE, NAMES:
        ID3D12Device_Vtbl.CreateReservedResource,
        ID3D12Device_Vtbl.OpenSharedHandle,
        ID3D12Device4_Vtbl.CreateCommandList1,
        ID3D12Device4_Vtbl.CreateCommittedResource1,
        ID3D12Device4_Vtbl.CreateHeap1,
        ID3D12Device4_Vtbl.CreateReservedResource1,
        ID3D12Device5_Vtbl.CreateStateObject,
        ID3D12Device8_Vtbl.CreateCommittedResource2,
        ID3D12Device8_Vtbl.CreatePlacedResource1,
        ID3D12Device8_Vtbl.CreateSamplerFeedbackUnorderedAccessView,
        ID3D12Device10_Vtbl.CreateCommittedResource3,
        ID3D12Device10_Vtbl.CreatePlacedResource2,
        ID3D12Device10_Vtbl.CreateReservedResource2,
        ID3D12Device11_Vtbl.CreateSampler2,
    );
    NAMES
}

pub(super) fn queue_table(t: &mut [usize; hook::SLOTS]) {
    set!(t, ID3D12CommandQueue_Vtbl.ExecuteCommandLists = execute);
    let _ = queue_rest(t);
}

pub(super) fn queue_rest(t: &mut [usize; hook::SLOTS]) -> &'static [(usize, &'static str)] {
    not_recorded!(t, QUEUE, NAMES:
        ID3D12CommandQueue_Vtbl.UpdateTileMappings,
        ID3D12CommandQueue_Vtbl.CopyTileMappings,
        ID3D12CommandQueue_Vtbl.Wait,
    );
    NAMES
}

pub(super) fn library_table(t: &mut [usize; hook::SLOTS]) {
    set!(
        t,
        ID3D12PipelineLibrary_Vtbl.LoadGraphicsPipeline = load_graphics
    );
    set!(
        t,
        ID3D12PipelineLibrary_Vtbl.LoadComputePipeline = load_compute
    );
    set!(t, ID3D12PipelineLibrary1_Vtbl.LoadPipeline = load_stream);
}

pub(super) fn list_table(t: &mut [usize; hook::SLOTS]) {
    set!(t, ID3D12GraphicsCommandList_Vtbl.Close = list_close);
    set!(t, ID3D12GraphicsCommandList_Vtbl.Reset = list_reset);
    set!(t, ID3D12GraphicsCommandList_Vtbl.DrawInstanced = draw);
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.DrawIndexedInstanced = draw_indexed
    );
    set!(t, ID3D12GraphicsCommandList_Vtbl.Dispatch = dispatch);
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.CopyBufferRegion = copy_buffer
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.CopyTextureRegion = copy_texture
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.CopyResource = copy_resource
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.ResolveSubresource = resolve
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.IASetPrimitiveTopology = topology
    );
    set!(t, ID3D12GraphicsCommandList_Vtbl.RSSetViewports = viewports);
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.RSSetScissorRects = scissors
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.OMSetBlendFactor = blend_factor
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.OMSetStencilRef = stencil_ref
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetPipelineState = set_pipeline
    );
    set!(t, ID3D12GraphicsCommandList_Vtbl.ResourceBarrier = barrier);
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetDescriptorHeaps = set_heaps
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetComputeRootSignature = root_compute
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetGraphicsRootSignature = root_graphics
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetComputeRootDescriptorTable = table_compute
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetGraphicsRootDescriptorTable = table_graphics
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetComputeRoot32BitConstant = constant_compute
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetGraphicsRoot32BitConstant = constant_graphics
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetComputeRoot32BitConstants = constants_compute
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetGraphicsRoot32BitConstants = constants_graphics
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetComputeRootConstantBufferView = cbv_compute
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetGraphicsRootConstantBufferView = cbv_graphics
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetComputeRootShaderResourceView = srv_compute
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetGraphicsRootShaderResourceView = srv_graphics
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetComputeRootUnorderedAccessView = uav_compute
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.SetGraphicsRootUnorderedAccessView = uav_graphics
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.IASetIndexBuffer = index_buffer
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.IASetVertexBuffers = vertex_buffers
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.OMSetRenderTargets = targets
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.ClearDepthStencilView = clear_depth
    );
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.ClearRenderTargetView = clear_target
    );
    set!(t, ID3D12GraphicsCommandList_Vtbl.DiscardResource = discard);
    set!(t, ID3D12GraphicsCommandList_Vtbl.BeginQuery = begin_query);
    set!(t, ID3D12GraphicsCommandList_Vtbl.EndQuery = end_query);
    set!(
        t,
        ID3D12GraphicsCommandList_Vtbl.ResolveQueryData = resolve_query
    );
    set!(t, ID3D12GraphicsCommandList_Vtbl.ExecuteIndirect = indirect);
    set!(
        t,
        ID3D12GraphicsCommandList1_Vtbl.ResolveSubresourceRegion = resolve_region
    );
    set!(
        t,
        ID3D12GraphicsCommandList1_Vtbl.OMSetDepthBounds = depth_bounds
    );
    set!(
        t,
        ID3D12GraphicsCommandList2_Vtbl.WriteBufferImmediate = write_immediate
    );
    set!(
        t,
        ID3D12GraphicsCommandList6_Vtbl.DispatchMesh = dispatch_mesh
    );
    let _ = list_rest(t);
}

pub(super) fn list_rest(t: &mut [usize; hook::SLOTS]) -> &'static [(usize, &'static str)] {
    not_recorded!(t, LIST, NAMES:
        ID3D12GraphicsCommandList_Vtbl.ClearState,
        ID3D12GraphicsCommandList_Vtbl.CopyTiles,
        ID3D12GraphicsCommandList_Vtbl.ExecuteBundle,
        ID3D12GraphicsCommandList_Vtbl.SOSetTargets,
        ID3D12GraphicsCommandList_Vtbl.ClearUnorderedAccessViewUint,
        ID3D12GraphicsCommandList_Vtbl.ClearUnorderedAccessViewFloat,
        ID3D12GraphicsCommandList_Vtbl.SetPredication,
        ID3D12GraphicsCommandList1_Vtbl.AtomicCopyBufferUINT,
        ID3D12GraphicsCommandList1_Vtbl.AtomicCopyBufferUINT64,
        ID3D12GraphicsCommandList1_Vtbl.SetSamplePositions,
        ID3D12GraphicsCommandList1_Vtbl.SetViewInstanceMask,
        ID3D12GraphicsCommandList3_Vtbl.SetProtectedResourceSession,
        ID3D12GraphicsCommandList4_Vtbl.BeginRenderPass,
        ID3D12GraphicsCommandList4_Vtbl.EndRenderPass,
        ID3D12GraphicsCommandList4_Vtbl.InitializeMetaCommand,
        ID3D12GraphicsCommandList4_Vtbl.ExecuteMetaCommand,
        ID3D12GraphicsCommandList4_Vtbl.BuildRaytracingAccelerationStructure,
        ID3D12GraphicsCommandList4_Vtbl.EmitRaytracingAccelerationStructurePostbuildInfo,
        ID3D12GraphicsCommandList4_Vtbl.CopyRaytracingAccelerationStructure,
        ID3D12GraphicsCommandList4_Vtbl.SetPipelineState1,
        ID3D12GraphicsCommandList4_Vtbl.DispatchRays,
        ID3D12GraphicsCommandList5_Vtbl.RSSetShadingRate,
        ID3D12GraphicsCommandList5_Vtbl.RSSetShadingRateImage,
        ID3D12GraphicsCommandList7_Vtbl.Barrier,
        ID3D12GraphicsCommandList8_Vtbl.OMSetFrontAndBackStencilRef,
        ID3D12GraphicsCommandList9_Vtbl.RSSetDepthBias,
        ID3D12GraphicsCommandList9_Vtbl.IASetIndexBufferStripCutValue,
        ID3D12GraphicsCommandList10_Vtbl.SetProgram,
        ID3D12GraphicsCommandList10_Vtbl.DispatchGraph,
    );
    NAMES
}

/// How often each method that is not recorded was called so far.
pub(in crate::replay) fn not_recorded_calls() -> Vec<(String, u64)> {
    let mut t = [0; hook::SLOTS];
    let mut all = Vec::new();
    for (class, of, names) in [
        (hook::LIST, "command list", list_rest(&mut t)),
        (hook::DEVICE, "device", device_rest(&mut t)),
        (hook::QUEUE, "queue", queue_rest(&mut t)),
    ] {
        for &(slot, name) in names {
            let what = match (class, name) {
                // Submissions are in order anyway: the replay has one queue.
                (hook::QUEUE, "Wait") => {
                    "waits of the drawing queue (the swap chain makes some) are not replayed"
                        .to_owned()
                }
                _ => format!("{of} method {name} is not recorded, and was called"),
            };
            all.push((what, hook::count(class, slot)));
        }
    }
    all
}

