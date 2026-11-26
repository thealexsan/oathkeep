//! Shader-visible descriptor heap versions and RTV/DSV view recreation.

use super::*;

impl Player {
    /// Makes a view; one of a resource that is not there is null.
    ///
    /// # Safety
    /// `at` is a slot of a heap of the view's type.
    unsafe fn view(&self, s: &Slot, at: D3D12_CPU_DESCRIPTOR_HANDLE, sampler: bool) {
        let gone = s.res != NONE && !self.objects.contains_key(&s.res);
        let res = self.objects.get(&s.res).map(|o| &o.res);
        let counter = self.objects.get(&s.counter).map(|o| &o.res);
        // The descriptions lie in the slot unaligned.
        // SAFETY: the slot's description is of its kind; the resources live.
        unsafe {
            match (s.kind, s.described != 0 && !gone) {
                (slot::CBV, described) => {
                    let mut d: D3D12_CONSTANT_BUFFER_VIEW_DESC = Default::default();
                    if described && let Some(o) = self.objects.get(&s.res) {
                        d = s
                            .desc
                            .as_ptr()
                            .cast::<D3D12_CONSTANT_BUFFER_VIEW_DESC>()
                            .read_unaligned();
                        d.BufferLocation = o.va + s.address();
                    }
                    self.device.CreateConstantBufferView(Some(&d), at);
                }
                (slot::SRV, true) => {
                    let d = s
                        .desc
                        .as_ptr()
                        .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
                        .read_unaligned();
                    self.device.CreateShaderResourceView(res, Some(&d), at);
                }
                (slot::SRV, false) if res.is_some() => {
                    self.device.CreateShaderResourceView(res, None, at)
                }
                (slot::UAV, true) => {
                    let d = s
                        .desc
                        .as_ptr()
                        .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
                        .read_unaligned();
                    self.device
                        .CreateUnorderedAccessView(res, counter, Some(&d), at);
                }
                (slot::UAV, false) if res.is_some() => {
                    self.device
                        .CreateUnorderedAccessView(res, None::<&ID3D12Resource>, None, at)
                }
                (slot::SAMPLER, true) => {
                    let d = s
                        .desc
                        .as_ptr()
                        .cast::<D3D12_SAMPLER_DESC>()
                        .read_unaligned();
                    self.device.CreateSampler(&d, at);
                }
                _ if sampler => {
                    let d = D3D12_SAMPLER_DESC {
                        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
                        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                        ..Default::default()
                    };
                    self.device.CreateSampler(&d, at);
                }
                (slot::UAV, _) => {
                    let d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT(41),
                        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                        ..Default::default()
                    };
                    self.device.CreateUnorderedAccessView(
                        None::<&ID3D12Resource>,
                        None::<&ID3D12Resource>,
                        Some(&d),
                        at,
                    );
                }
                _ => {
                    let d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT(41),
                        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        ..Default::default()
                    };
                    self.device
                        .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&d), at);
                }
            }
        }
    }

    pub(super) fn make_descriptors(&mut self) -> Result<()> {
        // A submission whose views replace ones that were in use gets a
        // version of the heaps of its own: lists are sent without waiting,
        // so a heap cannot be written between them.
        let mut now: HashMap<(u32, u32), Slot> = HashMap::new();
        let replaced: HashSet<(u32, u32)> = self
            .frame
            .submits
            .iter()
            .flat_map(|s| s.views.iter().map(|v| (v.0, v.1)))
            .collect();
        for h in &self.frame.descriptor_heaps {
            for (id, i) in replaced.iter().filter(|k| k.0 == h.id) {
                let k = h.runs.partition_point(|run| run.0 <= *i);
                if let Some((first, n, s)) = k.checked_sub(1).and_then(|k| h.runs.get(k))
                    && i - first < *n
                {
                    now.insert((*id, *i), *s);
                }
            }
        }
        // What each version changes against the one before.
        let mut versions: Vec<Vec<(u32, u32, Slot)>> = vec![Vec::new()];
        let mut touched: HashSet<(u32, u32)> = HashSet::new();
        for s in &self.frame.submits {
            let live =
                |old: Option<&Slot>| old.is_some_and(|o| o.kind == slot::SAMPLER || o.res != NONE);
            let news: Vec<&(u32, u32, Slot)> = s
                .views
                .iter()
                .filter(|(h, i, v)| now.get(&(*h, *i)) != Some(v))
                .collect();
            if news
                .iter()
                .any(|(h, i, _)| live(now.get(&(*h, *i))) || touched.contains(&(*h, *i)))
            {
                versions.push(Vec::new());
                touched.clear();
            }
            for (h, i, v) in news {
                now.insert((*h, *i), *v);
                touched.insert((*h, *i));
                if let Some(last) = versions.last_mut() {
                    last.push((*h, *i, *v));
                }
            }
            self.version_of.push(versions.len() - 1);
        }
        let mut made = HashMap::new();
        let mut full = 0;
        for h in &self.frame.descriptor_heaps {
            let sampler = h.kind == D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER.0;
            let kind = D3D12_DESCRIPTOR_HEAP_TYPE(h.kind);
            let make = |flags| -> windows::core::Result<ID3D12DescriptorHeap> {
                let d = D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: h.count,
                    Flags: flags,
                    NodeMask: 0,
                };
                // SAFETY: plain creation.
                unsafe { self.device.CreateDescriptorHeap(&d) }
            };
            let cpu = make(D3D12_DESCRIPTOR_HEAP_FLAG_NONE)?;
            // SAFETY: slots of the heap just made; threads write other ones.
            unsafe {
                let step = self.device.GetDescriptorHandleIncrementSize(kind) as usize;
                let start = cpu.GetCPUDescriptorHandleForHeapStart();
                let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start.ptr + i as usize * step,
                };
                self.view(&Slot::EMPTY, start, sampler);
                let mut filled = 1;
                while filled < h.count {
                    let n = filled.min(h.count - filled).min(1 << 16);
                    // Sources and destinations do not overlap.
                    self.device
                        .CopyDescriptorsSimple(n, at(filled), start, kind);
                    filled += n;
                }
                for (first, n, s) in &h.runs {
                    self.view(s, at(*first), sampler);
                    for i in 1..*n {
                        self.device
                            .CopyDescriptorsSimple(1, at(first + i), at(*first), kind);
                    }
                }
                let mut all = Vec::new();
                for changes in &versions {
                    let mine: Vec<&(u32, u32, Slot)> =
                        changes.iter().filter(|(id, _, _)| *id == h.id).collect();
                    for (_, i, s) in &mine {
                        if *i < h.count {
                            self.view(s, at(*i), sampler);
                        }
                    }
                    if mine.is_empty()
                        && let Some(Version { heap, gpu }) = all.last()
                    {
                        let same = Version {
                            heap: ID3D12DescriptorHeap::clone(heap),
                            gpu: *gpu,
                        };
                        all.push(same);
                        continue;
                    }
                    let heap = match make(D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE) {
                        Ok(heap) => heap,
                        // Room for no more of them: the changes go into the
                        // last one, and lists before see them too.
                        Err(_) if !all.is_empty() => {
                            full += 1;
                            let Some(Version { heap, gpu }) = all.last() else {
                                break;
                            };
                            let same = Version {
                                heap: ID3D12DescriptorHeap::clone(heap),
                                gpu: *gpu,
                            };
                            self.device.CopyDescriptorsSimple(
                                h.count,
                                same.heap.GetCPUDescriptorHandleForHeapStart(),
                                start,
                                kind,
                            );
                            all.push(same);
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    self.device.CopyDescriptorsSimple(
                        h.count,
                        heap.GetCPUDescriptorHandleForHeapStart(),
                        start,
                        kind,
                    );
                    let gpu = heap.GetGPUDescriptorHandleForHeapStart().ptr;
                    all.push(Version { heap, gpu });
                }
                made.insert(h.id, (all, step as u64));
            }
        }
        self.descriptors = made;
        if versions.len() > 1 {
            self.notes.push(format!(
                "{} versions of the descriptor heaps (views replaced during the frame)",
                versions.len()
            ));
        }
        if full > 0 {
            self.note(format!(
                "{full} versions of descriptor heaps had no room: earlier lists see later views"
            ));
        }
        // Render target and depth views.
        let mut views: [HashSet<Slot>; 2] = Default::default();
        for c in self.commands.iter().flatten() {
            match c {
                Cmd::Targets { colors, depth } => {
                    views[0].extend(colors);
                    views[1].extend(depth);
                }
                Cmd::ClearTarget { view, .. } => _ = views[0].insert(*view),
                Cmd::ClearDepth { view, .. } => _ = views[1].insert(*view),
                _ => {}
            }
        }
        for (i, all) in views.iter().enumerate() {
            if all.is_empty() {
                continue;
            }
            let kind = [
                D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
            ][i];
            let d = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: all.len() as u32,
                ..Default::default()
            };
            // SAFETY: plain creation; slots of the heap just made.
            unsafe {
                let heap: ID3D12DescriptorHeap = self.device.CreateDescriptorHeap(&d)?;
                let step = self.device.GetDescriptorHandleIncrementSize(kind) as usize;
                let mut at = heap.GetCPUDescriptorHandleForHeapStart();
                for s in all {
                    let res = self.objects.get(&s.res).map(|o| &o.res);
                    if i == 0 {
                        let d = s
                            .desc
                            .as_ptr()
                            .cast::<D3D12_RENDER_TARGET_VIEW_DESC>()
                            .read_unaligned();
                        // Without either there is nothing to make a view of.
                        let null = D3D12_RENDER_TARGET_VIEW_DESC {
                            Format: DXGI_FORMAT(28),
                            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
                            ..Default::default()
                        };
                        let d = if s.described != 0 {
                            Some(&d as *const _)
                        } else if res.is_none() {
                            Some(&null as *const _)
                        } else {
                            None
                        };
                        self.device.CreateRenderTargetView(res, d, at);
                    } else {
                        let d = s
                            .desc
                            .as_ptr()
                            .cast::<D3D12_DEPTH_STENCIL_VIEW_DESC>()
                            .read_unaligned();
                        let null = D3D12_DEPTH_STENCIL_VIEW_DESC {
                            Format: DXGI_FORMAT(40),
                            ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                            ..Default::default()
                        };
                        let d = if s.described != 0 {
                            Some(&d as *const _)
                        } else if res.is_none() {
                            Some(&null as *const _)
                        } else {
                            None
                        };
                        self.device.CreateDepthStencilView(res, d, at);
                    }
                    self.targets.insert(*s, at);
                    at.ptr += step;
                }
                self._target_heaps.push(heap);
            }
        }
        Ok(())
    }
}
