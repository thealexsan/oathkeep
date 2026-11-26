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

pub(super) struct Capture {
    pub(super) worker: Worker,
    pub(super) others: Vec<Worker>,
    pub(super) started: bool,
    pub(super) frame: u32,
    pub(super) states: States,
    pub(super) idle: HashSet<u32>,
    pub(super) heaps: Vec<(Arc<Heap>, Vec<(u32, u32, Slot)>)>,
    pub(super) contents: Vec<Content>,
    pub(super) buffers: Vec<CpuBuffer>,
    pub(super) known: HashSet<u32>,
    /// Resources descriptors name, or named during the frame.
    pub(super) viewed: HashSet<u32>,
    /// Read-back buffers for the slow ones: the buffer, its mapping, size,
    /// and how much of it is taken.
    pub(super) blocks: Vec<(ID3D12Resource, usize, u64, u64)>,
    pub(super) deltas: File,
    pub(super) delta_at: AtomicU64,
    pub(super) lists: Vec<Arc<Vec<u8>>>,
    pub(super) submits: Vec<wire::Submit>,
    pub(super) presents: Vec<wire::Present>,
    pub(super) present_file: File,
    pub(super) present_at: u64,
    pub(super) calls: Vec<(String, u64)>,
    pub(super) began: Instant,
    pub(super) took: BTreeMap<&'static str, std::time::Duration>,
}

impl Capture {
    pub(super) fn new(r: &Recorder) -> Result<Capture, Fault> {
        let _ours = Ours::new();
        let device = r.device().ok_or("no device")?;
        let queues = lock(&r.queues).clone();
        let direct = queues
            .iter()
            .find(|q| q.1 == D3D12_COMMAND_LIST_TYPE_DIRECT.0)
            .ok_or("no drawing queue")?;
        let mut others = Vec::new();
        for (queue, _) in queues.iter().filter(|q| q.0 != direct.0) {
            others.push(Worker::new(&device, queue)?);
        }
        Ok(Capture {
            worker: Worker::new(&device, &direct.0)?,
            others,
            started: false,
            frame: 0,
            states: States::default(),
            idle: HashSet::new(),
            heaps: Vec::new(),
            contents: Vec::new(),
            buffers: Vec::new(),
            known: HashSet::new(),
            viewed: HashSet::new(),
            blocks: Vec::new(),
            deltas: File::create(r.folder.join("deltas.bin"))?,
            delta_at: AtomicU64::new(0),
            lists: Vec::new(),
            submits: Vec::new(),
            presents: Vec::new(),
            present_file: File::create(r.folder.join("present.bin"))?,
            present_at: 0,
            calls: not_recorded_calls(),
            began: Instant::now(),
            took: BTreeMap::new(),
        })
    }

    pub(super) fn timed<T>(
        &mut self,
        what: &'static str,
        f: impl FnOnce(&mut Self) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        let began = Instant::now();
        let out = f(self);
        *self.took.entry(what).or_default() += began.elapsed();
        out
    }

    /// Before the first submission of the frame: what the card holds.
    pub(super) fn start(&mut self, r: &Recorder) -> Result<(), Fault> {
        self.began = Instant::now();
        r.watching.store(true, Ordering::SeqCst);
        self.started = true;
        self.worker.idle()?;
        for w in &mut self.others {
            w.idle()?;
        }
        let mut all: Vec<Arc<Res>> = Vec::new();
        {
            let live = read(&r.resources);
            let mut held = lock(&r.held);
            for res in live.values().filter(|res| !res.late) {
                let object = res.object();
                // SAFETY: a live resource, or one whose end waits for the
                // lock held here (`Recorder::gone`): the count tells which.
                let count = unsafe {
                    (Interface::vtable(&*object)
                        .base__
                        .base__
                        .base__
                        .base__
                        .AddRef)(object.as_raw())
                };
                if count <= 1 {
                    // Dying: the reference does not bring it back, and
                    // giving it back would end it a second time.
                    continue;
                }
                // SAFETY: the reference just taken. (The count does not tell
                // all that are dying: `Recorder::gone` takes those out.)
                held.insert(res.id, unsafe { ID3D12Resource::from_raw(object.as_raw()) });
                all.push(res.clone());
            }
        }
        all.sort_by_key(|res| res.id);
        self.states = lock(&r.states).clone();
        self.idle = self.find_idle(&all);
        self.timed("contents of the card's memory", |c| c.snapshot(r, &all))?;
        Ok(())
    }

