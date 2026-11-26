//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::memory::watch`]: write
//! watching, or read-only guards on physical-page views), all of it where
//! neither works. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! shortcut: only the ranges draws are known to read are brought up to
//! date (vertex and index buffer views, constant buffers in tables, root
//! descriptors, buffer views in tables); buffers read through directly
//! indexed heaps keep what their first use copied.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

mod card_alloc;
mod card_runs;
mod cbv_slab;
mod diff;
mod exact;
mod loose;
mod pool;
mod put;
mod registry;
mod relay;
mod shadow_back;
mod shadow_card;
mod shadow_core;
mod shadow_merge;
mod shadow_update;
mod snap;
mod wide;
mod window_grid;

pub(crate) use card_alloc::{CARD_STATS, card_hash_bytes, card_room_kept, card_stats, cards, live_span_bytes};
#[allow(unused_imports)]
pub(crate) use card_runs::{CardSpan, card_dirty, carded_copies, copy_all, joined_piece_runs, send_copied, stale_pages};
#[allow(unused_imports)]
pub(crate) use cbv_slab::{find_aligned_cbv, find_aligned_cbv_existing, has_aligned_cbv};
pub(crate) use diff::merged_stats;
#[cfg(feature = "diagnostics")]
pub(crate) use diff::stale_why;
pub(crate) use exact::{OWN_MADE, exact, exact_line};
pub(crate) use loose::by_address;
#[allow(unused_imports)]
pub(crate) use put::{ECHO, PUT, exact_back};
#[cfg(feature = "diagnostics")]
pub(crate) use put::send_stats;
#[cfg(test)]
pub(crate) use put::land_as_before;
pub(crate) use registry::{find, find_any, in_place_let_go, near, peek, register, spans, unregister};
#[cfg(feature = "diagnostics")]
pub(crate) use registry::extent;
pub(crate) use relay::{preparing, relay, relay_fetched};
pub(crate) use snap::{back_line, find_all, take_snapshots};
pub(crate) use wide::{wide_back, wide_each, wide_run};
pub(crate) use window_grid::{in_buffer, let_go_idle, small_buffer_in};
#[cfg(feature = "diagnostics")]
pub(crate) use window_grid::{memory, window_cover, window_use, windows};

use card_alloc::CARD_BYTES;
use card_runs::SentRuns;
use cbv_slab::release_cbv_slab_slot;
use loose::{WINDOW_CARD_BYTES, WINDOW_MAPPED_BYTES};
use pool::{KEPT_BYTES, PINNED_SNAP_BYTES, give_back};
use relay::Flights;

/// A title buffer's host copy.
struct CopiedState {
    pages: Vec<u64>,
    /// Exact `(from, to) -> asking` cache for sub-ranges taken whole.
    exact: std::collections::HashMap<(usize, usize), u64, std::hash::BuildHasherDefault<crate::backend::AddressHash>>,
    /// Up to 8 largest ranges `(from, to, asking)` taken whole, for containment queries.
    largest: [(usize, usize, u64); 8],
}

impl CopiedState {
    fn new(pages: usize) -> Self {
        Self {
            pages: vec![0; pages],
            exact: std::collections::HashMap::default(),
            largest: [(0, 0, 0); 8],
        }
    }

    fn ranged(&self, from: usize, to: usize) -> u64 {
        let mut best = self.exact.get(&(from, to)).copied().unwrap_or(0);
        for &(a, b, t) in &self.largest {
            if t > best && a <= from && to <= b {
                best = t;
            }
        }
        best
    }

    fn record_range(&mut self, from: usize, to: usize, now: u64) {
        if self.exact.len() < 256 || self.exact.contains_key(&(from, to)) {
            self.exact.insert((from, to), now);
        }
        let len = to - from;
        let mut min_idx = 0;
        let mut min_len = usize::MAX;
        for (i, slot) in self.largest.iter_mut().enumerate() {
            if slot.0 == from && slot.1 == to {
                slot.2 = now;
                return;
            }
            let slen = slot.1 - slot.0;
            if slen < min_len {
                min_len = slen;
                min_idx = i;
            }
        }
        if len >= min_len {
            self.largest[min_idx] = (from, to, now);
        }
    }
}

