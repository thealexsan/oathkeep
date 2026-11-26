//! Incremental per-heap buffer slot range tracking for bindless buffer sweeps.

/// Passes after which every buffer slot of the heap was looked at again.
pub(super) const HEAP_ROUND: u64 = 16;

/// Whether the slots looked at again for no write are a part of them at
/// every pass, each slot at every sixteenth (`OATHKEEP_HEAP_SPREAD=0`: all of
/// them at every sixteenth pass, as before).
///
/// All at once was some 115 ms on a thread that records the frame, every
/// sixteenth frame (72,000 slots; run record-1: 11 ms a pass in the lines
/// without such a pass, 22 in those of ten frames with one). A slot is
/// looked at as often as before.
pub(super) fn heap_spread() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_HEAP_SPREAD").is_none_or(|v| v != "0"))
}

pub(super) const SLOT_FLAG_UAV: u8 = 1 << 0;
pub(super) const SLOT_FLAG_OBJECT: u8 = 1 << 1;
pub(super) const SLOT_FLAG_IN_PLACE: u8 = 1 << 2;

/// Compact per-slot cached metadata in [`HeapRanges`] (holds no `Arc` of its
/// own; the owning [`CopyEntry`] holds one shared `T` across all slots on it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SlotView {
    pub(super) copy: usize,
    pub(super) from: u64,
    pub(super) to: u64,
    pub(super) words: [u32; 3],
    pub(super) pos: u32,
    pub(super) forgot: u32,
    pub(super) flags: u8,
}

impl SlotView {
    #[inline]
    pub(super) fn is_uav(self) -> bool {
        self.flags & SLOT_FLAG_UAV != 0
    }

    #[inline]
    pub(super) fn has_object(self) -> bool {
        self.flags & SLOT_FLAG_OBJECT != 0
    }

    #[inline]
    pub(super) fn in_place(self) -> bool {
        self.flags & SLOT_FLAG_IN_PLACE != 0
    }
}

/// Active slots referencing a single host copy `T`, with O(1) `swap_remove`
/// and cached `[min_from, max_to)` union span.
struct CopyEntry<T> {
    copy: T,
    views: Vec<(u32, u64, u64)>,
    span: Option<(u64, u64)>,
}

impl<T> CopyEntry<T> {
    fn span(&mut self) -> (u64, u64) {
        if let Some(span) = self.span {
            return span;
        }
        let mut lo = u64::MAX;
        let mut hi = 0u64;
        for &(_, from, to) in &self.views {
            lo = lo.min(from);
            hi = hi.max(to);
        }
        let computed = if lo <= hi { (lo, hi) } else { (0, 0) };
        self.span = Some(computed);
        computed
    }
}

/// The host copy and range the buffer view of each of a heap's slots reads
/// ([`super::State::heap_buffers`]), indexed by slot and grouped by copy so unchanged
/// slots skip re-mirroring/re-decoding and per-frame span union runs in
/// `O(#copies)` instead of scanning all `~80,000` slots.
pub(super) struct HeapRanges<T> {
    slots: Vec<Option<SlotView>>,
    copies: std::collections::HashMap<
        usize,
        CopyEntry<T>,
        std::hash::BuildHasherDefault<crate::backend::AddressHash>,
    >,
    active_views: u32,
}

impl<T> Default for HeapRanges<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            copies: Default::default(),
            active_views: 0,
        }
    }
}

/// Descriptor-derived references belong to the heap that produced them.
#[derive(Default)]
pub(crate) struct BufferSweep {
    pub(super) passes: u64,
    pub(super) ranges: HeapRanges<std::sync::Arc<crate::memory::shadow::Shadow>>,
    // Last refresh frame and covered interval for throttled, unwatched copies.
    pub(super) unwatched: std::collections::HashMap<u64, (u64, u64, u64)>,
}

impl<T> HeapRanges<T> {
    pub(super) fn clear(&mut self) {
        self.slots.clear();
        self.copies.clear();
        self.active_views = 0;
    }

    #[inline]
    pub(super) fn get(&self, slot: u32) -> Option<SlotView> {
        self.slots.get(slot as usize).copied().flatten()
    }

