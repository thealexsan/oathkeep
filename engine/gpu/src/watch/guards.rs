use std::collections::BTreeSet;
use std::sync::Mutex;

use super::BLOCK;
use super::bits::{Bits, Blocks, PageHash};
use super::journal::waited;
use super::mapping::title_memory_reading_now;
use super::protect::{open, uncommitted_end};

/// Whether the refreshes of host copies go the light way
/// (`OATHKEEP_LIGHT_REFRESH=0`: as before): view memory nothing wrote since
/// it was last asked about is known by its bits ([`PUBLISHED`]), a copy
/// nothing of whose range changed is told so before any thread is woken
/// for it (`crate::host`, `Shadow::nothing_new`), and the ends of a range,
/// pages the range holds in part, are not copied again at every refresh
/// (`Shadow::copy_stale`).
///
/// A frame asked about view memory some 1,500 times, for nearly all of it
/// to hear that nothing was written, on up to seven threads at once, each
/// asking under the guards' one lock and behind `ASKING`'s: 94 ms a frame
/// of all threads' time (run record-1), most of it waiting.
pub(crate) fn light() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LIGHT_REFRESH").is_none_or(|v| v != "0"))
}

/// The blocks that are read-only by our hand, not written since, and of
/// which the journal holds every write from before that: for those, what
/// `writes` answers is in the journal, and neither the kernel, nor the
/// guards' lock, nor `ASKING` need be asked.
///
/// Why that is so: a block's bit is set under the guards' lock while the
/// block is clean, by the asking that made it clean, after that asking noted
/// in the journal what it found ([`publish`]). It is cleared under the same
/// lock, before the block becomes writable again ([`open`]) or is forgotten
/// (`title_memory_remapped`). So while the bit is set nothing can have
/// written the block through this address, and who sees the bit and looks
/// at the journal after that finds every earlier write in it. A write that
/// comes while the bit is looked at is no different from one that comes just
/// after an asking: the next asking finds the block open.
///
/// So too for the blocks of placeholders that were copied as zeros
/// (`Guards::held`): nothing can be there before the runtime maps it, and
/// it tells of that (`title_memory_remapped`) before the title is.
pub(super) static PUBLISHED: Bits = Bits::new();

/// The blocks read-only by our hand, or about to be: set under the guards'
/// lock ahead of the protection, cleared under it when the block is open
/// again. Who finds none set has no guard to open and need not wait for the
/// lock (`writing`): some 700 pieces a frame are landed in memory that is
/// no view.
pub(crate) static PROTECTED: Bits = Bits::new();

/// The blocks the title's bytes were put in beside their guards
/// ([`put_beside`]) and that no asking has told as written since. Such a
/// block is as it was else: read-only, clean, and an asking that finds its
/// bit here takes the bit and tells it as written.
///
/// The bit is set when the bytes are in, at every view of the page, and
/// [`PUBLISHED`]'s is cleared after that. So an asking either finds the bit
/// or came before the title's thread went on from its write; and who takes
/// the bit reads the bytes after.
pub(super) static BESIDE: Bits = Bits::new();

/// How much was put into a block beside its guard since it was last told as
/// written (`crate::titlewrite::MOST`), by a few bits of its number.
static PUT: [std::sync::atomic::AtomicU32; 1 << 13] = [const { std::sync::atomic::AtomicU32::new(0) }; 1 << 13];

pub(super) fn put_count(block: usize) -> &'static std::sync::atomic::AtomicU32 {
    &PUT[block % PUT.len()]
}

/// Whether [`put_beside`] put it there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Beside {
    Done,
    /// The block has had its share.
    Full,
    /// Not guarded, or there is no other way to its page, or the runtime is
    /// changing a mapping.
    Cannot,
}

/// Whether a block of `[at, at + len)` is (or is about to be) read-only by
/// our hand. `len` is not 0.
#[inline]
pub(crate) fn guards_any(at: usize, len: usize) -> bool {
    PROTECTED.any(at / BLOCK, (at.saturating_add(len) - 1) / BLOCK + 1)
}

