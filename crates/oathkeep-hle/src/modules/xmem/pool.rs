use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};

use windows_sys::Win32::System::Memory::{
    MEM_PRESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile2,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::mapping::{Call, counted, lean, show, split, unmap};
use super::physical::{PAGE, Physical, Slice, shown_mut};

/// Pages changed under the locks at once.
pub(super) const GROUP: usize = 64;

/// How many threads map a group of pages side by side (`OATHKEEP_MAP_THREADS`;
/// 8 unless set; 1: the caller alone, a page after the other; `auto`: 1 while
/// mapping takes little of the time, [`WIDE`] while the title streams as fast
/// as its pages are mapped, [`Busy`]).
///
/// shortcut: `auto` is not the default. The intro's diner was there in time
/// with it (one run, textures-d3), but in the dorm, where each call takes 0.3
/// ms of something that only one at a time gets (until a step of the mission,
/// after which it takes 0.01), 16 threads map no more pages than one, and the
/// groups hold the mapping lock: 260 ms a frame against 125 (textures-d4, -d2).
///
/// Mapping and unmapping a view wait for a lock that every protection change
/// of a view takes too, and our guards make 100,000 of those a second, from
/// many threads, in the world: 0.2 to 4 ms a page. One thread stands in that
/// queue once at a time. In a bench (60,000 views, five threads changing
/// protections) one thread mapped 1,500 pages a second, eight 30,000.
pub(super) fn side_by_side() -> usize {
    // Diagnostics: 1 and 8 by turns.
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some(on) = oathkeep_gpu::switched("OATHKEEP_MAP_THREADS") {
        return if on { 8 } else { 1 };
    }
    match asked_threads() {
        Some(n) => n,
        None if BUSY.2.load(Ordering::Relaxed) => WIDE,
        None => 1,
    }
}

/// Threads while the title streams fast: they stand in the host's queue side
/// by side, each for a millisecond or two a page in the world's first seconds.
const WIDE: usize = 16;

/// `OATHKEEP_MAP_THREADS` as a number; `None`: by how much there is to map.
fn asked_threads() -> Option<usize> {
    static ASKED: LazyLock<Option<usize>> = LazyLock::new(|| {
        match std::env::var("OATHKEEP_MAP_THREADS") {
            Ok(v) if v.trim() == "auto" => None,
            v => Some(v.ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8).clamp(1, 32)),
        }
    });
    *ASKED
}

/// Of the window under way: when it began, the nanoseconds spent mapping in
/// it; and whether the last one was busy.
static BUSY: (Mutex<Option<std::time::Instant>>, AtomicU64, AtomicBool) =
    (Mutex::new(None), AtomicU64::new(0), AtomicBool::new(false));

/// Counts the time a mapping call of the title's takes, as long as it lives.
/// More than a quarter of a window, and the calls are what loading waits for
/// (one thread of the title's makes them); less than a twentieth, they are
/// not any more.
pub(super) struct Busy(std::time::Instant);

impl Busy {
    pub(super) fn begin() -> Busy {
        Busy(std::time::Instant::now())
    }
}

/// Whether mapping is busy after a window of `window` with `spent` in it.
pub(super) fn busy_after(was: bool, spent: std::time::Duration, window: std::time::Duration) -> bool {
    spent * if was { 20 } else { 4 } > window
}

impl Drop for Busy {
    fn drop(&mut self) {
        const WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
        let now = std::time::Instant::now();
        let all = BUSY.1.fetch_add((now - self.0).as_nanos() as u64, Ordering::Relaxed)
            + (now - self.0).as_nanos() as u64;
        let mut began = BUSY.0.lock().unwrap_or_else(|e| e.into_inner());
        let window = now - *began.get_or_insert(self.0);
        if window < WINDOW {
            return;
        }
        *began = Some(now);
        BUSY.1.store(0, Ordering::Relaxed);
        let was = BUSY.2.load(Ordering::Relaxed);
        let is = busy_after(was, std::time::Duration::from_nanos(all), window);
        if was != is {
            BUSY.2.store(is, Ordering::Relaxed);
            static SAID: AtomicU64 = AtomicU64::new(0);
            if asked_threads().is_none() && SAID.fetch_add(1, Ordering::Relaxed) < 200 {
                tracing::info!(
                    "title pages are mapped by {} (OATHKEEP_MAP_THREADS): {:.0} ms of the last {:.0} ms went into mapping calls",
                    if is { "several threads" } else { "the caller alone" },
                    all as f64 / 1e6,
                    window.as_secs_f64() * 1e3
                );
            }
        }
    }
}

/// A page of address space to unmap and, with a slice, map anew.
pub(super) struct Op {
    at: usize,
    slice: Option<Slice>,
    /// A view of this page alone is there, to be unmapped.
    had: bool,
    /// The placeholder is the page's own (or will be, after the unmapping).
    own: bool,
}

impl Op {
    pub(super) fn new(at: usize, slice: Option<Slice>) -> Op {
        Op { at, slice, had: false, own: false }
    }

    /// The system calls; whether the host did as asked.
    fn run(&self) -> bool {
        if self.had {
            // SAFETY: a view we mapped; it becomes a placeholder again.
            let gone = counted(Call::Unmap, || unsafe {
                UnmapViewOfFile2(
                    GetCurrentProcess(),
                    MEMORY_MAPPED_VIEW_ADDRESS { Value: self.at as *mut c_void },
                    MEM_PRESERVE_PLACEHOLDER,
                )
            });
            if gone == 0 {
                return false;
            }
        }
        let Some(slice) = &self.slice else {
            return true;
        };
        show(self.at, PAGE, slice, self.own)
    }
}

