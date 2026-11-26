use super::*;

impl Host {

    /// The relay's own list, fence, event for [`Host::relay_card_back`].
    pub(in crate::host) fn make_relay_fetch(&self) -> Option<RelayFetch> {
        // SAFETY: plain object creation.
        let copying = relay_copy().then(|| unsafe {
            (|| -> windows::core::Result<(Prelude, ID3D12CommandQueue)> {
                let allocator: ID3D12CommandAllocator = self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
                let list: ID3D12GraphicsCommandList =
                    self.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
                list.Close()?;
                let queue: ID3D12CommandQueue = self.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    Priority: D3D12_COMMAND_QUEUE_PRIORITY_HIGH.0,
                    ..Default::default()
                })?;
                Ok(((allocator, list), queue))
            })()
        });
        let (list, queue) = match copying {
            Some(Ok((list, queue))) => (list, Some(queue)),
            Some(Err(e)) => {
                tracing::warn!(error = %e, "no copy queue: what shaders wrote to the card's memory is fetched on the drawing queue");
                (self.make_prelude()?, None)
            }
            None => (self.make_prelude()?, None),
        };
        // SAFETY: plain objects of our own.
        unsafe {
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                queue,
                fence,
                event,
                sent: 0,
                buffer: None,
                suspects: torn::Suspects::default(),
                had: crate::land::Had::default(),
            })
        }
    }

    /// [`relay`], on the thread that lands flights: for the plans of the
    /// flights being landed (the card has run them), the pieces their
    /// compares flagged are fetched, once each, with a list and a wait of
    /// this thread's own, and what shaders wrote in them goes into title
    /// memory (`Shadow::card_merge_relay`). As [`Host::card_back`], but:
    /// no submission lock; a piece flagged under one range of a plan is
    /// fetched under all ([`flagged_pieces`]); the fetch runs behind
    /// whatever was sent meanwhile, so it may bring a later flight's
    /// bytes, which that flight's landing then finds merged already; and
    /// nothing is sent back up. `plans` come with the value signalled behind
    /// their flight. `meanwhile` is called while the fetch is on its way, if
    /// there is one.
    pub(in crate::host) fn relay_card_back(&self, plans: &[(&CardPlan, u64)], fetch: &mut Option<RelayFetch>, meanwhile: &mut dyn FnMut()) {
        use crate::land;
        let gathering = std::time::Instant::now();
        let differs = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            work.as_ref().map(|work| (work.differs.1, (work.differs.2 / 4) as usize))
        };
        let mut pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        if let (Some(fetch), Some(first)) = (fetch.as_mut(), plans.iter().map(|p| p.1).min()) {
            fetch.had.landing(first);
        }
        // SAFETY: the mapping of the results. The compares of these plans
        // have run, and their dwords are given to no other plan before these
        // are done with (`card_done`).
        let differs = differs.map(|(differs, dwords)| unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) });
        if let Some(differs) = differs {
            // Pieces caught while a later flight was at them (`torn`).
            let suspects = fetch.as_ref().map(|f| &f.suspects);
            let mut seen = std::collections::HashSet::<
                (usize, usize, usize),
                std::hash::BuildHasherDefault<AddressHash>,
            >::default();
            for &(plan, behind) in plans {
                plan.picked_found(differs);
                for r in plan.ranges.iter().filter(|r| !r.picked) {
                    let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                    let shadow_id = r.shadow.id();
                    let shadow_base = r.shadow.span().0 as usize;
                    if upcheck::on() {
                        upcheck::declared(shadow_id, r.span.from as usize, r.span.to as usize);
                    }
                    for p in 0..r.span.pieces {
                        let flagged = !r.compared || differs.get((r.at + p) as usize).is_none_or(|&d| torn::flagged(d));
                        let wanted = suspects.map_or(flagged, |s| s.wanted((shadow_id, r.span.first + p), flagged));
                        if !wanted {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo < hi && seen.insert((copy, lo, hi)) {
                            let key = (shadow_id, shadow_base + lo, hi - lo);
                            if fetch.as_ref().is_some_and(|f| f.had.holds(behind, key)) {
                                land::count(land::stat::HAD, 1);
                                land::count(land::stat::HAD_BYTES, (hi - lo) as u64);
                                if land::skip_on() {
                                    continue;
                                }
                            }
                            if upcheck::on() {
                                upcheck::fetched(shadow_id, lo);
                            }
                            pieces.push((r.shadow.clone(), lo, hi, total));
                            total += (hi - lo) as u64;
                        }
                    }
                    crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                }
            }
        }
        if land::on() {
            // In the order they are landed, each byte of a copy once.
            let keys: Vec<land::Piece> =
                pieces.iter().map(|(shadow, lo, hi, _)| (shadow.id(), shadow.span().0 as usize + lo, hi - lo)).collect();
            total = 0;
            pieces = land::ordered(&keys)
                .into_iter()
                .map(|(i, title, len)| {
                    let shadow = &pieces[i].0;
                    let lo = title - shadow.span().0 as usize;
                    let at = total;
                    total += len as u64;
                    (shadow.clone(), lo, lo + len, at)
                })
                .collect();
        }
        land::spent(land::stat::GATHER, gathering);
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        // On the copy queue a later flight may be at a piece while it is
        // fetched: the pieces come down whole, to be hashed.
        let check = (torn::on() && fetch.as_ref().is_some_and(|f| f.queue.is_some()))
            .then(|| torn::Check::new(plans.iter().map(|p| p.0), differs.unwrap_or_default(), &mut pieces));
        if let Some(check) = &check {
            total = check.total;
        }
        #[cfg(feature = "diagnostics")]
        for (shadow, lo, hi, _) in &pieces {
            traffic::moved(traffic::FETCHED, shadow.span(), std::iter::once((*lo, *hi)));
        }
        // The sends these flights ran behind, all a fetch on the copy queue
        // is known to have run behind.
        let behind = plans.iter().map(|p| p.0.sent_as).max().unwrap_or(0);
        if pieces.is_empty() {
            // Nothing to fetch: every send so far is older than the next
            // fetch, if that goes to the queue the sends went to.
            let after = match fetch.as_ref().is_some_and(|f| f.queue.is_some()) {
                true => behind,
                false => *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()),
            };
            crate::shadow::relay_fetched(after);
            return;
        }
        let Some(f) = fetch.as_mut() else {
            fail(format_args!("no list, fence or event to fetch what shaders wrote to the card's memory (OATHKEEP_RELAY=1)"));
        };
        if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
            let size = total.next_multiple_of(16 << 20);
            f.buffer = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
        }
        let Some((buffer, data, _)) = f.buffer.clone() else {
            fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes, OATHKEEP_RELAY=1)"));
        };
        f.sent += 1;
        let ((allocator, list), fence, event, sent) = (f.list.clone(), f.fence.clone(), f.event, f.sent);
        let copy_queue = f.queue.clone();
        let waiting = std::time::Instant::now();
        // The flights the fetch runs behind: those in the line by now have
        // been given to the queue. On the copy queue, those landed here.
        let ahead = match copy_queue {
            Some(_) => 0,
            None => line().target,
        };
        // SAFETY: the list's last run was waited for by this thread; live
        // buffers (the copies are held by the plans); the fence and the
        // event are this thread's.
        let ran = unsafe {
            (|| -> windows::core::Result<u64> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                for (shadow, lo, hi, at) in check.as_ref().map_or(&pieces, |c| &c.copies) {
                    list.CopyBufferRegion(&buffer, *at, &shadow.buffer, *lo as u64, (*hi - *lo) as u64);
                }
                list.Close()?;
                let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                let _fetching = crate::timeline::span("landing: fetching pieces");
                if let Some(queue) = &copy_queue {
                    queue.ExecuteCommandLists(&lists);
                    queue.Signal(&fence, sent)?;
                    meanwhile();
                    fence.SetEventOnCompletion(sent, event)?;
                    #[cfg(feature = "diagnostics")]
                    let _card = crate::timeline::card_wait(None);
                    WaitForSingleObject(event, INFINITE);
                    return Ok(behind);
                }
                // In the order of the queue: the sends of title bytes so
                // far are ahead of this fetch, every later one behind it.
                let order = QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
                self.queue.ExecuteCommandLists(&lists);
                let after = *order;
                drop(order);
                self.card_sent();
                self.queue.Signal(&fence, sent)?;
                meanwhile();
                fence.SetEventOnCompletion(sent, event)?;
                #[cfg(feature = "diagnostics")]
                let _card = crate::timeline::card_wait(None);
                WaitForSingleObject(event, INFINITE);
                Ok(after)
            })()
        };
        let after = match ran {
            Ok(after) => after,
            Err(e) => fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e}, OATHKEEP_RELAY=1)")),
        };
        let us = waiting.elapsed().as_micros() as u64;
        RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
        }
        let merging = std::time::Instant::now();
        let _merging = crate::timeline::span("landing: merging fetched pieces");
        let mut written = 0;
        if land::on() {
            let titles: Vec<(usize, usize)> =
                pieces.iter().map(|(shadow, lo, hi, _)| (shadow.span().0 as usize + lo, hi - lo)).collect();
            let jobs = land::jobs(&titles);
            let put = std::sync::atomic::AtomicU64::new(0);
            land::each(jobs.len(), &|j| {
                let working = std::time::Instant::now();
                if let Some(check) = &check {
                    for i in jobs[j].clone() {
                        // SAFETY: as below.
                        unsafe { check.look(i, data) };
                    }
                }
                let mut notes = land::Notes::default();
                let mut addressed: Vec<(usize, usize, usize)> = Vec::new();
                for run in pieces[jobs[j].clone()].chunk_by(|a, b| std::sync::Arc::ptr_eq(&a.0, &b.0)) {
                    addressed.clear();
                    addressed.extend(run.iter().map(|(_, lo, hi, at)| (*lo, *hi, data + *at as usize)));
                    // SAFETY: the mapping of the fetched bytes, `total` of
                    // them; the copy into it has run, and the next waits
                    // for this call.
                    put.fetch_add(unsafe { run[0].0.land_card(&addressed, after, &mut notes) }, Ordering::Relaxed);
                }
                notes.flush();
                land::spent(land::stat::CARD_ALL, working);
            });
            land::count(land::stat::JOBS, jobs.len() as u64);
            written = put.into_inner();
        } else {
            let mut title = Vec::new();
            for (i, (shadow, lo, hi, at)) in pieces.iter().enumerate() {
                if let Some(check) = &check {
                    // SAFETY: as below.
                    unsafe { check.look(i, data) };
                }
                // SAFETY: the mapping of the fetched bytes, `total` of them;
                // the copy into it has run.
                let gpu = unsafe { std::slice::from_raw_parts((data + *at as usize) as *const u8, hi - lo) };
                let merged = shadow.card_merge_relay(*lo, gpu, after, &mut title);
                shadow.walls_merged(gpu.len(), merged);
                written += merged as u64;
            }
        }
        land::spent(land::stat::CARD, merging);
        COPY_BACK[1].fetch_add(written, Ordering::Relaxed);
        if let Some(f) = fetch.as_mut() {
            if let Some(check) = &check {
                check.done(&mut f.suspects);
            }
            f.had.fetched(
                ahead,
                pieces.iter().map(|(shadow, lo, hi, _)| (shadow.id(), shadow.span().0 as usize + lo, hi - lo)),
            );
        }
        // Sends up to `after` are of no use to any later merge.
        crate::shadow::relay_fetched(after);
    }
}
