//! On the card, without the title: what levels on demand relies on, with
//! the pool and the mapping call of this module on a device of the test's
//! own. One test, one device: the pool is one for the process, and its
//! heaps belong to the device that made them.
//!
//! Run by name, alone, no game needed:
//! `cargo test -p oathkeep-gpu --release -- --ignored levels_on_demand --nocapture`
//!
//! It runs on the SOFTWARE adapter unless `OATHKEEP_TILES_TEST_CARD=1`: on
//! that adapter nothing reaches the card or its driver, so nothing it does
//! can reset the driver or touch the screen. Wherever it runs, the debug
//! layer is switched on if Windows has it (the optional feature "Graphics
//! Tools"), its messages are printed after every step, and the test stops
//! at the first error or lost device with the step's name, before
//! anything else is sent. On the card it does not run without the layer,
//! and no wait is longer than 1.5 s.

mod rig;
mod steps;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::pool::{give_back, take};
use super::tiled::{Given, Tiled};
use super::{HEAP_TILES, STATS, TILE, free_room, parse_floor, show_from};
use crate::host::host;
use rig::{LEVELS, Rig, SIDE, from_level};
use steps::{
    a_card_copy_lands_after_the_give, a_claim_given_up_is_made_again, a_full_card_keeps_new_levels_waiting,
    a_level_on_several_ranges, a_reused_tile_reads_as_written, levels_arrive,
    the_view_starts_at_the_first_level_with_tiles,
};

/// On the software adapter unless `OATHKEEP_TILES_TEST_CARD=1`.
#[test]
#[ignore = "on the card: run it by name with --ignored --nocapture"]
fn levels_on_demand_on_the_card() {
    let Some(mut rig) = Rig::new() else {
        eprintln!("no adapter with tiled resources of tier 2, or no debug layer for a run on the card; skipped");
        return;
    };
    rig.check("the device made");
    levels_arrive(&mut rig, DXGI_FORMAT_BC7_UNORM);
    levels_arrive(&mut rig, DXGI_FORMAT_BC3_UNORM);
    a_card_copy_lands_after_the_give(&mut rig);
    a_reused_tile_reads_as_written(&mut rig);
    a_level_on_several_ranges(&mut rig);
    a_full_card_keeps_new_levels_waiting(&mut rig);
    a_claim_given_up_is_made_again(&mut rig);
    if the_view_starts_at_the_first_level_with_tiles(&mut rig).is_none() {
        eprintln!("the view test's shader or objects could not be made; that part was skipped");
    }
    rig.step = "the end";
    rig.check("every step done");
    eprintln!(
        "all steps passed {}",
        if rig.card { "ON THE CARD" } else { "on the software adapter; OATHKEEP_TILES_TEST_CARD=1 is the run on the card" }
    );
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    eprintln!(
        "{} calls of UpdateTileMappings took {} microseconds in all ({} a call); the pool's heaps {} MB, {} given back",
        s[6],
        s[7],
        s[7] / s[6].max(1),
        s[3] >> 20,
        s[8]
    );
}

#[test]
fn floor_saturates_instead_of_wrapping() {
    assert_eq!(parse_floor(None), 1 << 20);
    assert_eq!(parse_floor(Some("0")), 64 << 10);
    assert_eq!(parse_floor(Some("2048")), 2048 << 10);
    assert_eq!(parse_floor(Some(&(1u64 << 54).to_string())), u64::MAX);
    assert_eq!(parse_floor(Some(&u64::MAX.to_string())), u64::MAX);
}

#[test]
fn show_from_clamps_2d_and_2d_array_and_adjusts_min_lod() {
    let mut srv2d = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: 5768,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 12,
                PlaneSlice: 0,
                ResourceMinLODClamp: 3.5,
            },
        },
    };
    show_from(&mut srv2d, 3, 11);
    // SAFETY: `ViewDimension` is `TEXTURE2D`.
    let v2d = unsafe { srv2d.Anonymous.Texture2D };
    assert_eq!(v2d.MostDetailedMip, 3);
    // Clamped to remaining levels (11 - 3 = 8), not 12 - 3 = 9.
    assert_eq!(v2d.MipLevels, 8);
    assert_eq!(v2d.ResourceMinLODClamp, 0.5);

    let mut srv_arr = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
        Shader4ComponentMapping: 5768,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 6,
                FirstArraySlice: 0,
                ArraySize: 1,
                PlaneSlice: 0,
                ResourceMinLODClamp: 1.5,
            },
        },
    };
    show_from(&mut srv_arr, 2, 11);
    // SAFETY: `ViewDimension` is `TEXTURE2DARRAY`.
    let varr = unsafe { srv_arr.Anonymous.Texture2DArray };
    assert_eq!(varr.MostDetailedMip, 2);
    assert_eq!(varr.MipLevels, 4);
    assert_eq!(varr.ResourceMinLODClamp, 0.0);
}

