//! What is recorded, and when. Every event goes into one buffer made ahead:
//! a write takes a place in it (one atomic addition) and nothing else, so it
//! can be done anywhere, the handler of write faults included.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

mod session;
#[cfg(test)]
mod tests;

pub(crate) use session::{presented, title_thread};
pub(super) use session::{Data, wanted};
#[cfg(test)]
pub(super) use session::{begin, end, frame, make_room};

/// What an event is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    /// A span of ours.
    Span,
    /// A run of list calls: `a` calls, `b` ticks inside them.
    Calls,
    /// A wait on the object `a` (an address, or a handle's value); `b`: the
    /// value of the host's fence it stands for (0: none, [`COPY_QUEUE`]).
    Wait,
    /// The object `a` is signalled or released.
    Wake,
    /// A call of the kernel's that took long; `a` its bytes.
    Kernel,
    /// Lists went to the card: `a` the value signalled behind them, `b` the
    /// place of their timestamps plus one.
    Sent,
    /// A present came back: a frame ends.
    Frame,
    /// The title is told something: `b` is stored at `a` (a fence, or what
    /// a list writes when it has run).
    Told,
    /// The title asked a fence (`a`) for its value and got `b`, again and
    /// again from the beginning to the end.
    Poll,
    /// Where a thread of the title's was: `a` the instruction, the stack
    /// what on its stack may be return addresses.
    Sample,
}

/// The `b` of a wait for the card's copy queue.
pub(super) const COPY_QUEUE: u64 = u64::MAX;

#[derive(Clone, Copy, Debug)]
pub(super) struct Event {
    pub(super) t0: u64,
    pub(super) t1: u64,
    pub(super) a: u64,
    pub(super) b: u64,
    pub(super) name: &'static str,
    pub(super) thread: u32,
    /// Its place among the stacks, plus one (0: none).
    pub(super) stack: u32,
    pub(super) cpu: u16,
    pub(super) kind: Kind,
}

pub(super) struct Slot {
    pub(super) event: std::cell::UnsafeCell<std::mem::MaybeUninit<Event>>,
    pub(super) written: AtomicBool,
}

/// Events kept, and stacks.
pub(super) const EVENTS: usize = 1 << 20;
pub(super) const STACKS: usize = 1 << 16;
/// Return addresses of a stack.
pub(super) const DEPTH: usize = 14;

pub(super) static ON: AtomicBool = AtomicBool::new(false);
pub(super) static SLOTS: AtomicPtr<Slot> = AtomicPtr::new(std::ptr::null_mut());
pub(super) static NEXT: AtomicUsize = AtomicUsize::new(0);
pub(super) static STACK: AtomicPtr<[usize; DEPTH]> = AtomicPtr::new(std::ptr::null_mut());
pub(super) static NEXT_STACK: AtomicUsize = AtomicUsize::new(0);

/// [`ticks`] per second.
pub(super) fn frequency() -> u64 {
    static TICKS: OnceLock<u64> = OnceLock::new();
    *TICKS.get_or_init(|| {
        let mut ticks = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut ticks) };
        ticks.max(1) as u64
    })
}

/// The performance counter: the clock the card's is calibrated against.
pub(crate) fn ticks() -> u64 {
    let mut ticks = 0;
    // SAFETY: a valid out pointer.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut ticks) };
    ticks as u64
}

pub(super) fn micros(ticks: u64) -> u64 {
    ticks.saturating_mul(1_000_000) / frequency()
}

/// Whether events are recorded now.
#[inline(always)]
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

pub(super) fn thread() -> u32 {
    thread_local!(static ID: Cell<u32> = const { Cell::new(0) });
    ID.with(|id| {
        if id.get() == 0 {
            // SAFETY: a plain query.
            id.set(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() });
        }
        id.get()
    })
}

/// Records an event of this thread.
pub(super) fn put(kind: Kind, name: &'static str, t: (u64, u64), a: u64, b: u64, stack: u32) {
    put_of(thread(), kind, name, t, a, b, stack);
}

