use super::*;


fn pso(pso: u32) -> Cmd {
    Cmd::Pipeline { pso }
}
fn go() -> Cmd {
    Cmd::Dispatch { groups: [1, 1, 1] }
}
fn uav(res: u32) -> Barrier {
    Barrier::Uav { flags: 0, res }
}
fn moved(res: u32, before: i32, after: i32) -> Barrier {
    Barrier::Transition {
        flags: 0,
        res,
        sub: ALL,
        before,
        after,
    }
}
fn barriers(all: &[Barrier]) -> Cmd {
    Cmd::Barriers { all: all.to_vec() }
}

#[test]
fn pipelines_are_skipped() {
    let list = vec![
        pso(1),
        go(),
        pso(2),
        go(),
        go(),
        Cmd::CopyResource { dst: 1, src: 2 },
        pso(1),
        go(),
    ];
    let (out, n) = skip_pipelines(list.clone(), &HashSet::from([2]));
    assert_eq!(n, 2);
    assert_eq!(
        out,
        vec![
            pso(1),
            go(),
            pso(2),
            Cmd::CopyResource { dst: 1, src: 2 },
            pso(1),
            go()
        ]
    );
    assert_eq!(skip_pipelines(list.clone(), &HashSet::new()), (list, 0));
}

#[test]
fn uav_barriers_are_dropped() {
    let list = vec![
        barriers(&[uav(0)]),
        go(),
        barriers(&[uav(1), moved(1, 8, 4)]),
    ];
    assert_eq!(
        no_uav_barriers(list.clone()),
        (vec![go(), barriers(&[moved(1, 8, 4)])], 2)
    );
    assert_eq!(
        barriers_only(list.clone()),
        vec![list[0].clone(), list[2].clone()]
    );
}

#[test]
fn barriers_are_merged() {
    let list = vec![
        barriers(&[moved(1, 0, 8)]),
        pso(1),
        barriers(&[uav(2), moved(1, 8, 64)]),
        barriers(&[uav(0), uav(3)]),
        go(),
        // There and back again: nothing is left but the wait.
        barriers(&[moved(1, 8, 64)]),
        barriers(&[moved(1, 64, 8)]),
        go(),
        barriers(&[moved(4, 4, 128)]),
        barriers(&[moved(4, 128, 4)]),
    ];
    let (out, saved) = merge_barriers(list);
    assert_eq!(saved, 4);
    assert_eq!(
        out,
        vec![
            barriers(&[moved(1, 0, 64), uav(0)]),
            pso(1),
            go(),
            barriers(&[uav(1)]),
            go()
        ]
    );
    // Parts of a resource are not mixed up.
    let part = |sub, before, after| Barrier::Transition {
        flags: 0,
        res: 1,
        sub,
        before,
        after,
    };
    let list = vec![
        barriers(&[part(0, 4, 8)]),
        barriers(&[part(1, 4, 8)]),
        barriers(&[part(ALL, 8, 64)]),
    ];
    assert_eq!(
        merge_barriers(list).0,
        vec![barriers(&[part(0, 4, 8), part(1, 4, 8), part(ALL, 8, 64)])]
    );
}

