//! Per-draw, compute, upload, and readback helpers on `Scene`.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
use windows::core::Interface;

use crate::d3d::{
    ARGUMENTS, FROM, In, SIZE, TO, UAV, buffer_desc, committed, footprint, moved, sub_of,
    uav_barrier,
};
use crate::scene_frame::Still;
use crate::scene_init::Scene;

impl Scene {
    pub(crate) fn cpu(&self, i: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        // SAFETY: a slot of the heap.
        let start = unsafe { self.views.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + i * self.step,
        }
    }

    /// Slot `i` shows `texture`.
    pub(crate) fn show(&self, i: usize, texture: &ID3D12Resource) {
        // SAFETY: a slot of the heap and a live texture.
        unsafe {
            self.device
                .CreateShaderResourceView(texture, None, self.cpu(i))
        };
    }

    pub(crate) fn wait(&mut self) {
        self.value += 1;
        // SAFETY: our queue, fence and event.
        unsafe {
            self.queue.Signal(&self.fence, self.value).unwrap();
            self.fence
                .SetEventOnCompletion(self.value, self.event)
                .unwrap();
            WaitForSingleObject(self.event, INFINITE);
            self.device.GetDeviceRemovedReason().unwrap();
        }
    }

    pub(crate) fn open(&self, i: usize) -> ID3D12GraphicsCommandList {
        let (allocator, list) = &self.lists[i];
        // SAFETY: its last run was waited for.
        unsafe {
            allocator.Reset().unwrap();
            list.Reset(allocator, None).unwrap();
        }
        list.clone()
    }

    pub(crate) fn send(&self, lists: &[&ID3D12GraphicsCommandList]) {
        let all: Vec<Option<ID3D12CommandList>> =
            lists.iter().map(|l| Some(l.cast().unwrap())).collect();
        // SAFETY: closed lists.
        unsafe { self.queue.ExecuteCommandLists(&all) };
    }

    /// Fills rows of a 16x16 texture in the upload buffer at `at`.
    pub(crate) fn fill(&self, at: usize, color: [u8; 4]) {
        for y in 0..16 {
            for x in 0..16 {
                let mut c = color;
                c[1] = c[1].wrapping_add((x * 8 + y) as u8);
                // SAFETY: inside the mapping (16 rows of 256 bytes).
                unsafe {
                    self.texels
                        .1
                        .add(at + y * 256 + x * 4)
                        .cast::<[u8; 4]>()
                        .write(c)
                };
            }
        }
    }

    pub(crate) fn texture_copy(
        &self,
        l: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        at: u64,
    ) {
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer never released.
            pResource: unsafe { std::mem::transmute_copy(&self.texels.0) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint(at, 16),
            },
        };
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        // SAFETY: an open list; live resources.
        unsafe { l.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }

