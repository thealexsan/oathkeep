use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use crate::modules::xasync::{E_INVALIDARG, E_PENDING};
use crate::modules::xgameruntime::{HRESULT, Methods, missing};

use super::scheduler::{Command, Read, close_lane, scheduler};
use super::trace::Note;
use super::{Instance, Request, add_ref_instance, release, stream_log};

// Status arrays.

pub(super) struct StatusArray {
    pub(super) entries: Box<[AtomicI32]>,
    /// For the log: when each entry was written ([`stream_log::now`]); 0
    /// once the title has seen it.
    pub(super) written: Box<[AtomicU64]>,
}

pub(super) type Array = *mut Instance<StatusArray>;

/// The result of the requests before entry `index`: pending, the first
/// failure, or success.
extern "system" fn status_result(this: Array, index: u32) -> HRESULT {
    // SAFETY: an array we created and the title still holds.
    let entries = &unsafe { &*this }.state.entries;
    entries
        .get(index as usize)
        .map_or(E_INVALIDARG, |e| e.load(Ordering::Acquire))
}

/// The title polls this (entry 0, a `Sleep(1)` loop) and never
/// reads the result itself.
extern "system" fn is_complete(this: Array, index: u32) -> bool {
    let done = status_result(this, index) != E_PENDING;
    if done && stream_log::on() {
        // SAFETY: as above.
        let written = &unsafe { &*this }.state.written;
        if let Some(at) = written
            .get(index as usize)
            .map(|w| w.swap(0, Ordering::Relaxed))
            .filter(|&at| at != 0)
        {
            stream_log::line(format_args!("P\t{:x}\t{at}\t{}", this as usize, stream_log::now()));
        }
    }
    done
}

pub(super) static STATUS_VTABLE: Methods<5> = Methods([
    missing::<0> as *const (),
    add_ref_instance::<StatusArray> as *const (),
    release::<StatusArray> as *const (),
    is_complete as *const (),   // 0x18
    status_result as *const (), // 0x20
]);

// Queues.

/// A queue object: its lane in the scheduler.
pub(super) struct Queue {
    pub(super) id: u64,
}

impl Drop for Queue {
    fn drop(&mut self) {
        close_lane(self.id);
    }
}

type QueueRef = *const Instance<Queue>;

/// A status entry to write, holding a reference on its array.
pub(super) struct Status {
    pub(super) array: Array,
    pub(super) index: u32,
    /// For the log.
    pub(super) enqueued: u64,
}

impl Drop for Status {
    fn drop(&mut self) {
        release(self.array);
    }
}

impl Status {
    pub(super) fn entry(&self) -> &AtomicI32 {
        // SAFETY: we hold a reference; the index was checked when enqueued.
        &unsafe { &*self.array }.state.entries[self.index as usize]
    }

    pub(super) fn written(&self) -> &AtomicU64 {
        // SAFETY: as above.
        &unsafe { &*self.array }.state.written[self.index as usize]
    }
}

extern "system" fn enqueue_request(this: QueueRef, request: *const Request) {
    // SAFETY: the title passes its request (null tolerated).
    let Some(&request) = (unsafe { request.as_ref() }) else {
        return;
    };
    let note = Note::enqueued(&request);
    queue(this, Command::Read(Read { request, note }));
}

extern "system" fn enqueue_status(this: QueueRef, array: Array, index: u32) {
    // SAFETY: the title passes an array we created (null tolerated).
    let Some(entries) = (unsafe { array.as_ref() }).map(|a| &a.state.entries) else {
        return;
    };
    let Some(entry) = entries.get(index as usize) else {
        tracing::warn!(index, "dstorage: status entry out of range");
        return;
    };
    entry.store(E_PENDING, Ordering::Release);
    add_ref_instance(array);
    let enqueued = stream_log::now();
    queue(this, Command::Status(Status { array, index, enqueued }));
}

