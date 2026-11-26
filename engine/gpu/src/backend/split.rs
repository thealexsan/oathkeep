use super::*;

/// The numbers of [`HostList::epoch`].
pub(super) fn next_epoch() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A table by a copy's address.
pub(crate) type ByCopy<T> = std::collections::HashMap<usize, T, std::hash::BuildHasherDefault<AddressHash>>;

/// A hash for addresses of ours: the bits that tell them apart, spread. The
/// tables it serves are asked 40,000 times a frame, and hold no one's keys
/// but our own.
#[derive(Default)]
pub(crate) struct AddressHash(u64);

impl std::hash::Hasher for AddressHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_usize(usize::from(b));
        }
    }
    fn write_usize(&mut self, n: usize) {
        let h = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
    fn write_u64(&mut self, n: u64) {
        self.write_usize(n as usize);
    }
    fn write_u32(&mut self, n: u32) {
        self.write_usize(n as usize);
    }
}

/// Since the last frame-rate line, nanoseconds of the submitting thread's
/// preparing: the write-watch asking for a submission; the title's bytes put
/// in under flights' writes; the lists' steps, the refreshes among them; the
/// snapshots; what goes to copies in the card's memory; the lists sent; and
/// of the steps: the refreshes, and the copies and fills (those with the
/// refreshes a copy of title memory flushes). Of the asking, which ranges it
/// is of. Of the refreshes, those before ranges are given to shaders, one
/// copy at a time: nanoseconds, and how many. Ahead of all that: the wait for
/// the submission lock, the steps done on the card planned ([`card_steps`]),
/// the flights waited for, the heaps' records and what is swept; and, at the
/// end, the wait for the heaps' records.
#[cfg(feature = "diagnostics")]
pub(super) static SPLIT: [std::sync::atomic::AtomicU64; 16] = [const { std::sync::atomic::AtomicU64::new(0) }; 16];

/// Since the last frame-rate line: flushes of refreshes, their refreshes,
/// those known to copy nothing, and those done on several threads.
#[cfg(feature = "diagnostics")]
pub(super) static FLUSHES: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Counts into [`SPLIT`].
#[cfg(feature = "diagnostics")]
pub(super) struct Split(std::cell::Cell<std::time::Instant>);

#[cfg(feature = "diagnostics")]
impl Split {
    pub(super) fn begin() -> Split {
        Split(std::cell::Cell::new(std::time::Instant::now()))
    }

    /// Nanoseconds since the last part.
    pub(super) fn so_far(&self) -> u64 {
        self.0.get().elapsed().as_nanos() as u64
    }

    /// What came since the last part was part `i`.
    pub(super) fn part(&self, i: usize) {
        let now = std::time::Instant::now();
        SPLIT[i].fetch_add((now - self.0.replace(now)).as_nanos() as u64, Ordering::Relaxed);
    }
}

/// The split of the preparing, per frame, for the frame-rate line.
#[cfg(feature = "diagnostics")]
pub(super) fn split_line(frames: f64) {
    let ms: Vec<f64> = SPLIT.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames).collect();
    let f: Vec<f64> = FLUSHES.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    let e: Vec<f64> = crate::memory::watch::early_asks().iter().map(|&n| n as f64 / frames).collect();
    let a: Vec<f64> = crate::memory::watch::asks().iter().map(|&n| n as f64 / frames).collect();
    let h: Vec<f64> = crate::execution::draw::HEAP_SPLIT.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames).collect();
    let s: Vec<f64> = crate::memory::shadow::send_stats().iter().map(|&n| n as f64 / frames).collect();
    tracing::info!(
        "preparing per frame, the submitting thread: {:.1} ms asking the write watch ({:.1} of them finding what to ask), {:.1} putting the title's bytes under flights' \
         writes, {:.1} in the lists' steps (refreshes {:.1} in {:.0} flushes of {:.0}, {:.0} of them known to copy nothing, {:.0} on \
         several threads, {:.1} one at a time in {:.0} before shader writes; copies and fills {:.1}, the refreshes before copies among them), {:.1} snapshots, {:.1} for copies in the \
         card's memory, {:.1} sending the lists (OATHKEEP_LIGHT_REFRESH is {}); ahead of these {:.1} waiting for the submission lock, {:.1} \
         planning steps done on the card, {:.1} waiting for flights, {:.1} the heaps' records and what is swept; at the end {:.1} waiting \
         for the heaps' records; asked early by a thread of its own {:.0} pieces of 2 MiB, {:.1} ms (OATHKEEP_ASK_EARLY)",
        ms[0],
        ms[8],
        ms[1],
        ms[2],
        ms[6],
        f[0],
        f[1],
        f[2],
        f[3],
        ms[9],
        ms[10] * 1e6,
        ms[7],
        ms[3],
        ms[4],
        ms[5],
        if crate::memory::watch::light() { "on" } else { "off" },
        ms[11],
        ms[12],
        ms[13],
        ms[14],
        ms[15],
        e[0],
        e[1] / 1e3,
    );
    tracing::info!(
        "asking about title memory per frame, all threads: {:.0} kernel askings over {:.0} MB ({:.1} ms, {:.0} pages of 4 KiB found \
         written, {:.0} askings refused); {:.0} questions about holes; view ranges: {:.0} answered by their \
         bits, {:.0} by the guards whole, {:.0} block by block; {:.0} blocks made read-only, {:.0} opened by ourselves; {:.1} ms \
         waiting for the guards, {:.1} ms changing protections; pages taken for the card: {:.1} MB compared, {:.1} MB of them \
         as they were, {:.1} MB to go up in {:.0} runs (OATHKEEP_SEND_CHANGED)",
        a[0],
        a[1] / 1024.0,
        a[11] / 1e6,
        a[12],
        a[10],
        a[9],
        a[2],
        a[3],
        a[4],
        a[5],
        a[6],
        a[7] / 1e6,
        a[8] / 1e6,
        s[0] / 1048576.0,
        s[1] / 1048576.0,
        s[2] / 1048576.0,
        s[3],
    );
    tracing::info!(
        "the pass over the heap's views per frame: {:.1} ms looking at slots, {:.1} their copies' spans and refreshes, {:.1} textures; the \
         first two beside the recording (OATHKEEP_HEAP_PASS_ASIDE), {:.1} ms waited for at the lists' close",
        h[0],
        h[1],
        h[2],
        heap_pass_async::WAITED.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames,
    );
}

