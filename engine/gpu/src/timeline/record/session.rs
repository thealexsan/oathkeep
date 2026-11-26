//! Timeline recording session lifecycle, thread sampling, and buffer management.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use super::{
    DEPTH, EVENTS, Event, Kind, NEXT, NEXT_STACK, ON, SLOTS, STACK, STACKS, frequency, on, put,
    put_of, thread, ticks,
};

/// Threads of the title's that submit or present.
static TITLES: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// This thread is one of the title's that submit: what it does is sampled.
#[inline(always)]
pub(crate) fn title_thread() {
    if !on() {
        return;
    }
    let id = thread();
    for place in &TITLES {
        match place.load(Ordering::Relaxed) {
            there if there == id => return,
            0 if place.compare_exchange(0, id, Ordering::Relaxed, Ordering::Relaxed).is_ok() => return,
            _ => {}
        }
    }
}

/// Looks every quarter of a millisecond where the title's submitting threads
/// are, while the recording goes on: what they do outside our calls (a loop
/// on a value in memory has no event of its own).
fn sample() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, CONTEXT_CONTROL_AMD64, GetThreadContext};
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_SUSPEND_RESUME,
    };
    /// `GetThreadContext` wants a 16-byte aligned context.
    #[repr(C, align(16))]
    struct Aligned(CONTEXT);
    let mut open: Vec<(u32, *mut std::ffi::c_void)> = Vec::new();
    while on() {
        for place in &TITLES {
            let id = place.load(Ordering::Relaxed);
            if id == 0 {
                continue;
            }
            let thread = match open.iter().find(|o| o.0 == id) {
                Some(o) => o.1,
                None => {
                    // SAFETY: a thread of our own process.
                    let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, 0, id) };
                    open.push((id, handle));
                    handle
                }
            };
            if thread.is_null() {
                continue;
            }
            let mut words = [0usize; 64];
            // SAFETY: zeroed is a valid CONTEXT; the thread is another one of
            // this process and runs on before anything here allocates or
            // locks; its stack is read from its pointer to the end of that
            // page alone.
            let at = unsafe {
                let mut context = Aligned(std::mem::zeroed());
                context.0.ContextFlags = CONTEXT_CONTROL_AMD64;
                if SuspendThread(thread) == u32::MAX {
                    continue;
                }
                let known = GetThreadContext(thread, &mut context.0) != 0;
                if known {
                    let sp = context.0.Rsp as usize & !7;
                    let n = ((sp | 0xfff) + 1 - sp) / 8;
                    for (i, word) in words.iter_mut().enumerate().take(n) {
                        *word = (sp as *const usize).add(i).read_volatile();
                    }
                }
                ResumeThread(thread);
                if known { context.0.Rip as usize } else { 0 }
            };
            let t = ticks();
            // What lies near the instruction may be a return address; where
            // the instruction is the system's, all of it may tell something.
            let mut frames = [0usize; DEPTH];
            let near = words.iter().copied().filter(|w| *w != 0 && (at >> 40 == 0x7f || w.abs_diff(at) < 0x1000_0000));
            for (frame, word) in frames.iter_mut().zip(near) {
                *frame = word;
            }
            put_of(id, Kind::Sample, "a sample", (t, t), at as u64, 0, keep(frames));
        }
        let until = ticks() + frequency() / 4000;
        while ticks() < until {
            std::hint::spin_loop();
        }
    }
    for (_, handle) in open {
        // SAFETY: opened above.
        unsafe { CloseHandle(handle) };
    }
}

/// `frames` among the stacks: its place, plus one.
pub(in crate::timeline) fn keep(frames: [usize; DEPTH]) -> u32 {
    let stacks = STACK.load(Ordering::Acquire);
    let at = NEXT_STACK.fetch_add(1, Ordering::Relaxed);
    if stacks.is_null() || at >= STACKS {
        return 0;
    }
    // SAFETY: the place was given to this call alone.
    unsafe { stacks.add(at).write(frames) };
    at as u32 + 1
}

/// What was asked for.
struct Asked {
    file: std::path::PathBuf,
    /// Seconds after which a recording begins, for each.
    after: Vec<f64>,
    frames: u64,
    started: std::time::Instant,
}

/// `<from s>[+<from s>..],<frames>,<file>`, or a file alone.
pub(super) fn parse(text: &str) -> (Option<(Vec<f64>, u64)>, &str) {
    let mut parts = text.splitn(3, ',');
    let three = (parts.next(), parts.next(), parts.next());
    if let (Some(from), Some(frames), Some(file)) = three
        && let Ok(from) = from.split('+').map(|f| f.trim().parse::<f64>()).collect::<Result<Vec<f64>, _>>()
        && let Ok(frames) = frames.trim().parse::<u64>()
    {
        return (Some((from, frames)), file);
    }
    (None, text)
}

/// The variables, read once (before the title runs: its imports are bound
/// by them).
fn asked() -> Option<&'static Asked> {
    static ASKED: OnceLock<Option<Asked>> = OnceLock::new();
    ASKED
        .get_or_init(|| {
            let file = std::env::var_os("OATHKEEP_TIMELINE").filter(|v| !v.is_empty())?;
            let number = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<f64>().ok());
            let text = file.to_string_lossy();
            let (when, file) = parse(&text);
            let (after, frames) = when.unwrap_or_else(|| {
                (
                    vec![number("OATHKEEP_TIMELINE_AT").unwrap_or(60.0)],
                    number("OATHKEEP_TIMELINE_FRAMES").map_or(3, |n| n as u64),
                )
            });
            Some(Asked {
                file: file.into(),
                after,
                frames: frames.clamp(1, 60),
                started: std::time::Instant::now(),
            })
        })
        .as_ref()
}

