use super::ranges::{BLOCK, Piece, Pool, REST, Ranges, Ripe, SPARE, SPARE_KEPT, STAGING_KEPT};
use super::staging::{staging, staging_block_cap};
use super::{STAGING, TEXTURES, lock, spares, texture};
use windows::Win32::Graphics::Direct3D12::*;

#[test]
fn ranges_are_cut_aligned_and_join_again() {
    let mut r = Ranges::new(1024);
    assert_eq!(r.take(100, 64), Some(0));
    assert_eq!(r.take(100, 64), Some(128));
    assert_eq!(r.take(28, 4), Some(100));
    assert_eq!(r.take(2000, 4), None);
    r.give(128, 100);
    r.give(0, 100);
    assert!(!r.whole(1024));
    r.give(100, 28);
    assert!(r.whole(1024));
}

#[test]
fn empty_blocks_stay_while_the_card_has_room() {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT);
    assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
    assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
    assert_eq!(spares(None, true), SPARE);
    // 8 GiB GPU (~7,161 MiB budget) stays compact and releases all empty blocks at >= 85%.
    let b8 = 7161 * MIB;
    assert_eq!(spares(Some([b8, 4000 * MIB, 0, 0]), true), 4);
    assert_eq!(spares(Some([b8, 5800 * MIB, 0, 0]), true), 1);
    assert_eq!(spares(Some([b8, 6100 * MIB, 0, 0]), true), 0);
    assert_eq!(spares(Some([b8, 4000 * MIB, 0, 0]), false), 1);
    // 16 GiB GPU retains SPARE_KEPT below 85%.
    assert_eq!(spares(Some([16 * GIB, 10 * GIB, 0, 0]), true), SPARE_KEPT);
}

#[test]
fn a_failed_block_is_not_retried_for_each_small_upload() {
    let mut pool: Pool<u32> = Pool::new(2);
    let calls = std::cell::Cell::new(0);
    let refuse = || { calls.set(calls.get() + 1); None };
    for size in [704008, 179720, 179720, 24080, 48648, 524288, 89616, 351760] {
        assert!(pool.take(size, 512, 12, refuse).is_none());
    }
    assert_eq!(calls.get(), 1);
    assert!(pool.take(24080, 512, 1012, || Some(1)).is_some());
}

#[test]
fn budget_tick_reclaims_idle_blocks_without_reusing_live_or_resting_pieces() {
    let mut pool = Pool::new(STAGING_KEPT);
    let (held, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(1)).unwrap();
    let (resting, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(2)).unwrap();
    pool.give(resting, 0);
    for i in 0..12 {
        pool.add(i, 0);
    }
    pool.reclaim(0, SPARE_KEPT, true);
    assert_eq!(pool.blocks(), STAGING_KEPT + 1);
    pool.reclaim(REST - 1, SPARE, true);
    assert_eq!(pool.blocks(), SPARE + 1);
    assert_eq!(pool.resting.len(), 1);
    pool.reclaim(REST, 0, true);
    assert_eq!(pool.blocks(), 1);
    assert!(pool.resting.is_empty());
    pool.give(held, REST);
    pool.reclaim(2 * REST, 0, true);
    assert_eq!(pool.blocks(), 0);
}

#[test]
fn pieces_never_overlap_and_rest_before_they_are_used_again() {
    let mut pool: Pool<u32> = Pool::new(SPARE);
    let made = std::cell::Cell::new(0);
    let make = || {
        made.set(made.get() + 1);
        Some(made.get())
    };
    let mut taken: Vec<Piece> = Vec::new();
    for i in 0..500u64 {
        let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
        assert!(!again);
        assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
        taken.push(p);
    }
    let blocks = pool.blocks();
    assert!(blocks >= 2);
    // Given back at 10: not used again before 10 + REST.
    let first = taken[0];
    pool.give(first, 10);
    let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
    assert_ne!((p.block, p.start), (first.block, first.start));
    let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
    assert_eq!((q.block, q.start, again), (first.block, first.start, true));
    // Everything back: the empty blocks past the spare ones go.
    for p in taken.drain(1..).chain([p, q]) {
        pool.give(p, 20);
    }
    pool.rested(20 + REST);
    assert_eq!(pool.blocks(), blocks.min(SPARE));
    // Too big for a block, and a block that cannot be made.
    assert!(pool.take(BLOCK, 65536, 30, make).is_none());
    let mut none: Pool<u32> = Pool::new(SPARE);
    assert!(none.take(65536, 65536, 0, || None).is_none());
}

#[test]
fn pieces_come_from_blocks_older_than_the_lists_that_run() {
    let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
    let made = std::cell::Cell::new(0);
    let make = || {
        made.set(made.get() + 1);
        Some(made.get())
    };
    let at = |started, finished| Ripe { started, finished, ahead: 2 };
    let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
    // Nothing runs: at once, and two are made ahead, one a time.
    assert_eq!(take(&mut pool, at(0, 0)), 1);
    assert_eq!(made.get(), 2);
    assert_eq!(take(&mut pool, at(0, 0)), 1);
    assert_eq!(made.get(), 3);
    // 4 is made while submission 5 runs.
    assert_eq!(take(&mut pool, at(5, 4)), 2);
    assert_eq!(made.get(), 4);
    assert_eq!(take(&mut pool, at(5, 4)), 2);
    assert_eq!(take(&mut pool, at(6, 4)), 3);
    assert_eq!(take(&mut pool, at(6, 4)), 3);
    // None is ripe: the oldest; it is ripe once 5 has finished.
    assert_eq!(take(&mut pool, at(7, 4)), 4);
    assert_eq!(take(&mut pool, at(7, 5)), 4);
    // A whole block's worth too.
    assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1, 5);
    // Off: the first with room, whatever its age, none ahead.
    let n = made.get();
    assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
    assert_eq!(made.get(), n);
    assert!(pool.take(BLOCK, 65536, 0, make).is_none());
}

