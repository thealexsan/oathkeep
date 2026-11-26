//! A console wave64 compute program on a host of 32-lane waves: a wave reduction inside a
//! per-lane branch, with that branch taken wave-wide ([`crate::structure::structure_wide`])
//! so that the two host waves can exchange their halves inside it. Lowered, validated by
//! `dxil.dll` when present, and run on the host GPU when it takes waves of 32.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// 64 threads in one console wave. Each thread t holds the bit `1 << (t >> 1)`; the threads
/// below s0 (a root constant) OR theirs over the wave inside `if (t < s0) { ... }` and store
/// the result to dword 0 of the raw UAV.
/// `skip`: the branch past the body when no thread is below s0: `s_cbranch_execz`, or
/// `s_cbranch_scc0` on the SCC `s_and_saveexec` sets ("some lane is left").
fn program(skip: u32) -> NativeProgram {
    let mut words = vec![
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        skip | 23,   // the branch to the restore of EXEC
    ];
    words.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    native(&words)
}

/// A wave64 compute program of 64 threads (one console wave; the thread index in v0) with 4
/// root constants in s0..s3 and a raw UAV's descriptor in s[8:11], then `body`.
fn native(body: &[u32]) -> NativeProgram {
    native_of(body, true, 64)
}

/// The same for a console of 32-lane waves when `wave64` is not set, with `threads` threads.
fn native_of(body: &[u32], wave64: bool, threads: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `program()` for a host of 32-lane waves, with the branches closing `wide` taken wave-wide.
fn lowered(native: &NativeProgram, p: &ir::Program, wide: &HashSet<usize>) -> Result<HostShader, ShaderError> {
    lowered_on(native, p, wide, 32)
}

/// [`lowered`] for a host whose waves have at most `host_wave` lanes.
fn lowered_on(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
) -> Result<HostShader, ShaderError> {
    lowered_counted(native, p, wide, host_wave, false)
}

fn lowered_counted(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
    counted: bool,
) -> Result<HostShader, ShaderError> {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let lowered = lower::lower_counted(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    }, counted);
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    })
}

#[test]
fn a_sum_of_halves_over_the_wave_is_the_consoles_tree_bit_for_bit() {
    // thread t holds the 16-bit float with bits 0x3C00 + 37 t (1.0 and up, with odd
    // mantissas, so that additions round); all of them store the wave's sum
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    // (the wave OR's words with v_add_f16 for v_or_b32: opcode 50 for 28)
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(p.blocks.iter().flat_map(|b| &b.insts).filter(|i| i.name() == "v_add_f16").count(), 6);
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // the console's sum: lanes 2k and 2k+1, then pairs, ... each addition rounded to 16 bits
    // (to nearest, ties to even); values here are normal numbers
    let wide = |h: u16| f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13);
    let narrow = |f: f32| {
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let terms: Vec<u16> = (0..64u16).map(|t| 0x3C00 + 37 * t).collect();
    let mut tree = terms.clone();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
    }
    let in_a_row = terms.iter().copied().reduce(add).unwrap();
    assert_ne!(tree[0], in_a_row, "these terms tell the tree from another order");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    // (the high half is lane 0's term's: zero)
    assert_eq!(out[0][0], u32::from(tree[0]), "tree {:#x}, in a row {in_a_row:#x}", tree[0]);
    eprintln!("ran on the hardware device: the sum {:#x} of the console's tree", tree[0]);
}

