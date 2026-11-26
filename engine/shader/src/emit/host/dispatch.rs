
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;


use super::draw::{Root, transition};
use super::views::{Dispatch, Stride, View};
use super::{Host, err};

#[cfg(test)]
impl Host {
    /// Runs a compute pipeline and returns each UAV's contents in heap
    /// order (texels as float bits, row by row).
    pub fn dispatch(
        &self,
        root: &ID3D12RootSignature,
        pso: &ID3D12PipelineState,
        d: &Dispatch,
    ) -> Result<Vec<Vec<u32>>, String> {
        let dev = &self.device;
        // SAFETY: D3D12 calls with descriptions that outlive them; resources
        // stay alive until the fence says the GPU is done.
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
            let make = |heap: D3D12_HEAP_TYPE, desc: &D3D12_RESOURCE_DESC, state| {
                let mut r: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: heap,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    desc,
                    state,
                    None,
                    &mut r,
                )
                .map_err(err)?;
                r.ok_or_else(|| "no resource".to_string())
            };
            let sample = DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            };
            let texture_as = |w: u32, h: u32, flags, format| D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(w),
                Height: h,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: sample,
                Flags: flags,
                ..Default::default()
            };
            let texture = |w, h, flags| texture_as(w, h, flags, DXGI_FORMAT_R32G32B32A32_FLOAT);
            let buffer = |size: u64, flags| D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: sample,
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: flags,
                ..Default::default()
            };
            // Texel words: 4 for RGBA32F, 1 for R32_UINT.
            let footprint_as =
                |r: &ID3D12Resource, w: u32, h: u32, words: u32| D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(r),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: if words == 1 {
                                    DXGI_FORMAT_R32_UINT
                                } else {
                                    DXGI_FORMAT_R32G32B32A32_FLOAT
                                },
                                Width: w,
                                Height: h,
                                Depth: 1,
                                RowPitch: (w * 4 * words).next_multiple_of(256),
                            },
                        },
                    },
                };
            let footprint = |r: &ID3D12Resource, w: u32, h: u32| footprint_as(r, w, h, 4);
            let subresource = |r: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(r),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let heap = |ty, n: usize| -> Result<ID3D12DescriptorHeap, String> {
                dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: ty,
                    NumDescriptors: n.max(1) as u32,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })
                .map_err(err)
            };
            // An upload buffer holding `data` (sized for a constant buffer).
            let upload = |data: &[u32]| -> Result<ID3D12Resource, String> {
                let size = (data.len() * 4).next_multiple_of(256);
                let b = make(
                    D3D12_HEAP_TYPE_UPLOAD,
                    &buffer(size as u64, D3D12_RESOURCE_FLAG_NONE),
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                )?;
                let mut p = std::ptr::null_mut();
                b.Map(0, None, Some(&mut p)).map_err(err)?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u32>(), data.len());
                b.Unmap(0, None);
                Ok(b)
            };
            let views = heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, d.views.len())?;
            let samplers = heap(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, d.samplers.len())?;
            let view_size = dev
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let sampler_size =
                dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER) as usize;
            let mut keep = Vec::new();
            // UAVs: (resource, dwords or texture size, readback buffer).
            let mut uavs = Vec::new();
            for (i, v) in d.views.iter().enumerate() {
                let mut at = views.GetCPUDescriptorHandleForHeapStart();
                at.ptr += i * view_size;
                match *v {
                    View::Texture(..) | View::Cube(..) => {
                        let (w, h, mut texels, faces, mips) = match *v {
                            View::Texture(w, h, t) => (w, h, t, 1, 1),
                            View::Cube(n, mips, t) => (n, n, t, 6, mips),
                            _ => unreachable!(),
                        };
                        let mut desc = texture(w, h, D3D12_RESOURCE_FLAG_NONE);
                        desc.DepthOrArraySize = faces as u16;
                        desc.MipLevels = mips;
                        let tex = make(
                            D3D12_HEAP_TYPE_DEFAULT,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        )?;
                        for sub in 0..u32::from(mips) * faces {
                            let mip = sub % u32::from(mips);
                            let (w, h) = ((w >> mip).max(1), (h >> mip).max(1));
                            let pitch = (w as usize * 16).next_multiple_of(256);
                            let upload = make(
                                D3D12_HEAP_TYPE_UPLOAD,
                                &buffer((pitch * h as usize) as u64, D3D12_RESOURCE_FLAG_NONE),
                                D3D12_RESOURCE_STATE_GENERIC_READ,
                            )?;
                            let mut p = std::ptr::null_mut();
                            upload.Map(0, None, Some(&mut p)).map_err(err)?;
                            let (level, rest) = texels.split_at((w * h) as usize);
                            texels = rest;
                            for y in 0..h as usize {
                                let row = &level[y * w as usize..(y + 1) * w as usize];
                                std::ptr::copy_nonoverlapping(
                                    row.as_ptr().cast::<u8>(),
                                    p.cast::<u8>().add(y * pitch),
                                    row.len() * 16,
                                );
                            }
                            upload.Unmap(0, None);
                            let mut at_face = subresource(&tex);
                            at_face.Anonymous.SubresourceIndex = sub;
                            list.CopyTextureRegion(
                                &at_face,
                                0,
                                0,
                                0,
                                &footprint(&upload, w, h),
                                None,
                            );
                            keep.push(upload);
                        }
                        list.ResourceBarrier(&[transition(
                            &tex,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                        )]);
                        let cube = D3D12_SHADER_RESOURCE_VIEW_DESC {
                            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                            ViewDimension: D3D12_SRV_DIMENSION_TEXTURECUBE,
                            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                                TextureCube: D3D12_TEXCUBE_SRV {
                                    MipLevels: u32::from(mips),
                                    ..Default::default()
                                },
                            },
                        };
                        dev.CreateShaderResourceView(
                            &tex,
                            (faces == 6).then_some(&cube as *const _),
                            at,
                        );
                        keep.push(tex);
                    }
                    View::RawUav(_) | View::HalfUav(_) => {
                        // (dwords, format, elements, flags)
                        let (n, format, elements, flags) = match *v {
                            View::HalfUav(e) => (
                                2 * e,
                                DXGI_FORMAT_R16G16B16A16_FLOAT,
                                e,
                                D3D12_BUFFER_UAV_FLAG_NONE,
                            ),
                            View::RawUav(n) => {
                                (n, DXGI_FORMAT_R32_TYPELESS, n, D3D12_BUFFER_UAV_FLAG_RAW)
                            }
                            _ => unreachable!(),
                        };
                        let bytes = u64::from(n) * 4;
                        let b = make(
                            D3D12_HEAP_TYPE_DEFAULT,
                            &buffer(bytes, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                            D3D12_RESOURCE_STATE_COMMON,
                        )?;
                        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                            Format: format,
                            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                                Buffer: D3D12_BUFFER_UAV {
                                    NumElements: elements,
                                    Flags: flags,
                                    ..Default::default()
                                },
                            },
                        };
                        dev.CreateUnorderedAccessView(&b, None, Some(&desc), at);
                        let rb = make(
                            D3D12_HEAP_TYPE_READBACK,
                            &buffer(bytes, D3D12_RESOURCE_FLAG_NONE),
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        )?;
                        uavs.push((b, None, n, rb));
                    }
                    View::TypedUav {
                        data,
                        format,
                        elem_bytes,
                    } => {
                        let n = data.len() as u32;
                        let bytes = u64::from(n) * 4;
                        let elements = (bytes / u64::from(elem_bytes)) as u32;
                        let b = make(
                            D3D12_HEAP_TYPE_DEFAULT,
                            &buffer(bytes, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        )?;
                        let up = upload(data)?;
                        list.CopyBufferRegion(&b, 0, &up, 0, bytes);
                        list.ResourceBarrier(&[transition(
                            &b,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        )]);
                        keep.push(up);
                        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                            Format: format,
                            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                                Buffer: D3D12_BUFFER_UAV {
                                    NumElements: elements,
                                    Flags: D3D12_BUFFER_UAV_FLAG_NONE,
                                    ..Default::default()
                                },
                            },
                        };
                        dev.CreateUnorderedAccessView(&b, None, Some(&desc), at);
                        let rb = make(
                            D3D12_HEAP_TYPE_READBACK,
                            &buffer(bytes, D3D12_RESOURCE_FLAG_NONE),
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        )?;
                        uavs.push((b, None, n, rb));
                    }
                    View::TextureUav(w, h) => {
                        let t = make(
                            D3D12_HEAP_TYPE_DEFAULT,
                            &texture(w, h, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        )?;
                        dev.CreateUnorderedAccessView(&t, None, None, at);
                        let pitch = (w * 16).next_multiple_of(256);
                        let rb = make(
                            D3D12_HEAP_TYPE_READBACK,
                            &buffer(u64::from(pitch * h), D3D12_RESOURCE_FLAG_NONE),
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        )?;
                        uavs.push((t, Some((w, h)), 4, rb));
                    }
                    View::UintTextureUav(w, h) => {
                        let t = make(
                            D3D12_HEAP_TYPE_DEFAULT,
                            &texture_as(
                                w,
                                h,
                                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                                DXGI_FORMAT_R32_UINT,
                            ),
                            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        )?;
                        dev.CreateUnorderedAccessView(&t, None, None, at);
                        let pitch = (w * 4).next_multiple_of(256);
                        let rb = make(
                            D3D12_HEAP_TYPE_READBACK,
                            &buffer(u64::from(pitch * h), D3D12_RESOURCE_FLAG_NONE),
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        )?;
                        uavs.push((t, Some((w, h)), 1, rb));
                    }
                    View::Cbv(data) => {
                        let b = upload(data)?;
                        dev.CreateConstantBufferView(
                            Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                                BufferLocation: b.GetGPUVirtualAddress(),
                                SizeInBytes: (data.len() * 4).next_multiple_of(256) as u32,
                            }),
                            at,
                        );
                        keep.push(b);
                    }
                    View::Buffer { data, stride } => {
                        let b = upload(data)?;
                        let (format, elements, stride, flags) = match stride {
                            Stride::Raw => (
                                DXGI_FORMAT_R32_TYPELESS,
                                data.len(),
                                0,
                                D3D12_BUFFER_SRV_FLAG_RAW,
                            ),
                            Stride::Typed => (
                                DXGI_FORMAT_R32G32B32A32_FLOAT,
                                data.len() / 4,
                                0,
                                D3D12_BUFFER_SRV_FLAG_NONE,
                            ),
                            Stride::Structured(s) => (
                                DXGI_FORMAT_UNKNOWN,
                                data.len() * 4 / s as usize,
                                s,
                                D3D12_BUFFER_SRV_FLAG_NONE,
                            ),
                            Stride::Format(f, bytes) => (
                                f,
                                data.len() * 4 / bytes as usize,
                                0,
                                D3D12_BUFFER_SRV_FLAG_NONE,
                            ),
                        };
                        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                            Format: format,
                            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                                Buffer: D3D12_BUFFER_SRV {
                                    FirstElement: 0,
                                    NumElements: elements as u32,
                                    StructureByteStride: stride,
                                    Flags: flags,
                                },
                            },
                        };
                        dev.CreateShaderResourceView(&b, Some(&desc), at);
                        keep.push(b);
                    }
                }
            }
            for (i, s) in d.samplers.iter().enumerate() {
                let mut at = samplers.GetCPUDescriptorHandleForHeapStart();
                at.ptr += i * sampler_size;
                dev.CreateSampler(s, at);
            }
            list.SetDescriptorHeaps(&[Some(views.clone()), Some(samplers.clone())]);
            list.SetComputeRootSignature(root);
            for (param, arg) in &d.roots {
                match arg {
                    Root::Constants(data) => list.SetComputeRoot32BitConstants(
                        *param,
                        data.len() as u32,
                        data.as_ptr().cast(),
                        0,
                    ),
                    Root::Table { sampler, index } => {
                        let (h, size) = if *sampler {
                            (&samplers, sampler_size)
                        } else {
                            (&views, view_size)
                        };
                        let mut at = h.GetGPUDescriptorHandleForHeapStart();
                        at.ptr += (index * size) as u64;
                        list.SetComputeRootDescriptorTable(*param, at);
                    }
                    Root::View { srv, data } => {
                        let b = upload(data)?;
                        let va = b.GetGPUVirtualAddress();
                        if *srv {
                            list.SetComputeRootShaderResourceView(*param, va);
                        } else {
                            list.SetComputeRootConstantBufferView(*param, va);
                        }
                        keep.push(b);
                    }
                }
            }
            let [x, y, z] = d.groups;
            list.Dispatch(x, y, z);
            for (r, size, n, rb) in &uavs {
                // (Buffers were promoted from the common state by the dispatch.)
                list.ResourceBarrier(&[transition(
                    r,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                match size {
                    Some((w, h)) => list.CopyTextureRegion(
                        &footprint_as(rb, *w, *h, *n),
                        0,
                        0,
                        0,
                        &subresource(r),
                        None,
                    ),
                    None => list.CopyBufferRegion(rb, 0, r, 0, u64::from(*n) * 4),
                }
            }
            list.Close().map_err(err)?;
            queue.ExecuteCommandLists(&[Some(list.cast().map_err(err)?)]);
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err)?;
            queue.Signal(&fence, 1).map_err(err)?;
            while fence.GetCompletedValue() < 1 {
                if let Err(e) = dev.GetDeviceRemovedReason() {
                    return Err(format!("device removed: {}", err(e)));
                }
                std::thread::yield_now();
            }
            let mut out = Vec::new();
            for (_, size, n, rb) in &uavs {
                let mut p = std::ptr::null_mut();
                rb.Map(0, None, Some(&mut p)).map_err(err)?;
                let words = match size {
                    Some((w, h)) => {
                        let words = *n as usize;
                        let pitch = (*w as usize * 4 * words).next_multiple_of(256);
                        (0..*h as usize)
                            .flat_map(|y| {
                                std::slice::from_raw_parts(
                                    p.cast::<u8>().add(y * pitch).cast::<u32>(),
                                    *w as usize * words,
                                )
                                .to_vec()
                            })
                            .collect()
                    }
                    None => std::slice::from_raw_parts(p.cast::<u32>(), *n as usize).to_vec(),
                };
                rb.Unmap(0, None);
                out.push(words);
            }
            drop(keep);
            Ok(out)
        }
    }
}
