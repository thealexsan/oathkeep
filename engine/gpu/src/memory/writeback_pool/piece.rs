use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{BLOCK, PAGE, beside_on, count, spent, stat};

/// Whether a piece is put through a view of ours while the runtime changes
/// a mapping too (`OATHKEEP_LAND_ANYTIME=0`: through the title's address, as
/// before).
///
/// While the world streams a change is under way nearly all the time (pages
/// are mapped in groups, eight threads side by side: 28 of 1,620 pieces a
/// frame went through a view of ours in run textures-4, 630 in run n03). Put
/// through the title's address a piece opens the guards of its blocks, 5,000
/// changes of protection a frame, which wait for the address-space lock the
/// mapping wants as well, and every refresh guards them again: the frame went
/// from 105 to 140 ms. The record names a run of a section only while it is
/// mapped there (it is taken out before a view goes and put in when one is
/// there), so the change is one of other memory, or one the title makes of
/// memory its shaders are writing at that moment, which it may not (its
/// fences are signalled after the landing).
fn anytime() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LAND_ANYTIME").is_none_or(|v| v != "0"))
}

/// What a job wrote beside the guards, to be noted ([`Notes::flush`]), plus
/// job-local stat accumulators so [`piece_at`] avoids 11 shared-cache-line
/// atomic RMWs per piece across worker threads.
#[derive(Default)]
pub(crate) struct Notes {
    /// Pages of 4 KiB, by number.
    pages: Vec<usize>,
    /// Per piece, from the first such page to the last: address, bytes.
    spans: Vec<(usize, usize)>,
    /// Per piece put beside the guards, all of it: its 64 KiB block and the
    /// copy it was landed through (which the job holds until the flush).
    landed: Vec<(usize, *const crate::memory::shadow::Shadow)>,
    stat_pieces: u64,
    stat_new_pieces: u64,
    stat_pages: u64,
    stat_new_pages: u64,
    stat_new_blocks: u64,
    stat_dwords: u64,
    stat_beside: u64,
    stat_direct: u64,
    stat_missing: u64,
    put_pieces: u64,
    put_bytes: u64,
}

// SAFETY: the pointers are used by the job that has the notes, in `flush`.
unsafe impl Send for Notes {}

impl Drop for Notes {
    fn drop(&mut self) {
        self.flush_stats();
    }
}

/// Whether a copy a landing went through is noted as up to date with it
/// (`OATHKEEP_LAND_TAKEN=0`: it is not, and the next refresh copies the 64
/// KiB again and sends them up to the card, as before).
///
/// What a landing puts into title memory the copy holds already: the card
/// wrote it there, and a copy in the card's memory was given it on the CPU's
/// side by the same merge. But the block counted as written, for that copy
/// as for every other one, so the next refresh copied the whole block, sent
/// it up and had the card hash it again: some 900 blocks a frame landed
/// beside the guards, of the 103 MB a frame that went up (run record-1).
///
/// Why it may be left out ([`crate::memory::shadow::Shadow::landed`]): the copy held
/// title memory's bytes of the block before the landing, if it had taken it
/// after the block's last noted write and no write was to be noted (the
/// block's bit, `crate::memory::watch::block_published`), and the landing put the
/// same bytes into both. Only where one copy alone landed in the block.
pub(super) fn taken() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LAND_TAKEN").is_none_or(|v| v != "0"))
}

/// Since the last frame-rate line: blocks landed in beside the guards, and
/// those of them the copy was noted as up to date with.
pub(super) static TAKEN: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

impl Notes {
    fn flush_stats(&mut self) {
        if self.stat_pieces == 0 && self.stat_pages == 0 {
            return;
        }
        for (which, val) in [
            (stat::PIECES, &mut self.stat_pieces),
            (stat::NEW_PIECES, &mut self.stat_new_pieces),
            (stat::PAGES, &mut self.stat_pages),
            (stat::NEW_PAGES, &mut self.stat_new_pages),
            (stat::NEW_BLOCKS, &mut self.stat_new_blocks),
            (stat::DWORDS, &mut self.stat_dwords),
            (stat::BESIDE, &mut self.stat_beside),
            (stat::DIRECT, &mut self.stat_direct),
            (stat::MISSING, &mut self.stat_missing),
        ] {
            let n = std::mem::take(val);
            if n != 0 {
                count(which, n);
            }
        }
        let put_p = std::mem::take(&mut self.put_pieces);
        let put_b = std::mem::take(&mut self.put_bytes);
        if put_p != 0 {
            crate::memory::shadow::PUT[0].fetch_add(put_p, Ordering::Relaxed);
        }
        if put_b != 0 {
            crate::memory::shadow::PUT[2].fetch_add(put_b, Ordering::Relaxed);
        }
    }

