use super::*;
use crate::execution::indirect::batch;


use super::window::{dispatch_records, one_argument};

/// What a batch's table says of records added to it: one job per
/// record, charged for every execution that reads it, and none for what
/// the pass cannot read.
#[test]
fn batch_tables() {
    use batch::{JOBS, SLOT, TABLE, TIMES, Table, job};
    let mut memory = vec![0xffff_ffffu32; TABLE as usize / 4 + 1];
    // SAFETY: the vector holds the table and outlives its use.
    let mut t = unsafe { Table::new(memory.as_mut_ptr().cast(), 4096, [7, 9, 0, 0]) };
    assert_eq!(t.place(7, 64, 10), Some(4096));
    assert_eq!(t.place(9, 64, 10), Some(4096 + u64::from(SLOT)));
    assert_eq!(t.place(7, 64, 2), Some(4096 + 2 * u64::from(SLOT)), "another kind is another record");
    // The same record again: the same place, charged twice.
    assert_eq!(t.place(9, 64, 10), Some(4096 + u64::from(SLOT)));
    assert_eq!(&memory[..4], [3, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff]);
    assert_eq!(memory[4..8], [job(0, 10, 1), 64, 4096, 0]);
    assert_eq!(memory[8..12], [job(1, 10, 2), 64, 4096 + SLOT, 0]);
    assert_eq!(job(1, 10, 2), 1 | 10 << 4 | 3 << 8 | 2 << 16);
    // Not a copy of the pass, not on 4 bytes, past 32 bits, not a kind.
    for (id, offset, kind) in [(8, 64, 10), (7, 66, 10), (7, 1 << 32, 10), (7, u64::from(u32::MAX) & !3, 10), (7, 64, 3)] {
        assert_eq!(t.place(id, offset, kind), None);
    }
    // Read more often than one job may be charged for: another job.
    for _ in 1..TIMES {
        assert_eq!(t.place(7, 64, 10), Some(4096));
    }
    assert_eq!(t.place(7, 64, 10), Some(4096 + 3 * u64::from(SLOT)));
    assert_eq!(t.place(7, 64, 10), Some(4096 + 3 * u64::from(SLOT)));
    for i in 4..u64::from(JOBS) {
        assert_eq!(t.place(7, 1024 + 16 * i, 0), Some(4096 + i * u64::from(SLOT)));
    }
    assert_eq!(t.place(7, 1024, 1), None, "full");
    assert_eq!(t.place(7, 1024 + 16 * 5, 0), Some(4096 + 5 * u64::from(SLOT)));
    assert_eq!((memory[0], memory[TABLE as usize / 4]), (JOBS, 0xffff_ffff));
}

