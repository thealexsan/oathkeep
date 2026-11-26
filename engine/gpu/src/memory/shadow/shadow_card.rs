use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use super::Shadow;
#[cfg(test)]
use super::{Card, PIECE};
use super::card_alloc::{CARD_BYTES, card_hash_bytes};
use super::card_runs::{CardSpan, card_hashed_of, card_piece_of, card_span_of};
use super::pool::{first_difference, last_difference};
use super::put::put_changed;
use super::relay::{card_merge_bytes, merge_piece, outside};

impl Shadow {
    /// Whether shaders read and write this copy in the card's own memory
    /// ([`cards`]).
    pub(crate) fn carded(&self) -> bool {
        self.card.is_some()
    }

    /// Shaders store in it what is not declared: from here on only the dwords
    /// the title changes go up ([`by_address`]).
    pub(crate) fn keep_stores(&self) {
        self.changes_only.store(true, Ordering::Relaxed);
    }

    /// Offsets `[from, to)` go up again whole, as the CPU's side has them.
    pub(crate) fn resend(&self, from: usize, to: usize) {
        self.card_changed(from, to);
    }

    /// The CPU side's bytes at offsets `[from, to)` changed: they go to the
    /// card's side before the next submission.
    pub(super) fn card_changed(&self, from: usize, to: usize) {
        self.card_changed_runs(std::iter::once((from, to)));
    }

    /// [`Shadow::card_changed`] of each of `runs`.
    pub(super) fn card_changed_runs(&self, runs: impl Iterator<Item = (usize, usize)>) {
        let Some(card) = &self.card else {
            return;
        };
        let mut runs = runs.filter(|r| r.0 < r.1).peekable();
        if runs.peek().is_none() {
            return;
        }
        let mut dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
        for (from, to) in runs {
            match dirty.last_mut() {
                Some(last) if from <= last.1 && to >= last.0 => *last = (last.0.min(from), last.1.max(to)),
                _ => dirty.push((from, to)),
            }
        }
        // (Under the lock: what takes the list sees it or finds it set.)
        card.touched.store(true, Ordering::Release);
    }

    /// Whether shaders were ever declared to write it.
    pub(crate) fn written(&self) -> bool {
        self.written.load(Ordering::Relaxed)
    }

