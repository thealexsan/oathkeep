//! Content upload, initial state transitions, and inter-submission CPU deltas.

use super::*;

impl Player {
    /// Reads parts of files into upload memory and has `copy` record what
    /// is done with each.
    fn upload<T: Sync>(
        &mut self,
        parts: &[(T, u64, u64)],
        file: &File,
        mut copy: impl FnMut(&Player, &ID3D12GraphicsCommandList, &[(&T, u64, u64)], &ID3D12Resource),
    ) -> Result<()> {
        // Batches of parts with their place in the buffer.
        let mut batches: Vec<Vec<(usize, u64)>> = vec![Vec::new()];
        let (mut at, mut most) = (0u64, 0u64);
        for (i, (_, len, _)) in parts.iter().enumerate() {
            at = at.next_multiple_of(512);
            if at + len > STAGING && at > 0 {
                batches.push(Vec::new());
                at = 0;
            }
            if let Some(b) = batches.last_mut() {
                b.push((i, at));
            }
            at += len;
            most = most.max(at);
        }
        if parts.is_empty() {
            return Ok(());
        }
        let mut stagings: Vec<(ID3D12Resource, usize, u64)> = Vec::new();
        for _ in 0..batches.len().min(2) {
            let (b, data) = gpu::buffer(
                &self.device,
                D3D12_HEAP_TYPE_UPLOAD,
                most,
                D3D12_RESOURCE_FLAG_NONE,
            )?;
            stagings.push((b, data as usize, 0));
        }
        for (n, batch) in batches.iter().enumerate() {
            let k = n % stagings.len();
            self.worker.wait(stagings[k].2)?;
            let map = stagings[k].1;
            let next = AtomicUsize::new(0);
            let failed: std::sync::Mutex<Option<std::io::Error>> = std::sync::Mutex::default();
            std::thread::scope(|scope| {
                for _ in 0..8.min(batch.len()) {
                    scope.spawn(|| {
                        while let Some(&(i, at)) = batch.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let (_, len, from) = &parts[i];
                            // SAFETY: this part's own bytes of the mapping.
                            let to = unsafe {
                                std::slice::from_raw_parts_mut(
                                    (map + at as usize) as *mut u8,
                                    *len as usize,
                                )
                            };
                            if *from == ZERO {
                                to.fill(0);
                            } else if let Err(e) = read_at(file, to, *from) {
                                failed
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .get_or_insert(e);
                            }
                        }
                    });
                }
            });
            if let Some(e) = failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
                return Err(e).context("the capture's contents");
            }
            let list: Vec<(&T, u64, u64)> = batch
                .iter()
                .map(|&(i, at)| (&parts[i].0, parts[i].1, at))
                .collect();
            let buffer = stagings[k].0.clone();
            // The worker is another part of the player than `copy` uses.
            let other = Worker::new(&self.device, &self.worker.queue)?;
            let mut worker = std::mem::replace(&mut self.worker, other);
            let sent = worker.send(|l| copy(self, l, &list, &buffer));
            self.worker = worker;
            stagings[k].2 = sent?;
        }
        self.worker.idle()?;
        Ok(())
    }

    pub(super) fn load_contents(&mut self) -> Result<()> {
        // Idle aliases first: a change of their state may write memory the
        // one in use holds.
        let known: HashSet<u32> = self.frame.contents.iter().map(|c| c.res).collect();
        let mut early = Vec::new();
        let mut into = Vec::new();
        for r in &self.frame.resources {
            let (Some(o), Some(state)) = (self.objects.get(&r.id), self.start.get(&r.id)) else {
                continue;
            };
            let loaded = !r.idle && known.contains(&r.id);
            gpu::moves(
                &o.res,
                &[COMMON],
                if loaded { &[COPY_DEST] } else { state },
                if loaded { &mut into } else { &mut early },
            );
        }
        let textures: Vec<(Content, u64, u64)> = self
            .frame
            .contents
            .iter()
            .filter(|c| self.objects.contains_key(&c.res))
            .map(|c| (c.clone(), c.len, c.file))
            .collect();
        let missing = self.frame.contents.len() - textures.len();
        self.worker.run(|list| {
            // SAFETY: an open list; live resources.
            unsafe {
                for all in early.chunks(1000) {
                    list.ResourceBarrier(all);
                }
                list.ResourceBarrier(&[alias()]);
                for all in into.chunks(1000) {
                    list.ResourceBarrier(all);
                }
            }
        })?;
        let file = File::open(self.folder.join("contents.bin"))?;
        self.upload(&textures, &file, |player, list, parts, staging| {
            for (c, len, at) in parts {
                let Some(o) = player.objects.get(&c.res) else {
                    continue;
                };
                let r = &player.frame.resources[o.index];
                // SAFETY: an open list; live resources.
                unsafe {
                    if r.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0 {
                        list.CopyBufferRegion(&o.res, c.first, staging, *at, *len);
                        continue;
                    }
                    let (all, _) = gpu::footprints(
                        &player.device,
                        &gpu::to_desc(&r.desc),
                        c.first as u32,
                        c.count,
                    );
                    for (i, mut f) in all.into_iter().enumerate() {
                        f.Offset += at;
                        let to = gpu::sub_location(&o.res, c.first as u32 + i as u32);
                        list.CopyTextureRegion(
                            &to,
                            0,
                            0,
                            0,
                            &gpu::placed_location(staging, f),
                            None,
                        );
                    }
                }
            }
        })?;
        // To the states of the frame's start.
        let mut back = Vec::new();
        for r in &self.frame.resources {
            let (Some(o), Some(state)) = (self.objects.get(&r.id), self.start.get(&r.id)) else {
                continue;
            };
            if !r.idle && known.contains(&r.id) {
                gpu::moves(&o.res, &[COPY_DEST], state, &mut back);
            }
        }
        self.worker.run(|list| {
            for all in back.chunks(1000) {
                // SAFETY: an open list; live resources.
                unsafe { list.ResourceBarrier(all) };
            }
        })?;
        if missing > 0 {
            self.note(format!(
                "contents of {missing} parts of resources that could not be made are left out"
            ));
        }
        // What the CPU had put into the memory both see by the first
        // submission: part of the start of the frame.
        let file = File::open(self.folder.join("deltas.bin"))?;
        let named = self.named_buffers();
        let of = |s: &wire::Submit| {
            s.deltas
                .iter()
                .filter(|d| named.contains(&d.res))
                .cloned()
                .collect::<Vec<Delta>>()
        };
        // In memory the card cannot write, bytes looked at first when a
        // later submission read them can stand there from the start.
        let upload: HashSet<u32> = self
            .frame
            .resources
            .iter()
            .filter(|r| matches!(&r.place, Place::Committed { props, .. } if props.kind == D3D12_HEAP_TYPE_UPLOAD.0))
            .map(|r| r.id)
            .collect();
        let mut first = of(&self.frame.submits[0]);
        let mut seen: HashSet<(u32, u64)> = HashSet::new();
        let pages = |d: &Delta| {
            (d.at / 4096..(d.at + d.len).div_ceil(4096))
                .map(|p| (d.res, p))
                .collect::<Vec<_>>()
        };
        first.iter().for_each(|d| seen.extend(pages(d)));
        let mut rest: Vec<Vec<Delta>> = vec![Vec::new()];
        for s in self.frame.submits.iter().skip(1) {
            let (early, late): (Vec<Delta>, Vec<Delta>) = of(s).into_iter().partition(|d| {
                upload.contains(&d.res) && pages(d).iter().all(|p| !seen.contains(p))
            });
            early.iter().for_each(|d| seen.extend(pages(d)));
            first.extend(early);
            rest.push(late);
        }
        let first: Vec<(Delta, u64, u64)> = first
            .into_iter()
            .map(|d| (d.clone(), d.len, d.file))
            .collect();
        self.upload(&first, &file, |player, list, parts, staging| {
            for (d, len, at) in parts {
                if let Some(o) = player.objects.get(&d.res) {
                    // SAFETY: an open list; live buffers.
                    unsafe { list.CopyBufferRegion(&o.res, d.at, staging, *at, *len) };
                }
            }
        })?;
        // The later ones wait in a buffer of their own.
        let mut later: Vec<((u64, Delta), u64, u64)> = Vec::new();
        let mut total = 0u64;
        self.delta_parts = vec![Vec::new(); self.frame.submits.len()];
        for (i, all) in rest.into_iter().enumerate() {
            for d in all {
                later.push(((total, d.clone()), d.len, d.file));
                total += d.len;
                self.delta_parts[i].push((d, total - later.last().map_or(0, |l| l.1)));
            }
        }
        if total > 0 {
            let (buffer, _) = gpu::buffer(
                &self.device,
                D3D12_HEAP_TYPE_DEFAULT,
                total,
                D3D12_RESOURCE_FLAG_NONE,
            )?;
            self.deltas = Some(buffer.clone());
            self.upload(&later, &file, |_, list, parts, staging| {
                for ((to, _), len, at) in parts {
                    // SAFETY: an open list; live buffers.
                    unsafe { list.CopyBufferRegion(&buffer, *to, staging, *at, *len) };
                }
            })?;
        }
        Ok(())
    }

    /// The buffers the frame names, in commands or descriptors: what the CPU
    /// writes to others (buffers it has results fetched to) is of no use.
    fn named_buffers(&self) -> HashSet<u32> {
        let mut named = HashSet::new();
        let mut va = |va: &Va| {
            if let Va::At { res, .. } = va {
                named.insert(*res);
            }
        };
        for c in self.commands.iter().flatten() {
            match c {
                Cmd::RootView { va: v, .. } => va(v),
                Cmd::IndexBuffer {
                    view: Some((v, _, _)),
                } => va(v),
                Cmd::VertexBuffers {
                    views: Some(all), ..
                } => all.iter().for_each(|(v, _, _)| va(v)),
                Cmd::WriteImmediate { all } => all.iter().for_each(|(v, _, _)| va(v)),
                _ => {}
            }
        }
        for c in self.commands.iter().flatten() {
            match c {
                Cmd::CopyBuffer { dst, src, .. } | Cmd::CopyResource { dst, src } => {
                    named.extend([*dst, *src])
                }
                Cmd::CopyTexture { dst, src, .. } => {
                    for Loc::Sub { res, .. } | Loc::Placed { res, .. } in [dst, src] {
                        named.insert(*res);
                    }
                }
                Cmd::Indirect { args, count, .. } => named.extend([*args, *count]),
                Cmd::ResolveQuery { dst, .. } => _ = named.insert(*dst),
                _ => {}
            }
        }
        for h in &self.frame.descriptor_heaps {
            named.extend(h.runs.iter().flat_map(|(_, _, s)| [s.res, s.counter]));
        }
        for s in &self.frame.submits {
            named.extend(s.views.iter().flat_map(|(_, _, s)| [s.res, s.counter]));
        }
        named
    }
}
