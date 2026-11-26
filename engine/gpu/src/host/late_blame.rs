use super::*;

/// Whether anything is in the air.
pub(super) fn flying() -> bool {
    if !late() {
        return false;
    }
    let line = line();
    // (With the relay: also while flights are being landed.)
    line.settling || line.line.iter().any(|l| matches!(l, Late::Run(_)))
}

/// With flights in the air ([`late`]): why `runs` cannot be prepared
/// before they are settled, if so. Preparing reads title memory on the
/// CPU (the title's buffer copies, fills of upload memory, textures) and
/// runs steps of ours that may read or write it anywhere; what the
/// flights' shaders write is not there yet.
pub(super) fn late_blocked(runs: &[&HostList]) -> Option<usize> {
    let (written, copies) = {
        let line = line();
        if line.written.is_empty() {
            return None;
        }
        (line.written.clone(), card_steps::copies(&line))
    };
    blocked_by(&written, runs, true).or_else(|| card_steps::copied_read(&copies, runs).then_some(WHY_READ))
}

/// Whether a small fill of upload memory, or copy of a title buffer, goes on without waiting for
/// flights when the title itself wrote the memory since they were given it
/// (`OATHKEEP_FILL_TITLE=1`; off by default).
///
/// The title's 174 MB heap is declared written as a whole (a root view has
/// no size), and the title keeps a ring of constants in it: one fill of 480
/// bytes a frame, at a new place each time, waited for every flight's
/// copy-back, some 30 ms of a 130 ms frame (run blame1: 38 of the 40 waits
/// named). The title writes those bytes just before the draw that reads
/// them, which takes the block's guard off ([`crate::watch::written_since_taken`]).
///
/// Or no shader's write went into that page in the last 600 frames
/// ([`crate::watch::landed_lately`]): the first alone left two thirds of
/// the wait, for constants at the end of a 64 KiB page that the title wrote
/// before the page was taken.
///
/// What can go wrong: a constant a flight's shader really writes, in a
/// block that the title (or an earlier flight's copy-back) also wrote, or
/// for the first time, is read as it was before that flight.
pub(super) fn fill_is_titles(src: usize, len: usize) -> bool {
    const SMALL: usize = 64 << 10;
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var_os("OATHKEEP_FILL_TITLE").is_some_and(|v| v == "1"));
    on && len <= SMALL && (crate::watch::written_since_taken(src, len) || !crate::watch::landed_lately(src, len, 600))
}

