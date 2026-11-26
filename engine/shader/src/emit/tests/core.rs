use crate::dxbc::{Container, FourCC};
use crate::emit::container::SigElement;
use crate::emit::{StaticSampler, container, emit};
use super::super::config::{parse_loop_limit, parse_shader_opt};
use super::super::lower::LOOP_LIMIT;
use crate::native::{ComputeIo, HwStage, NativeProgram, RootType, StageIo};
use crate::{ShaderError, io, ir, resource, structure};

/// The LLVM text `dxc -dumpbin` prints for a DXIL container, when dxc
/// is installed.
#[cfg(windows)]

pub(super) fn dump_dxil(container: &[u8]) -> Option<String> {
    let dxc = match std::env::var_os("OATHKEEP_PC_DXC") {
        Some(p) => std::path::PathBuf::from(p),
        None => crate::emit::validator::Validator::default_path()?.with_file_name("dxc.exe"),
    };
    if !dxc.exists() {
        return None;
    }
    let file = std::env::temp_dir().join(format!("victory-interp-{}.dxil", std::process::id()));
    std::fs::write(&file, container).ok()?;
    let out = std::process::Command::new(dxc)
        .arg("-dumpbin")
        .arg(&file)
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "dxc -dumpbin failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(windows)]
#[test]
fn interpolated_f16_writes_the_op_sel_half_with_modifiers() {
    // p1_f32 v2, v0, attr0.x; then a p2_f16 completing it into v3, which holds a value
    // (v2's own is no number here: a first half is evaluated with its second).
    let p2 = |dst_high: bool, omod: u32, clamp: bool| {
        let vgpr = |n: u32| 256 + n;
        let mut w0 = 0x35u32 << 26 | 0x35au32 << 16 | 3;
        if dst_high {
            w0 |= 1 << 14; // op_sel[3]: the high destination half.
        }
        if clamp {
            w0 |= 1 << 15;
        }
        let w1 = vgpr(1) << 9 | vgpr(2) << 18;
        [w0, w1 | omod << 27]
    };
    let dump = |dst_high: bool, omod: u32, clamp: bool| {
        let [w0, w1] = p2(dst_high, omod, clamp);
        let words = [
            0x7E06_0301u32, // v_mov_b32 v3, v1
            0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
            w0,
            w1, // v_interp_p2_f16 v3, attr0, v1, v2
            0xF800_180F,
            0x0303_0303, // exp mrt0, v3, v3, v3, v3 done vm
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: crate::native::StageIo::Ps {
                inputs: vec![crate::native::PsInput {
                    usage: crate::native::Usage::Generic,
                    row: 0,
                    mask: 0b1111,
                    attr: 0,
                    flat: false,
                    fp16: true,
                    pack2: false,
                    row2: 0,
                }],
                input_addr: 2,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let el = |name: &str, kind, usage, half| SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage,
            half,
        };
        let isg1 = crate::emit::container::signature(&[el("TEXCOORD", 0, 0, true)], true);
        let osg1 = crate::emit::container::signature(&[el("SV_Target", 16, 15, false)], false);
        let bytes =
            crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        emit(&c, &native, &p, &st, &io, &res).unwrap().dxil
    };
    let (Some(low), Some(high)) = (
        dump_dxil(&dump(false, 0, false)),
        dump_dxil(&dump(true, 1, true)),
    ) else {
        eprintln!("no dxc.exe; DXIL check skipped");
        return;
    };
    // The low-half write keeps the destination's high half.
    assert!(low.contains("-65536"), "low write keeps the high half");
    // The high-half write shifts the result up by 16.
    assert!(high.contains("shl i32"), "op_sel[3] writes the high half");
    // omod mul:2 and clamp (dx.op.unary.f32, opcode 7) are applied.
    assert!(high.contains("2.000000e+00"), "omod multiplies by two");
    assert!(
        high.contains("dx.op.unary.f32(i32 7"),
        "clamp saturates the result"
    );
    assert!(!low.contains("dx.op.unary.f32(i32 7"));
}

#[test]
fn the_clean_up_switch_parses_or_runs_every_pass() {
    let (all, usual) = (crate::emit::bitcode::opt::ALL, crate::emit::bitcode::opt::DEFAULT);
    assert_eq!(usual, 63, "the passes of the builds before the wide loads");
    assert_eq!(parse_shader_opt(None), usual);
    assert_eq!(parse_shader_opt(Some("0")), 0);
    assert_eq!(parse_shader_opt(Some(" 1 ")), 1);
    assert_eq!(parse_shader_opt(Some("0x1")), 1);
    assert_eq!(parse_shader_opt(Some("127")), all);
    assert_eq!(parse_shader_opt(Some("0xffffffff")), all);
    assert_eq!(parse_shader_opt(Some("on")), usual);
}

#[test]
fn the_loop_limit_switch_parses_or_falls_back() {
    assert_eq!(parse_loop_limit(None), LOOP_LIMIT);
    assert_eq!(parse_loop_limit(Some("4096")), 4096);
    assert_eq!(parse_loop_limit(Some(" 0x1000 ")), 4096);
    assert_eq!(parse_loop_limit(Some("1")), 1);
    for bad in ["", "0", "-5", "lots", "0x", "4294967296"] {
        assert_eq!(parse_loop_limit(Some(bad)), LOOP_LIMIT, "{bad:?}");
    }
}

/// A pixel shader writing SV_StencilRef (channel y of `exp mrtz`).
/// DXC sets ShaderFlags.StencilRef (bit 11) for a pixel shader whose
/// output signature has SV_StencilRef, and the feature bit
/// D3D_SHADER_FEATURE_STENCIL_REF (0x200) in SFI0; the validator checks
/// both against the module.
#[test]
fn a_stencil_export_declares_the_stencil_ref_feature() {
    let words = [
        0x7E00_02F2u32, // v_mov_b32 v0, 1.0
        0x7E02_0285,    // v_mov_b32 v1, 5
        0xF800_1883,
        0x0000_0100, // exp mrtz v0, v1, off, off done vm
        0xBF81_0000,
    ];
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 2,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let el = |name: &str, kind, comp| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp,
        interp: 0,
        row: None,
        col: 0,
        cols: 1,
        usage: 1,
        half: false,
    };
    let osg1 =
        container::signature(&[el("SV_Depth", 17, 9), el("SV_StencilRef", 20, 5)], false);
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let out = emit(&c, &native, &p, &st, &io, &res).unwrap();
    // dxil.dll, where there is one, checks the flags against the module.
    #[cfg(windows)]
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut out.dxil.clone()).unwrap();
    }
    let host = Container::parse(&out.dxil).unwrap();
    let sfi0 = host.part(FourCC::SFI0).unwrap().data;
    let features = u64::from_le_bytes(sfi0.try_into().unwrap());
    assert_ne!(features & 0x200, 0, "SFI0 {features:#x}: no StencilRef");
}

