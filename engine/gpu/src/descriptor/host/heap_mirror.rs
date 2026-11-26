use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use super::{HostHeap, VIEW_TAG, View, view_at};
#[cfg(feature = "diagnostics")]
use super::flips::{BEFORE, note_before};
use super::flips::{FLIPPED, LATE, PendingListFlip, neutral_for, remove_pending_list_flip, show_after, take_pending_list_flip};
use super::diag::{is_texture_view, twinned};
use super::staged::buffer_range;
#[cfg(feature = "diagnostics")]
use super::kin::same_bytes;
use super::kin::{Kin, Copied, KEPT_VIEWS, keep_shown, keep_views, kin, quick_mirror, rewritten, same_placed_texture};
use super::make::{make, make_twinned_with_copy};
use super::null_views::null_for;
use super::queue::{COPIED_FRAMES, COPIED_GRACE, TABLED_READ, tabled_out};

impl HostHeap {

    /// threads that compare what they read of its changed pages beside one
    /// another ([`Copied::changes`]; `super::sync_heaps`: the title copies
    /// records into its heap as bytes).
    ///
    /// The page says only that something on it was written, and `mirror`
    /// by itself would take every buffer or placed-texture record on it for
    /// newly written each time (it leaves such a slot unmade until a draw
    /// or a pass uses it): the texture queue's entry for the slot would
    /// start again, and the buffer pass would look at 2,048 slots for one
    /// that changed.
    pub(crate) fn copied_begin(&self, step: usize) -> Copied<'_> {
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        if copied.is_empty() {
            // Nothing there: a slot never looked at that holds nothing is
            // as it was.
            copied.resize(self.count as usize * step, 0);
        }
        let (at, len) = (copied.as_mut_ptr(), copied.len());
        Copied { at, len, step, _held: copied }
    }

    /// Whether new records of draws' tables are left to the draws
    /// (`OATHKEEP_HEAP_SYNC=all`: every new record is mirrored at once, as
    /// it first was).
    ///
    /// The title builds each draw's tables in its heap by copying records
    /// there as bytes: 60,000 to 210,000 a frame in its world (session
    /// play-1125-0127), 0.9 microseconds each on the submitting thread and
    /// 1.4 more in the buffer pass they were all handed to, 120 to 600 ms
    /// of every frame, and the draws make what they read of them anyway.
    pub(crate) fn copied_waits(&self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("OATHKEEP_HEAP_SYNC").is_none_or(|v| v != "all"))
    }

    /// Whether a draw read title slot `index` through a table within the
    /// last few frames before `frame` ([`HostHeap::tabled`]): what it holds
    /// is such a draw's to make.
    pub(crate) fn copied_tabled(&self, index: u32, frame: u64) -> bool {
        if !self.copied_waits() {
            return false;
        }
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        stamp != 0 && now.wrapping_sub(stamp) & !TABLED_READ <= COPIED_FRAMES
    }

    /// New records the title copied into slots `slots` in `frame` wait for
    /// the draw that may read them through a table.
    pub(crate) fn copied_wait(&self, frame: u64, slots: Vec<u32>) {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).push_back((frame, slots));
    }

    /// The slots that have waited long enough at `frame`
    /// ([`HostHeap::copied_wait`]), each once.
    pub(crate) fn copied_before(&self, frame: u64) -> Vec<u32> {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        while waiting.front().is_some_and(|(written, _)| written + COPIED_GRACE <= frame) {
            out.extend(waiting.pop_front().into_iter().flat_map(|(_, slots)| slots));
        }
        drop(waiting);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        self.mirror_inner(index, handle, for_draw, true).map(|(v, _)| v)
    }

    /// Like [`HostHeap::mirror`] with `for_draw = true` for `heap_buffer_pass`,
    /// returning any buffer copy already resolved during descriptor creation
    /// so the caller avoids a duplicate `shadow::find` lookup, and skipping
    /// re-marking `self.changed` for the slot being processed now.
    pub(crate) fn mirror_for_pass(
        &self,
        index: u32,
        handle: u64,
    ) -> Option<(View, Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)>)> {
        self.mirror_inner(index, handle, true, false)
    }

    fn mirror_inner(
        &self,
        index: u32,
        handle: u64,
        for_draw: bool,
        mark_changed: bool,
    ) -> Option<(View, Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)>)> {
        if index >= self.count {
            use std::sync::atomic::Ordering::Relaxed;
            let limit = self.count.saturating_add(self.dynamic);
            self.capped_mirror[usize::from(index >= limit)].fetch_add(1, Relaxed);
            if for_draw {
                self.capped_mirror[2].fetch_add(1, Relaxed);
            }
            self.highest_capped.fetch_max(index, Relaxed);
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    title_count = self.title_count,
                    dynamic_end = limit,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        if index > self.highest_direct.load(std::sync::atomic::Ordering::Relaxed) {
            self.highest_direct.fetch_max(index, std::sync::atomic::Ordering::Relaxed);
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        // Made from this record, and nothing has changed it: most of the
        // asks, from five recording threads, behind one lock while another
        // held it to make a descriptor (15 % of a recording thread's time in
        // the opening, profile cpu-1).
        if tag == VIEW_TAG
            && quick_mirror()
            && self.quick.get(index as usize).is_some_and(|k| k.load(std::sync::atomic::Ordering::Acquire) == key)
            && let Some(view) = view_at(i, generation)
        {
            return Some((view, None));
        }
        let held = self.made.lock(index);
        let shown = self.made.get(index).unwrap_or(0);
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            if shown != 0 {
                self.quick_is(index, 0);
                self.made.set(index, 0);
                self.make_null(index);
                let _ = self.note_buffer_inner(index, None, false, mark_changed);
            } else if for_draw {
                // Left for later and stale since: out of the sets and the stride table.
                let _ = self.note_buffer_inner(index, None, false, mark_changed);
            }
            return None;
        };
        let mut made_copy = None;
        if shown != key {
            // Views of other textures in the memory this one describes
            // are stale: their host slots read nothing from here on, and
            // let go of the host copies they held.
            // (When this slot's lock is let go: a lock at a time.)
            let stale = self.note_buffer_inner(index, Some(&view), for_draw, mark_changed);
            // A view of a texture object that has no host copy yet waits
            // for it, as buffer views wait for a draw: the slot is made
            // when the texture is first used (`super::make_waiting`), or
            // here when a draw reads the slot through a table.
            let waiting = if for_draw { None } else { view.waiting_texture() };
            if let Some(texture) = waiting {
                crate::descriptor::wait_for(texture, handle);
            }
            let later = !for_draw && (view.is_buffer() || waiting.is_some());
            // Another view of the placed texture the slot shows already (a
            // title that streams a texture in writes its view again with
            // more mips each time some arrive): the slot goes on showing
            // the old view until a draw or the texture pass makes the new
            // one and uploads what it shows. Made null here, the texture
            // read as nothing for as long as it waited in the queue, every
            // time it got sharper.
            if later
                && keep_views()
                && let Some(old) = view_at(shown as u32, (shown >> 32) as u32)
            {
                let kin = kin(&old, &view);
                if kin == Kin::Same {
                    KEPT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    drop(held);
                    self.unmake_stale(index, stale);
                    return Some((view, None));
                }
                // And of another texture that can stand for it
                // ([`keep_shown`]): black, then grey for a frame or two is
                // what popped.
                if is_texture_view(&old) {
                    let kept = kin.kept(keep_shown());
                    rewritten(kin, kept);
                    if kept {
                        drop(held);
                        self.unmake_stale(index, stale);
                        return Some((view, None));
                    }
                }
            }
            #[cfg(feature = "diagnostics")]
            if !self.sampler && is_texture_view(&view) {
                note_before(index, shown, if later { "null, waits" } else { "at once" });
            }
            self.quick_is(index, 0);
            self.made.set(index, if later { 0 } else { key });
            let kept = if later {
                None
            } else {
                make_twinned_with_copy(&view, self.at(index), self.twin(index)).map(|(k, copy)| {
                    made_copy = copy;
                    k
                })
            };
            if kept.is_none() && !self.sampler {
                null_for(&view, self.at(index));
            }
            if kept.is_some() || !self.sampler {
                self.put(index);
            }
            self.twin_made(index, kept.is_some() && twinned(&view));
            self.keep(index, kept.flatten());
            self.quick_is(index, if later { 0 } else { key });
            drop(held);
            self.unmake_stale(index, stale);
        }
        Some((view, made_copy))
    }

    /// Whether slot `index` was made from no record: what [`HostHeap::mirror`]
    /// would find before it did nothing for one that holds no view.
    pub(crate) fn shows_none(&self, index: u32) -> bool {
        self.made.get(index).unwrap_or(0) == 0
    }

    /// Whether slot `index` is already made from `key`.
    pub(crate) fn shows_key(&self, index: u32, key: u64) -> bool {
        key != 0 && self.made.get(index) == Some(key)
    }

    /// Slot `index` is made from record `key` and stays so (0: not). Under
    /// the lock of `made`.
    pub(super) fn quick_is(&self, index: u32, key: u64) {
        if let Some(quick) = self.quick.get(index as usize) {
            quick.store(key, std::sync::atomic::Ordering::Release);
        }
    }

    /// Whether slot `index` is backed by a made host descriptor (tests).
    #[cfg(test)]
    pub(crate) fn made(&self, index: u32) -> bool {
        self.made.get(index).is_some_and(|key| key != 0)
    }

    /// Whether host slot `index` shows `buffer`; `None` if it shows no
    /// resource (diagnostics: a buffer view's slot must show the host copy
    /// that is brought up to date for it).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn shows(&self, index: u32, buffer: &ID3D12Resource) -> Option<bool> {
        let shown = self.kept_in(index)?;
        Some(windows::core::Interface::as_raw(&shown) == windows::core::Interface::as_raw(buffer))
    }

    /// What the queue of textures picked by index knows of title slot
    /// `index` at `frame`, in a few words (diagnostics).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn texture_slot_state(&self, index: u32, frame: u64) -> String {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match textures.slots.get(&index) {
            None => "not queued".into(),
            Some(w) => format!(
                "queued due={} written@{} looks={} part_mapped={} recheck={}",
                if w.due <= frame { "now".to_string() } else { format!("@{}", w.due) },
                w.written,
                w.looks,
                w.mapped,
                w.checked.2 != 0
            ),
        }
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = self.made.get(index)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