/// "Some lane is left" of a mask operation as a number: an upscaler's programs wait for
/// other groups in a loop that ends when every lane saw its flag, or after enough rounds:
/// both tests become -1 or 0 and SCC of `timeout | ~waiting` ends it.
#[test]
fn some_lane_of_a_mask_is_a_number_the_whole_wave_agrees_on() {
    let native = native(&[
        0xD765_0001,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0xD766_0001,
        0x0002_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
        0x7D84_0200, // v_cmp_eq_u32 vcc, s0, v1
        0x8792_7E6A, // s_and_b64 s[18:19], vcc, exec
        0x8592_80C1, // s_cselect_b64 s[18:19], -1, 0
        0xBF09_8501, // s_cmp_ge_u32 s1, 5
        0x8594_80C1, // s_cselect_b64 s[20:21], -1, 0
        0x8B92_1214, // s_orn2_b64 s[18:19], s[20:21], s[18:19]
        0x8506_8081, // s_cselect_b32 s6, 1, 0
        0x7E04_0206, // v_mov_b32 v2, s6
        0xE070_0000,
        0x8002_0200, // buffer_store_dword v2, off, s[8:11], 0
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = on_64_lanes(&native, &p);
    // (lane s0 waits, if there is one; s1 rounds)
    for (wait, rounds, done) in [(0, 0, 0), (37, 4, 0), (63, 4, 0), (64, 0, 1), (37, 5, 1), (64, 9, 1)] {
        let Some(out) = dispatched(&shader, &[wait, rounds]) else {
            return;
        };
        assert_eq!(out, done, "lane {wait} after {rounds} rounds");
    }
}

/// `native` for a host of 64-lane waves.
fn on_64_lanes(native: &NativeProgram, p: &ir::Program) -> HostShader {
    let st = structure::structure(p).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(64));
    let lowered = lower::lower(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.expect("lower");
    assert_eq!(lowered.wave_size, Some(64));
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// Dword 0 of the UAV after `shader` ran with root constants `constants`; none without a
/// device that takes it.
fn dispatched(shader: &HostShader, constants: &[u32]) -> Option<u32> {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 64?): {e}; not run");
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if u.root == RootType::Constants {
            ud[u.index as usize] = constants.get(u.dword as usize).copied().unwrap_or(0);
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][0])
}

/// The same sum inside `if (t >= s0) { ... }` on a host of 64-lane waves: the lanes below s0
/// are elsewhere, and count as +0 in the console's tree.
#[test]
fn a_sum_of_halves_where_not_every_lane_runs_on_a_wave_of_64() {
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
        0x7D86_0000, // v_cmp_le_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | 23, // s_cbranch_execz to the restore of EXEC
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = on_64_lanes(&native, &p);
    let wide = |h: u16| match h {
        0 => 0.0,
        h => f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13),
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    for from in [0u32, 1, 5, 31, 32, 37, 63] {
        let mut tree: Vec<u16> = (0..64u16).map(|t| if u32::from(t) < from { 0 } else { 0x3C00 + 37 * t }).collect();
        while tree.len() > 1 {
            tree = tree.chunks(2).map(|c| narrow(wide(c[0]) + wide(c[1]))).collect();
        }
        let Some(out) = dispatched(&shader, &[from]) else {
            return;
        };
        assert_eq!(out, u32::from(tree[0]), "lanes from {from}");
    }
}

/// The console's 16-bit wave sum where the lanes at or past s0 have left the program (a
/// thread past the end of the data): on the console they add +0, and after each step of the
/// tree they hold their run's partial sum all the same. On a host of 64-lane waves (AMD) the
/// lanes that are here read each partner run's sum from a lane of it that is here.
#[test]
fn a_sum_of_halves_where_lanes_have_left_is_the_consoles_tree_bit_for_bit() {
    let mut words = vec![
        0x7DA8_0000, // v_cmpx_gt_u32 exec, s0, v0: the lanes below s0 stay
        0, // s_cbranch_execz to the end (filled in below)
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    // past every word after it, to the program's end
    words[1] = 0xBF88_0000 | (words.len() as u32 - 2);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered_on(&native, &p, &HashSet::new(), 64).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let wide = |h: u16| {
        if h == 0 {
            0.0
        } else {
            f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13)
        }
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = false;
    for stay in [64u32, 63, 40, 33, 32, 17, 5, 1] {
        let terms: Vec<u16> = (0..64u32)
            .map(|t| if t < stay { (0x3C00 + 37 * t) as u16 } else { 0 })
            .collect();
        let mut tree = terms.clone();
        while tree.len() > 1 {
            tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
        }
        let Some(got) = on_the_card(&shader, [stay, 0]) else {
            return;
        };
        ran = true;
        // (the high half is lane 0's term's: zero)
        assert_eq!(got, u32::from(tree[0]), "{stay} lanes stay: got {got:#x}, the console's tree {:#x}", tree[0]);
    }
    if ran {
        eprintln!("ran on the hardware device: the console's tree with lanes gone, in every case");
    }
}

/// One `v_add_f16` whose exact result lies between two halves: 1 + 3/1024 (0x3C03) plus 2
/// (0x4000) is 3 + 1.5 steps of that range, which the console rounds up to 0x4202. The
/// host's `f32tof16` may give 0x4201 (toward zero); with `OATHKEEP_F16_NEAREST=1` the
/// translation gives the console's whatever the host does.
#[test]
fn a_16_bit_sum_rounds_to_nearest_when_asked_to() {
    let words = [
        0x7E0E_02FF,
        0x0000_3C03, // v_mov_b32 v7, 0x3c03
        0x7E0C_02FF,
        0x0000_4000, // v_mov_b32 v6, 0x4000
        0x6400_0D07, // v_add_f16 v0, v7, v6
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert!(p.blocks.iter().flat_map(|b| &b.insts).any(|i| i.name() == "v_add_f16"));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for nearest in [false, true] {
        let was = lower::TEST_F16_NEAREST.replace(Some(nearest));
        let shader = lowered(&native, &p, &HashSet::new());
        lower::TEST_F16_NEAREST.set(was);
        let shader = shader.expect("lower");
        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        let Ok((root, pso)) = host.compute_pipeline(&shader) else {
            eprintln!("the hardware device does not take this program; not run");
            return;
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let got = out[0][0] & 0xFFFF;
        eprintln!("v_add_f16 0x3c03 + 0x4000 on the hardware device, nearest {nearest}: {got:#x}");
        if nearest {
            assert_eq!(got, 0x4202);
        } else {
            assert!(got == 0x4201 || got == 0x4202, "{got:#x}");
        }
    }
}

#[test]
fn a_wave_or_inside_a_lane_branch_needs_the_branch_wave_wide() {
    for (skip, name) in [(0xBF88_0000, "s_cbranch_execz"), (0xBF84_0000, "s_cbranch_scc0")] {
        a_wave_or_inside(skip, name);
    }
}

fn a_wave_or_inside(skip: u32, name: &str) {
    let native = program(skip);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == name))
        .expect("the branch");
    // per lane, the two host waves cannot meet inside the branch
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a wave reduction where not every lane runs"),
        "{:?}",
        per_lane.err()
    );
    // ... and the lowering names the block, around which that branch is the one to widen
    let needs = lower::take_needs();
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &needs), Some(vec![branch]));
    // (taken, past the body, when every lane's bit is clear)
    assert_eq!(st.wave_any[branch], Some(false));
    // wave-wide, every lane runs the reduction (EXEC stays a guard on what it does there)
    let wide = HashSet::from([branch]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    let inside = match p.blocks[branch].exit {
        ir::Exit::Branch { not_taken, .. } => not_taken,
        _ => unreachable!(),
    };
    assert!(st.guards[inside].iter().any(Option::is_some));
    let shader = lowered(&native, &p, &wide).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // On a GPU that takes waves of 32 (two host waves for the console's one): the OR is of
    // both halves, whichever half's lanes take the branch.
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    // (threads below s0 take the branch; thread t gives bit t >> 1)
    for (below, expected) in [(64u32, u32::MAX), (40, 0x000F_FFFF), (33, 0x0001_FFFF), (10, 0x1F), (1, 1)] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        assert_eq!(out[0][0], expected, "{name}: {below} threads in the branch");
    }
    eprintln!("ran on the hardware device ({name}): the OR of both halves in every case");
}