/// Whether a submission's refreshes of host copies run on several threads,
/// a copy to a thread (`OATHKEEP_WIDE_REFRESH=0` disables it; `=1` forces it).
///
/// A submission brings some sixty copies up to date, one after the other
/// on the submitting thread: for each, the pages the title wrote are asked
/// for and read through the kernel into the copy. That is the frame line's
/// "copies", 45-50 ms a frame, and the copies have nothing to do with one
/// another: each refresh writes its own copy and its own notes, and reads
/// title memory. With the switch the copies of a flush (four or more) are
/// spread over the threads kept for the snapshots (`crate::memory::shadow::wide`).
/// The write-watch journal and the guards are behind their own locks, as
/// they are for the title's threads.
pub(super) fn wide_refresh() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        wide_refresh_enabled(
            std::env::var("OATHKEEP_WIDE_REFRESH").ok().as_deref(),
            std::thread::available_parallelism().ok().map(|n| n.get()),
        )
    })
}

pub(super) fn wide_refresh_enabled(setting: Option<&str>, logical_cpus: Option<usize>) -> bool {
    match setting {
        Some("1") => true,
        Some("0") => false,
        _ => logical_cpus.is_some_and(|n| n >= 4),
    }
}

pub(super) fn ahead_of_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_REFRESH_AHEAD").is_none_or(|v| v != "0") && wide_refresh() && crate::memory::watch::light())
}

/// Whether everything a submission refreshes is brought up to date in one go,
/// on several threads, ahead of its steps (`OATHKEEP_REFRESH_FRONT=1`, and with
/// `OATHKEEP_JOIN_PARTS`, which needs it; else in groups where the steps ask
/// for it). Measured on its own at nothing (run submit-1: refreshes 17.1 ms a
/// frame without, 18.3 with).
///
/// A submission's refreshes were done in up to five groups one after the other
/// (the copies its shaders write, those between its steps, what is left),
/// each ending with the threads waiting for the last copy; and the submitting
/// thread went over all the steps once for each question it has (what to ask
/// about, what flights may write too, what shaders write). A refresh may come
/// earlier than its step: the title's bytes are the same until a step writes
/// title memory, and behind such a step everything is asked for again, as it
/// was.
pub(super) fn refresh_front() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    ahead_of_writes() && (joins_asked().0 > 1 || *ON.get_or_init(|| crate::diagnostics::switch::on("OATHKEEP_REFRESH_FRONT")))
}

/// Per copy what is refreshed in front of a submission's steps, as
/// [`written_spans`]'s but of all copies, up to the copy's first write by
/// shaders in each part of the submission (`Touch::Part`: another begins).
/// What a part refreshes of a copy it has given to its shaders stays where it
/// stands: around what they write. What a later part refreshes is the
/// title's bytes of before either.
pub(super) enum Touch {
    /// A refresh of a copy (by address), from, to.
    Refresh(usize, usize, usize),
    /// The copy is given to shaders.
    Write(usize),
    /// Title memory is written.
    Title,
    Nothing,
    /// Another part of the submission begins.
    Part,
}

/// Of `steps`, up to the first that writes title memory: the copies given to
/// shaders, each with the range its refreshes before that cover.
pub(super) fn written_spans(steps: impl Iterator<Item = Touch>) -> Vec<(usize, usize, usize)> {
    // (From, to, given to shaders.)
    let mut spans: ByCopy<(usize, usize, bool)> = ByCopy::default();
    for step in steps {
        match step {
            Touch::Refresh(copy, from, to) => {
                let span = spans.entry(copy).or_insert((from, to, false));
                if !span.2 {
                    (span.0, span.1) = (span.0.min(from), span.1.max(to));
                }
            }
            // (Not refreshed before: nothing of it is due.)
            Touch::Write(copy) => {
                if let Some(span) = spans.get_mut(&copy) {
                    span.2 = true;
                }
            }
            Touch::Title => break,
            Touch::Nothing | Touch::Part => {}
        }
    }
    spans.into_iter().filter(|(_, s)| s.2).map(|(copy, (from, to, _))| (copy, from, to)).collect()
}

pub(super) fn wide_fills() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WIDE_FILLS").is_none_or(|v| v != "0") && wide_refresh())
}

/// Whether a submission's textures are brought up to date on several
/// threads (`OATHKEEP_WIDE_TEXTURES=0`: one after the other, where the lists
/// name them, as before).
///
/// They are read, hashed and, if they changed, detiled into their staging
/// buffers: 5 ms a frame of the submitting thread, for some 16 MB. Each has
/// its own buffer, notes and lock, and is refreshed by the title's own
/// threads as it is. They are done before the next step that writes title
/// memory, as the copies' refreshes are, and before the lists go to the
/// card. What is new: the note of a list that its upload takes what the
/// buffer then holds (`texture.rs`, `dirty`) may come ahead of the refresh
/// an earlier list of the submission asked for, not behind it. The texture
/// is then uploaded by a later list too, once more than needed.
pub(super) fn wide_textures() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WIDE_TEXTURES").is_none_or(|v| v != "0") && wide_refresh())
}

