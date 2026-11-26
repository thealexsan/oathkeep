//! Command-list construction, timestamp placement, and command translation.

use super::*;

impl Player {
    /// Makes the lists of all submissions, and before each one's the copies
    /// of what the CPU wrote.
    pub(super) fn build(&self, submits: &[Vec<Vec<Cmd>>], o: &Options) -> Result<Sends> {
        // Room for the timestamps of every command that has one.
        let count: usize = submits
            .iter()
            .flatten()
            .map(|c| {
                2 + if o.detailed {
                    c.iter()
                        .filter(|c| c.works() || matches!(c, Cmd::Barriers { .. }))
                        .count()
                } else {
                    0
                }
            })
            .sum::<usize>()
            + 2 * submits.len();
        let mut heap: Option<ID3D12QueryHeap> = None;
        let desc = D3D12_QUERY_HEAP_DESC {
            Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
            Count: count.max(1) as u32,
            NodeMask: 0,
        };
        // SAFETY: plain creation.
        unsafe { self.device.CreateQueryHeap(&desc, &mut heap)? };
        let heap = heap.context("no timestamps")?;
        let (back, data) = gpu::buffer(
            &self.device,
            D3D12_HEAP_TYPE_READBACK,
            count.max(1) as u64 * 8,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        let mut next = 0u32;
        let mut steps = Vec::new();
        for (i, lists) in submits.iter().enumerate() {
            let parts = self.delta_parts.get(i).filter(|p| !p.is_empty());
            let delta = match (parts, &self.deltas) {
                (Some(parts), Some(from)) => {
                    let open = self.open(&heap, &mut next)?;
                    // SAFETY: an open list; live buffers.
                    unsafe {
                        let mut seen = HashSet::new();
                        let mut after = Vec::new();
                        let mut merges = Vec::new();
                        for (d, at) in parts {
                            let Some(ob) = self.objects.get(&d.res) else {
                                continue;
                            };
                            if self.merging && self.card_writes.contains(&d.res) {
                                merges.push((d.res, d.at, *at, d.len));
                                continue;
                            }
                            open.1.CopyBufferRegion(&ob.res, d.at, from, *at, d.len);
                            if seen.insert(d.res) {
                                // Back to the common state by itself, for
                                // lists in the same submission.
                                after.push(gpu::transition(&ob.res, ALL, COPY_DEST, COMMON));
                            }
                        }
                        barriers(&open.1, &after);
                        if !merges.is_empty() {
                            self.wait_for_all(&open.1);
                            self.merge_all(&open.1, from.GetGPUVirtualAddress(), &merges);
                            self.wait_for_all(&open.1);
                        }
                    }
                    Some(self.close(open, &heap, &mut next, Vec::new())?)
                }
                _ => None,
            };
            let mut built = Vec::new();
            for cmds in lists {
                let open = self.open(&heap, &mut next)?;
                let stamps = self.translate(
                    &open.1,
                    cmds,
                    i,
                    o.detailed.then_some((&heap, &mut next)),
                    o.wait_all,
                );
                built.push(self.close(open, &heap, &mut next, stamps)?);
            }
            steps.push((delta, built));
        }
        // Between submissions that are sent as one: everything waits. A list
        // each: one cannot stand twice in a submission.
        let mut joints = Vec::new();
        if o.one_submit {
            for _ in 0..2 * submits.len() {
                let open = self.plain_list()?;
                // SAFETY: an open list.
                unsafe {
                    self.wait_for_all(&open.1);
                    open.1.Close()?;
                }
                joints.push(open);
            }
        }
        let resolve = {
            let open = self.plain_list()?;
            // SAFETY: an open list; `next` queries were written.
            unsafe {
                open.1.ResolveQueryData(
                    &heap,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    0,
                    next.max(1),
                    &back,
                    0,
                );
                open.1.Close()?;
            }
            open
        };
        Ok(Sends {
            steps,
            joints,
            resolve,
            _heap: heap,
            _back: back,
            data: data as usize,
            count: next,
        })
    }

    /// Records a wait for everything before, with all caches written out.
    ///
    /// # Safety
    /// An open list.
    pub(super) unsafe fn wait_for_all(&self, list: &ID3D12GraphicsCommandList) {
        // SAFETY: the caller's; a feature structure of its size.
        unsafe {
            let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
            let size = size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS12>() as u32;
            let known = self
                .device
                .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS12, (&raw mut o).cast(), size)
                .is_ok();
            if known
                && o.EnhancedBarriersSupported.as_bool()
                && let Ok(list) = list.cast::<ID3D12GraphicsCommandList7>()
            {
                let all = [D3D12_GLOBAL_BARRIER {
                    SyncBefore: D3D12_BARRIER_SYNC_ALL,
                    SyncAfter: D3D12_BARRIER_SYNC_ALL,
                    AccessBefore: D3D12_BARRIER_ACCESS_COMMON,
                    AccessAfter: D3D12_BARRIER_ACCESS_COMMON,
                }];
                list.Barrier(&[D3D12_BARRIER_GROUP {
                    Type: D3D12_BARRIER_TYPE_GLOBAL,
                    NumBarriers: 1,
                    Anonymous: D3D12_BARRIER_GROUP_0 {
                        pGlobalBarriers: all.as_ptr(),
                    },
                }]);
            } else {
                list.ResourceBarrier(&[alias(), uav_barrier(None)]);
            }
        }
    }

