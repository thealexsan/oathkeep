//! First uploads of textures picked by index on a copy queue of their own
//! (`OATHKEEP_UPLOAD_AHEAD`): read, detiled and copied by threads of ours as
//! soon as the texture pass has made them, and the slots that wait for them
//! shown when the copy has run. Recorded into the pass's list, as every other
//! upload is, the copy ran with the frame that needs it, so a slot that shows
//! its texture only after its upload ([`crate::descriptors::host_heap::show_after`])
//! was black or grey in that frame, and in the next.
//!
//! Only a texture that no list has used is taken: made in the last second,
//! in the common state, and never asked for an upload before
//! ([`crate::d3d12::resource::HostTexture::ahead`]). It stays in the common state
//! (a copy queue's copies leave it there), from which the card's shaders read
//! it as any texture.

#[cfg(test)]
mod tests;
mod worker;

use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use super::Staging;
use crate::d3d12::object::This;
use crate::d3d12::resource::HostTexture;
use worker::{PriorityQueue, workers};

/// [`HostTexture::ahead`]: no upload was asked for yet; one is on its way
/// here; it has run; one was recorded into a list.
pub(crate) const NONE: u8 = 0;
pub(super) const PENDING: u8 = 1;
pub(super) const DONE: u8 = 2;
pub(crate) const LISTED: u8 = 3;

/// Bytes of title memory on their way at most: past it uploads go into the
/// lists, as before.
const MOST: u64 = 96 << 20;
/// Bytes and textures of one submission at most.
pub(super) const BATCH: (u64, usize) = (16 << 20, 64);
/// Compact / mip-tail uploads prioritized ahead of multi-megabyte speculative
/// textures so visible surfaces receive initial texels immediately.
const SMALL_PRIORITY_BYTES: u64 = 256 << 10;

/// Whether it is on (`OATHKEEP_UPLOAD_AHEAD`; off).
pub(crate) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::diagnostics::ab_toggle::pick("OATHKEEP_UPLOAD_AHEAD") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_UPLOAD_AHEAD").is_some_and(|v| v != "0"))
}

/// An upload on its way.
pub(crate) struct Job {
    pub(super) staging: Arc<Staging>,
    pub(super) copy: Arc<HostTexture>,
    pub(super) asked: std::time::Instant,
    /// What to do when it has run; `None`: it has.
    pub(super) then: Mutex<Option<Vec<Box<dyn FnOnce() + Send>>>>,
}

impl Job {
    /// Calls `f` when the copy has run: now if it has.
    pub(crate) fn then(&self, f: impl FnOnce() + Send + 'static) {
        let mut then = self.then.lock().unwrap_or_else(|e| e.into_inner());
        match then.as_mut() {
            Some(then) => then.push(Box::new(f)),
            None => {
                drop(then);
                f();
            }
        }
    }
}

/// What a texture asked for here gets.
pub(crate) enum Taken {
    /// A job of its own, to be [`send`] once what follows it is added.
    New(Arc<Job>),
    /// One already on its way, or that has run.
    Known(Arc<Job>),
}

pub(super) static QUEUE: (Mutex<PriorityQueue<Arc<Job>>>, Condvar) =
    (Mutex::new(PriorityQueue::new()), Condvar::new());
/// Those on their way, by host copy ([`HostTexture::serial`]).
pub(super) static ON_THEIR_WAY: Mutex<Vec<(u64, Arc<Job>)>> = Mutex::new(Vec::new());
pub(super) static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
pub(super) static LANDED: Condvar = Condvar::new();
pub(super) static BYTES: AtomicU64 = AtomicU64::new(0);
/// Since the start: uploads, bytes, microseconds from the asking until the
/// copy had run, the longest; askings turned down; waits of lists.
pub(super) static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Takes the first upload of `resource`'s levels from `first` on, `there` of
/// whose bytes are mapped (`u64::MAX`: not known). `None`: the list's.
pub(crate) fn take(resource: This, first: u32, there: u64) -> Option<Taken> {
    if on() { claim(resource, first, there) } else { None }
}