/// Runs `shader` on the hardware device with the root constants `s0`, `s1` and gives dword 0
/// of the UAV; None (and a line) without a device that takes it.
fn on_the_card(shader: &HostShader, constants: [u32; 2]) -> Option<u32> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][0])
}

/// A 32-lane console wave. A register holds a lane mask on one path (the lanes below s0)
/// and a number on the other, and is read as a number after the join, as the trace programs
/// keep their lanes' masks and their node numbers on one stack
/// (`deferred_tiled:CS_LaserLightShadowRaytracing`, s46 at 0x104, 0x75c and 0x188). The
/// mask arrives as the ballot of its lanes.
#[test]
fn a_lane_mask_joined_with_a_number_is_read_as_the_number_it_is() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE94_036A, // s_mov_b32 s20, vcc_lo
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0001, // s_cbranch_scc1 over the next
        0xBE94_0385, // s_mov_b32 s20, 5
        0x7E00_0214, // v_mov_b32 v0, s20
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (below, other, expected) in [(32u32, 0u32, u32::MAX), (10, 0, 0x3FF), (0, 0, 0), (10, 1, 5)] {
        let Some(got) = on_the_card(&shader, [below, other]) else { return };
        assert_eq!(got, expected, "{below} lanes in the mask, s1 = {other}");
    }
    eprintln!("ran on the hardware device: the mask's bits, or the number");
}

/// A 32-lane console wave. `if (t < s0) { ... SCC = (s1 == 7) }` and a branch on SCC after
/// the join: the skip brings the SCC of `s_and_saveexec` ("some lane is left") along
/// (`rtdirectionalshadows:CS_RTShadowTrace32HQ_TiledFarOnly`, 0x360-0x39c). On the console
/// the skip is taken by the wave, so with some lane inside every lane sees the inner test.
/// A lane that skipped on its own would see "some lane is left" instead: refused per lane,
/// right with the branch taken wave-wide. Lane 31 stores what it saw.
#[test]
fn scc_of_a_mask_operation_joined_with_a_scalar_test_needs_the_skip_wave_wide() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE8D_3C6A, // s_and_saveexec_b32 s13, vcc_lo
        0xBF88_0001, // s_cbranch_execz over the next
        0xBF06_8701, // s_cmp_eq_u32 s1, 7
        0xBEFE_030D, // s_mov_b32 exec_lo, s13
        0xBF85_0002, // s_cbranch_scc1 to "seven"
        0x7E0A_0281, // v_mov_b32 v5, 1
        0xBF82_0001, // s_branch over the next
        0x7E0A_0282, // seven: v_mov_b32 v5, 2
        0x7DA4_009F, // v_cmpx_eq_u32 exec_lo, 31, v0
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16 | 5 << 8, // buffer_store_dword v5, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
        .expect("the skip");
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask read as data"),
        "{:?}",
        per_lane.err()
    );
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &lower::take_needs()), Some(vec![branch]));
    let shader = lowered(&native, &p, &HashSet::from([branch])).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (lanes below s0 inside, s1) -> what lane 31 stores: 2 if SCC was set at the branch
    for (below, s1, expected) in [(0u32, 7u32, 1u32), (10, 7, 2), (10, 3, 1), (32, 3, 1), (32, 7, 2)] {
        let Some(got) = on_the_card(&shader, [below, s1]) else { return };
        assert_eq!(got, expected, "{below} lanes inside, s1 = {s1}");
    }
    eprintln!("ran on the hardware device: the inner test's SCC for every lane once some lane is inside");
}

