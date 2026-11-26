use super::bools::MAX_MEET;
use super::*;
use crate::ir::{self, EXEC, Reg, VCC};

const S_ENDPGM: u32 = 0xBF81_0000;

fn build(words: &[u32], wave64: bool) -> Program {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    ir::build(&code, wave64, 256).unwrap()
}

fn guarded(s: &Structured) -> usize {
    s.guards.iter().flatten().filter(|g| g.is_some()).count()
}

/// The search of [`Bools::meet`] with every case split made anew, as it
/// was before [`Meets`].
fn meet_anew(b: &Bools, mut all: Vec<(Bool, bool)>, budget: &mut usize) -> bool {
    let fails = |&(b, holds): &(Bool, bool)| b == if holds { Bool::FALSE } else { Bool::TRUE };
    if all.iter().any(fails) {
        return false;
    }
    all.retain(|&(b, _)| b != Bool::FALSE && b != Bool::TRUE);
    if all.is_empty() || *budget == 0 {
        return true;
    }
    *budget -= 1;
    let top = |x: Bool| b.nodes[x.0 as usize].0;
    let v = all.iter().map(|&(x, _)| top(x)).min().expect("not empty");
    [false, true].into_iter().any(|value| {
        let next = all
            .iter()
            .map(|&(x, holds)| {
                let (lo, hi) = b.split(x, v);
                (if value { hi } else { lo }, holds)
            })
            .collect();
        meet_anew(b, next, budget)
    })
}

#[test]
fn a_search_that_remembers_answers_as_one_made_anew() {
    // Functions of 12 variables put together at random, and conditions
    // of two to five of them (repeats and constants among them), under
    // budgets that run out early, late and never: the answer, and where
    // no lane is found the budget left, are those of the search that
    // remembers nothing; with what one `Meets` gathered over all of
    // them, and with an empty one.
    let mut b = Bools::new();
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut random = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    let mut f: Vec<Bool> = (0..12).map(|i| b.atom(Atom::Constant(i + 2), 0)).collect();
    f.extend([Bool::FALSE, Bool::TRUE]);
    for _ in 0..120 {
        let (x, y) = (f[random(f.len())], f[random(f.len())]);
        let made = match random(4) {
            0 => b.and(x, y),
            1 => b.or(x, y),
            2 => b.xor(x, y),
            _ => b.not(x),
        };
        f.push(made);
    }
    let mut seen = Meets::default();
    let (mut none, mut some, mut gave_up) = (0, 0, 0);
    for _ in 0..1500 {
        let all: Vec<(Bool, bool)> = (0..2 + random(4))
            .map(|_| (f[random(f.len())], random(2) == 0))
            .collect();
        for budget in [0, 1, 2, 3, 5, 9, 20, 60, 250, 1000, MAX_MEET] {
            let mut left = budget;
            let anew = meet_anew(&b, all.clone(), &mut left);
            for fresh in [false, true] {
                let mut empty = Meets::default();
                let seen = if fresh { &mut empty } else { &mut seen };
                let mut work = all
                    .iter()
                    .map(|&(x, holds)| x.0 << 1 | u32::from(holds))
                    .collect();
                let mut mine = budget;
                let found = b.meet_in(0, &mut work, &mut mine, seen);
                assert_eq!(found, anew, "{all:?} within {budget}");
                assert!(work.is_empty());
                if !found {
                    assert_eq!(mine, left, "{all:?} within {budget}");
                }
            }
            assert_eq!(b.meet_within(&all, budget, &mut seen), anew);
            if !anew {
                none += 1;
            } else if meet_anew(&b, all.clone(), &mut usize::MAX.clone()) {
                some += 1;
            } else {
                gave_up += 1;
            }
        }
    }
    // All three outcomes came up, and searches were remembered.
    assert!(
        none > 100 && some > 100 && gave_up > 100,
        "{none} {some} {gave_up}"
    );
    assert!(seen.none.len() > 100);
}

