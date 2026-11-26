use super::super::*;

use super::Batch;
use crate::memory::shadow::Shadow;
use std::sync::Arc;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_RESERVE, MapViewOfFile, PAGE_READWRITE,
    VirtualAlloc,
};

pub(super) const PAGE: usize = 64 << 10;
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// Title memory for the tests (never given back: copies of it stay in
/// the guards): write-watched, or a view of a section.
pub(super) fn title(len: usize, view: bool) -> usize {
    // SAFETY: plain allocations.
    let at = unsafe {
        if view {
            let section =
                CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, len as u32, std::ptr::null());
            let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            // As the runtime tells of it.
            crate::title_memory_section(section as usize, len as u64, true);
            crate::title_memory_remapped(at, len);
            crate::title_memory_view(at, len, section as usize, 0);
            at
        } else {
            VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) as usize
        }
    };
    assert_ne!(at, 0);
    at
}

/// Title memory of `blocks` pages in a placeholder, one view to a page,
/// and every fourth page not there: as a title that streams has it.
pub(super) fn with_holes(blocks: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS,
        VirtualAlloc2, VirtualFree,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a placeholder and a section of our own.
    unsafe {
        let at = VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            blocks * PAGE,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        assert_ne!(at, 0);
        let len = blocks * PAGE;
        let section =
            CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, (len >> 32) as u32, len as u32, std::ptr::null());
        for block in (0..blocks).filter(|b| b % 4 != 3) {
            let to = at + block * PAGE;
            VirtualFree(to as *mut _, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            let view = MapViewOfFile3(
                section,
                GetCurrentProcess(),
                to as *const _,
                (block * PAGE) as u64,
                PAGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(view.Value as usize, to);
            crate::title_memory_remapped(to, PAGE);
        }
        at
    }
}

pub(super) fn memory(len: usize) -> *mut u8 {
    // SAFETY: a plain allocation.
    unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE).cast() }
}

pub(super) struct Random(pub(super) u64);

impl Random {
    pub(super) fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as usize % n
    }
}

/// Prepares as a submission does: one asking for all the ranges, the
/// refreshes in several flushes, the asking's end.
pub(super) fn submission(ranges: &[(Arc<Shadow>, usize, usize)], flushes: usize) {
    // One at a time, as submissions are.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let asked: Vec<(usize, usize)> = ranges.iter().filter(|r| !r.0.unwatched()).map(|r| (r.1, r.1 + r.2)).collect();
    crate::memory::watch::ask_once(&asked);
    let mut batch = Batch::default();
    for part in ranges.chunks(ranges.len().div_ceil(flushes).max(1)) {
        for (shadow, at, len) in part {
            batch.add(shadow, *at, *len);
        }
        batch.flush();
    }
    crate::memory::watch::ask_once_end();
}

/// Whatever is written where, by the title or by us, and whichever
/// copies (of both kinds of memory, over one another, none on a page
/// line) are refreshed in which ranges: a refreshed range holds what
/// title memory holds.
#[test]
fn refreshed_ranges_hold_title_memory() {
    // SAFETY (all blocks): title memory and copies of the test's own,
    // written by this thread and, in `Batch::flush`, by the threads that
    // refresh, which are done when it returns.
    let mut random = Random(0x1234_5678_9ABC_DEF1);
    const LEN: usize = 24 * PAGE;
    let regions = [title(LEN, false), title(LEN, true)];
    let mut copies = Vec::new();
    for &region in &regions {
        for (from, to) in [(0, LEN), (100, LEN - 5000), (3 * PAGE + 777, 9 * PAGE + 10), (8 * PAGE, 8 * PAGE + 300), (PAGE, 20 * PAGE)] {
            let data = memory(to - from);
            copies.push((Shadow::unbacked((region + from) as u64, data, (to - from) as u64, false), data));
        }
    }
    let byte = |at: usize| unsafe { (at as *const u8).read_volatile() };
    for round in 0..600 {
        for _ in 0..random.below(4) {
            let at = regions[random.below(2)] + random.below(LEN - 64);
            let n = 1 + random.below(60);
            if random.below(3) == 0 {
                // What shaders wrote.
                let was: Vec<u8> = (0..n).map(|i| byte(at + i)).collect();
                let now: Vec<u8> = was.iter().map(|b| if random.below(2) == 0 { *b } else { random.below(256) as u8 }).collect();
                Shadow::put(at, &was, &now);
                assert!(now.iter().enumerate().all(|(i, &b)| byte(at + i) == b), "not put");
                continue;
            }
            for i in 0..n {
                unsafe { ((at + i) as *mut u8).write_volatile(random.below(256) as u8) };
            }
        }
        let mut ranges = Vec::new();
        for (shadow, _) in &copies {
            if random.below(3) == 0 {
                continue;
            }
            let (start, len) = shadow.span();
            let (start, len) = (start as usize, len as usize);
            // Often the range of the time before, as a list's is.
            let from = if random.below(2) == 0 { 0 } else { random.below(len) };
            let n = if random.below(2) == 0 { len - from } else { 1 + random.below(len - from) };
            ranges.push((shadow.clone(), start + from, n));
        }
        if ranges.is_empty() {
            continue;
        }
        let flushes = 1 + random.below(3);
        submission(&ranges, flushes);
        for (shadow, at, n) in &ranges {
            let data = copies.iter().find(|c| Arc::ptr_eq(&c.0, shadow)).unwrap().1;
            let start = shadow.span().0 as usize;
            let same = unsafe {
                std::slice::from_raw_parts(*at as *const u8, *n) == std::slice::from_raw_parts(data.add(at - start), *n)
            };
            assert!(same, "round {round}: {at:#x}+{n:#x} of a copy at {start:#x}; first byte {}", byte(*at));
        }
    }
}

/// A copy in the card's memory, with the card's side and the sends to it
/// played by the test.
pub(super) struct Carded {
    pub(super) shadow: Arc<Shadow>,
    pub(super) data: usize,
    pub(super) start: usize,
    pub(super) len: usize,
    /// The last send, the last send every fetch to come is behind, and
    /// the card's bytes: the queue's order is the order it is locked in.
    pub(super) card: std::sync::Mutex<(u64, u64, Vec<u8>)>,
    /// Whether a flight that writes it has not landed.
    pub(super) flying: std::sync::atomic::AtomicBool,
    /// The dword of each eight its shaders write.
    pub(super) class: usize,
}

impl Carded {
    pub(super) fn new(start: usize, len: usize, class: usize) -> Carded {
        let data = memory(len);
        Carded {
            shadow: Shadow::unbacked(start as u64, data, len as u64, true),
            data: data as usize,
            start,
            len,
            card: std::sync::Mutex::new((0, 0, vec![0; len])),
            flying: std::sync::atomic::AtomicBool::new(false),
            class,
        }
    }

    /// What changed on the CPU's side goes up.
    pub(super) fn send(&self) {
        let mut card = self.card.lock().unwrap();
        card.0 += 1;
        let (number, merged) = (card.0, card.1);
        for (a, b) in self.shadow.send_up(number, merged) {
            // SAFETY: the copy's bytes; nothing writes those that are to
            // go up.
            unsafe { std::ptr::copy_nonoverlapping((self.data + a) as *const u8, card.2[a..b].as_mut_ptr(), b - a) };
        }
    }
}
