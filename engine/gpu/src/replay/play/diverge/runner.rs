//! Unit execution, readback hashing, and master-copy restore/commit for divergence search.

use super::super::*;
use super::{PIECE, Part, Pieces, Reader, Unit};

impl Player {
    /// The states of textures at the frame's start.
    pub(in super::super) fn start_states(&self) -> States {
        let mut states = States::default();
        for (id, all) in &self.start {
            let Some(o) = self.objects.get(id) else {
                continue;
            };
            let flags = self.frame.resources[o.index].desc.flags;
            let decays = flags & D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS.0 != 0;
            states.add(*id, o.subs, all[0], decays);
            for (i, s) in all.iter().enumerate().filter(|_| all.len() > 1) {
                states.change(&states::Change {
                    res: *id,
                    sub: i as u32,
                    after: *s,
                    promoted: false,
                    alias: false,
                });
            }
        }
        states
    }

    pub(super) fn after(&self, unit: &Unit, states: &States) -> States {
        let mut after = states.clone();
        let mut all = Vec::new();
        for c in &unit.cmds[unit.from..unit.to] {
            states::changes(c, &mut all);
        }
        all.iter().for_each(|c| after.change(c));
        after
    }

    pub(in super::super) fn reader(&self, ids: &[u32]) -> Result<Reader> {
        const BLOCK: u64 = 512 << 20;
        let mut parts = Vec::new();
        let mut sizes: Vec<u64> = vec![0];
        for id in ids {
            let o = &self.objects[id];
            let r = &self.frame.resources[o.index];
            let back = matches!(&r.place, Place::Committed { props, .. } if gpu::seen(props) == Seen::ReadBack);
            if r.desc.samples[0] > 1 || back {
                continue;
            }
            let is_buffer = r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0;
            let desc = gpu::to_desc(&r.desc);
            for sub in 0..o.subs {
                let (sub, len) = if is_buffer {
                    (None, r.desc.width)
                } else {
                    let (f, total) = gpu::footprints(&self.device, &desc, sub, 1);
                    (Some((sub, f[0])), total)
                };
                let last = sizes.len() - 1;
                let at = sizes[last].next_multiple_of(512);
                let (block, at) = if at + len > BLOCK && at > 0 {
                    sizes.push(0);
                    (last + 1, 0)
                } else {
                    (last, at)
                };
                sizes[block] = at + len;
                parts.push(Part {
                    id: *id,
                    sub,
                    block,
                    at,
                    len,
                });
            }
        }
        let mut blocks = Vec::new();
        for size in sizes {
            let (b, data) = gpu::buffer(
                &self.device,
                D3D12_HEAP_TYPE_READBACK,
                size,
                D3D12_RESOURCE_FLAG_NONE,
            )?;
            blocks.push((b, data as usize));
        }
        Ok(Reader { blocks, parts })
    }

    /// Runs the unit once; resources are as at its start.
    pub(super) fn go(&mut self, unit: &Unit, wait: bool) -> Result<()> {
        let mut lists = Vec::new();
        if unit.delta
            && let Some(parts) = self.delta_parts.get(unit.submit).filter(|p| !p.is_empty())
            && let Some(from) = &self.deltas
        {
            let l = self.plain_list()?;
            for (d, at) in parts {
                if let Some(o) = self.objects.get(&d.res) {
                    // SAFETY: an open list; live buffers.
                    unsafe { l.1.CopyBufferRegion(&o.res, d.at, from, *at, d.len) };
                }
            }
            // SAFETY: as above.
            unsafe { l.1.Close()? };
            lists.push(l);
        }
        let l = self.plain_list()?;
        // What the commands before have set, without what they do.
        let before: Vec<Cmd> = unit.cmds[..unit.from]
            .iter()
            .filter(|c| {
                !c.works()
                    && !matches!(
                        c,
                        Cmd::Barriers { .. } | Cmd::BeginQuery { .. } | Cmd::EndQuery { .. }
                    )
            })
            .cloned()
            .collect();
        self.translate(&l.1, &before, unit.submit, None, false);
        self.translate(
            &l.1,
            &unit.cmds[unit.from..unit.to],
            unit.submit,
            None,
            wait,
        );
        // SAFETY: an open list.
        unsafe { l.1.Close()? };
        lists.push(l);
        for l in &lists {
            // SAFETY: a closed list; waited for before it goes.
            unsafe { self.worker.queue.ExecuteCommandLists(&[Some(l.1.cast()?)]) };
            self.worker.idle().context("the card was lost")?;
        }
        Ok(())
    }

