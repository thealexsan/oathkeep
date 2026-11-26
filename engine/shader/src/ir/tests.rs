use crate::ShaderError;
use super::cfg::remove_trivial_phis_steps;
use super::*;

const S_ENDPGM: u32 = 0xBF81_0000;

fn build_words(words: &[u32], wave64: bool) -> Program {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    build(&code, wave64, 256).unwrap()
}

fn input(p: &Program, reg: Reg) -> Value {
    p.inputs.iter().find(|i| i.0 == reg).expect("input").1
}

/// `if (v0 < s0) { ... }` whose body changes s6: `body` are its words, `after` what
/// follows the join.
fn skip(body: &[u32], after: &[u32]) -> Program {
    let mut w = vec![
        0xBE94_0306,                     // s_mov_b32 s20, s6
        0x7D88_0000,                     // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A,                     // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | body.len() as u32, // s_cbranch_execz past the body
    ];
    w.extend_from_slice(body);
    w.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    w.extend_from_slice(after);
    w.push(S_ENDPGM);
    build_words(&w, true)
}

fn skips(p: &Program) -> usize {
    p.blocks
        .iter()
        .filter(|b| b.insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
        .count()
}

#[test]
fn a_skip_is_left_out_when_the_scalars_it_changes_come_back_or_are_not_read() {
    const SET: u32 = 0xBE86_0385; // s_mov_b32 s6, 5
    const USE: u32 = 0x7E02_0206; // v_mov_b32 v1, s6
    const BACK: u32 = 0xBE86_0314; // s_mov_b32 s6, s20
    const READ: u32 = 0x7E04_0206; // v_mov_b32 v2, s6
    // s6 is changed in the body and read after the join: the branch stays
    let kept = skip(&[SET, USE], &[READ]);
    assert_eq!(skips(&kept), 1);
    assert!(without_skips(&kept, false).is_none());
    assert!(without_skips(&kept, true).is_none());
    // ... put back by a copy of what it held (as a scalar parked in a VGPR lane comes
    // back): every lane may walk the body
    let back = skip(&[SET, USE, BACK], &[READ]);
    assert!(without_skips(&back, false).is_none());
    assert_eq!(skips(&without_skips(&back, true).expect("left out")), 0);
    // ... or not read after the join at all
    let unread = skip(&[SET, USE], &[]);
    assert_eq!(skips(&without_skips(&unread, false).expect("left out")), 0);
    assert_eq!(skips(&without_skips(&unread, true).expect("left out")), 0);
    // ... or read only by a restore of EXEC that nothing uses before the next one
    // (s_mov_b64 exec, s[6:7]; s_mov_b64 exec, s[12:13]; v_mov_b32 v2, s0)
    let idle = skip(&[SET, USE], &[0xBEFE_0406, 0xBEFE_040C, 0x7E04_0200]);
    assert!(without_skips(&idle, false).is_none());
    assert_eq!(skips(&without_skips(&idle, true).expect("left out")), 0);
}

#[test]
fn joins_get_phis_only_where_values_differ() {
    let code = [
        0xBF06_0100, // s_cmp_eq_u32 s0, s1
        0xBF85_0001, // s_cbranch_scc1 -> 12
        0xBE82_0380, // s_mov_b32 s2, 0
        0x7E00_0202, // 12: v_mov_b32 v0, s2
        0x7E02_0200, // v_mov_b32 v1, s0
        S_ENDPGM,
    ];
    for wave64 in [false, true] {
        let p = build_words(&code, wave64);
        let [b0, b1, b2] = &p.blocks[..] else {
            panic!("{:?}", p.blocks)
        };
        assert_eq!(
            b0.exit,
            Exit::Branch {
                taken: 2,
                not_taken: 1
            }
        );
        assert_eq!(b1.exit, Exit::Jump(2));
        assert_eq!(b2.preds, [0, 1]);
        assert_eq!(b0.insts[1].read(Reg::Scc), b0.insts[0].write(Reg::Scc));

        // s2 differs per path; s0, EXEC, v0 and v1 do not.
        let [phi] = &b2.phis[..] else {
            panic!("{:?}", b2.phis)
        };
        assert_eq!(phi.reg, Reg::S(2));
        assert_eq!(
            phi.args,
            [input(&p, Reg::S(2)), b1.insts[0].write(Reg::S(2)).unwrap()]
        );
        let [mov0, mov1, end] = &b2.insts[..] else {
            panic!()
        };
        assert_eq!(mov0.read(Reg::S(2)), Some(phi.value));
        assert_eq!(mov1.read(Reg::S(0)), Some(input(&p, Reg::S(0))));
        // The old v1 is read: lanes outside EXEC keep it.
        assert_eq!(mov1.read(Reg::V(1)), Some(input(&p, Reg::V(1))));
        assert_eq!(mov1.read(Reg::S(EXEC)), Some(input(&p, Reg::S(EXEC))));
        assert_eq!(mov1.read(Reg::S(EXEC + 1)).is_some(), wave64);
        assert_eq!((end.name(), b2.exit), ("s_endpgm", Exit::End));
    }
}

#[test]
fn in_place_scalar_ops_read_their_destination() {
    let p = build_words(
        &[
            0xBE8D_1D94, // s_bitset1_b32 s13, 20
            0xB78E_0010, // s_addk_i32 s14, 0x10
            0xBE8F_0380, // s_mov_b32 s15, 0
            S_ENDPGM,
        ],
        false,
    );
    let [bitset, addk, mov, _] = &p.blocks[0].insts[..] else {
        panic!("{:?}", p.blocks[0].insts)
    };
    assert_eq!(bitset.read(Reg::S(13)), Some(input(&p, Reg::S(13))));
    assert_eq!(addk.read(Reg::S(14)), Some(input(&p, Reg::S(14))));
    assert_eq!(mov.read(Reg::S(15)), None);
}

#[test]
fn loops_carry_values_through_phis() {
    let p = build_words(
        &[
            0xBE80_0380, // s_mov_b32 s0, 0
            0x8000_8100, // 4: s_add_u32 s0, s0, 1
            0xBF07_8400, // s_cmp_lg_u32 s0, 4
            0xBF85_FFFD, // s_cbranch_scc1 -> 4
            S_ENDPGM,
        ],
        true,
    );
    assert!(p.inputs.is_empty(), "{:?}", p.inputs);
    let [b0, b1, b2] = &p.blocks[..] else {
        panic!()
    };
    assert_eq!(b1.preds, [0, 1]);
    assert_eq!(
        b1.exit,
        Exit::Branch {
            taken: 1,
            not_taken: 2
        }
    );
    let [phi] = &b1.phis[..] else {
        panic!("{:?}", b1.phis)
    };
    let add = &b1.insts[0];
    assert_eq!(phi.reg, Reg::S(0));
    assert_eq!(
        phi.args,
        [
            b0.insts[0].write(Reg::S(0)).unwrap(),
            add.write(Reg::S(0)).unwrap()
        ]
    );
    assert_eq!(add.read(Reg::S(0)), Some(phi.value));
    assert_eq!(b1.insts[1].read(Reg::S(0)), add.write(Reg::S(0)));
    assert_eq!(b2.insts[0].name(), "s_endpgm");
}

#[test]
fn drops_scheduling_and_follows_far_jumps() {
    let p = build_words(
        &[
            0xBF80_0000, // s_nop 0
            0xBE88_1F00, // s_getpc_b64 s[8:9]
            0x8008_FF08,
            20,          // s_add_u32 s8, s8, 20  -> 8 + 20 = 28
            0x8209_8009, // s_addc_u32 s9, s9, 0
            0xBE80_2008, // s_setpc_b64 s[8:9]
            0xFFFF_FFFF, // not code
            S_ENDPGM,
        ],
        false,
    );
    let names: Vec<_> = p.blocks[0].insts.iter().map(Inst::name).collect();
    assert_eq!(
        names,
        ["s_getpc_b64", "s_add_u32", "s_addc_u32", "s_setpc_b64"]
    );
    assert_eq!(p.blocks[0].exit, Exit::Jump(1));
    assert_eq!(p.blocks[1].offset, 28);
    // s_addc_u32 reads the carry s_add_u32 left in SCC.
    let i = &p.blocks[0].insts;
    assert_eq!(i[2].read(Reg::Scc), i[1].write(Reg::Scc));
}

#[test]
fn reports_what_it_cannot_express() {
    let code: Vec<u8> = [0xBE80_2E01_u32, S_ENDPGM] // s_movrels_b32 s0, s1
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    assert_eq!(
        build(&code, false, 256),
        Err(ShaderError::Unsupported {
            offset: 0,
            what: "s_movrels_b32"
        })
    );
}

#[test]
fn relative_vgpr_access_spans_up_to_the_vgpr_count() {
    let code: Vec<u8> = [
        0x7E04_8501_u32, // v_movreld_b32 v2, v1: v[2 + m0] = v1
        0x7E00_8704,     // v_movrels_b32 v0, v4: v0 = v[4 + m0]
        S_ENDPGM,
    ]
    .iter()
    .flat_map(|w| w.to_le_bytes())
    .collect();
    let p = build(&code, false, 6).unwrap();
    let [movreld, movrels, _] = &p.blocks[0].insts[..] else {
        panic!()
    };
    let vgprs = |regs: &[(Reg, Value)]| -> Vec<u8> {
        regs.iter()
            .filter_map(|r| match r.0 {
                Reg::V(v) => Some(v),
                _ => None,
            })
            .collect()
    };
    assert_eq!(vgprs(&movreld.writes), [2, 3, 4, 5]);
    // v1 is the source; v2-v5 are read for the lanes and indices not written.
    assert_eq!(vgprs(&movreld.reads), [1, 2, 3, 4, 5]);
    assert!(movreld.read(Reg::S(M0)).is_some());
    assert_eq!(vgprs(&movrels.writes), [0]);
    assert_eq!(vgprs(&movrels.reads), [4, 5, 0]);
    assert_eq!(movrels.read(Reg::V(4)), movreld.write(Reg::V(4)));
    assert!(build(&code, false, 4).is_err(), "v4 is past 4 VGPRs");
}

const fn s_branch(simm16: i16) -> u32 {
    0xBF82_0000 | simm16 as u16 as u32
}

const fn s_cbranch_scc1(simm16: i16) -> u32 {
    0xBF85_0000 | simm16 as u16 as u32
}

#[test]
fn reverse_order_diamond_chain_eliminates_trivial_phis() {
    const LAYERS: usize = 4096;
    let mut reverse = vec![
        0xBE81_0381, // s_mov_b32 s1, 1
        0xBE82_0382, // s_mov_b32 s2, 2
        s_branch(((LAYERS - 1) * 4) as i16),
    ];
    for k in 0..LAYERS {
        reverse.extend([
            0xBF06_0201, // s_cmp_eq_u32 s1, s2
            s_cbranch_scc1(1),
            0xBF80_0000, // s_nop 0
            if k == 0 {
                s_branch((LAYERS * 4 - 4) as i16)
            } else {
                s_branch(-8)
            },
        ]);
    }
    reverse.extend([0x8000_0201, S_ENDPGM]); // s_add_u32 s0, s1, s2

    let mut forward = vec![
        0xBE81_0381, // s_mov_b32 s1, 1
        0xBE82_0382, // s_mov_b32 s2, 2
        s_branch(0),
    ];
    for _ in 0..LAYERS {
        forward.extend([
            0xBF06_0201, // s_cmp_eq_u32 s1, s2
            s_cbranch_scc1(1),
            0xBF80_0000, // s_nop 0
            s_branch(0),
        ]);
    }
    forward.extend([0x8000_0201, S_ENDPGM]); // s_add_u32 s0, s1, s2

    let rev = build_words(&reverse, false);
    let fwd = build_words(&forward, false);
    assert!(rev.inputs.is_empty(), "{:?}", rev.inputs);
    assert!(fwd.inputs.is_empty(), "{:?}", fwd.inputs);
    assert!(rev.blocks.iter().all(|b| b.phis.is_empty()));
    assert!(fwd.blocks.iter().all(|b| b.phis.is_empty()));
    assert_eq!(rev.blocks.len(), fwd.blocks.len());

    let rev_s1 = rev.blocks[0].insts[0].write(Reg::S(1)).unwrap();
    let rev_s2 = rev.blocks[0].insts[1].write(Reg::S(2)).unwrap();
    let rev_exit = rev.blocks.last().unwrap();
    assert_eq!(rev_exit.insts[0].name(), "s_add_u32");
    assert_eq!(rev_exit.insts[0].read(Reg::S(1)), Some(rev_s1));
    assert_eq!(rev_exit.insts[0].read(Reg::S(2)), Some(rev_s2));

    let fwd_s1 = fwd.blocks[0].insts[0].write(Reg::S(1)).unwrap();
    let fwd_s2 = fwd.blocks[0].insts[1].write(Reg::S(2)).unwrap();
    let fwd_exit = fwd.blocks.last().unwrap();
    assert_eq!(fwd_exit.insts[0].name(), "s_add_u32");
    assert_eq!(fwd_exit.insts[0].read(Reg::S(1)), Some(fwd_s1));
    assert_eq!(fwd_exit.insts[0].read(Reg::S(2)), Some(fwd_s2));
}

#[test]
fn remove_trivial_phis_compresses_reverse_chain_in_bounded_steps() {
    const CHAIN: usize = 4096;
    let mut template = build_words(&[0xBE80_0380, S_ENDPGM], false).blocks[0].insts[0].clone();
    template.writes.clear();
    let mut blocks = Vec::with_capacity(CHAIN);
    for k in 0..CHAIN {
        let v1 = Value(2 + 2 * k as u32);
        let v2 = Value(3 + 2 * k as u32);
        let (n1, n2) = if k + 1 < CHAIN {
            (Value(2 + 2 * (k + 1) as u32), Value(3 + 2 * (k + 1) as u32))
        } else {
            (Value(0), Value(1))
        };
        let mut inst = template.clone();
        inst.reads = vec![(Reg::S(1), v1), (Reg::S(2), v2)];
        blocks.push(Block {
            offset: k * 4,
            preds: vec![],
            phis: vec![
                Phi {
                    reg: Reg::S(1),
                    value: v1,
                    args: vec![n1, n1],
                },
                Phi {
                    reg: Reg::S(2),
                    value: v2,
                    args: vec![n2, n2],
                },
            ],
            insts: vec![inst],
            exit: Exit::End,
        });
    }
    let steps = remove_trivial_phis_steps(&mut blocks, 2 + 2 * CHAIN as u32);
    assert!(blocks.iter().all(|b| b.phis.is_empty()));
    assert!(
        blocks
            .iter()
            .all(|b| b.insts[0].reads == [(Reg::S(1), Value(0)), (Reg::S(2), Value(1))])
    );
    assert!(steps < 100_000, "steps = {steps}");
}

#[test]
fn non_trivial_phis_preserve_compressed_representatives() {
    const LAYERS: usize = 4;
    let mut code = vec![
        0xBE81_0381, // s_mov_b32 s1, 1
        0xBE82_0382, // s_mov_b32 s2, 2
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        s_cbranch_scc1((2 + (LAYERS - 1) * 4) as i16),
        0xBE81_0383, // s_mov_b32 s1, 3
        s_branch((LAYERS * 4) as i16),
    ];
    for k in 0..LAYERS {
        code.extend([
            0xBF06_0201, // s_cmp_eq_u32 s1, s2
            s_cbranch_scc1(1),
            0xBF80_0000, // s_nop 0
            if k == 0 {
                s_branch((LAYERS * 4 - 4) as i16)
            } else {
                s_branch(-8)
            },
        ]);
    }
    code.extend([0x8000_0201, S_ENDPGM]); // s_add_u32 s0, s1, s2

    let p = build_words(&code, false);
    let entry_s1 = p.blocks[0].insts[0].write(Reg::S(1)).unwrap();
    let entry_s2 = p.blocks[0].insts[1].write(Reg::S(2)).unwrap();
    let arm_s1 = p.blocks[1].insts[0].write(Reg::S(1)).unwrap();
    let merge = p.blocks.last().unwrap();
    for b in &p.blocks[..p.blocks.len() - 1] {
        assert!(b.phis.is_empty(), "{:?}", b.phis);
    }
    let [phi] = &merge.phis[..] else {
        panic!("{:?}", merge.phis)
    };
    assert_eq!(phi.reg, Reg::S(1));
    assert_eq!(phi.args, [arm_s1, entry_s1]);
    assert_eq!(merge.insts[0].read(Reg::S(1)), Some(phi.value));
    assert_eq!(merge.insts[0].read(Reg::S(2)), Some(entry_s2));

    let mut loop_code = vec![
        0xBE81_0381, // s_mov_b32 s1, 1
        0xBE82_0382, // s_mov_b32 s2, 2
        s_branch(((LAYERS - 1) * 4) as i16),
    ];
    for k in 0..LAYERS {
        loop_code.extend([
            0xBF06_0201, // s_cmp_eq_u32 s1, s2
            s_cbranch_scc1(1),
            0xBF80_0000, // s_nop 0
            if k == 0 {
                s_branch((LAYERS * 4 - 4) as i16)
            } else {
                s_branch(-8)
            },
        ]);
    }
    loop_code.extend([
        0x8001_0201, // s_add_u32 s1, s1, s2
        0xBF07_8401, // s_cmp_lg_u32 s1, 4
        s_cbranch_scc1(-3),
        0x8000_0201, // s_add_u32 s0, s1, s2
        S_ENDPGM,
    ]);

    let lp = build_words(&loop_code, false);
    let lp_s1 = lp.blocks[0].insts[0].write(Reg::S(1)).unwrap();
    let lp_s2 = lp.blocks[0].insts[1].write(Reg::S(2)).unwrap();
    let header = &lp.blocks[lp.blocks.len() - 2];
    let exit = lp.blocks.last().unwrap();
    for b in &lp.blocks[..lp.blocks.len() - 2] {
        assert!(b.phis.is_empty(), "{:?}", b.phis);
    }
    assert!(exit.phis.is_empty(), "{:?}", exit.phis);
    let [loop_phi] = &header.phis[..] else {
        panic!("{:?}", header.phis)
    };
    let next_s1 = header.insts[0].write(Reg::S(1)).unwrap();
    assert_eq!(loop_phi.reg, Reg::S(1));
    assert_eq!(loop_phi.args, [lp_s1, next_s1]);
    assert_eq!(header.insts[0].read(Reg::S(1)), Some(loop_phi.value));
    assert_eq!(header.insts[0].read(Reg::S(2)), Some(lp_s2));
    assert_eq!(exit.insts[0].read(Reg::S(1)), Some(next_s1));
    assert_eq!(exit.insts[0].read(Reg::S(2)), Some(lp_s2));
}
