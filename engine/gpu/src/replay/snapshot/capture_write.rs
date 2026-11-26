//! One frame written down: what its submissions run, what the card's
//! memory held at its start, and what the CPU put into memory both see
//! before each submission.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::os::windows::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::gpu::{self, Seen, Worker};
use super::super::recorder::{At, Heap, Ours, Recorder, Res, lock, not_recorded_calls, read};
use super::super::states::{COMMON, COPY_SOURCE, Change, States};
use super::super::wire::{
    self, ALL, Cmd, Content, Delta, Loc, MISSING, NONE, Place, Slot, Va, ZERO, slot,
};

use super::io::{Piece, copy_out, read_sub, runs_of, write_at, changed_runs, zero};
use super::reads::{CpuBuffer, Reads};
use super::*;
use super::capture::Capture;
use super::io::resolved;

impl Capture {
    /// What changed in the buffers the CPU sees, of what a submission reads
    /// of them, since they were last looked at (the first time: what is not
    /// zero).
    fn changed(&mut self, r: &Recorder, reads: &Reads) -> Result<Vec<Delta>, Fault> {
        self.new_buffers(r)?;
        {
            let held = lock(&r.held);
            self.buffers.retain(|b| held.contains_key(&b.id));
        }
        let parts: Vec<(usize, usize, usize)> = self
            .buffers
            .iter()
            .enumerate()
            .flat_map(|(i, b)| {
                reads
                    .pages(b.id, b.size, self.viewed.contains(&b.id))
                    .into_iter()
                    .map(move |(from, to)| (i, from, to))
            })
            .collect();
        if parts.iter().any(|p| self.buffers[p.0].through.is_some()) {
            let (buffers, blocks) = (&self.buffers, &self.blocks);
            self.worker.run(|list| {
                for &(i, from, to) in &parts {
                    let b = &buffers[i];
                    if let Some((block, at)) = b.through {
                        // SAFETY: an open list and live buffers.
                        unsafe {
                            list.CopyBufferRegion(
                                &blocks[block].0,
                                at + from as u64,
                                &*b.res,
                                from as u64,
                                (to - from) as u64,
                            )
                        };
                    }
                }
            })?;
        }
        let chunks: Vec<(usize, usize, usize)> = parts
            .iter()
            .flat_map(|&(i, from, to)| {
                (from..to)
                    .step_by(CHUNK)
                    .map(move |at| (i, at, CHUNK.min(to - at)))
            })
            .collect();
        // Each chunk is one thread's; threads share what is kept by parts.
        struct Kept(*mut u8);
        // SAFETY: parts of it are used by one thread each.
        unsafe impl Sync for Kept {}
        let kept: Vec<Kept> = self
            .buffers
            .iter_mut()
            .map(|b| Kept(b.kept.as_mut_ptr()))
            .collect();
        let next = AtomicUsize::new(0);
        let out: std::sync::Mutex<Vec<Delta>> = std::sync::Mutex::default();
        let failed: std::sync::Mutex<Option<std::io::Error>> = std::sync::Mutex::default();
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get() / 2)
            .clamp(1, 16)
            .min(chunks.len().max(1));
        let (buffers, file, end) = (&self.buffers, &self.deltas, &self.delta_at);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(b, at, len)) = chunks.get(i) else {
                            break;
                        };
                        // SAFETY: a chunk of the buffer's memory (it may
                        // change meanwhile: what is kept is what was read)
                        // and of what is kept, which no other thread has.
                        let runs = unsafe {
                            changed_runs(buffers[b].live.add(at), kept[b].0.add(at), len)
                        };
                        for (from, to) in runs {
                            // SAFETY: as above.
                            let bytes = unsafe {
                                std::slice::from_raw_parts(kept[b].0.add(at + from), to - from)
                            };
                            let file_at = end.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                            if let Err(e) = write_at(file, bytes, file_at) {
                                lock(&failed).get_or_insert(e);
                            }
                            mine.push(Delta {
                                res: buffers[b].id,
                                at: (at + from) as u64,
                                len: bytes.len() as u64,
                                file: file_at,
                            });
                        }
                    }
                    lock(&out).extend(mine);
                });
            }
        });
        if let Some(e) = lock(&failed).take() {
            return Err(e.into());
        }
        let mut out = out.into_inner().unwrap_or_else(|e| e.into_inner());
        out.sort_by_key(|d| (d.res, d.at));
        Ok(out)
    }

    pub(super) fn submit(
        &mut self,
        r: &Recorder,
        _queue: &ID3D12CommandQueue,
        lists: &[Option<Arc<Vec<u8>>>],
    ) -> Result<(), Fault> {
        let first = !self.started;
        if first {
            self.start(r)?;
        } else {
            self.timed("waiting for the card", |c| Ok(c.worker.idle()?))?;
        }
        // Descriptors first: what they name is looked at below.
        let mut written = std::mem::take(&mut *lock(&r.writes));
        let mut views = Vec::new();
        if first {
            let heaps: Vec<Arc<Heap>> = read(&r.descriptors).visible().cloned().collect();
            for h in heaps {
                let runs = runs_of(r, &h.all());
                self.viewed
                    .extend(runs.iter().flat_map(|(_, _, s)| [s.res, s.counter]));
                self.heaps.push((h, runs));
            }
        } else {
            written.sort_unstable();
            let mut seen = HashSet::new();
            for (heap, at, n) in written {
                let h = match self.heaps.iter().find(|(h, _)| h.id == heap) {
                    Some((h, _)) => h.clone(),
                    None => {
                        let found = read(&r.descriptors)
                            .visible()
                            .find(|h| h.id == heap)
                            .cloned();
                        let Some(h) = found else { continue };
                        self.heaps.push((h.clone(), Vec::new()));
                        h
                    }
                };
                for i in at..at + n {
                    if seen.insert((heap, i)) {
                        let s = resolved(r, h.get(i));
                        self.viewed.extend([s.res, s.counter]);
                        views.push((heap, i, s));
                    }
                }
            }
        }
        let mut reads = Reads::default();
        for l in lists.iter().flatten() {
            reads.note(&Cmd::all(l)?);
        }
        let deltas = self.timed("what the CPU changed", |c| c.changed(r, &reads))?;
        let mut numbers = Vec::new();
        for l in lists {
            numbers.push(match l {
                None => {
                    r.problem(
                        "a list was submitted that was not recorded whole: its work is missing",
                    );
                    MISSING
                }
                Some(commands) => match self
                    .lists
                    .iter()
                    .position(|have| Arc::ptr_eq(have, commands))
                {
                    Some(i) => i as u32,
                    None => {
                        self.lists.push(commands.clone());
                        self.lists.len() as u32 - 1
                    }
                },
            });
        }
        self.submits.push(wire::Submit {
            frame: self.frame,
            lists: numbers,
            deltas,
            views,
        });
        Ok(())
    }

    pub(super) fn present(&mut self, r: &Recorder, texture: &ID3D12Resource, state: u32) -> Result<(), Fault> {
        self.worker.idle()?;
        // SAFETY: a live texture.
        let desc = unsafe { texture.GetDesc() };
        let (f, total) = gpu::footprints(&self.worker.device, &desc, 0, 1);
        let (back, data) = gpu::buffer(
            &self.worker.device,
            D3D12_HEAP_TYPE_READBACK,
            total,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        self.worker
            .run(|list| read_sub(list, texture, 0, state, &back, f[0]))?;
        // SAFETY: the buffer's mapping; the copy into it has run.
        let bytes = unsafe { std::slice::from_raw_parts(data, total as usize) };
        write_at(&self.present_file, bytes, self.present_at)?;
        let id = r.resource(texture.as_raw());
        let (width, height, pitch) = (desc.Width as u32, desc.Height, f[0].Footprint.RowPitch);
        self.presents.push(wire::Present {
            submits: self.submits.len() as u32,
            res: id,
            width,
            height,
            format: desc.Format.0,
            pitch,
            file: self.present_at,
        });
        self.present_at += total;
        match gpu::rgb(width, height, pitch, desc.Format.0, bytes) {
            Some(rgb) => std::fs::write(
                r.folder.join(format!("frame-{}.png", self.frame)),
                crate::diagnostics::capture::png(width, height, &rgb),
            )?,
            None => r.problem("the presented format is not shown in a PNG"),
        }
        self.frame += 1;
        Ok(())
    }

    /// Writes `frame.bin`.
    pub(super) fn finish(&mut self, r: &Recorder) -> Result<(), Fault> {
        let mut resources: Vec<wire::Resource> = Vec::new();
        {
            let names = lock(&r.names);
            for res in read(&r.resources).values() {
                let state = match (res.is_buffer(), res.late) {
                    (true, _) => vec![COMMON],
                    (_, true) => vec![res.made_in as u32],
                    _ => self.states.get(res.id).unwrap_or(vec![COMMON]),
                };
                resources.push(wire::Resource {
                    id: res.id,
                    place: res.place.clone(),
                    desc: res.desc.clone(),
                    made_in: res.made_in,
                    clear: res.clear,
                    state,
                    idle: self.idle.contains(&res.id),
                    late: res.late,
                    name: names.get(&res.id).cloned().unwrap_or_default(),
                });
            }
        }
        resources.sort_by_key(|res| res.id);
        let mut heaps: Vec<wire::Heap> = {
            let all = lock(&r.heaps);
            let used: HashSet<u32> = resources
                .iter()
                .filter_map(|res| {
                    if let Place::Placed { heap, .. } = res.place {
                        Some(heap)
                    } else {
                        None
                    }
                })
                .collect();
            used.iter().filter_map(|id| all.get(id).cloned()).collect()
        };
        heaps.sort_by_key(|h| h.id);
        // What the lists name.
        let (mut psos, mut roots, mut signatures, mut queries) = (
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
        );
        for l in &self.lists {
            for c in Cmd::all(l)? {
                match c {
                    Cmd::Pipeline { pso } => _ = psos.insert(pso),
                    Cmd::RootSignature { root, .. } => _ = roots.insert(root),
                    Cmd::Indirect { signature, .. } => _ = signatures.insert(signature),
                    Cmd::BeginQuery { heap, .. }
                    | Cmd::EndQuery { heap, .. }
                    | Cmd::ResolveQuery { heap, .. } => _ = queries.insert(heap),
                    _ => {}
                }
            }
        }
        let pipelines: Vec<wire::Pipeline> = {
            let all = lock(&r.pipelines);
            let names = lock(&r.names);
            let mut out: Vec<wire::Pipeline> = psos
                .iter()
                .filter_map(|id| {
                    let parts = all.get(id)?;
                    Some(wire::Pipeline {
                        id: *id,
                        parts: parts.to_vec(),
                        name: names.get(id).cloned().unwrap_or_default(),
                    })
                })
                .collect();
            out.sort_by_key(|p| p.id);
            out
        };
        roots.extend(pipelines.iter().flat_map(|p| &p.parts).filter_map(|p| {
            if let wire::Part::Root { root } = p {
                Some(*root)
            } else {
                None
            }
        }));
        let signatures: Vec<wire::Signature> = {
            let all = lock(&r.signatures);
            signatures
                .iter()
                .filter_map(|id| all.get(id).cloned())
                .collect()
        };
        roots.extend(signatures.iter().map(|s| s.root));
        roots.remove(&NONE);
        let roots: Vec<(u32, wire::Blob)> = {
            let all = lock(&r.roots);
            roots
                .iter()
                .filter_map(|id| Some((*id, wire::Blob(all.get(id)?.to_vec()))))
                .collect()
        };
        for ((what, before), (_, now)) in self.calls.iter().zip(not_recorded_calls()) {
            if now > *before {
                r.problem_times(what, now - before);
            }
        }
        let (adapter, vendor, device) = gpu::adapter(&self.worker.device);
        let mut notes = r.problems();
        let bytes: u64 = self
            .contents
            .iter()
            .filter(|c| c.file != ZERO)
            .map(|c| c.len)
            .sum();
        notes.push(format!(
            "{} submissions, {} lists, {} resources, {} pipelines; {} MB of the card's memory, {} MB of changes by the CPU; {:.1} s: {}",
            self.submits.len(),
            self.lists.len(),
            resources.len(),
            pipelines.len(),
            bytes >> 20,
            self.delta_at.load(Ordering::Relaxed) >> 20,
            self.began.elapsed().as_secs_f64(),
            self.took.iter().map(|(what, d)| format!("{what} {:.1} s", d.as_secs_f64())).collect::<Vec<_>>().join(", ")
        ));
        for n in &notes {
            tracing::info!("frame recorder: {n}");
        }
        let frame = wire::Frame {
            adapter,
            vendor,
            device,
            notes: notes.clone(),
            heaps,
            resources,
            roots,
            codes: lock(&r.codes).all.clone(),
            pipelines,
            signatures,
            descriptor_heaps: self
                .heaps
                .iter()
                .map(|(h, runs)| wire::DescriptorHeap {
                    id: h.id,
                    kind: h.kind,
                    count: h.count,
                    flags: h.flags,
                    runs: runs.clone(),
                })
                .collect(),
            query_heaps: {
                let all = lock(&r.queries);
                queries
                    .iter()
                    .filter_map(|id| all.get(id).cloned())
                    .collect()
            },
            lists: self
                .lists
                .iter()
                .map(|l| wire::List {
                    kind: D3D12_COMMAND_LIST_TYPE_DIRECT.0,
                    commands: wire::Blob(l.to_vec()),
                })
                .collect(),
            submits: std::mem::take(&mut self.submits),
            contents: std::mem::take(&mut self.contents),
            presents: std::mem::take(&mut self.presents),
        };
        frame.write(&r.folder.join("frame.bin"))?;
        std::fs::write(r.folder.join("notes.txt"), notes.join("\n"))?;
        tracing::info!(folder = %r.folder.display(), "frame recorder: the frame is captured");
        Ok(())
    }
}