    /// Hashes of all parts' pieces, as memory is now (in `states`).
    pub(in super::super) fn hashes_now(
        &mut self,
        reader: &Reader,
        states: &States,
    ) -> Result<Vec<Vec<u64>>> {
        let objects = &self.objects;
        for block in 0..reader.blocks.len() {
            let parts: Vec<&Part> = reader.parts.iter().filter(|p| p.block == block).collect();
            let (mut before, mut after) = (Vec::new(), Vec::new());
            let mut seen = HashSet::new();
            for p in parts.iter().filter(|p| p.sub.is_some()) {
                if seen.insert(p.id) {
                    let state = states.get(p.id).unwrap_or(vec![COMMON]);
                    let to: Vec<u32> = state
                        .iter()
                        .map(|&s| if s & COPY_SOURCE != 0 { s } else { COPY_SOURCE })
                        .collect();
                    gpu::moves(&objects[&p.id].res, &state, &to, &mut before);
                    gpu::moves(&objects[&p.id].res, &to, &state, &mut after);
                }
            }
            let to = &reader.blocks[block].0;
            self.worker.run(|list| {
                // SAFETY: an open list; live resources.
                unsafe {
                    barriers(list, &before);
                    for p in &parts {
                        let res = &objects[&p.id].res;
                        match p.sub {
                            None => list.CopyBufferRegion(to, p.at, res, 0, p.len),
                            Some((sub, mut f)) => {
                                f.Offset = p.at;
                                list.CopyTextureRegion(
                                    &gpu::placed_location(to, f),
                                    0,
                                    0,
                                    0,
                                    &gpu::sub_location(res, sub),
                                    None,
                                );
                            }
                        }
                    }
                    barriers(list, &after);
                }
            })?;
        }
        let next = AtomicUsize::new(0);
        let out: std::sync::Mutex<Vec<Vec<u64>>> =
            std::sync::Mutex::new(vec![Vec::new(); reader.parts.len()]);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    while let Some((i, p)) = {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        reader.parts.get(i).map(|p| (i, p))
                    } {
                        let all: Vec<u64> = reader
                            .bytes(p)
                            .chunks(PIECE)
                            .map(|c| {
                                let h = blake3::hash(c);
                                u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap_or_default())
                            })
                            .collect();
                        out.lock().unwrap_or_else(|e| e.into_inner())[i] = all;
                    }
                });
            }
        });
        Ok(out.into_inner().unwrap_or_else(|e| e.into_inner()))
    }

    /// Runs the unit `runs` times (memory is as at its start, and is put
    /// back between runs) and compares. Memory is as the last run left it.
    pub(super) fn trials(
        &mut self,
        reader: &Reader,
        unit: &Unit,
        states: &States,
        runs: u32,
        wait: bool,
    ) -> Result<Pieces> {
        let after = self.after(unit, states);
        let mut first: Option<Vec<Vec<u64>>> = None;
        let mut pieces = std::collections::BTreeSet::new();
        for run in 0..runs {
            if run > 0 {
                self.put_back_from(&after, states)?;
            }
            self.go(unit, wait)?;
            if runs == 1 {
                break;
            }
            let now = self.hashes_now(reader, &after)?;
            match &first {
                None => first = Some(now),
                Some(first) => {
                    for (p, (a, b)) in first.iter().zip(&now).enumerate() {
                        pieces.extend(
                            a.iter()
                                .zip(b)
                                .enumerate()
                                .filter(|(_, (a, b))| a != b)
                                .map(|(i, _)| (p, i)),
                        );
                    }
                }
            }
        }
        self.at = after;
        Ok(pieces.into_iter().collect())
    }

    /// Puts memory back to the start of a unit in `states`, from where the
    /// last run left it.
    pub(super) fn put_back(&mut self, states: &States) -> Result<()> {
        let from = self.at.clone();
        self.put_back_from(&from, states)
    }

    pub(super) fn put_back_from(&mut self, from: &States, to: &States) -> Result<()> {
        self.copy_masters(from, to, false)?;
        self.at = to.clone();
        Ok(())
    }

    /// What memory holds after the last run is the start of what follows.
    pub(super) fn commit(&mut self, unit: &Unit, states: &mut States) -> Result<()> {
        *states = self.after(unit, states);
        self.copy_masters(&states.clone(), states, true)?;
        Ok(())
    }

    /// Copies the masters over the resources (which are in `from` and go to
    /// `to`), or the resources into the masters.
    pub(super) fn copy_masters(&mut self, from: &States, to: &States, save: bool) -> Result<()> {
        let known: HashMap<u32, &ID3D12Resource> =
            self.masters.iter().map(|(id, m)| (*id, m)).collect();
        let (mut before, mut after, mut plain) = (Vec::new(), Vec::new(), Vec::new());
        let here = if save { COPY_SOURCE } else { COPY_DEST };
        for id in self.start.keys() {
            let Some(o) = self.objects.get(id) else {
                continue;
            };
            let (a, b) = (
                from.get(*id).unwrap_or(vec![COMMON]),
                to.get(*id).unwrap_or(vec![COMMON]),
            );
            let Some(m) = known.get(id) else {
                gpu::moves(&o.res, &a, &b, &mut plain);
                continue;
            };
            gpu::moves(&o.res, &a, &[here], &mut before);
            gpu::moves(&o.res, &[here], &b, &mut after);
            if save {
                before.push(gpu::transition(m, ALL, COPY_SOURCE, COPY_DEST));
                after.push(gpu::transition(m, ALL, COPY_DEST, COPY_SOURCE));
            }
        }
        let objects = &self.objects;
        self.worker.run(|list| {
            // SAFETY: an open list; live resources of one description.
            unsafe {
                barriers(list, &plain);
                barriers(list, &before);
                for (id, m) in &known {
                    if save {
                        list.CopyResource(*m, &objects[id].res);
                    } else {
                        list.CopyResource(&objects[id].res, *m);
                    }
                }
                barriers(list, &after);
            }
        })?;
        Ok(())
    }

    /// The masters hold another state than the frame's start now.
    pub(super) fn restore_all(&mut self) -> Result<()> {
        self.note("the player's copies are no longer of the frame's start: load it again".into());
        Ok(())
    }
}

impl Reader {
    pub(super) fn bytes(&self, p: &Part) -> &[u8] {
        // SAFETY: a part of a mapped block; copies into it have run.
        unsafe {
            std::slice::from_raw_parts(
                (self.blocks[p.block].1 + p.at as usize) as *const u8,
                p.len as usize,
            )
        }
    }
}