/// A title buffer's host copy.
pub(crate) struct Shadow {
    /// Title memory's pages of 4 KiB (from the one `start` lies in on), a bit
    /// each, that what shaders wrote was put into since a refresh last looked
    /// at them ([`Shadow::take_landed`]); empty until then.
    landings: Mutex<Vec<u64>>,
    /// Whether any is.
    landing: std::sync::atomic::AtomicBool,
    /// The bytes of `buffer`, once asked.
    width: std::sync::OnceLock<u64>,
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// Per page of title memory from `start`'s on: the write-watch asking
    /// after which it was last copied (0: never), plus cached whole-taken
    /// sub-ranges (`exact` + `largest`) so partial boundary pages are not
    /// re-copied when nothing changed. `RwLock` allows concurrent
    /// `nothing_new` checks across worker threads.
    copied: RwLock<CopiedState>,
    /// Whether the memory is write-watched: unknown, yes, no.
    watched: AtomicU8,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<Vec<(usize, usize)>>,
    /// Fast lock-free mirror of `!self.pending.is_empty()`.
    has_pending: std::sync::atomic::AtomicBool,
    /// With [`relay`]: how many submissions not yet copied back declared
    /// each range of `pending` ([`Flights`]). Locked inside `pending`'s
    /// lock.
    flights: Mutex<Flights>,
    /// The copy's bytes before the submission ran, for the ranges its
    /// shaders write (`pending`), each at its offset in the copy: what the
    /// GPU wrote differs. Room for the whole copy; only those ranges' bytes
    /// are set (and read).
    before: Mutex<Option<Vec<u8>>>,
    /// Whether `before` stays attached to this shadow across submissions
    /// within [`PINNED_SNAP_BYTES`] to avoid pool churn and demand-zero faults.
    before_pinned: std::sync::atomic::AtomicBool,
    /// Held shared while pieces of the copy are landed (`crate::memory::writeback_pool`), each
    /// its own bytes of it on several threads, and alone while title bytes
    /// go in under shader writes ([`Shadow::merge_under_writes`]), which
    /// takes `before`'s lock too. Taken ahead of that one.
    merging: RwLock<()>,
    /// With snapshots kept ([`keeps`]): which 64 KiB pieces of the snapshot
    /// hold the copy's bytes as they are now, and whether the snapshot is
    /// counted in [`KEPT_BYTES`].
    kept: Mutex<(Vec<bool>, bool)>,
    /// When the copy was last asked for (`crate::backend::moment`): a window
    /// outside buffers nothing asked for in a while, and nothing holds, is
    /// let go ([`let_go_idle`]).
    used: AtomicU64,
    /// The lowest and highest title address asked for in the copy (a
    /// window is 4 MiB whatever was asked: measured for the memory line).
    asked: [AtomicU64; 2],
    /// Asks for the copy that gave no length (root views: they read from
    /// their address to the copy's end, whatever lies there).
    unsized_asks: AtomicU64,
    /// Whether shaders were ever declared to write it.
    written: std::sync::atomic::AtomicBool,
    /// A number no other copy has had. Its address is no name for a copy:
    /// one let go and another made may share it.
    id: u64,
    /// With [`cards`], for a big title buffer: `buffer` is in the card's
    /// own memory, and this is the rest of the copy.
    card: Option<Card>,
    /// A window of title memory outside buffers, not a title buffer's copy.
    window: bool,
    /// Of a copy with two sides: only the dwords the title changed go up,
    /// as with [`send_changed`] ([`by_address`]).
    changes_only: std::sync::atomic::AtomicBool,
    /// A window in card memory the CPU maps ([`window_mapped_for`]).
    mapped: bool,
    /// With `crate::memory::inplace`: not a copy at all. `buffer` is placed in a
    /// heap opened on the title memory itself, so shaders read and write
    /// the title's own bytes and nothing is copied either way. The heap,
    /// and the bytes from the buffer's first byte to `start` (the heap
    /// begins on a 64 KiB line; a title buffer need not).
    place: Option<(windows::Win32::Graphics::Direct3D12::ID3D12Heap, u64)>,
    /// Sub-allocated slot inside the shared 4 MiB 256-byte-aligned CBV slab
    /// pool (`find_aligned_cbv`), if this shadow was created for an unaligned
    /// constant buffer view.
    cbv_slab: Option<CbvSlabSlot>,
}