    /// At the end of a job: what it wrote beside the guards is noted.
    pub(crate) fn flush(&mut self) {
        self.flush_stats();
        if self.pages.is_empty() {
            return;
        }
        let noting = std::time::Instant::now();
        let (now, blocks) = crate::memory::watch::landed_beside(&self.pages, &self.spans);
        TAKEN[0].fetch_add(blocks.len() as u64, Ordering::Relaxed);
        if taken() {
            for (block, before) in blocks {
                let mut through = self.landed.iter().filter(|l| l.0 == block).map(|l| l.1);
                let Some(copy) = through.next() else {
                    continue;
                };
                if !copy.is_null() && through.all(|other| other == copy) {
                    // SAFETY: the job holds its copies until this is done.
                    let copy = unsafe { &*copy };
                    TAKEN[1].fetch_add(u64::from(copy.landed(block, before, now)), Ordering::Relaxed);
                }
            }
        }
        self.pages.clear();
        self.spans.clear();
        self.landed.clear();
        spent(stat::NOTES, noting);
    }
}

/// Finds the first and last 4 KiB page indices (`0..=BLOCK / PAGE`, relative to
/// `base = title / PAGE`) within `parts` where `now` and `held` differ, stopping
/// immediately from both ends so middle pages of dirty pieces are not scanned
/// twice (once in `differ` and again in `safecopy::merge`). Returns `Err(pages)`
/// when no byte in `parts` differs.
///
/// # Safety
/// `now` and `held` are readable up to the highest `to` in `parts`.
pub(super) unsafe fn differing_page_bounds(
    title: usize,
    now: *const u8,
    held: *const u8,
    parts: &[(usize, usize)],
) -> Result<(usize, usize), u64> {
    let base = title / PAGE;
    let mut first = None;
    let mut total_pages = 0u64;
    'forward: for &(from, to) in parts {
        let mut at = from;
        while at < to {
            let n = (PAGE - (title + at) % PAGE).min(to - at);
            total_pages += 1;
            // SAFETY: the caller's.
            if unsafe { crate::memory::safecopy::differ(now.add(at), held.add(at), n) } {
                first = Some((title + at) / PAGE - base);
                break 'forward;
            }
            at += n;
        }
    }
    let Some(first) = first else {
        return Err(total_pages);
    };
    let mut last = first;
    for &(from, to) in parts.iter().rev() {
        if to <= from || (title + to - 1) / PAGE - base <= last {
            continue;
        }
        let mut end_at = to;
        while end_at > from {
            let page_start = (title + end_at - 1) & !(PAGE - 1);
            let at = page_start.saturating_sub(title).max(from);
            let page_idx = (title + at) / PAGE - base;
            if page_idx <= last {
                break;
            }
            let n = end_at - at;
            // SAFETY: the caller's.
            if unsafe { crate::memory::safecopy::differ(now.add(at), held.add(at), n) } {
                last = page_idx;
                break;
            }
            end_at = at;
        }
    }
    Ok((first, last))
}

/// Lands one piece: `parts` (offsets from and to) of the bytes at `now`,
/// which the card wrote, go into title memory from `title` on and into
/// `held` ([`crate::memory::safecopy::merge`] says how, and what `take` is). The
/// piece lies in one 64 KiB block of title memory. Returns the bytes put.
///
/// # Safety
/// `now` and `held` are ours from 0 to the last part's end, and nothing but
/// the card writes `now` there, nothing but this call `held`.
pub(crate) unsafe fn piece(
    title: usize,
    now: *const u8,
    held: *mut u8,
    parts: &[(usize, usize)],
    take: bool,
    notes: &mut Notes,
    copy: *const crate::memory::shadow::Shadow,
) -> u64 {
    // SAFETY: the caller's.
    unsafe { piece_at(title, now, held, parts, take, notes, beside_on(), copy) }
}