/// The title's bytes for a guarded block, put in by `put` at our own view of
/// the same page: it is given what to add to an address of the block (as by
/// `wrapping_add`). `weight`: how much of the block's share it is.
pub(crate) fn put_beside(block: usize, weight: u32, put: impl FnOnce(usize)) -> Beside {
    use std::sync::atomic::Ordering::Relaxed;
    if !light() || !view_guard() || !PROTECTED.any(block, block + 1) {
        return Beside::Cannot;
    }
    let count = put_count(block);
    if count.load(Relaxed) + weight > crate::titlewrite::MOST {
        return Beside::Full;
    }
    // No mapping changes until all is told: a view of the page that came
    // after the look at them would not be.
    let Some(_reading) = title_memory_reading_now() else {
        return Beside::Cannot;
    };
    let Some((page, shown)) = crate::inplace::beside_page(block * BLOCK) else {
        return Beside::Cannot;
    };
    put(page.wrapping_sub(block * BLOCK));
    count.fetch_add(weight, Relaxed);
    for view in shown.iter().filter(|&&view| view != 0) {
        BESIDE.set(view / BLOCK);
    }
    for view in shown.iter().filter(|&&view| view != 0) {
        PUBLISHED.clear(view / BLOCK);
    }
    Beside::Done
}

/// A faulting store of the title's leaves `block` as it is: whether it may be
/// left out (until that has been so often, since the block was taken, that
/// the faults cost more than opening it).
pub(crate) fn nothing_put(block: usize) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let count = put_count(block);
    count.load(Relaxed) < crate::titlewrite::MOST && count.fetch_add(1, Relaxed) < crate::titlewrite::MOST
}

/// The title is about to write block by block around `at`: as by a fault.
pub(crate) fn opened_for_writing(at: usize) {
    let mut g = guards();
    if open(&mut g, at, at + 1) != 0 {
        g.opened(at / BLOCK);
    }
}

/// An asking made blocks of `pages` clean and noted them in the journal.
pub(super) fn publish(pages: &[usize]) {
    let g = guards();
    for &page in pages {
        if g.clean.contains(page) || g.held.contains(page) {
            PUBLISHED.set(page);
            // (Put there since: whoever did clears the bit after this is
            // set, or its own bit is found here.)
            if BESIDE.any(page, page + 1) {
                PUBLISHED.clear(page);
            }
        }
    }
}

/// Whether placeholders are known for what they are (`OATHKEEP_KNOWN_HOLES=0`:
/// every hole is looked at at every asking, as before).
///
/// Most ranges asked about have holes, reservations the title maps its pages
/// into as it streams (231,000 regions, 1,125 of a frame's 1,530 asked view
/// ranges, run next1-1). Such a range was never known whole: each asking put
/// a question to Windows for each of its holes, under the guards' lock, and
/// 111 ms a frame of all threads' time went into waiting for that lock.
pub(super) fn known_holes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_KNOWN_HOLES").is_none_or(|v| v != "0") && light())
}

/// Whether `block` lies in a placeholder: a reservation in which pages come
/// to be only by mapping a view (or an allocation) in its place, which an
/// ordinary commit cannot do.
pub(super) fn placeholder(block: usize) -> bool {
    /// `MEMORY_REGION_INFORMATION` (ntddk).
    #[repr(C)]
    #[derive(Default)]
    struct Region {
        allocation_base: usize,
        allocation_protect: u32,
        region_type: u32,
        region_size: usize,
        commit_size: usize,
        partition_id: usize,
        node_preference: usize,
    }
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtQueryVirtualMemory(
            process: *mut std::ffi::c_void,
            at: *const std::ffi::c_void,
            class: u32,
            out: *mut std::ffi::c_void,
            len: usize,
            returned: *mut usize,
        ) -> i32;
    }
    // (Class 3 answers with the kind of memory in that word, not the flags.)
    const MEMORY_REGION_INFORMATION_EX: u32 = 7;
    const PLACEHOLDER_RESERVATION: u32 = 1 << 8;
    let mut region = Region::default();
    // SAFETY: the answer's room and its size.
    let status = unsafe {
        NtQueryVirtualMemory(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            (block * BLOCK) as *const _,
            MEMORY_REGION_INFORMATION_EX,
            (&raw mut region).cast(),
            size_of::<Region>(),
            std::ptr::null_mut(),
        )
    };
    status >= 0 && region.region_type & PLACEHOLDER_RESERVATION != 0
}

