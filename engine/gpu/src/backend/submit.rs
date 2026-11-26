use super::*;

impl Host {

    /// [`Host::execute`], or with `late` the first half of
    /// [`Host::execute_late`]: the lists are prepared and sent, and what is
    /// to be done once they have run comes back with the submission lock
    /// still held.
    ///
    /// `parts`: the lists of `runs` at which another part begins
    /// ([`joins`]): those go to the queue in calls of their own, one after the
    /// other, as they would have.
    pub(in crate::backend) fn submit<'a>(&'a self, runs: &[&HostList], parts: &[usize], late: bool) -> Option<(std::sync::MutexGuard<'a, u64>, Sent)> {
        #[cfg(feature = "diagnostics")]
        let called = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::submission_begins();
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        let mut stage = crate::diagnostics::timeline::span("submit: the submission lock");
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let was_working = working(true);
        submitting(1, runs.len());
        #[cfg(feature = "diagnostics")]
        let ahead = Split::begin();
        #[cfg(feature = "diagnostics")]
        SPLIT[11].fetch_add(called.elapsed().as_nanos() as u64, Ordering::Relaxed);
        stage.next("submit: steps given to the card");
        // Flights in the air ([`late`]): a submission that is waited for
        // settles them first, one sent ahead only if preparing it reads
        // what they write.
        // Or the reads are done on the card, behind the flights.
        let own = if late { self.card_steps(runs) } else { None };
        #[cfg(feature = "diagnostics")]
        ahead.part(12);
        stage.next("submit: waiting for flights it reads from");
        if !late {
            self.settle(&mut submitted, WHY_SYNC);
        } else if own.is_none()
            && let Some(why) = late_blocked(runs)
        {
            if relay_exact() && why == WHY_READ {
                // Only until the flights that may write what is read have
                // landed: their copy-back's end, not the card's other work.
                relay_wait(why, |line| {
                    blocked_by(&line.written, runs, false).is_none() && !gpu_steps::copied_read(&gpu_steps::copies(line), runs)
                });
            } else {
                self.settle(&mut submitted, why);
            }
        }
        #[cfg(feature = "diagnostics")]
        ahead.part(13);
        // Descriptors the title copied into placed heaps directly, before
        // command lists that index those slots are submitted.
        let heaps_synced = crate::descriptors::begin_sync_heaps();
        // That list runs first, and is one of the submission's from here on.
        let with: Vec<&HostList>;
        let runs = match &own {
            Some(own) => {
                with = std::iter::once(own.list()).chain(runs.iter().copied()).collect();
                &with[..]
            }
            None => runs,
        };
        let parts: Vec<usize> = parts.iter().map(|&n| n + usize::from(own.is_some())).collect();
        stage.next("submit: the sweep");
        let behind = late && flying();
        // Before this submission counts as started: what the sweep lets go
        // of is named by no list, and nothing runs now (behind flights the
        // sweep waits for a submission that finds none in the air: there is
        // one at every present).
        static SWEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !behind {
            if upload_verify::on() {
                upload_verify::at_rest(self);
            }
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        } else if late_free_present()
            && should_settle_for_sweep(
                crate::memory::residency::pressured(),
                now_ms().saturating_sub(SWEPT.load(Ordering::Relaxed)),
            )
        {
            // `sweep()` only frees objects that are unreferenced (`Arc::strong_count == 1`,
            // `references == 1`, `long_ago(..., IDLE_FRAMES=4)`) or defers via `retire()`,
            // so it does not need a full GPU drain unless `upcheck` is active.
            if upload_verify::on() {
                self.settle(&mut submitted, WHY_SYNC);
                upload_verify::at_rest(self);
            }
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        }
        let behind = late && flying();
        stage.next("submit: gathering what to ask about");
        let this_submission = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        // The ranges shaders write are counted for it (with the relay).
        crate::memory::shadow::preparing(this_submission);
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        ahead.part(14);
        #[cfg(feature = "diagnostics")]
        let lock = (started - called).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        #[cfg(feature = "diagnostics")]
        let split = Split::begin();
        // The time a settle of flights takes inside this preparing: the
        // wait for the card and the copy-back have their own places on the
        // frame line (run 92 showed them twice, "preparing" 250 ms for 135).
        let mut settled = std::time::Duration::ZERO;
        // One write-watch asking for everything the submission refreshes
        // (`OATHKEEP_ASK_ONCE`): per copy, the span of its refreshed ranges.
        if crate::memory::watch::once() {
            let mut spans: ByCopy<(usize, usize)> = ByCopy::default();
            let mut last_key = 0usize;
            let mut last_span = (0usize, 0usize);
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::Refresh(shadow, address, len) = step
                        && shadow.asked_ahead()
                    {
                        let key = std::sync::Arc::as_ptr(shadow) as usize;
                        let end = *address + *len;
                        if key == last_key {
                            last_span = (last_span.0.min(*address), last_span.1.max(end));
                        } else {
                            if last_key != 0 {
                                spans
                                    .entry(last_key)
                                    .and_modify(|s| *s = (s.0.min(last_span.0), s.1.max(last_span.1)))
                                    .or_insert(last_span);
                            }
                            last_key = key;
                            last_span = (*address, end);
                        }
                    }
                }
            }
            if last_key != 0 {
                spans
                    .entry(last_key)
                    .and_modify(|s| *s = (s.0.min(last_span.0), s.1.max(last_span.1)))
                    .or_insert(last_span);
            }
            let ranges: Vec<(usize, usize)> = spans.into_values().collect();
            #[cfg(feature = "diagnostics")]
            SPLIT[8].fetch_add(split.so_far(), Ordering::Relaxed);
            stage.next("submit: asking the write watch");
            crate::memory::watch::ask_once(&ranges);
            #[cfg(feature = "diagnostics")]
            split.part(0);
            stage.next("submit: title bytes under flights' writes");
            // Behind flights: the title wrote memory that a flight's shaders
            // may write too, and this submission refreshes it. The flights
            // are settled, and the asking made again: the copy-back wrote
            // title memory, which other copies of it must see.
            if upload_verify::on() {
                upload_verify::order(runs);
                if behind {
                    upload_verify::reads(runs);
                }
            }
            let stale = if behind { stale_under_flights(runs) } else { Vec::new() };
            if !stale.is_empty() {
                let settling = std::time::Instant::now();
                self.settle_stale(&mut submitted, runs, stale, &ranges);
                settled += settling.elapsed();
            }
        } else if behind && !stale_under_flights(runs).is_empty() {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_STALE);
            settled += settling.elapsed();
        }
        #[cfg(feature = "diagnostics")]
        split.part(1);
        // What flights in the air may write.
        let air = line().written.clone();
        stage.next("submit: refreshes ahead of writes");
        if refresh_front() {
            batch.refresh_front(runs, &parts);
        } else if ahead_of_writes() {
            batch.refresh_ahead(runs, |list, i| own.as_ref().is_some_and(|own| own.takes(list, i)));
        }
        stage.next("submit: the lists' steps");
        for list in runs {
            list.run_deferred(&mut batch, &air, |i| own.as_ref().is_some_and(|own| own.takes(list, i)));
            firsts.push(lists.len());
            if let Some(prelude) = list.prelude() {
                lists.push(Some(prelude));
            }
            lists.push(list.list.cast().ok());
        }
        batch.fill();
        stage.next("submit: the last refreshes");
        batch.flush();
        crate::memory::watch::ask_once_end();
        #[cfg(feature = "diagnostics")]
        split.part(2);
        stage.next("submit: snapshots");
        // The snapshots of the ranges shaders may write, if they were left
        // for now (`crate::memory::shadow::wide_back`): the refreshes are done, the
        // lists have not gone to the card.
        {
            #[cfg(feature = "diagnostics")]
            let snapshots = std::time::Instant::now();
            crate::memory::shadow::take_snapshots();
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::SnapshotUs, snapshots.elapsed().as_micros() as u64);
        }
        #[cfg(feature = "diagnostics")]
        split.part(3);
        stage.next("submit: copies in the card's memory");
        // Copies in the card's memory: what changed on their CPU side goes
        // up ahead of the lists, and a compare of what the shaders may
        // write follows them in the same submission.
        let flown = late && relay();
        if flown && self.card_results_low() {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_MOST);
            settled += settling.elapsed();
        }
        // What shaders may write through views they pick: last, so that no
        // refresh of this submission leaves it alone.
        let picked = crate::diagnostics::picked_writes::declare(runs);
        for (shadow, at, len) in &picked.written {
            shadow.writing(*at, *len);
        }
        let declared = declared_writes(runs);
        let card = self.card_before(&declared, &picked, flown);
        // The sends are taken: from here on the references hold what the
        // copies in the lists put.
        for (shadow, at, bytes) in batch.placed.drain(..) {
            shadow.placed_copy(at, &bytes);
        }
        if let Some(check) = card.as_ref().and_then(|c| c.check.clone()) {
            lists.push(Some(check));
        }
        #[cfg(feature = "diagnostics")]
        split.part(4);
        stage.next("submit: waiting for the heaps' records");
        drop(heaps_synced);
        // The slots that waited for a texture's host copy, before the
        // lists that may read them run.
        crate::descriptors::make_waiting();
        #[cfg(feature = "diagnostics")]
        split.part(15);
        let prepared = std::time::Instant::now();
        stage.next("submit: sending");
        // When it runs on the card, for the timeline.
        #[cfg(feature = "diagnostics")]
        let stamp = timeline_stamps::take();
        #[cfg(feature = "diagnostics")]
        if let Some((ahead, behind)) = stamp.and_then(timeline_stamps::lists) {
            lists.insert(0, Some(ahead));
            lists.push(Some(behind));
            firsts.iter_mut().for_each(|first| *first += 1);
        }
        #[cfg(feature = "diagnostics")]
        let sending = crate::diagnostics::timeline::ticks();
        if late {
            *submitted += 1;
            #[cfg(feature = "diagnostics")]
            if let Some([front, behind]) = self.span_lists() {
                lists.insert(0, Some(front));
                lists.push(Some(behind));
                firsts.iter_mut().for_each(|first| *first += 1);
            }
            let cuts: Vec<usize> = parts.iter().filter_map(|&n| firsts.get(n).copied()).collect();
            // SAFETY: closed lists; the fence is ours.
            let signalled = unsafe {
                let mut from = 0;
                for to in cuts.into_iter().chain([lists.len()]) {
                    self.queue.ExecuteCommandLists(&lists[from..to]);
                    from = to;
                }
                self.queue.Signal(&self.fence, *submitted)
            };
            clock_keepalive::sent(&self.device);
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::timeline::sent(sending, *submitted, stamp);
            self.card_sent();
            stage.next("submit: noting what is in the air");
            if let Err(e) = signalled {
                fail(format_args!("a submission sent ahead could not be followed by a signal ({e})"));
            }
            #[cfg(feature = "diagnostics")]
            split.part(5);
            let back = crate::diagnostics::picked_writes::with(declared, &picked.written);
            let mut texture_back_ranges = Vec::new();
            for list in runs {
                texture_back_ranges.extend(list.deferred().texture_backs.iter().map(crate::textures::readback::Back::range));
            }
            let writes = runs.iter().flat_map(|l| l.deferred().writes.clone()).collect();
            SUBMISSION_US[0].fetch_add((prepared - started).saturating_sub(settled).as_micros() as u64, Ordering::Relaxed);
            submitting(0, 0);
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::copy_stats::submission_ends(lock);
            let sent = Sent {
                number: this_submission,
                card,
                back,
                own,
                texture_back_ranges,
                writes,
            };
            working(was_working);
            return Some((submitted, sent));
        }
        // Diagnostics: each list on its own, timed (`OATHKEEP_GPU_TIMING`).
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        submitting(2, runs.len());
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                self.queue.ExecuteCommandLists(group);
                self.card_sent();
                let waited = self
                    .queue
                    .Signal(&self.fence, *submitted)
                    .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                #[cfg(feature = "diagnostics")]
                crate::diagnostics::timeline::sent(sending, *submitted, stamp);
                match waited {
                    Ok(()) => {
                        working(false);
                        #[cfg(feature = "diagnostics")]
                        let _card = crate::diagnostics::timeline::card_wait(Some(*submitted));
                        WaitForSingleObject(self.event, INFINITE);
                        stood(started);
                        working(true);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            // With timing on each group is one of `runs` (and the compare
            // of copies in the card's memory, with the last): every
            // command's time on the card goes to the sums by pipeline.
            #[cfg(feature = "diagnostics")]
            if timing() {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                for (ms, op) in runs[i].slowest(frequency, usize::MAX) {
                    crate::diagnostics::copy_stats::command_took(&op, ms);
                }
            }
            // A submission of one list was never named here (`groups.len()
            // > 1` was the test): the two 50 ms lists of the gameplay
            // frame, each a submission of its own, printed nothing.
            #[cfg(feature = "diagnostics")]
            if timing() && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner()).len();
                tracing::warn!(ms = started.elapsed().as_millis(), commands, "slow host list");
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        FINISHED.store(this_submission, Ordering::Release);
        bury(this_submission);
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::card_waited(gpu.as_micros() as u64, || {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            let text = format!("{} lists, pipelines {pipelines:?}", runs.len());
            text.chars().take(1200).collect()
        });
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            tracing::warn!(ms = gpu.as_millis(), lists = runs.len(), ?pipelines, "slow host submission");
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        submitting(3, runs.len());
        stage.next("submit: copying back");
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        // For copies in the card's memory the compare has run there: the
        // pieces that differ are fetched and merged.
        if let Some(card) = &card {
            self.card_back(card, &mut submitted, &mut std::collections::HashSet::new());
        }
        let back = crate::diagnostics::picked_writes::with(declared, &picked.written);
        hand_back(&back);
        if let Some(card) = card {
            self.card_done(card);
        }
        #[cfg(feature = "diagnostics")]
        if !back.is_empty() {
            crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::BackSubmissions, 1);
        }
        // Nothing of this submission goes back to the title: no range of
        // shader writes, no value written when it is done. The wait for
        // such a one is only for order and for what the card still holds.
        #[cfg(feature = "diagnostics")]
        if back.is_empty() && runs.iter().all(|l| l.deferred().writes.is_empty()) {
            use crate::diagnostics::copy_stats::{C, add};
            add(C::NothingBack, 1);
            add(C::NothingBackUs, gpu.as_micros() as u64);
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        for list in runs {
            // Read backs first: a value written at the bottom of the pipe
            // may be what tells the title that they are there.
            read_backs(list);
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
        submitting(0, 0);
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::submission_ends(lock);
        // With the relay: nothing is in the air now, so no fetch still to
        // be merged is older than the sends to card copies so far, and
        // their notes can go.
        if relay() {
            crate::memory::shadow::relay_fetched(*QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        }
        working(was_working);
        None
    }
}
