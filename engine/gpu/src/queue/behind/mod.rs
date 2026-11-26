//! Submissions prepared and sent by a thread of ours, in order, instead of by
//! the thread of the title that submits them (`OATHKEEP_ASYNC_SUBMIT=1`).
//!
//! The idea and the argument are fxceless/victory-fork's, commit f85101b6
//! (GPL-3.0-or-later, as this): a profile had the title's render thread inside
//! `queue::execute` for 202 of 292 ms of a frame while its recording threads
//! and the card stood idle, and the title does nothing with the result of that
//! work: it only has to be done before the lists run, in order. (Here, at
//! 77-79 ms a frame: 58 ms of it in the submit calls, the card idle for 37 ms
//! behind them.) What differs from that commit: one line for all queues, kept
//! in the order of the title's calls; the present stands in it too; nobody but
//! our thread runs what is in it.
//!
//! With the switch a submission's lists are taken from the title's lists (as
//! they are for every flight, `crate::list::detach`) and put in the [`Lane`],
//! with what they wait for, the queues' fence waits and signals, and the
//! presents. [`Lane::serve`], on a thread of ours, takes from it whatever is
//! free to run, the oldest first; within a queue the order is the title's,
//! always. What tells the title that work is done (fence values,
//! bottom-of-pipe writes) is told by the flights as before, when the card has
//! run them: later than the title's call by more than before, never earlier.
//!
//! What the title may do between its submit call and the moment our thread
//! prepares the lists, and why that is all right:
//!
//! - Reset or release the lists and allocators: their host sides, upload
//!   memory and draw state were taken out of them.
//! - Write upload memory, descriptors, constants for later lists: those of
//!   lists submitted are the card's to read until a fence behind them is
//!   reached, on the console as here; no fence behind them is reached before
//!   the lists have run ([`signal`] goes into the line behind them).
//! - Signal, wait, present on the queue: in the line behind them.
//! - Write a fence or a value in memory by the CPU: at once; our thread looks
//!   at what it stands at again ([`wake`], and every millisecond).
//! - Map pages (`CopyPageMappingsBatchX`): at once, as before, where the lists
//!   on the card may be reading them too; the bytes of uploads were read when
//!   the lists were recorded.
//! - Ask for something that has to see earlier submissions done (a read-back,
//!   a fence waited for without an event): [`drain`] first.
//! - Release a texture or a view's memory: what is retired waits for what is
//!   in the line too ([`queued`]).
//!
//! The line is bounded: a present returns when the one before it was shown
//! ([`frames_most`]), and a submission when at most [`runs_most`] are in the
//! line.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use super::This;
use job::Job;
use lane::{Lane, Work};

mod job;
mod lane;
#[cfg(test)]
mod tests;

static LANE: Lane<Job> = Lane::new();

/// Submissions and presents in the line that have not been given to the host
/// yet.
static QUEUED: AtomicU64 = AtomicU64::new(0);

/// Since the last frame-rate line: submissions put in the line, presents, the
/// times they found things in it (the sum of their numbers) and the most; what
/// threads of the title waited, in microseconds, for room in the line, at a
/// present, and for a drain, and the drains; what our thread worked; waits
/// not met at a present.
static STATS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
const RUNS: usize = 0;
const PRESENTS: usize = 1;
const DEPTH: usize = 2;
const DEEPEST: usize = 3;
const ROOM: usize = 4;
const AT_PRESENT: usize = 5;
const DRAINING: usize = 6;
const DRAINS: usize = 7;
const BUSY: usize = 8;
const FORCED: usize = 9;
/// Parts prepared together with the one before them.
const JOINED: usize = 10;

/// How many submissions may be in the line when a submit call returns
/// (`OATHKEEP_SUBMIT_DEPTH`; 64: about two frames of the title's).
fn runs_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("OATHKEEP_SUBMIT_DEPTH").unwrap_or(64).max(1))
}

/// How many presents may be in the line when a present call returns
/// (`OATHKEEP_SUBMIT_FRAMES`; 1: the frame before is sent, this one may not be;
/// 0: both are).
fn frames_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("OATHKEEP_SUBMIT_FRAMES").unwrap_or(1))
}

fn number(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

thread_local! {
    /// This is the thread that does what is in the line.
    static OURS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether submissions are prepared by a thread of ours
/// (`OATHKEEP_ASYNC_SUBMIT=1`; needs flights with the present sent ahead, as
/// the relay has them, and lists cut at their waits).
pub(super) fn on() -> bool {
    // Diagnostics: on and off while the title runs (`crate::ab`).
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("OATHKEEP_ASYNC_SUBMIT") {
        return on && started();
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        (std::env::var_os("OATHKEEP_ASYNC_SUBMIT").is_some_and(|v| v == "1") || crate::list::thin::wanted()) && started()
    })
}

/// The line is about to be left for the way without it: everything in it is
/// done first. (At a present, by the title's thread that submits.)
#[cfg(feature = "diagnostics")]
pub(super) fn leave() {
    LANE.wait_presents(0);
    LANE.drain();
}

/// Starts the thread, once: whether there is one.
fn started() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if !crate::host::late_free_present() || !super::cuts() {
            tracing::warn!("OATHKEEP_ASYNC_SUBMIT=1 needs the relay and lists cut at their waits: submissions are prepared by the thread that makes them");
            return false;
        }
        let worker = || {
            use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
            // SAFETY: this thread's own priority.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
            crate::cores::keep_on_fast_cores("victory-submit");
            OURS.set(true);
            let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| LANE.serve()));
            if ended.is_err() {
                // Nothing would ever be sent again, and the title's thread
                // would stand at its next present for good.
                crate::host::fail(format_args!("the thread that prepares and sends submissions panicked"));
            }
        };
        match std::thread::Builder::new().name("victory-submit".into()).spawn(worker) {
            Ok(_) => {
                tracing::info!(
                    frames = frames_most(),
                    runs = runs_most(),
                    "submissions are prepared and sent by a thread of ours, not by the thread that submits (OATHKEEP_ASYNC_SUBMIT=1)"
                );
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no thread to prepare submissions: the threads that make them do");
                false
            }
        }
    })
}