/// Diagnostics: the waits for flights, by what waited, why it was not done
/// on the card ([`card_steps`]), its bytes, and the declared write it lies
/// in (start, bytes).
#[cfg(feature = "diagnostics")]
pub(super) type Blamed = (&'static str, &'static str, usize, usize, usize);
/// How often and how long, and the address the last one read.
#[cfg(feature = "diagnostics")]
pub(super) static BLAME: Mutex<Option<std::collections::HashMap<Blamed, (u64, u64, usize)>>> = Mutex::new(None);
/// What the wait that comes next is for, and why not on the card.
#[cfg(feature = "diagnostics")]
pub(super) static BLAMED: Mutex<(Option<(Blamed, usize)>, &'static str)> = Mutex::new((None, ""));

/// Diagnostics: shader writes declared over a megabyte or more: the range
/// (from, to) and what declares it ([`wide_write`]): how often, and its
/// shaders.
#[cfg(feature = "diagnostics")]
pub(super) static WIDE_WRITES: Mutex<Option<std::collections::HashMap<(usize, usize, usize), (u64, String)>>> = Mutex::new(None);

/// Diagnostics: a draw or dispatch declares that it writes `len` bytes at
/// `address`, through a root view of `pipeline`, a view in a table (0) or a
/// raw address (1).
#[cfg(feature = "diagnostics")]
pub(crate) fn wide_write(address: u64, len: u64, pipeline: usize) {
    if len < 1 << 20 {
        return;
    }
    let mut all = WIDE_WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let e = all
        .get_or_insert_with(Default::default)
        .entry((address as usize, (address + len) as usize, pipeline))
        .or_insert_with(|| {
            let shaders = match pipeline {
                0 => "a view in a table".into(),
                1 => "a raw address".into(),
                p => crate::pipeline::host_pipeline(p as crate::object::This).map_or_else(String::new, |p| {
                    p.stages.iter().map(|s| s.shader.hash.to_string()).collect::<Vec<_>>().join("+")
                }),
            };
            (0, shaders)
        });
    e.0 += 1;
}

#[cfg(feature = "diagnostics")]
pub(super) fn blame(kind: &'static str, written: &[(usize, usize)], at: usize, len: usize) {
    let i = written.partition_point(|r| r.1 <= at);
    let (from, to) = written.get(i).copied().unwrap_or((0, 0));
    let mut blamed = BLAMED.lock().unwrap_or_else(|e| e.into_inner());
    let why = blamed.1;
    blamed.0 = Some(((kind, why, len, from, to - from), at));
}

/// Why [`card_steps`] left the submission to the wait that comes next.
#[cfg(feature = "diagnostics")]
pub(super) fn refused(why: &'static str) {
    *BLAMED.lock().unwrap_or_else(|e| e.into_inner()) = (None, why);
}

/// The wait that comes next is for these ranges ([`stale_under_flights`]).
#[cfg(feature = "diagnostics")]
pub(super) fn blame_stale(stale: &[(std::sync::Arc<crate::shadow::Shadow>, usize, usize)]) {
    if let Some((shadow, from, _)) = stale.first() {
        let (start, len) = shadow.span();
        let key = ("title memory changed under shader writes", crate::shadow::stale_why(), 0, start as usize, len as usize);
        BLAMED.lock().unwrap_or_else(|e| e.into_inner()).0 = Some((key, *from));
    }
}

/// The wait for what [`blame`] last named, or a wait of another reason than
/// title memory read, took `us`.
#[cfg(feature = "diagnostics")]
pub(super) fn blamed(why: usize, us: u64) {
    let (named, refused) = std::mem::replace(&mut *BLAMED.lock().unwrap_or_else(|e| e.into_inner()), (None, ""));
    let (key, at) = match (why, named) {
        (WHY_READ | WHY_STALE, Some(named)) => named,
        (WHY_READ, None) => (("a host copy of what a copy done on the card wrote", refused, 0, 0, 0), 0),
        _ => {
            let kind = match why {
                WHY_SYNC => "a synchronous submission, a present or the sweep",
                WHY_VALUE => "a memory value",
                WHY_STALE => "title memory changed under shader writes",
                WHY_RUN => "a step of ours",
                WHY_MOST => "lists in the air",
                WHY_FENCE => "a blocking fence wait",
                _ => "another reason",
            };
            ((kind, "", 0, 0, 0), 0)
        }
    };
    let mut all = BLAME.lock().unwrap_or_else(|e| e.into_inner());
    let e = all.get_or_insert_with(Default::default).entry(key).or_default();
    *e = (e.0 + 1, e.1 + us, at);
}

/// For the frame-rate line: the five that waited longest.
#[cfg(feature = "diagnostics")]
pub(super) fn log_blame(frames: f64) {
    let all = BLAME.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let wide = WIDE_WRITES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let mut all: Vec<_> = all.into_iter().collect();
    all.sort_unstable_by_key(|(_, (_, us, _))| std::cmp::Reverse(*us));
    for ((kind, why, len, from, size), (n, us, at)) in all.into_iter().take(5) {
        // The passes that declare it written, the ones that do so most.
        let mut passes: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
        for ((a, b, _), (n, shaders)) in &wide {
            if *a < at + len && at < *b {
                *passes.entry(shaders).or_default() += n;
            }
        }
        let mut passes: Vec<(&str, u64)> = passes.into_iter().collect();
        passes.sort_unstable_by_key(|p| std::cmp::Reverse(p.1));
        let passes: Vec<String> =
            passes.into_iter().take(5).map(|(shaders, n)| format!("{shaders} {:.1}x", n as f64 / frames)).collect();
        tracing::info!(
            "waited for flights, per frame: {:.1} ms in {:.2} waits for {kind} +{len:#x} (last at {at:#x}), in shader writes declared \
             over {from:#x}+{size:#x} (declared per frame over a megabyte or more there, by shader: {}){}{why}",
            us as f64 / 1e3 / frames,
            n as f64 / frames,
            passes.join(", "),
            match (why.is_empty(), kind) {
                (true, _) => "",
                (_, "title memory changed under shader writes") => "; not put in under them: ",
                _ => "; not done on the card: ",
            },
        );
    }
}

/// Whether flights that write `written` stop a read of `len` bytes at `src`.
pub(super) fn read_blocked(written: &[(usize, usize)], src: usize, len: usize) -> bool {
    touches(written, src, src + len) && !fill_is_titles(src, len)
}

/// Whether they stop a title buffer copy, and if so whether by its source.
pub(super) fn copy_blocked(written: &[(usize, usize)], dst: usize, src: usize, len: usize) -> Option<bool> {
    let read = read_blocked(written, src, len);
    (read || touches(written, dst, dst + len)).then_some(read)
}

/// [`late_blocked`] against `written`, the title memory flights' shaders
/// may write (in order, joined); `count` whether the kind of read is
/// counted for the frame line.
pub(super) fn blocked_by(written: &[(usize, usize)], runs: &[&HostList], count: bool) -> Option<usize> {
    if written.is_empty() {
        return None;
    }
    // Destinations of copies at their place, if a step reads them.
    let mut placed: Vec<(usize, usize)> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    if let Some(_read) = copy_blocked(written, dst, src, len) {
                        if count {
                            LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            if _read {
                                blame("a buffer copy's source", written, src, len);
                            } else {
                                blame("a buffer copy's destination", written, dst, len);
                            }
                        }
                        return Some(WHY_READ);
                    }
                }
                &Step::Fill(_, src, len, _) => {
                    if read_blocked(written, src, len) {
                        if count {
                            LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            blame("a fill of upload memory", written, src, len);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if touches(written, at, at + len) {
                        if count {
                            LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            blame("a texture", written, at, len);
                        }
                        return Some(WHY_READ);
                    }
                }
                &Step::Plane((at, len), _) => {
                    if touches(written, at, at.saturating_add(len)) {
                        if count {
                            LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            blame("a movie plane", written, at, len);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Run(_) => return Some(WHY_RUN),
                Step::Placed(p) => match p.waits(written) {
                    place::Waits::No => {}
                    place::Waits::ForReaders => placed.push((p.dst, p.dst + p.len)),
                    place::Waits::Yes => {
                        if count {
                            LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            blame("a buffer copy at its place", written, p.src, p.len);
                        }
                        return Some(WHY_READ);
                    }
                },
                Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(..) => {}
            }
        }
    }
    (!placed.is_empty() && place::read_by(runs, placed)).then_some(WHY_READ)
}

/// With flights in the air, after the submission's write-watch asking:
/// whether `runs` refresh a range of a host copy that a flight's shaders
/// may write and that the title wrote since it was last copied. The
/// refresh leaves ranges shaders are writing alone (`Shadow::update`), so
/// the title's bytes there would reach the card a submission late.
pub(super) fn stale_under_flights(runs: &[&HostList]) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> {
    use std::sync::Arc;
    // Per copy, the span of what is refreshed (a flush brings that much
    // up to date at most).
    let mut spans: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    // Where each copy is in it, if shaders may be writing it (none begins to
    // while this runs): a frame has 20,000 refreshes of some 4,500 copies.
    let mut places: ByCopy<Option<usize>> = ByCopy::default();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::Refresh(shadow, address, len) = step {
                let place = places.entry(Arc::as_ptr(shadow) as usize).or_insert_with(|| {
                    shadow.writes_pending().then(|| {
                        spans.push((shadow.clone(), *address, *address + *len));
                        spans.len() - 1
                    })
                });
                if let Some(i) = *place {
                    let s = &mut spans[i];
                    (s.1, s.2) = (s.1.min(*address), s.2.max(*address + *len));
                }
            }
        }
    }
    if late_exact() {
        // The title's bytes are put in beside the flights' writes; a
        // settle is still due where that could not be done. At most this
        // many changed runs go up to copies in the card's memory, each a
        // copy of its own there. With [`card_steps`] the pages of the
        // title's ring of constants come here too, which were waited for
        // before: hundreds of short runs each, at about 1.5 us a run.
        let mut runs_left = if card_steps::stage() > 0 { 4096 } else { 512 };
        spans.retain(|(shadow, from, to)| shadow.merge_under_writes(*from, *to - *from, &mut runs_left));
        return spans;
    }
    spans.retain(|(shadow, from, to)| shadow.stale_under_writes(*from, *to - *from));
    spans
}

