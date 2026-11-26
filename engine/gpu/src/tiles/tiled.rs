use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

use super::pool::{give_back, take};
use super::{HEAP_TILES, STATS, TILE, free_room};
use crate::host::host;

/// What is given so far to one texture.
pub(super) struct Given {
    /// Per level with tiles of its own: whether it has them.
    pub(super) levels: Vec<bool>,
    /// Whether the small levels the card packs together have theirs.
    pub(super) packed: bool,
    /// The runs taken: the heap's place, first tile, tiles.
    pub(super) runs: Vec<(usize, u32, u32)>,
}

/// A host texture made without memory, and the tiles its levels have been
/// given.
pub(crate) struct Tiled {
    /// Tiles of each level that has tiles of its own (level 0 first).
    pub(super) standard: Vec<u32>,
    /// The tiles of the levels the card packs together (0: there are none);
    /// they are the levels from `standard.len()` on.
    pub(super) packed: u32,
    /// Levels in all.
    pub(super) levels: u32,
    /// The bytes all its tiles would take.
    pub(crate) full: u64,
    pub(super) given: Mutex<Given>,
    /// The first level a view of the texture may show: the first one that
    /// is held or was written by the card and has tiles (`u32::MAX`: none
    /// yet).
    pub(super) shown: AtomicU32,
    /// The levels held at the last upload (a bit a level).
    pub(super) held: AtomicU32,
    /// The levels the card itself wrote, in lists submitted so far.
    pub(super) card: AtomicU32,
    /// The levels a recorded list of the card's own writes has already
    /// written with zeros first ([`Tiled::claim`]).
    pub(super) claimed: AtomicU32,
}

impl Tiled {
    /// For `texture`, just made reserved with `levels` levels and one
    /// layer: how the card cuts it into tiles. `None` if the card will not
    /// say, or cuts it in a way this does not handle; the caller then makes
    /// the texture the old way.
    pub(crate) fn make(device: &ID3D12Device, texture: &ID3D12Resource, levels: u32) -> Option<Tiled> {
        if levels == 0 || levels > 16 {
            return None;
        }
        let mut total = 0u32;
        let mut packed = D3D12_PACKED_MIP_INFO::default();
        let mut shape = D3D12_TILE_SHAPE::default();
        let mut count = levels;
        let mut tilings = vec![D3D12_SUBRESOURCE_TILING::default(); levels as usize];
        // SAFETY: outputs of our own, `count` tilings of them.
        unsafe {
            device.GetResourceTiling(
                texture,
                Some(&raw mut total),
                Some(&raw mut packed),
                Some(&raw mut shape),
                Some(&raw mut count),
                0,
                tilings.as_mut_ptr(),
            );
        }
        let standard_levels = u32::from(packed.NumStandardMips);
        if total == 0 || standard_levels + u32::from(packed.NumPackedMips) != levels || standard_levels > count {
            return None;
        }
        let mut standard = Vec::with_capacity(standard_levels as usize);
        for tiling in &tilings[..standard_levels as usize] {
            let tiles = tiling
                .WidthInTiles
                .checked_mul(u32::from(tiling.HeightInTiles))
                .and_then(|n| n.checked_mul(u32::from(tiling.DepthInTiles)))
                .unwrap_or(0);
            if tiles == 0 || tiles > HEAP_TILES || tiling.StartTileIndexInOverallResource == D3D12_PACKED_TILE {
                return None;
            }
            standard.push(tiles);
        }
        let packed_tiles = if packed.NumPackedMips > 0 { packed.NumTilesForPackedMips } else { 0 };
        if (packed.NumPackedMips > 0 && packed_tiles == 0) || packed_tiles > HEAP_TILES {
            return None;
        }
        let full = u64::from(total) * TILE;
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(full, Ordering::Relaxed);
        STATS[4].fetch_add(1, Ordering::Relaxed);
        Some(Tiled {
            given: Mutex::new(Given {
                levels: vec![false; standard.len()],
                packed: false,
                runs: Vec::new(),
            }),
            standard,
            packed: packed_tiles,
            levels,
            full,
            shown: AtomicU32::new(u32::MAX),
            held: AtomicU32::new(0),
            card: AtomicU32::new(0),
            claimed: AtomicU32::new(0),
        })
    }

