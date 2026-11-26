//! Levels on demand: host copies of title textures that take the card's
//! memory level by level, as the title holds the levels.
//!
//! The title places a texture's whole chain of levels in its address space
//! and holds only some of them: the smallest first, at the start of the
//! texture's memory. The memory of the larger ones is either not mapped,
//! or belongs to the next texture, which the title packs where they would
//! be (run 188). A host copy was a committed texture: every level of it
//! took the card's memory from the first upload on. Run 189's accounting
//! line counted 2.2 to 3.0 GB of 4.3 to 5.5 GB of host copies in levels
//! whose title memory was not mapped, on some 1,600 copies; with about
//! 2.2 to 3.1 GB of render targets that filled the card, and textures
//! that needed a new host copy waited for room (the main character's face
//! among them, runs 188 and 189).
//!
//! Here such a texture is a reserved one (`CreateReservedResource`): it has
//! every level, so level numbers and sizes are what they were, but no
//! memory. The rules:
//! - A level is given tiles of a heap of ours (`UpdateTileMappings`, on the
//!   queue) only in a submission-time step of a list that also writes all
//!   of that level: the upload of the texture (`texture.rs` `upload`, which
//!   copies every level from the staging buffer every time), or, before
//!   the card itself writes the texture (a copy, a resolve), an upload of
//!   every level with every level given (`texture.rs` `make_whole`). The
//!   step runs when the list is submitted, before the list is executed, so
//!   the mapping is ahead of the copy on the queue. Tiles hold what their
//!   last owner left; no level is shown before it is written whole.
//! - Which levels are held is said by one function, at the moment the
//!   staging buffer is filled (`Staging::held_levels`): the levels whose
//!   title memory is mapped and lies before the start of the next texture
//!   placed in this one's span. The staging buffer holds zeros for the
//!   others, and the step gives tiles by that same answer.
//! - The small levels the card packs into shared tiles are given together
//!   when any of them is held; the upload writes all of them (zeros for
//!   the ones not held).
//! - A shader must not reach a level without memory (it reads zeros: a
//!   black texture). The host view of a slot is made from the first level
//!   that is held and has tiles ([`show_from`], `descriptor/host.rs`
//!   `make`), and the slots of a texture are made again when that level
//!   changes (`HostHeap::texture_views_remade`).
//! - shortcut: tiles are not taken back while the host copy lives. A level
//!   the title lets go of, or that turns out to be the next texture's,
//!   keeps its tiles; it is hidden by the views and written with zeros at
//!   the next upload. Its tiles go back when the host copy does.
//!
//! Only for a texture the title places in memory and fills, two-dimensional,
//! one layer, more than one level, of [`floor`] bytes or more, whose memory
//! is not all its own when the host copy is made (`resource.rs` `hosted`);
//! every other texture is made as before. Off unless
//! `OATHKEEP_LEVELS_ON_DEMAND=1`.

mod pool;
#[cfg(test)]
mod tests;
mod tiled;

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::host;
pub(crate) use tiled::Tiled;

/// The card's tile: 64 KiB.
pub(super) const TILE: u64 = 64 << 10;

/// Tiles in one heap of the pool (64 MiB). The largest level given is a
/// 4096x4096 one of one byte a texel: 256 tiles.
pub(super) const HEAP_TILES: u32 = 1024;

/// Since the start, or now: such textures alive; the bytes all their
/// levels would take; the bytes of the tiles given to them; the bytes of
/// the pool's heaps; such textures made in all; levels given; calls of
/// `UpdateTileMappings`; their microseconds; heaps given back to the card;
/// uploads that left a held level without tiles (the card had no room, or
/// no heap could be had); uploads recorded again for such a level.
pub(crate) static STATS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

/// Whether textures are made this way: the switch
/// (`OATHKEEP_LEVELS_ON_DEMAND=1`; off by default), and a card that reads
/// zeros from, and drops writes to, levels without memory (tiled resources
/// tier 2).
pub(crate) fn wanted() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var_os("OATHKEEP_LEVELS_ON_DEMAND").is_some_and(|v| v == "1") {
            return false;
        }
        let Some(host) = host() else {
            return false;
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: a plain query into a struct of its own size.
        let asked = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        let tier = options.TiledResourcesTier.0;
        let on = asked.is_ok() && tier >= 2;
        tracing::info!(
            tier,
            on,
            floor_kb = floor() >> 10,
            "host copies of title textures take the card's memory level by level (OATHKEEP_LEVELS_ON_DEMAND=1)"
        );
        on
    })
}