/// How many parts of submissions, and lists, the line's thread gives to the
/// host as one (`OATHKEEP_JOIN_PARTS=<n>`; off by default, 1; `OATHKEEP_JOIN_LISTS`,
/// 48).
///
/// The idea is fxceless/victory-fork's, commit 39731795 (GPL-3.0-or-later, as
/// this): the title's lists wait for memory values, which cuts a frame into
/// some 29 parts, each prepared on its own: the write watch asked, the heaps'
/// records, the title's bytes under the flights' writes, a list that sends
/// what changed and one that compares. (Run submit-1, the dorm, 12 of 28 parts
/// joined, eight at most: the thread works 51.4 ms a frame instead of 54.8, the
/// frame is 0.6 to 3 ms shorter.) What differs here: the parts are taken
/// off one queue's entries of the line (`crate::queue`), still go to the queue
/// in calls of their own, and everything they refresh is brought up to date in
/// front of the first ([`refresh_front`], without which a later part's title
/// bytes under an earlier part's declared writes would go missing).
pub(crate) fn joins() -> (usize, usize) {
    if ahead_of_writes() { joins_asked() } else { (1, 1) }
}

pub(super) fn joins_asked() -> (usize, usize) {
    static MOST: OnceLock<(usize, usize)> = OnceLock::new();
    let of = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<usize>().ok());
    let (parts, lists) = *MOST.get_or_init(|| (of("OATHKEEP_JOIN_PARTS").unwrap_or(1).max(1), of("OATHKEEP_JOIN_LISTS").unwrap_or(48).max(1)));
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("OATHKEEP_JOIN_PARTS") {
        return (if on { 8 } else { 1 }, lists);
    }
    (parts, lists)
}

