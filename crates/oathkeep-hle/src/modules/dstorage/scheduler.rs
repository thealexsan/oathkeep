use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use crate::modules::xgameruntime::{HRESULT, S_OK};

use super::execute::{READING, execute};
use super::queue::Status;
use super::trace::{Note, Traced, trace_batch, walls_request};
use super::{BCPACK, MEMORY_SOURCE, PAGE_DESTINATION, Request, ZLIB, stream_log};

/// Every queue's requests, run by a pool of workers.
pub(super) struct Scheduler {
    pub(super) lanes: Mutex<Lanes>,
    /// Requests to start, or staging space freed.
    pub(super) work: Condvar,
    /// A request finished (for full queues and `close`).
    pub(super) finished: Condvar,
}

/// Priority weights per class (low, normal, high and real-time), as the
/// console's: about 10x per level, counted against request sizes.
const COST: [u64; 3] = [100, 10, 1];
/// The console's default staging buffer.
pub(super) const STAGING: u64 = 32 << 20;

pub(super) struct Lanes {
    pub(super) lanes: HashMap<u64, Lane>,
    pub(super) next_id: u64,
    /// Submission order over all queues.
    pub(super) next_seq: u64,
    /// Bytes of staging held by running requests, and the limit.
    pub(super) staged: u64,
    pub(super) staging: u64,
    /// A start waited for staging space since the last finish.
    pub(super) starved: bool,
    /// Weighted round robin: each class's virtual time (bytes × cost), and
    /// the time of the last start; within a class, the lane clock of the
    /// last start.
    pub(super) clock: [u64; 3],
    pub(super) now: u64,
    pub(super) lane_now: [u64; 3],
    /// Since the last debug summary: requests, bytes written, time spent
    /// running them.
    pub(super) done: (u64, u64, std::time::Duration),
    pub(super) refused: u64,
    /// BCPack bytes written and time spent on them.
    pub(super) bcpack: (u64, std::time::Duration),
    pub(super) since: Option<std::time::Instant>,
}

/// A queue's requests in the scheduler.
pub(super) struct Lane {
    pub(super) class: usize,
    pub(super) capacity: usize,
    /// Bytes started, for round robin among the lanes of a class.
    pub(super) clock: u64,
    /// Enqueued, not yet submitted.
    pub(super) unsubmitted: Vec<Command>,
    /// Submitted, by sequence number: reads not started, status entries
    /// not written, reads running, and failures not yet reported.
    pub(super) reads: VecDeque<(u64, Read)>,
    pub(super) statuses: VecDeque<(u64, Status)>,
    pub(super) running: BTreeSet<u64>,
    pub(super) failures: BTreeMap<u64, HRESULT>,
    pub(super) closed: bool,
}

// SAFETY: requests point into title memory and our own objects, which stay
// valid until the title has seen them complete; status arrays count their
// references atomically.
unsafe impl Send for Lane {}

/// A read a worker runs.
pub(super) struct Job {
    pub(super) lane: u64,
    pub(super) seq: u64,
    pub(super) request: Request,
    pub(super) note: Note,
    pub(super) staging: u64,
}

// SAFETY: as for `Lane`.
unsafe impl Send for Job {}

pub(super) enum Command {
    Read(Read),
    Status(Status),
}

/// An enqueued request, and what the trace knows of it.
#[derive(Clone, Copy)]
pub(super) struct Read {
    pub(super) request: Request,
    pub(super) note: Note,
}

/// The scheduler, with its workers started on first use; `None` if not
/// even one worker could start.
pub(super) fn scheduler() -> Option<&'static Scheduler> {
    static SCHEDULER: OnceLock<Option<Scheduler>> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let scheduler = SCHEDULER
        .get_or_init(|| {
            Some(Scheduler {
                lanes: Mutex::new(Lanes {
                    lanes: HashMap::new(),
                    next_id: 0,
                    next_seq: 0,
                    staged: 0,
                    staging: STAGING,
                    starved: false,
                    clock: [0; 3],
                    now: 0,
                    lane_now: [0; 3],
                    done: Default::default(),
                    refused: 0,
                    bcpack: Default::default(),
                    since: None,
                }),
                work: Condvar::new(),
                finished: Condvar::new(),
            })
        })
        .as_ref()?;
    STARTED.call_once(|| {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let started = (0..workers)
            .filter(|i| {
                std::thread::Builder::new()
                    .name(format!("dstorage {i}"))
                    .spawn(|| work(scheduler))
                    .is_ok()
            })
            .count();
        tracing::info!(workers = started, "dstorage: workers");
    });
    Some(scheduler)
}

