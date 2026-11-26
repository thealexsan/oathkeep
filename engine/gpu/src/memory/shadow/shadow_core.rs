use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use crate::backend::HostList;
use crate::memory::watch::PAGE;

use super::loose::seed_mapped_bytes;
use super::{CbvSlabSlot, CopiedState, GUARDED, LIVE, NOT_WATCHED, Shadow};
use super::pool::{keeps, piece_at, pieces_of};

impl Shadow {
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// The title memory the copy is of: its start and length.
    pub(crate) fn span(&self) -> (u64, u64) {
        (self.start, self.len)
    }

    /// The offset of a title address in the copy, if it lies in it.
    pub(crate) fn offset_in(&self, address: u64) -> Option<u64> {
        address.checked_sub(self.start).filter(|&at| at < self.len)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// Whether the memory behind the copy turned out not to be write-watched:
    /// every refresh then copies its whole range.
    pub(crate) fn unwatched(&self) -> bool {
        self.watched.load(Ordering::Relaxed) == NOT_WATCHED
    }

    /// Whether a submission asks the kernel about the copy's ranges ahead:
    /// not of memory that is not watched, nor of view memory, which the
    /// kernel would refuse (and with it the ranges asked about together).
    pub(crate) fn asked_ahead(&self) -> bool {
        let watched = self.watched.load(Ordering::Relaxed);
        watched != NOT_WATCHED && (watched != GUARDED || !crate::memory::watch::light())
    }

    /// The copy's own number (see the field).
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Whether it is no copy but title memory in place (`crate::memory::inplace`).
    pub(crate) fn in_place(&self) -> bool {
        self.place.is_some()
    }

    /// Whether this copy is a window of title memory outside buffer objects.
    pub(crate) fn is_window(&self) -> bool {
        self.window
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start + self.place.as_ref().map_or(0, |place| place.1)
    }

    /// Title memory `[start, start + len)` in place: `placed` is a buffer
    /// over it in a heap opened on it. Shaped as a copy so that everything
    /// that binds, views or keeps a copy works on it unchanged; what
    /// copies does nothing ([`Shadow::update`], [`Shadow::gpu_writes`]).
    pub(super) fn placed(placed: crate::memory::inplace::Placed, start: u64, len: u64, window: bool) -> Shadow {
        crate::backend::name(&placed.buffer, || format!("title memory {start:#x}+{len:#x} in place"));
        let mut shadow = Shadow::new(placed.buffer, start as *mut u8, placed.gpu, start, len);
        // Not a copy: not counted with them, and nothing asks what the
        // title wrote there.
        LIVE[0].fetch_sub(1, Ordering::Relaxed);
        LIVE[1].fetch_sub(len, Ordering::Relaxed);
        *shadow.watched.get_mut() = NOT_WATCHED;
        shadow.window = window;
        shadow.place = Some((placed.heap, placed.bias));
        shadow
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    /// Whether `size` bytes at `address` can be bound directly as a D3D12
    /// constant buffer view on this shadow: `BufferLocation` must be 256-byte
    /// aligned and `constant_bytes(size)` must fit within this shadow's buffer.
    pub(crate) fn can_bind_cbv(&self, address: u64, size: u32) -> bool {
        let aligned = u64::from(size.min(64 << 10).next_multiple_of(256));
        if address < self.start || !self.gpu(address).is_multiple_of(256) {
            return false;
        }
        if let Some(slot) = &self.cbv_slab {
            return (address - self.start)
                .checked_add(aligned)
                .is_some_and(|end| end <= slot.size);
        }
        self.offset(address)
            .checked_add(aligned)
            .is_some_and(|end| end <= self.width())
    }

    /// The bytes of the host buffer.
    pub(crate) fn width(&self) -> u64 {
        // SAFETY: a live buffer.
        *self.width.get_or_init(|| unsafe { self.buffer.GetDesc() }.Width)
    }

    pub(super) fn new_suballocated(
        buffer: ID3D12Resource,
        aligned_size: u64,
        data: *mut u8,
        gpu: u64,
        start: u64,
        len: u64,
        cbv_slab: Option<CbvSlabSlot>,
    ) -> Shadow {
        let mut shadow = Shadow::new(buffer, data, gpu, start, len);
        let _ = shadow.width.set(aligned_size);
        shadow.cbv_slab = cbv_slab;
        seed_mapped_bytes(data, aligned_size, start, len);
        shadow
    }

    pub(super) fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        LIVE[0].fetch_add(1, Ordering::Relaxed);
        LIVE[1].fetch_add(len, Ordering::Relaxed);
        Shadow {
            landings: Mutex::default(),
            landing: Default::default(),
            width: Default::default(),
            buffer,
            data,
            gpu,
            start,
            len,
            copied: RwLock::new(CopiedState::new(pages as usize)),
            watched: AtomicU8::new(0),
            pending: Mutex::default(),
            has_pending: std::sync::atomic::AtomicBool::new(false),
            flights: Mutex::default(),
            before: Mutex::default(),
            before_pinned: std::sync::atomic::AtomicBool::new(false),
            merging: RwLock::default(),
            kept: Mutex::default(),
            used: AtomicU64::new(crate::backend::moment()),
            asked: [AtomicU64::new(u64::MAX), AtomicU64::new(0)],
            unsized_asks: AtomicU64::new(0),
            written: Default::default(),
            id: {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                NEXT.fetch_add(1, Ordering::Relaxed)
            },
            card: None,
            window: false,
            changes_only: Default::default(),
            mapped: false,
            place: None,
            cbv_slab: None,
        }
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    pub(super) fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        // In place the shaders write title memory itself: no snapshot, no
        // compare, nothing to copy back, and no wait on its account.
        // shortcut: another host copy of the same bytes (a title buffer
        // inside this one with a copy of its own, a texture in it) is not
        // told: the card's writes are not seen by write watching.
        if self.place.is_some() {
            return;
        }
        self.written.store(true, Ordering::Relaxed);
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// The 64 KiB pieces (cut at multiples of 64 KiB of title memory, as
    /// [`Shadow::differing`] cuts them) that hold offsets `[from, to)` of
    /// the copy; `from < to`.
    pub(super) fn pieces(&self, from: usize, to: usize) -> std::ops::Range<usize> {
        pieces_of(self.start as usize, from, to)
    }

    /// The offsets in the copy that piece `piece` holds.
    pub(super) fn piece(&self, piece: usize) -> (usize, usize) {
        piece_at(self.start as usize, self.len as usize, piece)
    }

    /// The copy's bytes at offsets `[from, to)` change: a kept snapshot is
    /// not current there any more.
    pub(super) fn changed(&self, from: usize, to: usize) {
        if !keeps() || from >= to {
            return;
        }
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if kept.0.is_empty() {
            return;
        }
        for piece in self.pieces(from, to) {
            kept.0[piece] = false;
        }
    }
}