/// The runtime gave the title memory at `[at, at + len)`, allocated or
/// committed: what was known of holes there is asked again.
pub fn title_memory_allocated(at: usize, len: usize) {
    if len == 0 || !known_holes() {
        return;
    }
    let mut g = guards();
    let blocks: Vec<usize> = g.held.range(at / BLOCK..at.saturating_add(len).div_ceil(BLOCK)).collect();
    for block in blocks {
        g.held.remove(block);
        PUBLISHED.clear(block);
    }
}

/// Once a frame: a hundredth of the known holes are looked at, in case one
/// was filled with nobody telling ([`title_memory_allocated`],
/// `title_memory_remapped`), which no title we know does.
pub(crate) fn look_at_holes() {
    const PARTS: usize = 100;
    if !known_holes() {
        return;
    }
    static FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let blocks: Vec<usize> = {
        let g = guards();
        let from = FROM.load(std::sync::atomic::Ordering::Relaxed);
        let part: Vec<usize> = g.held.range(from..).take(g.held.len() / PARTS + 1).collect();
        FROM.store(part.last().map_or(0, |b| b + 1), std::sync::atomic::Ordering::Relaxed);
        part
    };
    let mut until = 0;
    for block in blocks {
        if block >= until {
            until = uncommitted_end(block);
        }
        if block >= until {
            // (Or unmapped memory was mapped since the part was taken: told
            // already, and forgotten.)
            let mut g = guards();
            if g.held.remove(block) {
                PUBLISHED.clear(block);
                static SAID: std::sync::Once = std::sync::Once::new();
                SAID.call_once(|| {
                    tracing::warn!(
                        address = format_args!("{:#x}", block * BLOCK),
                        "a placeholder of the title's was filled with nobody telling (OATHKEEP_KNOWN_HOLES=0 asks every time)"
                    );
                });
            }
        }
    }
}

/// Whether a block of `[start, end)` is view memory read-only by our hand:
/// the kernel keeps no written pages of it and would refuse an asking.
pub(crate) fn guarded(start: usize, end: usize) -> bool {
    PROTECTED.any(start / BLOCK, end.div_ceil(BLOCK))
}

/// Whether nothing of `[start, end)` can have been written since an asking
/// noted it in the journal ([`PUBLISHED`]).
pub(super) fn published(start: usize, end: usize) -> bool {
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // (In this order: who puts something in beside a guard sets the one bit,
    // then clears the other, which an asking that is publishing the block may
    // just be setting.)
    light() && PUBLISHED.all(first, last) && !BESIDE.any(first, last)
}

/// Since the last frame line: what asking about title memory was. Kernel
/// askings and the KiB they walked; view ranges answered by their bits, by
/// the guards whole clean, and block by block; blocks made read-only; blocks
/// opened by ourselves; nanoseconds waiting for the guards' lock and
/// nanoseconds of changing protections (all threads); questions to Windows
/// about holes; kernel askings refused; nanoseconds in kernel askings; the
/// 4 KiB pages they found written.
#[cfg(feature = "diagnostics")]
static ASKS: [std::sync::atomic::AtomicU64; 13] = [const { std::sync::atomic::AtomicU64::new(0) }; 13];

