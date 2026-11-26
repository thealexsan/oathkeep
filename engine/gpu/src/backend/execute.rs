use super::*;

impl Host {

    /// Runs closed lists and waits for them, and with [`late`] for
    /// everything sent to the card before them, which is settled first.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        // Behind what the title's queues were given before it.
        crate::d3d12::command_queue::before_sync();
        let _ = self.submit(runs, &[], false);
    }

    /// Runs a closed internal upload-only host list synchronously without
    /// draining title queues (`queue::before_sync`), so it is deadlock-safe
    /// when invoked from `OATHKEEP_THIN=1` worker threads. Settles in-flight
    /// GPU work and waits for the upload list's fence before returning.
    pub(crate) fn execute_sync_internal(&self, list: &HostList) {
        let _ = self.submit(&[list], &[], false);
    }

    /// A queue's submission with [`late`]: the host lists `lists`, taken
    /// from the title's, go to the card in `order` and are not waited for.
    /// They join the line of flights and are settled, with the others in
    /// the air, at the next point where what they do can be observed
    /// ([`late`] lists those points); their host lists are reset and kept
    /// for reuse then, as a queue does for the lists it ran.
    pub(crate) fn execute_late(&self, lists: Vec<crate::d3d12::command_list::Detached>, order: Vec<usize>) {
        self.execute_parts(lists, order, &[]);
    }

    /// [`Host::execute_late`] of parts of submissions in one: another begins
    /// at each place of `order` in `parts` ([`joins`]).
    pub(crate) fn execute_parts(&self, lists: Vec<crate::d3d12::command_list::Detached>, order: Vec<usize>, parts: &[usize]) {
        if order.is_empty() {
            crate::d3d12::command_list::recycle(lists, &order);
            return;
        }
        late_threads();
        if let Some(cut) = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            gpu_steps::cut_for(&runs)
        } {
            match split(lists, order, cut) {
                Ok([(lists, order), (more, behind)]) => {
                    let first: Vec<usize> = parts.iter().copied().filter(|&p| p < cut).collect();
                    let second: Vec<usize> = parts.iter().filter(|&&p| p > cut).map(|p| p - cut).collect();
                    self.execute_parts(lists, order, &first);
                    self.execute_parts(more, behind, &second);
                }
                Err((lists, order)) => self.send_late(lists, order, parts),
            }
            return;
        }
        self.send_late(lists, order, parts);
    }

    pub(in crate::backend) fn send_late(&self, lists: Vec<crate::d3d12::command_list::Detached>, order: Vec<usize>, parts: &[usize]) {
        let sent = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            self.submit(&runs, parts, true)
        };
        let Some((mut submitted, sent)) = sent else {
            // (Not reached: a submission sent ahead always comes back.)
            crate::d3d12::command_list::recycle(lists, &order);
            return;
        };
        let told = !sent.writes.is_empty();
        let sizes = crate::memory::allocator::asked_sizes(order.iter().map(|&i| lists[i].host()));
        let most = {
            let mut line = line();
            let Line { line: items, written, lists: flying, target, .. } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied())
                    .chain(sent.texture_back_ranges.iter().copied()),
            );
            *flying += order.len();
            *target = *submitted;
            items.push(Late::Run(Flight {
                number: sent.number,
                fence: *target,
                lists,
                order,
                card: sent.card,
                back: sent.back,
                own: sent.own,
                texture_back_ranges: sent.texture_back_ranges,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
            *flying >= late_most()
        };
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        RELAY_MOVED.notify_one();
        if told && !late_writes() {
            self.settle(&mut submitted, WHY_WRITES);
        } else if sizes {
            self.settle(&mut submitted, WHY_READ);
        } else if most && relay_exact() {
            // Until there are fewer, not until there are none.
            relay_wait(WHY_MOST, |line| line.lists < late_most());
        } else if most {
            self.settle(&mut submitted, WHY_MOST);
        } else {
            LINE_MOVED.notify_one();
        }
        drop(submitted);
        let _landing = crate::diagnostics::timeline::span("landed lists reset and given back");
        land();
    }

    /// Sends `list`, a closed list of ours (the present's copy), as a
    /// flight ([`late_free_present`]): not waited for, settled with the
    /// others. The caller keeps the list and does not reset it before the
    /// submission whose number comes back has finished ([`FINISHED`]); 0 if
    /// it ran and was waited for after all.
    pub(in crate::backend) fn execute_own(&self, list: &HostList) -> u64 {
        late_threads();
        let Some((submitted, sent)) = self.submit(&[list], &[], true) else {
            return 0;
        };
        let number = sent.number;
        {
            let mut line = line();
            let Line { line: items, written, lists: flying, target, .. } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied())
                    .chain(sent.texture_back_ranges.iter().copied()),
            );
            *flying += 1;
            *target = *submitted;
            items.push(Late::Run(Flight {
                number,
                fence: *target,
                lists: Vec::new(),
                order: Vec::new(),
                card: sent.card,
                back: sent.back,
                own: sent.own,
                texture_back_ranges: sent.texture_back_ranges,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
        }
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        drop(submitted);
        number
    }

    /// Waits for the card to run every flight in the air ([`late`]) and
    /// does what they left to do, as [`Host::execute`] does for one
    /// submission after its wait: the pieces their shaders wrote in copies
    /// in the card's memory are fetched and merged (each once), the ranges
    /// they declared in the other copies are compared and copied back
    /// (each byte once, whichever flights declared it), and then, in the
    /// order they were given: their bottom-of-pipe writes and the fence
    /// signals behind them. `submitted` is the submission lock's value;
    /// `why` one of the `WHY_` places. Nothing happens if nothing flies.
    ///
    /// Everything in the air is settled, never the first flights alone:
    /// the merge writes the copies' CPU side, which a flight still on the
    /// card may be writing too.
    pub(in crate::backend) fn settle(&self, submitted: &mut u64, why: usize) {
        if !late() {
            return;
        }
        if relay() {
            // Nothing is settled here: the thread that lands flights does
            // it, in order, and this waits until nothing is in the air and
            // nothing is being landed. Every point that settled before
            // comes here, so each still sees title memory, fences and
            // bottom-of-pipe values as after a settle.
            relay_wait(why, |line| line.line.is_empty() && !line.settling);
            return;
        }
        let was_working = working(true);
        let _settling = crate::diagnostics::timeline::span("settling flights");
        let mut counted = false;
        loop {
            let items = {
                let mut line = line();
                if line.line.is_empty() {
                    line.settling = false;
                    break;
                }
                line.settling = true;
                line.written.clear();
                line.lists = 0;
                std::mem::take(&mut line.line)
            };
            let flights = items.iter().filter(|i| matches!(i, Late::Run(_))).count();
            if !counted {
                counted = true;
                LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
            }
            LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
            if flights > 0 {
                submitting(2, flights);
                let waiting = std::time::Instant::now();
                // SAFETY: the fence and event are ours, under the
                // submission lock; `submitted` is the last value signalled.
                unsafe {
                    match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            working(false);
                            #[cfg(feature = "diagnostics")]
                            let _card = crate::diagnostics::timeline::card_wait(Some(*submitted));
                            WaitForSingleObject(self.event, INFINITE);
                            stood(waiting);
                            working(true);
                        }
                        Err(e) => tracing::warn!(error = %e, "submissions sent ahead not waited for"),
                    }
                }
                let gpu = waiting.elapsed();
                LATE_STATS[12].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                LATE_WAITED[why.min(10)].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::diagnostics::copy_stats::card_waited(gpu.as_micros() as u64, || {
                    format!("{flights} submissions sent ahead, settled together")
                });
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = items
                        .iter()
                        .filter_map(|i| match i {
                            Late::Run(f) => Some(f),
                            _ => None,
                        })
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    fail(format_args!(
                        "host GPU device removed after {flights} submissions sent ahead (OATHKEEP_LATE_WAIT=1) of {} lists{}{}",
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                submitting(3, flights);
                let copying = std::time::Instant::now();
                let mut seen = std::collections::HashSet::new();
                let mut back: Vec<Declared> = Vec::new();
                for item in &items {
                    let Late::Run(flight) = item else {
                        continue;
                    };
                    if let Some(card) = &flight.card {
                        self.card_back(card, submitted, &mut seen);
                    }
                    for (shadow, ranges) in &flight.back {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => {
                                for range in ranges {
                                    if !all.contains(range) {
                                        all.push(*range);
                                    }
                                }
                            }
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                hand_back(&back);
                COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            // In the order given: what tells the title the card is done.
            let mut last = 0;
            let mut landed = Vec::new();
            for item in items {
                match item {
                    Late::Run(mut flight) => {
                        last = flight.number;
                        for &i in &flight.order {
                            read_backs(flight.lists[i].host());
                        }
                        if let Some(card) = flight.card {
                            self.card_done(card);
                        }
                        for detached in &mut flight.lists {
                            detached.release_tables();
                        }
                        landed.push((flight.lists, flight.order));
                    }
                    Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                    Late::Signal(fence, value) => crate::d3d12::fence::set(fence as crate::d3d12::object::This, value),
                }
            }
            if last > 0 {
                FINISHED.store(last, Ordering::Release);
                bury(last);
            }
            line().landed.extend(landed);
        }
        if counted {
            self.log_messages();
            submitting(0, 0);
            // What was settled may be what a queue stands at, and the
            // caller may not be one that looks (a read-back, a present).
            if why != WHY_CARD_DONE {
                line().poke = true;
                LINE_MOVED.notify_one();
            }
        }
        working(was_working);
    }

    /// For a submission of `runs` that refreshes the ranges `stale` of host
    /// copies, which flights may be writing and the title wrote too
    /// ([`stale_under_flights`]): the flights land first. `ranges` are
    /// those of the submission's one asking, which is made again.
    ///
    /// With [`card_steps`] only the flights that declared them: a range of
    /// a copy nobody declared any more is refreshed like any other. What
    /// they landed is title memory written, so everything is asked and
    /// looked at again; what is left after three times waits as before.
    pub(in crate::backend) fn settle_stale(
        &self,
        submitted: &mut u64,
        runs: &[&HostList],
        mut stale: Vec<(std::sync::Arc<crate::memory::shadow::Shadow>, usize, usize)>,
        ranges: &[(usize, usize)],
    ) {
        if relay_exact() && gpu_steps::stage() > 0 {
            for _ in 0..3 {
                #[cfg(feature = "diagnostics")]
                blame_stale(&stale);
                relay_wait(WHY_STALE, |_| stale.iter().all(|(shadow, from, to)| !shadow.writes_pending_in(*from, *to)));
                crate::memory::watch::ask_once(ranges);
                stale = stale_under_flights(runs);
                if stale.is_empty() {
                    return;
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        blame_stale(&stale);
        self.settle(submitted, WHY_STALE);
        crate::memory::watch::ask_once(ranges);
    }
}
