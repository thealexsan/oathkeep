use super::*;


/// Exercises `HostList::indirect_scratch`, `bound`, the scratch buffer's
/// tracked state, `Host::execute`, and `HostList::reset` end-to-end on
/// the host device.
#[test]
fn host_list_reuses_and_grows_scratch_across_submissions() {
    let Some(host) = host() else {
        return;
    };
    // What earlier tests left in the debug layer's queue, if it is on.
    let _ = host.log_messages();
    let Ok(list) = host.new_list() else {
        return;
    };
    let Some((src_buf, src_ptr)) = host.shared_buffer(256 << 10) else {
        return;
    };
    let Some((count_buf, count_ptr)) = host.shared_buffer(256) else {
        return;
    };
    // SAFETY: mapped host buffers.
    unsafe {
        std::ptr::write_bytes(src_ptr, 0x7F, 256 << 10);
        (count_ptr as *mut u32).write_volatile(2);
    }

    let args = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
    // First two calls fit inside the initial 64 KiB scratch buffer and
    // must reuse the exact same `ID3D12Resource`, which stays readable
    // as arguments between them.
    let b1 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (0, 16, 64)).unwrap();
    assert_eq!(
        list.indirect_state(&b1),
        args,
        "a clamp leaves the scratch readable as arguments"
    );
    let b2 = bound(&list, (&src_buf, 64), Some((&count_buf, 0)), (1, 20, 128)).unwrap();
    assert_eq!(
        b1.as_raw(),
        b2.as_raw(),
        "scratch buffer within capacity must be reused"
    );
    assert_eq!(list.indirect_state(&b2), args);

    // A call needing >64 KiB grows the scratch buffer mid-list while keeping
    // `b1` alive until reset. The new buffer starts out common.
    let grown = list.indirect_scratch(5000 * 20).unwrap();
    assert_eq!(
        list.indirect_state(&grown),
        D3D12_RESOURCE_STATE_COMMON,
        "a new scratch buffer is common"
    );
    let b3 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (1, 20, 5000)).unwrap();
    assert_eq!(b3.as_raw(), grown.as_raw());
    assert_ne!(
        b3.as_raw(),
        b1.as_raw(),
        "larger request must grow scratch buffer"
    );
    assert_eq!(list.indirect_state(&b3), args);
    assert_eq!(
        list.indirect_state(&b1),
        D3D12_RESOURCE_STATE_COMMON,
        "the replaced buffer went back to common"
    );

    list.close();
    // With the debug layer on (`OATHKEEP_D3D12_DEBUG=1`, diagnostics):
    // every barrier recorded named the state the buffer was in. Read
    // before the submission, which moves the messages to the log.
    assert_eq!(host.log_messages(), None, "the debug layer found an error");
    host.execute(&[&list]);
    list.reset();

    // After reset, requests up to the grown high-water mark reuse `b3` with
    // zero new allocations; the buffer decayed to common when the
    // submission ran.
    assert_eq!(
        list.indirect_state(&b3),
        D3D12_RESOURCE_STATE_COMMON,
        "a reset list starts with a common scratch buffer"
    );
    let b4 = bound(&list, (&src_buf, 0), Some((&count_buf, 0)), (1, 20, 4096)).unwrap();
    assert_eq!(
        b4.as_raw(),
        b3.as_raw(),
        "grown scratch buffer must persist across reset"
    );
    assert_eq!(list.indirect_state(&b4), args);
    list.close();
    assert_eq!(host.log_messages(), None, "the debug layer found an error");
    host.execute(&[&list]);
    list.reset();
}

/// A group charges its records in lane order: with 300 left of the
/// budget, the first draw (100) fits, the second (250) does not and
/// takes the rest, and the ones after it run empty. (Racing for the
/// word record by record, which of them fitted was up to the GPU.)
#[test]
fn warp_a_group_charges_its_records_in_lane_order() {
    let Some((left, out)) = four_records_of_a_group(true) else {
        return;
    };
    assert_eq!(out[0..4], [100, 1, 7, 9], "the first record fits");
    for i in 1..4 {
        assert_eq!(out[i * 4..i * 4 + 4], [0, 0, 7, 9], "record {i} runs empty");
    }
    assert_eq!(left, 0, "the record that did not fit took the rest");
}

/// As above where a record that does not fit takes nothing
/// (`OATHKEEP_INDIRECT_REST=0`): those after it that fit run.
#[test]
fn warp_a_record_that_does_not_fit_may_take_nothing() {
    let Some((left, out)) = four_records_of_a_group(false) else {
        return;
    };
    for (i, count) in [100, 0, 50, 100].into_iter().enumerate() {
        assert_eq!(out[i * 4..i * 4 + 4], [count, count.min(1), 7, 9], "record {i}");
    }
    assert_eq!(left, 50);
}

