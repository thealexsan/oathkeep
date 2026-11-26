use super::{
    WINDOW, cover, default_card_limits_mb, grid, holder, longest_tail, resolve_card_limits,
    tail_of, window_of,
};

#[test]
fn card_limits_scale_with_local_vram_budget() {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    // Unknown (WARP / query failure) and < 6 GiB keep the 16 MiB floor;
    // 6..=12 GiB (8–12 GB cards) use 1 MiB min and 640 MiB cap.
    assert_eq!(default_card_limits_mb(None), (16, 512));
    assert_eq!(default_card_limits_mb(Some(4 * GIB)), (16, 512));
    assert_eq!(default_card_limits_mb(Some(7 * GIB)), (1, 640));
    assert_eq!(default_card_limits_mb(Some(8 * GIB)), (1, 640));
    assert_eq!(default_card_limits_mb(Some(11 * GIB)), (1, 640));
    assert_eq!(default_card_limits_mb(Some(12 * GIB)), (1, 640));

    // 16 GB tier (> 12 GiB, < 20 GiB), including WDDM budgets around 13.5–15 GiB.
    assert_eq!(default_card_limits_mb(Some(13 * GIB)), (1, 2048));
    assert_eq!(default_card_limits_mb(Some(14 * GIB)), (1, 2048));
    assert_eq!(default_card_limits_mb(Some(15 * GIB)), (1, 2048));
    assert_eq!(default_card_limits_mb(Some(16 * GIB)), (1, 2048));

    // 24–32+ GB tier (>= 20 GiB): 1 MiB min, 25% of budget capped at 8192 MiB.
    assert_eq!(default_card_limits_mb(Some(20 * GIB)), (1, 5120));
    assert_eq!(default_card_limits_mb(Some(24 * GIB)), (1, 6144));
    assert_eq!(default_card_limits_mb(Some(32 * GIB)), (1, 8192));
    assert_eq!(default_card_limits_mb(Some(48 * GIB)), (1, 8192));

    // Explicit env overrides always win, clamp min to >= 1 MiB, and saturate on overflow.
    assert_eq!(resolve_card_limits(Some(32 * GIB), Some(8), Some(4096)), (8 * MIB, 4096 * MIB));
    assert_eq!(resolve_card_limits(Some(12 * GIB), Some(0), Some(1024)), (MIB, 1024 * MIB));
    assert_eq!(resolve_card_limits(None, Some(u64::MAX), Some(u64::MAX)), (u64::MAX, u64::MAX));
    assert_eq!(resolve_card_limits(None, None, None), (16 * MIB, 512 * MIB));
}

/// An ask that may take any window takes the longest that holds all of
/// it, and none that holds only a part.
#[test]
fn an_ask_takes_the_longest_window_that_holds_it() {
    let cell = 0xd_1200_0000u64;
    let (ask, end) = (cell + 0x9_0000, cell + 0xb_0000);
    assert_eq!(holder([].into_iter(), ask, end), None);
    let there = [
        (cell, 0x4_0000),            // ends below the ask
        (cell + 0x8_0000, 0x2_0000), // holds its first half only
        (cell + 0x8_0000, 0x4_0000), // holds it
        (cell + 0x4_0000, 0x10_0000), // holds it, longer
        (cell + 0xa_0000, 0x40_0000), // begins above it
    ];
    assert_eq!(holder(there.into_iter(), ask, end), Some((cell + 0x4_0000, 0x10_0000)));
    // Of two as long, the one that begins lower.
    let two = [(cell + 0x8_0000, 0x8_0000), (cell + 0x4_0000, 0x8_0000)];
    assert_eq!(holder(two.into_iter(), ask, end), Some((cell + 0x4_0000, 0x8_0000)));
}

/// What windows lie over counts each byte once.
#[test]
fn overlapping_windows_cover_their_memory_once() {
    assert_eq!(cover([].into_iter()), 0);
    let apart = [(0x10_0000u64, 0x4_0000u64), (0x20_0000, 0x4_0000)];
    assert_eq!(cover(apart.into_iter()), 0x8_0000);
    // One inside another, one overlapping its end, one touching.
    let over = [
        (0x10_0000u64, 0x10_0000u64),
        (0x14_0000, 0x4_0000),
        (0x1c_0000, 0x8_0000),
        (0x24_0000, 0x4_0000),
    ];
    assert_eq!(cover(over.into_iter()), 0x18_0000);
}