impl Scheduler {
    pub(super) fn lock(&self) -> MutexGuard<'_, Lanes> {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A worker: runs the next read of the most deserving queue.
fn work(scheduler: &'static Scheduler) {
    let mut lanes = scheduler.lock();
    loop {
        let Some(job) = lanes.start() else {
            lanes = scheduler.work.wait(lanes).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        drop(lanes);
        let started = std::time::Instant::now();
        let traced = Traced::start(&job);
        let reading = oathkeep_gpu::timeline::span("streaming: a request");
        let result = execute(&job.request);
        reading.end();
        walls_request(&job.request, &result, started);
        stream_log::request(&job, started, &result);
        if let Some(traced) = traced {
            traced.end(&job, &result);
        }
        lanes = scheduler.lock();
        lanes.finish(&job, result);
        lanes.count(&job, started);
        if std::mem::take(&mut lanes.starved) {
            scheduler.work.notify_all();
        }
        scheduler.finished.notify_all();
    }
}

impl Lanes {
    /// Counts a finished request; every 5 s a debug line sums them up.
    fn count(&mut self, job: &Job, started: std::time::Instant) {
        let (requests, bytes, busy) = &mut self.done;
        *requests += 1;
        *bytes += u64::from(job.request.destination_size);
        *busy += started.elapsed();
        if job.request.options & BCPACK != 0 {
            self.bcpack.0 += u64::from(job.request.destination_size);
            self.bcpack.1 += started.elapsed();
        }
        let since = *self.since.get_or_insert(started);
        let elapsed = since.elapsed();
        if elapsed.as_secs() >= 5 {
            let waiting: usize = self.lanes.values().map(|l| l.reads.len()).sum();
            let reading = READING.swap(0, Ordering::Relaxed) as f64 / 1e9;
            tracing::debug!(
                requests = *requests,
                mb_per_s = format_args!("{:.1}", *bytes as f64 / elapsed.as_secs_f64() / 1e6),
                busy_workers = format_args!("{:.2}", busy.as_secs_f64() / elapsed.as_secs_f64()),
                reading_workers = format_args!("{:.2}", reading / elapsed.as_secs_f64()),
                staging_waits = std::mem::take(&mut self.refused),
                bcpack_mb = self.bcpack.0 / 1_000_000,
                bcpack_workers = format_args!("{:.2}", self.bcpack.1.as_secs_f64() / elapsed.as_secs_f64()),
                waiting,
                "dstorage: last {:.1} s",
                elapsed.as_secs_f64()
            );
            self.done = Default::default();
            self.bcpack = Default::default();
            self.since = None;
        }
    }

    /// Starts the next read of the class with the least virtual time, from
    /// its lane that has started the fewest bytes, if the staging buffer
    /// has room for it (or nothing else holds any).
    pub(super) fn start(&mut self) -> Option<Job> {
        let busy = || self.lanes.iter().filter(|(_, l)| !l.reads.is_empty());
        let (_, class) = busy().map(|(_, l)| (self.clock[l.class], l.class)).min()?;
        let (_, _, id) = busy()
            .filter(|(_, l)| l.class == class)
            .filter_map(|(&id, l)| Some((l.clock, l.reads.front()?.0, id)))
            .min()?;
        let lane = self.lanes.get_mut(&id)?;
        let &(seq, Read { request, note }) = lane.reads.front()?;
        let staging = staging_bytes(&request);
        if self.staged > 0 && self.staged + staging > self.staging {
            self.starved = true;
            self.refused += 1;
            return None;
        }
        lane.reads.pop_front();
        lane.running.insert(seq);
        self.staged += staging;
        let size = u64::from(request.destination_size.max(request.source_size)).max(1);
        self.now = self.clock[class];
        self.clock[class] += size * COST[class];
        self.lane_now[class] = lane.clock;
        lane.clock += size;
        Some(Job {
            lane: id,
            seq,
            request,
            note,
            staging,
        })
    }

    pub(super) fn finish(&mut self, job: &Job, result: Result<(), HRESULT>) {
        self.staged -= job.staging;
        if let Some(lane) = self.lanes.get_mut(&job.lane) {
            lane.running.remove(&job.seq);
            if let Err(hr) = result {
                lane.failures.insert(job.seq, hr);
            }
            lane.flush();
        }
    }

    /// Moves a queue's enqueued commands to the workers; `early` when the
    /// title has not asked for it ([`super::queue::queue`]).
    pub(super) fn submit(&mut self, id: u64, early: bool) {
        let Some(class) = self.lanes.get(&id).map(|l| l.class) else {
            return;
        };
        // A class that had nothing waiting starts from now, not from the
        // credit it left behind.
        if !self
            .lanes
            .values()
            .any(|l| l.class == class && !l.reads.is_empty())
        {
            self.clock[class] = self.clock[class].max(self.now);
        }
        let Some(lane) = self.lanes.get_mut(&id) else {
            return;
        };
        if lane.reads.is_empty() {
            lane.clock = lane.clock.max(self.lane_now[class]);
        }
        trace_batch(id, &lane.unsubmitted, early);
        for command in std::mem::take(&mut lane.unsubmitted) {
            let seq = self.next_seq;
            self.next_seq += 1;
            match command {
                Command::Read(mut read) => {
                    read.note.submitted(&read.request, early);
                    lane.reads.push_back((seq, read));
                }
                Command::Status(status) => lane.statuses.push_back((seq, status)),
            }
        }
        lane.flush();
    }
}

impl Lane {
    pub(super) fn new(priority: i8, capacity: u16) -> Lane {
        Lane {
            class: priority.clamp(-1, 1).wrapping_add(1) as usize,
            capacity: usize::from(capacity),
            clock: 0,
            unsubmitted: Vec::new(),
            reads: VecDeque::new(),
            statuses: VecDeque::new(),
            running: BTreeSet::new(),
            failures: BTreeMap::new(),
            closed: false,
        }
    }

    /// Requests and status entries held, submitted or not.
    pub(super) fn entries(&self) -> usize {
        self.unsubmitted.len() + self.reads.len() + self.statuses.len() + self.running.len()
    }

    /// Writes the status entries whose earlier reads have all finished.
    pub(super) fn flush(&mut self) {
        while let Some(&(seq, _)) = self.statuses.front() {
            let waiting = self.reads.front().map(|r| r.0);
            if waiting.into_iter().chain(self.running.first().copied()).any(|s| s < seq) {
                return;
            }
            let Some((_, status)) = self.statuses.pop_front() else {
                return;
            };
            let later = self.failures.split_off(&seq);
            let first = std::mem::replace(&mut self.failures, later)
                .into_values()
                .next();
            if stream_log::on() {
                let at = stream_log::now();
                stream_log::line(format_args!("S\t{:x}\t{seq}\t{}\t{at}", status.array as usize, status.enqueued));
                status.written().store(at, Ordering::Relaxed);
            }
            status
                .entry()
                .store(first.unwrap_or(S_OK), Ordering::Release);
        }
    }
}

/// Staging bytes a request holds while it runs: what it buffers between
/// the file and the destination. Raw reads go straight to their
/// destination, memory sources need none (as on the console).
fn staging_bytes(request: &Request) -> u64 {
    let buffered = request.options & (ZLIB | BCPACK | PAGE_DESTINATION) != 0;
    if buffered && request.options & MEMORY_SOURCE == 0 {
        u64::from(request.source_size)
    } else {
        0
    }
}

/// Ends a queue: what was not started is dropped, and this waits for the
/// reads already running (they write title memory).
pub(super) fn close_lane(id: u64) {
    let Some(scheduler) = scheduler() else {
        return;
    };
    let mut lanes = scheduler.lock();
    let Some(lane) = lanes.lanes.get_mut(&id) else {
        return;
    };
    lane.closed = true;
    lane.unsubmitted.clear();
    lane.reads.clear();
    lane.statuses.clear(); // releases their arrays
    while lanes.lanes.get(&id).is_some_and(|l| !l.running.is_empty()) {
        lanes = scheduler
            .finished
            .wait(lanes)
            .unwrap_or_else(|e| e.into_inner());
    }
    lanes.lanes.remove(&id);
    scheduler.finished.notify_all();
}
