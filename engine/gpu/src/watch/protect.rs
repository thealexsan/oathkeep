use super::BLOCK;
use super::ask::elsewhere;
use super::bits::{BitsOf, in_word};
use super::fault::install;
#[cfg(feature = "diagnostics")]
use super::guards::tally;
use super::guards::{
    BESIDE, Guards, PROTECTED, PUBLISHED, guards_timed, known_holes, open_one_view, placeholder, put_count, rest,
};

/// Views of one page past which its views are not guarded together.
pub(crate) const MANY_VIEWS: usize = 64;

/// Diagnostics: blocks guarded together with their other views, blocks
/// opened because another view of theirs was written, and blocks opened
/// because a view of theirs was mapped; since the start.
#[cfg(feature = "diagnostics")]
pub(crate) static ALIASES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
pub(super) fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows. The
    // question walks the page tables to the end of the pages that share the
    // first one's state, and a guarded view is one long run of read-only
    // pages: 0.6 ms for the title's 174 MB heap, 16 times a frame, and most
    // of the 68 ms a frame of "guards" (run 58).
    // `OATHKEEP_GUARD_ASK=1` asks every time, as before.
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    let known = !*ASK.get_or_init(|| std::env::var_os("OATHKEEP_GUARD_ASK").is_some_and(|v| v == "1"))
        && (PROTECTED.any(first, first + 1) || {
            let g = guards_timed();
            g.protected.contains(first) || g.absent.contains(first)
        });
    if !known && !is_view(start, end) {
        return None;
    }
    install();
    // The holes that an ordinary commit may have filled, with no word of the
    // runtime's. Windows is asked with the lock not held (the range is, no
    // other thread asks about it: `ASKING`); what the runtime tells meanwhile
    // takes the block out of the set, and it is looked at as any other.
    let unsure: Vec<usize> = {
        let g = guards_timed();
        if g.clean.range(first..last).count() == last - first && !BESIDE.any(first, last) {
            // Nothing written, mapped or new since the range was last taken.
            #[cfg(feature = "diagnostics")]
            tally(3, 1);
            return Some(Vec::new());
        }
        g.absent.range(first..last).filter(|block| !g.held.contains(*block)).collect()
    };
    let mut came: Vec<(usize, bool)> = Vec::new();
    let mut uncommitted_until = first;
    for block in unsure {
        // One question for the blocks of a region.
        if block >= uncommitted_until {
            uncommitted_until = uncommitted_end(block);
            #[cfg(feature = "diagnostics")]
            tally(9, 1);
        }
        if block >= uncommitted_until {
            // The hole may have been given up, and its address be the
            // title's heap's, or code's, now: not ours to guard.
            came.push((block, is_view(block * BLOCK, (block + 1) * BLOCK)));
        }
    }
    let rest = rest();
    let mut g = guards_timed();
    #[cfg(feature = "diagnostics")]
    tally(4, 1);
    for &(block, view) in &came {
        if g.absent.remove(block) && !view {
            g.foreign.insert(block);
        }
    }
    // The range's blocks that are neither clean nor known to be holes. A
    // frame asks about some 10 GB of view memory, 150,000 blocks, nearly all
    // clean.
    let mut written = Vec::new();
    for word in first / 64..last.div_ceil(64) {
        let known = g.clean.word(word) & !BESIDE.word(word) | g.absent.word(word);
        written.extend(BitsOf(!known & in_word(word * 64, first, last)).map(|bit| word * 64 + bit));
    }
    let mut run: Option<(usize, usize)> = None;
    for &block in &written {
        if let Some((from, to)) = run
            && to != block
        {
            protect(&mut g, from, to);
            run = None;
            // Faults of the title's wait for the lock: each block is done on
            // its own, and looked at anew below.
            drop(g);
            g = guards_timed();
        }
        // Told as written by this, and read by the caller after.
        if BESIDE.take(block) {
            put_count(block).store(0, std::sync::atomic::Ordering::Relaxed);
        }
        if g.protected.contains(block) {
            // Read-only by way of another view of its page, or not clean
            // because one could not be made so. (Or clean, and bytes were put
            // in beside the guard: it stays.)
            if let Some((from, to)) = run.take() {
                protect(&mut g, from, to);
            }
            if g.clean.contains(block) {
                continue;
            }
            if cover(&mut g, block, block + 1) {
                g.clean.insert(block);
            }
        } else if !g.foreign.contains(&block) && !g.left_alone(block) && !g.rests(block, rest) {
            run = Some((run.map_or(block, |r| r.0), block + 1));
        } else if let Some((from, to)) = run.take() {
            protect(&mut g, from, to);
        }
    }
    if let Some((from, to)) = run {
        protect(&mut g, from, to);
    }
    drop(g);
    #[cfg(feature = "diagnostics")]
    crate::faultbook::note(crate::faultbook::What::Asked, first, last, written.len() as u64);
    Some(written)
}