#[test]
fn divergent_if_becomes_a_lane_branch() {
    let p = build(
        &[
            0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
            0xBE80_3C6A, // s_and_saveexec_b32 s0, vcc_lo
            0xBF88_0001, // s_cbranch_execz -> 16
            0x7E02_0280, // v_mov_b32 v1, 0
            0x887E_007E, // 16: s_or_b32 exec_lo, exec_lo, s0
            0x7E04_0280, // v_mov_b32 v2, 0
            S_ENDPGM,
        ],
        false,
    );
    let s = structure(&p).unwrap();
    let LaneExit::Branch {
        when,
        taken: 2,
        not_taken: 1,
    } = s.exits[0]
    else {
        panic!("{:?}", s.exits)
    };
    // Skipped exactly where the compare fails.
    let cmp = p.blocks[0].insts[0].write(Reg::S(VCC)).unwrap();
    assert_eq!(
        s.bools.node(when),
        Some((Atom::Lane(cmp), Bool::TRUE, Bool::FALSE))
    );
    assert_eq!(guarded(&s), 0, "{:?}", s.guards);
}

#[test]
fn exec_narrowed_without_a_branch_leaves_a_guard() {
    let p = build(
        &[
            0x7DA4_0080, // v_cmpx_eq_u32 exec, 0, v0
            0x7E02_0280, // v_mov_b32 v1, 0
            S_ENDPGM,
        ],
        true,
    );
    let s = structure(&p).unwrap();
    assert_eq!(s.guards[0][0], None, "the compare runs in every lane");
    let g = s.guards[0][1].expect("guard");
    assert!(matches!(s.bools.support(g)[..], [Atom::Lane(_)]));
}

#[test]
fn guards_follow_exec_and_block_reachability() {
    let p = build(
        &[
            0x7DA4_0080, // v_cmpx_eq_u32 exec, 0, v0
            0x7E02_0280, // v_mov_b32 v1, 0
            0x7E04_0280, // v_mov_b32 v2, 0
            0xBF88_0005, // s_cbranch_execz -> end
            0x7E06_0280, // v_mov_b32 v3, 0
            0x7DA4_0880, // v_cmpx_eq_u32 exec, 0, v4
            0x7E06_0280, // v_mov_b32 v3, 0
            0xBE82_0480, // s_mov_b64 s[2:3], 0
            0x7E0A_0280, // v_mov_b32 v5, 0
            S_ENDPGM,
        ],
        true,
    );
    let s = structure(&p).unwrap();
    let lane = |block: usize, i: usize| {
        Atom::Lane(p.blocks[block].insts[i].write(Reg::S(EXEC)).unwrap())
    };
    let a = s.guards[0][1].unwrap();
    let b = s.guards[1][2].unwrap();
    assert_eq!(s.bools.node(a), Some((lane(0, 0), Bool::FALSE, Bool::TRUE)));
    assert_eq!(s.guards[0], [None, Some(a), Some(a), None]);
    // The fallthrough already implies the first compare. Only the second
    // compare needs testing, and the scalar move never needs a guard.
    assert_eq!(s.bools.node(b), Some((lane(1, 1), Bool::FALSE, Bool::TRUE)));
    assert_eq!(s.guards[1], [None, None, Some(b), None, Some(b)]);
}

#[test]
fn divergent_loop_exits_per_lane() {
    let p = build(
        &[
            0xBE82_0480, // s_mov_b64 s[2:3], 0
            0x4A00_0081, // 4: v_add_nc_u32 v0, 1, v0
            0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
            0x8882_026A, // s_or_b64 s[2:3], vcc, s[2:3]
            0x8AFE_027E, // s_andn2_b64 exec, exec, s[2:3]
            0xBF89_FFFB, // s_cbranch_execnz -> 4
            0x88FE_027E, // s_or_b64 exec, exec, s[2:3]
            0x7E02_0280, // v_mov_b32 v1, 0
            S_ENDPGM,
        ],
        true,
    );
    let s = structure(&p).unwrap();
    assert_eq!(guarded(&s), 0, "{:?}", s.guards);
    let LaneExit::Branch {
        when,
        taken: 1,
        not_taken: 2,
    } = s.exits[1]
    else {
        panic!("{:?}", s.exits)
    };
    // Back while the lane's compare fails.
    assert!(matches!(
        s.bools.node(when),
        Some((Atom::Lane(_), Bool::TRUE, Bool::FALSE))
    ));
}