#[cfg(feature = "diagnostics")]
pub(super) fn tally(what: usize, n: u64) {
    ASKS[what].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// [`ASKS`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn asks() -> [u64; 13] {
    std::array::from_fn(|i| ASKS[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The guards' lock, the wait for it counted.
pub(super) fn guards_timed() -> std::sync::MutexGuard<'static, Guards> {
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let g = guards();
    #[cfg(feature = "diagnostics")]
    tally(7, started.elapsed().as_nanos() as u64);
    g
}

/// Whether all the views of a guarded page are guarded together
/// (`OATHKEEP_VIEW_GUARD`).
pub(super) fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_VIEW_GUARD").is_none_or(|v| v != "0"))
}

/// Whether a write fault opens the view that was written and leaves the
/// page's other views read-only, not clean any more
/// (`OATHKEEP_OPEN_ONE_VIEW=0`: opens them all, as before).
///
/// The others were opened (a change of protection each, under the guards'
/// lock, with other faults waiting) and made read-only again when the page
/// was next taken, though the title writes through one of them: 586 faults a
/// frame opened 880 blocks (run faults-1). A write through one left as it was
/// faults, and opens it.
pub(super) fn open_one_view() -> bool {
    crate::toggled!("OATHKEEP_OPEN_ONE_VIEW")
}

/// How long a block the title has just opened by a write is left open by
/// those that take it (`OATHKEEP_GUARD_REST=<microseconds>`, 3,000 unless
/// given; `0`: not at all, as before).
///
/// 220 of a frame's 586 faults came within 3 ms after a refresh had taken the
/// block and made it read-only, while the title was still writing it, and
/// what a refresh takes is asked about again by the next within a millisecond
/// (run faults-1): made read-only, written, opened, up to a dozen times a
/// frame. A block that rests counts as written at each asking, so nothing is
/// missed; it is read-only at the first asking after its rest, which the
/// title's later writes are seen by.
pub(super) fn rest() -> std::time::Duration {
    #[cfg(test)]
    {
        REST_IN_TEST.get()
    }
    #[cfg(not(test))]
    {
        static US: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        if !crate::toggled!("OATHKEEP_GUARD_REST") {
            return std::time::Duration::ZERO;
        }
        let us = *US.get_or_init(|| {
            std::env::var("OATHKEEP_GUARD_REST").ok().and_then(|v| v.trim().parse().ok()).filter(|us| *us > 1).unwrap_or(3000)
        });
        std::time::Duration::from_micros(us)
    }
}

#[cfg(test)]
thread_local! {
    /// [`rest`], for what this thread asks.
    pub(super) static REST_IN_TEST: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

pub(super) struct Guards {
    /// Blocks we made read-only.
    pub(super) protected: Blocks,
    /// Of those, the ones not written since a copy took them.
    pub(super) clean: Blocks,
    /// Blocks already copied as zeros. While they remain wholly uncommitted,
    /// the copy can skip hundreds of MB of failing kernel reads per world frame.
    pub(super) absent: Blocks,
    /// Of those, the ones in placeholders ([`known_holes`]).
    pub(super) held: Blocks,
    /// Blocks that were absent and turned out to be other memory than views
    /// when something was there again: never guarded, copied every time.
    pub(super) foreign: BTreeSet<usize>,
    /// Blocks (first, past the last) the kernel writes now
    /// (`kernel_writing`), and blocks the runtime has unmapped in a change
    /// not yet done (`title_memory_unmapping`). Not in the sets above, and
    /// left as they are by `guard`, which reports them written.
    pub(super) kernel: Vec<(usize, usize)>,
    pub(super) changing: Vec<(usize, usize)>,
    /// Blocks whose page is shown at other addresses too and that were made
    /// read-only together with those (which are in it as well): what opens
    /// one opens them all. (A write's range can be huge, `fread`'s, so only
    /// these are looked up.)
    pub(super) aliased: Blocks,
    /// Blocks opened by a write fault, and when ([`rest`]).
    pub(super) opened: std::collections::HashMap<usize, std::time::Instant, std::hash::BuildHasherDefault<PageHash>>,
}

impl Guards {
    pub(super) fn left_alone(&self, block: usize) -> bool {
        self.kernel.iter().chain(&self.changing).any(|&(first, last)| (first..last).contains(&block))
    }

    /// Whether `block` is left open for now: the title has just written it.
    pub(super) fn rests(&self, block: usize, rest: std::time::Duration) -> bool {
        !rest.is_zero() && self.opened.get(&block).is_some_and(|at| at.elapsed() < rest)
    }

    /// A write fault opened `block`.
    pub(super) fn opened(&mut self, block: usize) {
        const MANY: usize = 1 << 13;
        if self.opened.len() >= MANY {
            self.opened.retain(|_, at| at.elapsed().as_millis() < 50);
            if self.opened.len() >= MANY {
                self.opened.clear();
            }
        }
        put_count(block).store(0, std::sync::atomic::Ordering::Relaxed);
        self.opened.insert(block, std::time::Instant::now());
    }
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: Blocks::new(),
    clean: Blocks::new(),
    absent: Blocks::new(),
    held: Blocks::new(),
    foreign: BTreeSet::new(),
    kernel: Vec::new(),
    changing: Vec::new(),
    aliased: Blocks::new(),
    opened: std::collections::HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
});

pub(super) fn guards() -> std::sync::MutexGuard<'static, Guards> {
    if let Ok(g) = GUARDS.try_lock() {
        return g;
    }
    waited(3, || GUARDS.lock().unwrap_or_else(|e| e.into_inner()))
}
