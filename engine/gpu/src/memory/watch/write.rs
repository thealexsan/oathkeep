use super::ask::elsewhere;
use super::bits::LANDED;
#[cfg(feature = "diagnostics")]
use super::guards::tally;
use super::guards::{PROTECTED, PUBLISHED, guards, guards_timed, light, published, view_guard};
use super::journal::{journal_mut, next_asking};
use super::mapping::CHANGING;
#[cfg(feature = "diagnostics")]
use super::protect::ALIASES;
use super::protect::{open, unguard};
use super::{BLOCK, KERNEL_PAGE, PAGE, PARTS, WHOLE};

/// What shaders wrote is about to go into title memory at `[at, at + len)`.
pub(crate) fn landing(at: usize, len: usize) {
    {
        let frame = crate::d3d12::frame::current();
        let (first, last) = (at / KERNEL_PAGE, (at + len.max(1) - 1) / KERNEL_PAGE);
        (first..=last).for_each(|page| LANDED.note(page, frame));
        for (block, others) in elsewhere(at, len) {
            let block = block / KERNEL_PAGE;
            let here = first.max(block)..=last.min(block + BLOCK / KERNEL_PAGE - 1);
            for other in others {
                here.clone().for_each(|page| LANDED.note(page - block + other / KERNEL_PAGE, frame));
            }
        }
    }
    writing(at, len);
}

/// What shaders wrote went into title memory through another view of the
/// same pages (`crate::memory::inplace::beside`): `pages` (of 4 KiB, by number)
/// hold some of it, all of them in `spans` (address, bytes).
///
/// Nothing faulted and no guard was opened. A guarded block stays read-only,
/// clean, and known by its bit ([`PUBLISHED`]), all of which say that the
/// title wrote nothing there: our write is an entry of the journal, which
/// every answer about the block comes from, the quick ones and the guards'
/// alike (a block whose guard is off they report anyway). So neither the
/// guards' lock nor a protection is touched here, or by the next refresh.
/// Noted after the bytes are in, never before: a copy that read the block
/// before they were did so with the number of an asking made before that,
/// which is under the one given here, and takes it again; noted before, a
/// copy could read the older bytes with a number above the note's.
///
/// Returns the note's number, and for each 64 KiB block noted the number of
/// the block's last write before (0: none is known).
pub(crate) fn landed_beside(pages: &[usize], spans: &[(usize, usize)]) -> (u64, Vec<(usize, u64)>) {
    let mut before = Vec::with_capacity(spans.len());
    let noted;
    // The same pages at other addresses: copies of the memory there have not
    // what was put in either.
    let elsewhere: Vec<(usize, Vec<usize>)> = spans.iter().flat_map(|&(at, len)| elsewhere(at, len)).collect();
    {
        let frame = crate::d3d12::frame::current();
        for &page in pages {
            LANDED.note(page, frame);
        }
        for (block, others) in &elsewhere {
            let first = block / KERNEL_PAGE;
            for &page in pages.iter().filter(|&&page| (first..first + BLOCK / KERNEL_PAGE).contains(&page)) {
                others.iter().for_each(|other| LANDED.note(page - first + other / KERNEL_PAGE, frame));
            }
        }
    }
    {
        let mut j = journal_mut();
        let now = next_asking();
        noted = now;
        let mut blocks: Vec<(usize, u16)> = Vec::with_capacity(spans.len());
        for &page in pages {
            let (block, part) = (page / PARTS, 1u16 << (page % PARTS));
            match blocks.last_mut() {
                Some((last, parts)) if *last == block => *parts |= part,
                _ => blocks.push((block, part)),
            }
        }
        for (block, parts) in blocks {
            before.push((block, j.note(block, parts, now)));
        }
        for (_, others) in &elsewhere {
            for other in others {
                j.note(other / PAGE, WHOLE, now);
            }
        }
    }
    for &(at, len) in spans {
        // A placed texture there changes with no descriptor written.
        crate::descriptors::host_heap::remapped(at, len);
    }
    for other in elsewhere.into_iter().flat_map(|(_, others)| others) {
        crate::descriptors::host_heap::remapped(other, BLOCK);
    }
    (noted, before)
}

/// Whether every write of the title's to 64 KiB block `block` so far is in
/// the journal ([`PUBLISHED`]).
pub(crate) fn block_published(block: usize) -> bool {
    published(block * BLOCK, (block + 1) * BLOCK)
}

