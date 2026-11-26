use super::*;

impl Host {

    /// Before a submission's lists run, for copies in the card's memory
    /// ([`crate::shadow::cards`]): what changed on their CPU side since
    /// the last submission goes to their card side (a list of ours, run
    /// now; the submission's lists follow it on the queue), and the ranges
    /// this submission's shaders may write get a compare to run after its
    /// lists. `None` if there is nothing to do.
    ///
    /// `flown`: the submission is sent ahead with [`relay`]. Its compare
    /// then stores the hashes it makes (see [`flagged_pieces`] and
    /// `relay`).
    /// `picked`: ranges that may be written too, and ranges to look at
    /// (`crate::picked`).
    pub(in crate::host) fn card_before(&self, declared: &[Declared], picked: &crate::picked::Ranges, flown: bool) -> Option<CardPlan> {
        use std::sync::Arc;
        if !crate::shadow::cards() {
            return None;
        }
        // The ranges this submission's shaders may write, per copy.
        let declared: Vec<Declared> = declared.iter().filter(|(shadow, _)| shadow.carded()).cloned().collect();
        let mut declared = if crate::picked::lands() { crate::picked::with(declared, &picked.written) } else { declared };
        // (Of a copy with a declared range here, that says it.)
        let looked: Vec<&crate::picked::Range> =
            picked.looked.iter().filter(|l| !declared.iter().any(|(s, _)| Arc::ptr_eq(s, &l.0))).collect();
        // With the relay: the order of the queue is held from here until
        // what is taken is on the queue, and the taking is numbered.
        let mut order = relay().then(|| QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        let sent_as = match order.as_mut() {
            Some(order) => {
                **order += 1;
                **order
            }
            None => 0,
        };
        let gathering = crate::timeline::span("copies in the card's memory: what changed");
        let mut dirty = crate::shadow::card_dirty(sent_as);
        gathering.end();
        if declared.is_empty() && dirty.is_empty() && looked.is_empty() {
            return None;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_mut()?;
        // A copy whose shader writes are declared for the first time gets
        // its bytes from before, whole.
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for shadow in declared.iter().map(|d| &d.0).chain(looked.iter().map(|l| &l.0)) {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::card_hash_bytes(size))?;
                name(&before, || "title buffer on the card, hashes of its pieces".into());
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new()));
                }
            }
        }
        let mut used: Vec<(bool, Prelude)> = Vec::new();
        let mut looks = None;
        // The bytes as they are now, whatever later submissions put there.
        let taking = crate::timeline::span("copies in the card's memory: taken");
        let own = if send::own() { send::take(self, &dirty) } else { None };
        taking.end();
        if !dirty.is_empty() {
            let Some(sync) = work.sync.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list to bring host copies in the card's memory up to date"));
            };
            let _recording = crate::timeline::span("copies in the card's memory: the list");
            let (allocator, list) = &sync;
            // SAFETY: the list's last run finished (it ran ahead of a
            // submission that was waited for, and came back after that);
            // live buffers, and the ranges lie in them.
            let recorded = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    if upcheck::on() {
                        looks = upcheck::record(self, list, &dirty);
                    }
                    // Every copy first, then one barrier call, then every
                    // pass: a wait between each copy and its pass had the
                    // card do them one after the other.
                    let mut sent = Vec::new();
                    let mut transitioned = Vec::new();
                    if let Some(own) = &own {
                        own.record(self, list, &dirty, |shadow| looks.as_ref().is_some_and(|l| l.hold(shadow)));
                    }
                    for (shadow, ranges) in &dirty {
                        let Some(staging) = shadow.card_staging() else {
                            continue;
                        };
                        let has_before = shadow.card_before_gpu().is_some();
                        if own.is_none() {
                            for &(a, b) in ranges {
                                list.CopyBufferRegion(&shadow.buffer, a as u64, staging, a as u64, (b - a) as u64);
                            }
                        }
                        let bytes: u64 = ranges.iter().map(|&(a, b)| (b - a) as u64).sum();
                        crate::shadow::CARD_STATS[0].fetch_add(bytes, Ordering::Relaxed);
                        crate::shadow::CARD_STATS[1].fetch_add(ranges.len() as u64, Ordering::Relaxed);
                        #[cfg(feature = "diagnostics")]
                        traffic::moved(traffic::UP, shadow.span(), ranges.iter().copied());
                        if own.is_none() && has_before && !ranges.is_empty() {
                            let raw = shadow.buffer.as_raw();
                            if !transitioned.contains(&raw) {
                                transitioned.push(raw);
                                sent.push(transition(
                                    &shadow.buffer,
                                    D3D12_RESOURCE_STATE_COPY_DEST,
                                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                                ));
                            }
                        }
                    }
                    if !sent.is_empty() {
                        list.ResourceBarrier(&sent);
                    }
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                    for (shadow, ranges) in &dirty {
                        let Some(before_gpu) = shadow.card_before_gpu() else {
                            continue;
                        };
                        let size = shadow.card_hash_size();
                        // No twin (it cost the card the copy's size again,
                        // 399 MB in run 75): `before` holds a 128-bit hash
                        // of each 64 KiB piece. The pieces just sent up are
                        // hashed again, all of them the first time; the
                        // compare after the lists hashes the declared
                        // pieces and flags those whose hash is another.
                        list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(2, before_gpu);
                        let joins = rehash_runs() || own.is_some();
                        let runs: Vec<crate::shadow::CardSpan> = if whole.iter().any(|s| Arc::ptr_eq(s, shadow)) {
                            vec![shadow.card_run(0, usize::MAX)]
                        } else {
                            // (In order: the next of the same piece, of the
                            // tens of thousands there are, says nothing.)
                            let mut runs: Vec<crate::shadow::CardSpan> = Vec::new();
                            for run in ranges.iter().map(|&(a, b)| shadow.card_run(a, b)) {
                                match runs.last_mut() {
                                    Some(last) if joins && (last.first, last.pieces) == (run.first, run.pieces) => last.to = last.to.max(run.to),
                                    _ => runs.push(run),
                                }
                            }
                            runs
                        };
                        // The same pieces in fewer passes (`rehash_runs`).
                        let runs = if joins { crate::shadow::joined_piece_runs(runs) } else { runs };
                        CARD_PASSES[0].fetch_add(runs.len() as u64, Ordering::Relaxed);
                        CARD_PASSES[1].fetch_add(runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>(), Ordering::Relaxed);
                        for run in runs {
                            card_pass(list, &run, size as u32, 0, 0);
                        }
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    Ok(())
                })()
            };
            if let Err(e) = recorded {
                fail(format_args!("host copies in the card's memory could not be brought up to date ({e})"));
            }
            used.push((false, sync));
        }
        drop(order);
        let mut plan = CardPlan {
            ranges: Vec::new(),
            check_runs: Vec::new(),
            check: None,
            _synced: dirty.into_iter().map(|(shadow, _)| shadow).collect(),
            lists: used,
            sent_as,
            looks,
            sends: own.map(send::Own::memory),
        };
        // Each byte once, as the copy-back joins the declared ranges, and each
        // 64 KiB piece hashed once on the GPU across sub-ranges of the same
        // buffer (`joined_piece_runs`).
        let mut at = work.taken;
        let declared_mode = if flown { 2 } else { 1 };
        for (shadow, ranges) in &mut declared {
            ranges.sort_unstable();
            let mut resolved: Vec<(usize, usize, crate::shadow::CardSpan)> = Vec::with_capacity(ranges.len());
            let mut w = 0usize;
            for i in 0..ranges.len() {
                let (from, to) = ranges[i];
                if w > 0 && from <= ranges[w - 1].1 {
                    ranges[w - 1].1 = ranges[w - 1].1.max(to);
                } else {
                    ranges[w] = (from, to);
                    w += 1;
                }
            }
            ranges.truncate(w);
            for &(from, to) in ranges.iter() {
                if let Some(span) = shadow.card_span(from, to) {
                    resolved.push((from, to, span));
                }
            }
            if resolved.is_empty() {
                continue;
            }
            let before_gpu = shadow.card_before_gpu();
            let size = shadow.card_hash_size() as u32;
            let joined = crate::shadow::joined_piece_runs(resolved.iter().map(|r| r.2).collect());
            let mut joined_meta: Vec<(crate::shadow::CardSpan, u32, bool)> = Vec::with_capacity(joined.len());
            for run in joined {
                let compared = before_gpu.is_some() && u64::from(at + run.pieces) * 4 <= work.differs.2;
                let run_at = at;
                if compared {
                    at += run.pieces;
                    if let Some(bgpu) = before_gpu {
                        plan.check_runs.push((shadow.clone(), bgpu, size, run, run_at, declared_mode));
                    }
                }
                joined_meta.push((run, run_at, compared));
            }
            let mut j = 0usize;
            for (from, to, span) in resolved {
                while j + 1 < joined_meta.len() && joined_meta[j].0.first + joined_meta[j].0.pieces <= span.first {
                    j += 1;
                }
                let (run, run_at, compared) = joined_meta[j];
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (from, to),
                    span,
                    at: if compared { run_at + (span.first - run.first) } else { run_at },
                    compared,
                    picked: false,
                });
            }
        }
        for (shadow, from, len) in looked {
            let before_gpu = shadow.card_before_gpu();
            if let (Some(bgpu), Some(span)) = (before_gpu, shadow.card_span(*from, from + len))
                && u64::from(at + span.pieces) * 4 <= work.differs.2
            {
                let size = shadow.card_hash_size() as u32;
                plan.check_runs.push((shadow.clone(), bgpu, size, span, at, 1));
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (*from, from + len),
                    span,
                    at,
                    compared: true,
                    picked: true,
                });
                at += span.pieces;
            }
        }
        work.taken = at;
        if !plan.check_runs.is_empty() {
            let Some(check) = work.check.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list for the compare of host copies in the card's memory"));
            };
            let (allocator, list) = &check;
            // SAFETY: the list's last run finished (its submission was
            // waited for, and it came back after that); live buffers; the
            // compare's own root signature.
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    // What the submission's shaders wrote, first.
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                    let mut last_shadow = 0usize;
                    for (shadow, before_gpu, size, span, run_at, mode) in &plan.check_runs {
                        let ptr = Arc::as_ptr(shadow) as usize;
                        if ptr != last_shadow {
                            last_shadow = ptr;
                            list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
                            list.SetComputeRootUnorderedAccessView(2, *before_gpu);
                        }
                        card_pass(list, span, *size, *run_at, *mode);
                        CARD_PASSES[2].fetch_add(1, Ordering::Relaxed);
                    }
                    list.Close()?;
                    list.cast()
                })()
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => fail(format_args!("the compare of host copies in the card's memory could not be recorded ({e})")),
            }
            plan.lists.push((true, check));
        }
        work.out += 1;
        Some(plan)
    }

    /// With [`relay`]: whether the compare's result area is half taken by
    /// plans in the air. It starts from the beginning only when no plan is
    /// out, so the caller lets the flights land first.
    pub(in crate::host) fn card_results_low(&self) -> bool {
        CARD_WORK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|work| u64::from(work.taken) * 8 > work.differs.2)
    }

    /// The plan's submission has run and its results were read
    /// ([`Host::card_back`]): its lists are free for the next plan. Called
    /// when nothing of ours is on the card, so the compare's results start
    /// from the beginning again.
    pub(in crate::host) fn card_done(&self, plan: CardPlan) {
        if let Some(looks) = plan.looks {
            looks.read();
        }
        if let Some(memory) = plan.sends {
            send::give_back(memory);
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        for (check, list) in plan.lists {
            if check { work.check.push(list) } else { work.sync.push(list) }
        }
        work.out = work.out.saturating_sub(1);
        // With the relay other plans may be on the card, their results
        // not read yet: the area starts again only when none is out.
        if !relay() || work.out == 0 {
            work.taken = 0;
        }
    }
}
