use super::*;
use super::analyze::{HashMap, RefreshSet, does_of, fill_to, holder, mapped};
use super::forwarding::Forwarding;
use super::plan::plan_inner;
use super::record::{DONE, Op, Own, POOL, ReadPiece, Source, To, record};
#[cfg(feature = "diagnostics")]
use super::record::noted;

impl Host {
    /// With flights in the air: if what keeps `runs` from being prepared
    /// now is reads of title memory that can all be done on the card
    /// ([`stage`]), the list that does them. `None` if nothing is in the
    /// way or the old wait is due. Under the submission lock.
    pub(in crate::host) fn card_steps(&self, runs: &[&HostList]) -> Option<Taken> {
        if stage() == 0 || !relay_exact() {
            return None;
        }
        let (written, copies) = {
            let line = line();
            if line.written.is_empty() {
                return None;
            }
            (line.written.clone(), copies(&line))
        };
        blocked_by(&written, runs, false)?;
        let taken = if copied_read(&copies, runs) {
            Err("a copy done on the card is read through another host copy")
        } else {
            self.take_steps(&written, runs)
        };
        match taken {
            Ok(taken) => taken,
            Err(why) => {
                DONE[3].fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                refused(why);
                let _ = why;
                None
            }
        }
    }

    fn take_steps(&self, written: &[(usize, usize)], runs: &[&HostList]) -> Result<Option<Taken>, &'static str> {
        let mut spent = crate::timeline::span("steps on the card: chosen");
        let (does, steps, _) = does_of(written, runs);
        let order_on = placed_order();
        let mut behind = None;
        let (chosen, fwd) = plan_inner(&does, stage(), order_on, order_on, &mut behind, None)?;
        if chosen.is_empty() {
            return Ok(None);
        }
        spent.next("steps on the card: what holds their bytes");
        let mut writers = {
            let line = line();
            let mut all = writers(&line.line);
            all.extend(line.landing.iter().map(|w| Writer {
                shadow: w.shadow.clone(),
                ..*w
            }));
            all
        };
        let mut ops: Vec<Op> = Vec::with_capacity(chosen.len());
        // The places read (host copy or 0 for staged, address, bytes, level):
        // where in the buffer between their bytes are.
        let mut places: HashMap<(usize, usize, usize, u32), u64> = HashMap::default();
        // Some 2,800 fills a frame, of a few hundred places: what holds each
        // is looked for once, as long as no copy is added to the writers.
        let mut held: HashMap<(usize, usize), Arc<Shadow>> = HashMap::default();
        let mut staged_places: HashMap<(usize, usize), u64> = HashMap::default();
        let mut refreshes: Option<RefreshSet> = None;
        let mut room = HashMap::default();
        let mut end = 0u64;
        let mut staged_end = 0u64;
        let mut single_buf = [(0usize, 0usize, 0usize); 1];
        for &(i, level) in &chosen {
            let Some((segs, _)) = fwd.segments_of(&does, i, &mut single_buf) else {
                continue;
            };
            let (Does::Copy { len, .. } | Does::Fill { len, .. }) = does[i] else {
                continue;
            };
            if len == 0 {
                return Err("nothing to read");
            }
            // Only materialize `ReadPiece`s for chosen steps.
            let mut pieces: Vec<ReadPiece> = Vec::with_capacity(segs.len());
            for &(seg_off, seg_src, seg_len) in segs {
                let source = match held.get(&(seg_src, seg_len)) {
                    Some(from) => Source::Shadow(from.clone(), seg_src),
                    None => match holder(&writers, seg_src, seg_src + seg_len) {
                        Ok(from) => {
                            held.insert((seg_src, seg_len), from.clone());
                            Source::Shadow(from, seg_src)
                        }
                        Err(why) => {
                            if touches(written, seg_src, seg_src + seg_len)
                                || writers.iter().any(|w| meet((w.from, w.to), (seg_src, seg_src + seg_len)))
                            {
                                return Err(why);
                            }
                            if !mapped(seg_src, seg_src + seg_len, &mut room) {
                                return Err("not all of what it reads is mapped");
                            }
                            let staged_at = *staged_places.entry((seg_src, seg_len)).or_insert_with(|| {
                                let at = staged_end;
                                staged_end = (staged_end + seg_len as u64).next_multiple_of(16);
                                at
                            });
                            Source::Staged(staged_at, seg_src)
                        }
                    },
                };
                pieces.push(ReadPiece {
                    source,
                    offset: seg_off as u64,
                    len: seg_len,
                });
            }
            let at = if pieces.len() == 1 {
                match &pieces[0].source {
                    Source::Shadow(from, seg_src) => {
                        let place = (Arc::as_ptr(from) as usize, *seg_src, len, level);
                        match places.get(&place) {
                            Some(&at) => at,
                            None => {
                                if end + len as u64 > BETWEEN {
                                    return Err("too much at once");
                                }
                                if !mapped(*seg_src, *seg_src + len, &mut room) {
                                    return Err("not all of what it reads is mapped");
                                }
                                let at = end;
                                end = (end + len as u64).next_multiple_of(256);
                                places.insert(place, at);
                                at
                            }
                        }
                    }
                    Source::Staged(_, seg_src) => {
                        let place = (0, *seg_src, len, level);
                        match places.get(&place) {
                            Some(&at) => at,
                            None => {
                                if end + len as u64 > BETWEEN {
                                    return Err("too much at once");
                                }
                                let at = end;
                                end = (end + len as u64).next_multiple_of(256);
                                places.insert(place, at);
                                at
                            }
                        }
                    }
                }
            } else {
                if end + len as u64 > BETWEEN {
                    return Err("too much at once");
                }
                for piece in &pieces {
                    if let Source::Shadow(_, seg_src) = &piece.source
                        && !mapped(*seg_src, *seg_src + piece.len, &mut room)
                    {
                        return Err("not all of what it reads is mapped");
                    }
                }
                let at = end;
                end = (end + len as u64).next_multiple_of(256);
                at
            };
            let to = match does[i] {
                Does::Copy { dst, .. } => {
                    let to = holder(&writers, dst, dst + len)?;
                    let rset = refreshes.get_or_insert_with(|| RefreshSet::from_runs(runs));
                    if rset.elsewhere(Arc::as_ptr(&to) as usize, dst, dst + len) {
                        return Err("another host copy of what it writes is read");
                    }
                    if !mapped(dst, dst + len, &mut room) {
                        return Err("not all of what it writes is mapped");
                    }
                    // What the steps behind it read of it is in that host
                    // copy, or nowhere.
                    writers.push(Writer {
                        shadow: Some(to.clone()),
                        from: dst,
                        to: dst + len,
                        copy: true,
                    });
                    held.clear();
                    To::Copy(to, dst)
                }
                _ => match fill_to(runs, steps[i]) {
                    Some(FillTo::Slot(heap, index)) => To::Slot(heap, index),
                    Some(FillTo::Buffer(buffer)) => To::Buffer(buffer),
                    None => continue,
                },
            };
            ops.push(Op {
                reads: pieces,
                len,
                at,
                level,
                to,
            });
        }
        spent.next("steps on the card: the list");
        let Some(mut own) = self.own_list() else {
            return Err("no host list or buffer for it");
        };
        if staged_end > 0 {
            let Some(staged_ptr) = self.ensure_staged(&mut own, staged_end) else {
                return Err("no staging buffer for it");
            };
            for (&(seg_src, seg_len), &staged_at) in &staged_places {
                // SAFETY: `staged` has at least `staged_end` mapped bytes.
                let out = unsafe { std::slice::from_raw_parts_mut((staged_ptr + staged_at as usize) as *mut u8, seg_len) };
                if !read_into(seg_src as *const u8, out) {
                    return Err("not all of what it reads is mapped");
                }
            }
        }
        // First every refresh, then the writes: the bytes compared with are
        // the title's. (One step for each host copy, over all that is read of
        // it: a flush brings a copy up to date from the first range asked for
        // to the last, whatever the steps.)
        let mut spans: ByCopy<(Arc<Shadow>, usize, usize)> = ByCopy::default();
        for op in &ops {
            for piece in &op.reads {
                if let Source::Shadow(shadow, at) = &piece.source {
                    let span = spans
                        .entry(Arc::as_ptr(shadow) as usize)
                        .or_insert_with(|| (shadow.clone(), *at, *at + piece.len));
                    (span.1, span.2) = (span.1.min(*at), span.2.max(*at + piece.len));
                }
            }
            if let To::Copy(to, dst) = &op.to {
                let span = spans
                    .entry(Arc::as_ptr(to) as usize)
                    .or_insert_with(|| (to.clone(), *dst, *dst + op.len));
                (span.1, span.2) = (span.1.min(*dst), span.2.max(*dst + op.len));
            }
        }
        for (shadow, from, to) in spans.into_values() {
            shadow.refresh(&own.list, from as u64, (to - from) as u64);
        }
        let mut copied = Vec::new();
        for op in &ops {
            if let To::Copy(to, dst) = &op.to {
                to.gpu_writes(&own.list, *dst as u64, op.len as u64);
                copied.push((to.clone(), *dst, *dst + op.len));
            }
        }
        #[cfg(feature = "diagnostics")]
        own.list.note_op(|| format!("{} copies of title memory done on the card, {end} bytes (OATHKEEP_CARD_STEPS)", ops.len()));
        let staged_buf = own.staged.as_ref().map(|(b, _, _)| b.clone());
        let levels = ops.iter().map(|op| op.level).max().unwrap_or(0);
        let mut by_level: Vec<Vec<usize>> = vec![Vec::new(); levels as usize + 1];
        for (idx, op) in ops.iter().enumerate() {
            by_level[op.level as usize].push(idx);
        }
        for (lvl_idx, bucket) in by_level.iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            let level = lvl_idx as u32;
            let mut reads = Vec::with_capacity(bucket.len());
            let mut writes = Vec::new();
            for &op_idx in bucket {
                let op = &ops[op_idx];
                let len = op.len as u64;
                if op.reads.len() == 1 {
                    match &op.reads[0].source {
                        Source::Shadow(from, seg_src) => {
                            if places
                                .remove(&(Arc::as_ptr(from) as usize, *seg_src, op.len, level))
                                .is_some()
                            {
                                reads.push((&from.buffer, from.offset(*seg_src as u64), op.at, len));
                            }
                        }
                        Source::Staged(staged_at, seg_src) => {
                            if places.remove(&(0, *seg_src, op.len, level)).is_some()
                                && let Some(buf) = &staged_buf
                            {
                                reads.push((buf, *staged_at, op.at, len));
                            }
                        }
                    }
                } else {
                    for piece in &op.reads {
                        match &piece.source {
                            Source::Shadow(from, seg_src) => {
                                reads.push((
                                    &from.buffer,
                                    from.offset(*seg_src as u64),
                                    op.at + piece.offset,
                                    piece.len as u64,
                                ));
                            }
                            Source::Staged(staged_at, _) => {
                                if let Some(buf) = &staged_buf {
                                    reads.push((buf, *staged_at, op.at + piece.offset, piece.len as u64));
                                }
                            }
                        }
                    }
                }
                match &op.to {
                    To::Copy(to, dst) => writes.push((&to.buffer, to.offset(*dst as u64), op.at, len)),
                    To::Buffer(buffer) => writes.push((buffer, 0, op.at, len)),
                    To::Slot(..) => {}
                }
            }
            record(level == 0, &own.list.list, &own.between, &reads, &writes);
        }
        own.list.close();
        // SAFETY: a live buffer.
        let gpu = unsafe { own.between.GetGPUVirtualAddress() };
        spent.next("steps on the card: the slots");
        // (11,000 slots a frame in a street, two descriptors each: on
        // several threads, a slot always on the same one, in the steps'
        // order.)
        let parts = if ops.len() < 256 { 1 } else { 8 };
        crate::land::each_under(parts, &|part| {
            for op in &ops {
                if let To::Slot(heap, index) = &op.to
                    && *index as usize / 4 % parts == part
                {
                    // (A heap that is gone: nothing can read the slot.)
                    crate::descriptor::constant_slot(*heap, *index, &own.between, gpu + op.at, op.len as u32);
                }
            }
        });
        let copies = ops.iter().filter(|op| matches!(op.to, To::Copy(..))).count();
        DONE[0].fetch_add((ops.len() - copies) as u64, Ordering::Relaxed);
        DONE[1].fetch_add(copies as u64, Ordering::Relaxed);
        DONE[2].fetch_add(ops.iter().map(|op| op.len as u64).sum(), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        noted(&ops);
        spent.end();
        Ok(Some(Taken {
            own,
            steps: chosen.iter().map(|&(i, _)| (steps[i].1, steps[i].2)).collect(),
            copied,
        }))
    }
}