    /// Of placed resources that overlap, those that are not the one used
    /// last.
    fn find_idle(&self, all: &[Arc<Res>]) -> HashSet<u32> {
        let mut heaps: HashMap<u32, Vec<(u64, u64, u64, u32)>> = HashMap::new();
        for res in all {
            if let Place::Placed { heap, offset } = res.place {
                // SAFETY: a plain query.
                let info = unsafe {
                    self.worker
                        .device
                        .GetResourceAllocationInfo(0, &[gpu::to_desc(&res.desc)])
                };
                let size = if info.SizeInBytes == u64::MAX {
                    res.desc.width
                } else {
                    info.SizeInBytes
                };
                let used = self.states.used.get(&res.id).copied().unwrap_or(0);
                heaps
                    .entry(heap)
                    .or_default()
                    .push((used, offset, offset + size, res.id));
            }
        }
        let mut idle = HashSet::new();
        for (_, mut list) in heaps {
            list.sort_by(|a, b| b.cmp(a));
            let mut taken: BTreeMap<u64, u64> = BTreeMap::new();
            for (_, from, to, id) in list {
                let overlaps = taken
                    .range(..to)
                    .next_back()
                    .is_some_and(|(_, &end)| end > from);
                if overlaps {
                    idle.insert(id);
                } else {
                    taken.insert(from, to);
                }
            }
        }
        idle
    }

    fn snapshot(&mut self, r: &Recorder, all: &[Arc<Res>]) -> Result<(), Fault> {
        let device = self.worker.device.clone();
        let mut pieces: Vec<Piece> = Vec::new();
        for res in all.iter().filter(|res| res.seen == Seen::No) {
            if matches!(res.place, Place::BackBuffer {}) || self.idle.contains(&res.id) {
                continue;
            }
            if res.is_buffer() {
                let mut at = 0;
                while at < res.desc.width {
                    let len = PIECE.min(res.desc.width - at);
                    pieces.push(Piece {
                        res: res.clone(),
                        first: at,
                        count: 1,
                        len,
                        at: 0,
                    });
                    at += len;
                }
                continue;
            }
            if res.desc.samples[0] > 1 {
                r.problem("contents of multisampled textures are not in the capture");
                continue;
            }
            let desc = gpu::to_desc(&res.desc);
            let (_, total) = gpu::footprints(&device, &desc, 0, res.subs);
            if total == u64::MAX {
                r.problem("contents of a texture without a copyable layout are not in the capture");
            } else if total <= STAGING {
                pieces.push(Piece {
                    res: res.clone(),
                    first: 0,
                    count: res.subs,
                    len: total,
                    at: 0,
                });
            } else {
                for sub in 0..res.subs {
                    let (_, len) = gpu::footprints(&device, &desc, sub, 1);
                    if len <= STAGING {
                        pieces.push(Piece {
                            res: res.clone(),
                            first: u64::from(sub),
                            count: 1,
                            len,
                            at: 0,
                        });
                    } else {
                        r.problem(
                            "contents of a texture level of over 256 MB are not in the capture",
                        );
                    }
                }
            }
        }
        // Into batches, one read-back buffer each.
        let mut batches: Vec<Vec<Piece>> = vec![Vec::new()];
        let mut at = 0u64;
        for mut p in pieces {
            at = at.next_multiple_of(512);
            if at + p.len > STAGING {
                batches.push(Vec::new());
                at = 0;
            }
            p.at = at;
            at += p.len;
            if let Some(b) = batches.last_mut() {
                b.push(p);
            }
        }
        let mut stagings = Vec::new();
        for _ in 0..STAGINGS.min(batches.len()) {
            stagings.push(gpu::buffer(
                &device,
                D3D12_HEAP_TYPE_READBACK,
                STAGING,
                D3D12_RESOURCE_FLAG_NONE,
            )?);
        }
        let file = File::create(r.folder.join("contents.bin"))?;
        let end = AtomicU64::new(0);
        let contents: std::sync::Mutex<Vec<Content>> = std::sync::Mutex::default();
        let failed: std::sync::Mutex<Option<std::io::Error>> = std::sync::Mutex::default();
        let (todo, work) = std::sync::mpsc::channel::<(usize, Vec<Piece>)>();
        let work = std::sync::Mutex::new(work);
        let (free, freed) = std::sync::mpsc::channel::<usize>();
        for i in 0..stagings.len() {
            free.send(i)?;
        }
        let maps: Vec<usize> = stagings.iter().map(|s| s.1 as usize).collect();
        std::thread::scope(|scope| -> Result<(), Fault> {
            for _ in 0..stagings.len() {
                let (work, free, maps, file, end, contents, failed) =
                    (&work, free.clone(), &maps, &file, &end, &contents, &failed);
                scope.spawn(move || {
                    loop {
                        let next = lock(work).recv();
                        let Ok((i, pieces)) = next else { break };
                        for p in pieces {
                            // SAFETY: inside the buffer's mapping; the copy
                            // into it has run.
                            let bytes = unsafe {
                                std::slice::from_raw_parts(
                                    (maps[i] + p.at as usize) as *const u8,
                                    p.len as usize,
                                )
                            };
                            let at = if zero(bytes) {
                                ZERO
                            } else {
                                end.fetch_add(p.len, Ordering::Relaxed)
                            };
                            if at != ZERO
                                && let Err(e) = write_at(file, bytes, at)
                            {
                                lock(failed).get_or_insert(e);
                            }
                            lock(contents).push(Content {
                                res: p.res.id,
                                first: p.first,
                                count: p.count,
                                len: p.len,
                                file: at,
                            });
                        }
                        let _ = free.send(i);
                    }
                });
            }
            // Moved in: the writers end when it goes.
            let todo = todo;
            for mut pieces in batches {
                {
                    let held = lock(&r.held);
                    pieces.retain(|p| held.contains_key(&p.res.id));
                }
                if pieces.is_empty() {
                    continue;
                }
                let i = freed.recv()?;
                let value = self
                    .worker
                    .send(|list| copy_out(&device, list, &self.states, &pieces, &stagings[i].0))?;
                self.worker.wait(value)?;
                todo.send((i, pieces))?;
            }
            Ok(())
        })?;
        if let Some(e) = lock(&failed).take() {
            return Err(e.into());
        }
        self.contents = contents.into_inner().unwrap_or_else(|e| e.into_inner());
        Ok(())
    }