/// A batch clamps records exactly as the clamp of one execution does, of
/// any kind, sensible or not; they are added to its table after the pass
/// was recorded; it charges each as often as it is read.
#[test]
fn warp_batch_clamps_as_the_clamp_of_one_does() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let pass = make_from(&warp.device, batch::SOURCE).expect("the batched clamp must compile on WARP");
    let kinds = [0u32, 1, 2, 10];
    let values = [0u32, 1, 3, 100, 65_535, 65_536, 1 << 20, (1 << 23) - 1, 1 << 23, 1 << 24, (1 << 26) + 1, u32::MAX];
    // Every kind with many records, in two sources.
    let mut records: Vec<(u32, [u32; 5])> = Vec::new();
    for kind in kinds {
        for (i, a) in values.into_iter().enumerate() {
            for (k, b) in values.into_iter().enumerate() {
                records.push((kind, [a, b, values[(i * 3 + k) % 4], 77, 0xABCD]));
            }
        }
    }
    // In passes of so few that the budget holds whatever one asks for,
    // and one pass of them all, which it does not.
    let (src, src_ptr, src_gpu) = warp.shared_buffer(64 << 10);
    let (_other, other_ptr, other_gpu) = warp.shared_buffer(64 << 10);
    for (i, (_, words)) in records.iter().enumerate() {
        for (w, word) in words.iter().enumerate() {
            // SAFETY: 400 records of 32 bytes in 64 KiB.
            unsafe {
                src_ptr.cast::<u32>().add(i * 8 + w).write_volatile(*word);
                other_ptr.cast::<u32>().add(i * 8 + w).write_volatile(*word);
            }
        }
    }
    let (_budget, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (back, back_ptr, _) = warp.shared_buffer(64 << 10);
    let dst = warp.default_uav(64 << 10);
    let (_table, table_ptr, table_gpu) = warp.shared_buffer(batch::TABLE);
    for (i, (kind, _)) in records.iter().enumerate().step_by(3) {
        let offset = i as u64 * 32;
        let words = batch::words(*kind).unwrap();
        let stride = words * 4;
        let read = |p: *mut u8| -> Vec<u32> {
            // SAFETY: the mapping holds more.
            (0..words as usize).map(|w| unsafe { p.cast::<u32>().add(w).read_volatile() }).collect()
        };
        let mut seen = Vec::new();
        for batched in [false, true] {
            // SAFETY: mapped buffers; nothing runs.
            unsafe { std::ptr::write_bytes(budget_ptr, 0, 256) };
            if batched {
                batch::record(&pass, &warp.list, (&dst, D3D12_RESOURCE_STATE_COMMON), budget_gpu, [src_gpu, other_gpu, src_gpu, src_gpu], table_gpu);
                // SAFETY: the mapped table; the list has not run.
                let mut table = unsafe { batch::Table::new(table_ptr, 0, [1, 2, 0, 0]) };
                assert_eq!(table.place(1 + i as u64 % 2, offset, *kind), Some(0));
            } else {
                assert!(record_clamp_as(
                    &warp.clamp,
                    &warp.list,
                    (&dst, D3D12_RESOURCE_STATE_COMMON),
                    budget_gpu,
                    (&src, offset),
                    None,
                    (*kind, stride, 1),
                    false,
                ));
            }
            // SAFETY: an open list; the records are readable as arguments.
            unsafe {
                warp.list.ResourceBarrier(&[transition(&dst, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                warp.list.CopyBufferRegion(&back, 0, &dst, 0, u64::from(stride));
                warp.list.ResourceBarrier(&[transition(&dst, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
            }
            warp.submit_and_wait();
            // SAFETY: the list has run; mapped words.
            let budget: Vec<u32> = (0..3).map(|k| unsafe { budget_ptr.cast::<u32>().add(k).read_volatile() }).collect();
            seen.push((read(back_ptr), budget));
        }
        // (A mesh record that asks for nothing notes nothing.)
        assert_eq!(seen[0], seen[1], "record {i} of kind {kind}: {:?}", records[i].1);
    }
}

/// Records past the list's budget come out empty, and one read by many
/// executions is charged for each.
#[test]
fn warp_batch_charges_every_execution() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let pass = make_from(&warp.device, batch::SOURCE).expect("the batched clamp must compile on WARP");
    const BUDGET: u32 = 1 << 27;
    // 2^26 vertices each, as much as a draw is let run: two fill it.
    for (times, records, filled) in [(1u32, 3u64, 2usize), (2, 1, 1), (1, 1, 1), (3, 1, 0)] {
        let (_src, src_ptr, src_gpu) = warp.shared_buffer(64 << 10);
        let (_budget, budget_ptr, budget_gpu) = warp.shared_buffer(256);
        let (back, back_ptr, _) = warp.shared_buffer(1024);
        let (_table, table_ptr, table_gpu) = warp.shared_buffer(batch::TABLE);
        let dst = warp.default_uav(64 << 10);
        batch::record(&pass, &warp.list, (&dst, D3D12_RESOURCE_STATE_COMMON), budget_gpu, [src_gpu; 4], table_gpu);
        // SAFETY: mapped buffers, which the list has not read yet.
        let mut table = unsafe {
            std::ptr::write_bytes(budget_ptr, 0, 256);
            for r in 0..records {
                for (w, word) in [1u32 << 22, 16, 5, 6].into_iter().enumerate() {
                    src_ptr.cast::<u32>().add(r as usize * 4 + w).write_volatile(word);
                }
            }
            batch::Table::new(table_ptr, 0, [1, 0, 0, 0])
        };
        for _ in 0..times {
            for r in 0..records {
                assert_eq!(table.place(1, r * 16, 0), Some(r * u64::from(batch::SLOT)));
            }
        }
        // SAFETY: an open list; the records are readable as arguments.
        unsafe {
            warp.list.ResourceBarrier(&[transition(&dst, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            warp.list.CopyBufferRegion(&back, 0, &dst, 0, 256);
        }
        warp.submit_and_wait();
        // SAFETY: the list has run; mapped words.
        let (spent, out): (u32, Vec<u32>) = unsafe {
            (
                budget_ptr.cast::<u32>().read_volatile(),
                (0..records as usize).map(|r| back_ptr.cast::<u32>().add(r * batch::SLOT as usize / 4).read_volatile()).collect(),
            )
        };
        assert_eq!(out.iter().filter(|&&v| v == 1 << 22).count(), filled, "{times} times {records}: {out:?}");
        assert!(out.iter().all(|&v| v == 0 || v == 1 << 22));
        let expected_spent = if rest() != 0 {
            BUDGET.min((1 << 26) * times * records as u32)
        } else {
            filled as u32 * (1 << 26) * times
        };
        assert_eq!(spent, expected_spent);
    }
}

#[test]
fn warp_window_dispatch_clamps_garbage_tail_to_empty() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let (src, src_ptr, _) = warp.shared_buffer(4096);
    let (_budget, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (back, back_ptr, _) = warp.shared_buffer(4096);
    let dst = warp.default_uav(4096);
    // SAFETY: mapped buffers; nothing runs yet.
    unsafe {
        std::ptr::write_bytes(budget_ptr, 0, 256);
        // Slot 0: valid dispatch (10, 2, 1) -> cost 20 * 64 = 1280.
        // Slot 1: unfilled garbage tail (u32::MAX, 1, 1) -> must run empty (0, 0, 0) when ahead=true.
        for (i, words) in [[10u32, 2, 1], [u32::MAX, 1, 1]].into_iter().enumerate() {
            for (w, v) in words.into_iter().enumerate() {
                src_ptr.cast::<u32>().add(i * 3 + w).write_volatile(v);
            }
        }
    }
    assert!(record_clamp_as(
        &warp.clamp,
        &warp.list,
        (&dst, D3D12_RESOURCE_STATE_COMMON),
        budget_gpu,
        (&src, 0),
        None,
        (2, 12, 2),
        true,
    ));
    // SAFETY: open list; dst is readable as indirect arguments.
    unsafe {
        warp.list.ResourceBarrier(&[transition(
            &dst,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        warp.list.CopyBufferRegion(&back, 0, &dst, 0, 24);
    }
    warp.submit_and_wait();
    // SAFETY: list finished; mapped words.
    let out: Vec<u32> = (0..6)
        .map(|i| unsafe { back_ptr.cast::<u32>().add(i).read_volatile() })
        .collect();
    let spent = unsafe { budget_ptr.cast::<u32>().read_volatile() };
    assert_eq!(out, [10, 2, 1, 0, 0, 0]);
    assert_eq!(spent, 10 * 2 * 64);
}

#[test]
fn warp_batch_mesh_all_asked_counts_repeated_executions() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let pass = make_from(&warp.device, batch::SOURCE).expect("batched clamp compiles on WARP");
    let (_src, src_ptr, src_gpu) = warp.shared_buffer(4096);
    let (_budget, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (_table, table_ptr, table_gpu) = warp.shared_buffer(batch::TABLE);
    let dst = warp.default_uav(64 << 10);
    // SAFETY: mapped buffers; list has not run yet.
    unsafe {
        std::ptr::write_bytes(budget_ptr, 0, 256);
        for (w, v) in [12u32, 1, 1].into_iter().enumerate() {
            src_ptr.cast::<u32>().add(w).write_volatile(v);
        }
    }
    batch::record(
        &pass,
        &warp.list,
        (&dst, D3D12_RESOURCE_STATE_COMMON),
        budget_gpu,
        [src_gpu; 4],
        table_gpu,
    );
    // SAFETY: mapped table; list has not run yet.
    let mut table = unsafe { batch::Table::new(table_ptr, 0, [1, 0, 0, 0]) };
    for _ in 0..3 {
        assert_eq!(table.place(1, 0, 10), Some(0));
    }
    warp.submit_and_wait();
    // SAFETY: list finished; mapped words.
    let words: [u32; 3] = std::array::from_fn(|i| unsafe {
        budget_ptr.cast::<u32>().add(i).read_volatile()
    });
    assert_eq!(words[0], 12 * 128 * 3, "budget spent");
    assert_eq!(words[1], 12, "largest mesh dispatch asked");
    assert_eq!(words[2], 36, "total mesh groups asked across 3 executions");
}

/// Two batches through a host list, twice (the second recording finds
/// the scratch buffer as the first one's end left it): executions with
/// nothing between them read what they asked for, of two source buffers,
/// the second batch while the first one's places are still read. With
/// the debug layer on no barrier names a state the buffer is not in.
#[test]
fn host_list_batches() {
    let Some(host) = host() else {
        return;
    };
    let _ = host.log_messages();
    let Ok(list) = host.new_list() else {
        return;
    };
    let worker = compile_cs(&host.device, WORKER_CS).expect("worker shader must compile");
    let signature = one_argument(&host.device, 2, 12);
    let mut sources = Vec::new();
    for first in [1u32, 100] {
        let Some((buffer, ptr)) = host.shared_buffer(1024) else {
            return;
        };
        for i in 0..16u32 {
            for (w, word) in [first + i, 1, 1].into_iter().enumerate() {
                // SAFETY: inside the mapping.
                unsafe { ptr.cast::<u32>().add(i as usize * 8 + w).write_volatile(word) };
            }
        }
        sources.push(buffer);
    }
    let Some((hits_buf, hits_ptr)) = host.shared_buffer(256) else {
        return;
    };
    // SAFETY: a live buffer.
    let hits_gpu = unsafe { hits_buf.GetGPUVirtualAddress() };
    for _ in 0..2 {
        // SAFETY: the mapping holds 256 bytes; nothing runs.
        unsafe { std::ptr::write_bytes(hits_ptr, 0, 256) };
        let mut expected = 0;
        // More batches than one scratch buffer holds.
        for k in 0..12u32 {
            let (mut table, scratch) =
                open_batch(&list, (1, &sources[0]), &[(1, sources[0].clone()), (2, sources[1].clone())]).unwrap();
            // SAFETY: an open list; the worker's root signature and
            // pipeline; buffers the test holds until the list has run.
            unsafe {
                list.list.SetComputeRootSignature(&worker.root);
                list.list.SetPipelineState(&worker.pso);
                list.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
            }
            for (id, record) in [(1u64, 3 + k % 4), (2, 5), (1, 0), (2, 5)] {
                let at = table.place(id, u64::from(record) * 32, 2).unwrap();
                // SAFETY: as above.
                unsafe { list.list.ExecuteIndirect(&signature, 1, &scratch, at, None, 0) };
                expected += record + if id == 1 { 1 } else { 100 };
            }
        }
        list.close();
        assert_eq!(host.log_messages(), None, "the debug layer found an error");
        host.execute(&[&list]);
        list.reset();
        // SAFETY: the list has run; a mapped buffer.
        let hits = unsafe { (hits_ptr as *const u32).read_volatile() };
        assert_eq!(hits, expected, "every execution runs what it asked for");
    }
}

/// What clamps of a frame cost on the card one by one and in batches
/// (ignored: a measurement): 2,355 executions of an empty dispatch, in
/// 49 lists' worth of runs, as in the captured frame.
#[test]
#[ignore]
fn batch_costs() {
    let Some(mut gpu) = WarpHarness::on(false) else {
        return;
    };
    const EXECUTIONS: u32 = 2355;
    let pass = make_from(&gpu.device, batch::SOURCE).expect("the batched clamp must compile");
    let sig = gpu.signature(2, 12);
    let groups: Vec<u32> = vec![1; 2048];
    let src = dispatch_records(&gpu, &groups, 32);
    // SAFETY: a live buffer.
    let src_gpu = unsafe { src.GetGPUVirtualAddress() };
    let budget = gpu.default_uav(256);
    let hits = gpu.default_uav(256);
    // SAFETY: live buffers.
    let (budget_gpu, hits_gpu) = unsafe { (budget.GetGPUVirtualAddress(), hits.GetGPUVirtualAddress()) };
    let (_tables, tables_ptr, tables_gpu) = gpu.shared_buffer(batch::TABLE * 2400);
    let dst = gpu.default_uav(4 << 20);
    for run in [0u32, 2355 / 49, 5, 1] {
        let mut best = f64::MAX;
        for _ in 0..6 {
            let mut state = D3D12_RESOURCE_STATE_COMMON;
            let mut table = batch::Table::default();
            for i in 0..EXECUTIONS {
                if run == 0 {
                    assert!(record_clamp_as(&gpu.clamp, &gpu.list, (&dst, state), budget_gpu, (&src, u64::from(i % 2048) * 32), None, (2, 12, 1), false));
                } else if i % run == 0 {
                    let k = u64::from(i / run) * batch::TABLE;
                    batch::record(&pass, &gpu.list, (&dst, state), budget_gpu, [src_gpu; 4], tables_gpu + k);
                    // SAFETY: the mapped table; the list has not run.
                    table = unsafe { batch::Table::new(tables_ptr.add(k as usize), 0, [1, 0, 0, 0]) };
                }
                state = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
                let at = if run == 0 { 0 } else { table.place(1, u64::from(i % 2048) * 32, 2).unwrap() };
                // SAFETY: an open list; live buffers.
                unsafe {
                    if run == 0 || i % run == 0 {
                        gpu.list.SetComputeRootSignature(&gpu.worker.root);
                        gpu.list.SetPipelineState(&gpu.worker.pso);
                        gpu.list.SetComputeRootUnorderedAccessView(0, hits_gpu);
                    }
                    gpu.list.ExecuteIndirect(&sig, 1, &dst, at, None, 0);
                }
            }
            let t = std::time::Instant::now();
            gpu.submit_and_wait();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        let what = if run == 0 { "each clamped on its own".to_owned() } else { format!("a batch every {run}") };
        println!("{EXECUTIONS} indirect dispatches, {what}: {best:.2} ms");
    }
}
