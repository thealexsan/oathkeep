use super::early::note_hot;
#[cfg(feature = "diagnostics")]
use super::early::slow;
#[cfg(feature = "diagnostics")]
use super::guards::tally;
use super::guards::{PROTECTED, light, publish, published, view_guard};
use super::journal::{
    AHEAD_NONEMPTY, Asking, Journal, KEPT, journal, journal_mut, kernel_written, next_asking, normalize_ahead,
};
use super::protect::guard;
use super::{BLOCK, PAGE, WHOLE};

/// Ranges of one submission closer than this are asked about together:
/// walking the pages between them costs less than another kernel call (a
/// call that resets written pages costs 30-70 microseconds whatever the
/// range).
const AHEAD_GAP: usize = (8 << 20) / PAGE;
/// The most one asking covers (its answer needs 8 bytes per 4 KiB page).
const AHEAD_MAX: usize = (1 << 30) / PAGE;

/// Asks the kernel about many ranges `(start, end)` at once, before the
/// [`writes`] of each: ranges close together are one asking, and `writes`
/// then answers from the journal until [`ask_ahead_done`]. The world's
/// submissions refresh thousands of host copies (6,664 askings a frame,
/// 456 ms, in run 14). Ranges that are not write-watched, or lie alone, are
/// left to `writes`.
pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    if KEPT.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        // Not view memory known by its bits: the kernel would refuse the
        // asking (and, asked together with others, theirs).
        .filter(|r| r.0 < r.1 && !published(r.0, r.1))
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`.
    if journal().kept {
        return;
    }
    let _all = Asking::hold_all();
    let (mut asked, mut written) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len()
            && pieces[k].0 <= last + AHEAD_GAP
            && last.max(pieces[k].1) - first <= AHEAD_MAX
        {
            last = last.max(pieces[k].1);
            k += 1;
        }
        ask_run(&pieces[i..k], &mut asked, &mut written);
        i = k;
    }
    note_hot(&written);
    normalize_ahead(&mut asked);
    let mut j = journal_mut();
    j.ahead_asked = next_asking();
    j.ahead = asked;
    AHEAD_NONEMPTY.store(!j.ahead.is_empty(), std::sync::atomic::Ordering::Release);
    let now = j.ahead_asked;
    for (page, parts) in written {
        j.note(page, parts, now);
    }
}

/// The submission's refreshes are done: `writes` asks the kernel again.
pub(crate) fn ask_ahead_done() {
    if KEPT.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut j = journal_mut();
    if !j.kept {
        j.ahead.clear();
        AHEAD_NONEMPTY.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Whether a submission asks the kernel once, before its first refresh,
/// about every range its lists refresh (`OATHKEEP_ASK_ONCE=1`; off by
/// default).
///
/// A submission's refreshes are done in groups, between the title's own
/// buffer copies (each copy flushes the refreshes recorded before it), and
/// every group asked the kernel again: 2,644 askings a frame, 70 a
/// submission. With the switch the journal answers all of them from one
/// asking made when the submission starts.
///
/// What changes: memory the title's other threads write while the
/// submission is being prepared is seen by the next submission's asking,
/// not by this one's later groups. What this submission's lists read was
/// written before they were submitted, so before the asking. Our own
/// writes into title memory while preparing are told to the journal
/// ([`wrote`]) or end the asking ([`ask_once_end`]).
pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default since run 81 (with card buffers: 375-450 ms a frame
    // against 573-647, the picture unchanged over four minutes);
    // `OATHKEEP_ASK_ONCE=0` asks per range, as before.
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_ASK_ONCE").is_none_or(|v| v != "0"))
}

/// With [`once`]: asks about `ranges` (everything a submission's lists
/// refresh) as [`ask_ahead`] does, and lets that asking stand until
/// [`ask_once_end`].
pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if !once() {
        return;
    }
    ask_once_end();
    ask_ahead(ranges);
    ask_rest(ranges);
}

/// [`ask_once`] after its [`ask_ahead`]: what that left to `writes`, and the
/// asking stands.
pub(super) fn ask_rest(ranges: &[(usize, usize)]) {
    let _all = Asking::hold_all();
    let mut ahead = {
        let mut j = journal_mut();
        AHEAD_NONEMPTY.store(false, std::sync::atomic::Ordering::Release);
        std::mem::take(&mut j.ahead)
    };
    // `ask_ahead` leaves a range that lies alone to `writes`, which asks
    // once per flush: with one asking for the submission that was every
    // such range asked again at each of its 70 flushes (run 76: 2,300
    // askings a frame with the switch on, as without it). They are asked
    // here, once; what is not write-watched stays with `writes`.
    let mut written = Vec::new();
    for &(start, end) in ranges {
        if start >= end || published(start, end) {
            continue;
        }
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        if !ahead.iter().any(|&(a, b)| a <= first && last <= b)
            && let Some(pages) = kernel_written(first, last)
        {
            written.extend(pages);
            ahead.push((first, last));
        }
    }
    note_hot(&written);
    normalize_ahead(&mut ahead);
    let mut j = journal_mut();
    // A number of its own, and the one this asking stands under: between
    // the two parts another thread may have asked about a range of the
    // second, copied it, and the title written it since. Under the first
    // part's number that write was older than the copy, and never copied.
    let now = next_asking();
    j.ahead_asked = now;
    for (page, parts) in written {
        j.note(page, parts, now);
    }
    j.kept = !ahead.is_empty();
    j.ahead = ahead;
    KEPT.store(j.kept, std::sync::atomic::Ordering::Release);
    AHEAD_NONEMPTY.store(!j.ahead.is_empty(), std::sync::atomic::Ordering::Release);
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    let mut j = journal_mut();
    if j.kept {
        j.kept = false;
        j.ahead.clear();
        KEPT.store(false, std::sync::atomic::Ordering::Release);
        AHEAD_NONEMPTY.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
/// Returns `true` if any physical-page aliases were also marked.
pub(crate) fn wrote(at: usize, len: usize) -> bool {
    let mut j = journal_mut();
    if !j.kept || len == 0 {
        return false;
    }
    let now = next_asking();
    // What is copied from now on counts as copied after these writes (fork
    // PR 410): answers older than the writes in them made every refresh
    // until the next asking copy the pages again.
    j.ahead_asked = now;
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        j.note(page, WHOLE, now);
    }
    let aliases = elsewhere(at, len);
    let had_aliases = !aliases.is_empty();
    for (_, others) in aliases {
        for other in others {
            j.note(other / PAGE, WHOLE, now);
        }
    }
    had_aliases
}

/// Whether `[at, at + len)` has physical-page aliases at other virtual addresses.
pub(crate) fn has_aliases(at: usize, len: usize) -> bool {
    !elsewhere(at, len).is_empty()
}

/// The blocks of `[at, at + len)` whose pages the title shows at other
/// addresses too, and there.
pub(super) fn elsewhere(at: usize, len: usize) -> Vec<(usize, Vec<usize>)> {
    if !view_guard() || len == 0 {
        return Vec::new();
    }
    crate::inplace::shown_elsewhere(at / BLOCK * BLOCK, at.saturating_add(len))
}

/// One kernel asking for `pieces` (page ranges in order); by halves if it
/// fails, as a run across two reservations or into memory that is not
/// write-watched does. A piece alone is left to [`writes`]. Adds what was
/// asked to `asked` and the pages found written to `written`.
fn ask_run(pieces: &[(usize, usize)], asked: &mut Vec<(usize, usize)>, written: &mut Vec<(usize, u16)>) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if let Some(pages) = kernel_written(first, last) {
        written.extend(pages);
        asked.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(a, asked, written);
    ask_run(b, asked, written);
}

/// What [`writes`] would say of `[start, end)` from the journal alone, if
/// the range lies in one asked ahead ([`ask_ahead`], [`ask_once`]): the
/// pages written since tracking began and the asking that last found each
/// written, with the number of the asking that stands (as [`writes`] gives
/// it). `None` if the kernel would have to be asked.
pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let j = journal();
    j.has_ahead(first, last)
        .then(|| (j.ahead_asked, j.written.range(first..last).map(|(&p, &w)| (p, w)).collect()))
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let of = |j: &Journal| -> Vec<(usize, u64)> { j.written.range(first..last).map(|(&p, &w)| (p, w)).collect() };
    // Asked ahead with the submission's other ranges: the journal has it.
    let ahead = |j: &Journal| j.has_ahead(first, last).then(|| (j.ahead_asked, of(j)));
    if let Some(known) = ahead(&journal()) {
        return Some(known);
    }
    // Read-only since an asking noted it: the bits first, then the journal.
    if guarded && published(start, end) {
        #[cfg(feature = "diagnostics")]
        {
            tally(2, 1);
            crate::faultbook::note(crate::faultbook::What::Known, first, last, 0);
        }
        let j = journal();
        return Some((next_asking(), of(&j)));
    }
    // Asked, and the answer noted, with the range held: nothing else asks
    // about any of it in between.
    let _range = Asking::hold(first, last);
    if let Some(known) = ahead(&journal()) {
        return Some(known);
    }
    let mut guards_asked = false;
    // (View memory read-only by our hand: the kernel would refuse.)
    let refused = guarded && light() && PROTECTED.any(first, last);
    let pages = match if refused { None } else { kernel_written(first, last) } {
        Some(pages) => pages,
        None => {
            guards_asked = true;
            if !guarded {
                return None;
            }
            #[cfg(feature = "diagnostics")]
            let guarding = std::time::Instant::now();
            let written = guard(first * PAGE, last * PAGE).map(|pages| pages.into_iter().map(|p| (p, WHOLE)).collect::<Vec<_>>());
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                let us = guarding.elapsed().as_micros() as u64;
                add(C::GuardUs, us);
                if written.is_some() {
                    add(C::Watches, 1);
                    add(C::WatchUs, us);
                    slow(us);
                }
            }
            written?
        }
    };
    if pages.is_empty() {
        // Nothing to note: the others may go on reading.
        let j = journal();
        return Some((next_asking(), of(&j)));
    }
    let answer = {
        let mut j = journal_mut();
        let now = next_asking();
        for &(page, parts) in &pages {
            j.note(page, parts, now);
        }
        (now, of(&j))
    };
    // Noted: what the guards made clean of it is known by its bits from
    // now on. (The range is still held: no other asking of any of it came
    // between.)
    if guards_asked && light() {
        publish(&pages.iter().map(|p| p.0).collect::<Vec<_>>());
    }
    Some(answer)
}

/// Whether nothing of `[start, end)` was written that a copy with the notes
/// `copied` (the asking after which each page from page `base` on was taken)
/// and `ranged` (the asking after which a range that holds this one was
/// taken whole, or 0) does not hold, as [`writes`] would answer with nothing
/// asked: the submission's asking covers the range, or its blocks are known
/// by their bits. `false` if something would have to be asked.
pub(crate) fn nothing_new(start: usize, end: usize, base: usize, copied: &[u64], ranged: u64) -> bool {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    // The bits first, then the journal, as in `writes`.
    let known = published(start, end);
    #[cfg(feature = "diagnostics")]
    if known {
        crate::faultbook::note(crate::faultbook::What::Known, first, last, 1);
    }
    if !known && !AHEAD_NONEMPTY.load(std::sync::atomic::Ordering::Acquire) {
        return false;
    }
    let j = journal();
    if !known && !j.has_ahead(first, last) {
        return false;
    }
    let Some(taken) = first.checked_sub(base).and_then(|from| copied.get(from..last - base)) else {
        return false;
    };
    (ranged != 0 || taken.iter().all(|&at| at != 0))
        && j.written.range(first..last).all(|(&page, &write)| write <= taken[page - first].max(ranged))
}