/// Slot metadata for a 256-byte-aligned CBV sub-allocation inside [`CBV_SLAB_POOL`].
struct CbvSlabSlot {
    slab_idx: usize,
    offset: u64,
    size: u64,
}

/// What a copy in the card's memory ([`cards`]) has beside the buffer
/// shaders see.
struct Card {
    /// The CPU's side, in the PC's memory: what `data` maps. Title memory
    /// is copied into it as into any copy, and what changed in it goes to
    /// the card's side before the next submission.
    staging: ID3D12Resource,
    /// The card side's bytes as they were when the submission's shaders
    /// started, in the card's memory too: made when shader writes are
    /// first declared for the copy, and compared with the card's side, on
    /// the card, after each submission that declares some.
    before: Mutex<Option<ID3D12Resource>>,
    /// Lock-free GPU virtual address of `before` once created (`0` until then).
    before_gpu: AtomicU64,
    /// Offsets of the copy whose bytes changed on the CPU's side and have
    /// not gone to the card's side yet.
    dirty: Mutex<Vec<(usize, usize)>>,
    /// With [`relay`]: the offsets that went from `dirty` to the card's
    /// side, each with the number of the send that took it
    /// ([`card_dirty`]), kept until a fetch sent after it has been merged.
    /// Locked inside `dirty`'s lock.
    sent: Mutex<SentRuns>,
    /// Whether something was put into `dirty` since it was last taken.
    touched: std::sync::atomic::AtomicBool,
    /// The buffers' bytes (the title buffer's, rounded up to 64 KiB).
    size: u64,
}

const NOT_WATCHED: u8 = 2;
/// Watched, not by the kernel but by the guards, when last asked about.
const GUARDED: u8 = 3;

/// The unit shader writes are looked for and put back in.
const PIECE: usize = 64 << 10;

/// The kernel's page.
const KERNEL_PAGE: usize = 4096;

/// Host copies alive (also those only a list, a descriptor pass or a
/// snapshot still holds) and the bytes of title memory they copy.
static LIVE: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Shadow {
    fn drop(&mut self) {
        if let Some(slot) = self.cbv_slab.take() {
            release_cbv_slab_slot(slot);
        }
        if self.place.is_some() {
            // Never counted with the copies (`Shadow::placed`).
            crate::memory::inplace::gone(self.window, self.len);
            return;
        }
        LIVE[0].fetch_sub(1, Ordering::Relaxed);
        LIVE[1].fetch_sub(self.len, Ordering::Relaxed);
        if self.kept.get_mut().unwrap_or_else(|e| e.into_inner()).1 {
            KEPT_BYTES.fetch_sub(self.len, Ordering::Relaxed);
        }
        if let Some(bytes) = self.before.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            if *self.before_pinned.get_mut() {
                PINNED_SNAP_BYTES.fetch_sub(bytes.capacity() as u64, Ordering::Relaxed);
            }
            give_back(bytes);
        }
        let window = self.window;
        if self.mapped {
            WINDOW_MAPPED_BYTES.fetch_sub(self.len, Ordering::Relaxed);
        }
        if let Some(card) = &mut self.card {
            if window {
                WINDOW_CARD_BYTES.fetch_sub(card.size, Ordering::Relaxed);
            } else {
                CARD_BYTES[0].fetch_sub(card.size, Ordering::Relaxed);
            }
            if card.before.get_mut().unwrap_or_else(|e| e.into_inner()).is_some() {
                CARD_BYTES[1].fetch_sub(card_hash_bytes(card.size), Ordering::Relaxed);
            }
        }
    }
}

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

#[cfg(test)]
mod tests;
