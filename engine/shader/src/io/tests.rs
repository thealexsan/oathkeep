use crate::ir::{Program, Reg};
use crate::native::{ExportPatch, HwStage, NativeProgram, PsInput, StageIo};
use super::halves::ps_vgprs;
use super::*;

const S_ENDPGM: u32 = 0xBF81_0000;

fn program(words: &[u32]) -> Program {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    crate::ir::build(&code, false, 256).unwrap()
}

fn pixel(inputs: Vec<PsInput>, input_addr: u32) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 256,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs,
            input_addr,
            db_shader_control: 0,
            patches: Vec::<ExportPatch>::new(),
        },
        code: vec![],
    }
}

const TEXCOORD: PsInput = PsInput {
    usage: Usage::Generic,
    row: 1,
    mask: 3,
    attr: 0,
    flat: false,
    fp16: false,
    pack2: false,
    row2: 1,
};

#[test]
fn interpolation_pairs_and_color_exports() {
    // Blit's pixel shader: TEXCOORD.xy from the perspective-center i/j
    // in v0/v1, then exported unconverted to MRT0.
    let p = program(&[
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        0xF800_180F,
        0x0302_0102, // exp mrt0, v2, v1, v2, v3 done vm
        S_ENDPGM,
    ]);
    let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
    let input = |reg| p.inputs.iter().find(|i| i.0 == Reg::V(reg)).unwrap().1;
    let center = |index| Input::Barycentric {
        kind: Barycentric::PerspectiveCenter,
        index,
    };
    assert!(io.inputs.contains(&(input(0), center(0))));
    assert!(io.inputs.contains(&(input(1), center(1))));
    // v2 and v3 are read before written but hold nothing.
    assert!(!io.inputs.iter().any(|i| i.0 == input(3)));
    assert_eq!(
        io.interps,
        [(
            (0, 1),
            Interp::Interpolated {
                attribute: Attribute {
                    usage: Usage::Generic,
                    row: 1,
                    channel: 0,
                    high: false
                },
                barycentric: Barycentric::PerspectiveCenter,
                first: vec![(0, 0)],
            }
        )]
    );
    let [e] = &io.exports[..] else {
        panic!("{:?}", io.exports)
    };
    assert_eq!(
        (e.target, e.done, e.compressed),
        (Target::Color(0), true, false)
    );
    assert_eq!(e.values[0], p.blocks[0].insts[1].write(Reg::V(2)));
}

#[test]
fn unpaired_or_unknown_interpolation_fails() {
    // p2 alone: its old dst is no p1's result.
    let lone = program(&[0xC809_0001, S_ENDPGM]); // v_interp_p2_f32 v2, v1, attr0.x
    assert!(raise(&lone, &pixel(vec![TEXCOORD], 2)).is_err());
    // attr1 is not in the stage data.
    let other = program(&[0xC808_0400, S_ENDPGM]); // v_interp_p1_f32 v2, v0, attr1.x
    assert!(raise(&other, &pixel(vec![TEXCOORD], 2)).is_err());
}

#[test]
fn computed_barycentrics_evaluate_each_half() {
    // i scaled before the first half: both halves stand on their own.
    let p = program(&[
        0x1006_00F0, // v_mul_f32 v3, 0.5, v0
        0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        S_ENDPGM,
    ]);
    let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
    let attribute = Attribute {
        usage: Usage::Generic,
        row: 1,
        channel: 0,
        high: false,
    };
    let mut interps = io.interps.clone();
    interps.sort_by_key(|i| i.0);
    assert_eq!(
        interps,
        [
            ((0, 1), Interp::Weighted { attribute }),
            ((0, 2), Interp::Weighted { attribute }),
        ]
    );
}

#[test]
fn first_halves_read_through_a_phi_keep_their_partial_values() {
    let p = program(&[
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        0xBF85_0002, // s_cbranch_scc1 -> second first half
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xBF82_0001, // s_branch -> export
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xF800_180F,
        0x0202_0202, // exp mrt0, v2, v2, v2, v2 done vm
        S_ENDPGM,
    ]);
    let io = raise(&p, &pixel(vec![TEXCOORD], 2)).unwrap();
    assert_eq!(io.interps.len(), 2);
    assert!(
        io.interps
            .iter()
            .all(|(_, i)| matches!(i, Interp::Weighted { .. }))
    );
}