/// `s_cselect` on the SCC a lane loop ended with (the blend-shape skinning
/// programs): the loop is left when no lane remains, so the select takes
/// its second operand; it was a uniform atom whose value no lane has.
#[test]
fn a_select_on_the_condition_a_lane_loop_ended_with_takes_its_second_operand() {
    let cnd = 256 | (256 << 9) | (3 << 18);
    let p = build(
        &[
            0xBE83_037E, // 0: s_mov_b32 s3, exec_lo
            0xBE9C_037E, // s_mov_b32 s28, exec_lo
            0x7E3A_0507, // 8: v_readfirstlane_b32 s29, v7
            0x7DA4_0E1D, // v_cmpx_eq_u32 exec_lo, s29, v7
            0xBE9C_471C, // s_andn2_wrexec_b32 s28, s28
            0xBF89_FFFC, // s_cbranch_execnz -> 8
            0xBEFE_0303, // s_mov_b32 exec_lo, s3
            0x8503_807E, // s_cselect_b32 s3, exec_lo, 0
            0xD501_0001,
            cnd, // v_cndmask_b32 v1, v0, v0, s3
            S_ENDPGM,
        ],
        false,
    );
    let s = structure(&p).unwrap();
    let select = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .find(|i| i.name() == "s_cselect_b32")
        .expect("the select");
    let mask = select.write(Reg::S(3)).unwrap();
    assert_eq!(s.masks.get(&mask), Some(&Bool::FALSE));
}

#[test]
fn scalar_branches_stay_uniform() {
    let p = build(
        &[
            0xBF06_0100, // s_cmp_eq_u32 s0, s1
            0xBF85_0001, // s_cbranch_scc1 -> 12
            0x7E02_0280, // v_mov_b32 v1, 0
            S_ENDPGM,    // 12
        ],
        true,
    );
    let s = structure(&p).unwrap();
    let LaneExit::Branch { when, .. } = s.exits[0] else {
        panic!()
    };
    assert!(matches!(s.bools.support(when)[..], [Atom::Uniform(_)]));
    assert_eq!(guarded(&s), 0);
}

#[test]
fn a_mask_whose_high_dword_is_rewritten_is_not_the_compare() {
    for reader in [
        0x0202_0480, // v_cndmask_b32 v1, 0, v2, vcc
        0x5002_0480, // v_add_co_ci_u32 v1, vcc, 0, v2, vcc
    ] {
        let p = build(
            &[
                0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
                0xBEEB_0380, // s_mov_b32 vcc_hi, 0
                reader,
                S_ENDPGM,
            ],
            true,
        );
        let s = structure(&p).unwrap();
        let lo = p.blocks[0].insts[2].read(Reg::S(VCC)).unwrap();
        assert_eq!(p.blocks[0].insts[0].write(Reg::S(VCC)), Some(lo));
        // The reader sees (compare low dword, 0): lanes 32-63 are clear.
        // The mask exported for that low dword must not be the compare.
        assert_ne!(
            s.masks.get(&lo).and_then(|&m| s.bools.node(m)),
            Some((Atom::Lane(lo), Bool::FALSE, Bool::TRUE)),
            "{reader:#x}: vcc_hi was zeroed after the compare"
        );
    }
}

#[test]
fn a_mask_joined_over_a_back_edge_with_a_rewritten_high_dword_is_not_the_compare() {
    let p = build(
        &[
            0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
            0x0202_0480, // 4: v_cndmask_b32 v1, 0, v2, vcc
            0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
            0xBEEB_0380, // s_mov_b32 vcc_hi, 0
            0xBF85_FFFC, // s_cbranch_scc1 -> 4
            S_ENDPGM,
        ],
        true,
    );
    let s = structure(&p).unwrap();
    // The select reads the loop header's join; over the back edge it
    // takes (compare low dword, 0), which only the join sees.
    let lo = p.blocks[1].insts[1].write(Reg::S(VCC)).unwrap();
    assert_ne!(
        s.masks.get(&lo).and_then(|&m| s.bools.node(m)),
        Some((Atom::Lane(lo), Bool::FALSE, Bool::TRUE))
    );
}

#[test]
fn irreducible_loops_are_split() {
    let p = build(
        &[
            0xBF06_0100, // s_cmp_eq_u32 s0, s1
            0xBF85_0001, // s_cbranch_scc1 -> 12, into the loop's middle
            0x8002_8102, // 8: s_add_u32 s2, s2, 1
            0xBF84_FFFE, // 12: s_cbranch_scc0 -> 8
            S_ENDPGM,
        ],
        false,
    );
    // The loop keeps 12 as its header; the entry into 8 gets a copy of it.
    let copy = &p.blocks[4];
    assert_eq!((copy.offset, copy.exit), (8, ir::Exit::Jump(2)));
    assert_eq!(p.blocks[0].exit.successors(), [2, 4]);
    assert_eq!(p.blocks[1].preds, [2]);
    assert!(structure(&p).is_ok());
}