/// Draws of 100, 250, 50 and 100 with 300 left of the budget: what is
/// left then, and the records as clamped.
fn four_records_of_a_group(rest: bool) -> Option<(u32, Vec<u32>)> {
    let mut warp = WarpHarness::new()?;
    let source = SOURCE.replace("REST ?", if rest { "true ?" } else { "false ?" });
    let clamp = make_from(&warp.device, &source).expect("the clamp must compile on WARP");
    const KIND_DRAW: u32 = 0;
    const STRIDE: u32 = 16;
    const BUDGET_CAP: u32 = 1 << 27;
    let counts = [100u32, 250, 50, 100];
    let total = counts.len() * STRIDE as usize;
    let (src_buf, src_ptr, _) = warp.shared_buffer(total as u64);
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (readback_buf, readback_ptr, _) = warp.shared_buffer(total as u64);
    let dst_buf = warp.default_uav(total as u64);
    // SAFETY: mapped buffers of these sizes.
    unsafe {
        for (i, &count) in counts.iter().enumerate() {
            let words = [count, 1, 7, 9];
            std::ptr::copy_nonoverlapping(
                words.as_ptr().cast::<u8>(),
                src_ptr.add(i * STRIDE as usize),
                16,
            );
        }
        std::ptr::write_bytes(budget_ptr, 0, 256);
        (budget_ptr as *mut u32).write_volatile(BUDGET_CAP - 300);
    }
    assert!(record_clamp_as(
        &clamp,
        &warp.list,
        (&dst_buf, D3D12_RESOURCE_STATE_COMMON),
        budget_gpu,
        (&src_buf, 0),
        None,
        (KIND_DRAW, STRIDE, counts.len() as u32),
        false,
    ));
    // SAFETY: an open list; the clamped records are copied for the check.
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
    let (spent, out) = unsafe {
        (
            (budget_ptr as *const u32).read_volatile(),
            std::slice::from_raw_parts(readback_ptr.cast::<u32>().cast_const(), total / 4)
                .to_vec(),
        )
    };
    Some((BUDGET_CAP - spent, out))
}

/// The list's budget starts empty at each submission, wherever it
/// lives (the card's copy is zeroed in the list): a recording whose
/// clamps spend all of it is followed, after the reset, by one whose
/// small record still runs.
#[test]
fn host_list_budget_starts_empty_at_each_submission() {
    let Some(host) = host() else {
        return;
    };
    let Ok(list) = host.new_list() else {
        return;
    };
    let _ = host.log_messages();
    let worker = compile_cs(&host.device, WORKER_CS).expect("worker shader must compile");
    const KIND_DISPATCH: u32 = 2;
    const STRIDE: u32 = 12;
    let signature = {
        let arg = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE(KIND_DISPATCH as i32),
            ..Default::default()
        };
        let desc = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: STRIDE,
            NumArgumentDescs: 1,
            pArgumentDescs: &arg,
            NodeMask: 0,
        };
        let mut sig: Option<ID3D12CommandSignature> = None;
        // SAFETY: a valid single-argument command signature description.
        unsafe {
            host.device
                .CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut sig)
                .unwrap();
        }
        sig.unwrap()
    };
    // Three records of 65,535 x 16 groups: about half the budget each.
    let Some((big, big_ptr)) = host.shared_buffer(3 * u64::from(STRIDE)) else {
        return;
    };
    let Some((small, small_ptr)) = host.shared_buffer(u64::from(STRIDE)) else {
        return;
    };
    let Some((hits_buf, hits_ptr)) = host.shared_buffer(256) else {
        return;
    };
    // SAFETY: mapped buffers of these sizes; nothing runs yet.
    unsafe {
        for i in 0..3 {
            let words = [32768u32, 32, 1];
            std::ptr::copy_nonoverlapping(
                words.as_ptr().cast::<u8>(),
                big_ptr.add(i * STRIDE as usize),
                12,
            );
        }
        std::ptr::copy_nonoverlapping([1u32, 1, 1].as_ptr().cast::<u8>(), small_ptr, 12);
        std::ptr::write_bytes(hits_ptr, 0, 256);
    }
    // SAFETY: a live buffer.
    let hits_gpu = unsafe { hits_buf.GetGPUVirtualAddress() };
    for round in 0..2 {
        // Spends the budget (clamped, not executed), then one group.
        bound(&list, (&big, 0), None, (KIND_DISPATCH, STRIDE, 3)).unwrap();
        let scratch = bound(&list, (&small, 0), None, (KIND_DISPATCH, STRIDE, 1)).unwrap();
        // SAFETY: an open list; the worker's root signature and
        // pipeline; buffers the test holds until the list has run.
        unsafe {
            list.list.SetComputeRootSignature(&worker.root);
            list.list.SetPipelineState(&worker.pso);
            list.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
            list.list.ExecuteIndirect(&signature, 1, &scratch, 0, None, 0);
        }
        list.close();
        assert_eq!(host.log_messages(), None, "the debug layer found an error");
        host.execute(&[&list]);
        list.reset();
        // SAFETY: the list has run; a mapped buffer.
        let hits = unsafe { (hits_ptr as *const u32).read_volatile() };
        // The small record comes after the big ones spent the budget:
        // it runs empty, in every round alike (none carries over).
        assert_eq!(hits, 0, "round {round}: the spent budget empties the record after it");
    }
    // A recording with only the small record: the budget is empty
    // again, so it runs.
    let scratch = bound(&list, (&small, 0), None, (KIND_DISPATCH, STRIDE, 1)).unwrap();
    // SAFETY: as above.
    unsafe {
        list.list.SetComputeRootSignature(&worker.root);
        list.list.SetPipelineState(&worker.pso);
        list.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
        list.list.ExecuteIndirect(&signature, 1, &scratch, 0, None, 0);
    }
    list.close();
    assert_eq!(host.log_messages(), None, "the debug layer found an error");
    host.execute(&[&list]);
    list.reset();
    // SAFETY: the list has run; a mapped buffer.
    let hits = unsafe { (hits_ptr as *const u32).read_volatile() };
    assert_eq!(hits, 1, "the budget starts empty at the next submission");
}

