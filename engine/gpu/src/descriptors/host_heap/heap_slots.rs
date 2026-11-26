use windows::Win32::Graphics::Direct3D12::*;
use super::{HostHeap, View, host, view_at};
use super::diag::{OBJECT_TEXTURE_SLOTS, PLACED_TEXTURE_SLOTS, is_texture_view, twinned, view_stride_and_kind, view_stride_word};
#[cfg(feature = "diagnostics")]
use super::flips::note_before;
use super::kin::placed_texture;
use super::listed::{LIST_VIEWS, ListView, ListViews, Listed};
use super::make::{make, make_twinned};
use super::null_views::{null_buffer_view, null_for, null_view};
use super::queue::{TABLED_FRAMES, TABLED_GRACE, TABLED_READ, TABLED_WRITES, tabled_out};
use super::spans::{SPANNED, STALE, stale_rule};
use super::staged::{Kept, staged};

impl HostHeap {
    /// Title slot `index` is a table's ([`HostHeap::tabled`]). `read`: a

    /// Title slot `index` is a table's ([`HostHeap::tabled`]). `read`: a
    /// draw read it through the table and brought what it views up to
    /// date, so the slot leaves the queue of textures picked by index; the
    /// caller makes the host slot in place ([`HostHeap::mirror`]), as the
    /// pass would have, for a shader that picks it by index all the same.
    pub(crate) fn texture_slot_tabled(&self, index: u32, read: bool) {
        self.tabled_in(index, read, crate::d3d12::frame::current());
    }