    /// At the submission of a list that uploads every level of `texture`
    /// from its staging buffer, of which `held` are the title's now (the
    /// others are zeros there): tiles for the held ones that have none.
    /// `room`: whether the card has room for more host copies
    /// (`resource.rs` `room`). Without room no held level gets tiles: the
    /// list still brings the levels that have theirs up to date (which
    /// takes none of the card's memory), and its copies into the others go
    /// nowhere. (An upload of a host copy that exists passes the full
    /// card's gate in `draw.rs` on the ground that it takes no memory;
    /// with tiles given here regardless, the line would not bound these
    /// textures.)
    /// Returns whether the first level a view may show changed, and
    /// whether a level named is still without tiles (no room, or no heap
    /// could be had): the caller has the upload recorded again later, or
    /// the level would stay empty until the title writes the texture.
    pub(crate) fn uploaded(&self, texture: &ID3D12Resource, held: u32, room: bool) -> (bool, bool) {
        match host() {
            Some(host) => {
                let reuse_only = !room && free_room() >= (16 << 20);
                self.uploaded_on_mode(host.queue(), &host.device, texture, held, room, reuse_only)
            }
            None => (false, false),
        }
    }

    /// [`Tiled::uploaded`] on a queue and a device.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn uploaded_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        held: u32,
        room: bool,
    ) -> (bool, bool) {
        self.uploaded_on_mode(queue, device, texture, held, room, false)
    }

    /// Like [`Tiled::uploaded_on`], optionally allowing held levels to reuse
    /// already-committed free pool tiles (`no_grow = true`) when `!room`.
    pub(crate) fn uploaded_on_mode(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        held: u32,
        room: bool,
        reuse_only: bool,
    ) -> (bool, bool) {
        let held = held & self.every_level();
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        self.held.store(held, Ordering::Relaxed);
        let card = self.card.load(Ordering::Relaxed);
        let named = held | card;
        // The levels the card itself writes are few and are not kept
        // waiting: a write of the card's into no memory is lost.
        let changed = if room {
            self.give_locked(&mut given, queue, device, texture, named, named, false)
        } else {
            let mut changed = self.give_locked(&mut given, queue, device, texture, named, card, false);
            let held_only = held & !card;
            if reuse_only && held_only != 0 {
                changed |= self.give_locked(&mut given, queue, device, texture, named, held_only, true);
            }
            changed
        };
        let missing = self.without_locked(&given, named) != 0;
        (changed, missing)
    }

    /// At the submission of a list in which the card itself writes the
    /// levels `levels` of `texture` (after zeros, if they were new:
    /// [`Tiled::claim`]): tiles for those that have none. Returns whether
    /// the first level a view may show changed.
    /// Only the levels this list writes get tiles here: a held level that
    /// is waiting for room is written by no copy of this list, and tiles
    /// hold what their last owner left.
    pub(crate) fn card_wrote(&self, texture: &ID3D12Resource, levels: u32) -> bool {
        match host() {
            Some(host) => self.card_wrote_on(host.queue(), &host.device, texture, levels),
            None => false,
        }
    }

    /// [`Tiled::card_wrote`] on a queue and a device.
    pub(crate) fn card_wrote_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        levels: u32,
    ) -> bool {
        let levels = levels & self.every_level();
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let prev = self.card.load(Ordering::Relaxed);
        let changed = self.give_locked(
            &mut given,
            queue,
            device,
            texture,
            prev | levels | self.held.load(Ordering::Relaxed),
            levels,
            false,
        );
        // Only levels that actually received tiles (or already had them) saw
        // the card's write; a level that got no tiles saw its zeros and write
        // dropped, so neither `card` nor `claimed` should keep it.
        let without = self.without_locked(&given, levels);
        let backed = levels & !without;
        if backed != 0 {
            self.card.fetch_or(backed, Ordering::Relaxed);
        }
        if without != 0 {
            self.unclaim(without);
        }
        changed
    }

    /// The bits of the levels that get their tiles with level `level`:
    /// itself, or every level the card packs together if it is one of them.
    pub(crate) fn with_level(&self, level: u32) -> u32 {
        let first_packed = self.standard.len() as u32;
        if level < first_packed {
            1 << level
        } else if level < self.levels {
            self.every_level() & !((1u32 << first_packed) - 1)
        } else {
            0
        }
    }

    /// Of `levels`, the ones that have no tiles now and that no list
    /// recorded before has claimed: the caller's list writes them with
    /// zeros before the card's own write, so that what the write does not
    /// cover is not what the tiles' last owner left.
    pub(crate) fn claim(&self, levels: u32) -> u32 {
        let fresh = self.without(levels);
        fresh & !self.claimed.fetch_or(fresh, Ordering::Relaxed)
    }

    /// Gives up the claim on `levels` ([`Tiled::claim`]): the zeros of the
    /// list that claimed them never reached tiles of theirs (the list was
    /// reset without being submitted, the zeros could not be recorded, or
    /// no tiles could be had), so the next list that writes them sends its
    /// own.
    /// shortcut: a second list recorded between the claim and this sends
    /// no zeros; if it is the first whose write gets tiles, what that
    /// write does not cover is what the tiles' last owner left.
    pub(crate) fn unclaim(&self, levels: u32) {
        self.claimed.fetch_and(!levels, Ordering::Relaxed);
    }

    /// Of `levels` (a bit a level), the ones that have no tiles now.
    pub(crate) fn without(&self, levels: u32) -> u32 {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        self.without_locked(&given, levels)
    }

    fn without_locked(&self, given: &Given, levels: u32) -> u32 {
        let first_packed = self.standard.len() as u32;
        let mut without = 0;
        for level in 0..self.levels.min(32) {
            let has = if level < first_packed { given.levels[level as usize] } else { given.packed };
            if !has {
                without |= 1 << level;
            }
        }
        levels & without
    }

    /// Gives tiles to the levels of `texture` that both `named` and `may`
    /// name (a bit a level, level 0 the lowest) and that have none yet;
    /// the levels the card packs together get theirs when any of them is
    /// so named. `named`: the levels that are the title's or the card's
    /// own, by which the first level a view may show is told; `may`: the
    /// ones of them that may get tiles in this call. To be called only
    /// from a submission-time step of a list that writes every level of
    /// `may`, whole: the call is ahead of that list on `queue`, and tiles
    /// hold what their last owner left.
    /// Returns whether the first level a view may show changed
    /// ([`Tiled::first_shown`]): the caller has the texture's slots made
    /// again.
    #[cfg(test)]
    pub(crate) fn give_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        named: u32,
        may: u32,
    ) -> bool {
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        self.give_locked(&mut given, queue, device, texture, named, may, false)
    }

    fn give_locked(
        &self,
        given: &mut Given,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        named: u32,
        may: u32,
        no_grow: bool,
    ) -> bool {
        let has = |level: u32| level < 32 && named & (1 << level) != 0;
        let now = |level: u32| level < 32 && named & may & (1 << level) != 0;
        let first_packed = self.standard.len() as u32;
        // (subresource, tiles, the level if it has tiles of its own) to map now.
        let mut wanted: Vec<(u32, u32, Option<usize>)> = Vec::new();
        for level in 0..first_packed {
            if !given.levels[level as usize] && now(level) {
                wanted.push((level, self.standard[level as usize], Some(level as usize)));
            }
        }
        if self.packed > 0 && !given.packed && (first_packed..self.levels).any(now) {
            wanted.push((first_packed, self.packed, None));
        }
        for (subresource, tiles, level) in wanted {
            let Some((heap, index, runs)) = take(device, tiles, no_grow) else {
                continue;
            };
            let at = D3D12_TILED_RESOURCE_COORDINATE {
                X: 0,
                Y: 0,
                Z: 0,
                Subresource: subresource,
            };
            let size = D3D12_TILE_REGION_SIZE {
                NumTiles: tiles,
                ..Default::default()
            };
            // One region, the subresource's tiles in order, on as many
            // ranges of the heap as the runs taken.
            let flags = vec![D3D12_TILE_RANGE_FLAG_NONE; runs.len()];
            let starts: Vec<u32> = runs.iter().map(|run| run.0).collect();
            let counts: Vec<u32> = runs.iter().map(|run| run.1).collect();
            let began = std::time::Instant::now();
            // SAFETY: a live reserved texture and a live heap of ours; one
            // region of `tiles` tiles from the subresource's first tile;
            // `runs.len()` ranges whose counts add up to `tiles`, in three
            // arrays of that length that live through the call.
            unsafe {
                queue.UpdateTileMappings(
                    texture,
                    1,
                    Some(&raw const at),
                    Some(&raw const size),
                    &heap,
                    runs.len() as u32,
                    Some(flags.as_ptr()),
                    Some(starts.as_ptr()),
                    Some(counts.as_ptr()),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
            }
            STATS[6].fetch_add(1, Ordering::Relaxed);
            STATS[7].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
            match level {
                Some(level) => given.levels[level] = true,
                None => given.packed = true,
            }
            for &(start, count) in &runs {
                given.runs.push((index, start, count));
            }
            STATS[2].fetch_add(u64::from(tiles) * TILE, Ordering::Relaxed);
            STATS[5].fetch_add(1, Ordering::Relaxed);
        }
        let with_tiles = |level: u32| {
            if level < first_packed {
                given.levels[level as usize]
            } else {
                given.packed
            }
        };
        let shown = (0..self.levels).find(|&level| has(level) && with_tiles(level)).unwrap_or(u32::MAX);
        self.shown.swap(shown, Ordering::Relaxed) != shown
    }

    /// The first level a view of the texture may show; `None` while no
    /// level is both held and given (the views are then as the title made
    /// them, and read zeros).
    pub(crate) fn first_shown(&self) -> Option<u32> {
        let shown = self.shown.load(Ordering::Relaxed);
        (shown != u32::MAX).then_some(shown)
    }

    /// Whether level `level` has tiles.
    #[cfg(test)]
    pub(super) fn has_tiles(&self, level: u32) -> bool {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        match given.levels.get(level as usize) {
            Some(&has) => has,
            None => level < self.levels && given.packed,
        }
    }

    /// Whether every level has tiles.
    #[cfg(test)]
    pub(super) fn whole(&self) -> bool {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.levels.iter().all(|&has| has) && (self.packed == 0 || given.packed)
    }

    /// A bit for every level of the texture.
    pub(crate) fn every_level(&self) -> u32 {
        if self.levels >= 32 { u32::MAX } else { (1 << self.levels) - 1 }
    }

    /// The bytes of the tiles it has now.
    pub(crate) fn given_bytes(&self) -> u64 {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.runs.iter().map(|run| u64::from(run.2) * TILE).sum()
    }
}

impl Drop for Tiled {
    fn drop(&mut self) {
        let given = self.given.get_mut().unwrap_or_else(|e| e.into_inner());
        let mut bytes = 0;
        let mut by_heap: std::collections::BTreeMap<usize, Vec<(u32, u32)>> = std::collections::BTreeMap::new();
        for &(index, start, tiles) in &given.runs {
            by_heap.entry(index).or_default().push((start, tiles));
            bytes += u64::from(tiles) * TILE;
        }
        for (index, runs) in by_heap {
            give_back(index, &runs);
        }
        STATS[0].fetch_sub(1, Ordering::Relaxed);
        STATS[1].fetch_sub(self.full, Ordering::Relaxed);
        STATS[2].fetch_sub(bytes, Ordering::Relaxed);
    }
}