/// A tail runs from the ask's page to the end of its 4 MiB cell, and an
/// ask takes the longest tail there is that holds it.
#[test]
fn tails_run_to_the_cells_end_and_the_longest_is_taken() {
    let cell = 0xd_1200_0000u64;
    let ask = cell + 0x12_3456;
    assert_eq!(tail_of(ask, ask + 1), (cell + 0x10_0000, WINDOW - 0x10_0000));
    // The cell's first page: the whole cell, the window there always was.
    assert_eq!(tail_of(cell + 0xffff, cell + 0x1_0000), (cell, WINDOW));
    let end = cell + WINDOW;
    let tail = |from: u64| (cell + from, WINDOW - from);
    // Nothing there, or only tails that begin above the ask.
    assert_eq!(longest_tail([].into_iter(), ask, ask + 1), None);
    assert_eq!(longest_tail([tail(0x20_0000)].into_iter(), ask, ask + 1), None);
    // The longest of those that hold the ask; windows that do not reach
    // the cell's end (cut on a finer grid for asks with a length) and
    // windows of other cells are not tails.
    let there = [
        (cell - WINDOW, WINDOW),
        (cell, 0x4_0000),
        tail(0x8_0000),
        tail(0x10_0000),
        (cell + 0x10_0000, 0x4_0000),
        tail(0x20_0000),
    ];
    assert_eq!(longest_tail(there.into_iter(), ask, ask + 1), Some(tail(0x8_0000)));
    let (start, bytes) = tail(0x8_0000);
    assert!(start <= ask && start + bytes == end);
    // The whole cell's window is the longest tail of all.
    let with_whole = [(cell, WINDOW), tail(0x8_0000)];
    assert_eq!(longest_tail(with_whole.into_iter(), ask, ask + 1), Some((cell, WINDOW)));
}

/// The setting takes powers of two from 64 KiB to 4 MiB; anything else
/// falls back to the RAM-tiered default grid (512 KiB on <= 20 GiB,
/// 1 MiB on <= 32 GiB, 2 MiB on > 32 GiB).
#[test]
fn the_grid_is_a_power_of_two_or_the_default() {
    use super::{grid_for, grid_for_ram, longest_tail_to, superseded_window_flags, tail_bounds};
    assert_eq!(grid(None), 512 << 10);
    assert_eq!(grid_for_ram(None, true, true), 512 << 10);
    assert_eq!(grid_for(None, true), 1 << 20);
    assert_eq!(grid_for(None, false), 2 << 20);
    assert_eq!(grid_for(Some(4096), true), WINDOW);
    assert_eq!(grid_for(Some(1024), true), 1 << 20);
    assert_eq!(grid_for(Some(512), false), 512 << 10);
    assert_eq!(grid_for(Some(256), true), 256 << 10);
    assert_eq!(grid_for(Some(64), true), 64 << 10);
    for bad in [0, 32, 100, 1000, 8192] {
        assert_eq!(grid_for_ram(Some(bad), true, true), 512 << 10, "{bad}");
        assert_eq!(grid_for(Some(bad), true), 1 << 20, "{bad}");
        assert_eq!(grid_for(Some(bad), false), 2 << 20, "{bad}");
    }

    // Private VAD bounds clamp tail start/end to 64 KiB pages within the
    // allocation and let unsized asks reuse existing sized windows covering it.
    let cell = 0x2000_0000u64;
    let vad = Some((cell + 0x2_0000, cell + 0x8_0000));
    let ask = cell + 0x3_1000;
    let (step, reach) = tail_bounds(ask, ask + 1, vad);
    assert_eq!((step, reach), (cell + 0x2_0000, cell + 0x8_0000));
    assert!(step <= ask && reach > ask);
    let sized_512k = [(cell, 0x8_0000u64)];
    assert_eq!(
        longest_tail_to(sized_512k.into_iter(), cell, step, reach),
        Some((cell, 0x8_0000))
    );

    // Superseded sub-windows are detected in linear sorted order, whether
    // covered by an earlier start or a longer window at the same start.
    let keys = [
        (cell, 0x8_0000),
        (cell, 0x10_0000),
        (cell + 0x4_0000, 0x8_0000),
        (cell + 0x20_0000, 0x8_0000),
    ];
    assert_eq!(superseded_window_flags(&keys), vec![true, false, true, false]);
}