/// A dispatch counted in threads: of the last group only the lanes below the
/// count store (after DexNom/Oathkeep bc3cd4b0).
#[test]
fn the_lanes_past_a_count_of_threads_do_not_run() {
    // Each lane that runs adds 1 to counter[local x].
    let native = native(&[
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x7e04_0281, // v_mov_b32 v2, 1
        0xe0c8_1000, 0x8002_0201, // buffer_atomic_add v2, v1, s[8:11], 0 offen
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let shader = lowered_counted(&native, &p, &HashSet::new(), 64, true).unwrap();
    let limit = shader
        .user_data
        .iter()
        .find(|u| u.root == RootType::Other(super::USER_DISPATCH_THREADS_X))
        .unwrap();
    assert_eq!((limit.slot, limit.dword), (0, 64));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [0u32, 1, 7, 63, 64, 65, 74, 77, 127, 128] {
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        constants[limit.index as usize] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(68)],
                samplers: &[],
                groups: [2, 1, 1],
            })
            .unwrap();
        let active = if count == 0 { 128 } else { count };
        for (i, &word) in result[0].iter().enumerate().take(64) {
            let i = i as u32;
            assert_eq!(word, u32::from(i < active) + u32::from(i + 64 < active), "count {count}, word {i}");
        }
    }
}

/// Helper to build a wave64 compute program with custom thread count and LDS size
/// (`lds_blocks` in 512-byte GFX10 LDS blocks, max 64 = 32768 bytes).
fn native_with_lds(body: &[u32], threads: u32, lds_blocks: u32) -> NativeProgram {
    let mut p = native_of(body, true, threads);
    if let StageIo::Cs(cs) = &mut p.io {
        // COMPUTE_PGM_RSRC2: lds_size sits in bits 15..23 (units of 128 dwords = 512 B).
        cs.rsrc2 |= (lds_blocks & 0x1FF) << 15;
    }
    p
}

