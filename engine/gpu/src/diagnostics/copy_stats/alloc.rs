//! Allocation-counting global allocator for diagnostics builds.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{BANKS, Bank, bank};

/// Our own allocations alive now, by size of block (under 4 KiB, to 64 KiB,
/// to 1 MiB, larger): bytes, then blocks. "Our own" is the Rust code of
/// every crate in the process: in diagnostics builds [`Counting`] stands in
/// front of the system's heap. The memory map's "process heap segments"
/// hold the blocks under about half a megabyte of ours, of the graphics
/// runtime and of the driver alike (3.5 GB in run 60's gameplay, growing by
/// some 2 MB a frame, and nothing said whose): this says which part is
/// ours. Bigger blocks are allocations of their own in the map.
static OWN: [Bank<8>; BANKS] = [const { Bank([const { AtomicU64::new(0) }; 8]) }; BANKS];

/// Count `i` of [`OWN`], all sets together (a block made by one thread and
/// released by another leaves one set below zero: the sum is right).
pub(super) fn owned(i: usize) -> u64 {
    OWN.iter().fold(0u64, |sum, b| sum.wrapping_add(b.0[i].load(Relaxed)))
}

/// The class of [`OWN`] a block of `size` bytes counts in.
pub(super) fn block_class(size: usize) -> usize {
    match size {
        0..0x1000 => 0,
        0x1000..0x1_0000 => 1,
        0x1_0000..0x10_0000 => 2,
        _ => 3,
    }
}

fn own(size: usize, made: bool) {
    let class = block_class(size);
    let own = &OWN[bank()].0;
    if made {
        own[class].fetch_add(size as u64, Relaxed);
        own[4 + class].fetch_add(1, Relaxed);
    } else {
        own[class].fetch_sub(size as u64, Relaxed);
        own[4 + class].fetch_sub(1, Relaxed);
    }
}

/// Our own blocks of [`BIG`] bytes and more alive now: where each is (0:
/// none) and its bytes. The memory map lists the process's big allocations
/// by size without saying whose they are (run 65: one of 544 MB, one of
/// 401 MB, two of 174 MB, two of 115 MB); the sizes here say which of them
/// are ours.
pub(super) static BIG_AT: [std::sync::atomic::AtomicUsize; 64] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 64];
pub(super) static BIG_BYTES: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
const BIG: usize = 32 << 20;

/// A block of [`BIG`] bytes or more was made at `at`, or released there.
fn big(at: *mut u8, size: usize, made: bool) {
    if size < BIG {
        return;
    }
    for (place, bytes) in BIG_AT.iter().zip(&BIG_BYTES) {
        if made {
            if place.compare_exchange(0, at as usize, Relaxed, Relaxed).is_ok() {
                bytes.store(size as u64, Relaxed);
                return;
            }
        } else if place.load(Relaxed) == at as usize {
            bytes.store(0, Relaxed);
            place.store(0, Relaxed);
            return;
        }
    }
}

/// The sizes of our own big blocks alive, largest first, in MB.
pub(super) fn big_blocks() -> String {
    let mut sizes: Vec<u64> = BIG_BYTES.iter().map(|b| b.load(Relaxed)).filter(|&b| b != 0).collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let text: Vec<String> = sizes.iter().map(|b| format!("{:.2}", *b as f64 / (1 << 20) as f64)).collect();
    if text.is_empty() { "none".into() } else { text.join(", ") }
}

/// The system's allocator with every block counted in [`OWN`] (two atomic
/// additions per allocation and per release: diagnostics builds only).
struct Counting;

// SAFETY: every call goes to the system's allocator unchanged; only
// counters are kept beside it.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let at = unsafe { std::alloc::System.alloc(layout) };
        if !at.is_null() {
            own(layout.size(), true);
            big(at, layout.size(), true);
        }
        at
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let at = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !at.is_null() {
            own(layout.size(), true);
            big(at, layout.size(), true);
        }
        at
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: std::alloc::Layout) {
        own(layout.size(), false);
        big(at, layout.size(), false);
        // SAFETY: the caller's contract, passed on.
        unsafe { std::alloc::System.dealloc(at, layout) };
    }

    unsafe fn realloc(&self, at: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let moved = unsafe { std::alloc::System.realloc(at, layout, size) };
        if !moved.is_null() {
            own(layout.size(), false);
            own(size, true);
            big(at, layout.size(), false);
            big(moved, size, true);
        }
        moved
    }
}

#[global_allocator]
static HEAP: Counting = Counting;
