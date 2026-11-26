use super::*;


/// Records the dispatch records `groups` (three words each, `step`
/// bytes apart, the rest of each record ones) into a fresh mapped
/// buffer of `warp`'s device.
pub(super) fn dispatch_records(warp: &WarpHarness, groups: &[u32], step: u32) -> ID3D12Resource {
    let total = groups.len() * step as usize;
    let (buffer, ptr, _) = warp.shared_buffer(total as u64);
    let mut bytes = vec![0xffu8; total];
    for (i, &g) in groups.iter().enumerate() {
        for (w, word) in [g, 1, 1].into_iter().enumerate() {
            let at = i * step as usize + w * 4;
            bytes[at..at + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    // SAFETY: the mapping holds `total` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, total) };
    buffer
}

/// Two windows in one list, as `crate::execution::draw` records them: each clamps
/// its records ahead, then one execution per record reads the scratch
/// buffer with no barrier between them; the second clamp moves the
/// scratch buffer straight from arguments back to unordered access.
/// Every record runs once, with the group count it asked for.
#[test]
fn warp_window_executions_read_their_records_with_no_barrier_between() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    const KIND_DISPATCH: u32 = 2;
    // The title names 12 bytes and keeps its records 32 apart.
    const STRIDE: u32 = 12;
    const STEP: u32 = 32;
    let windows: [&[u32]; 2] = [&[1, 2, 3, 4], &[10, 11, 12, 13]];
    let sources: Vec<ID3D12Resource> =
        windows.iter().map(|g| dispatch_records(&warp, g, STEP)).collect();
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (_hits_buf, hits_ptr, hits_gpu) = warp.shared_buffer(256);
    let dst_buf = warp.default_uav(u64::from(STEP) * 4);
    let sig = warp.signature(KIND_DISPATCH, STRIDE);
    // SAFETY: mapped buffers of these sizes.
    unsafe {
        std::ptr::write_bytes(budget_ptr, 0, 256);
        std::ptr::write_bytes(hits_ptr, 0, 256);
    }
    let mut state = D3D12_RESOURCE_STATE_COMMON;
    for (groups, src) in windows.iter().zip(&sources) {
        assert!(record_clamp_as(
            &warp.clamp,
            &warp.list,
            (&dst_buf, state),
            budget_gpu,
            (src, 0),
            None,
            (KIND_DISPATCH, STEP, groups.len() as u32),
            true,
        ));
        state = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
        // SAFETY: an open list; the worker's root signature and
        // pipeline; the scratch buffer is readable as arguments.
        unsafe {
            warp.list.SetComputeRootSignature(&warp.worker.root);
            warp.list.SetPipelineState(&warp.worker.pso);
            warp.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
            for i in 0..groups.len() as u64 {
                warp.list.ExecuteIndirect(&sig, 1, &dst_buf, i * u64::from(STEP), None, 0);
            }
        }
    }
    warp.submit_and_wait();
    // SAFETY: the work is done; mapped buffers.
    let (hits, spent) = unsafe {
        (
            (hits_ptr as *const u32).read_volatile(),
            (budget_ptr as *const u32).read_volatile(),
        )
    };
    let expected: u32 = windows.iter().flat_map(|g| g.iter()).sum();
    assert_eq!(hits, expected, "every record of both windows runs once");
    assert_eq!(spent, expected * 64, "and is charged what it asked for");
}

/// The windows of `warp_window_executions_read_their_records_with_no_barrier_between`
/// through a host list (`bound_ahead`, the list's tracked scratch state,
/// `Host::execute`), twice: on the first recording the scratch buffer
/// starts common, on the second (after the reset) it starts common
/// again. With the debug layer on (`OATHKEEP_D3D12_DEBUG=1` in a
/// diagnostics build) no barrier names a state the buffer is not in.
#[test]
fn host_list_window_executions_need_no_barrier_of_their_own() {
    let Some(host) = host() else {
        return;
    };
    // What earlier tests left in the debug layer's queue, if it is on.
    let _ = host.log_messages();
    let Ok(list) = host.new_list() else {
        return;
    };
    let worker = compile_cs(&host.device, WORKER_CS).expect("worker shader must compile");
    const KIND_DISPATCH: u32 = 2;
    const STRIDE: u32 = 12;
    const STEP: u32 = 32;
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
    let windows: [&[u32]; 2] = [&[1, 2, 3, 4], &[10, 11, 12, 13]];
    let mut sources = Vec::new();
    for groups in windows {
        let total = groups.len() * STEP as usize;
        let Some((buffer, ptr)) = host.shared_buffer(total as u64) else {
            return;
        };
        let mut bytes = vec![0xffu8; total];
        for (i, &g) in groups.iter().enumerate() {
            for (w, word) in [g, 1, 1].into_iter().enumerate() {
                let at = i * STEP as usize + w * 4;
                bytes[at..at + 4].copy_from_slice(&word.to_le_bytes());
            }
        }
        // SAFETY: the mapping holds `total` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, total) };
        sources.push(buffer);
    }
    let Some((hits_buf, hits_ptr)) = host.shared_buffer(256) else {
        return;
    };
    let expected: u32 = windows.iter().flat_map(|g| g.iter()).sum();
    // SAFETY: a live buffer.
    let hits_gpu = unsafe { hits_buf.GetGPUVirtualAddress() };
    for _ in 0..2 {
        // SAFETY: the mapping holds 256 bytes; nothing runs.
        unsafe { std::ptr::write_bytes(hits_ptr, 0, 256) };
        for (groups, src) in windows.iter().zip(&sources) {
            let spec = (KIND_DISPATCH, STEP, groups.len() as u32);
            let scratch = bound_ahead(&list, (src, 0), spec).unwrap();
            assert_eq!(list.indirect_state(&scratch), D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
            // SAFETY: an open list; the worker's root signature and
            // pipeline; buffers the test holds until the list has run.
            unsafe {
                list.list.SetComputeRootSignature(&worker.root);
                list.list.SetPipelineState(&worker.pso);
                list.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
                for i in 0..groups.len() as u64 {
                    let at = i * u64::from(STEP);
                    list.list.ExecuteIndirect(&signature, 1, &scratch, at, None, 0);
                }
            }
        }
        list.close();
        // With the debug layer on: nothing it objects to was recorded.
        // Read before the submission, which moves the messages to the log.
        assert_eq!(host.log_messages(), None, "the debug layer found an error");
        host.execute(&[&list]);
        list.reset();
        // SAFETY: the list has run; a mapped buffer.
        let hits = unsafe { (hits_ptr as *const u32).read_volatile() };
        assert_eq!(hits, expected, "every record of both windows runs once");
    }
}

/// A command signature of one argument of `kind` on `device`.
pub(super) fn one_argument(device: &ID3D12Device, kind: u32, stride: u32) -> ID3D12CommandSignature {
    let arg = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &arg,
        NodeMask: 0,
    };
    let mut sig: Option<ID3D12CommandSignature> = None;
    // SAFETY: a valid single-argument command signature description.
    unsafe {
        device.CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut sig).unwrap();
    }
    sig.unwrap()
}

/// An AMD card runs every group of an indirect dispatch of more than
/// 65,535, each under its own number: the title's tile lists
/// ([`widest`]), clamped one by one and in a batch. Others are not asked.
#[test]
fn card_runs_wide_indirect_dispatches() {
    const CS: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer seen : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main(uint3 group : SV_GroupID, uint3 thread : SV_DispatchThreadID) {
uint was;
seen.InterlockedAdd(0, 1, was);
seen.InterlockedMax(4, group.x, was);
seen.InterlockedMax(8, thread.x, was);
seen.InterlockedAdd(16 + group.x * 4, 1, was);
}
"#;
    let Some(mut gpu) = WarpHarness::on(false) else {
        return;
    };
    if !amd(&gpu.device) || widest(true) == 65_535 {
        return;
    }
    let worker = compile_cs(&gpu.device, CS).expect("the worker must compile");
    let pass = make_from(&gpu.device, batch::SOURCE).expect("the batched clamp must compile");
    let sig = gpu.signature(2, 12);
    for asked in [67_440u32, 115_200, 1 << 20, (1 << 20) + 1] {
        let runs = if asked > 1 << 20 { 65_535 } else { asked };
        for batched in [false, true] {
            let (src, src_ptr, src_gpu) = gpu.shared_buffer(1024);
            let (_budget, budget_ptr, budget_gpu) = gpu.shared_buffer(256);
            let (_table, table_ptr, table_gpu) = gpu.shared_buffer(batch::TABLE);
            let size = 16 + (4 << 20) + 16;
            let seen = gpu.default_uav(size);
            let (back, back_ptr, _) = gpu.shared_buffer(size);
            let dst = gpu.default_uav(64 << 10);
            // SAFETY: mapped buffers, which nothing reads yet; an open
            // list and live buffers.
            unsafe {
                std::ptr::write_bytes(budget_ptr, 0, 256);
                for (w, word) in [asked, 1, 1].into_iter().enumerate() {
                    src_ptr.cast::<u32>().add(w).write_volatile(word);
                }
                if batched {
                    batch::record(&pass, &gpu.list, (&dst, D3D12_RESOURCE_STATE_COMMON), budget_gpu, [src_gpu; 4], table_gpu);
                    assert_eq!(batch::Table::new(table_ptr, 0, [1, 0, 0, 0]).place(1, 0, 2), Some(0));
                } else {
                    let to = (&dst, D3D12_RESOURCE_STATE_COMMON);
                    assert!(record_clamp_as(&gpu.clamp, &gpu.list, to, budget_gpu, (&src, 0), None, (2, 12, 1), false));
                }
                gpu.list.SetComputeRootSignature(&worker.root);
                gpu.list.SetPipelineState(&worker.pso);
                gpu.list.SetComputeRootUnorderedAccessView(0, seen.GetGPUVirtualAddress());
                gpu.list.ExecuteIndirect(&sig, 1, &dst, 0, None, 0);
                gpu.list.ResourceBarrier(&[uav_barrier()]);
                gpu.list.ResourceBarrier(&[transition(&seen, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                gpu.list.CopyBufferRegion(&back, 0, &seen, 0, size);
            }
            gpu.submit_and_wait();
            // SAFETY: the list has run; mapped words.
            let word = |i: usize| unsafe { back_ptr.cast::<u32>().add(i).read_volatile() };
            assert_eq!([word(0), word(1), word(2)], [runs, runs - 1, runs - 1], "asked for {asked}, batched {batched}");
            let wrong = (0..runs as usize).find(|&g| word(4 + g) != 1);
            assert_eq!((wrong, word(4 + runs as usize)), (None, 0), "every group once, asked for {asked}");
        }
    }
}

