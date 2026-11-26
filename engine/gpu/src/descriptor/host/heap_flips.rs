use windows::Win32::Graphics::Direct3D12::*;
use super::{HostHeap, VIEW_TAG, View, view_at};
#[cfg(feature = "diagnostics")]
use super::flips::note_before;
use super::flips::{RESHOWN_AFTER, neutral_for, FLIPPED, FLIPS, Flip, LATE, NEXT_PENDING_FLIP, PENDING_LIST_COUNT, PENDING_LIST_FLIPS, PendingListFlip, record_key, remove_pending_list_flip, show_after, take_pending_list_flip};
#[cfg(feature = "diagnostics")]
use super::kin::same_bytes;
use super::kin::{Kin, placed_texture, same_placed_texture, keep_shown, keep_views, kin, rewritten};
use super::make::make;
use super::null_views::{null_for, null_view};
use super::spans::{STALE, stale_rule};

impl HostHeap {

    /// The memory of the texture title slot `index` views is not mapped
    /// (any more): a host slot made from it becomes null again, so that
    /// the host copy can be let go (`crate::resource::let_go_idle`; a slot
    /// keeps what it views). True if the slot was made.
    ///
    /// Not at the first look: a title that moves a texture in its streaming
    /// memory unmaps it before it maps it again, and what was shown went
    /// black for the pictures in between (session play-1125-0127: faces,
    /// books, posters and sleeves for 0.3 to 2 s). Only when it is still
    /// away `AWAY_MS` after it was first found away
    /// (`OATHKEEP_TEXTURE_AWAY_MS`; 0: at once, as before).
    pub(crate) fn texture_slot_gone(&self, index: u32) -> bool {
        const AWAY_MS: u64 = 3000;
        static WAIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let wait = *WAIT.get_or_init(|| {
            std::env::var("OATHKEEP_TEXTURE_AWAY_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(AWAY_MS)
        });
        let _made = self.made.lock(index);
        let Some(key) = self.made.get(index).filter(|&key| key != 0) else {
            return false;
        };
        let now = crate::host::now_ms();
        {
            let mut away = self.away.lock().unwrap_or_else(|e| e.into_inner());
            match away.get(&index) {
                Some(&(was, since)) if was == key && now.saturating_sub(since) < wait => return false,
                Some(&(was, _)) if was == key => {
                    away.remove(&index);
                }
                _ if wait == 0 => {}
                _ => {
                    away.insert(index, (key, now));
                    return false;
                }
            }
        }
        self.unmake(index);
        STALE[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Immediately evicts a cold or long-unmapped texture slot under VRAM
    /// pressure without waiting for `AWAY_MS` (`texture_slot_gone`): writes a
    /// null view and retires the held `ID3D12Resource` behind in-flight GPU
    /// fences via [`HostHeap::unmake`] -> [`HostHeap::keep`] ->
    /// `host::retire_resource`. When `requeue_due` is `Some((frame, due))`,
    /// re-queues the slot so a still-mapped texture can re-upload once VRAM
    /// headroom returns.
    pub(crate) fn evict_texture_slot(&self, index: u32, requeue_due: Option<(u64, u64)>) -> bool {
        {
            let _made = self.made.lock(index);
            if !self.made.get(index).is_some_and(|key| key != 0) {
                return false;
            }
            {
                let mut away = self.away.lock().unwrap_or_else(|e| e.into_inner());
                away.remove(&index);
            }
            self.unmake(index);
        }
        if let Some((frame, due)) = requeue_due {
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            textures.changed(index, frame, due);
        }
        STALE[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Whether host slot `index` shows the placed texture the title's record
    /// at `handle` views, by an earlier view of it.
    pub(crate) fn texture_slot_shows_other(&self, index: u32, handle: u64) -> bool {
        let key = record_key(handle);
        let Some(shown) = self.made.get(index) else {
            return false;
        };
        if key == 0 || shown == 0 || shown == key {
            return false;
        }
        let view = |key: u64| view_at(key as u32, (key >> 32) as u32);
        matches!((view(shown), view(key)), (Some(a), Some(b)) if same_placed_texture(&a, &b))
    }

    /// Slot `index` is made from the record at `handle` (what it is now)
    /// when `list`, which holds the upload of what that view shows, has run:
    /// until then it shows what it showed. (Made at once, the frames on the
    /// card read the new view before its levels were filled.)
    pub(crate) fn texture_slot_flip(&self, list: &crate::host::HostList, index: u32, handle: u64) {
        self.flip(list, index, handle, None);
    }

    /// The same of a slot that shows nothing of the texture the record at
    /// `handle` views, or something else: until then it shows the neutral
    /// texture ([`show_after`]).
    pub(crate) fn texture_slot_show_after(&self, list: &crate::host::HostList, index: u32, handle: u64) {
        if self.show_later(index, handle) {
            self.flip(list, index, handle, Some(crate::frame::current()));
        }
    }

    /// What slot `index` shows until the texture the record at `handle` views
    /// is shown. False: it shows it, or there is none.
    fn show_later(&self, index: u32, handle: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let key = record_key(handle);
        let view = view_at(key as u32, (key >> 32) as u32);
        let (true, Some(view)) = (key != 0, view) else {
            return false;
        };
        let stale = {
            let _made = self.made.lock(index);
            let Some(shown) = self.made.get(index) else {
                return false;
            };
            if shown == key {
                return false;
            }
            // (As `mirror`.)
            let stale = self.note_buffer(index, Some(&view), true);
            let old = view_at(shown as u32, (shown >> 32) as u32);
            let kept = old.is_some_and(|old| match kin(&old, &view) {
                Kin::Same => true,
                kin => kin.kept(keep_shown()),
            });
            if shown == 0 || !kept {
                #[cfg(feature = "diagnostics")]
                note_before(index, shown, "neutral");
                self.quick_is(index, 0);
                self.made.set(index, 0);
                let kept = neutral_for(&view, self.at(index));
                self.put(index);
                self.clear_twin(index);
                LATE[3].fetch_add(u64::from(kept.is_some()), Relaxed);
                self.keep(index, kept);
            }
            stale
        };
        self.unmake_stale(index, stale);
        true
    }

    /// [`HostHeap::texture_slot_show_after`] of a texture whose upload `job`
    /// makes: shown when that has run.
    pub(crate) fn texture_slot_show_ahead(&self, job: &crate::texture::ahead::Job, index: u32, handle: u64) {
        if !self.show_later(index, handle) {
            return;
        }
        let (heap, key) = (std::ptr::from_ref(self) as usize, record_key(handle));
        let late = Some(crate::frame::current());
        job.then(move || {
            let flip = Flip { after: 0, heap, index, handle, key, late };
            FLIPS.lock().unwrap_or_else(|e| e.into_inner()).push(flip);
        });
    }

    /// [`HostHeap::mirror`] of a slot whose table a draw read: a texture made
    /// a moment ago is shown after the draw's list ([`show_after`]; the draw
    /// reads the table's dynamic slots).
    pub(crate) fn mirror_after(&self, list: &crate::host::HostList, index: u32, handle: u64) {
        if show_after()
            && !crate::texture::BAKING.get()
            && index < self.count
            && !self.texture_slot_made_from(index, handle)
            && let Some(view) = crate::descriptor::view(handle)
            && let Some(View::Shader(resource, _)) = placed_texture(&view).or(Some(view))
            && crate::resource::buffer_bytes(resource as crate::object::This).is_none()
            && crate::resource::host_resource(resource as crate::object::This).is_some_and(crate::pool::fresh)
            && !crate::texture::ahead::there(resource as crate::object::This)
        {
            return self.texture_slot_show_after(list, index, handle);
        }
        self.mirror(index, handle, true);
    }

    fn flip(&self, list: &crate::host::HostList, index: u32, handle: u64, late: Option<u64>) {
        use std::sync::atomic::Ordering::{Relaxed, Release};
        let (heap, key) = (std::ptr::from_ref(self) as usize, record_key(handle));
        let id = NEXT_PENDING_FLIP.fetch_add(1, Relaxed);
        let origin = (std::ptr::from_ref(list) as usize, list.epoch());
        {
            let mut pending = PENDING_LIST_FLIPS.lock().unwrap_or_else(|e| e.into_inner());
            pending.push(PendingListFlip {
                id,
                origin,
                flip: Flip { after: 0, heap, index, handle, key, late },
            });
            PENDING_LIST_COUNT.store(pending.len(), Release);
        }
        struct PendingGuard(u64);
        impl PendingGuard {
            fn id(&self) -> u64 {
                self.0
            }
        }
        impl Drop for PendingGuard {
            fn drop(&mut self) {
                remove_pending_list_flip(self.0);
            }
        }
        let guard = PendingGuard(id);
        list.defer_host(move || {
            // Call a `&self` method so Rust 2021+ moves the whole `guard`
            // into this closure rather than copying `guard.0` and dropping
            // `guard` immediately at the end of `flip()`.
            let Some(mut flip) = take_pending_list_flip(guard.id()) else {
                return;
            };
            flip.after = crate::host::submissions().0;
            FLIPS.lock().unwrap_or_else(|e| e.into_inner()).push(flip);
        });
    }

    /// Whether any deferred slot flip or view remake for this heap requires
    /// settling in-flight GPU submissions before exposing real textures to a
    /// one-shot decoration bake draw.
    pub(crate) fn needs_gpu_settle_for_bake(&self) -> bool {
        let own = std::ptr::from_ref(self) as usize;
        let finished = crate::host::submissions().1;
        if PENDING_LIST_COUNT.load(std::sync::atomic::Ordering::Acquire) > 0 {
            let pending = PENDING_LIST_FLIPS.lock().unwrap_or_else(|e| e.into_inner());
            if pending.iter().any(|p| p.flip.heap == own) {
                return true;
            }
        }
        {
            let flips = FLIPS.lock().unwrap_or_else(|e| e.into_inner());
            if flips.iter().any(|f| f.heap == own && f.after > finished) {
                return true;
            }
        }
        let after = RESHOWN_AFTER.lock().unwrap_or_else(|e| e.into_inner());
        after.iter().any(|&(at, _)| at > finished)
    }

    /// Drains deferred flips still sitting in unsubmitted command lists for
    /// this heap so a decoration bake can mirror them out of `neutral()`.
    /// Entries that cannot be promoted yet (unmapped memory or deferred
    /// staging) must be returned via [`HostHeap::restore_pending_list_flip`].
    pub(crate) fn take_pending_list_flips(&self) -> Vec<PendingListFlip> {
        if PENDING_LIST_COUNT.load(std::sync::atomic::Ordering::Acquire) == 0 {
            return Vec::new();
        }
        let own = std::ptr::from_ref(self) as usize;
        let mut pending = PENDING_LIST_FLIPS.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest): (Vec<PendingListFlip>, Vec<PendingListFlip>) =
            pending.drain(..).partition(|p| p.flip.heap == own);
        *pending = rest;
        PENDING_LIST_COUNT.store(pending.len(), std::sync::atomic::Ordering::Release);
        mine
    }

    /// Re-queues an unpromoted deferred flip drained by
    /// [`HostHeap::take_pending_list_flips`] so its originating command list
    /// still flips it out of `neutral()` once submitted.
    pub(crate) fn restore_pending_list_flip(&self, item: PendingListFlip) {
        let mut pending = PENDING_LIST_FLIPS.lock().unwrap_or_else(|e| e.into_inner());
        pending.push(item);
        PENDING_LIST_COUNT.store(pending.len(), std::sync::atomic::Ordering::Release);
    }

    /// Whether title descriptor slot `handle` still holds record `key`.
    pub(crate) fn slot_still_at(&self, handle: u64, key: u64) -> bool {
        key != 0 && record_key(handle) == key
    }

    /// Looks up `(view, key)` for slot `index` prior to a synchronous bake
    /// upload without replacing its current host descriptor with `neutral()`;
    /// [`HostHeap::publish_uploaded_slot`] exposes the real descriptor and
    /// updates `Spans`/stride tracking once the GPU copy finishes.
    pub(crate) fn prepare_bake_slot(&self, index: u32, handle: u64) -> Option<(View, u64)> {
        if index >= self.count {
            return None;
        }
        let key = record_key(handle);
        let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten()?;
        Some((view, key))
    }

    /// Exposes the real host descriptor for slot `index` after its synchronous
    /// bake upload has completed on the GPU, updating `Spans`/stride tracking
    /// without re-queuing the slot in `TextureQueue`.
    pub(crate) fn publish_uploaded_slot(&self, index: u32, handle: u64, key: u64) -> bool {
        if index >= self.count || key == 0 {
            return false;
        }
        let (published, stale) = {
            let _made = self.made.lock(index);
            if self.made.get(index) == Some(key) {
                return false;
            }
            if record_key(handle) != key {
                return false;
            }
            let Some(view) = view_at(key as u32, (key >> 32) as u32) else {
                return false;
            };
            let stale = self.note_buffer(index, Some(&view), true);
            if let Some(kept) = make(&view, self.at(index)) {
                self.put(index);
                self.quick_is(index, 0);
                self.clear_twin(index);
                self.made.set(index, key);
                self.keep(index, kept);
                (true, stale)
            } else {
                (false, stale)
            }
        };
        self.unmake_stale(index, stale);
        published
    }

    /// Makes the slots whose new view's upload has run
    /// ([`HostHeap::texture_slot_flip`]); returns how many.
    pub(crate) fn texture_slots_flip(&self) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let finished = crate::host::submissions().1;
        let ready: Vec<Flip> = {
            let mut flips = FLIPS.lock().unwrap_or_else(|e| e.into_inner());
            if flips.is_empty() {
                return 0;
            }
            let own = std::ptr::from_ref(self) as usize;
            let (ready, wait): (Vec<Flip>, Vec<Flip>) =
                flips.drain(..).partition(|f| f.after <= finished && f.heap == own);
            *flips = wait;
            ready
        };
        let mut flipped = 0;
        let mut late_flipped = 0;
        for f in ready {
            let _made = self.made.lock(f.index);
            let Some(shown) = self.made.get(f.index) else {
                continue;
            };
            if shown == f.key {
                continue;
            }
            // Written again since, or made null: that write is in the queue.
            // (A slot shown late is null until now.)
            let view = view_at(f.key as u32, (f.key >> 32) as u32);
            let there = shown != 0 || f.late.is_some();
            let (true, true, Some(view)) = (f.key != 0 && record_key(f.handle) == f.key, there, view) else {
                FLIPPED[1].fetch_add(1, Relaxed);
                LATE[4].fetch_add(u64::from(f.late.is_some()), Relaxed);
                continue;
            };
            // Its memory went meanwhile: the texture pass finds that.
            if f.late.is_some()
                && let View::Placed(_, resource, desc) = &view
            {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let len = crate::resource::placed_bytes(resource).unwrap_or(1);
                if crate::texture::mapped_bytes(address as *const u8, len as usize) == 0 {
                    LATE[4].fetch_add(1, Relaxed);
                    continue;
                }
            }
            if let Some(kept) = make(&view, self.at(f.index)) {
                #[cfg(feature = "diagnostics")]
                note_before(f.index, shown, if f.late.is_some() { "shown after its upload" } else { "flip" });
                self.put(f.index);
                #[cfg(feature = "diagnostics")]
                let was = shown;
                self.quick_is(f.index, 0);
                self.made.set(f.index, f.key);
                self.keep(f.index, kept);
                #[cfg(feature = "diagnostics")]
                if f.late.is_some() && was != 0 {
                    same_bytes(was, f.key);
                }
                if let Some(since) = f.late {
                    late_flipped += 1;
                    let frames = crate::frame::current().saturating_sub(since);
                    let n = LATE[0].fetch_add(1, Relaxed) + 1;
                    let all = LATE[1].fetch_add(frames, Relaxed) + frames;
                    let most = LATE[2].fetch_max(frames, Relaxed).max(frames);
                    if n == 1 || n.is_multiple_of(5000) {
                        tracing::info!(
                            "slots shown after their texture's upload (OATHKEEP_SHOW_AFTER=1), since the start: {n}, {:.2} frames after they were asked for, the longest {most}; the neutral texture meanwhile in {}; {} not shown any more by then",
                            all as f64 / n as f64,
                            LATE[3].load(Relaxed),
                            LATE[4].load(Relaxed)
                        );
                    }
                } else {
                    flipped += 1;
                }
            }
        }
        FLIPPED[0].fetch_add(flipped as u64, Relaxed);
        flipped + late_flipped
    }

    /// The memory of the texture title slot `index` views is there (again):
    /// see [`HostHeap::texture_slot_gone`].
    pub(crate) fn texture_slot_here(&self, index: u32) {
        let mut away = self.away.lock().unwrap_or_else(|e| e.into_inner());
        if !away.is_empty() {
            away.remove(&index);
        }
    }

    /// Makes host slot `index` null again, as it is before its view is
    /// made: a null view of the kind it held. Under its lock ([`Made`]).
    pub(super) fn unmake(&self, index: u32) {
        self.quick_is(index, 0);
        let Some(key) = self.made.get(index) else {
            return;
        };
        self.made.set(index, 0);
        if key == 0 {
            return;
        }
        match view_at(key as u32, (key >> 32) as u32) {
            Some(view) => null_for(&view, self.at(index)),
            None => null_view(self.at(index)),
        }
        self.put(index);
        self.clear_twin(index);
        self.write_stride(index, 0);
        self.keep(index, None);
    }

    /// The slots [`HostHeap::note_buffer`] found stale for slot `index`, with
    /// what they were made from then, become null. None of their locks is
    /// held here; one made from another record meanwhile is left as it is.
    pub(super) fn unmake_stale(&self, index: u32, stale: Vec<(u32, u64)>) {
        for (slot, key) in stale {
            if slot == index || key == 0 {
                continue;
            }
            let _made = self.made.lock(slot);
            if self.made.get(slot) == Some(key) {
                self.unmake(slot);
            }
        }
    }
}
