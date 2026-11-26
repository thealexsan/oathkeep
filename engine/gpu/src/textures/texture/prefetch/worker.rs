use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::{BATCH, BYTES, DONE, IN_FLIGHT, Job, LANDED, NONE, ON_THEIR_WAY, QUEUE, STATS};

/// Two-tier upload queue: draw-demanded (promoted by [`super::listed`]) and compact
/// mip-tail uploads drain ahead of large speculative background textures.
pub(super) struct PriorityQueue<T> {
    urgent: VecDeque<(u64, T)>,
    background: VecDeque<(u64, T)>,
}

impl<T> PriorityQueue<T> {
    pub(super) const fn new() -> Self {
        Self { urgent: VecDeque::new(), background: VecDeque::new() }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.urgent.is_empty() && self.background.is_empty()
    }

    pub(super) fn push(&mut self, serial: u64, item: T, urgent: bool) {
        if urgent {
            self.urgent.push_back((serial, item));
        } else {
            self.background.push_back((serial, item));
        }
    }

    /// Moves a queued background job to the front of the urgent tier when a
    /// command list actively demands it.
    pub(super) fn promote(&mut self, serial: u64) -> bool {
        if let Some(pos) = self.background.iter().position(|(s, _)| *s == serial)
            && let Some(entry) = self.background.remove(pos)
        {
            self.urgent.push_front(entry);
            return true;
        }
        false
    }

    pub(super) fn pop_batch(&mut self, max_bytes: u64, max_count: usize, size_of: impl Fn(&T) -> u64) -> Vec<T> {
        let mut out = Vec::new();
        let mut bytes = 0u64;
        while bytes < max_bytes && out.len() < max_count {
            let next = self.urgent.pop_front().or_else(|| self.background.pop_front());
            let Some((_, item)) = next else { break };
            bytes += size_of(&item);
            out.push(item);
        }
        out
    }
}

/// The copy queue and its threads, made on first use.
pub(super) fn workers() -> Option<&'static ID3D12CommandQueue> {
    /// Free-threaded.
    struct Queue(ID3D12CommandQueue);
    // SAFETY: a command queue is free-threaded.
    unsafe impl Send for Queue {}
    // SAFETY: as above.
    unsafe impl Sync for Queue {}
    static WORKERS: OnceLock<Option<Queue>> = OnceLock::new();
    WORKERS
        .get_or_init(|| {
            let host = crate::backend::host()?;
            // SAFETY: plain object creation.
            let queue: ID3D12CommandQueue = unsafe {
                host.device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_COPY,
                        ..Default::default()
                    })
                    .ok()?
            };
            for n in 0..2 {
                let worker = Worker::new(&host.device, queue.clone()).ok()?;
                std::thread::Builder::new().name(format!("uploads ahead {n}")).spawn(move || worker.work()).ok()?;
            }
            Some(Queue(queue))
        })
        .as_ref()
        .map(|q| &q.0)
}

struct Worker {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
}

// SAFETY: a worker's objects are its thread's own; the queue is free-threaded.
unsafe impl Send for Worker {}

impl Worker {
    fn new(device: &ID3D12Device, queue: ID3D12CommandQueue) -> windows::core::Result<Worker> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.Close()?;
            Ok(Worker { queue, allocator, list, fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? })
        }
    }

    fn work(self) {
        let mut value = 0;
        loop {
            let jobs = {
                let mut queue = QUEUE.0.lock().unwrap_or_else(|e| e.into_inner());
                while queue.is_empty() {
                    queue = QUEUE.1.wait(queue).unwrap_or_else(|e| e.into_inner());
                }
                queue.pop_batch(BATCH.0, BATCH.1, |job| job.staging.read as u64)
            };
            value += 1;
            match self.copy(&jobs, value) {
                Ok(copied) => finish(jobs, Some(&copied)),
                Err(e) => {
                    tracing::warn!(error = %e, "uploads ahead did not run");
                    finish(jobs, None);
                }
            }
        }
    }

    fn copy(&self, jobs: &[Arc<Job>], value: u64) -> windows::core::Result<Vec<bool>> {
        let mut ready = Vec::with_capacity(jobs.len());
        let mut any = false;
        for job in jobs {
            job.staging.refresh();
            let has_data = job.staging.has_data();
            ready.push(has_data);
            any |= has_data;
        }
        if !any {
            return Ok(ready);
        }
        // SAFETY: this worker's allocator and list, whose last submission has
        // run; the jobs hold what the list names until after the wait.
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            for (job, &has_data) in jobs.iter().zip(&ready) {
                if has_data {
                    job.record(&self.list);
                }
            }
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.queue.Signal(&self.fence, value)?;
            // (Without an event: this waits.)
            self.fence.SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE::default())?;
        }
        Ok(ready)
    }
}