#[test]
fn private_vad_bounds_detects_virtual_alloc_extent_including_reserved_tail() {
    use super::{WINDOW, private_vad_bounds, tail_bounds};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    // Reserve 384 KiB (`MEM_PRIVATE`), commit the first 128 KiB, leave the
    // trailing 256 KiB `MEM_RESERVE` under the same `AllocationBase`.
    let total = 384 * 1024;
    let committed = 128 * 1024;
    // SAFETY: standard Win32 private reservation + commit, released before return.
    let base = unsafe { VirtualAlloc(std::ptr::null(), total, MEM_RESERVE, PAGE_READWRITE) };
    assert!(!base.is_null());
    // SAFETY: committing inside `base`.
    let comm = unsafe { VirtualAlloc(base, committed, MEM_COMMIT, PAGE_READWRITE) };
    assert_eq!(comm, base);
    let addr = base as u64 + 0x4000;
    let cap = (addr & !(WINDOW - 1)) + WINDOW;
    let vad = private_vad_bounds(addr, cap);
    // SAFETY: releasing our test reservation.
    unsafe {
        VirtualFree(base, 0, MEM_RELEASE);
    }
    let expected_end = (base as u64 + total as u64).min(cap);
    assert_eq!(vad, Some((base as u64, expected_end)));
    let (step, reach) = tail_bounds(addr, addr + 1, vad);
    assert!(step >= (base as u64 & !0xffff) && step <= addr);
    assert_eq!(reach, expected_end.next_multiple_of(1 << 16).min(cap));
}

/// A window holds its ask and is cut on the grid; on the default grid
/// it is the window there always was.
#[test]
fn windows_hold_their_ask_on_the_grid() {
    let (address, end) = (0xd_1234_5678u64, 0xd_1234_5678u64 + 0x9000);
    assert_eq!(window_of(address, end, WINDOW), (0xd_1200_0000, 4 << 20));
    assert_eq!(window_of(address, end, 1 << 20), (0xd_1230_0000, 1 << 20));
    assert_eq!(window_of(address, end, 256 << 10), (0xd_1234_0000, 256 << 10));
    // An ask across a grid line takes both cells.
    let end = address + 0x4_0000;
    assert_eq!(window_of(address, end, 256 << 10), (0xd_1234_0000, 512 << 10));
    for g in [64u64 << 10, 256 << 10, 1 << 20, WINDOW] {
        let (start, len) = window_of(address, end, g);
        assert!(start <= address && start + len >= end);
        assert!(start % g == 0 && len % g == 0);
    }
}

#[test]
fn cbv_slab_sizing_clamps_and_aligns() {
    use super::{CBV_SLAB_MAX_SLOT, cbv_slab_sizes};
    assert_eq!(cbv_slab_sizes(0, 256), None);
    assert_eq!(cbv_slab_sizes(0x1000, 0), None);
    assert_eq!(cbv_slab_sizes(u64::MAX - 8, 64), None);
    assert_eq!(cbv_slab_sizes(0x1010, 1), Some((1, 256)));
    assert_eq!(cbv_slab_sizes(0x1090, 320), Some((320, 512)));
    assert_eq!(cbv_slab_sizes(0x1090, CBV_SLAB_MAX_SLOT), Some((CBV_SLAB_MAX_SLOT, CBV_SLAB_MAX_SLOT)));
    // Oversized CBVs clamp to the 64 KiB D3D12 CBV maximum.
    assert_eq!(cbv_slab_sizes(0x1090, 969_472), Some((CBV_SLAB_MAX_SLOT, CBV_SLAB_MAX_SLOT)));
}

