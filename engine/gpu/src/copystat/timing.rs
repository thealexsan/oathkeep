//! Per-range copy/compare tracking and thread-level timing diagnostics.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{C, add};

/// Per source range (start, length): copies, bytes, microseconds, memory type.
pub(super) type Ranges = HashMap<(usize, usize), (u64, u64, u64, u32)>;
pub(super) static RANGES: Mutex<Option<Ranges>> = Mutex::new(None);

/// Per range copied whole because its memory is not watched (start,
/// length): copies and microseconds.
pub(super) static WHOLE: Mutex<Option<HashMap<(usize, usize), (u64, u64)>>> = Mutex::new(None);

/// `len` bytes at `src`, memory that is not watched, were copied whole in
/// `us`.
pub(crate) fn whole(src: usize, len: usize, us: u64) {
    add(C::WholeUs, us);
    let mut map = WHOLE.lock().unwrap_or_else(|e| e.into_inner());
    let e = map.get_or_insert_with(HashMap::new).entry((src, len)).or_insert((0, 0));
    e.0 += 1;
    e.1 += us;
}

/// The longest wait for the card since the last line: microseconds, and
/// what ran (lists and pipelines).
pub(super) static LONGEST: Mutex<(u64, String)> = Mutex::new((0, String::new()));

/// The waits for the card since the last line by what ran (lists and
/// pipelines): how many, and their microseconds.
pub(super) static WAITS_BY_WORK: Mutex<Option<HashMap<String, (u64, u64)>>> = Mutex::new(None);

/// With `OATHKEEP_GPU_TIMING`: the commands' time on the card since the last
/// line by what they ran (a pipeline, or the kind of command): how many,
/// their microseconds, and the longest one's.
pub(super) static COMMANDS_BY_WORK: Mutex<Option<HashMap<String, (u64, u64, u64)>>> = Mutex::new(None);

/// A command took `ms` on the card (timing run). `op` is its note: the
/// kind and size first, then the pipeline in brackets, then what it binds.
pub(crate) fn command_took(op: &str, ms: f64) {
    let us = (ms * 1e3) as u64;
    let mut by = COMMANDS_BY_WORK.lock().unwrap_or_else(|e| e.into_inner());
    let e = by.get_or_insert_with(HashMap::new).entry(command_key(op)).or_insert((0, 0, 0));
    e.0 += 1;
    e.1 += us;
    e.2 = e.2.max(us);
}

/// What a command's note is summed under: its pipeline (`[compute ...]`,
/// `[mesh ..., pixel ...]`) with the command's first word, or without a
/// pipeline its first two words.
pub(super) fn command_key(op: &str) -> String {
    let head = op.split(" | ").next().unwrap_or(op);
    let word = head.split(' ').next().unwrap_or("");
    match (head.find('['), head.find(']')) {
        (Some(a), Some(b)) if a < b => format!("{word} {}", &head[a..=b]),
        _ => head.split(' ').take(2).collect::<Vec<_>>().join(" "),
    }
}

/// The card was waited for `us` for a submission; `what` names it if it is
/// the longest since the last line.
pub(crate) fn card_waited(us: u64, what: impl FnOnce() -> String) {
    let (n, t) = match us {
        0..2_000 => (C::CardUnder2, C::CardUnder2Us),
        2_000..8_000 => (C::Card2To8, C::Card2To8Us),
        8_000..32_000 => (C::Card8To32, C::Card8To32Us),
        _ => (C::CardOver32, C::CardOver32Us),
    };
    add(n, 1);
    add(t, us);
    // Every submission is named (some forty a frame): the same lists run
    // every frame, and the eight longest waits of a line were one list
    // eight times.
    let text = what();
    {
        let mut by = WAITS_BY_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let e = by.get_or_insert_with(HashMap::new).entry(text.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 += us;
    }
    let mut longest = LONGEST.lock().unwrap_or_else(|e| e.into_inner());
    if us > longest.0 {
        *longest = (us, text);
    }
}

/// Per range compared after a submission (start, length): times, and the
/// bytes of it written back to the title.
pub(super) static COMPARED: Mutex<Option<HashMap<(usize, usize), (u64, u64)>>> = Mutex::new(None);

/// `len` bytes at `src` were compared after a submission and `written` of
/// them put back into title memory.
pub(crate) fn compared(src: usize, len: usize, written: usize) {
    let mut map = COMPARED.lock().unwrap_or_else(|e| e.into_inner());
    let e = map.get_or_insert_with(HashMap::new).entry((src, len)).or_insert((0, 0));
    e.0 += 1;
    e.1 += written as u64;
}

/// A copy of `len` bytes from `src` (memory type `kind`) took `us`.
pub(crate) fn range(src: usize, len: usize, us: u64, kind: u32) {
    let mut map = RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0, 0, kind));
    e.0 += 1;
    e.1 += len as u64;
    e.2 += us;
}

