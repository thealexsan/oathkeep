use super::*;

pub(crate) const WAVE_OR_V7_TO_S7: [u32; 20] = [
    0xBE84_25C1, // s_or_saveexec_b64 s[4:5], -1
    0xD501_0001,
    0x0012_0E80, // v_cndmask_b32 v1, 0, v7, s[4:5]
    0x3802_02E9,
    0xDE54_C101, // v_or_b32 v1, v1, v1 dpp8:[1,0,3,2,5,4,7,6]
    0x3802_02E9,
    0x9360_1201, // v_or_b32 v1, v1, v1 dpp8:[2,2,0,0,6,6,4,4]
    0x3802_02E9,
    0x0009_2401, // v_or_b32 v1, v1, v1 dpp8:[4,4,4,4,0,0,0,0]
    0x3802_02FA,
    0xFF09_4001, // v_or_b32 v1, v1, v1 row_mirror bound_ctrl:1
    0xD778_0002,
    0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
    0x3802_0302, // v_or_b32 v1, v2, v1
    0xD760_0007,
    0x0001_4101, // v_readlane_b32 s7, v1, 32
    0x3802_0207, // v_or_b32 v1, s7, v1
    0xD760_0007,
    0x0001_0101, // v_readlane_b32 s7, v1, 0
    0xBEFE_0404, // s_mov_b64 exec, s[4:5]
];



const S_ENDPGM: u32 = 0xBF81_0000;

const fn sop1(sdst: u8, op: u8, ssrc0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | (op as u32) << 8 | ssrc0 as u32
}