/// Whether `[start, end)` is view memory. The title's views lie in
/// placeholders, mapped and unmapped block by block as it streams, so a
/// range can begin in a block that is not mapped right now: the first
/// pages of the range that are there decide. A range with nothing there
/// counts as a view (its blocks are noted absent, and looked at again when
/// the runtime maps them).
///
/// Looking at the first address only took such a range for "not a view":
/// its copy was marked not watched for good and copied whole at every
/// refresh, around its unmapped pages (run 54: 300 such ranges, 570 MB
/// copied a frame for them, 255 copies a frame reading around holes).
/// `OATHKEEP_GUARD_HOLES=0` looks at the first address only, as before.
pub(super) fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes = *HOLES.get_or_init(|| std::env::var_os("OATHKEEP_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        // Committed memory that is no view (the title's heaps, ours), or
        // the answer as it was before.
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        // A hole (a placeholder, or free): on to what follows it.
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent. The same physical pages
/// at every other view of them are made read-only too: a write through
/// one of those is this memory written, and only a fault can say so.
/// `OATHKEEP_VIEW_GUARD=0` leaves the other views as they are, as before.
fn protect(g: &mut Guards, from: usize, to: usize) {
    #[cfg(feature = "diagnostics")]
    crate::faultbook::note(crate::faultbook::What::Taken, from, to, 0);
    protect_here(g, from, to);
    cover(g, from, to);
}

/// Of the blocks `[from, to)`, which are read-only by our hand where they
/// can be: every other view of their pages is made read-only too. Where one
/// cannot be (the kernel writes through it now, or the runtime is changing
/// it), a write through it would not be seen: that block is not clean, and
/// told as written at every asking, until it can. Whether all are covered.
fn cover(g: &mut Guards, from: usize, to: usize) -> bool {
    let mut all = true;
    for (page, others) in elsewhere(from * BLOCK, (to - from) * BLOCK) {
        let block = page / BLOCK;
        if !g.protected.contains(block) {
            continue; // not there, or not ours to guard
        }
        #[cfg(feature = "diagnostics")]
        ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        g.aliased.insert(block);
        let mut covered = true;
        for other in others {
            covered &= protect_alias(g, other / BLOCK);
        }
        if !covered {
            all = false;
            g.clean.remove(block);
        }
    }
    all
}

/// Makes `block` read-only: another view of a page a copy is about to take
/// through its own, so that a write through this one faults too. It is not
/// noted clean: the copies of title memory at this view have not taken the
/// page ([`guard`] tells such a block as written, then notes it clean).
/// One that is clean stays so: had the page been written since its copies
/// took it, [`open`] would have opened it. Whether it is read-only now.
fn protect_alias(g: &mut Guards, block: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    g.aliased.insert(block);
    if g.protected.contains(block) {
        return true;
    }
    if g.left_alone(block) || g.rests(block, rest()) {
        return false;
    }
    // Ahead of the protection.
    PROTECTED.set(block);
    let mut old = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: title view memory; only its protection changes.
    let done = unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut old) } != 0;
    #[cfg(feature = "diagnostics")]
    {
        tally(5, 1);
        tally(8, started.elapsed().as_nanos() as u64);
    }
    if done && matches!(old, PAGE_READWRITE | PAGE_READONLY) {
        g.protected.insert(block);
        #[cfg(feature = "diagnostics")]
        crate::faultbook::note(crate::faultbook::What::Alias, block, 0, 0);
        return true;
    }
    if done {
        // Not for us to guard: as it was.
        // SAFETY: as above.
        unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, old, &mut old) };
    }
    PROTECTED.clear(block);
    false
}