    /// `bytes` were put into title memory at `address`, and the submission
    /// being prepared copies the same to the copy at that copy's place in its
    /// list: the reference (the CPU's side of a copy in the card's memory, the
    /// snapshot) takes them and nothing is sent. What the card makes of them
    /// is then what differs from them when the submission has run. The range
    /// is declared written by it, and its sends are taken.
    pub(crate) fn placed_copy(&self, address: usize, bytes: &[u8]) {
        let (from, to) = self.inside(address, bytes.len());
        if from >= to || self.place.is_some() {
            return;
        }
        let bytes = &bytes[self.start as usize + from - address..][..to - from];
        let _merging = self.merging.write().unwrap_or_else(|e| e.into_inner());
        if self.card.is_some() {
            // SAFETY: inside the mapping of the CPU's side, in a range no
            // refresh writes; the lock keeps the landing out.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.data.add(from), bytes.len()) };
        } else if let Some(snapshot) = self.before.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            // SAFETY: the snapshot's allocation holds the copy's length.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), snapshot.as_mut_ptr().add(from), bytes.len()) };
        }
        self.changed(from, to);
    }

    /// Of a copy in the card's memory: where its CPU side is.
    pub(crate) fn card_data(&self) -> *const u8 {
        self.data
    }

    /// Diagnostics: of a copy in the card's memory, what its CPU side holds
    /// at offsets `[at, at + len)` (cut to the copy).
    pub(crate) fn card_held(&self, at: usize, len: usize) -> Vec<u8> {
        let to = (at + len).min(self.len as usize);
        if self.card.is_none() || at >= to {
            return Vec::new();
        }
        // SAFETY: inside the mapping; the caller holds the submission lock,
        // under which it is written but for what is being landed.
        unsafe { std::slice::from_raw_parts(self.data.add(at).cast_const(), to - at) }.to_vec()
    }

    /// Diagnostics: the offsets whose bytes have not gone to the card's side
    /// yet.
    pub(crate) fn card_dirty_now(&self) -> Vec<(usize, usize)> {
        self.card.as_ref().map_or_else(Vec::new, |c| c.dirty.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Of a copy in the card's memory: the CPU's side (PC memory), the
    /// card side's bytes from before if made yet, and their size.
    pub(crate) fn card_sides(&self) -> Option<(&ID3D12Resource, Option<ID3D12Resource>, u64)> {
        let card = self.card.as_ref()?;
        let before = card.before.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Some((&card.staging, before, card.size))
    }

    /// Lock-free borrow of the CPU staging buffer for a copy in the card's memory.
    pub(crate) fn card_staging(&self) -> Option<&ID3D12Resource> {
        self.card.as_ref().map(|c| &c.staging)
    }

    /// Lock-free GPU virtual address of the card side's hash buffer (`before`),
    /// if created, avoiding Mutex locks and COM `AddRef`/`Release` clones.
    #[inline]
    pub(crate) fn card_before_gpu(&self) -> Option<u64> {
        let gpu = self.card.as_ref()?.before_gpu.load(Ordering::Acquire);
        (gpu != 0).then_some(gpu)
    }

    /// Shader writes are declared for a copy in the card's memory: makes
    /// its "before" twin with `make` (bytes to buffer) if it has none.
    /// Whether one was made now: the caller fills it, whole, from the
    /// card's side before the submission's lists run. Without one (no
    /// memory) every piece of a declared range is fetched.
    pub(crate) fn card_make_before(&self, make: impl FnOnce(u64) -> Option<ID3D12Resource>) -> bool {
        if self.card_before_gpu().is_some() {
            return false;
        }
        let Some(card) = &self.card else {
            return false;
        };
        let mut before = card.before.lock().unwrap_or_else(|e| e.into_inner());
        if before.is_some() {
            return false;
        }
        *before = make(card.size);
        if let Some(ref res) = *before {
            // SAFETY: live D3D12 buffer owned by `card.before`.
            let gpu = unsafe { res.GetGPUVirtualAddress() };
            card.before_gpu.store(gpu, Ordering::Release);
            CARD_BYTES[1].fetch_add(card_hash_bytes(card.size), Ordering::Relaxed);
        }
        before.is_some()
    }

    /// The declared range `[address, end)` as the compare on the card
    /// takes it; `None` if none of it lies in the copy.
    pub(crate) fn card_span(&self, address: usize, end: usize) -> Option<CardSpan> {
        let (from, to) = self.inside(address, end.saturating_sub(address));
        (from < to).then(|| card_span_of(self.start as usize, from, to))
    }

    /// The pieces that hold offsets `[from, to)` of the copy (cut to it),
    /// as the card's hashing takes them.
    pub(crate) fn card_run(&self, from: usize, to: usize) -> CardSpan {
        let to = to.min(self.len as usize);
        card_span_of(self.start as usize, from.min(to.saturating_sub(1)), to)
    }

    /// The offsets of piece `i` of `span` that lie in the span.
    pub(crate) fn card_piece(&self, span: &CardSpan, i: u32) -> (usize, usize) {
        card_piece_of(self.start as usize, self.len as usize, span, i)
    }

    /// The piece that holds offset `at`.
    pub(crate) fn card_piece_of(&self, at: usize) -> u32 {
        self.pieces(at, at + 1).start as u32
    }

    /// The offsets the compare on the card hashes of a piece: what lies of
    /// it in the card's buffer (which may be longer than the copy), widened
    /// to 16 bytes.
    pub(crate) fn card_hashed(&self, piece: u32) -> (usize, usize) {
        let size = self.card_hash_size() as usize;
        card_hashed_of(self.start as usize, size, piece as usize)
    }

    /// The bytes of the copy the card's hashes cover: the title buffer's own,
    /// rounded up to the hash's 16-byte step. The card's buffer is the title
    /// buffer rounded up to 64 KiB, of which a title buffer of 288 bytes can
    /// give nothing back but those (after fxceless c36971b2).
    /// `OATHKEEP_CARD_HASH_LEN=0`: the whole buffer.
    pub(crate) fn card_hash_size(&self) -> u64 {
        static OWN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let size = self.card.as_ref().map_or(self.len, |c| c.size);
        if *OWN.get_or_init(|| std::env::var_os("OATHKEEP_CARD_HASH_LEN").is_none_or(|v| v != "0")) {
            self.len.next_multiple_of(16).min(size)
        } else {
            size
        }
    }

    /// The walls trace (`crate::backend::walls_trace`): a piece of `fetched`
    /// bytes of this copy came back from the card and `written` of them
    /// went into title memory. Summed per buffer and logged once a frame:
    /// which shader-written buffers the title's CPU could see fresh in
    /// that frame, and how much of each changed.
    pub(crate) fn walls_merged(&self, fetched: usize, written: usize) {
        if !crate::backend::walls_trace() {
            return;
        }
        static SEEN: Mutex<(u64, BTreeMap<u64, (u64, u32, u64, u64)>)> = Mutex::new((0, BTreeMap::new()));
        let frame = crate::d3d12::frame::current();
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.0 != frame {
            let buffers: Vec<String> = seen
                .1
                .iter()
                .map(|(at, v)| format!("{at:#x}+{:#x}: {} pieces, {} fetched, {} written", v.0, v.1, v.2, v.3))
                .take(32)
                .collect();
            if !buffers.is_empty() {
                tracing::info!(
                    frame = seen.0,
                    count = seen.1.len(),
                    buffers = ?buffers,
                    "walls trace: shader-written card buffers merged into title memory in this frame"
                );
            }
            seen.1.clear();
            seen.0 = frame;
        }
        let entry = seen.1.entry(self.start).or_insert((self.len, 0, 0, 0));
        entry.1 += 1;
        entry.2 += fetched as u64;
        entry.3 += written as u64;
    }

    /// After a submission, for a copy in the card's memory: `gpu` is what
    /// the card's side holds at offset `at` on, in a piece the compare
    /// found differing. The bytes shaders wrote (those that differ from the
    /// CPU's side, which is what the card's side held when they started)
    /// go into title memory, from the first to the last of them, as
    /// [`Shadow::copy_back`] puts them; the CPU's side takes the result and
    /// sends it up before the next submission. `title` is scratch memory.
    /// Returns the bytes written to title memory.
    pub(crate) fn card_merge(&self, at: usize, gpu: &[u8], title: &mut Vec<u8>) -> usize {
        let start = self.start as usize;
        if self.card.is_none() || at + gpu.len() > self.len as usize {
            return 0;
        }
        // SAFETY: inside the mapping (checked above); only the submitting
        // thread writes it.
        let old = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), gpu.len()) };
        let Some(first) = first_difference(gpu, old) else {
            return 0;
        };
        let last = last_difference(gpu, old).unwrap_or(first);
        let (at, n) = (at + first, last + 1 - first);
        let (gpu, old) = (&gpu[first..=last], &mut old[first..=last]);
        title.clear();
        title.resize(n, 0);
        let mut written = 0;
        if crate::backend::read_into((start + at) as *const u8, title) {
            // The stretch as read: what the card did not change is the
            // title's, and may have been written again since.
            // As in `copy_back`: the card's byte where it is not what it
            // was before the submission, else the title's.
            let was = title.clone();
            if card_merge_bytes(gpu, old, title) {
                written = put_changed(start + at, &was, title);
            }
        } else {
            // Title memory is not mapped there: the copy keeps what the
            // card wrote, as before.
            old.copy_from_slice(gpu);
        }
        // The card's side and its "before" take the result.
        self.card_changed(at, at + n);
        written
    }

    /// [`relay`], for a copy in the card's memory: `gpu` is what the
    /// card's side held at offset `at` on when a fetch sent after send
    /// `fetched_after` (`crate::backend`'s order of the queue) ran, in a piece
    /// the compare found differing. The bytes that differ from the CPU's
    /// side are what shaders wrote; they go into title memory, from the
    /// first to the last of them, and into the CPU's side. Nothing is sent
    /// up afterwards (the card holds these bytes or newer ones: sending
    /// them up could undo a later flight's write), and the title's other
    /// bytes are left to the refresh.
    ///
    /// Left out: bytes of the CPU's side that hold title bytes the card
    /// had not received when the fetch ran, those still to go up and those
    /// that went up with a later send. There the card's side is the older
    /// one, and taking it for a shader's write would put it over what the
    /// title wrote. Under the CPU side's lock (the snapshot's, which such
    /// a copy has no other use for), as [`Shadow::merge_under_writes`].
    /// `title` is scratch memory. Returns the bytes written to title
    /// memory.
    pub(crate) fn card_merge_relay(&self, at: usize, gpu: &[u8], fetched_after: u64, title: &mut Vec<u8>) -> usize {
        let start = self.start as usize;
        let Some(card) = &self.card else {
            return 0;
        };
        if at + gpu.len() > self.len as usize {
            return 0;
        }
        let _side = self.before.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: inside the mapping (checked above). The offsets are in a
        // range declared and not done with, which no refresh writes; what
        // does write it (the title's bytes put in under shader writes)
        // holds the lock held here.
        let held = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), gpu.len()) };
        let Some(first) = first_difference(gpu, held) else {
            return 0;
        };
        let last = last_difference(gpu, held).unwrap_or(first);
        let (at, n) = (at + first, last + 1 - first);
        let (gpu, held) = (&gpu[first..=last], &mut held[first..=last]);
        let skip: Vec<(usize, usize)> = {
            let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            let sent = card.sent.lock().unwrap_or_else(|e| e.into_inner());
            let mut skip: Vec<(usize, usize)> = dirty.iter().copied().filter(|&(a, b)| a < at + n && b > at).collect();
            sent.near(fetched_after, at, at + n, &mut skip);
            skip
        };
        title.clear();
        title.resize(n, 0);
        let mapped = crate::backend::read_into((start + at) as *const u8, title);
        let was = title.clone();
        let mut changed = false;
        for (a, b) in outside(at, at + n, skip) {
            let (a, b) = (a - at, b - at);
            // Title memory that is not mapped: the CPU's side keeps what
            // the card wrote, as before; `title` is scratch then.
            changed |= merge_piece(&gpu[a..b], &mut held[a..b], &mut title[a..b]);
        }
        if !mapped || !changed {
            return 0;
        }
        put_changed(start + at, &was, title)
    }
}

