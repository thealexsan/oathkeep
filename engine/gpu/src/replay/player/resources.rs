//! Heap and resource recreation plus adapter memory queries.

use super::*;

impl Player {
    pub(super) fn make_resources(&mut self, buffers: Buffers, only: &[String]) -> Result<()> {
        // A heap of memory the CPU sees becomes one the card may write too:
        // what the CPU wrote is put back by the card.
        let props = |p: &wire::HeapProps, buffer: bool| -> D3D12_HEAP_PROPERTIES {
            let pc = |page| D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: page,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            };
            let card = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            match (gpu::seen(p), buffers) {
                (Seen::ReadBack, _) => gpu::to_props(p),
                (_, Buffers::Card) if buffer => card,
                (Seen::No, Buffers::Pc) if buffer => pc(D3D12_CPU_PAGE_PROPERTY_WRITE_BACK),
                (Seen::Combined, _) if p.kind > D3D12_HEAP_TYPE_CUSTOM.0 => card,
                (Seen::Combined, _) => pc(D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE),
                _ => gpu::to_props(p),
            }
        };
        // Flags a replay has no use for: sharing, display, not zeroed.
        const DROP: i32 = 0x1 | 0x8 | 0x20 | 0x1000;
        let used_by_buffers_only: HashSet<u32> = {
            let mut textures = HashSet::new();
            for r in &self.frame.resources {
                if let Place::Placed { heap, .. } = r.place
                    && r.desc.dimension != D3D12_RESOURCE_DIMENSION_BUFFER.0
                {
                    textures.insert(heap);
                }
            }
            self.frame
                .heaps
                .iter()
                .map(|h| h.id)
                .filter(|id| !textures.contains(id))
                .collect()
        };
        for h in &self.frame.heaps {
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: h.size,
                Properties: props(
                    &h.props,
                    only.is_empty() && used_by_buffers_only.contains(&h.id),
                ),
                Alignment: h.alignment,
                Flags: D3D12_HEAP_FLAGS(h.flags & !DROP),
            };
            let mut heap: Option<ID3D12Heap> = None;
            // SAFETY: plain creation.
            unsafe { self.device.CreateHeap(&desc, &mut heap) }
                .with_context(|| format!("heap {}", h.id))?;
            self._heaps.extend(heap.map(|heap| (h.id, heap)));
        }
        let mut failed = 0;
        let mut moves: BTreeMap<String, (u32, u64)> = BTreeMap::new();
        for (index, r) in self.frame.resources.iter().enumerate() {
            let is_buffer = r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0;
            let moved = is_buffer && chosen(r, only);
            if moved && let Place::Committed { props, .. } = &r.place {
                let seen = gpu::seen(props);
                let m = moves.entry(format!("{seen:?}")).or_insert((0u32, 0u64));
                *m = (m.0 + 1, m.1 + r.desc.width);
            }
            let desc = gpu::to_desc(&r.desc);
            let mut made: Option<ID3D12Resource> = None;
            let clear: Option<D3D12_CLEAR_VALUE> = r.clear.map(|c| {
                // SAFETY: 20 bytes that were a clear value.
                unsafe { std::mem::transmute::<[u32; 5], D3D12_CLEAR_VALUE>(c) }
            });
            let clear = clear.as_ref().map(|c| c as *const _);
            // SAFETY: plain creation from a description the host took.
            let result = unsafe {
                match &r.place {
                    Place::Placed { heap, offset } => match self._heaps.get(heap) {
                        Some(h) => self.device.CreatePlacedResource(
                            h,
                            *offset,
                            &desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            clear,
                            &mut made,
                        ),
                        None => Err(windows::core::Error::empty()),
                    },
                    Place::Committed { props: p, flags } => {
                        let p = props(p, moved);
                        let state = if p.Type == D3D12_HEAP_TYPE_READBACK {
                            D3D12_RESOURCE_STATE_COPY_DEST
                        } else {
                            D3D12_RESOURCE_STATE_COMMON
                        };
                        self.device.CreateCommittedResource(
                            &p,
                            D3D12_HEAP_FLAGS(flags & !DROP),
                            &desc,
                            state,
                            clear,
                            &mut made,
                        )
                    }
                    Place::BackBuffer {} => {
                        let p = D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        };
                        self.device.CreateCommittedResource(
                            &p,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COMMON,
                            None,
                            &mut made,
                        )
                    }
                }
            };
            match (result, made) {
                (Ok(()), Some(res)) => {
                    // SAFETY: a live resource.
                    let va = if is_buffer {
                        unsafe { res.GetGPUVirtualAddress() }
                    } else {
                        0
                    };
                    self.objects.insert(
                        r.id,
                        Object {
                            res,
                            va,
                            index,
                            subs: gpu::subresources(&r.desc),
                        },
                    );
                }
                (e, _) => {
                    if failed == 0 {
                        tracing::warn!("resource {} not made: {e:?}: {r:?}", r.id);
                    }
                    failed += 1;
                }
            }
            if !is_buffer {
                self.start.insert(r.id, r.state.clone());
            }
        }
        if failed > 0 {
            self.note(format!("{failed} resources could not be made again"));
        }
        if buffers != Buffers::AsRecorded {
            self.notes.push(format!(
                "--buffers is of, by how the CPU saw them (buffers, bytes): {moves:?}"
            ));
        }
        Ok(())
    }

    /// The card's memory budget and use, and the use of the PC's memory, in
    /// MB.
    pub(super) fn memory(&self) -> Option<[u64; 3]> {
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut pc = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter3 = factory
                .EnumAdapterByLuid(self.device.GetAdapterLuid())
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut pc)
                .ok()?;
        }
        Some([
            own.Budget >> 20,
            own.CurrentUsage >> 20,
            pc.CurrentUsage >> 20,
        ])
    }

    /// What is free of the card's memory budget.
    pub(super) fn room(&self) -> Option<u64> {
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter3 = factory
                .EnumAdapterByLuid(self.device.GetAdapterLuid())
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
        }
        Some(own.Budget.saturating_sub(own.CurrentUsage))
    }
}
