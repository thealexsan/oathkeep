use super::*;


/// The clamp compiles with the system's compiler and the host takes it.
#[test]
fn argument_clamp_builds() {
    if host().is_none() {
        return; // no host GPU here
    }
    assert!(clamp().is_some());
}

#[test]
fn rejects_unaligned_or_undersized_layouts() {
    assert_eq!(validate_layout((0, 16, 4), 0, None), Some(64));
    assert_eq!(validate_layout((1, 20, 4), 64, Some(128)), Some(80));
    assert_eq!(validate_layout((2, 12, 0), 0, Some(0)), Some(0));
    assert_eq!(validate_layout((10, 16, 2), 8, None), Some(32));
    // Undersized strides for each kind.
    assert_eq!(validate_layout((0, 12, 1), 0, None), None);
    assert_eq!(validate_layout((1, 16, 1), 0, None), None);
    assert_eq!(validate_layout((2, 8, 1), 0, None), None);
    assert_eq!(validate_layout((10, 8, 1), 0, None), None);
    // Unknown kind.
    assert_eq!(validate_layout((3, 16, 1), 0, None), None);
    // Unaligned stride, source offset, or count offset.
    assert_eq!(validate_layout((0, 18, 1), 0, None), None);
    assert_eq!(validate_layout((0, 16, 1), 2, None), None);
    assert_eq!(validate_layout((0, 16, 1), 0, Some(6)), None);
    // Total byte size exceeding 32-bit ByteAddressBuffer indexing.
    assert_eq!(validate_layout((1, 20, u32::MAX), 0, None), None);
}

