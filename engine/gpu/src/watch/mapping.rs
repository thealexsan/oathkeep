use super::BLOCK;
use super::fault::{install, writable};
use super::guards::{Guards, PROTECTED, PUBLISHED, guards};
use super::protect::open;

/// Held shared by a read of title memory in place, and alone by the runtime
/// while it changes what title memory shows.
///
/// The runtime does not change a mapping in one step. To unmap one page of
/// a view it unmaps the whole view and maps what stays again
/// (`xmem.rs` `unmap`), and a view it maps is a reservation until the next
/// call commits it (`map`, then `commit`). For that moment pages the title
/// never gave up are "reserved, never committed", and a thread reading
/// them in place takes an access violation: run 71 in `Layout::detile`,
/// run 74 in the hash before it, both under `Staging::refresh_now`, which
/// had asked whether the texture was all mapped and been told yes. Asking
/// cannot help; not changing the mapping while such a read is under way
/// does.
static MAPPING: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// What the runtime holds while it changes what title memory shows
/// ([`title_memory_remapping`]).
pub struct Remapping(#[allow(dead_code)] std::sync::RwLockWriteGuard<'static, ()>);

impl Drop for Remapping {
    fn drop(&mut self) {
        // The change is done: what is there now may be guarded.
        if CHANGING.replace((false, false)).1 {
            guards().changing.clear();
        }
        CHANGES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// How often the runtime began or ended a change of what title memory
/// shows: odd while one is under way.
static CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// This thread holds [`Remapping`]; and put blocks in
    /// `Guards::changing`.
    pub(super) static CHANGING: std::cell::Cell<(bool, bool)> = const { std::cell::Cell::new((false, false)) };
}

/// For [`again_after_change`]: to be asked before what may fail.
pub(crate) fn changes() -> u64 {
    CHANGES.load(std::sync::atomic::Ordering::SeqCst)
}

/// `f` failed to read or write title memory, at some time since [`changes`]
/// said `since`. If the runtime changed a mapping in that time, the pages
/// may have been away for that moment only ([`MAPPING`]): again, when the
/// change is done. Whether `f` did it.
pub(crate) fn again_after_change(since: u64, mut f: impl FnMut() -> bool) -> bool {
    let mut since = since;
    for _ in 0..4 {
        let now = change_done();
        if now == since {
            return false;
        }
        since = now;
        if f() {
            return true;
        }
    }
    false
}

/// Returns when the change of a mapping that is under way, if one is, is
/// over (or has been going on for 2 ms): pages it took away for its moment
/// are back. Another may have begun.
pub(crate) fn change_over() {
    let now = changes();
    if now.is_multiple_of(2) {
        return;
    }
    let started = std::time::Instant::now();
    while changes() == now && started.elapsed().as_millis() < 2 {
        for _ in 0..50 {
            std::hint::spin_loop();
        }
    }
}

/// [`changes`], when no change is under way (or one has been for a second).
fn change_done() -> u64 {
    let mut tries = 0;
    let mut started = None;
    loop {
        let now = changes();
        if now.is_multiple_of(2) {
            return now;
        }
        tries += 1;
        if tries < 100 {
            std::hint::spin_loop();
            continue;
        }
        if started.get_or_insert_with(std::time::Instant::now).elapsed().as_secs() >= 1 {
            return now;
        }
        // SAFETY: gives the processor to another thread.
        unsafe { windows_sys::Win32::System::Threading::Sleep(u32::from(tries > 200)) };
    }
}

/// Faults of title threads that met a change of mapping and went on after
/// it ([`came_back`]).
static MET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A title thread's access to `at` (`write`: a write) faulted. Whether it
/// can be made now: if a change of mapping is under way, after it.
///
/// The console changes one page's mapping and no other's. Here the rest of
/// a view is unmapped and mapped again ([`MAPPING`]), and a title thread that
/// uses a page of it just then, which the title never gave up, faulted
/// ("reserved, never committed", or read-only for the moment between our
/// forgetting a guarded block and its unmapping). It waits here instead.
/// Memory that is not there with no change under way is not waited for.
pub(super) fn came_back(at: usize, write: bool) -> bool {
    let there = || {
        if !write {
            return readable_view(at);
        }
        // (A refresh may have guarded the page that came back.)
        open(&mut guards(), at, at + 1) != 0 || writable(at)
    };
    let before = changes();
    if there() || again_after_change(before, there) {
        MET.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return true;
    }
    false
}

/// Whether `at` is in a committed page of a view that can be read.
fn readable_view(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_READONLY, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == MEM_COMMIT
        && info.Type == MEM_MAPPED
        && matches!(info.Protect, PAGE_READONLY | PAGE_READWRITE)
}

/// Title memory that could not be written with what was to go there (what a
/// shader wrote, a title's copy): counted, and said at the 1st, 2nd, 4th, 8th
/// and so on. A page the title unmapped loses nothing by it; a page that is
/// there does.
pub(crate) fn lost(what: &'static str, at: usize, len: usize) {
    static LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = LOST.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        let there = writable(at);
        tracing::warn!(n, what, at = format_args!("{at:#x}"), len, there, "title memory not written");
    }
}

/// The runtime is about to map, unmap or move physical pages of title
/// memory: until what it returns is dropped, no read of title memory in
/// place is under way and none begins ([`title_memory_reading`]). Taken
/// around the whole change, the unmapping and the mapping again.
pub fn title_memory_remapping() -> Remapping {
    // For the title's own threads ([`came_back`]).
    install();
    let met = MET.load(std::sync::atomic::Ordering::Relaxed);
    if met != 0 {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if met >= 2 * SAID.load(std::sync::atomic::Ordering::Relaxed) {
            SAID.store(met, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(met, "accesses of title threads that waited for a change of mapping");
        }
    }
    let changing = MAPPING.write().unwrap_or_else(|e| e.into_inner());
    CHANGES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    CHANGING.set((true, false));
    Remapping(changing)
}

/// What a read of title memory in place holds ([`title_memory_reading`]).
pub struct Reading(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>);

/// For reading title memory in place: until what it returns is dropped,
/// the runtime changes no mapping, so memory found mapped stays mapped.
/// Not to be taken twice by one thread, nor held across anything that
/// maps title memory.
pub fn title_memory_reading() -> Reading {
    Reading(MAPPING.read().unwrap_or_else(|e| e.into_inner()))
}

/// [`title_memory_reading`] if it can be had at once: `None` while the
/// runtime changes a mapping (or waits to), for a caller that has another
/// way to read and would rather not wait.
pub fn title_memory_reading_now() -> Option<Reading> {
    match MAPPING.try_read() {
        Ok(guard) => Some(Reading(guard)),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(Reading(e.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The runtime mapped title memory `[at, at + len)`: what it shows is new,
/// so it is no longer guarded (nor known unmapped), and read-write.
///
/// The view is there before we are told of it, and a copy's refresh on
/// another thread may have guarded its blocks in between (it finds blocks
/// it knew absent committed, and holds `GUARDS`, which we wait for).
/// Forgetting such a block and leaving it read-only would make the title's
/// next write to it an access violation nobody takes (`on_write` finds it
/// neither guarded nor writable) and a kernel read into it fail, so the
/// write access is given back.
pub fn title_memory_remapped(at: usize, len: usize) {
    crate::texback::remapped(at, len);
    open(&mut title_memory_changed(at, len), at, at.saturating_add(len));
}

/// The runtime is about to unmap title memory `[at, at + len)`: it is
/// nothing of ours any more. Its protection goes with it. Until the change
/// is done ([`Remapping`]) no refresh guards what is there: one that did
/// before the unmapping would leave the address guarded, clean and known by
/// its bits ([`PUBLISHED`]) with no view behind it.
pub fn title_memory_unmapping(at: usize, len: usize) {
    crate::texback::remapped(at, len);
    let mut g = title_memory_changed(at, len);
    if CHANGING.get().0 && len != 0 {
        CHANGING.set((true, true));
        let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
        match g.changing.last_mut() {
            Some(before) if before.1 == first => before.1 = last,
            _ => g.changing.push((first, last)),
        }
    }
    let blocks: Vec<usize> = g.protected.range(at / BLOCK..at.saturating_add(len).div_ceil(BLOCK)).collect();
    for block in blocks {
        g.protected.remove(block);
        g.clean.remove(block);
        PUBLISHED.clear(block);
        PROTECTED.clear(block);
    }
}

/// The title is about to free or decommit memory of its own,
/// `[at, at + len)`, or with no length the whole allocation at `at`: what
/// was noted of its blocks (guarded, clean, absent) would be taken for true
/// of the next memory at the address.
pub fn title_memory_freed(at: usize, len: usize) {
    let mut g = guards();
    let first = at / BLOCK;
    let noted = [g.protected.range(first..).next(), g.absent.range(first..).next(), g.foreign.range(first..).next().copied()];
    let Some(noted) = noted.into_iter().flatten().min() else {
        return;
    };
    let end = if len != 0 {
        at.saturating_add(len).div_ceil(BLOCK)
    } else {
        allocation_end(at, noted * BLOCK).div_ceil(BLOCK)
    };
    if noted >= end {
        return;
    }
    let absent: Vec<usize> = g.absent.range(first..end).collect();
    for block in absent {
        g.absent.remove(block);
        if g.held.remove(block) {
            PUBLISHED.clear(block);
        }
    }
    let foreign: Vec<usize> = g.foreign.range(first..end).copied().collect();
    for block in foreign {
        g.foreign.remove(&block);
    }
    // (Memory that is still a view is not freed by that call, which fails:
    // it must not stay read-only.)
    open(&mut g, at, end * BLOCK);
}

/// The end of the allocation at `at`, if `inside` lies in it; else `at`.
fn allocation_end(at: usize, inside: usize) -> usize {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    let ask = |address: usize| {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
        (n != 0 && info.AllocationBase as usize == at)
            .then(|| (info.BaseAddress as usize).saturating_add(info.RegionSize))
    };
    let mut end = at;
    let mut next = ask(inside.max(at));
    while let Some(to) = next {
        end = to;
        next = ask(to);
    }
    end
}

/// Tells what keeps track of what title memory shows; the guards, with no
/// block of the range known unmapped.
fn title_memory_changed(at: usize, len: usize) -> std::sync::MutexGuard<'static, Guards> {
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    crate::inplace::views_gone(at, len);
    // And nothing stays in place over it.
    crate::inplace::title_memory_freeing(at, len);
    let mut g = guards();
    let range = at / BLOCK..at.saturating_add(len).div_ceil(BLOCK);
    let absent: Vec<usize> = g.absent.range(range.clone()).collect();
    for block in absent {
        g.absent.remove(block);
        if g.held.remove(block) {
            PUBLISHED.clear(block);
        }
    }
    let foreign: Vec<usize> = g.foreign.range(range.clone()).copied().collect();
    for block in foreign {
        g.foreign.remove(&block);
    }
    let aliased: Vec<usize> = g.aliased.range(range).collect();
    for block in aliased {
        g.aliased.remove(block);
    }
    g
}