#[test]
fn mask_phi_joining_constant_and_compare_records_edge_bools() {
    let cnd = 258 | (259 << 9) | (2 << 18);
    let p = build(
        &[
            0xBF06_8000, // 0: s_cmp_eq_u32 s0, 0
            0xBF85_0002, // 4: s_cbranch_scc1 -> 16
            0xBE82_0380, // 8: s_mov_b32 s2, 0
            0xBF82_0002, // 12: s_branch -> 24
            0xD4C2_0002,
            0x0002_0080, // 16: v_cmp_eq_u32 s2, 0, v0
            0xD501_0001,
            cnd, // 24: v_cndmask_b32 v1, v2, v3, s2
            S_ENDPGM,
        ],
        false,
    );
    let s = structure(&p).unwrap();
    let phi = p
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .find(|phi| phi.reg == Reg::S(2))
        .expect("mask phi for s2");
    assert_eq!(
        s.masks.get(&phi.value).and_then(|&m| s.bools.node(m)),
        Some((Atom::Phi(phi.value), Bool::FALSE, Bool::TRUE))
    );
    let edges = &s.phi_masks[&phi.value];
    assert_eq!(edges.len(), 2);
    assert!(edges.contains(&Bool::FALSE), "{edges:?}");
    assert!(
        edges
            .iter()
            .any(|&e| matches!(s.bools.support(e)[..], [Atom::Lane(_)])),
        "{edges:?}"
    );
}

#[test]
fn loop_mask_accumulator_from_zero_tracks_phi_and_edges() {
    let cnd = 258 | (259 << 9) | (2 << 18);
    let p = build(
        &[
            0xBE82_0480, // 0: s_mov_b64 s[2:3], 0
            0xBE84_0380, // 4: s_mov_b32 s4, 0
            0xD501_0001,
            cnd,         // 8: v_cndmask_b32 v1, v2, v3, s[2:3]
            0x7D84_0080, // 16: v_cmp_eq_u32 vcc, 0, v0
            0x8882_026A, // 20: s_or_b64 s[2:3], vcc, s[2:3]
            0x8004_8104, // 24: s_add_u32 s4, s4, 1
            0xBF0A_8404, // 28: s_cmp_lt_u32 s4, 4
            0xBF85_FFF9, // 32: s_cbranch_scc1 -> 8
            0xD501_0004,
            cnd, // 36: v_cndmask_b32 v4, v2, v3, s[2:3]
            S_ENDPGM,
        ],
        true,
    );
    let s = structure(&p).unwrap();
    let phi = p
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .find(|phi| phi.reg == Reg::S(2))
        .expect("loop mask phi for s[2:3]");
    assert!(s.masks.contains_key(&phi.value));
    let edges = &s.phi_masks[&phi.value];
    assert_eq!(edges.len(), 2);
    assert!(edges.contains(&Bool::FALSE), "{edges:?}");
    assert!(edges.iter().any(|&e| {
        let sup = s.bools.support(e);
        sup.contains(&Atom::Phi(phi.value)) && sup.iter().any(|a| matches!(a, Atom::Lane(_)))
    }));
}

#[test]
fn loop_mask_phi_reset_to_constant_on_back_edge() {
    let cnd = 258 | (259 << 9) | (2 << 18);
    let p = build(
        &[
            0xD4C2_0002,
            0x0002_0080, // 0: v_cmp_eq_u32 s2, 0, v0
            0xBE84_0380, // 8: s_mov_b32 s4, 0
            0xD501_0001,
            cnd,         // 12: v_cndmask_b32 v1, v2, v3, s2
            0xBE82_0380, // 20: s_mov_b32 s2, 0
            0x8004_8104, // 24: s_add_u32 s4, s4, 1
            0xBF0A_8404, // 28: s_cmp_lt_u32 s4, 4
            0xBF85_FFFA, // 32: s_cbranch_scc1 -> 12
            S_ENDPGM,
        ],
        false,
    );
    let s = structure(&p).unwrap();
    let phi = p
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .find(|phi| phi.reg == Reg::S(2))
        .expect("loop mask phi for s2");
    assert!(s.masks.contains_key(&phi.value));
    let edges = &s.phi_masks[&phi.value];
    assert_eq!(edges.len(), 2);
    assert!(edges.contains(&Bool::FALSE), "{edges:?}");
    assert!(
        edges
            .iter()
            .any(|&e| matches!(s.bools.support(e)[..], [Atom::Lane(_)])),
        "{edges:?}"
    );
}
