
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;


use super::{Host, err};

#[cfg(test)]
pub enum Root {
    Constants(Vec<u32>),
    /// A table starting at this heap index.
    Table {
        sampler: bool,
        index: usize,
    },
    /// A root CBV (or with `srv`, a root SRV) over a buffer of these
    /// dwords ([`Host::dispatch`] only).
    View {
        srv: bool,
        data: Vec<u32>,
    },
}

/// Bindings for [`Host::draw`]: root arguments and the heap contents.
#[cfg(test)]
pub struct Draw<'a> {
    pub roots: Vec<(u32, Root)>,
    /// RGBA32F textures (width, height, texels) at view-heap indices 0..
    pub textures: &'a [(u32, u32, &'a [[f32; 4]])],
    /// Samplers at sampler-heap indices 0..
    pub samplers: &'a [D3D12_SAMPLER_DESC],
    /// Three vertices of stride bytes (none for position-free layouts).
    pub vertices: &'a [f32],
    pub stride: u32,
    pub size: (u32, u32),
    /// Mesh thread groups to dispatch instead of drawing (0: draw).
    pub mesh_groups: u32,
}

#[cfg(test)]
impl Host {
    /// Draws one triangle into an RGBA32F target and returns its texels.
    pub fn draw(
        &self,
        root: &ID3D12RootSignature,
        pso: &ID3D12PipelineState,
        d: &Draw,
    ) -> Result<Vec<[f32; 4]>, String> {
        let dev = &self.device;
        let (w, h) = d.size;
        // SAFETY: D3D12 calls with descriptions that outlive them; resources
        // are kept alive until the fence says the GPU is done.
        unsafe {
            let queue: ID3D12CommandQueue = dev
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .map_err(err)?;
            let alloc: ID3D12CommandAllocator = dev
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .map_err(err)?;
            let list: ID3D12GraphicsCommandList = dev
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, pso)
                .map_err(err)?;
            let tex_desc = |w: u32, h: u32, flags| D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(w),
                Height: h,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let buf_desc = |size: u64| D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let heap = |t| D3D12_HEAP_PROPERTIES {
                Type: t,
                ..Default::default()
            };
            let make = |props: D3D12_HEAP_TYPE,
                        desc: &D3D12_RESOURCE_DESC,
                        state,
                        clear: Option<&D3D12_CLEAR_VALUE>| {
                let mut r: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(
                    &heap(props),
                    D3D12_HEAP_FLAG_NONE,
                    desc,
                    state,
                    clear.map(|c| c as *const _),
                    &mut r,
                )
                .map_err(err)?;
                r.ok_or_else(|| "no resource".to_string())
            };
            let target = make(
                D3D12_HEAP_TYPE_DEFAULT,
                &tex_desc(w, h, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET),
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                Some(&D3D12_CLEAR_VALUE {
                    Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                    Anonymous: D3D12_CLEAR_VALUE_0 { Color: [0.0; 4] },
                }),
            )?;
            let rtv_heap: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .map_err(err)?;
            let rtv = rtv_heap.GetCPUDescriptorHandleForHeapStart();
            dev.CreateRenderTargetView(&target, None, rtv);
            let views: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: d.textures.len().max(1) as u32,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })
                .map_err(err)?;
            let sampler_heap: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                    NumDescriptors: d.samplers.len().max(1) as u32,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })
                .map_err(err)?;
            let view_size = dev
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let sampler_size =
                dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER) as usize;
            let mut keep = Vec::new();
            for (i, &(tw, th, texels)) in d.textures.iter().enumerate() {
                let tex = make(
                    D3D12_HEAP_TYPE_DEFAULT,
                    &tex_desc(tw, th, D3D12_RESOURCE_FLAG_NONE),
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                )?;
                let pitch = (tw as usize * 16).next_multiple_of(256);
                let upload = make(
                    D3D12_HEAP_TYPE_UPLOAD,
                    &buf_desc((pitch * th as usize) as u64),
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                )?;
                let mut p = std::ptr::null_mut();
                upload.Map(0, None, Some(&mut p)).map_err(err)?;
                for y in 0..th as usize {
                    let row = &texels[y * tw as usize..(y + 1) * tw as usize];
                    std::ptr::copy_nonoverlapping(
                        row.as_ptr().cast::<u8>(),
                        p.cast::<u8>().add(y * pitch),
                        row.len() * 16,
                    );
                }
                upload.Unmap(0, None);
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&tex),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: 0,
                    },
                };
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&upload),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                                Width: tw,
                                Height: th,
                                Depth: 1,
                                RowPitch: pitch as u32,
                            },
                        },
                    },
                };
                list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                list.ResourceBarrier(&[transition(
                    &tex,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                        | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                )]);
                let mut at = views.GetCPUDescriptorHandleForHeapStart();
                at.ptr += i * view_size;
                dev.CreateShaderResourceView(&tex, None, at);
                keep.push(tex);
                keep.push(upload);
            }
            for (i, s) in d.samplers.iter().enumerate() {
                let mut at = sampler_heap.GetCPUDescriptorHandleForHeapStart();
                at.ptr += i * sampler_size;
                dev.CreateSampler(s, at);
            }
            let vb = if d.vertices.is_empty() {
                None
            } else {
                let bytes = d.vertices.len() * 4;
                let vb = make(
                    D3D12_HEAP_TYPE_UPLOAD,
                    &buf_desc(bytes as u64),
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                )?;
                let mut p = std::ptr::null_mut();
                vb.Map(0, None, Some(&mut p)).map_err(err)?;
                std::ptr::copy_nonoverlapping(
                    d.vertices.as_ptr().cast::<u8>(),
                    p.cast::<u8>(),
                    bytes,
                );
                vb.Unmap(0, None);
                Some(vb)
            };
            list.SetDescriptorHeaps(&[Some(views.clone()), Some(sampler_heap.clone())]);
            list.SetGraphicsRootSignature(root);
            for (param, arg) in &d.roots {
                match arg {
                    Root::Constants(data) => list.SetGraphicsRoot32BitConstants(
                        *param,
                        data.len() as u32,
                        data.as_ptr().cast(),
                        0,
                    ),
                    Root::Table { sampler, index } => {
                        let (heap, size) = if *sampler {
                            (&sampler_heap, sampler_size)
                        } else {
                            (&views, view_size)
                        };
                        let mut at = heap.GetGPUDescriptorHandleForHeapStart();
                        at.ptr += (index * size) as u64;
                        list.SetGraphicsRootDescriptorTable(*param, at);
                    }
                    Root::View { .. } => return Err("root views in a draw".into()),
                }
            }
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: w as f32,
                Height: h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                right: w as i32,
                bottom: h as i32,
                ..Default::default()
            }]);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            if let Some(vb) = &vb {
                list.IASetVertexBuffers(
                    0,
                    Some(&[D3D12_VERTEX_BUFFER_VIEW {
                        BufferLocation: vb.GetGPUVirtualAddress(),
                        SizeInBytes: (d.vertices.len() * 4) as u32,
                        StrideInBytes: d.stride,
                    }]),
                );
            }
            if d.mesh_groups > 0 {
                let list6: ID3D12GraphicsCommandList6 = list.cast().map_err(err)?;
                list6.DispatchMesh(d.mesh_groups, 1, 1);
            } else {
                list.DrawInstanced(3, 1, 0, 0);
            }
            list.ResourceBarrier(&[transition(
                &target,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let pitch = (w as usize * 16).next_multiple_of(256);
            let readback = make(
                D3D12_HEAP_TYPE_READBACK,
                &buf_desc((pitch * h as usize) as u64),
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
            )?;
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                            Width: w,
                            Height: h,
                            Depth: 1,
                            RowPitch: pitch as u32,
                        },
                    },
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&target),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            list.Close().map_err(err)?;
            queue.ExecuteCommandLists(&[Some(list.cast().map_err(err)?)]);
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err)?;
            queue.Signal(&fence, 1).map_err(err)?;
            while fence.GetCompletedValue() < 1 {
                if dev.GetDeviceRemovedReason().is_err() {
                    return Err(format!(
                        "device removed: {}",
                        err(dev.GetDeviceRemovedReason().unwrap_err())
                    ));
                }
                std::thread::yield_now();
            }
            if let Err(e) = dev.GetDeviceRemovedReason() {
                return Err(format!("device removed: {}", err(e)));
            }
            let mut p = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut p)).map_err(err)?;
            let mut out = Vec::with_capacity((w * h) as usize);
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts(
                    p.cast::<u8>().add(y * pitch).cast::<[f32; 4]>(),
                    w as usize,
                );
                out.extend_from_slice(row);
            }
            readback.Unmap(0, None);
            drop(keep);
            Ok(out)
        }
    }
}

#[cfg(test)]
pub(super) fn transition(
    r: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(r) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