/// Submissions in the line that the host has not begun
/// (`crate::host::retire` waits for them too).
pub(super) fn queued() -> u64 {
    QUEUED.load(Relaxed)
}

/// Something a queue may stand at moved.
pub(super) fn wake() {
    LANE.wake();
}

/// This thread does work the line waits for.
pub(super) fn no_drain_here() {
    OURS.set(true);
}

/// Before something that has to see the submissions made so far sent: they
/// are, as far as they are free to run. Not on our own thread, which is
/// inside one of them.
pub(super) fn drain() {
    if OURS.get() {
        return;
    }
    let began = Instant::now();
    LANE.drain();
    STATS[DRAINING].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    STATS[DRAINS].fetch_add(1, Relaxed);
}

/// A submission of `lists` to queue `this`.
pub(super) fn submit(this: This, lists: &[This]) {
    let queue = this as usize;
    if crate::list::thin::on() {
        let recordings = crate::list::thin::submitted(lists);
        if recordings.is_empty() {
            return;
        }
        let before = QUEUED.fetch_add(1, Relaxed);
        STATS[DEPTH].fetch_add(before, Relaxed);
        STATS[DEEPEST].fetch_max(before + 1, Relaxed);
        LANE.push(queue, [Job::Lists(queue, recordings)]);
        room(before + 1);
        return;
    }
    let jobs = jobs_of(this, lists);
    let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
    let before = QUEUED.fetch_add(runs, Relaxed);
    STATS[RUNS].fetch_add(runs, Relaxed);
    STATS[DEPTH].fetch_add(before, Relaxed);
    STATS[DEEPEST].fetch_max(before + runs, Relaxed);
    LANE.push(queue, jobs);
    room(before + runs);
}

/// Waits for room in the line, with `queued` submissions in it.
fn room(queued: u64) {
    if queued > runs_most() as u64 {
        let began = Instant::now();
        let _waiting = crate::timeline::span("a submission waits for room in the line");
        LANE.wait_runs(runs_most());
        STATS[ROOM].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

/// What a submission of `lists` is in the line.
fn jobs_of(this: This, lists: &[This]) -> Vec<Job> {
    let queue = this as usize;
    // In one piece up to the first list that waits for a memory value, as
    // `super::submit_cut` runs them; the rest part by part.
    let first = lists.iter().position(|&l| crate::list::cut_list(l)).unwrap_or(lists.len());
    #[cfg(feature = "diagnostics")]
    crate::capture::op(queue, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {} lists {named:?} to the line (the first {first} in one piece)",
            super::queue(this).kind
        )
    });
    let mut jobs: Vec<Job> = Vec::new();
    if first > 0 {
        let (taken, order) = crate::list::detach(&lists[..first]);
        jobs.push(Job::Run(queue, taken, order));
    }
    if first < lists.len() {
        for (waits, taken, order) in crate::list::detach_parts(&lists[first..]) {
            jobs.extend(waits.into_iter().map(Job::Value));
            jobs.push(Job::Run(queue, taken, order));
        }
    }
    jobs
}

/// A signal of queue `this`: behind what it has in the line; if that is
/// nothing, behind the flights in the air, or now.
pub(super) fn signal(this: This, fence: This, value: u64) {
    let fence = fence as usize;
    LANE.push_behind(this as usize, false, Job::Signal(fence, value), || {
        if !crate::host::late_signal(fence, value) {
            crate::fence::set(fence as This, value);
        }
    });
}

/// Queue `this` waits for `fence`: nothing if it has nothing in the line and
/// the fence is there already.
pub(super) fn wait(this: This, fence: This, value: u64) {
    let always = crate::fence::completed(fence) < value;
    LANE.push_behind(this as usize, always, Job::Wait(fence as usize, value), || {});
}

/// A present on queue `this` of `texture`, in the state the lists recorded so
/// far leave it in.
pub(super) fn present(
    this: This,
    texture: Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>,
    overlay: Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>,
    shown: super::Shown,
) {
    QUEUED.fetch_add(1, Relaxed);
    STATS[PRESENTS].fetch_add(1, Relaxed);
    LANE.push(this as usize, [Job::Present(texture, overlay, shown)]);
    let began = Instant::now();
    let _waiting = crate::timeline::span("a present waits for the one before it to be sent");
    LANE.wait_presents(frames_most());
    STATS[AT_PRESENT].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64).collect();
    let ms = |i: usize| s[i] / 1000.0 / frames;
    tracing::info!(
        "submissions prepared by a thread of ours, per frame (OATHKEEP_ASYNC_SUBMIT): {:.1} in {:.1} presents, {:.1} in the line \
         when one was added (at most {:.0}); that thread worked {:.1} ms; threads of the title waited {:.1} ms for room in the \
         line, {:.1} ms at presents, {:.1} ms in {:.1} drains; waits not met at a present {:.2}; {:.1} of the \
         submissions were prepared together with the one before them (OATHKEEP_JOIN_PARTS)",
        s[RUNS] / frames,
        s[PRESENTS] / frames,
        s[DEPTH] / s[RUNS].max(1.0),
        s[DEEPEST],
        ms(BUSY),
        ms(ROOM),
        ms(AT_PRESENT),
        ms(DRAINING),
        s[DRAINS] / frames,
        s[FORCED] / frames,
        s[JOINED] / frames
    );
}