    /// A quad of the texture in slot `pick`, at the constants at `at`.
    pub(crate) fn quad(&self, l: &ID3D12GraphicsCommandList, pick: u32, at: u64, record: u64) {
        // SAFETY: an open list; live objects.
        unsafe {
            l.SetGraphicsRootSignature(&self.draw.root);
            l.SetPipelineState(&self.draw.pso);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            l.SetGraphicsRoot32BitConstant(0, pick, 0);
            l.SetGraphicsRootConstantBufferView(1, self.constants.0.GetGPUVirtualAddress() + at);
            l.SetGraphicsRootDescriptorTable(2, self.views.GetGPUDescriptorHandleForHeapStart());
            let step = self
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER);
            let first = self.samplers.GetGPUDescriptorHandleForHeapStart().ptr;
            l.SetGraphicsRootDescriptorTable(
                3,
                D3D12_GPU_DESCRIPTOR_HANDLE {
                    ptr: first + 2 * u64::from(step),
                },
            );
            l.ExecuteIndirect(
                &self.signature,
                1,
                &self.scratch,
                record * 16,
                None::<&ID3D12Resource>,
                0,
            );
        }
    }

    /// The arguments made and clamped, as the host layer does it.
    pub(crate) fn arguments(
        &self,
        l: &ID3D12GraphicsCommandList,
        before: D3D12_RESOURCE_STATES,
    ) {
        // SAFETY: an open list; live objects.
        unsafe {
            l.SetComputeRootSignature(&self.args.root);
            l.SetPipelineState(&self.args.pso);
            l.SetComputeRootShaderResourceView(0, self.shared.0.GetGPUVirtualAddress() + 65536);
            l.SetComputeRootUnorderedAccessView(1, self.arguments.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
            l.ResourceBarrier(&[
                uav_barrier(),
                moved(
                    &self.arguments,
                    UAV,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                ),
            ]);
            l.ResourceBarrier(&[uav_barrier(), moved(&self.scratch, before, UAV)]);
            l.SetComputeRootSignature(&self.clamp.root);
            l.SetPipelineState(&self.clamp.pso);
            let constants = [0u32, 16, 2, 0, 0, 0];
            l.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, self.arguments.GetGPUVirtualAddress());
            l.SetComputeRootShaderResourceView(2, self.arguments.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(3, self.scratch.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(4, self.budget.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
            l.ResourceBarrier(&[uav_barrier(), moved(&self.scratch, UAV, ARGUMENTS)]);
            l.ResourceBarrier(&[moved(
                &self.arguments,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                UAV,
            )]);
        }
    }

    pub(crate) fn place(&self, at: usize, place: [f32; 4]) {
        // SAFETY: inside the mapping.
        unsafe { self.constants.1.add(at).cast::<[f32; 4]>().write(place) };
    }

    pub(crate) fn give(&self, at: usize, value: u32) {
        // SAFETY: inside the mapping.
        unsafe { self.shared.1.add(at).cast::<u32>().write(value) };
    }

    pub(crate) fn targets(&self, l: &ID3D12GraphicsCommandList) {
        // SAFETY: an open list; live objects.
        unsafe {
            let (rtv, dsv) = (
                self.rtvs.GetCPUDescriptorHandleForHeapStart(),
                self.dsvs.GetCPUDescriptorHandleForHeapStart(),
            );
            l.OMSetRenderTargets(1, Some(&rtv), false, Some(&dsv));
            let size = SIZE as f32;
            l.RSSetViewports(&[D3D12_VIEWPORT {
                Width: size,
                Height: size,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            l.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: SIZE as i32,
                bottom: SIZE as i32,
            }]);
            l.SetDescriptorHeaps(&[Some(self.views.clone()), Some(self.samplers.clone())]);
        }
    }

    /// A triangle with vertices and indices.
    pub(crate) fn triangle(&self, l: &ID3D12GraphicsCommandList, n: u32) {
        // SAFETY: an open list; live objects; inside the mapping, which the
        // card does not use now.
        unsafe {
            let x = 0.5 + 0.05 * n as f32;
            let vertices = [
                [x, 0.4, 1.0, 0.0, 0.0],
                [x + 0.3, 0.4, 0.0, 1.0, 0.0],
                [x, 0.9, 0.0, 0.0, 1.0f32],
            ];
            self.vertices.1.cast::<[[f32; 5]; 3]>().write(vertices);
            self.vertices
                .1
                .add(1024)
                .cast::<[u16; 4]>()
                .write([9, 2, 0, 1]);
            let at = self.vertices.0.GetGPUVirtualAddress();
            l.SetGraphicsRootSignature(&self.plain.root);
            l.SetPipelineState(&self.plain.pso);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            l.IASetVertexBuffers(
                0,
                Some(&[D3D12_VERTEX_BUFFER_VIEW {
                    BufferLocation: at,
                    SizeInBytes: 60,
                    StrideInBytes: 20,
                }]),
            );
            let indices = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: at + 1024,
                SizeInBytes: 8,
                Format: DXGI_FORMAT_R16_UINT,
            };
            l.IASetIndexBuffer(Some(&indices));
            l.DrawIndexedInstanced(3, 1, 1, 0, 0);
        }
    }

    /// Fills the textures no frame touches.
    pub(crate) fn fill_still(&mut self) {
        // SAFETY: an open list; live objects; mappings written inside their
        // bounds.
        unsafe {
            let (up, data) = committed(
                &self.device,
                In::Upload,
                &buffer_desc(1 << 20, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_GENERIC_READ,
            );
            for i in 0..1 << 20 {
                data.add(i).write((i * 31 + i / 251) as u8 & 0x3b);
            }
            let l = self.open(0);
            let mut at = 0;
            for (name, t, ends) in &self.still {
                if *name == "stencil" {
                    let view = D3D12_DEPTH_STENCIL_VIEW_DESC {
                        Format: DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
                        ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                        ..Default::default()
                    };
                    let heap: ID3D12DescriptorHeap = self
                        .device
                        .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                            Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                            NumDescriptors: 1,
                            ..Default::default()
                        })
                        .unwrap();
                    let dsv = heap.GetCPUDescriptorHandleForHeapStart();
                    self.device.CreateDepthStencilView(t, Some(&view), dsv);
                    let both = D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL;
                    l.ClearDepthStencilView(dsv, both, 0.75, 3, None);
                    let part = RECT {
                        left: 5,
                        top: 6,
                        right: 20,
                        bottom: 17,
                    };
                    l.ClearDepthStencilView(dsv, both, 0.25, 200, Some(&[part]));
                    continue;
                }
                let desc = t.GetDesc();
                let layers = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                    1
                } else {
                    desc.DepthOrArraySize
                };
                for sub in 0..u32::from(layers * desc.MipLevels) {
                    let mut f = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
                    let mut total = 0;
                    self.device.GetCopyableFootprints(
                        &desc,
                        sub,
                        1,
                        at,
                        Some(&mut f),
                        None,
                        None,
                        Some(&mut total),
                    );
                    at = (at + total).next_multiple_of(512);
                    let from = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&up),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
                    };
                    l.CopyTextureRegion(&sub_of(t, sub), 0, 0, 0, &from, None);
                }
                l.ResourceBarrier(&[moved(t, TO, *ends)]);
            }
            l.Close().unwrap();
            self.send(&[&l]);
            self.wait();
        }
    }

    /// What they hold.
    pub(crate) fn read_still(&mut self) -> Vec<Still> {
        let mut all = Vec::new();
        for (name, t, state) in self.still.clone() {
            // SAFETY: as above.
            unsafe {
                let desc = t.GetDesc();
                let layers = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                    1
                } else {
                    desc.DepthOrArraySize
                };
                let planes = if name == "stencil" { 2 } else { 1 };
                let mut subs = Vec::new();
                for sub in 0..u32::from(layers * desc.MipLevels) * planes {
                    let mut f = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
                    let (mut rows, mut len) = (0, 0);
                    self.device.GetCopyableFootprints(
                        &desc,
                        sub,
                        1,
                        0,
                        Some(&mut f),
                        Some(&mut rows),
                        Some(&mut len),
                        None,
                    );
                    let l = self.open(0);
                    l.ResourceBarrier(&[moved(&t, state, FROM)]);
                    let to = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&self.back.0),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
                    };
                    l.CopyTextureRegion(&to, 0, 0, 0, &sub_of(&t, sub), None);
                    l.ResourceBarrier(&[moved(&t, FROM, state)]);
                    l.Close().unwrap();
                    self.send(&[&l]);
                    self.wait();
                    let pitch = f.Footprint.RowPitch as usize;
                    let rows = (0..rows as usize * f.Footprint.Depth as usize)
                        .map(|r| {
                            std::slice::from_raw_parts(self.back.1.add(r * pitch), len as usize)
                                .to_vec()
                        })
                        .collect();
                    subs.push((pitch, rows));
                }
                all.push(Still { name, subs });
            }
        }
        all
    }
}
