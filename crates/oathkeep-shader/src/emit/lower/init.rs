use super::*;

impl<'a> Lower<'a> {
    pub(in crate::emit::lower) fn new(x: &'a Parts<'a>, bad: &'a HashMap<Value, Why>) -> Self {
        let p = x.p;
        let mut phi_at = HashMap::new();
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, phi) in block.phis.iter().enumerate() {
                phi_at.insert(phi.value, (b, i));
            }
        }
        let mut s = Shader::new(x.native.stage == HwStage::Ps);
        s.inputs = x.inputs.clone();
        s.outputs = x.outputs.clone();
        let insts = || p.blocks.iter().flat_map(|b| &b.insts);
        let writes_uavs = insts().any(|i| {
            matches!(i.fields, Fields::Image { .. } | Fields::Buffer { .. })
                && (i.name().contains("store") || i.name().contains("atomic"))
        });
        let mut console_lanes = false;
        // LDS accesses get a group barrier each (a console wave split in two host waves)
        let mut split_lds = false;
        // with `split_lds`, LDS accesses in lane branches: the blocks every lane runs, and
        // where the host waves meet around those branches (`lds_meetings`)
        let mut lds_meet: Option<(Vec<bool>, Vec<(bool, bool)>)> = None;
        // a console wave64 program in two host waves of 32 (`exchange`)
        let mut exchange: Option<(u32, bool, Vec<bool>)> = None;
        // multi-wave widened execution (`group > 64` with `st.wide`)
        let mut group_wide = false;
        // a console wave64 program in host waves of 32: the LDS dword of `wave_base`'s counter
        let mut halves: Option<u32> = None;
        let local_waves = LOCAL_WAVES.get();
        if let StageIo::Cs(cs) = x.native.io {
            s.threads = Some(cs.threads);
            s.lds_dwords = cs.lds_bytes() / 4;
            // Code reading other lanes needs the console's wave size and
            // lane order; so do lane masks read as numbers, and LDS shared
            // within one wave, where the console compiler leaves out barriers.
            let lane_ordered = insts().any(|i| {
                let n = i.name();
                n.starts_with("ds_swizzle")
                    || n.contains("permute")
                    || n.starts_with("v_permlane")
                    || matches!(n, "v_readlane_b32" | "v_writelane_b32")
                    || matches!(&i.fields, Fields::Valu(v) if v.dpp.is_some())
            }) || lane_constant_masks(x.st)
                || !x.st.wide.is_empty();
            console_lanes = lane_ordered
                || insts().any(|i| i.name().starts_with("v_mbcnt") || reads_mask_as_data(x.st, i));
            let mut wave = if x.native.wave64 { 64 } else { 32 };
            let group = cs.threads.iter().product::<u32>();
            let one_wave = group <= wave;
            // A host whose waves are at most 32 lanes (NVIDIA) refuses `WaveSize(64)`. A console
            // wave64 program whose cross-lane code stays inside 32-lane halves – DPP rows,
            // `ds_swizzle`, `v_permlane16` / `v_permlanex16`, which the ISA confines there – runs
            // exactly in waves of 32: the thread relabelling makes each host wave one half of a
            // console wave (lanes 32k..32k+31). Not when it crosses halves (`v_mbcnt`,
            // `v_readlane`, `v_writelane`, `ds_(b)permute`, masks as numbers or lane-constant
            // masks), nor when LDS may be shared within the console wave without barriers (a
            // group over 32 threads with LDS): those still need 64-lane waves.
            if wave == 64 && host_max_wave() == 32 && (console_lanes || one_wave && s.lds_dwords > 0) {
                let crosses = insts().any(|i| {
                    let n = i.name();
                    n.starts_with("v_mbcnt")
                        || matches!(n, "v_readlane_b32" | "v_writelane_b32")
                        || n.starts_with("ds_permute")
                        || n.starts_with("ds_bpermute")
                        || reads_mask_as_data(x.st, i)
                }) || lane_constant_masks(x.st)
                    || !x.st.wide.is_empty();
                if !crosses && (s.lds_dwords == 0 || group <= 32) {
                    wave = 32;
                } else {
                    // (Groups of several console waves too: each console wave is two host
                    // waves, which exchange through their own pair of dwords.)
                    // LDS the console shares within its one wave without barriers: split into
                    // two waves of 32, a group barrier before every LDS access keeps their order
                    // – where every thread of the group gets there (no lane branch around it,
                    // no guard), else a barrier would hang the group
                    let lds = |i: &Inst| {
                        matches!(i.fields, Fields::Ds { .. })
                            && !(i.name().starts_with("ds_swizzle") || i.name().contains("permute"))
                    };
                    if let Some(all) = all_lane_blocks(x.st) {
                        let ok = p.blocks.iter().enumerate().all(|(b, blk)| {
                            blk.insts.iter().all(|inst| {
                                !lds(inst) || all.get(b).copied().unwrap_or(false)
                            })
                        });
                        let mut lds_ok = !(s.lds_dwords > 0 && group > 32) || ok;
                        if !lds_ok {
                            match lds_meetings(p, x.st, x.io, cs.threads, &all) {
                                Ok(meet) => {
                                    lds_meet = Some((all.clone(), meet));
                                    lds_ok = true;
                                }
                                Err(bad) => {
                                    // Only widen branches enclosing regions whose LDS accesses
                                    // actually failed ordering — leave already-safe regions
                                    // (e.g. lock-free CAS / disjoint-atomic regions) untouched.
                                    for b in bad {
                                        need(b);
                                    }
                                }
                            }
                        }
                        if lds_ok {
                            wave = 32;
                            split_lds = s.lds_dwords > 0 && group > 32;
                            if crosses {
                                // lanes 32..63 are the other host wave: masks, lane reads and lane
                                // numbers go through two exchange dwords past the program's LDS
                                // (local wave-independent lowering touches no exchange LDS)
                                exchange = Some((s.lds_dwords, group <= 32, all));
                                if !local_waves {
                                    s.lds_dwords += 2 * group.div_ceil(64);
                                }
                                group_wide = group > 64 && !x.st.wide.is_empty() && !local_waves;
                            }
                        }
                    }
                }
                // `OATHKEEP_WAVE_WHY=1`: why a program keeps 64-lane waves (one line per reason)
                if wave == 64 && std::env::var_os("OATHKEEP_WAVE_WHY").is_some() {
                    let mut why: Vec<&str> = Vec::new();
                    for i in insts() {
                        let n = i.name();
                        let r = if n.starts_with("v_mbcnt") {
                            "v_mbcnt"
                        } else if n == "v_readlane_b32" {
                            "v_readlane"
                        } else if n == "v_writelane_b32" {
                            "v_writelane"
                        } else if n.starts_with("ds_permute") || n.starts_with("ds_bpermute") {
                            "ds_(b)permute"
                        } else if reads_mask_as_data(x.st, i) {
                            "mask as data"
                        } else {
                            continue;
                        };
                        if !why.contains(&r) {
                            why.push(r);
                        }
                    }
                    if lane_constant_masks(x.st) {
                        why.push("lane-constant mask");
                    }
                    if s.lds_dwords > 0 && group > 32 {
                        why.push("LDS in a group over 32");
                        match all_lane_blocks(x.st) {
                            None => why.push("an indirect exit"),
                            Some(all) if lds_meetings(p, x.st, x.io, cs.threads, &all).is_err() => {
                                why.push("an LDS read and write in one lane-branch region")
                            }
                            Some(_) => {}
                        }
                    }
                    why.sort_unstable();
                    eprintln!("WAVE64-WHY group={group} lds={} [{}]", s.lds_dwords > 0, why.join(", "));
                }
            }
            if wave == 32 && x.native.wave64 && (!local_waves || lane_ordered) {
                // console threads go to host waves in the order the waves take them (`wave_base`)
                halves = Some(s.lds_dwords);
                s.lds_dwords += 1;
            }
            if console_lanes || one_wave && s.lds_dwords > 0 {
                s.wave_size = Some(wave);
            }
        }
        // The title's barriers a thread of the group may never reach, which
        // leaves the others waiting on the host: those in blocks some lanes
        // do not run, and all of them in a program with a loop closed by a
        // plain jump (a lane still in it at the iteration limit leaves the
        // program there, `abort_block`).
        let fence_only: HashSet<usize> = if insts().any(|i| i.name() == "s_barrier") {
            let order = crate::ir::reverse_postorder(&p.blocks);
            let mut place = vec![usize::MAX; p.blocks.len()];
            for (n, &b) in order.iter().enumerate() {
                place[b] = n;
            }
            let back = |from: usize, to: usize| {
                let at = |b: usize| place.get(b).copied().unwrap_or(usize::MAX);
                at(from) != usize::MAX && at(to) <= at(from)
            };
            let cut_short = x
                .st
                .exits
                .iter()
                .enumerate()
                .any(|(b, e)| matches!(*e, LaneExit::Jump(t) if back(b, t)));
            let all = all_lane_blocks(x.st);
            (0..p.blocks.len())
                .filter(|&b| p.blocks[b].insts.iter().any(|i| i.name() == "s_barrier"))
                .filter(|&b| {
                    cut_short
                        || all
                            .as_ref()
                            .is_none_or(|all| !all.get(b).copied().unwrap_or(false))
                })
                .collect()
        } else {
            HashSet::new()
        };
        let mut no_barrier = HashSet::new();
        if let StageIo::Ms(ms) = &x.native.io {
            s.prims = x.prims.clone();
            s.threads = Some([ms.threads, 1, 1]);
            s.lds_dwords = ms.lds_bytes.div_ceil(4);
            s.mesh = Some(crate::emit::shader::Mesh {
                max_vertices: ms.max_vertices,
                max_primitives: ms.max_primitives,
                topology: ms.topology,
            });
            // Barriers order the lanes' LDS accesses; the console's
            // allocation request needs them too, but the host does not. One
            // lane sends that request, and what it dominates (the path for
            // counts out of range) runs in that lane alone, so its barrier
            // would hang the host.
            let order = crate::ir::reverse_postorder(&p.blocks);
            let mut index = vec![usize::MAX; p.blocks.len()];
            for (n, &b) in order.iter().enumerate() {
                index[b] = n;
            }
            let idom = crate::ir::idoms(&p.blocks, &order, &index);
            let alloc: Vec<usize> = (0..p.blocks.len())
                .filter(|&b| {
                    p.blocks[b].insts.iter().any(|i| {
                        i.name() == "s_sendmsg"
                            && matches!(i.fields, Fields::Sopp { imm } if imm & 0xF == GS_ALLOC_REQ)
                    })
                })
                .collect();
            for &b in &order {
                let mut d = b;
                let dominated = loop {
                    if alloc.contains(&d) {
                        break true;
                    }
                    if idom[d] == d || idom[d] == usize::MAX {
                        break false;
                    }
                    d = idom[d];
                };
                if ms.lds_bytes == 0 || dominated {
                    no_barrier.insert(b);
                }
            }
        }
        Lower {
            s,
            x,
            bad,
            lane_fail: HashMap::new(),
            vals: HashMap::new(),
            floats: HashMap::new(),
            block: vec![None; p.blocks.len()],
            cur: 0,
            bools: HashMap::new(),
            lane_bits: HashMap::new(),
            phi_at,
            mask_phis: HashMap::new(),
            pending_masks: Vec::new(),
            data_phis: Vec::new(),
            read_phis: HashSet::new(),
            stale: HashSet::new(),
            coherent: HashSet::new(),
            root_copy: None,
            page_there: None,
            ordered: if x.native.stage == HwStage::Ps {
                ordered_section(p)
            } else {
                HashSet::new()
            },
            params: x.res.params.iter().map(|&(v, p, d)| (v, (p, d))).collect(),
            user_data: None,
            user_reads: BTreeMap::new(),
            samplers: Vec::new(),
            descriptors: x
                .res
                .descriptors
                .iter()
                .map(|d| ((d.site, d.role == Role::Sampler), &d.source))
                .collect(),
            scalar: x.res.scalar.iter().copied().collect(),
            exports: x.io.exports.iter().map(|e| (e.site, e)).collect(),
            interps: {
                let mut m: HashMap<Site, &Interp> = x.io.interps.iter().map(|(s, i)| (*s, i)).collect();
                // A first half whose result is also read as data (an f16 second half keeps
                // the other half of its dst, PS_DeferredTextured): evaluated as the pair it
                // starts. Approximate for those other bits (P0 + i P10 on the console); a
                // first half only the second half reads is gone before lowering.
                // Only in the second translation of a program the first
                // one refused for it ([`with_first_half_pairs`]).
                if FIRST_HALF_PAIRS.get() && std::env::var_os("OATHKEEP_FIRST_HALF").is_none_or(|v| v != "0") {
                    for (_, i) in &x.io.interps {
                        if let Interp::Interpolated { first, .. } = i {
                            for s in first {
                                m.entry(*s).or_insert(i);
                            }
                        }
                    }
                }
                m
            },
            handles: HashMap::new(),
            strides: HashMap::new(),
            edges: HashMap::new(),
            exported: vec![false; p.blocks.len()],
            rpo: vec![usize::MAX; p.blocks.len()],
            tail: vec![None; p.blocks.len()],
            writes_uavs,
            src0: None,
            console_lanes,
            split_lds,
            lds_meet,
            local: exchange.is_some() && LOCAL_WAVES.get(),
            local_ok: false,
            local_any: LOCAL_ANY.get(),
            uniform: match &exchange {
                Some((_, false, all)) if !LOCAL_WAVES.get() => {
                    lane_uniform(p, x.st, all, split_lds && crate::emit::group_sync() != 0)
                }
                _ => HashSet::new(),
            },
            own_bits: match &exchange {
                Some((_, false, _)) if !LOCAL_WAVES.get() && own_bits_on() => {
                    crate::emit::own_bits::find(p, x.st, std::env::var_os("OATHKEEP_WAVE_WHY").is_some())
                }
                _ => Default::default(),
            },
            exchange,
            group_wide,
            clane: None,
            pair: None,
            halves,
            wave_base: None,
            flat: None,
            no_barrier,
            fence_only,
            half_outputs: half_outputs(&x.native.io, &x.outputs),
            mask_data: match x.native.stage {
                HwStage::Cs => MaskData::Console,
                HwStage::Ps | HwStage::Vs => MaskData::Host,
                _ => MaskData::None,
            },
            mask_ok: false,
            ballots: HashMap::new(),
            balloting: HashSet::new(),
            def_block: p
                .blocks
                .iter()
                .enumerate()
                .flat_map(|(b, block)| {
                    block
                        .insts
                        .iter()
                        .flat_map(move |i| i.writes.iter().map(move |w| (w.1, b)))
                        .chain(block.phis.iter().map(move |phi| (phi.value, b)))
                })
                .collect(),
            cur_ir: 0,
            sections: HashMap::new(),
            section_out: HashSet::new(),
            code_table: None,
            counters: HashMap::new(),
            abort: None,
        }
    }
}
