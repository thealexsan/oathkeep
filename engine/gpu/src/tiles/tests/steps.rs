use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use super::super::show_from;
use super::super::pool::{give_back, take};
use super::rig::{LEVELS, LOOK, Rig, SIDE, all, from_level, placed, sub};
use crate::host::{compile, transition};

/// (i) Levels arrive one by one: after each step every level with
/// tiles reads back its own bytes and every level without reads zeros.
pub(super) fn levels_arrive(rig: &mut Rig, format: DXGI_FORMAT) {
    rig.step = if format == DXGI_FORMAT_BC7_UNORM { "step 1, levels arrive (BC7)" } else { "step 1, levels arrive (BC3)" };
    let desc = Rig::desc(format, SIDE, LEVELS, true);
    let (texture, tiled) = rig.reserved(&desc);
    // (The card packs the levels smaller than a tile into shared
    // tiles; the software adapter may give each its own.)
    eprintln!(
        "format {}: {} levels with tiles of their own ({:?} tiles), {} tiles for the packed ones",
        format.0,
        tiled.standard.len(),
        tiled.standard,
        tiled.packed
    );
    assert!(!rig.card || tiled.packed > 0, "on the card a 1024 texture has levels smaller than a tile");
    let layout = rig.footprints(&desc);
    let upload = rig.filled(&layout, |level| level as u8 + 1);
    for first in [3u32, 1, 0] {
        // Ahead of the list on the queue, as at a submission.
        rig.give(&tiled, &texture, from_level(first));
        let read = rig.round_trip(&texture, &layout, Some(&upload));
        for (level, bytes) in read.iter().enumerate() {
            let want = if tiled.has_tiles(level as u32) { level as u8 + 1 } else { 0 };
            assert!(all(bytes, want), "format {} from level {first}: level {level} reads {bytes:?}, wanted {want}", format.0);
        }
    }
    assert!(tiled.whole());
    assert_eq!(tiled.first_shown(), Some(0));
}