/// Microseconds since the first call.
fn now_us() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// When the last frame was presented and when the last submission returned
/// ([`now_us`]), and whether a list call and a submission followed that
/// present yet.
static PRESENTED_AT: AtomicU64 = AtomicU64::new(0);
static SUBMITTED_AT: AtomicU64 = AtomicU64::new(0);
static RECORDED_SINCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static SUBMITTED_SINCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// What a thread that submits did between its submissions and in them
/// (microseconds; list calls in nanoseconds).
#[derive(Default, Clone, Copy)]
pub(super) struct Submitter {
    pub(super) submissions: u64,
    /// From a submission's return to the next one's call: wall time, of it
    /// the thread running (user, kernel), and inside our list calls.
    pub(super) between: u64,
    pub(super) between_user: u64,
    pub(super) between_kernel: u64,
    pub(super) between_ours: u64,
    /// Waiting for the submission lock; running inside submissions.
    pub(super) lock: u64,
    pub(super) inside_user: u64,
    pub(super) inside_kernel: u64,
}

/// By thread: the submitting threads, and list-call time (nanoseconds,
/// calls) of every recording thread.
pub(super) static SUBMITTERS: Mutex<Vec<(u32, Submitter)>> = Mutex::new(Vec::new());
pub(super) static RECORDERS: Mutex<Vec<(u32, u64, u64)>> = Mutex::new(Vec::new());

thread_local! {
    /// This thread's last submission: when it returned ([`now_us`]) and the
    /// thread's processor time then (user, kernel).
    static LEFT: std::cell::Cell<Option<(u64, u64, u64)>> = const { std::cell::Cell::new(None) };
    /// The thread's processor time when its running submission was called.
    static ENTERED: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
    /// List-call nanoseconds of this thread since its last submission.
    static OURS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// List-call time not yet added to `RECORDERS` (nanoseconds, calls).
    static UNSENT: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

fn micros(t: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)) / 10
}

/// This thread's processor time so far (user, kernel), in microseconds.
fn thread_times() -> (u64, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: our own thread and four valid out pointers.
    unsafe { GetThreadTimes(GetCurrentThread(), &mut created, &mut exited, &mut kernel, &mut user) };
    (micros(user), micros(kernel))
}

/// The process's processor time so far (user, kernel), in microseconds.
pub(super) fn process_times() -> (u64, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: our own process and four valid out pointers.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    (micros(user), micros(kernel))
}

fn submitter(all: &mut Vec<(u32, Submitter)>, id: u32) -> &mut Submitter {
    let at = match all.iter().position(|s| s.0 == id) {
        Some(at) => at,
        None => {
            all.push((id, Submitter::default()));
            all.len() - 1
        }
    };
    &mut all[at].1
}