    #[inline]
    pub(super) fn copy_of(&self, copy_key: usize) -> Option<&T> {
        self.copies.get(&copy_key).map(|e| &e.copy)
    }

    #[inline]
    pub(super) fn touch_forgot(&mut self, slot: u32, forgot: u32) {
        if let Some(Some(entry)) = self.slots.get_mut(slot as usize) {
            entry.forgot = forgot;
        }
    }

    #[cfg(test)]
    pub(super) fn set(&mut self, slot: u32, next: Option<(usize, T, u64, u64, [u32; 3], u8)>) {
        self.set_with_forgot(slot, next, 0);
    }

    pub(super) fn set_with_forgot(
        &mut self,
        slot: u32,
        next: Option<(usize, T, u64, u64, [u32; 3], u8)>,
        forgot: u32,
    ) {
        let idx = slot as usize;
        let Some((copy_key, copy, from, to, words, flags)) = next else {
            self.clear_slot(slot);
            return;
        };
        if let Some(Some(old)) = self.slots.get(idx).copied() {
            if old.copy == copy_key && old.from == from && old.to == to {
                self.slots[idx] = Some(SlotView {
                    copy: copy_key,
                    from,
                    to,
                    words,
                    pos: old.pos,
                    forgot,
                    flags,
                });
                return;
            }
            self.clear_slot(slot);
        }
        if idx >= self.slots.len() {
            self.slots.resize(idx + 1, None);
        }
        let entry = self.copies.entry(copy_key).or_insert_with(|| CopyEntry {
            copy,
            views: Vec::new(),
            span: Some((from, to)),
        });
        let pos = entry.views.len() as u32;
        entry.views.push((slot, from, to));
        if let Some((lo, hi)) = &mut entry.span {
            *lo = (*lo).min(from);
            *hi = (*hi).max(to);
        }
        self.slots[idx] = Some(SlotView {
            copy: copy_key,
            from,
            to,
            words,
            pos,
            forgot,
            flags,
        });
        self.active_views += 1;
    }

    pub(super) fn clear_slot(&mut self, slot: u32) {
        let Some(old) = self.slots.get_mut(slot as usize).and_then(Option::take) else {
            return;
        };
        self.active_views = self.active_views.saturating_sub(1);
        let mut empty = false;
        if let Some(entry) = self.copies.get_mut(&old.copy) {
            let pos = old.pos as usize;
            if pos < entry.views.len() {
                entry.views.swap_remove(pos);
                if let Some(&(moved_slot, _, _)) = entry.views.get(pos)
                    && let Some(Some(moved)) = self.slots.get_mut(moved_slot as usize)
                {
                    moved.pos = old.pos;
                }
            }
            if entry.views.is_empty() {
                empty = true;
            } else if let Some((lo, hi)) = entry.span
                && (old.from == lo || old.to == hi)
            {
                entry.span = None;
            }
        }
        if empty {
            self.copies.remove(&old.copy);
        }
    }

    /// Drops cached slots in `part` (modulo `parts`) that are no longer in the
    /// sorted `live_part` list from `HostHeap::buffer_slots_of(part, parts)`.
    pub(super) fn prune_missing_in_part(&mut self, part: u32, parts: u32, live_part: &[u32]) {
        let step = parts as usize;
        let mut slot = part as usize;
        let mut live_idx = 0usize;
        while slot < self.slots.len() {
            let s32 = slot as u32;
            while live_idx < live_part.len() && live_part[live_idx] < s32 {
                live_idx += 1;
            }
            if self.slots[slot].is_some()
                && live_part.get(live_idx).copied() != Some(s32)
            {
                self.clear_slot(s32);
            }
            slot += step;
        }
    }

    #[inline]
    pub(super) fn views_count(&self) -> u32 {
        self.active_views
    }

    #[inline]
    pub(super) fn copies_len(&self) -> usize {
        self.copies.len()
    }

    pub(super) fn copy_spans(&mut self) -> impl Iterator<Item = (&T, u64, u64)> {
        self.copies.values_mut().map(|entry| {
            let (lo, hi) = entry.span();
            (&entry.copy, lo, hi)
        })
    }
}
