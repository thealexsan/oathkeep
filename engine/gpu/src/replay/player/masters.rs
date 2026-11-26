//! Written-resource master snapshots and between-loop restoration.

use super::*;

impl Player {
    /// Copies of what the frame changes; and the states it leaves.
    pub(super) fn make_masters(&mut self, buffers: Buffers, wanted: bool) -> Result<()> {
        let mut states = States::default();
        for r in &self.frame.resources {
            if r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0 {
                continue;
            }
            let subs = gpu::subresources(&r.desc);
            let decays = r.desc.flags & D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS.0 != 0;
            states.add(r.id, subs, r.state[0], decays);
            if r.state.len() > 1 {
                for (i, s) in r.state.iter().enumerate() {
                    states.change(&states::Change {
                        res: r.id,
                        sub: i as u32,
                        after: *s,
                        promoted: false,
                        alias: false,
                    });
                }
            }
        }
        // Resources in a state they are written in at some time, or written
        // some other way.
        const WRITE: u32 =
            RENDER_TARGET | UNORDERED_ACCESS | DEPTH_WRITE | COPY_DEST | RESOLVE_DEST;
        let mut written: HashSet<u32> = self
            .frame
            .resources
            .iter()
            .filter(|r| r.state.iter().any(|s| s & WRITE != 0))
            .map(|r| r.id)
            .collect();
        let mut by_cpu: HashSet<u32> = HashSet::new();
        for (i, s) in self.frame.submits.iter().enumerate() {
            for &l in s.lists.iter().filter(|&&l| l != MISSING) {
                for c in &self.commands[l as usize] {
                    let mut changes = Vec::new();
                    states::changes(c, &mut changes);
                    for ch in &changes {
                        states.change(ch);
                    }
                    match c {
                        Cmd::Barriers { all } => {
                            for b in all {
                                if let Barrier::Transition {
                                    res, before, after, ..
                                } = b
                                    && (*before | *after) as u32 & WRITE != 0
                                {
                                    written.insert(*res);
                                }
                            }
                        }
                        Cmd::CopyBuffer { dst, .. }
                        | Cmd::CopyResource { dst, .. }
                        | Cmd::ResolveQuery { dst, .. } => _ = written.insert(*dst),
                        Cmd::CopyTexture {
                            dst: Loc::Sub { res, .. } | Loc::Placed { res, .. },
                            ..
                        } => _ = written.insert(*res),
                        Cmd::Resolve { dst, .. } | Cmd::ResolveRegion { dst, .. } => {
                            _ = written.insert(*dst)
                        }
                        Cmd::RootView {
                            kind: slot::UAV,
                            va: Va::At { res, .. },
                            ..
                        } => _ = written.insert(*res),
                        Cmd::WriteImmediate { all } => {
                            written.extend(all.iter().filter_map(|(va, _, _)| {
                                if let Va::At { res, .. } = va {
                                    Some(*res)
                                } else {
                                    None
                                }
                            }));
                        }
                        _ => {}
                    }
                }
            }
            states.decay();
            if i > 0 {
                by_cpu.extend(self.delta_parts[i].iter().map(|(d, _)| d.res));
            }
        }
        // Buffers any unordered-access view names.
        let is_buffer = |id: u32| {
            self.objects.get(&id).is_some_and(|o| {
                self.frame.resources[o.index].desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0
            })
        };
        let simultaneous = |id: u32| {
            self.objects.get(&id).is_some_and(|o| {
                self.frame.resources[o.index].desc.flags
                    & D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS.0
                    != 0
            })
        };
        let views = self
            .frame
            .descriptor_heaps
            .iter()
            .flat_map(|h| h.runs.iter().map(|(_, _, s)| s));
        let late = self
            .frame
            .submits
            .iter()
            .flat_map(|s| s.views.iter().map(|(_, _, s)| s));
        for s in views.chain(late).filter(|s| s.kind == slot::UAV) {
            for id in [s.res, s.counter] {
                if is_buffer(id) || simultaneous(id) {
                    written.insert(id);
                }
            }
        }
        self.card_writes = written.clone();
        written.extend(by_cpu);
        for id in self.start.keys() {
            self.end
                .insert(*id, states.get(*id).unwrap_or(vec![COMMON]));
        }
        let mut ids: Vec<u32> = written
            .into_iter()
            .filter(|id| {
                self.objects.get(id).is_some_and(|o| {
                    let r = &self.frame.resources[o.index];
                    let back = matches!(&r.place, Place::Committed { props, .. } if gpu::seen(props) == Seen::ReadBack);
                    !r.idle && !back && !matches!(r.place, Place::BackBuffer {})
                })
            })
            .collect();
        ids.sort_unstable();
        // Without restoring they are of no use, and their room may be what
        // the frame itself lacks.
        if !wanted {
            ids.clear();
        }
        let mut total = 0u64;
        let mut in_pc = 0u64;
        for id in ids {
            let o = &self.objects[&id];
            let r = &self.frame.resources[o.index];
            let mut desc = gpu::to_desc(&r.desc);
            desc.Alignment = 0;
            // Copies are only copied to and from; a depth format needs its
            // flag.
            if r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0 {
                desc.Flags = D3D12_RESOURCE_FLAG_NONE;
            }
            // SAFETY: a plain query.
            let size = unsafe { self.device.GetResourceAllocationInfo(0, &[desc]) }.SizeInBytes;
            let room = self.room();
            let buffer = r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0;
            // With all buffers in the card's memory there is no room for
            // their copies as well.
            let pc = buffer
                && (buffers == Buffers::Card || room.is_some_and(|room| room < size + (1 << 30)));
            let props = if pc {
                D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_CUSTOM,
                    CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE,
                    MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                    ..Default::default()
                }
            } else {
                D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                }
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            let result = unsafe {
                self.device.CreateCommittedResource(
                    &props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut made,
                )
            };
            match (result, made) {
                (Ok(()), Some(m)) => {
                    total += size;
                    in_pc += if pc { size } else { 0 };
                    self.masters.push((id, m));
                }
                (e, _) => self.note(format!(
                    "no copy of resource {id} to restore it from ({e:?})"
                )),
            }
        }
        tracing::info!(
            resources = self.masters.len(),
            mb = total >> 20,
            in_pc_memory_mb = in_pc >> 20,
            "copies of what the frame changes"
        );
        // Filled now: the resources are as at the frame's start. Masters
        // stay in the copy-source state (buffers decay).
        let (masters, objects, start) = (&self.masters, &self.objects, &self.start);
        for some in masters.chunks(256) {
            let (mut before, mut after) = (Vec::new(), Vec::new());
            for (id, m) in some {
                let Some(state) = start.get(id) else { continue };
                let o = &objects[id];
                let to: Vec<u32> = state
                    .iter()
                    .map(|&s| if s & COPY_SOURCE != 0 { s } else { COPY_SOURCE })
                    .collect();
                gpu::moves(&o.res, state, &to, &mut before);
                gpu::moves(&o.res, &to, state, &mut after);
                before.push(gpu::transition(m, ALL, COMMON, COPY_DEST));
                after.push(gpu::transition(m, ALL, COPY_DEST, COPY_SOURCE));
            }
            self.worker.run(|list| {
                // SAFETY: an open list; live resources of one size.
                unsafe {
                    barriers(list, &before);
                    for (id, m) in some {
                        list.CopyResource(m, &objects[id].res);
                    }
                    barriers(list, &after);
                }
            })?;
        }
        Ok(())
    }

    /// Puts back what a run changed.
    pub(super) fn restore(&mut self, contents: bool) -> Result<()> {
        if self.fresh {
            return Ok(());
        }
        let restored: HashMap<u32, &ID3D12Resource> = if contents {
            self.masters.iter().map(|(id, m)| (*id, m)).collect()
        } else {
            HashMap::new()
        };
        let (mut idle, mut into, mut back, mut plain) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (id, start) in &self.start {
            let Some(o) = self.objects.get(id) else {
                continue;
            };
            let end = self.end.get(id).unwrap_or(start);
            if restored.contains_key(id) {
                gpu::moves(&o.res, end, &[COPY_DEST], &mut into);
                gpu::moves(&o.res, &[COPY_DEST], start, &mut back);
            } else if self.frame.resources[o.index].idle {
                gpu::moves(&o.res, end, start, &mut idle);
            } else {
                gpu::moves(&o.res, end, start, &mut plain);
            }
        }
        let objects = &self.objects;
        self.worker.run(|list| {
            // SAFETY: an open list; live resources, each copied from one of
            // its own description.
            unsafe {
                barriers(list, &idle);
                barriers(list, &[alias()]);
                barriers(list, &plain);
                barriers(list, &into);
                for (id, m) in &restored {
                    list.CopyResource(&objects[id].res, *m);
                }
                if let Some(merger) = &self.merger {
                    for (id, base) in &merger.bases {
                        if let Some((_, m)) = self.masters.iter().find(|m| m.0 == *id) {
                            list.CopyResource(base, m);
                        }
                    }
                }
                barriers(list, &back);
            }
        })?;
        self.fresh = true;
        Ok(())
    }
}