/// A list call on this thread that began at `started` has returned.
pub(crate) fn recorded(started: std::time::Instant) {
    let took = started.elapsed();
    let ns = took.as_nanos() as u64;
    crate::timeline::called(took);
    add(C::RecordCalls, 1);
    add(C::RecordNs, ns);
    if !RECORDED_SINCE.load(Relaxed) && !RECORDED_SINCE.swap(true, Relaxed) {
        let began = now_us().saturating_sub(ns / 1000);
        add(C::UntilRecordUs, began.saturating_sub(PRESENTED_AT.load(Relaxed)));
    }
    OURS.with(|o| o.set(o.get() + ns));
    let (unsent, calls) = UNSENT.with(|u| u.get());
    let (unsent, calls) = (unsent + ns, calls + 1);
    // By thread, a couple of milliseconds at a time.
    if unsent < 2_000_000 {
        UNSENT.with(|u| u.set((unsent, calls)));
        return;
    }
    UNSENT.with(|u| u.set((0, 0)));
    let id = thread_id();
    let mut all = RECORDERS.lock().unwrap_or_else(|e| e.into_inner());
    match all.iter_mut().find(|r| r.0 == id) {
        Some(r) => (r.1, r.2) = (r.1 + unsent, r.2 + calls),
        None => all.push((id, unsent, calls)),
    }
}

/// A submission is called on this thread (before it waits for the
/// submission lock).
pub(crate) fn submission_begins() {
    let now = now_us();
    INSIDE.fetch_add(1, Relaxed);
    if !SUBMITTED_SINCE.load(Relaxed) && !SUBMITTED_SINCE.swap(true, Relaxed) {
        add(C::UntilSubmitUs, now.saturating_sub(PRESENTED_AT.load(Relaxed)));
    }
    let (user, kernel) = thread_times();
    ENTERED.with(|e| e.set((user, kernel)));
    let ours = OURS.with(|o| o.replace(0));
    if let Some((left, u, k)) = LEFT.with(|l| l.get()) {
        let mut all = SUBMITTERS.lock().unwrap_or_else(|e| e.into_inner());
        let s = submitter(&mut all, thread_id());
        s.between += now.saturating_sub(left);
        s.between_user += user.saturating_sub(u);
        s.between_kernel += kernel.saturating_sub(k);
        s.between_ours += ours;
    }
}

/// The submission returns; it waited `lock` microseconds for the
/// submission lock.
pub(crate) fn submission_ends(lock: u64) {
    let now = now_us();
    INSIDE.fetch_sub(1, Relaxed);
    SUBMITTED_AT.store(now, Relaxed);
    let (user, kernel) = thread_times();
    let (u, k) = ENTERED.with(|e| e.get());
    LEFT.with(|l| l.set(Some((now, user, kernel))));
    OURS.with(|o| o.set(0));
    let mut all = SUBMITTERS.lock().unwrap_or_else(|e| e.into_inner());
    let s = submitter(&mut all, thread_id());
    s.submissions += 1;
    s.lock += lock;
    s.inside_user += user.saturating_sub(u);
    s.inside_kernel += kernel.saturating_sub(k);
}

/// The submissions being prepared now.
static INSIDE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What the frame is at, for the sampling profiler (`OATHKEEP_PROFILE`): 3
/// in a submission, else 0 after a present until the first list call, 1 until
/// the first submission, 2 behind it.
pub(crate) fn phase() -> u32 {
    if INSIDE.load(Relaxed) > 0 {
        3
    } else if SUBMITTED_SINCE.load(Relaxed) {
        2
    } else {
        u32::from(RECORDED_SINCE.load(Relaxed))
    }
}

/// The title calls its present (before our own copy to the window runs).
pub(crate) fn present_begins() {
    add(C::TailUs, now_us().saturating_sub(SUBMITTED_AT.load(Relaxed)));
}

/// A frame was presented.
pub(crate) fn presented() {
    add(C::Frames, 1);
    PRESENTED_AT.store(now_us(), Relaxed);
    RECORDED_SINCE.store(false, Relaxed);
    SUBMITTED_SINCE.store(false, Relaxed);
}

pub(super) fn page_faults() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    u64::from(c.PageFaultCount)
}
