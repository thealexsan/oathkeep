use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Copies `len` bytes, on several threads if they are many: a snapshot of
/// the world's big heap is 100-170 MB per submission, 2-4 GB a frame, and
/// one thread copies some 8 GB/s.
///
/// # Safety
/// Both ranges are valid for `len` bytes and do not overlap.
pub(super) unsafe fn copy_wide(src: *const u8, dst: *mut u8, len: usize) {
    const PER_THREAD: usize = 16 << 20;
    let threads = (len / PER_THREAD).clamp(1, 6);
    if threads == 1 {
        // SAFETY: the caller's.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
        return;
    }
    let (src, dst, each) = (src as usize, dst as usize, len.div_ceil(threads));
    std::thread::scope(|s| {
        for i in 0..threads {
            let at = (i * each).min(len);
            let n = each.min(len - at);
            s.spawn(move || {
                // SAFETY: a part of the caller's ranges.
                unsafe { std::ptr::copy_nonoverlapping((src + at) as *const u8, (dst + at) as *mut u8, n) };
            });
        }
    });
}

/// Whether the snapshots of a submission's shader-written ranges, and the
/// compare that finds what the shaders wrote, run on several threads
/// across ranges (`OATHKEEP_WIDE_BACK=1`; off by default).
///
/// Both ran range by range on the submitting thread, a range on several
/// threads only if it was 16 MiB or more. The ranges left on the CPU's side
/// since the big buffers went to the card are small and many: in run 86,
/// 7,024 declared ranges a frame, 375 MB of them compared after the card
/// ran in 563 pieces of work, none of 16 MiB: 51 ms a frame of snapshots
/// and 35 ms of comparing, one thread, bound by one core's memory speed.
///
/// With the switch a submission's snapshots are noted as it is prepared
/// and taken together when its refreshes are done, just before it goes to
/// the card ([`take_snapshots`]), and the compares of everything declared
/// run together after the card ([`find_all`]), both cut into pieces of
/// work of about 1 MiB for a few threads that are kept ([`wide`]). The
/// bytes taken and the bytes compared are the same ones: a range's bytes
/// cannot change between the moment it is declared and the end of the
/// preparing (a declared range is left alone by every refresh, the title's
/// own copies write title memory and not the copy, and the card runs the
/// lists only afterwards).
pub(crate) fn wide_back() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default; `OATHKEEP_WIDE_BACK=0` turns it off.
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WIDE_BACK").is_none_or(|v| v != "0"))
}

pub(super) type Job = Box<dyn FnOnce() + Send>;

/// Threads kept for [`wide`]: what they are to do, and the word that there
/// is something.
struct Workers {
    jobs: Mutex<std::collections::VecDeque<Job>>,
    more: std::sync::Condvar,
    /// How many jobs there are, for [`spin`].
    there: std::sync::atomic::AtomicUsize,
}

/// Waits awake until `there` says so, `OATHKEEP_WIDE_SPIN` microseconds at
/// most (25, the default; `0`: sleep immediately): asleep, a thread is woken
/// in 50–150 us, ~95 times a frame across refreshes and snapshots.
fn spin(there: impl Fn() -> bool) {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let most = *MOST.get_or_init(|| std::env::var("OATHKEEP_WIDE_SPIN").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(25));
    if most == 0 || there() {
        return;
    }
    let began = std::time::Instant::now();
    while !there() && (began.elapsed().as_micros() as u64) < most {
        for _ in 0..16 {
            std::hint::spin_loop();
        }
    }
}

/// How many helper threads are kept for [`wide`] (plus the calling thread:
/// e.g. 5 helpers + 1 caller = 6 threads on a 6c/12t CPU).
fn wide_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| (n.get() / 2).saturating_sub(1))
        .clamp(2, 6)
}

fn workers() -> &'static Workers {
    static WORKERS: std::sync::OnceLock<Workers> = std::sync::OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let workers = WORKERS.get_or_init(|| Workers {
        jobs: Mutex::new(std::collections::VecDeque::new()),
        more: std::sync::Condvar::new(),
        there: std::sync::atomic::AtomicUsize::new(0),
    });
    STARTED.call_once(|| {
        for i in 0..wide_threads() {
            let _ = std::thread::Builder::new().name(format!("victory-wide-{i}")).spawn(move || {
                crate::cores::keep_helper_on_fast_cores("victory-wide");
                let workers = WORKERS.get().expect("made above");
                loop {
                    let job = {
                        // A little while awake first: the next work comes
                        // within microseconds more often than not.
                        spin(|| workers.there.load(Ordering::Acquire) > 0);
                        let mut jobs = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(job) = jobs.pop_front() {
                                workers.there.fetch_sub(1, Ordering::AcqRel);
                                break job;
                            }
                            jobs = workers.more.wait(jobs).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let _working = crate::timeline::span("a job for another thread");
                    job();
                }
            });
        }
    });
    workers
}

/// Runs `jobs` on the kept threads and on this one, and returns when all
/// of them are done. The jobs copy and compare memory; they do not panic.
pub(super) fn wide(jobs: Vec<Job>) {
    if jobs.len() < 2 {
        for job in jobs {
            job();
        }
        return;
    }
    let workers = workers();
    let _wide = crate::timeline::span("work spread over threads");
    let left = Arc::new((Mutex::new(jobs.len()), std::sync::Condvar::new()));
    {
        let mut queue = workers.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs {
            let left = left.clone();
            workers.there.fetch_add(1, Ordering::AcqRel);
            queue.push_back(Box::new(move || {
                job();
                let mut n = left.0.lock().unwrap_or_else(|e| e.into_inner());
                *n -= 1;
                if *n == 0 {
                    left.1.notify_all();
                }
            }));
        }
    }
    workers.more.notify_all();
    loop {
        let job = workers.jobs.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        match job {
            Some(job) => {
                workers.there.fetch_sub(1, Ordering::AcqRel);
                job()
            }
            None => break,
        }
    }
    spin(|| left.0.try_lock().is_ok_and(|n| *n == 0));
    let mut n = left.0.lock().unwrap_or_else(|e| e.into_inner());
    while *n > 0 {
        n = left.1.wait(n).unwrap_or_else(|e| e.into_inner());
    }
}

/// [`wide`], for the host's own work on host copies (a submission's
/// refreshes, the merge of what shaders wrote to card copies).
pub(crate) fn wide_run(jobs: Vec<Box<dyn FnOnce() + Send>>) {
    wide(jobs);
}

/// Calls `work` with every number under `n`, on the kept threads and on
/// this one, the next number to whichever is free: uses [`crate::land::each_wide`]
/// to avoid per-call `Arc`/`Box`/`VecDeque` allocations and Condvar straggler waits.
pub(crate) fn wide_each(n: usize, work: impl Fn(usize) + Send + Sync + 'static) {
    crate::land::each_wide(n, &work);
}