pub(super) fn claim(resource: This, first: u32, there: u64) -> Option<Taken> {
    workers()?;
    let turned_down = || {
        STATS[4].fetch_add(1, Relaxed);
        None
    };
    let copy = crate::d3d12::resource::host_copy(resource)?;
    if copy.ahead.load(Relaxed) == PENDING {
        let way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
        return way.iter().find(|(serial, _)| *serial == copy.serial).map(|(_, job)| Taken::Known(job.clone()));
    }
    let most = if crate::memory::residency::limited_host_ram() { 32 << 20 } else { MOST };
    let est_bytes = super::title_span(resource, first).map_or(0, |(_, len)| len);
    if !crate::memory::residency::admit_ahead(
        crate::memory::residency::vram_level(),
        BYTES.load(Relaxed),
        est_bytes,
        most,
        super::draw_active_recently(),
        crate::memory::residency::allow_staging_growth(false),
    )
        || copy.flags() & 0x7 != 0
        || copy.state.load(Relaxed) != 0
        || copy.grown.load(Relaxed)
        || copy.tiled.is_some()
        || copy.holds.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        || !crate::memory::pool::fresh(&copy.texture)
        || super::host_wrote(resource as usize)
        || crate::d3d12::resource::alias_of(resource).is_some()
    {
        return turned_down();
    }
    if copy.ahead.compare_exchange(NONE, PENDING, Relaxed, Relaxed).is_err() {
        return turned_down();
    }
    super::PICKING.set(Some(there));
    let staging = super::staging::staging(resource, &copy, first);
    super::PICKING.set(None);
    let Some(staging) = staging else {
        // Staging was deferred or unavailable; leave the texture eligible for
        // retry instead of permanently marking it LISTED.
        copy.ahead.store(NONE, Relaxed);
        return turned_down();
    };
    if staging.on_demand || staging.first != staging.level0 || staging.uploaded.load(Relaxed) || !staging.once.load(Relaxed) {
        copy.ahead.store(LISTED, Relaxed);
        return turned_down();
    }
    super::note_picked_upload(resource, &copy, &staging);
    if super::refresh::no_wait() && super::refresh::fill_unlocked() {
        staging.expected.store(if there == u64::MAX { super::mapped_bytes(staging.memory, staging.read) } else { there }, Relaxed);
    }
    let job = Arc::new(Job { staging, copy, asked: std::time::Instant::now(), then: Mutex::new(Some(Vec::new())) });
    BYTES.fetch_add(job.staging.read as u64, Relaxed);
    {
        let mut way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
        way.push((job.copy.serial, job.clone()));
        IN_FLIGHT.store(way.len(), std::sync::atomic::Ordering::Release);
    }
    Some(Taken::New(job))
}

/// Sends a job of [`take`] on its way.
pub(crate) fn send(job: &Arc<Job>) {
    let urgent = job.staging.read as u64 <= SMALL_PRIORITY_BYTES || job.staging.first > 0;
    QUEUE
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(job.copy.serial, job.clone(), urgent);
    QUEUE.1.notify_one();
}

/// Waits until every upload currently on its way here has finished copying
/// and run its completion callbacks (queuing slot flips into `FLIPS`).
/// Returns how many jobs were waited for.
pub(crate) fn drain() -> usize {
    if IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return 0;
    }
    let mut way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
    let waiting = way.len();
    let started = std::time::Instant::now();
    while !way.is_empty() && started.elapsed().as_millis() < 2000 {
        way = LANDED
            .wait_timeout(way, std::time::Duration::from_millis(50))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    waiting
}

/// A list is about to record an upload into `copy`: one on its way here runs
/// first, and no other is taken after it.
pub(crate) fn listed(copy: &HostTexture) {
    if copy.ahead.compare_exchange(NONE, LISTED, Relaxed, Relaxed) != Err(PENDING) {
        return;
    }
    STATS[5].fetch_add(1, Relaxed);
    // Promote a draw-demanded job straight to the front of the urgent queue so
    // the recording thread does not wait behind speculative background uploads.
    if QUEUE.0.lock().unwrap_or_else(|e| e.into_inner()).promote(copy.serial) {
        QUEUE.1.notify_all();
    }
    let mut way = ON_THEIR_WAY.lock().unwrap_or_else(|e| e.into_inner());
    let started = std::time::Instant::now();
    while way.iter().any(|(serial, _)| *serial == copy.serial) && started.elapsed().as_millis() < 1000 {
        way = LANDED.wait_timeout(way, std::time::Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// Whether `resource`'s host copy was filled here: no list still brings what
/// it shows.
pub(crate) fn there(resource: This) -> bool {
    crate::d3d12::resource::made_copy(resource).is_some_and(|copy| copy.ahead.load(Relaxed) == DONE)
}

/// The state of [`HostTexture::ahead`] of a new host copy.
pub(crate) fn fresh() -> AtomicU8 {
    AtomicU8::new(NONE)
}