/// Whether a queue submits itself once half of it is enqueued
/// (`OATHKEEP_DSTORAGE_HALF=1`), as DirectStorage for the PC does and as ours
/// did. The title's batches (up to 4,095 requests, in queues of 4,159) hold
/// more than half now and then, and it maps the pages they go to and fills
/// in their page lists in its own submit, after it has enqueued them all
/// (image +0xe88f0: the batch's mapping list first, at +0xf2c470, which
/// writes the page lists at +0x55aa6a0; then the status entry; then our
/// submit). So 2,079 requests, three or four times a load, ran on lists of
/// page 0, or, worse, of the pages the addresses had shown before: their
/// data went into whatever those pages belonged to by then, and the pages it
/// was for stayed as they were.
fn half_submits() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_DSTORAGE_HALF").is_some_and(|v| v == "1"))
}

/// Adds a command to the queue; blocks while the queue is full. A queue
/// that is full of commands nobody submitted submits itself, since nothing
/// else could make room.
fn queue(this: QueueRef, command: Command) {
    // SAFETY: a queue we created and the title still holds.
    let id = unsafe { &*this }.state.id;
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    loop {
        let Some(lane) = lanes.lanes.get(&id).filter(|l| !l.closed) else {
            return; // closed: dropped (a status releases its array)
        };
        // One slot stays free, as the console's count of empty slots says.
        if lane.entries() + 1 < lane.capacity.max(2) {
            break;
        }
        if !lane.unsubmitted.is_empty() && lane.reads.is_empty() && lane.running.is_empty() {
            tracing::warn!(
                queue = id,
                commands = lane.unsubmitted.len(),
                "dstorage: a queue full of commands that are not submitted submits itself"
            );
            lanes.submit(id, true);
            scheduler.work.notify_all();
            continue;
        }
        lanes = scheduler
            .finished
            .wait(lanes)
            .unwrap_or_else(|e| e.into_inner());
    }
    let Some(lane) = lanes.lanes.get_mut(&id) else {
        return;
    };
    lane.unsubmitted.push(command);
    if half_submits() && lane.unsubmitted.len() >= (lane.capacity / 2).max(1) {
        lanes.submit(id, true);
        scheduler.work.notify_all();
    }
}

/// The title's submit, where its page lists become valid ([`half_submits`]):
/// the pages are mapped just before, also those under addresses without a
/// list (memory only the CPU sees).
extern "system" fn submit(this: QueueRef) {
    let _called = oathkeep_gpu::timeline::span("the title's DirectStorage Submit");
    // SAFETY: as above.
    let id = unsafe { &*this }.state.id;
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    let pending = lanes.lanes.get(&id).map_or(0, |l| l.unsubmitted.len());
    tracing::debug!(commands = pending, "dstorage: submit");
    lanes.submit(id, false);
    scheduler.work.notify_all();
}

/// Ends the queue whatever its reference count: what was not started never
/// completes, and this returns once the reads already running have
/// finished. The title calls this, not `Release`.
extern "system" fn close(this: QueueRef) {
    // SAFETY: as above.
    close_lane(unsafe { &*this }.state.id);
}

/// Slots by the 2020 order, which the argument shapes at
/// the title's call sites match (0x18, 0x20, 0x30 and 0x40 are called).
pub(super) static QUEUE_VTABLE: Methods<12> = Methods([
    missing::<0> as *const (),
    add_ref_instance::<Queue> as *const (),
    release::<Queue> as *const (),
    enqueue_request as *const (), // 0x18
    enqueue_status as *const (),  // 0x20
    missing::<5> as *const (),    // 0x28 (2020: fence signal)
    submit as *const (),          // 0x30
    missing::<7> as *const (),    // 0x38 (2020: cancel by tag)
    close as *const (),           // 0x40
    missing::<9> as *const (),    // 0x48 (2020: error event)
    missing::<10> as *const (),   // 0x50 (2020: error record)
    missing::<11> as *const (),   // 0x58 (2020: queue info)
]);