#[test]
fn a_standalone_f16_first_half_is_weighted() {
    let p = program(&[
        0xD742_0002,
        0x0002_0000, // v_interp_p1ll_f16 v2, v0, attr0.x
        0xF800_180F,
        0x0202_0202,
        S_ENDPGM,
    ]);
    let io = raise(
        &p,
        &pixel(
            vec![PsInput {
                fp16: true,
                ..TEXCOORD
            }],
            2,
        ),
    )
    .unwrap();
    assert!(matches!(&io.interps[..], [(_, Interp::Weighted { .. })]));
}

#[test]
fn mesh_inputs_and_primitive_export() {
    let mesh = |words: &[u32]| {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, true, 256).unwrap();
        let native = NativeProgram {
            stage: HwStage::Ms,
            wave64: true,
            vgprs: 256,
            layout: vec![],
            used: vec![],
            io: StageIo::Ms(crate::native::MeshIo {
                outputs: vec![],
                clip_cntl: 0,
                threads: 128,
                max_vertices: 128,
                max_primitives: 128,
                topology: 2,
                lds_bytes: 0,
            }),
            code,
        };
        raise(&p, &native)
    };
    let io = mesh(&[
        0x7E04_0505, // v_readfirstlane_b32 s2, v5
        0xF800_0941,
        0x0000_0009, // exp prim, v9, off, off, off done
        S_ENDPGM,
    ])
    .unwrap();
    assert!(io.inputs.iter().any(|i| i.1 == Input::MeshGroup));
    assert_eq!(io.exports[0].target, Target::Primitive);
    // The group input is uniform only as the first lane reads it.
    assert!(mesh(&[0x7E02_0305, S_ENDPGM]).is_err()); // v_mov_b32 v1, v5
    // Also as an operand of a write to v5, or after a join.
    let per_lane: [&[u32]; 5] = [
        &[0x4A0A_0305, S_ENDPGM],              // v_add_nc_u32 v5, v5, v1
        &[0x7E0A_0D05, S_ENDPGM],              // v_cvt_f32_u32 v5, v5
        &[0xDC30_8000, 0x0500_0005, S_ENDPGM], // global_load_dword v5, v5, s[0:1]
        &[0x7E0A_CB01, S_ENDPGM],              // v_swap_b32 v5, v1
        &[
            0xBF06_8001, // s_cmp_eq_u32 s1, 0
            0xBF85_0001, // s_cbranch_scc1 -> 0xc
            0x7E0A_0280, // v_mov_b32 v5, 0
            0x7E02_0305, // 0xc: v_mov_b32 v1, v5
            S_ENDPGM,
        ],
    ];
    let accepted: Vec<_> = per_lane.iter().filter(|w| mesh(w).is_ok()).collect();
    assert!(accepted.is_empty(), "accepted: {accepted:08X?}");
    // Overwriting v5 reads the group input only for lanes outside EXEC:
    // v_mov_b32 v5, 0; v_mov_b32 v1, v5.
    assert!(mesh(&[0x7E0A_0280, 0x7E02_0305, S_ENDPGM]).is_ok());
    // Skipping that write with s_cbranch_execz is the same case: under the
    // same mask, the read sees only the 0.
    let reuse = [
        0x7E04_0505, // v_readfirstlane_b32 s2, v5
        0x7D88_0084, // v_cmp_gt_u32 vcc, 4, v0
        0xBE86_246A, // s_and_saveexec_b64 s[6:7], vcc
        0xBF88_0001, // s_cbranch_execz -> 0x14
        0x7E0A_0280, // v_mov_b32 v5, 0
        0x88FE_067E, // 0x14: s_or_b64 exec, exec, s[6:7]
        0xBE86_246A, // s_and_saveexec_b64 s[6:7], vcc
        0xBF88_0001, // s_cbranch_execz -> 0x24
        0x7E02_0305, // v_mov_b32 v1, v5
        0x88FE_067E, // 0x24: s_or_b64 exec, exec, s[6:7]
        S_ENDPGM,
    ];
    assert_eq!(mesh(&reuse).map(|_| ()), Ok(()));
}

#[test]
fn pixel_vgprs_follow_the_input_address() {
    // Linear center (bit 5), then position x and y (bits 8, 9).
    let v = ps_vgprs(1 << 5 | 1 << 8 | 1 << 9);
    assert_eq!(
        v,
        [
            (
                0,
                Input::Barycentric {
                    kind: Barycentric::LinearCenter,
                    index: 0
                }
            ),
            (
                1,
                Input::Barycentric {
                    kind: Barycentric::LinearCenter,
                    index: 1
                }
            ),
            (2, Input::Position(0)),
            (3, Input::Position(1)),
        ]
    );
}