const fn vop1(vdst: u8, op: u8, src0: u16) -> u32 {
    (0x3F << 25) | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn vop2(vdst: u8, op: u8, src0: u16, vsrc1: u8) -> u32 {
    (op as u32) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn dpp16(vsrc0: u8, ctrl: u16, row_mask: u8, bank_mask: u8, bound_ctrl: bool) -> u32 {
    (row_mask as u32) << 28
        | (bank_mask as u32) << 24
        | (bound_ctrl as u32) << 19
        | (ctrl as u32) << 8
        | vsrc0 as u32
}

const fn vop3(vdst: u8, op: u16, src0: u16, src1: u16, src2: u16) -> [u32; 2] {
    [
        0xD400_0000 | (op as u32) << 16 | vdst as u32,
        (src0 as u32 & 0x1FF) | ((src1 as u32 & 0x1FF) << 9) | ((src2 as u32 & 0x1FF) << 18),
    ]
}

fn build(words: &[u32], wave64: bool) -> Program {
    let code: Vec<u8> = words
        .iter()
        .copied()
        .chain(std::iter::once(S_ENDPGM))
        .flat_map(u32::to_le_bytes)
        .collect();
    crate::ir::build(&code, wave64, 256).unwrap()
}

#[test]
fn dpp_lanes_follow_the_isa() {
    // row_shr:1 with bound_ctrl: lane 16 reads 0, lane 17 lane 16.
    let shr = Dpp::Row {
        ctrl: 0x111,
        row_mask: 0xF,
        bank_mask: 0xF,
        bound_ctrl: true,
        fi: false,
    };
    assert_eq!(dpp_lane(shr, 16), Some((16, true, true)));
    assert_eq!(dpp_lane(shr, 17), Some((16, false, true)));
    // row_mirror: lane 3 reads lane 12.
    let mirror = Dpp::Row {
        ctrl: 0x140,
        row_mask: 0xF,
        bank_mask: 0xF,
        bound_ctrl: true,
        fi: false,
    };
    assert_eq!(dpp_lane(mirror, 3), Some((12, false, true)));
    // dpp8 [1,0,3,2,5,4,7,6] swaps neighbours.
    let swap = Dpp::Lanes {
        sel: 1 | 3 << 6 | 2 << 9 | 5 << 12 | 4 << 15 | 7 << 18 | 6 << 21,
        fi: false,
    };
    assert_eq!(dpp_lane(swap, 8), Some((9, false, true)));
}

#[test]
fn prefixes_are_recognised() {
    let (x, m) = (Value(1), Value(2));
    let red = |lanes: u64| Sym::Red {
        op: Some(Op::AddU),
        src: x,
        mask: m,
        ident: 0,
        lanes,
    };
    let inclusive: Vec<Sym> = (0..64).map(|l| red(u64::MAX >> (63 - l))).collect();
    assert_eq!(
        per_lane(&inclusive, u64::MAX),
        Some(Shape::Prefix(Op::AddU, x, m, 0, true))
    );
    let mut exclusive: Vec<Sym> = (0..64).map(|l| red((1u64 << l) - 1)).collect();
    exclusive[0] = Sym::K(0);
    assert_eq!(
        per_lane(&exclusive, u64::MAX),
        Some(Shape::Prefix(Op::AddU, x, m, 0, false))
    );
    assert_eq!(
        per_lane(&vec![red(u64::MAX); 64], u64::MAX),
        Some(Shape::Active(Op::AddU, x, m, 0))
    );
}

#[test]
fn constant_decodes_width_and_inline_kinds() {
    let b64 = 0x04;
    let b32 = 0x03;
    let check = |words: &[u32], wave64: bool| -> Option<u64> {
        let p = build(words, wave64);
        constant(&p.blocks[0].insts[0])
    };

    assert_eq!(
        check(&[sop1(EXEC, b64, 194)], true),
        Some(0xFFFF_FFFF_FFFF_FFFE)
    );
    assert_eq!(
        check(&[sop1(EXEC, b64, 196)], true),
        Some(0xFFFF_FFFF_FFFF_FFFC)
    );
    assert_eq!(
        check(&[sop1(EXEC, b64, 208)], true),
        Some(0xFFFF_FFFF_FFFF_FFF0)
    );
    assert_eq!(check(&[sop1(EXEC, b64, 193)], true), Some(u64::MAX));
    assert_eq!(check(&[sop1(EXEC, b64, NULL)], true), Some(0));
    assert_eq!(check(&[sop1(EXEC, b64, 243)], true), None);
    assert_eq!(
        check(&[sop1(EXEC, b64, 255), 0x1234_5678], true),
        Some(0x1234_5678)
    );

    assert_eq!(check(&[sop1(EXEC, b32, 194)], false), Some(0xFFFF_FFFE));
    assert_eq!(check(&[sop1(EXEC, b32, 243)], false), Some(0xBF80_0000));
    assert_eq!(check(&[sop1(EXEC, b32, NULL)], false), Some(0));
    assert_eq!(
        check(&[sop1(EXEC, b32, 255), 0x1234_5678], false),
        Some(0x1234_5678)
    );
    assert_eq!(check(&[sop1(EXEC, b32, 0)], false), None);
}

#[test]
fn exec_minus_two_masks_lane_zero_and_enforces_dpp_guard() {
    for wave64 in [false, true] {
        let mut w = Vec::new();
        let save_op = if wave64 { 0x25 } else { 0x3D };
        let mov_exec = if wave64 { 0x04 } else { 0x03 };
        let push_exec = |w: &mut Vec<u32>, k: u8| w.push(sop1(EXEC, mov_exec, k));
        let push_exec_lit = |w: &mut Vec<u32>, lit: u32| {
            w.push(sop1(EXEC, mov_exec, 255));
            w.push(lit);
        };

        w.push(sop1(0, save_op, 193));
        w.extend(vop3(1, 0x101, 128, 256, 0));
        for shift in [1u16, 2, 4, 8] {
            w.push(vop2(1, 0x25, 0xFA, 1));
            w.push(dpp16(1, 0x110 + shift, 0xF, 0xF, false));
        }
        w.extend(vop3(2, 0x360, 257, 128 + 15, 0));
        push_exec_lit(&mut w, 0xFFFF_0000);
        w.push(vop2(1, 0x25, 2, 1));

        if wave64 {
            w.extend(vop3(2, 0x360, 257, 128 + 31, 0));
            w.push(sop1(EXEC, 0x03, 128));
            w.push(sop1(EXEC + 1, 0x03, 255));
            w.push(0x0000_FFFF);
            w.push(vop2(1, 0x25, 2, 1));

            w.extend(vop3(2, 0x360, 257, 128 + 47, 0));
            w.push(sop1(EXEC, 0x03, 128));
            w.push(sop1(EXEC + 1, 0x03, 255));
            w.push(0xFFFF_0000);
            w.push(vop2(1, 0x25, 2, 1));
        }

        push_exec(&mut w, 193);
        w.push(vop1(3, 0x01, 257));
        w.push(vop1(3, 0x01, 0xFA));
        w.push(dpp16(1, 0x111, 0xF, 0xF, false));

        w.extend(vop3(2, 0x360, 257, 128 + 15, 0));
        push_exec_lit(&mut w, 0x0001_0000);
        w.push(vop1(3, 0x01, 2));

        if wave64 {
            w.extend(vop3(2, 0x360, 257, 128 + 31, 0));
            w.push(sop1(EXEC, 0x03, 128));
            w.push(sop1(EXEC + 1, 0x03, 129));
            w.push(vop1(3, 0x01, 2));

            w.extend(vop3(2, 0x360, 257, 128 + 47, 0));
            w.push(sop1(EXEC, 0x03, 128));
            w.push(sop1(EXEC + 1, 0x03, 255));
            w.push(0x0001_0000);
            w.push(vop1(3, 0x01, 2));
        }

        push_exec(&mut w, 193);
        w.push(vop1(2, 0x01, 128));
        w.push(vop1(5, 0x01, 135));
        push_exec(&mut w, 194);
        w.push(vop1(2, 0x01, 259));
        w.push(vop1(4, 0x01, 0xFA));
        w.push(dpp16(5, 0xE4, 0xF, 0xF, false));
        w.push(sop1(EXEC, mov_exec, 0));

        let p = build(&w, wave64);
        let insts = &p.blocks[0].insts;
        let saved = insts[0].write(Reg::S(0)).unwrap();
        let v0_in = p
            .inputs
            .iter()
            .find_map(|&(r, v)| (r == Reg::V(0)).then_some(v))
            .unwrap();
        let v2_val = insts[insts.len() - 4].write(Reg::V(2)).unwrap();
        let v4_val = insts[insts.len() - 3].write(Reg::V(4)).unwrap();
        let sec = &sections(&p, 0)[&0];
        assert_eq!(
            sec.shapes.get(&v2_val),
            Some(&Shape::Prefix(Op::AddU, v0_in, saved, 0, false))
        );
        assert_eq!(sec.shapes.get(&v4_val), None);
    }
}

/// A wave OR whose last step stays in the section's VGPR: the lower
/// lanes add the first half's to themselves, so only the upper ones
/// hold the whole, and the last lane is read after the section (an
/// upscaler's programs).
#[test]
fn one_lane_of_what_a_section_leaves_is_read_after_it() {
    let words = |lane: u32| {
        let mut w = WAVE_OR_V7_TO_S7[..14].to_vec(); // .. v_or_b32 v1, v2, v1
        w.extend([
            0xD760_0007,
            0x0001_0101, // v_readlane_b32 s7, v1, 0
            0x3802_0207, // v_or_b32 v1, s7, v1
            0xBEFE_0404, // s_mov_b64 exec, s[4:5]
            0xD760_0006,
            0x0001_0101 | lane << 9, // v_readlane_b32 s6, v1, lane
            S_ENDPGM,
        ]);
        w
    };
    let read = |lane: u32| {
        let p = build(&words(lane), true);
        let insts = &p.blocks[0].insts;
        let saved = insts[0].write(Reg::S(4)).unwrap();
        let v7 = p.inputs.iter().find_map(|&(r, v)| (r == Reg::V(7)).then_some(v)).unwrap();
        let got = insts[insts.len() - 2].write(Reg::S(6)).unwrap();
        let sec = &sections(&p, 0)[&0];
        (sec.reads.get(&got).copied(), Shape::Active(Op::Or, v7, saved, 0))
    };
    let (found, whole) = read(63);
    assert_eq!(found, Some(whole));
    // Lane 0 has the first half's alone: still a lane read.
    assert_eq!(read(0).0, None);
}

/// `WaveActiveSum` of 16-bit floats, as compiled in
/// `compute_graph_ops:CS_TransposeVecMatMulAlignedOperation` (0x19c on): the tree the
/// lowering reproduces. A tree in another order is not followed.
#[test]
fn a_sum_of_halves_over_the_wave_is_followed_in_the_order_of_its_tree() {
    let words = |first: [u32; 2], second: [u32; 2]| {
        [
            0xBE84_25C1, // s_or_saveexec_b64 s[4:5], -1
            0xD501_0001,
            0x0012_0E80, // v_cndmask_b32 v1, 0, v7, s[4:5]
            first[0],
            first[1],
            second[0],
            second[1],
            0x6402_02E9,
            0x0009_2401, // v_add_f16 v1, v1, v1 dpp8:[4,4,4,4,0,0,0,0]
            0x6402_02FA,
            0xFF09_4001, // v_add_f16 v1, v1, v1 row_mirror bound_ctrl:1
            0xD778_0002,
            0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
            0x6402_0302, // v_add_f16 v1, v2, v1
            0xD760_0007,
            0x0001_4101, // v_readlane_b32 s7, v1, 32
            0x6402_0207, // v_add_f16 v1, s7, v1
            0xD760_0007,
            0x0001_0101, // v_readlane_b32 s7, v1, 0
            0xBEFE_0404, // s_mov_b64 exec, s[4:5]
        ]
    };
    let pairs = [0x6402_02E9, 0xDE54_C101]; // v_add_f16 v1, v1, v1 dpp8:[1,0,3,2,5,4,7,6]
    let quads = [0x6402_02E9, 0x9360_1201]; // v_add_f16 v1, v1, v1 dpp8:[2,2,0,0,6,6,4,4]
    let shape = |w: &[u32]| {
        let p = build(w, true);
        let insts = &p.blocks[0].insts;
        let saved = insts[0].write(Reg::S(4)).unwrap();
        let v7 = p.inputs.iter().find_map(|&(r, v)| (r == Reg::V(7)).then_some(v)).unwrap();
        // ..., v_readlane_b32 s7, v1, 0; s_mov_b64 exec, s[4:5]; s_endpgm
        let total = insts[insts.len() - 3].write(Reg::S(7)).unwrap();
        (sections(&p, 0)[&0].shapes.get(&total).copied(), Shape::Active(Op::AddH, v7, saved, 0))
    };
    let (found, want) = shape(&words(pairs, quads));
    assert_eq!(found, Some(want));
    // lanes two apart first, neighbours second: every lane counted once, but
    // (x0 + x2) + (x1 + x3) rounds differently from (x0 + x1) + (x2 + x3)
    let (found, _) = shape(&words(quads, pairs));
    assert_eq!(found, None);
}

#[test]
fn buddies_are_the_two_halves_of_an_aligned_run() {
    assert!(buddies(0b01, 0b10));
    assert!(buddies(0b1100, 0b0011));
    assert!(buddies(0xF0, 0x0F));
    assert!(buddies(u64::from(u32::MAX), u64::from(u32::MAX) << 32));
    assert!(!buddies(0b010, 0b100)); // lanes 1 and 2: not one pair
    assert!(!buddies(0b0011, 0b110000)); // pairs 0 and 2
    assert!(!buddies(0b0101, 0b1010)); // not runs
    assert!(!buddies(0b01, 0b1100)); // unequal
    assert!(!buddies(0b11, 0b11));
}

#[test]
fn wave32_mov_b64_all_lanes_keeps_exec_masked_for_dpp() {
    let w = [
        sop1(0, 0x3D, 193),
        vop1(1, 0x01, 135),
        sop1(EXEC, 0x04, 193),
        vop1(2, 0x01, 0xFA),
        dpp16(1, 0xE4, 0xF, 0xF, false),
        sop1(EXEC + 1, 0x03, 193),
        vop1(3, 0x01, 0xFA),
        dpp16(1, 0xE4, 0xF, 0xF, false),
        sop1(EXEC, 0x03, 0),
    ];
    let p = build(&w, false);
    let insts = &p.blocks[0].insts;
    let v2_val = insts[3].write(Reg::V(2)).unwrap();
    let v3_val = insts[5].write(Reg::V(3)).unwrap();
    let sec = &sections(&p, 0)[&0];
    assert_eq!(sec.shapes.get(&v2_val), Some(&Shape::K(7)));
    assert_eq!(sec.shapes.get(&v3_val), Some(&Shape::K(7)));
}

#[test]
fn per_lane_prefix_soundness() {
    let (v0, v_other, m0, m_other) = (Value(1), Value(2), Value(3), Value(4));
    let red = |op: Option<Op>, src: Value, mask: Value, ident: u32, lanes: u64| Sym::Red {
        op,
        src,
        mask,
        ident,
        lanes,
    };

    for (wave, all) in [(32usize, u64::from(u32::MAX)), (64usize, u64::MAX)] {
        let below_mask = |l: usize| if l == 0 { 0 } else { (1u64 << l) - 1 };

        let mut valid_inc: Vec<Sym> = (0..wave)
            .map(|l| red(Some(Op::AddU), v0, m0, 0, below_mask(l) | (1u64 << l)))
            .collect();
        valid_inc[0] = red(None, v0, m0, 0, 1);
        assert_eq!(
            per_lane(&valid_inc, all),
            Some(Shape::Prefix(Op::AddU, v0, m0, 0, true))
        );

        let mut valid_exc: Vec<Sym> = (0..wave)
            .map(|l| red(Some(Op::AddU), v0, m0, 0, below_mask(l)))
            .collect();
        valid_exc[0] = Sym::K(0);
        valid_exc[1] = red(None, v0, m0, 0, 1);
        assert_eq!(
            per_lane(&valid_exc, all),
            Some(Shape::Prefix(Op::AddU, v0, m0, 0, false))
        );

        let mut mixed_other_src = valid_exc.clone();
        mixed_other_src[0] = red(None, v_other, m0, 0, 1);
        assert_eq!(per_lane(&mixed_other_src, all), None);

        let mut mixed_same_src = valid_exc.clone();
        mixed_same_src[0] = red(None, v0, m0, 0, 1);
        assert_eq!(per_lane(&mixed_same_src, all), None);

        let mut exc_bad_lane1_src = valid_exc.clone();
        exc_bad_lane1_src[1] = red(None, v_other, m0, 0, 1);
        assert_eq!(per_lane(&exc_bad_lane1_src, all), None);

        let mut exc_bad_lane1_set = valid_exc.clone();
        exc_bad_lane1_set[1] = red(None, v0, m0, 0, 0b11);
        assert_eq!(per_lane(&exc_bad_lane1_set, all), None);

        for bad0 in [
            red(None, v0, m0, 1, 1),
            red(None, v_other, m0, 0, 1),
            red(None, v0, m_other, 0, 1),
            red(None, v0, m0, 0, 0),
            Sym::K(0),
        ] {
            let mut bad_inc = valid_inc.clone();
            bad_inc[0] = bad0;
            assert_eq!(per_lane(&bad_inc, all), None);
        }

        let mut bad_ident: Vec<Sym> = (0..wave)
            .map(|l| red(Some(Op::AddU), v0, m0, 1, below_mask(l) | (1u64 << l)))
            .collect();
        bad_ident[0] = red(None, v0, m0, 1, 1);
        assert_eq!(per_lane(&bad_ident, all), None);
    }
}