/// (ii) A copy on the card into a level without tiles is dropped; the
/// same copy in a list that runs after the level was given lands.
pub(super) fn a_card_copy_lands_after_the_give(rig: &mut Rig) {
    rig.step = "step 2, a copy on the card into a level without tiles";
    let format = DXGI_FORMAT_BC7_UNORM;
    let desc = Rig::desc(format, SIDE, LEVELS, true);
    let (texture, tiled) = rig.reserved(&desc);
    let layout = rig.footprints(&desc);
    // The last level with tiles of its own, and a texture of its size
    // with every byte 0x5a to copy from.
    let level = tiled.standard.len() as u32 - 1;
    let source_desc = Rig::desc(format, SIDE >> level, 1, false);
    let source_layout = rig.footprints(&source_desc);
    let source_bytes = rig.filled(&source_layout, |_| 0x5a);
    let mut made: Option<ID3D12Resource> = None;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    // SAFETY: plain texture creation.
    unsafe {
        rig.device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &source_desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made)
            .unwrap();
    }
    let source = made.unwrap();
    rig.run(|list| {
        // SAFETY: an open list and live resources.
        unsafe {
            list.CopyTextureRegion(&sub(&source, 0), 0, 0, 0, &placed(&source_bytes, source_layout.footprints[0]), None);
            list.ResourceBarrier(&[transition(&source, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            // No tiles yet: this copy goes nowhere.
            list.CopyTextureRegion(&sub(&texture, level), 0, 0, 0, &sub(&source, 0), None);
        }
    });
    let before = rig.round_trip(&texture, &layout, None);
    assert!(all(&before[level as usize], 0), "a copy into a level without tiles left {:?}", before[level as usize]);
    // As `texture.rs` `card_writes` has it: the give at the list's
    // submission, the copy in the list.
    assert_ne!(tiled.claim(tiled.with_level(level)), 0, "the level is new");
    rig.give(&tiled, &texture, tiled.with_level(level));
    rig.run(|list| {
        // SAFETY: as above.
        unsafe { list.CopyTextureRegion(&sub(&texture, level), 0, 0, 0, &sub(&source, 0), None) };
    });
    let after = rig.round_trip(&texture, &layout, None);
    assert!(all(&after[level as usize], 0x5a), "the copy after the give left {:?}", after[level as usize]);
    assert_eq!(tiled.claim(tiled.with_level(level)), 0, "claimed once");
}

/// (iii) Tiles a texture gave back go to the next one, which reads
/// what was written into it and zeros where it has no tiles; what a
/// level given and not written reads is printed (the reason no level
/// is given outside a list that writes it).
pub(super) fn a_reused_tile_reads_as_written(rig: &mut Rig) {
    rig.step = "step 3, a reused tile";
    let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
    let layout = rig.footprints(&desc);
    let (first, first_tiles) = rig.reserved(&desc);
    let level = first_tiles.standard.len() as u32 - 1;
    rig.give(&first_tiles, &first, 1 << level);
    let upload = rig.filled(&layout, |_| 0xa5);
    let read = rig.round_trip(&first, &layout, Some(&upload));
    assert!(all(&read[level as usize], 0xa5));
    let runs = first_tiles.given.lock().unwrap().runs.clone();
    drop(first_tiles);
    drop(first);
    let (second, second_tiles) = rig.reserved(&desc);
    rig.give(&second_tiles, &second, 1 << level);
    let reused = second_tiles.given.lock().unwrap().runs == runs;
    let unwritten = rig.round_trip(&second, &layout, None);
    eprintln!(
        "a level given tiles and not written reads {:?} (its tiles are the first texture's: {reused})",
        &unwritten[level as usize][0][..4]
    );
    let upload = rig.filled(&layout, |_| 0x3c);
    let read = rig.round_trip(&second, &layout, Some(&upload));
    assert!(all(&read[level as usize], 0x3c), "the reused level reads {:?}", read[level as usize]);
    assert!(all(&read[0], 0), "level 0 has no tiles and reads {:?}", read[0]);
}

/// (iv) A level whose tiles come from several runs of the heap (single
/// free tiles between taken ones) reads as written in every tile.
pub(super) fn a_level_on_several_ranges(rig: &mut Rig) {
    rig.step = "step 4, a level on several ranges of the heap";
    // Three single tiles; the first and the last go back, the middle
    // one stays taken.
    let singles: Vec<_> = (0..3).map(|_| take(&rig.device, 1, false).expect("a tile")).collect();
    give_back(singles[0].1, &singles[0].2);
    give_back(singles[2].1, &singles[2].2);
    let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
    let layout = rig.footprints(&desc);
    let (texture, tiled) = rig.reserved(&desc);
    // Level 1 of a 1024 texture: four tiles.
    assert!(tiled.standard.len() > 1 && tiled.standard[1] >= 2);
    rig.give(&tiled, &texture, 1 << 1);
    let runs = tiled.given.lock().unwrap().runs.len();
    eprintln!("level 1 ({} tiles) was mapped on {runs} ranges", tiled.standard[1]);
    assert!(runs >= 2, "the single free tiles were not used first");
    let upload = rig.filled(&layout, |_| 0x77);
    let read = rig.round_trip(&texture, &layout, Some(&upload));
    assert!(all(&read[1], 0x77), "level 1 reads {:?}", read[1]);
    give_back(singles[1].1, &singles[1].2);
}

/// (v) A view made by [`show_from`] of a texture whose level 0 has no
/// tiles returns level 1's colour for a sample at level 0; the view as
/// the title asked returns zero.
pub(super) fn the_view_starts_at_the_first_level_with_tiles(rig: &mut Rig) -> Option<()> {
    rig.step = "step 5, the view from the first level with tiles";
    let cs = compile(LOOK, s!("look"), s!("cs_5_1")).ok()?;
    let format = DXGI_FORMAT_R8G8B8A8_UNORM;
    let desc = Rig::desc(format, SIDE, LEVELS, true);
    let layout = rig.footprints(&desc);
    let (texture, tiled) = rig.reserved(&desc);
    // Every level but the largest; each level's bytes are sixteen
    // times its number plus one.
    rig.give(&tiled, &texture, from_level(1));
    assert_eq!(tiled.first_shown(), Some(1));
    let upload = rig.filled(&layout, |level| (level as u8 + 1) * 16);
    rig.round_trip(&texture, &layout, Some(&upload));
    // SAFETY: plain object creation; the views are made in a heap
    // that lives to the end of the function, of a live texture.
    let (root, pipeline, views, words, back) = unsafe {
        let root: ID3D12RootSignature = rig.device.CreateRootSignature(0, &cs[..]).ok()?;
        let pipeline: ID3D12PipelineState = rig
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: cs.as_ptr().cast(),
                    BytecodeLength: cs.len(),
                },
                ..Default::default()
            })
            .ok()?;
        let views: ID3D12DescriptorHeap = rig
            .device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .ok()?;
        let as_asked = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: 5768,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: u32::MAX,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 0.0,
                },
            },
        };
        let mut shown = as_asked;
        show_from(&mut shown, tiled.first_shown()?, u32::from(LEVELS));
        assert_eq!(shown.Anonymous.Texture2D.MostDetailedMip, 1);
        let step = rig.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        let mut at = views.GetCPUDescriptorHandleForHeapStart();
        rig.device.CreateShaderResourceView(&texture, Some(&raw const shown), at);
        at.ptr += step;
        rig.device.CreateShaderResourceView(&texture, Some(&raw const as_asked), at);
        let words = rig.buffer(
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            256,
        );
        let back = rig.buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST, 256);
        (root, pipeline, views, words, back)
    };
    // Before the one dispatch: the pipeline and both views are sound.
    rig.check("the pipeline and the views made");
    rig.run(|list| {
        // SAFETY: an open list and live objects.
        unsafe {
            list.ResourceBarrier(&[transition(&texture, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);
            list.SetDescriptorHeaps(&[Some(views.clone())]);
            list.SetComputeRootSignature(&root);
            list.SetPipelineState(&pipeline);
            list.SetComputeRootDescriptorTable(0, views.GetGPUDescriptorHandleForHeapStart());
            list.SetComputeRootUnorderedAccessView(1, words.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&[transition(&words, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            list.CopyBufferRegion(&back, 0, &words, 0, 16);
        }
    });
    // SAFETY: a read-back buffer the finished list copied sixteen bytes into.
    let read = unsafe {
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: 16 };
        back.Map(0, Some(&range), Some(&mut data)).unwrap();
        let read = std::slice::from_raw_parts(data.cast::<u32>(), 2).to_vec();
        back.Unmap(0, None);
        read
    };
    eprintln!("a sample at level 0: {} through the view from level 1, {} through the view as asked", read[0], read[1]);
    assert_eq!(read[0], 32, "the view from the first level with tiles reads level 1");
    assert_eq!(read[1], 0, "the view from level 0 reads a level without tiles");
    Some(())
}

pub(super) fn a_full_card_keeps_new_levels_waiting(rig: &mut Rig) {
    rig.step = "step 6, no room on the card: held levels wait, and get their tiles when asked again";
    let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
    let (texture, tiled) = rig.reserved(&desc);
    let layout = rig.footprints(&desc);
    let upload = rig.filled(&layout, |level| level as u8 + 1);
    let read_as_given = |rig: &mut Rig, what: &str| {
        let read = rig.round_trip(&texture, &layout, Some(&upload));
        for (level, bytes) in read.iter().enumerate() {
            let want = if tiled.has_tiles(level as u32) { level as u8 + 1 } else { 0 };
            assert!(all(bytes, want), "{what}: level {level} reads {bytes:?}, wanted {want}");
        }
    };
    // No room: nothing is given, the held levels are said to wait,
    // nothing is shown, every level reads zeros.
    let (changed, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(3), false);
    rig.check("an upload without room");
    assert!(!changed && waiting, "without room: changed {changed}, waiting {waiting}");
    assert_eq!(tiled.given_bytes(), 0, "without room no tile is given");
    assert_eq!(tiled.first_shown(), None);
    read_as_given(rig, "without room");
    // Asked again with room: the same levels get theirs.
    let (changed, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(3), true);
    rig.check("the upload again, with room");
    assert!(changed && !waiting, "with room: changed {changed}, waiting {waiting}");
    assert!(tiled.has_tiles(3) && !tiled.has_tiles(2));
    assert_eq!(tiled.first_shown(), Some(3));
    read_as_given(rig, "with room");
    // More levels held and no room again: the ones that have tiles
    // are brought up to date, the new ones wait, the view stays.
    let before = tiled.given_bytes();
    let (changed, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(1), false);
    rig.check("more levels held, without room");
    assert!(!changed && waiting, "more levels without room: changed {changed}, waiting {waiting}");
    assert_eq!(tiled.given_bytes(), before);
    assert_eq!(tiled.first_shown(), Some(3));
    read_as_given(rig, "more levels without room");
    // The card's own write is not kept waiting, and brings only its
    // own level: level 1 still waits although it is held.
    assert_eq!(tiled.claim(1 << 0), 1 << 0);
    let changed = tiled.card_wrote_on(&rig.queue, &rig.device, &texture, 1 << 0);
    rig.check("the card's own write, without room");
    assert!(changed && tiled.has_tiles(0) && !tiled.has_tiles(1) && !tiled.has_tiles(2));
    assert_eq!(tiled.first_shown(), Some(0));
    assert_eq!(tiled.claim(1 << 0), 0, "a level with tiles is not claimed again");
    // With room everything held gets its tiles.
    let (_, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(1), true);
    rig.check("the last upload, with room");
    assert!(!waiting && tiled.whole());
    read_as_given(rig, "everything given");
}

pub(super) fn a_claim_given_up_is_made_again(rig: &mut Rig) {
    rig.step = "step 7, a claim whose zeros never landed is given up";
    let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
    let (_texture, tiled) = rig.reserved(&desc);
    let level = tiled.with_level(2);
    assert_eq!(tiled.claim(level), level, "the level is new");
    assert_eq!(tiled.claim(level), 0, "claimed once");
    // The list that claimed it was reset without being submitted.
    tiled.unclaim(level);
    assert_eq!(tiled.claim(level), level, "claimed again after it was given up");
    // The small levels the card packs together are claimed together.
    let small = tiled.with_level(u32::from(LEVELS) - 1);
    assert!(!rig.card || small.count_ones() > 1, "on the card a 1024 texture has several levels in one tile");
    assert_eq!(tiled.claim(1 << (u32::from(LEVELS) - 1)), 1 << (u32::from(LEVELS) - 1));
    tiled.unclaim(small);
    assert_eq!(tiled.claim(small), small);
}