/// WARP D3D12 execution test proving:
/// (a) `countBuffer` with a small count (`3`) and `4,093` garbage tail slots
///     (`max = 4096`) does NOT blow `BUDGET` or zero valid dispatches in
///     this or subsequent `ExecuteIndirect` calls in the same list,
/// (b) `src_args` (and `count_buf`) are NOT mutated in-place, and
/// (c) full `stride` bytes (`stride = 24` > `12`) are copied for active
///     records into `dst_args` at offset `0`.
#[test]
fn warp_count_buffer_ignores_garbage_tail_and_preserves_source() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    const KIND_DISPATCH: u32 = 2;
    const STRIDE: u32 = 24; // 6 dwords: 3 dispatch group counts + 3 extra payload dwords
    const MAX: u32 = 4096;
    const ARGS_OFFSET: usize = 64; // non-zero offset with sentinel prefix
    const COUNT_OFFSET_1: usize = 128;
    const COUNT_OFFSET_2: usize = 192;

    let src_bytes = ARGS_OFFSET + (STRIDE as usize) * (MAX as usize) + 64;
    let (src_buf, src_ptr, _) = warp.shared_buffer(src_bytes as u64);
    let (count_buf, count_ptr, _) = warp.shared_buffer(512);
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (_hits_buf, hits_ptr, hits_gpu) = warp.shared_buffer(256);
    let (readback_buf, readback_ptr, _) = warp.shared_buffer((STRIDE * 8) as u64);
    let dst_buf = warp.default_uav(u64::from(STRIDE) * u64::from(MAX));
    let sig = warp.signature(KIND_DISPATCH, STRIDE);

    // Fill the entire source buffer with non-zero sentinel bytes, then
    // populate 3 valid records at `ARGS_OFFSET` and 4,093 huge garbage
    // records in `3..MAX` that would exhaust `BUDGET` ~2,000x over if read.
    let mut initial_src = vec![0xA5u8; src_bytes];
    let write_record = |buf: &mut [u8], idx: usize, words: [u32; 6]| {
        let base = ARGS_OFFSET + idx * (STRIDE as usize);
        for (w, val) in words.into_iter().enumerate() {
            buf[base + w * 4..base + (w + 1) * 4].copy_from_slice(&val.to_le_bytes());
        }
    };
    write_record(
        &mut initial_src,
        0,
        [3, 2, 1, 0x1111_2222, 0x3333_4444, 0x5555_6666],
    );
    write_record(
        &mut initial_src,
        1,
        [5, 1, 1, 0x7777_8888, 0x9999_AAAA, 0xBBBB_CCCC],
    );
    // Record 2 exceeds what any card runs in X -> clamped to (65535, 1, 1).
    write_record(
        &mut initial_src,
        2,
        [(1 << 20) + 1, 1, 1, 0xDDDD_EEEE, 0xFFFF_0001, 0x0123_4567],
    );
    for slot in 3..MAX as usize {
        // Each garbage slot would clamp to 65535 * 16 * 1 = 1,048,560 groups
        // (`cost = 67,107,840 ~= BUDGET / 2`), exhausting `BUDGET` immediately!
        write_record(
            &mut initial_src,
            slot,
            [0xFFFF_FFFF, 16, 1, 0xDEAD_BEEF, 0xBAD0_C0DE, slot as u32],
        );
    }

    // Second indirect call's source records (at offset 0 of a second buffer,
    // or later in `count_buf`): put 2 active records + garbage tail in a
    // second range to run back-to-back in the SAME command list.
    let mut initial_count = vec![0x5Au8; 512];
    initial_count[COUNT_OFFSET_1..COUNT_OFFSET_1 + 4].copy_from_slice(&3u32.to_le_bytes());
    initial_count[COUNT_OFFSET_2..COUNT_OFFSET_2 + 4].copy_from_slice(&2u32.to_le_bytes());

    // SAFETY: buffers are mapped with exact capacities above.
    unsafe {
        std::ptr::copy_nonoverlapping(initial_src.as_ptr(), src_ptr, src_bytes);
        std::ptr::copy_nonoverlapping(initial_count.as_ptr(), count_ptr, 512);
        (budget_ptr as *mut u32).write_volatile(0);
        (hits_ptr as *mut u32).write_volatile(0);
        std::ptr::write_bytes(readback_ptr, 0, (STRIDE * 8) as usize);
    }

    // Record Call #1 (`eff_count = 3` out of `MAX = 4096`), copy the first
    // 5 records of `dst_buf` out for inspection, then record Call #2
    // (`eff_count = 2` out of `MAX = 4096`) reusing `dst_buf` in the same list!
    assert!(record_clamp_as(
        &warp.clamp,
        &warp.list,
        (&dst_buf, D3D12_RESOURCE_STATE_COMMON),
        budget_gpu,
        (&src_buf, ARGS_OFFSET as u64),
        Some((&count_buf, COUNT_OFFSET_1 as u64)),
        (KIND_DISPATCH, STRIDE, MAX),
        false,
    ));
    // SAFETY: open command list, valid worker PSO/root and buffers.
    unsafe {
        warp.list.SetComputeRootSignature(&warp.worker.root);
        warp.list.SetPipelineState(&warp.worker.pso);
        warp.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
        warp.list.ExecuteIndirect(
            &sig,
            MAX,
            &dst_buf,
            0,
            Some(&count_buf),
            COUNT_OFFSET_1 as u64,
        );
    }

    // Snapshot `dst_buf` after Call #1 (copy 5 * STRIDE bytes, then leave
    // `dst_buf` readable as arguments again, as the clamp left it).
    // SAFETY: open command list; the clamp left `dst_buf` in `INDIRECT_ARGUMENT`.
    unsafe {
        warp.list.ResourceBarrier(&[transition(
            &dst_buf,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        warp.list
            .CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, u64::from(STRIDE * 5));
        warp.list.ResourceBarrier(&[transition(
            &dst_buf,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        )]);
    }

    // Call #2 in the SAME command list (`eff_count = 2` -> records 0 and 1:
    // 6 + 5 = 11 groups). Would run empty if Call #1's tail blew `BUDGET`!
    // The scratch buffer goes straight from arguments back to unordered
    // access.
    assert!(record_clamp_as(
        &warp.clamp,
        &warp.list,
        (&dst_buf, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT),
        budget_gpu,
        (&src_buf, ARGS_OFFSET as u64),
        Some((&count_buf, COUNT_OFFSET_2 as u64)),
        (KIND_DISPATCH, STRIDE, MAX),
        false,
    ));
    // SAFETY: open command list, valid worker PSO/root and buffers.
    unsafe {
        warp.list.SetComputeRootSignature(&warp.worker.root);
        warp.list.SetPipelineState(&warp.worker.pso);
        warp.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
        warp.list.ExecuteIndirect(
            &sig,
            MAX,
            &dst_buf,
            0,
            Some(&count_buf),
            COUNT_OFFSET_2 as u64,
        );
    }

    warp.submit_and_wait();

    // SAFETY: GPU execution finished; read mapped host results.
    let (hits, spent, after_src, after_count, clamped_words) = unsafe {
        let hits = (hits_ptr as *const u32).read_volatile();
        let spent = (budget_ptr as *const u32).read_volatile();
        let after_src = std::slice::from_raw_parts(src_ptr.cast_const(), src_bytes).to_vec();
        let after_count = std::slice::from_raw_parts(count_ptr.cast_const(), 512).to_vec();
        let words = std::slice::from_raw_parts(
            readback_ptr.cast::<u32>().cast_const(),
            (STRIDE as usize / 4) * 5,
        )
        .to_vec();
        (hits, spent, after_src, after_count, words)
    };

    // Call #1: (3*2*1) + (5*1*1) + (65535*1*1) = 65,546 groups.
    // Call #2: (3*2*1) + (5*1*1) = 11 groups.
    let expected_groups = (6 + 5 + 65_535) + (6 + 5);
    assert_eq!(
        hits, expected_groups,
        "valid indirect dispatches must all execute"
    );
    assert_eq!(
        spent,
        expected_groups * 64,
        "budget must reflect only active records below countBuffer"
    );
    assert_eq!(
        after_src, initial_src,
        "src_args must never be mutated in-place"
    );
    assert_eq!(
        after_count, initial_count,
        "count_buf must never be mutated"
    );

    // Check `dst_args` records 0..3: clamped group counts + preserved extra stride words.
    assert_eq!(
        &clamped_words[0..6],
        &[3, 2, 1, 0x1111_2222, 0x3333_4444, 0x5555_6666]
    );
    assert_eq!(
        &clamped_words[6..12],
        &[5, 1, 1, 0x7777_8888, 0x9999_AAAA, 0xBBBB_CCCC]
    );
    assert_eq!(
        &clamped_words[12..18],
        &[65_535, 1, 1, 0xDDDD_EEEE, 0xFFFF_0001, 0x0123_4567]
    );
    // Inactive slot 3 was never copied from `src_args`.
    assert_ne!(clamped_words[21], 0xDEAD_BEEF);
}

/// WARP D3D12 test proving `BUDGET` saturates without 32-bit unsigned
/// wraparound across >64 huge indexed-draw records (`count = None`), while
/// preserving `StartIndexLocation`, `BaseVertexLocation`,
/// `StartInstanceLocation`, and custom stride padding, and leaving
/// `src_args` untouched.
/// A window of mesh records clamped ahead: the filled ones come out as
/// they are and are charged, the slots of all ones come out empty and
/// charge nothing; clamped as single records the same slots are clamped
/// to the most and spend the budget.
#[test]
fn warp_window_leaves_unfilled_slots_empty_and_uncharged() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    const KIND_MESH: u32 = 10;
    const STRIDE: u32 = 32;
    const RECORDS: u32 = 16;
    let filled = [5u32, 0, 519];
    let total = (STRIDE * RECORDS) as usize;
    let mut src = vec![0xffu8; total];
    for (i, groups) in filled.into_iter().enumerate() {
        let words = [groups, 1, 1, 0xAB00 + i as u32];
        for (w, word) in words.into_iter().enumerate() {
            let at = i * STRIDE as usize + w * 4;
            src[at..at + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    for ahead in [true, false] {
        let (src_buf, src_ptr, _) = warp.shared_buffer(total as u64);
        let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
        let (readback_buf, readback_ptr, _) = warp.shared_buffer(total as u64);
        let dst_buf = warp.default_uav(total as u64);
        // SAFETY: mapped buffers of these sizes.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), src_ptr, total);
            std::ptr::write_bytes(budget_ptr, 0, 256);
            std::ptr::write_bytes(readback_ptr, 0, total);
        }
        assert!(record_clamp_as(
            &warp.clamp,
            &warp.list,
            (&dst_buf, D3D12_RESOURCE_STATE_COMMON),
            budget_gpu,
            (&src_buf, 0),
            None,
            (KIND_MESH, STRIDE, RECORDS),
            ahead,
        ));
        // SAFETY: an open list; the scratch buffer, which the clamp left
        // readable as arguments, is copied for the check.
        unsafe {
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            warp.list.CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, total as u64);
        }
        warp.submit_and_wait();
        // SAFETY: the work is done; mapped buffers.
        let (spent, largest, out) = unsafe {
            (
                (budget_ptr as *const u32).read_volatile(),
                (budget_ptr as *const u32).add(1).read_volatile(),
                std::slice::from_raw_parts(readback_ptr.cast::<u32>().cast_const(), total / 4).to_vec(),
            )
        };
        for (i, groups) in filled.into_iter().enumerate() {
            let rec = &out[i * 8..i * 8 + 4];
            assert_eq!(rec, [groups, u32::from(groups > 0).max(1).min(1), 1, 0xAB00 + i as u32], "filled record {i}, ahead {ahead}");
        }
        let charged_filled = (5 + 519) * 128;
        // As one window ahead of its draws and one by one alike.
        for i in filled.len()..RECORDS as usize {
            assert_eq!(out[i * 8..i * 8 + 3], [0, 0, 0], "unfilled slot {i} must come out empty");
            assert_eq!(out[i * 8 + 3], u32::MAX, "the words after the record are copied as they are");
        }
        assert_eq!(spent, charged_filled, "unfilled slots charge nothing");
        assert_eq!(largest, 519, "and are not counted as asked for");
    }
}

#[test]
fn warp_budget_saturates_without_wraparound_and_preserves_draw_fields() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    const KIND_INDEXED: u32 = 1;
    const STRIDE: u32 = 24; // 5 indexed-draw words + 1 trailing custom word
    const RECORDS: u32 = 128; // 2 waves of 64: old InterlockedAdd wrapped 2^32 at 64!
    const BUDGET_CAP: u32 = 1 << 27;

    let total_bytes = (STRIDE * RECORDS) as usize;
    let (src_buf, src_ptr, _) = warp.shared_buffer(total_bytes as u64);
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (readback_buf, readback_ptr, _) = warp.shared_buffer(total_bytes as u64);
    let dst_buf = warp.default_uav(total_bytes as u64);

    let mut initial_src = vec![0u8; total_bytes];
    for i in 0..RECORDS as usize {
        let words: [u32; 6] = [
            // The last is no draw: empty, and free.
            if i + 1 < RECORDS as usize { 1 << 22 } else { 1 << 23 }, // IndexCount
            u32::MAX,                    // InstanceCount -> clamps to 16 (cost = 1 << 26)
            1000 + i as u32,             // StartIndexLocation
            (-200i32 + i as i32) as u32, // BaseVertexLocation
            5000 + i as u32,             // StartInstanceLocation
            0xCAFE_0000 | (i as u32),    // Extra stride word
        ];
        let base = i * (STRIDE as usize);
        for (w, val) in words.into_iter().enumerate() {
            initial_src[base + w * 4..base + (w + 1) * 4].copy_from_slice(&val.to_le_bytes());
        }
    }

    // SAFETY: mapped buffers have sufficient capacity.
    unsafe {
        std::ptr::copy_nonoverlapping(initial_src.as_ptr(), src_ptr, total_bytes);
        (budget_ptr as *mut u32).write_volatile(0);
        std::ptr::write_bytes(readback_ptr, 0, total_bytes);
    }

    assert!(record_clamp_as(
        &warp.clamp,
        &warp.list,
        (&dst_buf, D3D12_RESOURCE_STATE_COMMON),
        budget_gpu,
        (&src_buf, 0),
        None,
        (KIND_INDEXED, STRIDE, RECORDS),
        false,
    ));

    // SAFETY: open command list; copy clamped scratch buffer (left
    // readable as arguments) for verification.
    unsafe {
        warp.list.ResourceBarrier(&[transition(
            &dst_buf,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        warp.list
            .CopyBufferRegion(&readback_buf, 0, &dst_buf, 0, total_bytes as u64);
    }
    warp.submit_and_wait();

    // SAFETY: GPU work completed.
    let (spent, after_src, out_words) = unsafe {
        let spent = (budget_ptr as *const u32).read_volatile();
        let after_src = std::slice::from_raw_parts(src_ptr.cast_const(), total_bytes).to_vec();
        let out_words = std::slice::from_raw_parts(
            readback_ptr.cast::<u32>().cast_const(),
            total_bytes / 4,
        )
        .to_vec();
        (spent, after_src, out_words)
    };

    assert_eq!(
        spent, BUDGET_CAP,
        "budget must saturate at BUDGET without wrapping"
    );
    assert_eq!(after_src, initial_src, "src_args must remain untouched");

    let mut kept = 0usize;
    let mut zeroed = 0usize;
    for i in 0..RECORDS as usize {
        let rec = &out_words[i * 6..(i + 1) * 6];
        if rec[0] == (1 << 22) && rec[1] == 16 {
            kept += 1;
        } else {
            assert_eq!((rec[0], rec[1]), (0, 0), "excess draw must be zeroed");
            zeroed += 1;
        }
        assert_eq!(rec[2], 1000 + i as u32);
        assert_eq!(rec[3], (-200i32 + i as i32) as u32);
        assert_eq!(rec[4], 5000 + i as u32);
        assert_eq!(rec[5], 0xCAFE_0000 | (i as u32));
    }
    assert_eq!((kept, zeroed), (2, (RECORDS as usize) - 2));
}