/// A pooled texture's piece comes back when its last reference goes, not
/// before; its staging piece begins as zeros.
#[test]
fn a_texture_gives_its_piece_back_when_it_goes() {
    let Some(host) = crate::host::host() else { return };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 256,
        Height: 256,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    // SAFETY: a valid description.
    let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
    assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
    let resting = || lock(&TEXTURES).resting.len();
    let before = resting();
    let other = t.clone();
    drop(t);
    assert_eq!(resting(), before);
    drop(other);
    assert_eq!(resting(), before + 1);
    // SAFETY: the piece's mapping, 4096 bytes of it ours.
    unsafe {
        let (piece, _, at, data) = staging(host, 4096).unwrap();
        assert!(at.is_multiple_of(512));
        std::ptr::write_bytes(data, 0xAB, 4096);
        drop(piece);
        lock(&STAGING).rested(u64::MAX);
        let (_piece, _, _, data) = staging(host, 4096).unwrap();
        assert!((0..4096).all(|i| *data.add(i) == 0));
        // SAFETY: plain query.
        assert!(host.device.GetDeviceRemovedReason().is_ok());
    }
}

#[test]
fn no_grow_reuses_free_hole_and_never_calls_make() {
    let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
    let calls = std::cell::Cell::new(0u32);
    let mut make = || {
        calls.set(calls.get() + 1);
        Some(calls.get())
    };
    let (a, _, _) = pool.take(BLOCK / 4, 65536, 0, &mut make).unwrap();
    let (b, _, _) = pool.take(BLOCK / 4, 65536, 0, &mut make).unwrap();
    assert_eq!(calls.get(), 1);
    pool.give(a, 10);
    // Under NO_GROW (max_blocks = Some(0)), reuse the rested hole with 0 make() calls.
    let (reused, heap, again) = pool
        .take_ripe_capped(BLOCK / 4, 65536, 10 + REST, Ripe { started: 5, finished: 0, ahead: 4 }, Some(0), &mut make)
        .unwrap();
    assert_eq!(heap, 1);
    assert!(again);
    assert_eq!(reused.start, a.start);
    assert_eq!(calls.get(), 1, "no_grow must not allocate ahead or grow");
    // Fill remaining half, then ask for another slice under NO_GROW -> None, 0 make() calls, no backoff.
    let _ = pool.take_capped(BLOCK / 2, 65536, 10 + REST, Some(0), &mut make).unwrap();
    assert!(pool.take_capped(65536, 65536, 10 + REST, Some(0), &mut make).is_none());
    assert_eq!(calls.get(), 1);
    // Normal growth immediately afterwards is NOT blocked by backoff.
    let (_, heap2, _) = pool.take(65536, 65536, 10 + REST, &mut make).unwrap();
    assert_eq!(heap2, 2);
    assert_eq!(calls.get(), 2);
    let _ = b;
}

#[test]
fn staging_cap_limits_blocks_on_limited_ram_without_blocking_reuse() {
    assert_eq!(staging_block_cap(false, true, false), None);
    assert_eq!(staging_block_cap(true, true, false), Some(5));
    assert_eq!(staging_block_cap(true, false, true), Some(6));
    assert_eq!(staging_block_cap(true, false, false), Some(6));

    let mut pool: Pool<u32> = Pool::new(STAGING_KEPT);
    let calls = std::cell::Cell::new(0u32);
    let mut make = || {
        calls.set(calls.get() + 1);
        Some(calls.get())
    };
    let mut pieces = Vec::new();
    // Fill 5 blocks (2 x BLOCK/2 each = 10 pieces).
    for _ in 0..10 {
        let (p, _, _) = pool.take_capped(BLOCK / 2, 512, 0, Some(5), &mut make).unwrap();
        pieces.push(p);
    }
    assert_eq!(pool.blocks(), 5);
    assert_eq!(calls.get(), 5);
    // 11th speculative piece at cap 5 fails without calling make() or arming backoff.
    assert!(pool.take_capped(65536, 512, 0, Some(5), &mut make).is_none());
    assert_eq!(calls.get(), 5);
    // Baking (cap 6) can immediately allocate block 6 at the same timestamp.
    let (_, b6, _) = pool.take_capped(BLOCK / 2, 512, 0, Some(6), &mut make).unwrap();
    assert_eq!(b6, 6);
    assert_eq!(pool.blocks(), 6);
    // Releasing a piece lets speculative cap 5 reuse it once rested without growing.
    pool.give(pieces[0], 10);
    let (reused, _, again) = pool.take_capped(BLOCK / 2, 512, 10 + REST, Some(5), &mut make).unwrap();
    assert!(again);
    assert_eq!(reused.block, pieces[0].block);
    assert_eq!(pool.blocks(), 6);
}