#[test]
fn exact_range_allows_arbitrary_byte_alignment() {
    use super::exact_range_valid;
    assert!(!exact_range_valid(0, 16));
    assert!(!exact_range_valid(0x1000, 0));
    assert!(!exact_range_valid(0x1000, (64 << 20) + 1));
    assert!(!exact_range_valid(u64::MAX - 4, 16));
    // 2-byte aligned offset=6 (e.g. R32/R16G16 view on unaligned base) is accepted.
    assert!(exact_range_valid(0x10006, 40));
    assert!(exact_range_valid(0x10002, 16));
    assert!(exact_range_valid(0x10008, 128));
}

#[test]
fn aligned_cbv_slab_suballocates_seeds_and_reuses_safely() {
    use super::{find_aligned_cbv, find_aligned_cbv_existing};
    if crate::host::host().is_none() {
        return; // no host GPU
    }
    let mut backing = vec![0u8; 8192];
    for (i, b) in backing.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(13).wrapping_add(7);
    }
    let page = (backing.as_ptr() as u64).next_multiple_of(4096);
    let addr_a = page + 0x90;
    let addr_b = page + 0x110;
    let sa = find_aligned_cbv(addr_a, 300).expect("aligned CBV shadow A");
    let sb = find_aligned_cbv(addr_b, 200).expect("aligned CBV shadow B");
    assert!(sa.can_bind_cbv(addr_a, 300));
    assert!(sb.can_bind_cbv(addr_b, 200));
    assert_eq!(sa.gpu(addr_a) % 256, 0);
    assert_eq!(sb.gpu(addr_b) % 256, 0);
    assert_eq!(sa.width(), 512);
    assert_eq!(sb.width(), 256);
    // Existing lookup returns the exact same Arc<Shadow>.
    let sa2 = find_aligned_cbv_existing(addr_a, 300).expect("cached CBV shadow A");
    assert!(std::sync::Arc::ptr_eq(&sa, &sa2));
    // Initial bytes are seeded from guest memory and padded tail is zeroed.
    // SAFETY: `sa.data` maps 512 bytes owned by `sa`.
    let seeded = unsafe { std::slice::from_raw_parts(sa.data, 512) };
    let src_off = (addr_a - backing.as_ptr() as u64) as usize;
    assert_eq!(&seeded[..300], &backing[src_off..src_off + 300]);
    assert!(seeded[300..].iter().all(|&b| b == 0));
    // Growing the CBV at the same address replaces with a larger slot cleanly.
    let sa_big = find_aligned_cbv(addr_a, 1024).expect("grown CBV shadow A");
    assert!(sa_big.can_bind_cbv(addr_a, 1024));
    assert_eq!(sa_big.width(), 1024);
    assert!(!std::sync::Arc::ptr_eq(&sa, &sa_big));
}

#[test]
fn exact_shadow_supports_two_byte_aligned_offsets() {
    use super::exact;
    if crate::host::host().is_none() {
        return; // no host GPU
    }
    let mut backing = vec![0u8; 8192];
    for (i, b) in backing.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(3);
    }
    let page = (backing.as_ptr() as u64).next_multiple_of(4096);
    let unaligned = page + 6;
    let len = 64u64;
    let s = exact(unaligned, len).expect("2-byte aligned exact shadow");
    assert_eq!(s.span(), (unaligned, len));
    assert_eq!(s.offset(unaligned), 0);
    assert_eq!(s.gpu(unaligned) % 16, 0);
    assert_eq!(s.width(), 256);
    // SAFETY: `s.data` maps 256 bytes owned by `s`.
    let seeded = unsafe { std::slice::from_raw_parts(s.data, 256) };
    let src_off = (unaligned - backing.as_ptr() as u64) as usize;
    assert_eq!(&seeded[..len as usize], &backing[src_off..src_off + len as usize]);
    assert!(seeded[len as usize..].iter().all(|&b| b == 0));
    let s2 = exact(unaligned, len).expect("cached exact shadow");
    assert!(std::sync::Arc::ptr_eq(&s, &s2));
}