pub(super) fn parse_floor(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(1 << 20, |kb| kb.max(64).saturating_mul(1024))
}

/// The bytes from which a texture is made this way: a megabyte, or
/// `OATHKEEP_LEVELS_MIN_KB`. (The accounting of run 189: the levels not
/// held are 1.9 MB a copy on some 1,600 copies; a 512x512 texture has six
/// tiles at most, and most of the calls for nothing.)
pub(crate) fn floor() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| parse_floor(std::env::var("OATHKEEP_LEVELS_MIN_KB").ok().as_deref()))
}

/// The bytes of the tiles given to textures now: the card's memory they
/// hold (`resource.rs` `room` counts it with the other host copies).
pub(crate) fn given_total() -> u64 {
    STATS[2].load(Ordering::Relaxed)
}

/// The bytes of already-allocated pool heaps currently free for reuse without
/// taking additional DXGI local VRAM (`heaps - given`).
pub(crate) fn free_room() -> u64 {
    STATS[3]
        .load(Ordering::Relaxed)
        .saturating_sub(STATS[2].load(Ordering::Relaxed))
}

/// Keeps a two-dimensional shader view (2D or 2D array) from the levels
/// before `first` of a texture of `levels` levels: it starts at `first` if
/// it started before it, shows as many levels fewer (clamped to the
/// remaining levels), and reduces `ResourceMinLODClamp` by the same shift so
/// the effective minimum LOD stays at the same resource level.
pub(crate) fn show_from(desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, first: u32, levels: u32) {
    if first >= levels {
        return;
    }
    let clamp = |most: &mut u32, mips: &mut u32, min_lod: &mut f32| {
        if first <= *most {
            return;
        }
        let fewer = first - *most;
        *most = first;
        if *mips != u32::MAX {
            *mips = mips.saturating_sub(fewer).clamp(1, levels - first);
        }
        if *min_lod > 0.0 {
            *min_lod = (*min_lod - fewer as f32).max(0.0);
        }
    };
    match desc.ViewDimension {
        D3D12_SRV_DIMENSION_TEXTURE2D => {
            // SAFETY: `ViewDimension` selects `Texture2D`.
            let view = unsafe { &mut desc.Anonymous.Texture2D };
            clamp(&mut view.MostDetailedMip, &mut view.MipLevels, &mut view.ResourceMinLODClamp);
        }
        D3D12_SRV_DIMENSION_TEXTURE2DARRAY => {
            // SAFETY: `ViewDimension` selects `Texture2DArray`.
            let view = unsafe { &mut desc.Anonymous.Texture2DArray };
            clamp(&mut view.MostDetailedMip, &mut view.MipLevels, &mut view.ResourceMinLODClamp);
        }
        _ => {}
    }
}

/// For the log, with the texture queue's line: what this saves, and what
/// the mapping calls cost since the line before.
pub(crate) fn status() {
    static BEFORE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    if s[4] == 0 {
        return;
    }
    let calls = s[6] - BEFORE[0].swap(s[6], Ordering::Relaxed);
    let micros = s[7] - BEFORE[1].swap(s[7], Ordering::Relaxed);
    tracing::info!(
        "textures with levels on demand: {} alive ({} made so far), {} MB of the card given to {} levels in all, \
         {} MB of the card not allocated for levels the title does not hold ({} MB if every level had memory); \
         the pool's heaps {} MB ({} given back to the card so far); since the line before: {} calls of \
         UpdateTileMappings in {} microseconds; in all {} calls, {} microseconds; {} uploads left a held \
         level without memory (no room on the card, or no heap), {} uploads were recorded again for one",
        s[0],
        s[4],
        s[2] >> 20,
        s[5],
        s[1].saturating_sub(s[2]) >> 20,
        s[1] >> 20,
        s[3] >> 20,
        s[8],
        calls,
        micros,
        s[6],
        s[7],
        s[9],
        s[10],
    );
}