/// Does `ops`, each of another page, side by side: first what is noted of
/// the pages goes, then the system calls, then what is there now is noted.
/// With [`PHYSICAL`] and the change of mapping (`title_memory_remapping`)
/// held.
pub(super) fn change(physical: &mut Physical, mut ops: Vec<Op>) -> bool {
    for op in &mut ops {
        let there = physical
            .views
            .range(..=op.at)
            .next_back()
            .filter(|(base, view)| op.at < **base + view.0)
            .map(|(base, view)| (*base, view.0));
        match there {
            Some((base, PAGE)) if base == op.at => {
                if let Some((size, slice)) = physical.views.remove(&op.at) {
                    shown_mut().forget(op.at, size, &slice);
                }
                oathkeep_gpu::title_memory_unmapping(op.at, PAGE);
                physical.holes.remove(&op.at);
                (op.had, op.own) = (true, lean());
            }
            // Part of a larger view.
            Some(_) => {
                unmap(physical, op.at, PAGE);
            }
            None => {}
        }
        if op.had {
            continue;
        }
        op.own = physical.holes.remove(&op.at) == Some(PAGE) && lean();
        if op.slice.is_some() && !op.own {
            // Cut out of a larger placeholder here, not side by side: a
            // thread that maps a piece while others cut the same one can
            // fail.
            split(op.at, PAGE);
            op.own = true;
        }
    }
    let done = all(ops);
    let mut ok = true;
    for (op, done) in done {
        match op.slice {
            Some(slice) if done => {
                // As in `map`.
                shown_mut().note(op.at, PAGE, &slice);
                oathkeep_gpu::title_memory_remapped(op.at, PAGE);
                oathkeep_gpu::title_memory_view(op.at, PAGE, slice.section.handle as usize, (slice.index * PAGE) as u64);
                physical.views.insert(op.at, (PAGE, slice));
            }
            None if op.had && done => {
                physical.holes.insert(op.at, PAGE);
            }
            _ => ok &= done,
        }
    }
    ok
}

struct Batch {
    ops: Vec<(Op, AtomicBool)>,
    next: AtomicUsize,
    /// Ops not done yet.
    left: AtomicUsize,
    over: (Mutex<bool>, Condvar),
}

impl Batch {
    /// Does ops until none is left to begin.
    fn work(&self) {
        loop {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            let Some((op, done)) = self.ops.get(i) else {
                return;
            };
            done.store(op.run(), Ordering::Release);
            if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                *self.over.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                self.over.1.notify_all();
            }
        }
    }
}

struct Pool {
    batches: Mutex<VecDeque<Arc<Batch>>>,
    more: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let pool = POOL.get_or_init(|| Pool { batches: Mutex::default(), more: Condvar::new() });
    STARTED.call_once(|| {
        let started = (1..asked_threads().unwrap_or(WIDE).max(side_by_side()))
            .filter(|i| std::thread::Builder::new().name(format!("mapping {i}")).spawn(|| serve(pool)).is_ok())
            .count();
        tracing::info!(started, "threads that map pages side by side (OATHKEEP_MAP_THREADS=1: none)");
    });
    pool
}

fn serve(pool: &Pool) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: a plain call on our own thread.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    let mut batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let Some(batch) = batches.front().cloned() else {
            batches = pool.more.wait(batches).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        if batch.next.load(Ordering::Relaxed) >= batch.ops.len() {
            // All begun: nobody needs to see it any more.
            if batches.front().is_some_and(|b| Arc::ptr_eq(b, &batch)) {
                batches.pop_front();
            }
            continue;
        }
        drop(batches);
        batch.work();
        batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
    }
}

/// Runs `ops`; each with whether the host did it.
fn all(ops: Vec<Op>) -> Vec<(Op, bool)> {
    // Waking the others takes longer than a few quick calls.
    if ops.len() < 3 {
        return ops
            .into_iter()
            .map(|op| {
                let done = op.run();
                (op, done)
            })
            .collect();
    }
    let n = ops.len();
    let batch = Arc::new(Batch {
        ops: ops.into_iter().map(|op| (op, AtomicBool::new(false))).collect(),
        next: AtomicUsize::new(0),
        left: AtomicUsize::new(n),
        over: Default::default(),
    });
    let pool = pool();
    pool.batches.lock().unwrap_or_else(|e| e.into_inner()).push_back(batch.clone());
    if n > side_by_side() {
        pool.more.notify_all();
    } else {
        (1..n).for_each(|_| pool.more.notify_one());
    }
    batch.work();
    let mut over = batch.over.0.lock().unwrap_or_else(|e| e.into_inner());
    while !*over {
        over = batch.over.1.wait(over).unwrap_or_else(|e| e.into_inner());
    }
    drop(over);
    // No worker changes it any more (one may still hold it for a moment).
    batch
        .ops
        .iter()
        .map(|(op, done)| {
            let op = Op { at: op.at, slice: op.slice.clone(), had: op.had, own: op.own };
            (op, done.load(Ordering::Acquire))
        })
        .collect()
}