/// Records an event of `thread`.
pub(super) fn put_of(
    thread: u32,
    kind: Kind,
    name: &'static str,
    (t0, t1): (u64, u64),
    a: u64,
    b: u64,
    stack: u32,
) {
    let slots = SLOTS.load(Ordering::Acquire);
    if slots.is_null() {
        return;
    }
    let at = NEXT.fetch_add(1, Ordering::Relaxed);
    if at >= EVENTS {
        return;
    }
    // SAFETY: a plain query.
    let cpu = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() } as u16;
    let event = Event { t0, t1, a, b, name, thread, stack, cpu, kind };
    // SAFETY: within the buffer, and this place was given to this call alone.
    unsafe {
        let slot = &*slots.add(at);
        (*slot.event.get()).write(event);
        slot.written.store(true, Ordering::Release);
    }
}

/// The caller's stack: its place, plus one.
fn stack() -> u32 {
    let stacks = STACK.load(Ordering::Acquire);
    if stacks.is_null() {
        return 0;
    }
    let at = NEXT_STACK.fetch_add(1, Ordering::Relaxed);
    if at >= STACKS {
        return 0;
    }
    let mut frames = [0usize; DEPTH];
    // SAFETY: room for `DEPTH` return addresses; the place was given to this
    // call alone.
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(
            1,
            DEPTH as u32,
            frames.as_mut_ptr().cast(),
            std::ptr::null_mut(),
        );
        stacks.add(at).write(frames);
    }
    at as u32 + 1
}

/// Shorter spans of ours are not kept (microseconds).
const SHORTEST: u64 = 5;
/// The stack of a wait or a list call this long is kept (microseconds).
const DEEP: u64 = 50;

/// A span of ours: from where it is made until it is dropped.
pub struct Span {
    name: &'static str,
    /// When it began (0: nothing is recorded).
    began: u64,
}

/// A span named `name`.
#[inline(always)]
pub fn span(name: &'static str) -> Span {
    Span {
        name,
        began: if on() { ticks() } else { 0 },
    }
}

impl Span {
    /// The span ends here and another begins.
    #[inline(always)]
    pub fn next(&mut self, name: &'static str) {
        self.close();
        (self.name, self.began) = (name, if on() { ticks() } else { 0 });
    }

    /// The span ends here.
    #[inline(always)]
    pub fn end(self) {}

    fn close(&mut self) {
        let began = std::mem::take(&mut self.began);
        if began != 0 {
            let now = ticks();
            if micros(now - began) >= SHORTEST {
                put(Kind::Span, self.name, (began, now), 0, 0, 0);
            }
        }
    }
}

impl Drop for Span {
    #[inline(always)]
    fn drop(&mut self) {
        if self.began != 0 {
            self.close();
        }
    }
}

/// List calls closer than this (microseconds) are one event.
const RUN_GAP: u64 = 10;

/// A list call of the title's that took `took` has returned.
pub(crate) fn called(took: std::time::Duration) {
    if !on() {
        return;
    }
    let inside = (took.as_nanos() as u64).saturating_mul(frequency()) / 1_000_000_000;
    let t1 = ticks();
    let t0 = t1 - inside;
    thread_local! {
        /// The run under way: from, to, calls, ticks inside them.
        static RUN: Cell<(u64, u64, u64, u64)> = const { Cell::new((0, 0, 0, 0)) };
    }
    RUN.with(|run| {
        let (from, to, calls, sum) = run.get();
        if calls > 0 && micros(t0.saturating_sub(to)) < RUN_GAP {
            run.set((from, t1, calls + 1, sum + inside));
            return;
        }
        if calls > 0 {
            put(Kind::Calls, "list calls", (from, to), calls, sum, 0);
        }
        run.set((t0, t1, 1, inside));
    });
    if micros(inside) >= DEEP {
        put(Kind::Span, "a long list call", (t0, t1), 0, 0, stack());
    }
}