/// [`piece`], and whether a view of ours is looked for.
///
/// # Safety
/// [`piece`]'s.
pub(super) unsafe fn piece_at(
    title: usize,
    now: *const u8,
    held: *mut u8,
    parts: &[(usize, usize)],
    take: bool,
    notes: &mut Notes,
    beside: bool,
    copy: *const crate::memory::shadow::Shadow,
) -> u64 {
    let Some(end) = parts.iter().map(|p| p.1).max().filter(|&e| e > 0) else {
        return 0;
    };
    notes.stat_pieces += 1;
    let base = title / PAGE;
    // Check `differing_page_bounds` first for BOTH `beside` and direct pieces:
    // unchanged pieces exit before acquiring global watch/view locks, and
    // changed pieces skip all unchanged leading/trailing 4 KiB pages.
    // SAFETY: the caller's.
    let (first, last) = match unsafe { differing_page_bounds(title, now, held, parts) } {
        Ok(bounds) => bounds,
        Err(unchanged_pages) => {
            notes.stat_pages += unchanged_pages;
            return 0;
        }
    };
    let mut differing = [true; BLOCK / PAGE + 1];
    differing[..first].fill(false);
    differing[last + 1..].fill(false);
    // From here until the piece is put the runtime changes no mapping: our
    // view's address stays where the title sees `title`. Not waited for: if
    // a change is under way, what the record of the views says now does
    // ([`anytime`]), or the title's own address.
    let reading = beside.then(crate::memory::watch::title_memory_reading_now).flatten();
    let beside = if reading.is_some() || beside && anytime() { crate::memory::inplace::beside(title, end) } else { None };
    if beside.is_some() {
        notes.stat_beside += 1;
    } else {
        notes.stat_direct += 1;
        drop(reading);
        let (from, to) = (((base + first) * PAGE).max(title), ((base + last + 1) * PAGE).min(title + end));
        let opening = std::time::Instant::now();
        crate::memory::watch::landing(from, to - from);
        spent(stat::NOTES, opening);
    }
    let into = beside.unwrap_or(title);
    let since = crate::memory::watch::changes();
    let (mut pages, mut new_pages, mut blocks, mut dwords, mut missing) = (0, 0u64, 0, 0, 0);
    let mut span: Option<(usize, usize)> = None;
    for_pages(title, parts, |from, n| {
        pages += 1;
        if !differing[(title + from) / PAGE - base] {
            return;
        }
        // SAFETY: the caller's; title memory, by an address that shows it
        // (`beside`) or the title's own.
        let merge = |take| unsafe { crate::memory::safecopy::merge(into.wrapping_add(from) as *mut u8, now.add(from), held.add(from), n, take) };
        let mut done = merge(take && beside.is_some());
        if !done.there && beside.is_none() {
            // Away only while the runtime changes a mapping beside it: the
            // rest of it when that is done. What is still not there then is
            // taken as the caller says.
            let mut put = done.dwords;
            let again = crate::memory::watch::again_after_change(since, || {
                done = merge(false);
                put += done.dwords;
                done.there
            });
            if !again && take {
                done = merge(true);
                put += done.dwords;
            }
            done.dwords = put;
            // (What was new to the first try was new.)
            done.new = done.new.max(1);
        }
        blocks += done.new;
        dwords += done.dwords;
        missing += u64::from(!done.there);
        if done.new > 0 {
            new_pages += 1;
            if beside.is_some() {
                let page = (title + from) / PAGE;
                // (Two parts in one page.)
                if notes.pages.last() != Some(&page) {
                    notes.pages.push(page);
                }
                let s = span.get_or_insert((title + from, title + from + n));
                s.1 = title + from + n;
            }
        }
    });
    if let Some((from, to)) = span {
        notes.spans.push((from, to - from));
        // Not through a copy that is not to be noted, if that is asked: the
        // block then has two.
        let through = if missing == 0 { copy } else { std::ptr::null() };
        notes.landed.push((title / BLOCK, through));
    }
    notes.stat_new_pieces += u64::from(new_pages > 0);
    notes.stat_pages += pages;
    notes.stat_new_pages += new_pages;
    notes.stat_new_blocks += blocks;
    notes.stat_dwords += dwords;
    notes.stat_missing += missing;
    if dwords > 0 {
        notes.put_pieces += 1;
        notes.put_bytes += dwords * 4;
    }
    dwords * 4
}

/// `parts` of a piece that begins at `title`, cut where 4 KiB pages of title
/// memory end: offset and bytes.
pub(super) fn for_pages(title: usize, parts: &[(usize, usize)], mut f: impl FnMut(usize, usize)) {
    for &(from, to) in parts {
        let mut at = from;
        while at < to {
            let n = (PAGE - (title + at) % PAGE).min(to - at);
            f(at, n);
            at += n;
        }
    }
}