#[test]
fn clamps_are_found_kept_and_skipped() {
    let clamp = |scratch, stride, records| {
        vec![
            barriers(&[uav(0), moved(scratch, 0, 8)]),
            pso(9),
            Cmd::Constants {
                graphics: false,
                param: 0,
                offset: 0,
                values: vec![10, stride, records, 0, 0, 0],
            },
            Cmd::RootView {
                graphics: false,
                kind: 3,
                param: 3,
                va: Va::At {
                    res: scratch,
                    offset: 0,
                },
            },
            go(),
            barriers(&[uav(0), moved(scratch, 8, 0x200)]),
        ]
    };
    let run = |args, at| Cmd::Indirect {
        signature: 1,
        most: 1,
        args,
        at,
        count: 0,
        count_at: 0,
    };
    let mut list = clamp(5, 12, 2);
    list.extend([pso(1), run(5, 12), run(5, 0), run(6, 0)]);
    list.extend(clamp(5, 12, 100));
    list.extend([pso(1), run(5, 24), barriers(&[moved(5, 0x200, 0)])]);
    let mut next = 0;
    let found = find_clamps(&list, &HashSet::from([9]), &mut next);
    assert_eq!(
        found
            .iter()
            .map(|c| (c.scratch, c.bytes, c.result))
            .collect::<Vec<_>>(),
        [(5, 24, 0), (5, 1200, 256)]
    );
    assert_eq!(next, 256 + 1280);
    let kept = keep_clamped(list.clone(), &found);
    assert_eq!(kept.len(), list.len() + 6);
    assert_eq!(
        kept[7],
        Cmd::CopyBuffer {
            dst: RESULTS,
            dst_at: 0,
            src: 5,
            src_at: 0,
            len: 24
        }
    );
    assert_eq!(
        kept.iter()
            .filter(|c| matches!(c, Cmd::CopyBuffer { .. }))
            .count(),
        2
    );
    let skipped = skip_clamps(list, &found);
    assert!(
        !skipped
            .iter()
            .any(|c| matches!(c, Cmd::Dispatch { .. } | Cmd::Barriers { .. }))
    );
    let runs: Vec<(u32, u64)> = skipped
        .iter()
        .filter_map(|c| {
            if let Cmd::Indirect { args, at, .. } = c {
                Some((*args, *at))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        runs,
        [(RESULTS, 12), (RESULTS, 0), (6, 0), (RESULTS, 256 + 24)]
    );
}

#[test]
fn clamps_are_batched() {
    use crate::execution::indirect::batch::job;
    let clamp = |kind, offset, records, source| {
        let view = |param, res| Cmd::RootView {
            graphics: false,
            kind: 2,
            param,
            va: Va::At { res, offset: 0 },
        };
        vec![
            barriers(&[uav(0), moved(5, 0x200, 8)]),
            Cmd::RootSignature {
                graphics: false,
                root: 8,
            },
            pso(9),
            Cmd::Constants {
                graphics: false,
                param: 0,
                offset: 0,
                values: vec![kind, 32, records, offset, 0, 0],
            },
            view(1, source),
            view(2, source),
            view(3, 5),
            view(4, 6),
            go(),
            barriers(&[uav(0), moved(5, 8, 0x200)]),
        ]
    };
    let run = |at| Cmd::Indirect {
        signature: 1,
        most: 1,
        args: 5,
        at,
        count: 0,
        count_at: 0,
    };
    // A dispatch, two indirect ones with a wait of the title's between
    // them, then draws: a window of two and another buffer's.
    let mut list = vec![pso(1), go()];
    list.extend(clamp(2, 0, 1, 70));
    list.extend([pso(1), run(0)]);
    list.extend(clamp(2, 64, 1, 70));
    list.extend([pso(1), run(0), barriers(&[uav(0)])]);
    list.extend(clamp(2, 64, 1, 70));
    list.extend([pso(1), run(0)]);
    list.extend(clamp(10, 128, 2, 70));
    list.extend([pso(2), run(0), run(32), run(32)]);
    list.extend(clamp(10, 256, 1, 71));
    list.extend([pso(2), run(0)]);
    let (clamps, compute) = (HashSet::from([9]), HashSet::from([1]));
    let table = 16 + 16 * 2048;
    for (careful, batches) in [(false, 3), (true, 4)] {
        let mut jobs = Vec::new();
        let (out, done) = batch_clamps(list.clone(), &clamps, &compute, 99, careful, &mut jobs);
        assert_eq!((done.clamps, done.batches, done.jobs), (5, batches, 6), "careful {careful}");
        assert_eq!(jobs.len(), table * batches as usize);
        let passes = out.iter().filter(|c| matches!(c, Cmd::Pipeline { pso: BATCH })).count();
        assert_eq!(passes, batches as usize);
        // Nothing of the clamps is left; the title's wait is.
        assert!(!out.contains(&pso(9)) && out.contains(&barriers(&[uav(0)])));
        let names_5 = |c: &Cmd| matches!(c, Cmd::Barriers { all } if all.iter().any(|b| matches!(b, Barrier::Transition { res: 5, .. })));
        assert!(!out.iter().any(names_5));
        let places: Vec<u64> = out
            .iter()
            .filter_map(|c| match c {
                Cmd::Indirect { args: 99, at, .. } => Some(*at),
                _ => None,
            })
            .collect();
        assert_eq!(places, [0, 32, 64, 96, 128, 128, 160]);
        // The last table: two records, one read twice, then one of the
        // other buffer.
        let word = |i: usize| {
            let at = jobs.len() - table + 4 * i;
            u32::from_le_bytes(jobs[at..at + 4].try_into().unwrap())
        };
        assert_eq!(word(0), 3);
        let all: Vec<u32> = (4..16).map(word).collect();
        assert_eq!(
            all,
            [
                [job(0, 10, 1), 128, 96, 0],
                [job(0, 10, 2), 160, 128, 0],
                [job(1, 10, 1), 256, 160, 0]
            ]
            .concat()
        );
        let sources: Vec<u32> = out
            .iter()
            .filter_map(|c| match c {
                Cmd::RootView { param: 1, va: Va::At { res, .. }, .. } => Some(*res),
                _ => None,
            })
            .collect();
        assert_eq!(sources.last(), Some(&71));
        // It ends common, as it began.
        assert_eq!(out.last(), Some(&barriers(&[moved(99, 0x200, 0)])));
    }
    // A list without clamps is left as it is.
    let plain = vec![pso(1), go(), barriers(&[uav(0)])];
    let (out, done) = batch_clamps(plain.clone(), &clamps, &compute, 99, false, &mut Vec::new());
    assert_eq!((out, done), (plain, Batched::default()));
}