/// Whether shaders' writes went into a page of `[at, at + len)` in the last
/// `frames` frames.
pub(crate) fn landed_lately(at: usize, len: usize, frames: u64) -> bool {
    let now = crate::d3d12::frame::current();
    (at / 4096..=(at + len.max(1) - 1) / 4096).any(|page| LANDED.since(page, now.saturating_sub(frames), now))
}

/// Whether every block of `[at, at + len)` is view memory written since a
/// host copy last took it: guarded then, and the guard is off. (Written by
/// the title, or by us putting a flight's shader writes into it, if not
/// beside the guard, [`landed_beside`].) Not for
/// blocks no copy ever took, which were never guarded: the caller's range
/// lies in a copy a flight was given, which took all of it. (Nor for blocks
/// read-only by way of another view of their page: whether they were written
/// since is not known.)
pub(crate) fn written_since_taken(at: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let g = guards();
    (at / BLOCK..=(at + len - 1) / BLOCK)
        .all(|b| !g.clean.contains(b) && !g.absent.contains(b) && !g.protected.contains(b))
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    let last = at.saturating_add(len).div_ceil(BLOCK);
    if !light() || PROTECTED.any(at / BLOCK, last) {
        let mut g = guards_timed();
        let opened = open(&mut g, at, at.saturating_add(len));
        #[cfg(feature = "diagnostics")]
        {
            tally(6, opened as u64);
            crate::diagnostics::fault_log::note(crate::diagnostics::fault_log::What::Ours, at / BLOCK, last, opened as u64);
        }
        let _ = opened;
    }
    // A placed texture there changes with no descriptor written.
    crate::descriptors::host_heap::remapped(at, len);
}

/// The kernel writes title memory `[at, at + len)` until what this returns
/// is dropped ([`kernel_writing`]).
pub struct KernelWrite(usize, usize);

/// The runtime is about to have the kernel write title memory
/// `[at, at + len)` (a file read): its guarded blocks are opened, as by
/// [`writing`], and no refresh guards them again before what this returns is
/// dropped, at the end of that call. One that did would make the kernel's
/// write fail (the kernel does not fault into our handler): a read cut
/// short, which the title takes for what the file holds.
pub fn kernel_writing(at: usize, len: usize) -> KernelWrite {
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    {
        let mut g = guards_timed();
        open(&mut g, at, at.saturating_add(len));
        if len != 0 {
            // Nothing known of them stands while they are left alone.
            let absent: Vec<usize> = g.absent.range(first..last).collect();
            for block in absent {
                g.absent.remove(block);
                if g.held.remove(block) {
                    PUBLISHED.clear(block);
                }
            }
            g.kernel.push((first, last));
        }
    }
    crate::descriptors::host_heap::remapped(at, len);
    KernelWrite(first, if len == 0 { first } else { last })
}

impl Drop for KernelWrite {
    fn drop(&mut self) {
        if self.0 == self.1 {
            return;
        }
        let mut g = guards();
        if let Some(i) = g.kernel.iter().position(|&r| r == (self.0, self.1)) {
            g.kernel.swap_remove(i);
        }
    }
}

/// What `OATHKEEP_PAGE_WRITES` asks to be done with a write the guards do
/// not see once its bytes are there ([`title_memory_written`]), as bits:
/// 1, the guarded blocks under it are opened and count as written (host
/// copies of buffers and textures read through tables take them again);
/// 2, the placed textures picked by index that lie under it are due again;
/// 4, both also for reads to plain addresses when they have finished.
/// Unset or `guards`: bit 1 alone. `0`: nothing, counted only (the default
/// until run 199). `1`: bits 1 and 2. `slots`: bit 2 alone. `all`:
/// everything.
/// (Bit 1 by default: in every run 10,000 to 50,000 guarded blocks of host
/// copies lay under such writes and kept the bytes from before; runs 197
/// and 198 opened them, 9,766 and 10,796 blocks, with nothing worse for
/// it. Bit 2 makes texture slots due and is the one that flooded the
/// queue in an earlier form: still asked for by name.)
pub(crate) fn page_writes() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("OATHKEEP_PAGE_WRITES").as_deref().map(str::trim) {
        Ok("0") => 0,
        Ok("1") => 3,
        Ok("slots") => 2,
        Ok("all") => 7,
        _ => 1,
    })
}

