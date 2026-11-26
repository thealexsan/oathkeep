use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use super::threads;

/// Microseconds a pool thread spins politely before sleeping on a Condvar,
/// avoiding ~50–150 us kernel wake latency between back-to-back scan/card/back
/// and flush/snapshot/send batches (`OATHKEEP_LAND_SPIN`, default 60).
fn pool_spin_us() -> u64 {
    static US: OnceLock<u64> = OnceLock::new();
    *US.get_or_init(|| std::env::var("OATHKEEP_LAND_SPIN").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(60))
}

thread_local! {
    /// Tracks the [`Pool`] currently executing on this thread so nested calls
    /// to the same pool execute inline without deadlocking on `Pool::turn`.
    static ACTIVE_POOL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct ActivePoolGuard(usize);

impl ActivePoolGuard {
    fn enter(pool: &Pool) -> Self {
        let prev = ACTIVE_POOL.with(|p| p.replace(std::ptr::from_ref(pool) as usize));
        Self(prev)
    }
}

impl Drop for ActivePoolGuard {
    fn drop(&mut self) {
        ACTIVE_POOL.with(|p| p.set(self.0));
    }
}

fn spin_until(ready: impl Fn() -> bool) {
    let most = pool_spin_us();
    if most == 0 || ready() {
        return;
    }
    let began = std::time::Instant::now();
    while !ready() && (began.elapsed().as_micros() as u64) < most {
        for _ in 0..16 {
            std::hint::spin_loop();
        }
    }
}

/// Threads kept for landing, and the work they are at.
pub(super) struct Pool {
    state: Mutex<State>,
    more: Condvar,
    idle: Condvar,
    /// Lock-free index cursor within the active batch (`0..count`).
    next: AtomicUsize,
    /// Active batch epoch (> 0 while a batch has unclaimed work) and active helper count,
    /// allowing lock-free polite spin before locking `state`.
    live_epoch: AtomicU64,
    active_helpers: AtomicUsize,
    /// One batch at a time.
    turn: Mutex<()>,
    threads: usize,
}

struct State {
    /// What to do with each number below `count`.
    work: Option<Work>,
    count: usize,
    epoch: u64,
    /// Helpers inside a batch.
    running: usize,
    panicked: bool,
}

#[derive(Clone, Copy)]
struct Work(*const (dyn Fn(usize) + Sync));

// SAFETY: the closure may be called from any thread (`Sync`), and
// `Pool::each` keeps it alive until no helper is inside a call.
unsafe impl Send for Work {}

impl Pool {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn new(name: &'static str, threads: usize) -> &'static Pool {
        let pool: &'static Pool = Box::leak(Box::new(Pool {
            state: Mutex::new(State { work: None, count: 0, epoch: 0, running: 0, panicked: false }),
            more: Condvar::new(),
            idle: Condvar::new(),
            next: AtomicUsize::new(0),
            live_epoch: AtomicU64::new(0),
            active_helpers: AtomicUsize::new(0),
            turn: Mutex::new(()),
            threads,
        }));
        for i in 1..threads {
            // (A helper that is not there is only missed.)
            let _ = std::thread::Builder::new().name(format!("{name}-{i}")).spawn(move || {
                if name == "victory-land" {
                    crate::cores::keep_helper_on_fast_cores(name);
                } else {
                    crate::cores::keep_many_on_fast_cores(name);
                }
                pool.help()
            });
        }
        pool
    }

    fn help(&self) {
        let mut seen_epoch = 0u64;
        loop {
            spin_until(|| {
                let e = self.live_epoch.load(Ordering::Acquire);
                e != 0 && e != seen_epoch
            });
            let (work, count) = {
                let mut state = self.state();
                loop {
                    if let Some(work) = state.work
                        && state.epoch != seen_epoch
                        && self.next.load(Ordering::Relaxed) < state.count
                    {
                        seen_epoch = state.epoch;
                        state.running += 1;
                        self.active_helpers.fetch_add(1, Ordering::Release);
                        break (work, state.count);
                    }
                    state = self.more.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            };
            let working = crate::timeline::span("a landing's job");
            let _active = ActivePoolGuard::enter(self);
            let mut panicked = false;
            loop {
                let i = self.next.fetch_add(1, Ordering::Relaxed);
                if i >= count {
                    self.live_epoch.store(0, Ordering::Relaxed);
                    break;
                }
                // SAFETY: counted in `running`, so `each` has not returned.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { (*work.0)(i) }));
                if ran.is_err() {
                    panicked = true;
                    // Stop any further work in this batch on panic.
                    self.next.store(count, Ordering::Relaxed);
                    self.live_epoch.store(0, Ordering::Relaxed);
                    break;
                }
            }
            working.end();
            let mut state = self.state();
            state.running -= 1;
            state.panicked |= panicked;
            let done = state.running == 0;
            self.active_helpers.fetch_sub(1, Ordering::Release);
            if done {
                self.idle.notify_all();
            }
        }
    }

    /// Calls `f` with every number below `count`, on the pool's threads and
    /// this one, and returns when every call has.
    pub(super) fn each(&self, count: usize, f: &(dyn Fn(usize) + Sync)) {
        let self_addr = std::ptr::from_ref(self) as usize;
        if count < 2 || self.threads < 2 || ACTIVE_POOL.with(|p| p.get() == self_addr) {
            (0..count).for_each(f);
            return;
        }
        let _spread = crate::timeline::span("landing: spread over threads");
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        let _active = ActivePoolGuard::enter(self);
        // SAFETY: the lifetime is left out only until this call returns, and
        // it does not before no helper is inside `f` and none can enter.
        let work = Work(unsafe { std::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), *const (dyn Fn(usize) + Sync + 'static)>(f) });
        self.next.store(0, Ordering::Relaxed);
        let epoch = {
            let mut state = self.state();
            state.epoch = state.epoch.wrapping_add(1).max(1);
            (state.work, state.count, state.panicked) = (Some(work), count, false);
            state.epoch
        };
        self.live_epoch.store(epoch, Ordering::Release);
        if count > self.threads {
            self.more.notify_all();
        } else {
            (1..count).for_each(|_| self.more.notify_one());
        }
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            loop {
                let i = self.next.fetch_add(1, Ordering::Relaxed);
                if i >= count {
                    self.live_epoch.store(0, Ordering::Relaxed);
                    break;
                }
                f(i);
            }
        }));
        if ran.is_err() {
            self.next.store(count, Ordering::Relaxed);
        }
        self.live_epoch.store(0, Ordering::Release);
        spin_until(|| self.active_helpers.load(Ordering::Acquire) == 0);
        let mut state = self.state();
        // (After a panic here: nothing more is begun.)
        state.work = None;
        while state.running > 0 {
            state = self.idle.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        let panicked = state.panicked;
        drop(state);
        if let Err(panic) = ran {
            std::panic::resume_unwind(panic);
        }
        assert!(!panicked, "a job of landing panicked");
    }
}