/// Whether a timeline was asked for.
pub(in crate::timeline) fn wanted() -> bool {
    asked().is_some()
}

/// Where the recording is.
struct Stage {
    ready: bool,
    began: bool,
    frames: u64,
    /// Recordings made, and whether one is being written.
    made: usize,
    writing: bool,
    before: Option<super::super::report::Threads>,
}

/// The title's present came back.
pub(crate) fn presented() {
    let Some(asked) = asked() else {
        return;
    };
    static STAGE: Mutex<Stage> = Mutex::new(Stage {
        ready: false,
        began: false,
        frames: 0,
        made: 0,
        writing: false,
        before: None,
    });
    let mut stage = STAGE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(&after) = asked.after.get(stage.made) else {
        return;
    };
    if stage.writing || asked.started.elapsed().as_secs_f64() < after {
        return;
    }
    if !stage.ready {
        // The buffers, the card's lists and the threads' clocks take a
        // while: the recording begins two frames later.
        stage.ready = true;
        make_room();
        crate::host::timeline_ready();
        return;
    }
    if !stage.began {
        stage.began = true;
        stage.before = Some(super::super::report::Threads::now());
        tracing::info!(file = %asked.file.display(), frames = asked.frames, "the timeline begins (OATHKEEP_TIMELINE)");
        begin();
        if std::thread::Builder::new().name("victory-sample".into()).spawn(sample).is_err() {
            tracing::warn!("the timeline is without samples of the title's threads");
        }
        frame();
        return;
    }
    frame();
    stage.frames += 1;
    if stage.frames < asked.frames {
        return;
    }
    stage.writing = true;
    let before = stage.before.take().unwrap_or_default();
    // Several recordings: a file each.
    let file = match asked.after.len() {
        1 => asked.file.clone(),
        _ => {
            let end = asked.file.extension().map_or("json".into(), |e| e.to_string_lossy().into_owned());
            asked.file.with_extension(format!("{after:.0}.{end}"))
        }
    };
    drop(stage);
    let write = move || {
        let after = super::super::report::Threads::now();
        // The spans under way at the last present end first.
        std::thread::sleep(std::time::Duration::from_millis(250));
        let data = end();
        super::super::report::write(&file, data, &before, &after, crate::host::timeline_stamps());
        crate::host::timeline_restart();
        let mut stage = STAGE.lock().unwrap_or_else(|e| e.into_inner());
        (stage.began, stage.frames, stage.writing) = (false, 0, false);
        stage.made += 1;
    };
    if std::thread::Builder::new().name("victory-timeline".into()).spawn(write).is_err() {
        tracing::warn!("no thread to write the timeline");
    }
}

/// A frame ends now.
pub(in crate::timeline) fn frame() {
    let t = ticks();
    put(Kind::Frame, "present", (t, t), 0, 0, 0);
}

/// Makes the buffers (kept for good: a thread may be about to write).
pub(in crate::timeline) fn make_room() {
    if !SLOTS.load(Ordering::Acquire).is_null() {
        return;
    }
    fn zeroed<T>(n: usize) -> *mut T {
        let layout = std::alloc::Layout::array::<T>(n).expect("a small number");
        // SAFETY: not of no size.
        unsafe { std::alloc::alloc_zeroed(layout) }.cast()
    }
    STACK.store(zeroed(STACKS), Ordering::Release);
    SLOTS.store(zeroed(EVENTS), Ordering::Release);
}

/// Recording starts.
pub(in crate::timeline) fn begin() {
    ON.store(true, Ordering::SeqCst);
}

/// What was recorded, in no order.
pub(in crate::timeline) struct Data {
    pub(in crate::timeline) events: Vec<Event>,
    pub(in crate::timeline) stacks: Vec<[usize; DEPTH]>,
    /// Events there was no room for.
    pub(in crate::timeline) lost: usize,
}

/// Recording ends.
pub(in crate::timeline) fn end() -> Data {
    ON.store(false, Ordering::SeqCst);
    // Those about to write do.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let taken = NEXT.load(Ordering::SeqCst);
    TITLES.iter().for_each(|t| t.store(0, Ordering::SeqCst));
    let slots = SLOTS.load(Ordering::Acquire);
    let mut events = Vec::with_capacity(taken.min(EVENTS));
    for at in 0..taken.min(EVENTS) {
        // SAFETY: within the buffer; a written place is never written again.
        unsafe {
            let slot = &*slots.add(at);
            if slot.written.load(Ordering::Acquire) {
                events.push((*slot.event.get()).assume_init());
                slot.written.store(false, Ordering::Release);
            }
        }
    }
    NEXT.store(0, Ordering::SeqCst);
    let count = NEXT_STACK.swap(0, Ordering::SeqCst).min(STACKS);
    // SAFETY: that many were given out; each was written before the event
    // that names it.
    let stacks = unsafe { std::slice::from_raw_parts(STACK.load(Ordering::Acquire), count) }.to_vec();
    Data {
        events,
        stacks,
        lost: taken.saturating_sub(EVENTS),
    }
}
