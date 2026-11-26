//! Pipeline, descriptor heap, vertex/index buffer, and stage binding.

use oathkeep_shader::RootType;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use crate::backend::HostList;
use crate::d3d12::object::This;
use crate::d3d12::pipeline::host::HostPipeline;

use super::readable::BakingGuard;
use super::root_views::root_view;
use super::state::{Root, State};
use super::{DEPTH_READ, DEPTH_WRITE, RECTANGLES, SHADER_RESOURCE, STORES, TESTED_DEPTH, write_skip};

impl State {
    /// Brings the host list up to date for a draw; false if the draw cannot
    /// run on the host.
    pub(super) fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    /// As [`State::prepare`], for draws (`mesh` false) or mesh dispatches.
    pub(super) fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::d3d12::pipeline::is_skipped(self.pipeline as This) {
            return false;
        }
        let Some(p) = crate::d3d12::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        if p.compute || p.mesh != mesh {
            once!(mesh, "a draw or mesh dispatch with another kind of pipeline");
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            // Its geometry stage would turn each triangle into a rectangle.
            once!("a rectangle pipeline drawn with another topology");
        }
        if self.depth != 0 {
            // Sync any pending depth-plane twin into the depth texture before
            // `bind` so an inline plane pass cannot clobber the draw's heaps,
            // root signature, pipeline, render targets, viewports, or scissors.
            list.twin_use(self.depth as This, false, false);
        }
        if !self.bind(list, p, false) {
            return false;
        }
        let l = &list.list;
        // Vertex buffers the input layout reads.
        let mut views = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        let mut count = 0;
        for (slot, (view, &v)) in views.iter_mut().zip(&self.vertex).enumerate() {
            if p.slots & 1 << slot == 0 {
                continue;
            }
            count = slot + 1;
            if v.address == 0 || v.size == 0 {
                continue;
            }
            let Some(shadow) = crate::memory::shadow::find_any(v.address, u64::from(v.size)) else {
                continue;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            *view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size.min(u32::try_from(shadow.left(v.address)).unwrap_or(u32::MAX)),
                StrideInBytes: v.stride,
            };
        }
        // SAFETY: an open list; the views name live host buffers.
        unsafe {
            if count > 0 {
                l.IASetVertexBuffers(0, Some(&views[..count]));
            }
        }
        if indexed {
            let v = self.index;
            let Some(shadow) = crate::memory::shadow::find_any(v.address, u64::from(v.size)) else {
                return false;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            let view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size.min(u32::try_from(shadow.left(v.address)).unwrap_or(u32::MAX)),
                Format: DXGI_FORMAT(v.format as i32),
            };
            // SAFETY: an open list; the view names a live host buffer.
            unsafe { l.IASetIndexBuffer(Some(&view)) };
        }
        const RENDER_TARGET: u32 = 0x4;
        for &t in &self.targets {
            crate::d3d12::command_list::ensure_state(list, t as This, RENDER_TARGET);
        }
        if self.depth != 0 {
            // Mark the depth target written only when the DSV can write depth
            // or stencil; read-only depth tests leave the colour twin in sync.
            list.twin_use(self.depth as This, false, self.depth_state != DEPTH_READ);
            if list.take_state_clobbered() {
                self.replay(list);
                if !self.bind(list, p, false) {
                    return false;
                }
            }
            // A view that writes neither plane keeps the texture readable:
            // the depth read state, together with the shader read states
            // if this draw's shaders read the same texture (the write state
            // put it out of the state those views need, and a move to it
            // and back for every such draw).
            let state = if self.depth_state == DEPTH_READ {
                let (tested, read) = TESTED_DEPTH.get();
                if read && tested == self.depth {
                    DEPTH_READ | SHADER_RESOURCE
                } else {
                    DEPTH_READ
                }
            } else {
                DEPTH_WRITE
            };
            crate::d3d12::command_list::ensure_state(list, self.depth as This, state);
        }
        true
    }

    /// A descriptor a shader reads through a table: its host slot made if
    /// it changed, constant data copied, textures made readable.
    /// What draws and dispatches share: heaps, our root signature, the
    /// pipeline, and per stage the user data, root views and the
    /// descriptors its tables hold.
    /// False if the draw or dispatch must not run: a root SRV it reads
    /// could not be given a view.
    pub(crate) fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        let l = &list.list;
        // Textures picked by index, before anything asks for them.
        self.early_textures(list, p, compute);
        if crate::backend::walls_trace() {
            self.walls_census(p, compute);
        }
        // A list without a view heap: one of the title's for the views the
        // host itself puts into heap slots.
        if self.heaps[0] == 0
            && p.stages.iter().any(|s| !s.root_uavs.is_empty() || !s.root_srvs.is_empty())
            && let Some(heap) = crate::descriptors::lent_heap()
        {
            once!("a dispatch without a view heap that needs one gets the title's");
            self.heaps[0] = heap;
        }
        // Heaps first: a root signature with directly indexed heaps must be
        // set after them, with both a view heap and a sampler heap bound.
        // An inline depth-plane pass or depth-to-plane copy-back during table
        // preparation clobbers the list's heaps, root signatures, pipeline,
        // and graphics state; replay and re-bind cleanly when that happens.
        'bind_pass: for attempt in 0..=p.stages.len() {
            if list.take_state_clobbered() {
                self.replay(list);
            }
            if !self.host_heaps_set || self.host_heaps != self.heaps {
                let mut view_heap: Option<ID3D12DescriptorHeap> = None;
                let mut sampler_heap: Option<ID3D12DescriptorHeap> = None;
                for &h in &self.heaps {
                    if h == 0 {
                        continue;
                    }
                    let title_heap = crate::descriptors::heap(h as This);
                    if let Some(host_heap) = title_heap.host() {
                        if title_heap.samplers() {
                            sampler_heap = Some(host_heap.heap.clone());
                        } else {
                            view_heap = Some(host_heap.heap.clone());
                        }
                    }
                }
                if view_heap.is_none() {
                    view_heap = crate::backend::host().and_then(|h| h.fallback_view_heap().cloned());
                }
                if sampler_heap.is_none() {
                    sampler_heap = crate::backend::host().and_then(|h| h.fallback_sampler_heap().cloned());
                }
                let mut heaps: Vec<Option<ID3D12DescriptorHeap>> = Vec::with_capacity(2);
                if let Some(h) = view_heap {
                    list.keep(&h);
                    heaps.push(Some(h));
                }
                if let Some(h) = sampler_heap {
                    list.keep(&h);
                    heaps.push(Some(h));
                }
                if !heaps.is_empty() {
                    // SAFETY: an open list and shader-visible heaps.
                    unsafe { l.SetDescriptorHeaps(&heaps) };
                }
                self.host_heaps = self.heaps;
                self.host_heaps_set = true;
                self.host_roots = [0; 2];
            }
            // The depth target a draw only tests, for the views of it that the
            // draw's shaders read (`readable`).
            let tested = if !compute && self.depth_state == DEPTH_READ {
                self.depth
            } else {
                0
            };
            TESTED_DEPTH.set((tested, false));
            let root = p.root.as_raw() as usize;
            if self.host_roots[usize::from(compute)] != root {
                // SAFETY: an open list and a live root signature.
                unsafe {
                    if compute {
                        l.SetComputeRootSignature(&p.root);
                    } else {
                        l.SetGraphicsRootSignature(&p.root);
                    }
                }
                self.host_roots[usize::from(compute)] = root;
            }
            // By host object: a title pipeline released and another made at its
            // address is another host pipeline (the list keeps the old one).
            if self.host_pipeline != p.pso.as_raw() as usize {
                // SAFETY: an open list and a live pipeline.
                unsafe { l.SetPipelineState(&p.pso) };
                self.host_pipeline = p.pso.as_raw() as usize;
                // The title may release the pipeline before it submits the list.
                list.bind_pipeline(&p.name, &p.pso, &p.root);
            }
            for stage in &p.stages {
                let t = stage.translated();
                STORES.set(t.stores);
                if t.stores && crate::diagnostics::picked_writes::on() && crate::diagnostics::picked_writes::picks(&stage.shader, &t.picks) {
                    crate::diagnostics::picked_writes::runs_in(list);
                }
                // `depth_pyramid_cpu_occlusion` (`42683eebec081520`) samples the
                // CPU software-rasterized 128x72 R32_FLOAT occlusion pyramid to
                // seed GPU Hi-Z culling (`meshlet_args` / mesh draws). Force
                // same-frame CopyTextureRegion recording so camera cuts never
                // cull interior walls or ceilings against a 1-frame-stale CPU
                // occlusion pyramid.
                let _cpu_occ_guard =
                    (compute && stage.shader.hash == "42683eebec081520").then(BakingGuard::enter);
                let made = self.tables(list, &t.tables, compute);
                drop(_cpu_occ_guard);
                STORES.set(true);
                if made.iter().any(|m| m.1 == u32::MAX) {
                    once!("a draw whose descriptor table found no free slots is dropped");
                    return false;
                }
                if attempt < p.stages.len() && list.take_state_clobbered() {
                    self.replay(list);
                    continue 'bind_pass;
                }
                if stage.shader.ambient.is_some() {
                    self.ambient_tables.clear();
                    self.ambient_tables.extend(made.iter().copied());
                }
                // What the shader stores through root views and raw addresses is
                // copied back into title memory afterwards.
                let writes = (t.stores || !write_skip())
                    && !crate::d3d12::pipeline::builder_quarantined(self.pipeline as This);
                // Title memory by address: what the shader finds it by.
                let memory = if t.host.root_srvs.contains(&oathkeep_shader::emit::SLOT_MEMORY) {
                    // Only through a store by address: what it stores through
                    // views is declared with them.
                    let use_ = if crate::d3d12::pipeline::is_builder(self.pipeline as This)
                        || writes && t.stores_by_address
                    {
                        crate::memory::allocator::Use::Builds { back: writes }
                    } else {
                        crate::memory::allocator::Use::Reads
                    };
                    let Some(ready) = self.memory_ready(list, &t, compute, use_) else {
                        once!("a shader that reads title memory by address without a view heap to hold its views: the draw is dropped");
                        return false;
                    };
                    Some(ready)
                } else {
                    None
                };
                if let Some((param, dwords)) = stage.user_data {
                    // (A root signature holds 64 dwords.)
                    let mut room = [0u32; 64];
                    let mut more = Vec::new();
                    let values: &mut [u32] = match room.get_mut(..dwords as usize) {
                        Some(values) => values,
                        None => {
                            more.resize(dwords as usize, 0);
                            &mut more
                        }
                    };
                    for u in &t.host.user_data {
                        if let RootType::Other(kind) = u.root {
                            values[u.index as usize] =
                                self.host_word(compute, kind, u.slot, u.dword, memory.as_ref());
                            continue;
                        }
                        let fresh = made
                            .iter()
                            .find(|m| m.0 == u.slot && u.root == RootType::Table);
                        values[u.index as usize] = match fresh {
                            Some(&(_, offset)) => offset,
                            None => self.user_value(compute, u.slot, u.root, u.dword),
                        };
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        self.user.clear();
                        self.user.extend_from_slice(values);
                    }
                    // SAFETY: an open list; `values` covers the parameter.
                    unsafe {
                        if compute {
                            l.SetComputeRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                        } else {
                            l.SetGraphicsRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                        }
                    }
                }
                for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                    for &(slot, param) in views {
                        if srv && slot == oathkeep_shader::emit::SLOT_MEMORY {
                            // Ours: the directory of title memory by address.
                            if let Some(m) = &memory {
                                // SAFETY: an open list; the directory lives as
                                // long as the process and the list holds it.
                                unsafe {
                                    if compute {
                                        l.SetComputeRootShaderResourceView(param, m.directory);
                                    } else {
                                        l.SetGraphicsRootShaderResourceView(param, m.directory);
                                    }
                                }
                            }
                            continue;
                        }
                        if srv && slot == oathkeep_shader::emit::SLOT_STRIDES {
                            // Ours, not the title's: the shader picks buffers
                            // from the view heap by an index it computes.
                            match self.heap_buffers(list) {
                                // SAFETY: an open list; the table lives as long
                                // as the heap the list holds.
                                Some(gpu) => unsafe {
                                    if compute {
                                        l.SetComputeRootShaderResourceView(param, gpu);
                                    } else {
                                        l.SetGraphicsRootShaderResourceView(param, gpu);
                                    }
                                },
                                None => once!("a shader picks buffers from a view heap that is not set"),
                            }
                            continue;
                        }
                        if srv && slot == oathkeep_shader::emit::SLOT_TABLE_STRIDES {
                            // Ours: the shader inspects typed view kinds for its
                            // descriptor tables only (already mirrored by `tables`),
                            // so bind the strides buffer without sweeping the heap.
                            match self.heap_strides(list) {
                                // SAFETY: an open list; the table lives as long
                                // as the heap the list holds.
                                Some(gpu) => unsafe {
                                    if compute {
                                        l.SetComputeRootShaderResourceView(param, gpu);
                                    } else {
                                        l.SetGraphicsRootShaderResourceView(param, gpu);
                                    }
                                },
                                None => once!("a shader inspects table view kinds without a view heap set"),
                            }
                            continue;
                        }
                        let tabled = srv && crate::d3d12::pipeline::host::bounded_root_srvs();
                        let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                        else {
                            once!(slot, "a shader reads a root view the title did not set");
                            // A table left unset is undefined: a null view.
                            if tabled && !self.root_srv_table(list, None, param, compute, false) {
                                return false;
                            }
                            continue;
                        };
                        // The shader adds the bytes a raw view begins early by.
                        let biased = srv
                            && t.host.user_data.iter().any(|u| {
                                u.slot == slot
                                    && u.root == RootType::Other(oathkeep_shader::emit::USER_VIEW_BIAS)
                            });
                        if tabled {
                            if !self.root_srv_table(list, Some(address), param, compute, biased) {
                                return false;
                            }
                            continue;
                        }
                        let Some(mut gpu) = root_view(list, address, srv) else {
                            continue;
                        };
                        if biased {
                            gpu -= u64::from(self.host_word(
                                compute,
                                oathkeep_shader::emit::USER_VIEW_BIAS,
                                slot,
                                0,
                                None,
                            ));
                        }
                        // SAFETY: an open list; the address is in live memory.
                        unsafe {
                            match (compute, srv) {
                                (false, false) => l.SetGraphicsRootConstantBufferView(param, gpu),
                                (false, true) => l.SetGraphicsRootShaderResourceView(param, gpu),
                                (true, false) => l.SetComputeRootConstantBufferView(param, gpu),
                                (true, true) => l.SetComputeRootShaderResourceView(param, gpu),
                            }
                        }
                    }
                }
                for &(slot, param) in &stage.root_uavs {
                    if slot == oathkeep_shader::emit::SLOT_FAULTS {
                        // Ours: where shaders note the pages they found empty.
                        if let Some(m) = &memory {
                            // SAFETY: an open list; the buffer lives as long as
                            // the process and the list holds it.
                            unsafe {
                                if compute {
                                    l.SetComputeRootUnorderedAccessView(param, m.faults);
                                } else {
                                    l.SetGraphicsRootUnorderedAccessView(param, m.faults);
                                }
                            }
                        }
                        continue;
                    }
                    let address = match self.roots_ref(compute).get(usize::from(slot)) {
                        Some(&Root::View(address)) => Some(address),
                        _ => {
                            once!(slot, "a shader writes through a root view the title did not set");
                            None
                        }
                    };
                    if !self.root_uav(list, address, param, compute, writes) {
                        return false;
                    }
                }
            }
            break;
        }
        // Depth textures it writes through unordered access: their colour
        // twins hold what they lack from here on.
        list.twins_written();
        true
    }
}