    /// Buffers the CPU sees that were not known yet.
    pub(super) fn new_buffers(&mut self, r: &Recorder) -> Result<(), Fault> {
        let fresh: Vec<Arc<Res>> = read(&r.resources)
            .values()
            .filter(|res| {
                matches!(res.seen, Seen::Cached | Seen::Combined) && !self.known.contains(&res.id)
            })
            .cloned()
            .collect();
        for res in fresh {
            self.known.insert(res.id);
            if !res.is_buffer() {
                r.problem("contents of textures in memory the CPU sees are not in the capture");
                continue;
            }
            // Those alive at the start are held, those made since too; what
            // is not is dying.
            if !lock(&r.held).contains_key(&res.id) {
                continue;
            }
            let object = res.object();
            let size = res.desc.width as usize;
            let (live, through) = if res.seen == Seen::Cached {
                let mut data = std::ptr::null_mut();
                // SAFETY: a live buffer in memory the CPU sees.
                unsafe { object.Map(0, None, Some(&mut data))? };
                (data.cast::<u8>().cast_const(), None)
            } else {
                let room = self.blocks.iter().position(|b| b.3 + size as u64 <= b.2);
                let i = match room {
                    Some(i) => i,
                    None => {
                        let len = STAGING.max(size as u64);
                        let (b, data) = gpu::buffer(
                            &self.worker.device,
                            D3D12_HEAP_TYPE_READBACK,
                            len,
                            D3D12_RESOURCE_FLAG_NONE,
                        )?;
                        self.blocks.push((b, data as usize, len, 0));
                        self.blocks.len() - 1
                    }
                };
                let b = &mut self.blocks[i];
                let at = b.3;
                b.3 = (at + size as u64).next_multiple_of(256);
                ((b.1 + at as usize) as *const u8, Some((i, at)))
            };
            self.buffers.push(CpuBuffer {
                id: res.id,
                res: object,
                live,
                through,
                size,
                kept: vec![0; size],
            });
        }
        Ok(())
    }


}