    /// [`HostHeap::texture_slot_tabled`] in frame `frame`.
    pub(super) fn tabled_in(&self, index: u32, read: bool, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(stamp) = self.tabled.get(index as usize) else {
            return;
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        let old = stamp.load(Relaxed);
        if !read {
            if old & !TABLED_READ != now {
                stamp.store(now, Relaxed);
            }
            return;
        }
        // Once per slot and frame: tables are read by every draw.
        if old != now | TABLED_READ {
            stamp.store(now | TABLED_READ, Relaxed);
            if tabled_out() {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.remove(index) {
                    TABLED_WRITES[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// Whether title slot `index` was a table's within the last
    /// [`TABLED_FRAMES`] frames before `frame` (never, if such slots are
    /// to be queued as any other).
    pub(super) fn tabled_lately(&self, index: u32, frame: u64) -> bool {
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        tabled_out() && stamp != 0 && u64::from(now.wrapping_sub(stamp)) <= TABLED_FRAMES
    }

    /// What `tabled` holds for title slot `index`; 0: never a table's.
    pub(super) fn tabled_stamp(&self, index: u32) -> u32 {
        self.tabled
            .get(index as usize)
            .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The queue's part of a write of title slot `index` in `frame`:
    /// `texture`, whether it now holds a view of a texture the title may
    /// fill; `seen` as for [`HostHeap::note_buffer`].
    pub(super) fn queue_texture(&self, index: u32, texture: bool, seen: bool, frame: u64) {
        let stamp = if tabled_out() { self.tabled_stamp(index) } else { 0 };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        // A table's slot lately: the draw that reads it uploads its
        // texture, so its write waits for that draw.
        let lately = stamp != 0 && u64::from(now.wrapping_sub(stamp & !TABLED_READ)) <= TABLED_FRAMES;
        // Made now by a draw of this frame that read it through a table:
        // up to date.
        let read_now = seen && stamp == now | TABLED_READ;
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if texture && !read_now {
            let due = if lately && !seen {
                TABLED_WRITES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame + TABLED_GRACE
            } else {
                0
            };
            textures.written(index, frame, due, seen);
        } else {
            textures.remove(index);
        }
    }

    /// Title slot `index` lies in a table a draw reads or had made afresh:
    /// what it holds is the draw's to make ([`HostHeap::copied_tabled`]).
    pub(crate) fn slot_tabled(&self, index: u32) {
        self.tabled_in(index, false, crate::d3d12::frame::current());
    }

    /// The title slots that hold buffer views.
    pub(crate) fn buffer_slots(&self) -> Vec<u32> {
        self.buffers.all(|_| true)
    }

    /// Of [`HostHeap::buffer_slots`], those of one of `parts` parts: every
    /// slot is in the same part each time.
    pub(crate) fn buffer_slots_of(&self, part: u32, parts: u32) -> Vec<u32> {
        self.buffers.all_mod(part, parts)
    }

    /// The title slots whose buffer view was written, or went, since the
    /// last call.
    pub(crate) fn buffer_changes(&self) -> Vec<u32> {
        self.changed.take()
    }

    pub(super) fn note_buffer(&self, index: u32, view: Option<&View>, seen: bool) -> Vec<(u32, u64)> {
        self.note_buffer_inner(index, view, seen, true)
    }

    /// Records what title slot `index` now holds for shaders that pick
    /// buffers from the heap: its element stride in the table, and whether
    /// it is a buffer view at all.
    /// `seen`: the slot was not written now, a draw or a pass made its
    /// host slot from what it held.
    /// `mark_changed`: whether to mark `index` in `self.changed` for the
    /// next [`heap_buffer_pass`] (false when `heap_buffer_pass` itself calls).
    /// Returns the title slots whose placed texture view the write made
    /// stale ([`Spans`]), each with the record its host slot is made from:
    /// out of the queue of textures already, their host slots are the
    /// caller's to make null when it has let go of its own slot's lock
    /// ([`HostHeap::unmake_stale`]).
    pub(super) fn note_buffer_inner(&self, index: u32, view: Option<&View>, seen: bool, mark_changed: bool) -> Vec<(u32, u64)> {
        if self.sampler {
            return Vec::new();
        }
        // Measured for the texture side of the same gap (textures a shader
        // picks by a computed index are neither made, if placed, nor
        // uploaded): slots written with a placed texture view, and with a
        // view of a texture object.
        let texture = match view {
            Some(View::Placed(false, resource, _)) if resource[0] != 1 => {
                PLACED_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Some(View::Shader(r, _)) if crate::d3d12::resource::buffer_bytes(*r as crate::d3d12::object::This).is_none() => {
                OBJECT_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        };
        // The memory a placed texture view describes. Not for a slot
        // tables read (thousands of writes a frame, each read by its
        // draw): no pass looks at it, and the same texture's view in a
        // slot shaders pick by index says what it says.
        let placed_view = match view {
            Some(View::Placed(false, resource, desc))
                if resource[0] != 1 && !self.tabled_lately(index, crate::d3d12::frame::current()) =>
            {
                // (And the level the view starts at: word 4 of a texture
                // view's description, after format, dimension, mapping
                // and a pad.)
                let at = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let held = crate::d3d12::resource::placed_bytes_from(resource, crate::d3d12::resource::first_mip(desc));
                Some((resource, at, desc[4], held.map_or(u64::MAX, |held| at.saturating_add(held))))
            }
            _ => None,
        };
        let was_buffer = self.buffers.contains(index);
        let stale = if !texture && placed_view.is_none() && was_buffer {
            // Already known to be a buffer: cannot be in `self.textures` or
            // `self.placed`, so skip both mutexes.
            Vec::new()
        } else {
            self.queue_texture(index, texture, seen, crate::d3d12::frame::current());
            let stale = {
                use std::sync::atomic::Ordering::Relaxed;
                let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
                let span = placed_view.and_then(|(resource, start, first, held)| {
                    Some((start, start.saturating_add(placed.bytes(resource)?), first, held))
                });
                match span {
                    Some((start, end, first, held)) => {
                        SPANNED[0].fetch_min(start, Relaxed);
                        SPANNED[1].fetch_max(end, Relaxed);
                        placed.rule = stale_rule();
                        let stale = placed.note(index, start, end, !seen);
                        placed.shows(index, first, held);
                        // (Under the lock of `placed`: a later write of one of
                        // them comes by here before it makes the slot.)
                        stale.into_iter().map(|slot| (slot, self.made.get(slot).unwrap_or(0))).collect()
                    }
                    None => {
                        placed.forget(index);
                        Vec::new()
                    }
                }
            };
            if !stale.is_empty() {
                STALE[0].fetch_add(stale.len() as u64, std::sync::atomic::Ordering::Relaxed);
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                for &(slot, _) in &stale {
                    textures.remove(slot);
                }
            }
            stale
        };
        let (stride, kind) = view.map_or((None, 0), view_stride_and_kind);
        self.write_stride(index, stride.unwrap_or(0) | kind);
        let was = if stride.is_some() {
            if !was_buffer {
                self.buffers.put(index);
            }
            true
        } else {
            self.buffers.remove(index)
        };
        if was && (mark_changed || stride.is_none()) {
            self.changed.put(index);
        }
        stale
    }

    /// Writes `word` (stride | format kind) for view-heap slot `index` into
    /// the host's [`oathkeep_shader::emit::SLOT_STRIDES`] table.
    #[inline]
    pub(super) fn write_stride(&self, index: u32, word: u32) {
        if let Some((_, _, data)) = &self.strides
            && index < oathkeep_shader::emit::SLOT_STRIDES_COUNT
        {
            // SAFETY: inside the mapping; shaders read it while we write (a
            // single aligned dword: either value is one the slot had, and
            // dynamic slots are exclusively owned until submission finishes).
            unsafe { (*data as *mut u32).add(index as usize).write_volatile(word) };
        }
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            let given = slots.iter().filter(|taken| **taken).count() as u32;
            self.free.fetch_add(given, std::sync::atomic::Ordering::Relaxed);
            self.too_long.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
            slots.fill(false);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let kept = make_twinned(view, self.at(index), self.twin(index));
        if kept.is_none() {
            null_for(view, self.at(index));
        }
        self.put(index);
        self.twin_made(index, kept.is_some() && twinned(view));
        self.write_stride(index, if kept.is_some() { view_stride_word(view) } else { 0 });
        self.keep(index, kept.flatten());
    }

    /// Makes slot `index` (a dynamic one) the view the record `words` names,
    /// from this thread's descriptor of it if the recording `recording` has
    /// made one, else as [`HostHeap::make_at`] does, and keeps that
    /// ([`ListViews`]). What is known of the record in this recording;
    /// `None` (the slot is not made): not a view kept so.
    pub(crate) fn make_listed(&self, recording: u64, words: [u32; 3], index: u32) -> Option<Listed> {
        if self.sampler {
            return None;
        }
        LIST_VIEWS.with(|views| {
            let mut views = views.borrow_mut();
            if views.is_none() {
                *views = Some(ListViews::new()?);
            }
            let views = views.as_mut()?;
            if views.recording != recording {
                views.recording = recording;
                views.made.clear();
            }
            let twins = self.raw != 0 && self.twins.get(index as usize).is_some();
            if !views.made.contains_key(&words) {
                let view = super::view_at(words[1], words[2])?;
                let at = 2 * views.made.len() as u32;
                // (A constant buffer's bytes are copied for each draw.)
                if at >= ListViews::SLOTS || matches!(view, View::Constant(..)) {
                    return None;
                }
                let twin = twinned(&view) && twins;
                // Only construct a raw twin when this heap actually has a raw-twin
                // half (`self.raw != 0`); `ListViews` lives in a CPU-only heap so
                // `move_raw` is safe there on adapters that support it.
                let twin_slot = twin.then(|| (views.slot(at + 1), true));
                let kept = make_twinned(&view, views.slot(at), twin_slot)?;
                let stride_word = view_stride_word(&view);
                let listed = Listed {
                    kind: crate::execution::draw::view_kind(&view),
                    texture: is_texture_view(&view),
                    ready: (0, 0, false),
                    tabled: None,
                };
                views.made.insert(words, ListView { view, at, twin, stride_word, kept, listed });
            }
            let made = &views.made[&words];
            self.copy_from(index, views.slot(made.at));
            if made.twin && twins {
                self.copy_from(self.raw + index, views.slot(made.at + 1));
                self.twins[index as usize].store(true, std::sync::atomic::Ordering::Relaxed);
            } else {
                self.clear_twin(index);
            }
            self.write_stride(index, made.stride_word);
            self.keep(index, made.kept.clone());
            Some(made.listed)
        })
    }

    /// As [`HostHeap::copy_to`], from the slot `from`, which the card does not
    /// see.
    pub(super) fn copy_from(&self, to: u32, from: D3D12_CPU_DESCRIPTOR_HANDLE) {
        #[cfg(feature = "diagnostics")]
        if let Some(written) = self.written.get(to as usize) {
            written.store(crate::backend::coarse_ms() as u32, std::sync::atomic::Ordering::Relaxed);
        }
        let Some(host) = host() else { return };
        // SAFETY: a slot of our heap, and one of this thread's heap.
        unsafe { host.device.CopyDescriptorsSimple(1, self.cpu(to), from, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) };
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.at(index));
            self.put(index);
            self.write_stride(index, 0);
        }
        self.clear_twin(index);
        self.keep(index, None);
    }

    /// Where the raw twin of slot `index` is made, and whether that handle
    /// lies in a CPU-only staging heap (so `move_raw` may patch it before copy).
    pub(super) fn twin(&self, index: u32) -> Option<(D3D12_CPU_DESCRIPTOR_HANDLE, bool)> {
        (index < self.twins.len() as u32).then(|| match staged(self.sampler, 1) {
            Some(h) => (h, true),
            None => (self.cpu(self.raw + index), false),
        })
    }

    /// The raw twin of slot `index` was made by this thread ([`make_twinned`]),
    /// or is a null view if it was not.
    pub(super) fn twin_made(&self, index: u32, made: bool) {
        if !made {
            return self.clear_twin(index);
        }
        if let Some(shown) = self.twins.get(index as usize) {
            self.put_twin(index);
            shown.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The raw twin of slot `index` is a null view again.
    pub(super) fn clear_twin(&self, index: u32) {
        if let Some(made) = self.twins.get(index as usize)
            && made.swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            null_buffer_view(self.at(self.raw + index));
            self.put(self.raw + index);
        }
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    pub(super) fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        let Some(slot) = self.kept.get(index as usize) else {
            return;
        };
        let old = {
            use windows::core::Interface;
            let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            // The same again: nothing to retire.
            if slot.as_ref().map(Interface::as_raw) == resource.as_ref().map(Interface::as_raw) {
                return;
            }
            std::mem::replace(&mut *slot, resource)
        };
        // A running submission may still read the old resource through
        // this slot.
        if let Some(old) = old {
            crate::backend::retire_resource(old);
        }
    }

    /// What slot `index` views.
    pub(super) fn kept_in(&self, index: u32) -> Option<ID3D12Resource> {
        self.kept.get(index as usize)?.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