/// [`protect`] for one range, without the other views of its pages.
fn protect_here(g: &mut Guards, from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        // SAFETY: title view memory; only its protection changes.
        let mut done = unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        };
        if done && !matches!(old, PAGE_READWRITE | PAGE_READONLY) {
            // What is there is not for us to guard (code, or memory the
            // title protects itself): as it was.
            // SAFETY: as above.
            unsafe { VirtualProtect((from * BLOCK) as *const _, (to - from) * BLOCK, old, &mut old) };
            done = false;
        }
        #[cfg(feature = "diagnostics")]
        {
            tally(5, (to - from) as u64);
            tally(8, started.elapsed().as_nanos() as u64);
        }
        done
    };
    let note = |g: &mut Guards, from: usize, to: usize| {
        g.protected.insert_all(from..to);
        g.clean.insert_all(from..to);
    };
    // Ahead of the protection (`PROTECTED`).
    (from..to).for_each(|block| PROTECTED.set(block));
    if set(from, to) {
        note(g, from, to);
        return;
    }
    let mut uncommitted_until = from;
    for block in from..to {
        // (Not one question a block of a region that is not there.)
        if block >= uncommitted_until && set(block, block + 1) {
            note(g, block, block + 1);
            continue;
        }
        PROTECTED.clear(block);
        if block >= uncommitted_until {
            uncommitted_until = uncommitted_end(block);
            #[cfg(feature = "diagnostics")]
            tally(9, 1);
        }
        if block < uncommitted_until {
            g.absent.insert(block);
            if known_holes() && placeholder(block) {
                g.held.insert(block);
            }
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
pub(super) fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; how many there were. The same physical pages at
/// every other view of them are written by that too (the write that faulted,
/// or is ours, `crate::host::copy_mapped`, goes through one view): they are
/// clean no longer, whether they are opened as well ([`open_one_view`]) or
/// not.
pub(super) fn open(g: &mut Guards, start: usize, end: usize) -> usize {
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    if first >= last {
        return 0;
    }
    let mut blocks: Vec<usize> = g.protected.range(first..last).collect();
    let here = blocks.len();
    let aliased: Vec<usize> = g.aliased.range(first..last).collect();
    if !aliased.is_empty() {
        let one = open_one_view();
        for block in aliased {
            g.aliased.remove(block);
            for other in crate::inplace::pages_of(block * BLOCK) {
                let other = other / BLOCK;
                if one {
                    // Before the first can be written.
                    g.clean.remove(other);
                    PUBLISHED.clear(other);
                    continue;
                }
                g.aliased.remove(other);
                if !(first..last).contains(&other) && g.protected.contains(other) {
                    blocks.push(other);
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        ALIASES[1].fetch_add((blocks.len() - here) as u64, std::sync::atomic::Ordering::Relaxed);
    }
    unguard(g, &blocks);
    here
}

/// Gives write access back to `blocks`, guarded ones.
pub(super) fn unguard(g: &mut Guards, blocks: &[usize]) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // All of them before the first can be written: a write through one is a
    // write of the others' bytes.
    for &block in blocks {
        g.protected.remove(block);
        g.clean.remove(block);
        PUBLISHED.clear(block);
    }
    for &block in blocks {
        let mut old = 0;
        // SAFETY: a block we made read-only.
        unsafe {
            if VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) != 0
                && !matches!(old, PAGE_READONLY | PAGE_READWRITE)
            {
                // Not what we left there: the address is other memory's
                // (code's, say), which keeps its protection.
                VirtualProtect((block * BLOCK) as *const _, BLOCK, old, &mut old);
            }
        }
        PROTECTED.clear(block);
    }
    #[cfg(feature = "diagnostics")]
    if !blocks.is_empty() {
        tally(8, started.elapsed().as_nanos() as u64);
    }
}