impl Job {
    /// # Safety
    /// `list` is an open copy list, and the job lives until it has run.
    unsafe fn record(&self, list: &ID3D12GraphicsCommandList) {
        let (s, texture) = (&self.staging, &self.copy.texture);
        for (i, footprint) in s.footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: s.first - s.level0 + i as u32 },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&s.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: s.base + footprint.Offset, ..*footprint },
                },
            };
            // SAFETY: the caller's.
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
}

/// The jobs' copies have run (or failed/skipped when `copied` is `None` or
/// `false` for a job, in which case slots stay on `neutral()` and retry).
fn finish(jobs: Vec<Arc<Job>>, copied: Option<&[bool]>) {
    let mut any_flipped = false;
    for (i, job) in jobs.iter().enumerate() {
        BYTES.fetch_sub(job.staging.read as u64, Relaxed);
        let ok = copied.is_some_and(|flags| flags.get(i).copied().unwrap_or(false));
        if ok {
            job.staging.uploaded.store(true, Relaxed);
            job.staging.dirty.store(false, Relaxed);
            job.staging.note_held();
            job.copy.ahead.store(DONE, Relaxed);
        } else {
            // Do not flip the descriptor slot to an un-uploaded texture; reset
            // `ahead` so a retry or list upload can fill it cleanly.
            job.copy.ahead.store(NONE, Relaxed);
            let _ = job.then.lock().unwrap_or_else(|e| e.into_inner()).take();
            crate::descriptors::host_heap::read_short(job.staging.memory as u64);
        }
    }
    for (i, job) in jobs.iter().enumerate() {
        if !copied.is_some_and(|flags| flags.get(i).copied().unwrap_or(false)) {
            continue;
        }
        any_flipped = true;
        let us = job.asked.elapsed().as_micros() as u64;
        let n = STATS[0].fetch_add(1, Relaxed) + 1;
        let bytes = STATS[1].fetch_add(job.staging.read as u64, Relaxed) + job.staging.read as u64;
        let all = STATS[2].fetch_add(us, Relaxed) + us;
        let most = STATS[3].fetch_max(us, Relaxed).max(us);
        if n == 1 || n.is_multiple_of(5000) {
            tracing::info!(
                "textures uploaded ahead of the lists (OATHKEEP_UPLOAD_AHEAD=1), since the start: {n}, {} MB, {:.1} ms from the asking until they were there, the longest {:.0}; {} turned down, {} waited for by lists",
                bytes >> 20,
                all as f64 / n as f64 / 1e3,
                most as f64 / 1e3,
                STATS[4].load(Relaxed),
                STATS[5].load(Relaxed)
            );
        }
        let then = job.then.lock().unwrap_or_else(|e| e.into_inner()).take();
        for f in then.into_iter().flatten() {
            f();
        }
    }
    // Remove from `ON_THEIR_WAY` only after `job.then` callbacks have queued
    // their `FLIPS` entries so `drain()` observes every flip ready to apply.
    {
        let mut way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
        way.retain(|(serial, _)| !jobs.iter().any(|job| job.copy.serial == *serial));
        IN_FLIGHT.store(way.len(), std::sync::atomic::Ordering::Release);
        LANDED.notify_all();
    }
    if any_flipped {
        crate::descriptors::host_heap::flip_now();
    }
}