/// [`Pool::each`] on the threads kept for landing.
pub(crate) fn each(count: usize, f: &(dyn Fn(usize) + Sync)) {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool::new("victory-land", threads())).each(count, f);
}

/// [`Pool::each`] on threads kept for the title's bytes under flights' writes
/// (`OATHKEEP_UNDER_THREADS`, 6 with this one).
pub(crate) fn each_under(count: usize, f: &(dyn Fn(usize) + Sync)) {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("OATHKEEP_UNDER_THREADS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(6usize);
        Pool::new("victory-under", threads.clamp(1, 16))
    })
    .each(count, f);
}

/// Calls `f` with every number below `count`, on this thread and on threads
/// kept for making descriptors (`OATHKEEP_VIEW_THREADS`, 4 with this one, 6 for records; 1:
/// on this one alone). Those for `records`, the heaps' records copied as
/// bytes, are others than those for the views picked by index: submissions
/// wait for the first, lists for the second, and each kept the other waiting.
pub(crate) fn each_view(records: bool, count: usize, f: &(dyn Fn(usize) + Sync)) {
    static POOLS: [OnceLock<&'static Pool>; 2] = [const { OnceLock::new() }; 2];
    POOLS[usize::from(records)]
        .get_or_init(|| {
            let threads = std::env::var("OATHKEEP_VIEW_THREADS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(if records { 6usize } else { 4 });
            Pool::new(if records { "victory-records" } else { "victory-views" }, threads.clamp(1, 16))
        })
        .each(count, f);
}

/// [`Pool::each`] on the fast-core helper pool backing `shadow::wide_each`,
/// avoiding per-call `Arc`/`Box`/`VecDeque` allocations and straggler waits.
pub(crate) fn each_wide(count: usize, f: &(dyn Fn(usize) + Sync)) {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map_or(5, |n| n.get() / 2)
            .clamp(3, 7);
        Pool::new("victory-wide-idx", threads)
    })
    .each(count, f);
}