/// Whether the part `next` can be prepared together with `ahead`, which goes
/// before it and has not been sent: nothing done on the CPU for it reads what
/// those give their shaders, write back or write when they are done, and it
/// changes no title memory that they read ([`Step::Copy`], [`Step::Run`]).
pub(crate) fn joinable<'a>(ahead: impl Iterator<Item = &'a HostList>, next: &[&HostList]) -> bool {
    if next.iter().any(|l| l.deferred().copies.iter().any(|s| matches!(s, Step::Copy(..) | Step::Run(_)))) {
        return false;
    }
    let ahead: Vec<&HostList> = ahead.collect();
    let mut written: Vec<(usize, usize)> = declared_writes(&ahead).into_iter().flat_map(|(_, ranges)| ranges).collect();
    for list in &ahead {
        let deferred = list.deferred();
        written.extend(deferred.writes.iter().map(|&(at, _, bytes)| (at, at + bytes)));
        written.extend(deferred.backs.iter().map(|&(_, at, [row, rows, pitch])| (at, at + rows.saturating_sub(1) * pitch + row)));
    }
    let mut joined = Vec::new();
    join_ranges(&mut joined, written);
    blocked_by(&joined, next, false).is_none()
}

/// Whether a bottom-of-pipe write of `list` meets `wait`.
pub(crate) fn will_meet(list: &HostList, wait: &crate::list::ValueWait) -> bool {
    let (address, bytes) = wait.place();
    // The last write there says.
    let deferred = list.deferred();
    let last = deferred.writes.iter().rev().find(|(at, _, n)| *at < address + bytes && address < *at + *n);
    matches!(last, Some((at, crate::queue::Written::Value(v), n)) if *at == address && *n >= bytes && wait.holds_for(*v))
}
