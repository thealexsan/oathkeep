use super::*;
use crate::emit::wave::WAVE_OR_V7_TO_S7;

const ADD_BIT_NUMBER: u32 = 0x4A0A_0A0A; // v_add_nc_u32 v5, s10, v5

/// The title's scalarised light loop (`indirect_light_probes:CS_IrradianceCache_Shade`,
/// 0x71f8 on), in small: the wave OR of v7 in s7, a loop over its bits with the lowest
/// one's number in s10 and the bit in s11, and `body` for the lanes whose v7 has the bit
/// (`own`) or for every lane; `latch` runs in every lane after the body, `after` after
/// the loop.
fn light_loop(own: bool, body: &[u32], latch: &[u32], after: &[u32]) -> Program {
    let mut w = WAVE_OR_V7_TO_S7.to_vec();
    let test: &[u32] = if own {
        &[
            0x3608_0E0B, // v_and_b32 v4, s11, v7
            0xBE8C_047E, // s_mov_b64 s[12:13], exec
            0x7DAA_0880, // v_cmpx_ne_u32 exec, 0, v4
        ]
    } else {
        &[0xBE8C_047E] // s_mov_b64 s[12:13], exec
    };
    // from the loop's head to its last branch
    let head = 3 + test.len() + usize::from(own) + body.len() + 1 + latch.len() + 3;
    w.extend_from_slice(&[
        0xBF06_8007,                     // s_cmp_eq_u32 s7, 0
        0xBF85_0000 | (3 + head) as u32, // s_cbranch_scc1 past the loop
        0x7E06_0207,                     // v_mov_b32 v3, s7
        0xBF82_0001,                     // s_branch to the loop's head
        0x7E06_0209,                     // again: v_mov_b32 v3, s9
        0x7E10_0503,                     // head: v_readfirstlane_b32 s8, v3
        0xBE8A_1308,                     // s_ff1_i32_b32 s10, s8
        0x8F0B_0A81,                     // s_lshl_b32 s11, 1, s10
    ]);
    w.extend_from_slice(test);
    if own {
        w.push(0xBF88_0000 | body.len() as u32); // s_cbranch_execz past the body
    }
    w.extend_from_slice(body);
    w.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    w.extend_from_slice(latch);
    w.extend_from_slice(&[
        0x8909_080B,                            // s_xor_b32 s9, s11, s8
        0xBF06_0B08,                            // s_cmp_eq_u32 s8, s11
        0xBF84_0000 | (0x1_0000 - head - 1) as u32, // s_cbranch_scc0 again
    ]);
    w.extend_from_slice(after);
    w.push(0xBF81_0000); // s_endpgm
    let code: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    crate::ir::build(&code, true, 256).unwrap()
}

fn found(p: &Program) -> OwnBits {
    let st = crate::structure::structure(p).unwrap();
    find(p, &st, false)
}

#[test]
fn a_wave_or_that_only_steers_a_loop_over_own_bits_is_found() {
    let p = light_loop(true, &[ADD_BIT_NUMBER], &[], &[]);
    let insts = || p.blocks.iter().flat_map(|b| &b.insts);
    let names: Vec<&str> = insts().map(|i| i.name()).collect();
    for n in [
        "s_ff1_i32_b32",
        "s_lshl_b32",
        "v_and_b32",
        "v_cmpx_ne_u32",
        "s_xor_b32",
        "v_readfirstlane_b32",
        "s_cbranch_execz",
    ] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    // a loop: its last branch goes back to the copy of the bits still to visit
    assert!(p.blocks.iter().enumerate().any(|(b, blk)| blk.exit.successors().iter().any(|&s| s < b)));
    let own = found(&p);
    // the OR is the section's last lane read, and the bits still to visit (v3) are alike
    let total = insts()
        .rfind(|i| i.name() == "v_readlane_b32")
        .and_then(|i| i.write(Reg::S(7)))
        .unwrap();
    assert_eq!(own.totals, HashSet::from([total]));
    let first = insts().find(|i| i.name() == "v_readfirstlane_b32").unwrap();
    assert!(own.alike.contains(&first.read(Reg::V(3)).unwrap()));
}

#[test]
fn a_loop_whose_lanes_all_work_at_every_bit_needs_the_whole_waves_or() {
    // (the same title's volume loops, 0xa668 on: no test of the lane's own mask)
    let p = light_loop(false, &[ADD_BIT_NUMBER], &[], &[]);
    assert!(found(&p).totals.is_empty());
}

#[test]
fn a_bit_number_read_after_the_loop_is_not_the_lanes_own() {
    // v_mov_b32 v6, s10: the last bit of the OR, which another lane may have set
    let p = light_loop(true, &[ADD_BIT_NUMBER], &[], &[0x7E0C_020A]);
    assert!(found(&p).totals.is_empty());
}

#[test]
fn the_bits_still_to_visit_read_in_the_body_are_not_the_lanes_own() {
    // v_add_nc_u32 v5, s8, v5: s8 holds the other lanes' bits too
    let p = light_loop(true, &[0x4A0A_0A08], &[], &[]);
    assert!(found(&p).totals.is_empty());
}

#[test]
fn a_count_of_the_rounds_is_not_the_lanes_own() {
    // s_add_i32 s14, s14, 1 where every lane passes, read after the loop
    // (v_mov_b32 v6, s14): one round for each bit any lane has
    let p = light_loop(true, &[ADD_BIT_NUMBER], &[0x810E_810E], &[0x7E0C_020E]);
    assert!(p.blocks.iter().flat_map(|b| &b.insts).any(|i| i.name() == "s_add_i32"));
    assert!(found(&p).totals.is_empty());
    // (reading s14 there without counting in it is nothing of the loop's)
    let p = light_loop(true, &[ADD_BIT_NUMBER], &[], &[0x7E0C_020E]);
    assert_eq!(found(&p).totals.len(), 1);
}

#[test]
fn an_effect_where_every_lane_passes_is_not_the_lanes_own() {
    // ds_write_b32 v5, v5 after the body: once for each bit any lane has
    let p = light_loop(true, &[ADD_BIT_NUMBER], &[0xD834_0000, 0x0000_0505], &[]);
    assert!(p.blocks.iter().flat_map(|b| &b.insts).any(|i| i.name() == "ds_write_b32"));
    assert!(found(&p).totals.is_empty());
}