/// For tests: copies with no host buffer behind them.
#[cfg(test)]
impl Shadow {
    /// A copy at `data` of title memory `[start, start + len)`, with a card
    /// side if `card`. It is never let go: there is no buffer to release.
    pub(crate) fn unbacked(start: u64, data: *mut u8, len: u64, card: bool) -> Arc<Shadow> {
        // SAFETY: an interface pointer nothing calls.
        let none = || unsafe { std::mem::transmute::<usize, ID3D12Resource>(8) };
        let mut shadow = Shadow::new(none(), data, 0, start, len);
        if card {
            shadow.card = Some(Card {
                staging: none(),
                before: Mutex::default(),
                before_gpu: AtomicU64::new(0),
                dirty: Mutex::default(),
                sent: Mutex::default(),
                touched: Default::default(),
                size: len.next_multiple_of(PIECE as u64),
            });
        }
        let shadow = Arc::new(shadow);
        std::mem::forget(shadow.clone());
        shadow
    }

    /// What shaders wrote goes into title memory ([`put_changed`]).
    pub(crate) fn put(at: usize, title: &[u8], merged: &[u8]) {
        put_changed(at, title, merged);
    }

    /// What is to go up to the card, noted as sent with `number`, as
    /// [`card_dirty`] does; notes of sends up to `merged` go.
    pub(crate) fn send_up(&self, number: u64, merged: u64) -> Vec<(usize, usize)> {
        let card = self.card.as_ref().unwrap();
        let mut dirty = card.dirty.lock().unwrap();
        let ranges = std::mem::take(&mut *dirty);
        let mut sent = card.sent.lock().unwrap();
        sent.forget(merged);
        sent.note(number, ranges.clone());
        ranges
    }

    /// What is to go up to the card, taken.
    pub(crate) fn take_dirty(&self) -> Vec<(usize, usize)> {
        std::mem::take(&mut *self.card.as_ref().unwrap().dirty.lock().unwrap())
    }
}