#[test]
fn card_wrote_failure_does_not_poison_card_mask() {
    let Some(host) = host() else {
        return;
    };
    // A dummy live resource (never mapped because `take()` rejects `HEAP_TILES + 1`).
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = Rig::desc(DXGI_FORMAT_R8G8B8A8_UNORM, 64, 2, false);
    let mut tex: Option<ID3D12Resource> = None;
    // SAFETY: plain texture creation on the host device.
    unsafe {
        host.device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut tex,
            )
            .unwrap();
    }
    let tex = tex.unwrap();
    STATS[0].fetch_add(1, Ordering::Relaxed);
    STATS[1].fetch_add(TILE, Ordering::Relaxed);
    let mut tiled = Tiled {
        // Level 0 demands > HEAP_TILES so `take()` fails deterministically.
        standard: vec![HEAP_TILES + 1, 1],
        packed: 0,
        levels: 2,
        full: TILE,
        given: Mutex::new(Given {
            levels: vec![false, false],
            packed: false,
            runs: Vec::new(),
        }),
        shown: AtomicU32::new(u32::MAX),
        held: AtomicU32::new(0),
        card: AtomicU32::new(0),
        claimed: AtomicU32::new(0),
    };
    assert_eq!(tiled.claim(1 << 0), 1 << 0);
    let changed = tiled.card_wrote_on(host.queue(), &host.device, &tex, 1 << 0);
    assert!(!changed);
    assert_eq!(tiled.card.load(Ordering::Relaxed), 0, "failed tile allocation must not poison `card`");
    assert_eq!(tiled.first_shown(), None);
    assert_eq!(tiled.claim(1 << 0), 1 << 0, "failed level must be unclaimed");
    // Even if level 0 later becomes allocatable, an upload with `room == false`
    // that does not hold level 0 must not treat level 0 as card-owned.
    tiled.standard[0] = 1;
    let (changed, waiting) = tiled.uploaded_on(host.queue(), &host.device, &tex, 1 << 1, false);
    assert!(!changed && waiting);
    assert!(!tiled.has_tiles(0));
    assert_eq!(tiled.given_bytes(), 0);
}

#[test]
fn no_grow_reuses_free_tiles_without_allocating_new_heap() {
    let Some(rig) = Rig::new() else {
        return;
    };
    let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
    // Seed one heap in POOL on this device and release its tiles back to POOL
    // (the last heap stays kept with all HEAP_TILES free).
    {
        let (seed_tex, seed_tiled) = rig.reserved(&desc);
        let (changed, waiting) = seed_tiled.uploaded_on(&rig.queue, &rig.device, &seed_tex, from_level(3), true);
        assert!(changed && !waiting);
    }
    let heaps_before = STATS[3].load(Ordering::Relaxed);
    assert!(free_room() >= (16 << 20), "seeded heap should leave free tiles for reuse");
    let (tex, tiled) = rig.reserved(&desc);
    let (changed, waiting) = tiled.uploaded_on_mode(&rig.queue, &rig.device, &tex, from_level(3), false, true);
    rig.check("no_grow reuse of free tiles");
    assert!(changed && !waiting, "reuse_only must map held levels from existing free tiles");
    assert_eq!(STATS[3].load(Ordering::Relaxed), heaps_before, "no_grow must not allocate a new heap");
    assert!(tiled.given_bytes() > 0);
    // Asking for more tiles than any existing heap has free (while holding some) must fail without growing.
    let (_, idx, runs) = take(&rig.device, 1, true).unwrap();
    assert_eq!(STATS[3].load(Ordering::Relaxed), heaps_before);
    // Requesting all HEAP_TILES cannot fit in the partially occupied heap; no_grow returns None.
    assert!(take(&rig.device, HEAP_TILES, true).is_none());
    assert_eq!(STATS[3].load(Ordering::Relaxed), heaps_before);
    give_back(idx, &runs);
}