    pub(super) fn plain_list(&self) -> Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
        // SAFETY: plain creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            Ok((allocator, list))
        }
    }

    fn open(
        &self,
        heap: &ID3D12QueryHeap,
        next: &mut u32,
    ) -> Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList, u32)> {
        let (allocator, list) = self.plain_list()?;
        // SAFETY: an open list; a query of the heap.
        unsafe { list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, *next) };
        *next += 1;
        Ok((allocator, list, *next - 1))
    }

    fn close(
        &self,
        (allocator, list, first): (ID3D12CommandAllocator, ID3D12GraphicsCommandList, u32),
        heap: &ID3D12QueryHeap,
        next: &mut u32,
        stamps: Vec<Stamp>,
    ) -> Result<Built> {
        // SAFETY: as above.
        unsafe {
            list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, *next);
            list.Close()
                .context("a list of the frame could not be recorded again")?;
        }
        *next += 1;
        Ok(Built {
            _allocator: allocator,
            list,
            first,
            last: *next - 1,
            stamps,
        })
    }

    /// Records commands into a list, in submission `submit`.
    pub(super) fn translate(
        &self,
        list: &ID3D12GraphicsCommandList,
        cmds: &[Cmd],
        submit: usize,
        mut stamp: Option<(&ID3D12QueryHeap, &mut u32)>,
        wait: bool,
    ) -> Vec<Stamp> {
        let version = self.version_of.get(submit).copied().unwrap_or(0);
        let mut clamps = self.pipelines_named(CLAMP);
        clamps.insert(edit::BATCH);
        let compares = self.pipelines_named(COMPARE);
        let mesh: HashSet<u32> = self
            .frame
            .pipelines
            .iter()
            .filter(|p| p.parts.iter().any(|part| matches!(part, wire::Part::Code { kind, .. } if *kind == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS.0)))
            .map(|p| p.id)
            .collect();
        let dispatches: HashSet<u32> = self
            .frame
            .signatures
            .iter()
            .filter(|s| {
                s.args
                    .iter()
                    .any(|a| a[0] == D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH.0 as u32)
            })
            .map(|s| s.id)
            .collect();
        let mut stamps = Vec::new();
        let mut bound = NONE;
        // What the compute side has bound.
        let mut compute: Vec<Cmd> = Vec::new();
        for c in cmds {
            if let Cmd::Pipeline { pso } = c {
                bound = *pso;
            }
            let merged = self.merged_copy(c);
            if let Some((from, what)) = merged {
                // SAFETY: an open list; live buffers; what the compute side
                // of it had bound is bound again.
                unsafe {
                    self.wait_for_all(list);
                    self.merge_all(list, from, &[what]);
                    self.wait_for_all(list);
                    for c in &compute {
                        self.command(list, c, version);
                    }
                }
            } else {
                // SAFETY: an open list; the objects are the frame's, alive
                // with the player; commands the host recorded the same way.
                unsafe { self.command(list, c, version) };
            }
            match c {
                Cmd::RootSignature {
                    graphics: false, ..
                } => {
                    compute.retain(|c| matches!(c, Cmd::Pipeline { .. }));
                    compute.push(c.clone());
                }
                Cmd::Pipeline { .. } => {
                    compute.retain(|c| !matches!(c, Cmd::Pipeline { .. }));
                    compute.push(c.clone());
                }
                Cmd::Table {
                    graphics: false, ..
                }
                | Cmd::Constants {
                    graphics: false, ..
                }
                | Cmd::RootView {
                    graphics: false, ..
                } => compute.push(c.clone()),
                _ => {}
            }
            if wait && c.works() {
                // SAFETY: an open list.
                unsafe { self.wait_for_all(list) };
            }
            let timed = c.works() || matches!(c, Cmd::Barriers { .. });
            if timed && let Some((heap, next)) = &mut stamp {
                // SAFETY: as above.
                unsafe { list.EndQuery(*heap, D3D12_QUERY_TYPE_TIMESTAMP, **next) };
                let category = match c {
                    Cmd::Barriers { .. } => "barriers",
                    Cmd::Dispatch { .. } if clamps.contains(&bound) => "argument clamps (ours)",
                    Cmd::Dispatch { .. } if compares.contains(&bound) => {
                        "hash and compare passes (ours)"
                    }
                    Cmd::Dispatch { .. } => "dispatches",
                    Cmd::Indirect { signature, .. } if dispatches.contains(signature) => {
                        "indirect dispatches"
                    }
                    Cmd::Indirect { .. } if mesh.contains(&bound) => "indirect mesh draws",
                    Cmd::Indirect { .. } => "indirect draws",
                    Cmd::Draw { .. } | Cmd::DrawIndexed { .. } | Cmd::DispatchMesh { .. } => {
                        "draws"
                    }
                    Cmd::ClearDepth { .. } | Cmd::ClearTarget { .. } | Cmd::Discard { .. } => {
                        "clears"
                    }
                    Cmd::Resolve { .. } | Cmd::ResolveRegion { .. } => "resolves",
                    _ => self.copy_kind(c),
                };
                let runs = match c {
                    c if c.runs_shaders() => bound,
                    // Copies to textures by what they go to.
                    Cmd::CopyResource { dst, .. }
                    | Cmd::CopyTexture {
                        dst: Loc::Sub { res: dst, .. },
                        ..
                    } => COPY_TO | dst,
                    _ => NONE,
                };
                stamps.push((**next as usize, category, runs));
                **next += 1;
            }
        }
        stamps
    }

    /// Which copies a copy is counted with: by the memory it crosses.
    fn copy_kind(&self, c: &Cmd) -> &'static str {
        let pc = |id: u32| {
            self.objects.get(&id).is_some_and(|o| {
                matches!(&self.frame.resources[o.index].place,
                    Place::Committed { props, .. } if gpu::seen(props) != Seen::No)
            })
        };
        let res = |l: &Loc| match l {
            Loc::Sub { res, .. } | Loc::Placed { res, .. } => *res,
        };
        match c {
            Cmd::CopyBuffer { dst, src, .. } => match (pc(*dst), pc(*src)) {
                (false, true) => "copies of buffers, PC to card",
                (true, false) => "copies of buffers, card to PC",
                (true, true) => "copies of buffers, PC to PC",
                (false, false) => "copies of buffers, card to card",
            },
            Cmd::CopyTexture { dst, src, .. } => match (pc(res(dst)), pc(res(src))) {
                (false, true) => "copies of textures, PC to card",
                (true, _) => "copies of textures, to the PC",
                (false, false) => "copies of textures, card to card",
            },
            Cmd::CopyResource { .. } => "copies of whole resources",
            _ => "copies",
        }
    }
}