/// A pixel program with an ordered section, as the console compiler
/// makes one of rasterizer ordered views (the title's hair cards keep
/// their nearest layers that way): the wait for overlapped waves is
/// skipped, the views inside the section are the host's ordered ones,
/// the one after it is not.
#[test]
fn an_ordered_section_uses_rasterizer_ordered_views() {
    let section = [
        0xBE82_0300u32, // s_mov_b32 s2, s0
        0xBE83_0380,    // s_mov_b32 s3, 0
        0xF40C_0201,    // s_load_dwordx8 s[8:15], s[2:3], 0
        0xFA00_0000,
        0x9390_FF01, // s_bfe_u32 s16, s1, 0x2001c
        0x0002_001C,
        0x9391_FF01, // s_bfe_u32 s17, s1, 0xa0000
        0x000A_0000,
        0x8F10_8110, // s_lshl_b32 s16, s16, 1
        0x9392_FF01, // s_bfe_u32 s18, s1, 0xa0010
        0x000A_0010,
        0xBE91_0711, // s_not_b32 s17, s17
        0x9013_9F01, // s_lshr_b32 s19, s1, 31
        0x8810_8110, // s_or_b32 s16, s16, 1
        0x8112_1112, // s_add_i32 s18, s18, s17
        0xBF06_8013, // s_cmp_eq_u32 s19, 0
        0xB990_1019, // s_setreg_b32 hwreg(HW_REG_POPS_PACKER, 0, 3), s16
        0x8514_807E, // s_cselect_b32 s20, exec_lo, 0
        0x8115_11EF, // s_add_i32 s21, src_pops_exiting_wave_id, s17
        0xBF08_1215, // s_cmp_gt_u32 s21, s18
        0x8516_807E, // s_cselect_b32 s22, exec_lo, 0
        0x8814_1614, // s_or_b32 s20, s20, s22
        0x876A_147E, // s_and_b32 vcc_lo, exec_lo, s20
        0xBF87_0004, // s_cbranch_vccnz past the wait
        0xBF8E_FFFF, // s_sleep 0xffff
        0x8115_11EF, // s_add_i32 s21, src_pops_exiting_wave_id, s17
        0xBF0A_1512, // s_cmp_lt_u32 s18, s21
        0xBF84_FFFC, // s_cbranch_scc0 back to the sleep
        0x7E00_0280, // v_mov_b32 v0, 0
        0x7E02_0280, // v_mov_b32 v1, 0
        0xF000_3F88, // image_load v[2:5], v[0:1], s[8:15] dmask:0xf 2D glc dlc
        0x0002_0200,
        0x0604_04F2, // v_add_f32 v2, 1.0, v2
        0xF020_3F08, // image_store v[2:5], v[0:1], s[8:15] dmask:0xf 2D glc
        0x0002_0200,
        0xBEFC_0380, // s_mov_b32 m0, 0
        0xBF90_0007, // s_sendmsg sendmsg(MSG_ORDERED_PS_DONE)
    ];
    let end = [
        0xF800_180Fu32, // exp mrt0 v2, v2, v2, v2 done vm
        0x0202_0202,
        0xBF81_0000, // s_endpgm
    ];
    // A store after the section, through an ordinary view.
    let after = [0xF020_3F08u32, 0x0002_0200];
    let lower = |words: Vec<u32>| {
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 8,
            layout: vec![crate::native::RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![crate::native::UsedResource {
                kind: crate::native::ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 4,
                dwords: 8,
                user_data: 0,
                slot: 0,
            }],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let target = SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        };
        let osg1 = container::signature(&[target], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
        let st = structure::structure(&p)?;
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, &native)?;
        let out = emit(&c, &native, &p, &st, &io, &res)?;
        #[cfg(windows)]
        if let Ok(mut v) = crate::emit::validator::Validator::load(
            crate::emit::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
        let host = Container::parse(&out.dxil).unwrap();
        let sfi0 = host.part(FourCC::SFI0).unwrap().data;
        Ok::<u64, ShaderError>(u64::from_le_bytes(sfi0.try_into().unwrap()))
    };
    // D3D_SHADER_REQUIRES_ROVS
    let features = lower([&section[..], &end].concat()).unwrap();
    assert_ne!(features & 0x1000, 0, "SFI0 {features:#x}: no ROVs");
    lower([&section[..], &after, &end].concat()).unwrap();
    // The same accesses outside a section: ordinary views, no feature.
    let plain = [&section[..4], &section[28..35], &end].concat();
    let features = lower(plain).unwrap();
    assert_eq!(features & 0x1000, 0, "SFI0 {features:#x}: ROVs");
    // Without the section's start the wait stays unknown.
    let mut unmarked = [&section[..], &end].concat();
    unmarked.remove(16);
    assert!(lower(unmarked).is_err());
}

#[test]
fn immediate_sampler_slots_distinguish_comparison_uses() {
    let words = [
        0xBE81_0380u32, // s_mov_b32 s1, 0
        0xF40C_0000,
        0xFA00_0000, // s_load_dwordx8 s[0:7], s[0:1], null
        0xBE88_0380, // s_mov_b32 s8, 0
        0xBE89_0380, // s_mov_b32 s9, 0
        0xBE8A_0380, // s_mov_b32 s10, 0
        0xBE8B_0380, // s_mov_b32 s11, 0
        0x7E00_02F0, // v_mov_b32 v0, 0.5 (comparison reference)
        0x7E02_02F0, // v_mov_b32 v1, 0.5 (u)
        0x7E04_02F0, // v_mov_b32 v2, 0.5 (v)
        0xBF8C_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
        0xF080_0108,
        0x0040_0301, // image_sample v3, v[1:2], s[0:7], s[8:11]
        0xF0A0_0108,
        0x0040_0400, // image_sample_c v4, v[0:2], s[0:7], s[8:11]
        0xF0BC_0108,
        0x0040_0500, // image_sample_c_lz v5, v[0:2], s[0:7], s[8:11]
        0xF080_0108,
        0x0040_0601, // image_sample v6, v[1:2], s[0:7], s[8:11]
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xF800_180F,
        0x0605_0403, // exp mrt0, v3, v4, v5, v6 done vm
        0xBF81_0000,
    ];
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 7,
        layout: vec![crate::native::RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![crate::native::UsedResource {
            kind: crate::native::ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let output = SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    };
    let osg1 = container::signature(&[output], false);
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let out = emit(&c, &native, &p, &st, &io, &res).unwrap();

    let sampler = StaticSampler::from_words([0; 4]);
    let comparison = StaticSampler {
        filter: sampler.filter | 0x80,
        ..sampler
    };
    assert_eq!(out.samplers, [[0; 4]; 2]);
    assert_eq!(out.static_samplers, [sampler, comparison]);
    #[cfg(windows)]
    if let Some(path) = crate::emit::validator::Validator::default_path() {
        let mut v = crate::emit::validator::Validator::load(Some(&path)).unwrap();
        v.validate(&mut out.dxil.clone()).unwrap();
    }
}

/// A wave64 compute program that writes only EXEC's high dword
/// (`s_mov_b32 exec_hi, 0`) before a vector instruction. The new mask was
/// opaque and keyed by the unchanged low dword, so building its lane bit
/// needed the mask's own value: lowering recursed until the stack
/// overflowed, which takes the whole process down (found by
/// random-input testing). Now that low dword, seen with two high
/// dwords, has no mask, and the instruction fails like any other that
/// cannot be expressed: its result is missing, and only a side effect
/// that uses it would stop the translation.
#[test]
fn an_exec_high_write_does_not_overflow_the_stack() {
    // s_mov_b32 exec_hi, 0; v_mov_b32 v1, v0; s_endpgm
    let words = [0xBEFF_0380u32, 0x7E02_0300, 0xBF81_0000];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 2,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let container = crate::dxbc::build(&[]);
    let c = Container::parse(&container).unwrap();
    if let Err(e) = emit(&c, &native, &p, &st, &io, &res) {
        assert!(e.to_string().contains("its own value"), "{e}");
    }
}

