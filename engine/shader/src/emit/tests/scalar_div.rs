use super::super::shader_opt;
use crate::native::{HwStage, NativeProgram};


/// `s_buffer_load_dwordx4`, `x8` and `x16` from a raw SRV read the dwords
/// the console's range-checked load reads, a call a dword or
/// (`OATHKEEP_SHADER_OPT` with 64) a call for every four: also where a
/// call's four dwords lie across the view's end (the dwords out of it
/// are 0, the others are read). On the software adapter only: run the
/// test once plain and once with `OATHKEEP_SHADER_OPT=127`.
#[test]
#[cfg(windows)]
fn scalar_buffer_loads_read_the_same_dwords_one_call_or_four() {
    // (first register, dwords) of the three loads, in order.
    const LOADS: [(u32, u32); 3] = [(16, 8), (24, 4), (32, 16)];
    let mut words: Vec<u32> = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0 (the SRV)
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20 (the UAV)
        0xF42C_0404,
        0x0000_0000, // s_buffer_load_dwordx8 s[16:23], s[8:11], s0
        0xF428_0604,
        0xFA00_0008, // s_buffer_load_dwordx4 s[24:27], s[8:11], 0x8
        0xF430_0804,
        0x0200_0004, // s_buffer_load_dwordx16 s[32:47], s[8:11], s1, 0x4
    ];
    let mut out_dword = 0u32;
    for (first, n) in LOADS {
        for k in 0..n {
            // v_mov_b32 v1, s<first + k>
            words.push(0x7E00_0000 | 1 << 17 | 0x001 << 9 | (first + k));
            // buffer_store_dword v1, s[12:15], offset 4 * out_dword
            words.push(0x38 << 26 | 0x1C << 18 | (4 * out_dword));
            words.push(0x80 << 24 | (12 >> 2) << 16 | 1 << 8);
            out_dword += 1;
        }
    }
    words.push(0xBF81_0000); // s_endpgm
    let table = |kind, at| crate::UsedResource {
        kind,
        root: crate::RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 4,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![
            crate::RootParam {
                root: crate::RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            crate::RootParam {
                root: crate::RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![table(crate::ResourceType::Srv, 0), table(crate::ResourceType::Uav, 8)],
        io: crate::native::StageIo::Cs(crate::native::ComputeIo {
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, None).unwrap();
    let io = crate::io::raise(&p, &native).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let l = super::super::lower::lower(&super::super::lower::Parts {
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
    .unwrap();
    // The calls that read the SRV: 28 a dword, 7 with four a call.
    let wide = shader_opt() & crate::emit::bitcode::opt::WIDE != 0;
    let mut dxil = l.dxil;
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut dxil).unwrap();
    }
    let _one = super::valu_helpers::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = crate::emit::host::Host::new(true) else {
        eprintln!("no software adapter; the dispatch is skipped");
        return;
    };
    let cs = crate::emit::HostShader {
        stage: HwStage::Cs,
        dxil,
        user_data: l.user_data,
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        root_indexed_views: Vec::new(),
    };
    let (root, pso) = host.compute_pipeline(&cs).unwrap();
    // Twelve dwords: the loads below run across the view's end.
    let data: Vec<u32> = (0..12u32).map(|i| 0x1111_1111u32.wrapping_mul(i + 1) ^ 0x8000_0000).collect();
    let dword = |byte: u64| -> u32 {
        if byte.is_multiple_of(4) && byte + 4 <= 4 * data.len() as u64 { data[(byte / 4) as usize] } else { 0 }
    };
    for (s0, s1) in [(0u32, 0u32), (16, 0), (24, 4), (40, 28), (48, 44), (8, 12)] {
        let out = host
            .dispatch(
                &root,
                &pso,
                &crate::emit::host::Dispatch {
                    roots: vec![(0, crate::emit::host::Root::Constants(vec![s0, s1, 0, 0, 0]))],
                    views: &[
                        crate::emit::host::View::Buffer {
                            data: &data,
                            stride: crate::emit::host::Stride::Raw,
                        },
                        crate::emit::host::View::RawUav(32),
                    ],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        let starts = [u64::from(s0), 8, u64::from(s1) + 4];
        let mut want = Vec::new();
        for (&(_, n), start) in LOADS.iter().zip(starts) {
            want.extend((0..u64::from(n)).map(|k| dword(start + 4 * k)));
        }
        eprintln!("offsets {s0} and {s1}, four dwords a call: {wide}: {:x?}", &out[0][..28]);
        assert_eq!(out[0][..28], want[..], "offsets {s0} and {s1}, four dwords a call: {wide}");
    }
}

#[test]
fn valu_div_f32_sequence_executes_on_gpu() {
    let words: &[u32] = &[
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
        0x340E_0083, // v_lshlrev_b32 v7, 3, v0
        0xE034_1000,
        0x8002_0107, // buffer_load_dwordx2 v[1:2], v7, s[8:11], 0 offen
        0xD56D_6A03,
        0x0406_0502, // v_div_scale_f32 v3, vcc_lo, v2, v2, v1
        0xD56D_1008,
        0x0406_0501, // v_div_scale_f32 v8, s16, v1, v2, v1
        0x7E08_5503, // v_rcp_f32 v4, v3
        0xD54B_0005,
        0x23CA_0903, // v_fma_f32 v5, -v3, v4, 1.0
        0xD54B_0004,
        0x0412_0905, // v_fma_f32 v4, v5, v4, v4
        0x100A_0908, // v_mul_f32 v5, v8, v4
        0xD54B_0006,
        0x2422_0B03, // v_fma_f32 v6, -v3, v5, v8
        0xD56F_0004,
        0x0416_0906, // v_div_fmas_f32 v4, v6, v4, v5
        0xD55F_0001,
        0x0406_0504, // v_div_fixup_f32 v1, v4, v2, v1
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0xE070_1000,
        0x8003_0107, // buffer_store_dword v1, v7, s[12:15], 0 offen
        0xBF81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 9,
        layout: vec![
            crate::RootParam {
                root: crate::RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            crate::RootParam {
                root: crate::RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![
            crate::UsedResource {
                kind: crate::ResourceType::Srv,
                root: crate::RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 4,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
            crate::UsedResource {
                kind: crate::ResourceType::Uav,
                root: crate::RootType::Table,
                table_offset_dwords: 8,
                lower_bound: 0,
                stride: 4,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
        ],
        io: crate::native::StageIo::Cs(crate::native::ComputeIo {
            rsrc2: 5 << 1,
            threads: [7, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, None).unwrap();
    let io = crate::io::raise(&p, &native).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let l = super::super::lower::lower(&super::super::lower::Parts {
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
    .unwrap();
    let mut dxil = l.dxil;
    #[cfg(windows)]
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut dxil).unwrap();
    }
    #[cfg(windows)]
    if let Ok(host) = crate::emit::host::Host::new(false) {
        let cs = crate::emit::HostShader {
            stage: HwStage::Cs,
            dxil,
            user_data: l.user_data,
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: l.root_cbvs,
            root_srvs: l.root_srvs,
            root_uavs: l.root_uavs,
            root_indexed_views: Vec::new(),
        };
        let (root, pso) = host.compute_pipeline(&cs).unwrap();
        let inputs: Vec<u32> = [
            7.0f32, 3.0, -1.0, 1e-40, 1.0, 1e35, 1e-35, 1e-36, 0.0, 0.0, -1.0, 0.0, -1e35, 1e35,
        ]
        .into_iter()
        .map(f32::to_bits)
        .collect();
        let out = host
            .dispatch(
                &root,
                &pso,
                &crate::emit::host::Dispatch {
                    roots: vec![(0, crate::emit::host::Root::Constants(vec![0, 0, 0, 0, 0]))],
                    views: &[
                        crate::emit::host::View::Buffer {
                            data: &inputs,
                            stride: crate::emit::host::Stride::Raw,
                        },
                        crate::emit::host::View::RawUav(64),
                    ],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        let got: Vec<f32> = out[0][..7].iter().copied().map(f32::from_bits).collect();
        assert!((got[0] - (7.0f32 / 3.0)).abs() < 1e-6, "got {:?}", got);
        assert_eq!(out[0][1], (-1.0f32 / 1e-40).to_bits());
        assert!(
            (got[2] - 1e-35).abs() <= 1e-40,
            "lane 2 got {:?} ({:#x})",
            got[2],
            out[0][2]
        );
        assert!(
            (got[3] - 10.0).abs() < 1e-5,
            "lane 3 got {:?} ({:#x})",
            got[3],
            out[0][3]
        );
        assert_eq!(out[0][4], 0xFFC0_0000);
        assert_eq!(out[0][5], (-1.0f32 / 0.0).to_bits());
        assert!(
            (got[6] - (-1.0)).abs() < 1e-6,
            "lane 6 got {:?} ({:#x})",
            got[6],
            out[0][6]
        );
    }
}

