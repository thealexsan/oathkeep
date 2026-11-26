//! Descriptor table mirroring, fresh CBV/high-slot snapshots, and readiness checks.

use std::sync::atomic::{AtomicU64, Ordering};

use oathkeep_shader::{ResourceType, RootType, UsedResource};

use crate::descriptors::View;
use crate::backend::HostList;

use super::plan::{Few, FreshTable, RecordHash, TablePlan, ready_once};
use super::readable::{readable, view_kind};
use super::state::{Root, State};
use super::{STORES, TESTED_DEPTH};

impl State {
    fn table_descriptor(
        &self,
        list: &HostList,
        slot: u8,
        descriptor: u32,
        kind: Option<ResourceType>,
        compute: bool,
    ) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
            return;
        };
        let Some(handle) = table.checked_add(u64::from(descriptor) * 32) else {
            return;
        };
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true) {
            if let Some(kind) = kind.or_else(|| view_kind(&view)) {
                readable(list, kind, &view);
            }
            // Up to date by this draw: not the texture pass's to make.
            if crate::descriptors::host_heap::is_texture_view(&view) {
                host.texture_slot_tabled(index, true);
            } else {
                host.slot_tabled(index);
            }
        }
    }

    fn table_entry(&self, list: &HostList, u: &UsedResource, compute: bool) {
        self.table_descriptor(list, u.slot, u.table_offset_dwords / 8, Some(u.kind), compute);
    }

    /// The descriptors a shader reads through tables. Tables holding a
    /// constant buffer are made afresh for the draw in dynamic slots, with
    /// the constant data copied to upload memory when the list is
    /// submitted: the host wants constant buffers 256-byte aligned, the
    /// console only 16. Returns those tables' new byte offsets by root
    /// parameter; the other tables' slots are made from the title's.
    ///
    /// Within a list, the same table holding the same descriptors is made
    /// once: its constant data is copied at submission either way, and the
    /// big tables the world binds would otherwise take hundreds of slots per
    /// draw, wrapping the dynamic slots onto lists not yet submitted.
    pub(crate) fn tables(
        &mut self,
        list: &HostList,
        plan: &TablePlan,
        compute: bool,
    ) -> Few<(u8, u32), 16> {
        let mut fresh = [0u64; 4];
        let selected_slot = |mask: &[u64; 4], slot: u8| mask[usize::from(slot / 64)] & (1u64 << (slot % 64)) != 0;
        let selected_by_group: Vec<Vec<u32>> = plan
            .groups
            .iter()
            .map(|group| {
                group.selected_descriptors(|cslot, cdword| {
                    self.user_value(compute, cslot, RootType::Constants, cdword)
                })
            })
            .collect();
        for (group, selected) in plan.groups.iter().zip(&selected_by_group) {
            let slot = group.slot;
            let needs_snapshot = group.has_cbv
                || match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::Table(table)) => self.heap_of(table).is_some_and(|heap| {
                        heap.host().is_some_and(|host| {
                            group.needs_snapshot(heap, host.count(), table, selected)
                        })
                    }),
                    _ => false,
                };
            if needs_snapshot {
                fresh[usize::from(slot / 64)] |= 1u64 << (slot % 64);
            }
        }
        for &(g, e) in &plan.direct {
            let group = &plan.groups[g];
            if !selected_slot(&fresh, group.slot) {
                self.table_entry(list, &group.entries[e], compute);
            }
        }
        for (group, selected) in plan.groups.iter().zip(&selected_by_group) {
            if !selected.is_empty() && !selected_slot(&fresh, group.slot) {
                for &k in selected {
                    if group.entry(k).is_none() {
                        self.table_descriptor(list, group.slot, k, None, compute);
                    }
                }
            }
        }
        let mut made = Few::default();
        for (group, selected) in plan.groups.iter().zip(selected_by_group) {
            let slot = group.slot;
            if !selected_slot(&fresh, slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            let entries = &group.entries;
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            if heap.samplers() || (table - heap.start) % 32 != 0 {
                continue;
            }
            let Some(host) = heap.host() else {
                continue;
            };
            let n = (group.max_dword / 8).max(selected.last().copied().unwrap_or(0)) + 1;
            let last = table.checked_add(u64::from(n - 1) * 32);
            if n > host.dynamic_count()
                || last.and_then(|handle| heap.index(handle)).is_none()
            {
                once!(slot, n, "fresh descriptor table exceeds the available span");
                continue;
            }
            // The table and what its entries hold (record words).
            let key = {
                let mut h = RecordHash::default();
                for k in 0..n {
                    let at = table + u64::from(k) * 32;
                    // SAFETY: a descriptor slot in title memory.
                    h.add(unsafe { (at as *const [u32; 3]).read_unaligned() });
                }
                (table, n, h.0)
            };
            // Who reads of it, and as what, as far as [`readable`] goes by it.
            let reader = (std::ptr::from_ref(plan) as usize, plan.direct.len(), STORES.get(), TESTED_DEPTH.get().0);
            if let Some(known) = self.fresh_tables.get_mut(&key) {
                let first = known.first;
                // This reader had it brought up to date, and nothing in the
                // list changed since: nothing to do again.
                let once = ready_once()
                    && !crate::textures::texture::BAKING.get()
                    && selected.is_empty()
                    && !group.has_uav;
                if once
                    && known.epoch == list.epoch()
                    && let Some((_, depth)) = known.ready.iter().find(|(r, _)| *r == reader)
                {
                    if *depth {
                        TESTED_DEPTH.set((reader.3, true));
                    }
                    made.push((slot, first * 32));
                    continue;
                }
                let depth_before = TESTED_DEPTH.get();
                TESTED_DEPTH.set((depth_before.0, false));
                for u in entries.iter().filter(|u| u.kind != ResourceType::Cbv) {
                    let handle = table + u64::from(u.table_offset_dwords) * 4;
                    if let Some(view) = crate::descriptors::view(handle) {
                        readable(list, u.kind, &view);
                    }
                }
                for k in selected {
                    if group.entry(k).is_some() {
                        continue;
                    }
                    let handle = table + u64::from(k) * 32;
                    if let Some(view) = crate::descriptors::view(handle)
                        && let Some(kind) = view_kind(&view)
                    {
                        readable(list, kind, &view);
                    }
                }
                let depth = TESTED_DEPTH.get().1;
                TESTED_DEPTH.set((depth_before.0, depth_before.1 || depth));
                if once && let Some(known) = self.fresh_tables.get_mut(&key) {
                    // (The epoch as it is now: what this changed itself it
                    // need not do again.)
                    let epoch = list.epoch();
                    if known.epoch != epoch {
                        known.epoch = epoch;
                        known.ready.clear();
                    }
                    if known.ready.len() < 16 {
                        known.ready.push((reader, depth));
                    }
                }
                made.push((slot, first * 32));
                continue;
            }
            let Some(first) = host.dynamic(n).or_else(|| {
                crate::backend::settle_for_fence();
                host.dynamic(n)
            }) else {
                // No room: the draw must not run with the title's own
                // offsets (slots never made for it); `bind` drops it.
                made.push((slot, u32::MAX));
                continue;
            };
            self.fresh_tables.insert(key, FreshTable { first, epoch: 0, ready: Vec::new() });
            self.dynamic
                .push((std::ptr::from_ref(heap) as usize, first, n));
            // The title's own slots of this table: what the draw reads of
            // them it brings up to date here, in the dynamic slots.
            let title_first = heap.index(table).filter(|first| first + n <= host.count());
            // They are such a table's, whatever they hold. (Of a big one,
            // which may lie over slots shaders pick by index, only the ones
            // the shader names.)
            if let Some(title) = title_first {
                for k in (0..n).filter(|&k| n <= 64 || group.entry(k).is_some()) {
                    host.slot_tabled(title + k);
                }
            }
            let list_views = crate::descriptors::host_heap::list_views();
            if self.recording == 0 {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                self.recording = NEXT.fetch_add(1, Ordering::Relaxed);
            }
            let recording = self.recording;
            // The whole range: a shader may index past the entries its
            // used-resource list names (descriptor arrays).
            for k in 0..n {
                let index = first + k;
                // A view this list has made before: its descriptor copied,
                // and nothing more if nothing changed in the list since.
                if list_views
                    && let Some(words) = crate::descriptors::record_at(table + u64::from(k) * 32)
                    && let Some(mut listed) = host.make_listed(recording, words, index)
                {
                    let kind = group
                        .entry(k)
                        .map(|u| u.kind)
                        .or_else(|| selected.binary_search(&k).ok().and(listed.kind));
                    let was = (listed.ready, listed.tabled);
                    if let Some(kind) = kind {
                        let tested = TESTED_DEPTH.get();
                        if !crate::textures::texture::BAKING.get()
                            && kind == ResourceType::Srv
                            && listed.ready.0 == list.epoch()
                            && listed.ready.1 == tested.0
                        {
                            if listed.ready.2 {
                                TESTED_DEPTH.set((tested.0, true));
                            }
                        } else if let Some(view) = crate::descriptors::host_heap::listed_view(recording, words) {
                            let _ = crate::memory::residency::take_staging_deferred();
                            TESTED_DEPTH.set((tested.0, false));
                            readable(list, kind, &view);
                            let depth = TESTED_DEPTH.get().1;
                            TESTED_DEPTH.set((tested.0, tested.1 || depth));
                            if kind == ResourceType::Srv && !crate::memory::residency::take_staging_deferred() {
                                listed.ready = (list.epoch(), tested.0, depth);
                            }
                        }
                    }
                    if let Some(title) = title_first
                        && listed.texture
                        && listed.tabled != Some((title + k, kind.is_some()))
                    {
                        listed.tabled = Some((title + k, kind.is_some()));
                        host.texture_slot_tabled(title + k, kind.is_some());
                        if kind.is_some() {
                            host.mirror_after(list, title + k, table + u64::from(k) * 32);
                        }
                    }
                    if was != (listed.ready, listed.tabled) {
                        crate::descriptors::host_heap::note_listed(recording, words, listed);
                    }
                    continue;
                }
                let used = group.entry(k);
                match crate::descriptors::view(table + u64::from(k) * 32) {
                    Some(View::Constant(address, size)) => {
                        let Some((buffer, gpu, to)) = list.scratch_held(u64::from(size)) else {
                            host.make_null(index);
                            continue;
                        };
                        let slot = (std::ptr::from_ref(heap) as usize, index);
                        list.defer_constants(to, address as *const u8, size as usize, slot);
                        host.constant_at(index, &buffer, gpu, size);
                    }
                    Some(view) => {
                        host.make_at(index, &view);
                        let kind = used.map(|u| u.kind).or_else(|| {
                            selected.binary_search(&k).ok().and_then(|_| view_kind(&view))
                        });
                        if let Some(kind) = kind {
                            readable(list, kind, &view);
                        }
                        // The title's slot is a table's: out of the queue
                        // of textures picked by index if the draw read it
                        // (its texture is up to date now), and made in
                        // place as the texture pass would have made it,
                        // for a shader that picks it by index all the same.
                        if let Some(title) = title_first
                            && crate::descriptors::host_heap::is_texture_view(&view)
                        {
                            host.texture_slot_tabled(title + k, kind.is_some());
                            if kind.is_some() {
                                host.mirror_after(list, title + k, table + u64::from(k) * 32);
                            }
                        }
                    }
                    None => {
                        host.make_null(index);
                        if used.is_some() {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            made.push((slot, first * 32));
        }
        made
    }
}