/// Wave64 compute program with 128 threads and full 32 KiB LDS using:
/// 1) a single-lane `if (v0 == 0)` block that reads and writes LDS,
/// 2) a divergent branch `if (v0 < s0)` that reads `lds[8]` while reserving
///    per-wave slots via `ds_add_rtn_u32` at disjoint `lds[4]` (wave-local
///    allocation idiom).
/// Verifies `wave_local` accepts LDS atomics, `lds_meetings` accepts single-half
/// and disjoint-offset regions, local lowering adds zero extra LDS dwords (fits
/// within the 32 KiB DXIL TGSM limit), validates with `dxil.dll`, and runs
/// accurately on a Wave32 GPU.
#[test]
fn lds_atomic_wave_local_idiom_and_full_32k_lds_lower_to_wave32() {
    // s0 = active thread count, s[8:11] = raw UAV
    // Single-lane init (v0 == 0): lds[4] = 0, lds[8] = 7
    // s_barrier
    // Single-lane update (v0 == 0): lds[8] = lds[8] + 2 (= 9)
    // s_barrier
    // Divergent (v0 < s0):
    //   read v1 = lds[8] (offset:8)
    //   s_ff1_i32_b64 s16, exec
    //   v_mbcnt_lo/hi(-1, 0) -> v4 (console lane)
    //   v_mbcnt_lo/hi(exec, 0) -> v2 (prefix)
    //   save exec -> s[14:15]; v_cmpx_eq_i32 exec, s16, v4
    //   s_bcnt1_i32_b64 s17, s[14:15]
    //   v_mov_b32 v3, 0; v_mov_b32 v5, s17
    //   ds_add_rtn_u32 v3, v3, v5 offset:4
    //   s_mov_b64 exec, s[14:15]
    //   v_readfirstlane_b32 s18, v3
    //   v_add_nc_u32 v2, s18, v2   (unique slot index 0..s0)
    //   buffer_store_dword v1, (v2 << 2), s[8:11], 0 offen
    let body: &[u32] = &[
        // 0: v_cmp_eq_u32 vcc, 0, v0
        0x7D84_0080,
        // 1: s_and_saveexec_b64 s[12:13], vcc
        0xBE8C_246A,
        // 2: s_cbranch_execz +6 -> word 9
        0xBF88_0006,
        // 3: v_mov_b32 v1, 0
        0x7E02_0280,
        // 4: v_mov_b32 v2, 7
        0x7E04_0287,
        // 5-6: ds_write_b32 v1, v1 offset:4
        0xD834_0004,
        0x0000_0101,
        // 7-8: ds_write_b32 v1, v2 offset:8
        0xD834_0008,
        0x0000_0201,
        // 9: s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
        // 10: s_barrier
        0xBF8A_0000,
        // 11: s_and_saveexec_b64 s[12:13], vcc  (single-lane read+write region)
        0xBE8C_246A,
        // 12: s_cbranch_execz +6 -> word 19
        0xBF88_0006,
        // 13: v_mov_b32 v1, 0
        0x7E02_0280,
        // 14-15: ds_read_b32 v2, v1 offset:8
        0xD8D8_0008,
        0x0200_0001,
        // 16: v_add_nc_u32 v2, 2, v2
        0x4A04_0482,
        // 17-18: ds_write_b32 v1, v2 offset:8
        0xD834_0008,
        0x0000_0201,
        // 19: s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
        // 20: s_barrier
        0xBF8A_0000,
        // 21: v_cmp_gt_u32 vcc, s0, v0  (v0 < s0)
        0x7D88_0000,
        // 22: s_and_saveexec_b64 s[12:13], vcc
        0xBE8C_246A,
        // 23: s_cbranch_execz +25 -> word 49
        0xBF88_0019,
        // 24: v_mov_b32 v1, 0
        0x7E02_0280,
        // 25-26: ds_read_b32 v1, v1 offset:8  (disjoint from offset:4)
        0xD8D8_0008,
        0x0100_0001,
        // 27: s_ff1_i32_b64 s16, exec
        0xBE90_147E,
        // 28: s_mov_b64 s[14:15], exec
        0xBE8E_047E,
        // 29-30: v_mbcnt_lo_u32_b32 v2, -1, 0
        0xD765_0002,
        0x0001_00C1,
        // 31-32: v_mbcnt_hi_u32_b32 v4, -1, v2
        0xD766_0004,
        0x0002_04C1,
        // 33-34: v_mbcnt_lo_u32_b32 v3, s14, 0
        0xD765_0003,
        0x0001_000E,
        // 35-36: v_mbcnt_hi_u32_b32 v2, s15, v3
        0xD766_0002,
        0x0002_060F,
        // 37: v_cmpx_eq_i32 exec, s16, v4
        0x7D24_0810,
        // 38: s_bcnt1_i32_b64 s17, s[14:15]
        0xBE91_100E,
        // 39: v_mov_b32 v3, 0
        0x7E06_0280,
        // 40: v_mov_b32 v5, s17
        0x7E0A_0211,
        // 41-42: ds_add_rtn_u32 v3, v3, v5 offset:4
        0xD880_0004,
        0x0300_0503,
        // 43: s_mov_b64 exec, s[14:15]
        0xBEFE_040E,
        // 44: v_readfirstlane_b32 s18, v3
        0x7E24_0503,
        // 45: v_add_nc_u32 v2, s18, v2
        0x4A04_0412,
        // 46: v_lshlrev_b32 v2, 2, v2
        0x3404_0482,
        // 47-48: buffer_store_dword v1, v2, s[8:11], 0 offen
        0xE070_1000,
        0x8002_0102,
        // 49: s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
    ];
    // 64 LDS blocks = 32768 bytes (full DXIL TGSM limit: zero extra LDS allowed!).
    let native = native_with_lds(body, 128, 64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::wave_local(&p, &st), Ok(()), "LDS atomic reservation must match wave_local idiom");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
    let io = io::raise(&p, &native).expect("io");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let out = lower::with_local_waves(|| {
        lower::lower(&lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        })
    });
    lower::TEST_HOST_WAVE.set(was);
    let out = out.expect("local wave32 lowering with full 32 KiB LDS must succeed");
    assert_eq!(out.wave_size, Some(32));
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: out.dxil,
        user_data: out.user_data,
        samplers: out.samplers,
        static_samplers: Vec::new(),
        root_cbvs: out.root_cbvs,
        root_srvs: out.root_srvs,
        root_uavs: out.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed (TGSM must stay <= 32768 B)");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [1u32, 37, 95, 128] {
        let mut constants = vec![0u32; shader.user_data_dwords() as usize];
        constants[0] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(128)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        for i in 0..128usize {
            let expected = if (i as u32) < count { 9 } else { 0 };
            assert_eq!(result[0][i], expected, "count={count} slot={i}");
        }
    }
}

