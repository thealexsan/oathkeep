//! Individual D3D12 command and barrier replay encoding.

use super::*;

impl Player {
    pub(super) unsafe fn command(&self, l: &ID3D12GraphicsCommandList, c: &Cmd, version: usize) {
        let res = |id: u32| self.resource(id);
        let view = |s: &Slot| self.targets.get(s).copied().unwrap_or_default();
        // SAFETY: the caller's.
        unsafe {
            match c {
                Cmd::Draw {
                    vertices,
                    instances,
                    first,
                    first_instance,
                } => l.DrawInstanced(*vertices, *instances, *first, *first_instance),
                Cmd::DrawIndexed {
                    indices,
                    instances,
                    first,
                    base,
                    first_instance,
                } => {
                    l.DrawIndexedInstanced(*indices, *instances, *first, *base, *first_instance);
                }
                Cmd::Dispatch { groups } => l.Dispatch(groups[0], groups[1], groups[2]),
                Cmd::DispatchMesh { groups } => {
                    if let Ok(l) = l.cast::<ID3D12GraphicsCommandList6>() {
                        l.DispatchMesh(groups[0], groups[1], groups[2]);
                    }
                }
                Cmd::CopyBuffer {
                    dst,
                    dst_at,
                    src,
                    src_at,
                    len,
                } => {
                    if let (Some(dst), Some(src)) = (res(*dst), res(*src)) {
                        l.CopyBufferRegion(dst, *dst_at, src, *src_at, *len);
                    }
                }
                Cmd::CopyTexture { dst, at, src, part } => {
                    let side = |loc: &Loc| match loc {
                        Loc::Sub { res: id, sub } => Some(gpu::sub_location(res(*id)?, *sub)),
                        Loc::Placed {
                            res: id,
                            offset,
                            format,
                            size,
                            pitch,
                        } => {
                            let f = D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT(*format),
                                Width: size[0],
                                Height: size[1],
                                Depth: size[2],
                                RowPitch: *pitch,
                            };
                            Some(gpu::placed_location(
                                res(*id)?,
                                D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                                    Offset: *offset,
                                    Footprint: f,
                                },
                            ))
                        }
                    };
                    if let (Some(dst), Some(src)) = (side(dst), side(src)) {
                        let part = part.map(|p| D3D12_BOX {
                            left: p[0],
                            top: p[1],
                            front: p[2],
                            right: p[3],
                            bottom: p[4],
                            back: p[5],
                        });
                        l.CopyTextureRegion(
                            &dst,
                            at[0],
                            at[1],
                            at[2],
                            &src,
                            part.as_ref().map(|p| p as *const _),
                        );
                    }
                }
                Cmd::CopyResource { dst, src } => {
                    if let (Some(dst), Some(src)) = (res(*dst), res(*src)) {
                        l.CopyResource(dst, src);
                    }
                }
                Cmd::Resolve {
                    dst,
                    dst_sub,
                    src,
                    src_sub,
                    format,
                } => {
                    if let (Some(dst), Some(src)) = (res(*dst), res(*src)) {
                        l.ResolveSubresource(dst, *dst_sub, src, *src_sub, DXGI_FORMAT(*format));
                    }
                }
                Cmd::ResolveRegion {
                    dst,
                    dst_sub,
                    at,
                    src,
                    src_sub,
                    part,
                    format,
                    mode,
                } => {
                    if let (Some(dst), Some(src), Ok(l)) =
                        (res(*dst), res(*src), l.cast::<ID3D12GraphicsCommandList1>())
                    {
                        let part = part.map(to_rect);
                        let part = part.as_ref().map(|r| r as *const _);
                        l.ResolveSubresourceRegion(
                            dst,
                            *dst_sub,
                            at[0],
                            at[1],
                            src,
                            *src_sub,
                            part,
                            DXGI_FORMAT(*format),
                            D3D12_RESOLVE_MODE(*mode),
                        );
                    }
                }
                Cmd::Topology { kind } => l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(*kind)),
                Cmd::Viewports { all } => {
                    let all: Vec<D3D12_VIEWPORT> = all
                        .iter()
                        .map(|v| D3D12_VIEWPORT {
                            TopLeftX: v[0],
                            TopLeftY: v[1],
                            Width: v[2],
                            Height: v[3],
                            MinDepth: v[4],
                            MaxDepth: v[5],
                        })
                        .collect();
                    l.RSSetViewports(&all);
                }
                Cmd::Scissors { all } => {
                    l.RSSetScissorRects(&all.iter().map(|r| to_rect(*r)).collect::<Vec<_>>())
                }
                Cmd::BlendFactor { factor } => l.OMSetBlendFactor(factor.as_ref()),
                Cmd::StencilRef { value } => l.OMSetStencilRef(*value),
                Cmd::DepthBounds { min, max } => {
                    if let Ok(l) = l.cast::<ID3D12GraphicsCommandList1>() {
                        l.OMSetDepthBounds(*min, *max);
                    }
                }
                Cmd::Pipeline { pso } => {
                    if let Some(p) = self.pipelines.get(pso) {
                        l.SetPipelineState(p);
                    }
                }
                Cmd::Barriers { all } => {
                    let all: Vec<D3D12_RESOURCE_BARRIER> =
                        all.iter().filter_map(|b| self.barrier(b)).collect();
                    barriers(l, &all);
                }
                Cmd::Heaps { heaps } => {
                    let all: Vec<Option<ID3D12DescriptorHeap>> = heaps
                        .iter()
                        .filter_map(|id| {
                            Some(Some(self.descriptors.get(id)?.0.get(version)?.heap.clone()))
                        })
                        .collect();
                    l.SetDescriptorHeaps(&all);
                }
                Cmd::RootSignature { graphics, root } => {
                    let root = self.roots.get(root);
                    if *graphics {
                        l.SetGraphicsRootSignature(root)
                    } else {
                        l.SetComputeRootSignature(root)
                    }
                }
                Cmd::Table {
                    graphics,
                    param,
                    heap,
                    slot,
                } => {
                    if let Some((versions, step)) = self.descriptors.get(heap)
                        && let Some(v) = versions.get(version)
                    {
                        let at = D3D12_GPU_DESCRIPTOR_HANDLE {
                            ptr: v.gpu + u64::from(*slot) * step,
                        };
                        if *graphics {
                            l.SetGraphicsRootDescriptorTable(*param, at)
                        } else {
                            l.SetComputeRootDescriptorTable(*param, at)
                        }
                    }
                }
                Cmd::Constants {
                    graphics,
                    param,
                    offset,
                    values,
                } => {
                    let (n, data) = (values.len() as u32, values.as_ptr().cast());
                    if *graphics {
                        l.SetGraphicsRoot32BitConstants(*param, n, data, *offset);
                    } else {
                        l.SetComputeRoot32BitConstants(*param, n, data, *offset);
                    }
                }
                Cmd::RootView {
                    graphics,
                    kind,
                    param,
                    va,
                } => {
                    let va = self.va(va);
                    match (*kind, *graphics) {
                        (slot::CBV, true) => l.SetGraphicsRootConstantBufferView(*param, va),
                        (slot::CBV, false) => l.SetComputeRootConstantBufferView(*param, va),
                        (slot::SRV, true) => l.SetGraphicsRootShaderResourceView(*param, va),
                        (slot::SRV, false) => l.SetComputeRootShaderResourceView(*param, va),
                        (_, true) => l.SetGraphicsRootUnorderedAccessView(*param, va),
                        (_, false) => l.SetComputeRootUnorderedAccessView(*param, va),
                    }
                }
                Cmd::IndexBuffer { view } => {
                    let v = view
                        .as_ref()
                        .map(|(va, size, format)| D3D12_INDEX_BUFFER_VIEW {
                            BufferLocation: self.va(va),
                            SizeInBytes: *size,
                            Format: DXGI_FORMAT(*format),
                        });
                    l.IASetIndexBuffer(v.as_ref().map(|v| v as *const _));
                }
                Cmd::VertexBuffers {
                    start,
                    views,
                    count,
                } => match views {
                    Some(views) => {
                        let all: Vec<D3D12_VERTEX_BUFFER_VIEW> = views
                            .iter()
                            .map(|(va, size, stride)| D3D12_VERTEX_BUFFER_VIEW {
                                BufferLocation: self.va(va),
                                SizeInBytes: *size,
                                StrideInBytes: *stride,
                            })
                            .collect();
                        l.IASetVertexBuffers(*start, Some(&all));
                    }
                    None => l.IASetVertexBuffers(
                        *start,
                        Some(&vec![D3D12_VERTEX_BUFFER_VIEW::default(); *count as usize]),
                    ),
                },
                Cmd::Targets { colors, depth } => {
                    let colors: Vec<D3D12_CPU_DESCRIPTOR_HANDLE> =
                        colors.iter().map(view).collect();
                    let depth = depth.as_ref().map(view);
                    l.OMSetRenderTargets(
                        colors.len() as u32,
                        (!colors.is_empty()).then_some(colors.as_ptr()),
                        false,
                        depth.as_ref().map(|d| d as *const _),
                    );
                }
                Cmd::ClearDepth {
                    view: v,
                    flags,
                    depth,
                    stencil,
                    rects,
                } => {
                    let rects: Vec<RECT> = rects.iter().map(|r| to_rect(*r)).collect();
                    let rects = (!rects.is_empty()).then_some(&rects[..]);
                    if self.objects.contains_key(&v.res) {
                        l.ClearDepthStencilView(
                            view(v),
                            D3D12_CLEAR_FLAGS(*flags),
                            *depth,
                            *stencil,
                            rects,
                        );
                    }
                }
                Cmd::ClearTarget {
                    view: v,
                    color,
                    rects,
                } => {
                    let rects: Vec<RECT> = rects.iter().map(|r| to_rect(*r)).collect();
                    let rects = (!rects.is_empty()).then_some(&rects[..]);
                    if self.objects.contains_key(&v.res) {
                        l.ClearRenderTargetView(view(v), color, rects);
                    }
                }
                Cmd::Discard {
                    res: id,
                    rects,
                    subs,
                } => {
                    if let Some(r) = res(*id) {
                        let rects: Vec<RECT> = rects.iter().map(|r| to_rect(*r)).collect();
                        let region = subs.map(|s| D3D12_DISCARD_REGION {
                            NumRects: rects.len() as u32,
                            pRects: if rects.is_empty() {
                                std::ptr::null()
                            } else {
                                rects.as_ptr()
                            },
                            FirstSubresource: s[0],
                            NumSubresources: s[1],
                        });
                        l.DiscardResource(r, region.as_ref().map(|r| r as *const _));
                    }
                }
                Cmd::BeginQuery { heap, kind, index } => {
                    if let Some(h) = self.queries.get(heap) {
                        l.BeginQuery(h, D3D12_QUERY_TYPE(*kind), *index);
                    }
                }
                Cmd::EndQuery { heap, kind, index } => {
                    if let Some(h) = self.queries.get(heap) {
                        l.EndQuery(h, D3D12_QUERY_TYPE(*kind), *index);
                    }
                }
                Cmd::ResolveQuery {
                    heap,
                    kind,
                    first,
                    count,
                    dst,
                    at,
                } => {
                    if let (Some(h), Some(dst)) = (self.queries.get(heap), res(*dst)) {
                        l.ResolveQueryData(h, D3D12_QUERY_TYPE(*kind), *first, *count, dst, *at);
                    }
                }
                Cmd::Indirect {
                    signature,
                    most,
                    args,
                    at,
                    count,
                    count_at,
                } => {
                    if let (Some(s), Some(a)) = (self.signatures.get(signature), res(*args)) {
                        l.ExecuteIndirect(s, *most, a, *at, res(*count), *count_at);
                    }
                }
                Cmd::WriteImmediate { all } => {
                    if let Ok(l) = l.cast::<ID3D12GraphicsCommandList2>() {
                        let all: Vec<&(Va, u32, i32)> =
                            all.iter().filter(|(va, _, _)| self.va(va) != 0).collect();
                        let params: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = all
                            .iter()
                            .map(|(va, v, _)| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
                                Dest: self.va(va),
                                Value: *v,
                            })
                            .collect();
                        let modes: Vec<D3D12_WRITEBUFFERIMMEDIATE_MODE> = all
                            .iter()
                            .map(|(_, _, m)| D3D12_WRITEBUFFERIMMEDIATE_MODE(*m))
                            .collect();
                        if !params.is_empty() {
                            l.WriteBufferImmediate(
                                params.len() as u32,
                                params.as_ptr(),
                                Some(modes.as_ptr()),
                            );
                        }
                    }
                }
            }
        }
    }

    fn barrier(&self, b: &Barrier) -> Option<D3D12_RESOURCE_BARRIER> {
        Some(match b {
            Barrier::Transition {
                flags,
                res,
                sub,
                before,
                after,
            } => {
                let mut t =
                    gpu::transition(self.resource(*res)?, *sub, *before as u32, *after as u32);
                t.Flags = D3D12_RESOURCE_BARRIER_FLAGS(*flags);
                t
            }
            Barrier::Uav { flags, res } => {
                let mut u = uav_barrier(if *res == NONE {
                    None
                } else {
                    Some(self.resource(*res)?)
                });
                u.Flags = D3D12_RESOURCE_BARRIER_FLAGS(*flags);
                u
            }
            Barrier::Alias {
                flags,
                before,
                after,
            } => {
                let side = |id: &u32| -> std::mem::ManuallyDrop<Option<ID3D12Resource>> {
                    // SAFETY: a borrowed pointer (or none) never released.
                    self.resource(*id)
                        .map_or(std::mem::ManuallyDrop::new(None), |r| unsafe {
                            std::mem::transmute_copy(r)
                        })
                };
                D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
                    Flags: D3D12_RESOURCE_BARRIER_FLAGS(*flags),
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                            pResourceBefore: side(before),
                            pResourceAfter: side(after),
                        }),
                    },
                }
            }
        })
    }
}