/// Since the start: writes into physical pages through the runtime's own
/// views that landed at addresses the title has mapped (one for each run
/// of addresses); their bytes; guarded blocks that lay under them (host
/// copies that would otherwise keep the bytes from before); reads to
/// plain addresses that finished.
pub(crate) static WRITTEN: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The bytes of a write the guards do not see are there: title memory
/// `[at, at + len)`. `pages`: DirectStorage wrote them into physical pages
/// through a view of the runtime's own, and this is an address where the
/// title has those pages mapped (no title address faulted, nothing was
/// mapped or unmapped; the shortcut this file's header names). Otherwise a
/// read to a plain address has finished, of which [`writing`] was told
/// before it began.
///
/// Upstream's c75fdba does the same for its trackers (its `Writing` marks
/// the pages once more when dropped; page destinations are reported at
/// each address showing them). Here it is counted always and acted on as
/// [`page_writes`] says. Called without the runtime's page-table lock;
/// takes the guards' lock, then, alone, the lock of the ranges kept for
/// the texture pass.
pub fn title_memory_written(at: usize, len: usize, pages: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if len == 0 {
        return;
    }
    let mode = page_writes();
    let end = at.saturating_add(len);
    if pages {
        WRITTEN[0].fetch_add(1, Relaxed);
        WRITTEN[1].fetch_add(len as u64, Relaxed);
    } else {
        WRITTEN[3].fetch_add(1, Relaxed);
        if mode & 4 == 0 {
            return;
        }
    }
    {
        let mut g = guards();
        let guarded = g.protected.range(at / BLOCK..end.div_ceil(BLOCK)).count() as u64;
        if pages {
            WRITTEN[2].fetch_add(guarded, Relaxed);
        }
        if mode & 1 != 0 && guarded != 0 {
            open(&mut g, at, end);
        }
    }
    // A placed texture there changed with no descriptor written.
    crate::descriptors::host_heap::page_written(at, len);
}

/// The runtime mapped a view of the physical bytes `[offset, offset + len)`
/// of `section` (`crate::memory::inplace::title_memory_view`): the view is
/// writable, so the same bytes guarded at another view must be checked
/// again - and are, the pages noted written, which also has the next check
/// guard the new view (`cover`).
pub(crate) fn views_mapped(section: usize, offset: u64, len: usize) {
    if !view_guard() {
        return;
    }
    let ranges = crate::memory::inplace::views_of_section(section, offset, len);
    // (A block is noted in `PROTECTED` ahead of the look at its page's views
    // that did not find this one yet.)
    if !ranges.iter().any(|&(at, n)| PROTECTED.any(at / BLOCK, (at + n).div_ceil(BLOCK))) {
        return;
    }
    // All of them, once one is guarded: another may have been made clean
    // since that look, relying on those that are opened here. And all
    // together: none may be writable while another is clean (who asks about
    // that one by its bits takes no lock).
    let mut g = guards();
    let blocks: Vec<usize> =
        ranges.iter().flat_map(|&(at, n)| g.protected.range(at / BLOCK..(at + n).div_ceil(BLOCK)).collect::<Vec<_>>()).collect();
    #[cfg(feature = "diagnostics")]
    ALIASES[2].fetch_add(blocks.len() as u64, std::sync::atomic::Ordering::Relaxed);
    unguard(&mut g, &blocks);
}

/// The runtime is about to unmap a view of which it maps a part again at
/// once (`title_memory_unmapping` of all of it comes next): the title's
/// threads may write through that part before we are told of it
/// ([`views_mapped`]). What is guarded of its pages at other views is opened,
/// and not guarded again before the change is done (`Remapping`).
pub fn title_memory_splitting(at: usize, len: usize) {
    let others: Vec<usize> = elsewhere(at, len).into_iter().flat_map(|(_, others)| others).collect();
    if others.is_empty() {
        return;
    }
    let mut g = guards();
    // (The view itself too: the part that stays is read-only until then. And
    // all of them told as written before the first is writable.)
    for &other in &others {
        g.clean.remove(other / BLOCK);
        PUBLISHED.clear(other / BLOCK);
    }
    open(&mut g, at, at.saturating_add(len));
    for other in others {
        open(&mut g, other, other + BLOCK);
        if CHANGING.get().0 {
            CHANGING.set((true, true));
            g.changing.push((other / BLOCK, other / BLOCK + 1));
        }
    }
}