/// Multi-wave (`group = 128`, two console waves) Wave64 compute shader where:
/// - Console Wave 0 (`v0 < 64`) runs a scalar loop (`s0` iterations) with
///   `v_readlane_b32(..., 63)` and `s_bcnt1_i32_b64`, accumulating into `uav[0]`.
/// - Console Wave 1 (`!(v0 < 64)`) runs an independent scalar loop (`s1` iterations)
///   with `v_readlane_b32(..., 63)` and `s_bcnt1_i32_b64`, accumulating into `uav[1]`.
/// Verifies `multi_wave_wide_safe` accepts it, `group_wide` lowers both widened
/// and guarded scalar branches in lockstep across all 4 Wave32 host waves,
/// passes `dxil.dll` validation, and executes deadlock-free on the host GPU.
#[test]
fn multi_wave_single_wave_regions_with_scalar_loops_run_deadlock_free() {
    // s0 = wave 0 loop iterations (>= 1), s1 = wave 1 loop iterations (>= 1)
    // s[12:13] = initial exec
    // v_cmpx_gt_u32 exec, 64, v0     ; v0 < 64 (Console Wave 0)
    // s_cbranch_execz -> wave1
    //   v_mov_b32 v5, s0
    //   v_mov_b32 v6, 0
    // loop0:
    //   s_bcnt1_i32_b64 s6, exec     ; 64 (crosses halves -> forces widening!)
    //   v_add_nc_u32 v6, s6, v6      ; v6 += 64
    //   v_sub_nc_u32 v5, v5, 1       ; remaining -= 1
    //   v_readlane_b32 s7, v5, 63    ; lane 63 broadcasts remaining
    //   s_cmp_eq_u32 s7, 0
    //   s_cbranch_scc0 loop0         ; guarded scalar back-edge!
    //   v_mov_b32 v1, 0
    //   buffer_store_dword v6, v1, s[8:11], 0 offen  ; uav[0] = 64 * s0
    // wave1:
    //   s_andn2_b64 exec, s[12:13], exec ; !(v0 < 64) (Console Wave 1)
    //   s_cbranch_execz -> done
    //   v_mov_b32 v5, s1
    //   v_mov_b32 v6, 0
    // loop1:
    //   s_bcnt1_i32_b64 s6, exec     ; 64
    //   v_add_nc_u32 v6, s6, v6
    //   v_sub_nc_u32 v5, v5, 1
    //   v_readlane_b32 s7, v5, 63    ; lane 63 of wave 1 (thread 127)
    //   s_cmp_eq_u32 s7, 0
    //   s_cbranch_scc0 loop1         ; guarded scalar back-edge!
    //   v_mov_b32 v1, 4
    //   buffer_store_dword v6, v1, s[8:11], 0 offen  ; uav[1] = 64 * s1
    // done:
    //   s_mov_b64 exec, s[12:13]
    //   s_barrier
    let body: &[u32] = &[
        // 0: s_mov_b64 s[12:13], exec
        0xBE8C_047E,
        // 1: v_cmpx_gt_u32 exec, 64, v0
        0x7DA8_00C0,
        // 2: s_cbranch_execz +12 -> word 15 (wave1)
        0xBF88_000C,
        // 3: v_mov_b32 v5, s0
        0x7E0A_0200,
        // 4: v_mov_b32 v6, 0
        0x7E0C_0280,
        // 5 (loop0): s_bcnt1_i32_b64 s6, exec
        0xBE86_107E,
        // 6: v_add_nc_u32 v6, s6, v6
        0x4A0C_0C06,
        // 7: v_sub_nc_u32 v5, v5, 1   (v_subrev_nc_u32 v5, 1, v5)
        0x4E0A_0A81,
        // 8-9: v_readlane_b32 s7, v5, 63
        0xD760_0007,
        0x0001_7F05,
        // 10: s_cmp_eq_u32 s7, 0
        0xBF06_8007,
        // 11: s_cbranch_scc0 -7 -> word 5 (loop0)
        0xBF84_FFF9,
        // 12: v_mov_b32 v1, 0
        0x7E02_0280,
        // 13-14: buffer_store_dword v6, v1, s[8:11], 0 offen
        0xE070_1000,
        0x8002_0601,
        // 15 (wave1): s_andn2_b64 exec, s[12:13], exec
        0x8AFE_7E0C,
        // 16: s_cbranch_execz +12 -> word 29 (done)
        0xBF88_000C,
        // 17: v_mov_b32 v5, s1
        0x7E0A_0201,
        // 18: v_mov_b32 v6, 0
        0x7E0C_0280,
        // 19 (loop1): s_bcnt1_i32_b64 s6, exec
        0xBE86_107E,
        // 20: v_add_nc_u32 v6, s6, v6
        0x4A0C_0C06,
        // 21: v_subrev_nc_u32 v5, 1, v5
        0x4E0A_0A81,
        // 22-23: v_readlane_b32 s7, v5, 63
        0xD760_0007,
        0x0001_7F05,
        // 24: s_cmp_eq_u32 s7, 0
        0xBF06_8007,
        // 25: s_cbranch_scc0 -7 -> word 19 (loop1)
        0xBF84_FFF9,
        // 26: v_mov_b32 v1, 4
        0x7E02_0284,
        // 27-28: buffer_store_dword v6, v1, s[8:11], 0 offen
        0xE070_1000,
        0x8002_0601,
        // 29 (done): s_mov_b64 exec, s[12:13]
        0xBEFE_040C,
        // 30: s_barrier
        0xBF8A_0000,
    ];
    let native = native_with_lds(body, 128, 1);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    // Discover branches to widen iteratively just like emit::widen().
    let mut wide = HashSet::new();
    let shader = loop {
        let st = structure::structure_wide(&p, &wide).expect("structure");
        let io = io::raise(&p, &native).expect("io");
        lower::take_needs();
        match lowered(&native, &p, &wide) {
            Ok(s) => {
                assert!(
                    lower::multi_wave_wide_safe(&p, &st, &io, [128, 1, 1]),
                    "single-console-wave widened regions must pass safety check"
                );
                break s;
            }
            Err(ShaderError::Unsupported { what, .. }) if lower::needs_every_lane(what) => {
                let more = lower::branches_around(&st, &lower::take_needs()).expect("branches");
                assert!(!more.is_empty(), "stuck on {what}");
                wide.extend(more);
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for (iters0, iters1) in [(1u32, 1u32), (1, 5), (4, 1), (3, 7), (6, 2)] {
        let mut constants = vec![0u32; shader.user_data_dwords() as usize];
        constants[0] = iters0;
        constants[1] = iters1;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(4)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        assert_eq!(result[0][0], 64 * iters0, "wave 0 result for ({iters0}, {iters1})");
        assert_eq!(result[0][1], 64 * iters1, "wave 1 result for ({iters0}, {iters1})");
    }
}

/// Negative safety check: a 128-thread shader whose widened branch can be entered
/// by BOTH console waves (`v0 < s0`) must be rejected by `multi_wave_wide_safe`.
#[test]
fn multi_wave_wide_safe_rejects_two_active_console_waves() {
    let body: &[u32] = &[
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0  (both waves can satisfy v0 < s0)
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0002, // s_cbranch_execz +2
        0xBE86_107E, // s_bcnt1_i32_b64 s6, exec
        0x7E00_0206, // v_mov_b32 v0, s6
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ];
    let native = native_with_lds(body, 128, 0);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure_wide(&p, &HashSet::from([0usize])).expect("structure");
    let io = io::raise(&p, &native).expect("io");
    assert!(
        !lower::multi_wave_wide_safe(&p, &st, &io, [128, 1, 1]),
        "branch where both console waves can be active must not be marked single-wave safe"
    );
}

/// Multi-wave (`group = 128`) shader combining:
/// 1) a single-console-wave (`v0 < 64`) divergent non-atomic LDS read+write region
///    with `s_bcnt1_i32_b64` (must be widened), and
/// 2) a separate two-console-wave (`v0 < s0`) divergent LDS region whose only write
///    is a disjoint atomic `ds_add_rtn_u32` (already safe under `lds_meetings`, must
///    NOT be pulled into `st.wide`).
/// Verifies targeted LDS region widening keeps `multi_wave_wide_safe` happy, validates
/// with `dxil.dll`, and computes exact results on the host GPU.
#[test]
fn multi_wave_widening_keeps_safe_atomic_lds_region_unwidened() {
    let body: &[u32] = &[
        // Phase 0 (tid == 0): lds[0] = 0, lds[4] = 5, lds[8] = 0
        0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0008, // s_cbranch_execz +8
        0x7E02_0280, // v_mov_b32 v1, 0
        0x7E04_0285, // v_mov_b32 v2, 5
        0xD834_0000, 0x0000_0101, // ds_write_b32 v1, v1 offset:0
        0xD834_0004, 0x0000_0201, // ds_write_b32 v1, v2 offset:4
        0xD834_0008, 0x0000_0101, // ds_write_b32 v1, v1 offset:8
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0xBF8A_0000, // s_barrier
        // Phase 1 (Console Wave 0 only: v0 < 64): non-atomic LDS read+write + s_bcnt1
        0xBE8C_047E, // s_mov_b64 s[12:13], exec
        0x7DA8_00C0, // v_cmpx_gt_u32 exec, 64, v0
        0xBF88_0007, // s_cbranch_execz +7
        0x7E02_0280, // v_mov_b32 v1, 0
        0xD8D8_0004, 0x0200_0001, // ds_read_b32 v2, v1 offset:4
        0xBE86_107E, // s_bcnt1_i32_b64 s6, exec
        0x4A04_0406, // v_add_nc_u32 v2, s6, v2
        0xD834_0008, 0x0000_0201, // ds_write_b32 v1, v2 offset:8
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0xBF8A_0000, // s_barrier
        // Phase 2 (BOTH console waves: v0 < s0): read lds[4], atomic add disjoint lds[0]
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0009, // s_cbranch_execz +9
        0x7E02_0280, // v_mov_b32 v1, 0
        0xD8D8_0004, 0x0200_0001, // ds_read_b32 v2, v1 offset:4
        0x7E06_0281, // v_mov_b32 v3, 1
        0xD880_0000, 0x0400_0301, // ds_add_rtn_u32 v4, v1, v3 offset:0
        0x7E0A_0288, // v_mov_b32 v5, 8
        0xE070_1000, 0x8002_0205, // buffer_store_dword v2, v5, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0xBF8A_0000, // s_barrier
        // Phase 3 (tid == 0): store lds[0] -> uav[0], lds[8] -> uav[1]
        0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_000A, // s_cbranch_execz +10
        0x7E02_0280, // v_mov_b32 v1, 0
        0xD8D8_0000, 0x0200_0001, // ds_read_b32 v2, v1 offset:0
        0xE070_1000, 0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
        0xD8D8_0008, 0x0200_0001, // ds_read_b32 v2, v1 offset:8
        0x7E02_0284, // v_mov_b32 v1, 4
        0xE070_1000, 0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ];
    let native = native_with_lds(body, 128, 1);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let mut wide = HashSet::new();
    let shader = loop {
        let st = structure::structure_wide(&p, &wide).expect("structure");
        let io = io::raise(&p, &native).expect("io");
        lower::take_needs();
        let was = lower::TEST_HOST_WAVE.replace(Some(32));
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
        let out = lower::lower(&lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        });
        lower::TEST_HOST_WAVE.set(was);
        let needs = lower::take_needs();
        match out {
            Ok(out) if out.wave_size == Some(64) && !needs.is_empty() => {
                let more = lower::branches_around(&st, &needs).expect("branches");
                assert!(!more.is_empty(), "stuck on wave64 LDS");
                wide.extend(more);
            }
            Ok(out) => {
                assert_eq!(out.wave_size, Some(32), "must lower to wave32");
                assert_eq!(wide.len(), 1, "only Phase 1 branch should be widened, got {wide:?}");
                assert!(
                    lower::multi_wave_wide_safe(&p, &st, &io, [128, 1, 1]),
                    "two-wave atomic LDS region must stay unwidened so multi_wave_wide_safe passes"
                );
                break HostShader {
                    stage: HwStage::Cs,
                    dxil: out.dxil,
                    user_data: out.user_data,
                    samplers: out.samplers,
                    static_samplers: Vec::new(),
                    root_cbvs: out.root_cbvs,
                    root_srvs: out.root_srvs,
                    root_uavs: out.root_uavs,
                    root_indexed_views: res.root_indexed_views,
                };
            }
            Err(ShaderError::Unsupported { what, .. }) if lower::needs_every_lane(what) => {
                let more = lower::branches_around(&st, &needs).expect("branches");
                assert!(!more.is_empty(), "stuck on {what}");
                wide.extend(more);
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [1u32, 37, 95, 128] {
        let mut constants = vec![0u32; shader.user_data_dwords() as usize];
        constants[0] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(4)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        assert_eq!(result[0][0], count, "atomic LDS counter for count={count}");
        assert_eq!(result[0][1], 69, "wave 0 widened LDS result for count={count}");
        assert_eq!(result[0][2], 5, "disjoint LDS read for count={count}");
    }
}

/// Single-console-wave (`8x8x1 = 64` threads) compute program with LDS whose
/// lane-divergent branch performs both an LDS write and an LDS read plus a
/// cross-half wave reduction (`WAVE_OR_V7_TO_S7`). Initial lowering returns
/// `Ok((_, Some(64)))` because `lds_meetings` rejects the divergent read+write
/// region; `emit::emit` must automatically widen the branch (`single_wave_lds`)
/// without requiring a hardcoded container digest, pass `dxil.dll` validation,
/// and produce exact results across both 32-lane host waves.
#[test]
fn single_wave_lds_branch_lowers_through_emit() {
    let mut body = vec![
        // Flatten 2D `[8, 8, 1]` thread ID (`v0 = x`, `v1 = y`) into `v0 = y * 8 + x` (`0..63`).
        0x3402_0283, // v_lshlrev_b32 v1, 3, v1
        0x4A00_0101, // v_add_nc_u32 v0, v1, v0
        // Inside `if (v0 < s0)`: write `(v0 + 10)` to `lds[v0 * 4]`, read `lds[0]`,
        // compute wave-wide OR of `1 << (v0 >> 1)`, and store `(wave_or + lds[0])`.
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0021, // s_cbranch_execz +33 (to restore of exec)
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x4A04_008A, // v_add_nc_u32 v2, 10, v0
        0xD834_0000, 0x0000_0201, // ds_write_b32 v1, v2 offset:0
        0x7E06_0280, // v_mov_b32 v3, 0
        0xD8D8_0000, 0x0400_0003, // ds_read_b32 v4, v3 offset:0
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
    ];
    body.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    body.extend_from_slice(&[
        0x4A00_0807, // v_add_nc_u32 v0, s7, v4
        0xE070_1000, 0x8002_0003, // buffer_store_dword v0, v3, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    let mut native = native_with_lds(&body, 64, 1);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.threads = [8, 8, 1];
        cs.rsrc2 |= 1 << 11; // TIDIG_COMP_CNT = 1 (v0 = x, v1 = y)
    }
    let container = crate::dxbc::Container {
        digest: [0xaa; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
    let io = io::raise(&p, &native).expect("io");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let emitted = super::emit(&container, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let shader = emitted.expect("single-wave LDS branch must lower to wave32 via emit()");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false).or_else(|_| Host::new(true)) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [1u32, 19, 32, 33, 50, 64] {
        let mut constants = vec![0u32; shader.user_data_dwords() as usize];
        constants[0] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(4)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .expect("dispatch");
        let expected_or = (0..count).fold(0u32, |acc, t| acc | (1u32 << (t >> 1)));
        assert_eq!(
            result[0][0],
            expected_or.wrapping_add(10),
            "widened 8x8x1 LDS + wave OR mismatch for count={count}"
        );
    }
}