thread_local! {
    /// What the wait this thread makes now stands for ([`card_wait`]).
    static CARD: Cell<u64> = const { Cell::new(0) };
}

/// While it lives, this thread's waits are for the card.
pub(crate) struct CardWait(());

/// This thread's waits are for the card to reach `fence` (`None`: for its
/// copy queue) until what comes back is dropped.
pub(crate) fn card_wait(fence: Option<u64>) -> CardWait {
    CARD.with(|c| c.set(fence.unwrap_or(COPY_QUEUE)));
    CardWait(())
}

impl Drop for CardWait {
    fn drop(&mut self) {
        CARD.with(|c| c.set(0));
    }
}

/// This thread waited on `object` with `name` since `t0`.
pub(super) fn waited(name: &'static str, object: u64, t0: u64) {
    let t1 = ticks();
    let us = micros(t1 - t0);
    if us >= 2 {
        let stack = if us >= DEEP { stack() } else { 0 };
        put(Kind::Wait, name, (t0, t1), object, CARD.with(Cell::get), stack);
    }
}

/// `object` is about to be signalled.
pub(super) fn woken(name: &'static str, object: u64) {
    let t = ticks();
    put(Kind::Wake, name, (t, t), object, 0, 0);
}

/// A call of the kernel's on `bytes` began at `t0`.
pub(super) fn kernel(name: &'static str, bytes: u64, t0: u64) {
    let t1 = ticks();
    if micros(t1 - t0) >= 20 {
        put(Kind::Kernel, name, (t0, t1), bytes, 0, 0);
    }
}

/// Lists went to the card between `t0` ([`ticks`]) and now, the value
/// `fence` signalled behind them, with the timestamps at `stamp`.
pub(crate) fn sent(t0: u64, fence: u64, stamp: Option<usize>) {
    if on() {
        put(Kind::Sent, "ExecuteCommandLists", (t0, ticks()), fence, stamp.map_or(0, |s| s as u64 + 1), 0);
    }
}

/// `value` is being stored at `at`, where the title looks for it.
#[inline(always)]
pub(crate) fn told(name: &'static str, at: usize, value: u64) {
    if on() {
        let t = ticks();
        put(Kind::Told, name, (t, t), at as u64, value, 0);
    }
}

/// The title asked the fence at `at` for its value, which is `value`.
#[inline(always)]
pub(crate) fn polled(at: usize, value: u64) {
    if !on() {
        return;
    }
    thread_local! {
        /// The same answer, again and again: fence, value, from, to.
        static SAME: Cell<(usize, u64, u64, u64)> = const { Cell::new((0, 0, 0, 0)) };
    }
    let t = ticks();
    SAME.with(|same| {
        let (fence, was, from, to) = same.get();
        if (fence, was) == (at, value) && micros(t.saturating_sub(to)) < 1000 {
            same.set((at, value, from, t));
            return;
        }
        if fence != 0 && micros(to.saturating_sub(from)) >= 100 {
            put(Kind::Poll, "asks a fence", (from, to), fence as u64, was, 0);
        }
        same.set((at, value, t, t));
    });
}

/// Objects threads stand at now, by a hash of theirs: releases of a lock
/// nobody waits for are not recorded.
static STOOD_AT: [AtomicU32; 4096] = [const { AtomicU32::new(0) }; 4096];

fn place(object: u64) -> &'static AtomicU32 {
    &STOOD_AT[(object.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 52) as usize]
}

/// This thread is about to wait for `object`, until what comes back is
/// dropped.
pub(super) fn standing(object: u64) -> Standing {
    place(object).fetch_add(1, Ordering::SeqCst);
    Standing(object)
}

/// See [`standing`].
pub(super) struct Standing(u64);

impl Drop for Standing {
    fn drop(&mut self) {
        place(self.0).fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether a thread may wait for `object`.
pub(super) fn stood_at(object: u64) -> bool {
    place(object).load(Ordering::SeqCst) != 0
}
