use crate::gfx10::{Cache, Fields, Operand, decode};

/// Text of each instruction in `words` (a straight-line program).
fn texts(words: &[u32], wave64: bool) -> Vec<String> {
    let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    code.extend(0xBF81_0000u32.to_le_bytes()); // s_endpgm
    let insts = decode(&code).unwrap();
    insts[..insts.len() - 1]
        .iter()
        .map(|i| i.text(wave64))
        .collect()
}

fn one(words: &[u32], wave64: bool) -> String {
    let t = texts(words, wave64);
    assert_eq!(t.len(), 1, "{t:?}");
    t[0].clone()
}

#[test]
fn scalar() {
    assert_eq!(
        texts(&[0xBF88_0001, 0xBF8C_0070, 0xBE80_03FF, 0x1234_5678], true),
        [
            "s_cbranch_execz label_0008",
            "s_waitcnt vmcnt(0) & lgkmcnt(0)",
            "s_mov_b32 s0, lit(0x12345678)",
        ]
    );
    assert_eq!(one(&[0x8782_027E], true), "s_and_b64 s[2:3], exec, s[2:3]");
    assert_eq!(
        one(&[0xF408_0501, 0xFA00_0040], true),
        "s_load_dwordx4 s[20:23], s[2:3], 0x000040"
    );
    assert_eq!(
        one(&[0xF420_0144, 0x0400_0000], true),
        "s_buffer_load_dword s5, s[8:11], s2"
    );
}

/// `s_atc_probe` / `s_atc_probe_buffer`: SDATA is a 7-bit immediate, the
/// buffer resource is 4 dwords, and GLC/DLC are not operands.
///
/// Words checked with `llvm-mc -arch=amdgcn -mcpu=gfx1030 --disassemble`.
/// The offset keeps this printer's 6-digit hex (`0x000064`, not `0x64`).
#[test]
fn atc_probe_immediate_is_not_a_register() {
    let cases = [
        (
            [0xF498_01C2, 0xFA00_0064],
            "s_atc_probe 7, s[4:5], 0x000064",
            Operand::Literal(7),
            Operand::Scalar { code: 4, dwords: 2 },
        ),
        (
            [0xF498_1FF4, 0x0A00_0001],
            "s_atc_probe 0x7f, s[104:105], s5 offset:0x000001",
            Operand::Literal(0x7f),
            Operand::Scalar {
                code: 104,
                dwords: 2,
            },
        ),
        (
            [0xF498_0000, 0xFA00_0000],
            "s_atc_probe 0, s[0:1], null",
            Operand::Literal(0),
            Operand::Scalar { code: 0, dwords: 2 },
        ),
        (
            [0xF49C_01C4, 0xFA00_0020],
            "s_atc_probe_buffer 7, s[8:11], 0x000020",
            Operand::Literal(7),
            Operand::Scalar { code: 8, dwords: 4 },
        ),
        (
            [0xF499_41C2, 0xFA00_0000],
            "s_atc_probe 7, s[4:5], null",
            Operand::Literal(7),
            Operand::Scalar { code: 4, dwords: 2 },
        ),
    ];
    for (words, text, data, base) in cases {
        assert_eq!(one(&words, false), text, "{words:08x?}");
        assert_eq!(one(&words, true), text, "{words:08x?}");
        let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        code.extend(0xBF81_0000u32.to_le_bytes());
        let inst = &decode(&code).unwrap()[0];
        let Fields::Smem {
            data: got,
            base: got_base,
            cache,
            ..
        } = inst.fields(false)
        else {
            panic!("not an SMEM instruction");
        };
        assert_eq!(got, Some(data), "{words:08x?}");
        assert_eq!(got_base, base, "{words:08x?}");
        assert_eq!(cache, Cache::default(), "{words:08x?}");
    }
}

#[test]
fn lane_masks_follow_the_wave_size() {
    let cmp = [0x7C08_06F0]; // v_cmp_gt_f32 with 0.5 and v3
    assert_eq!(one(&cmp, false), "v_cmp_gt_f32 vcc_lo, 0.5, v3");
    assert_eq!(one(&cmp, true), "v_cmp_gt_f32 vcc, 0.5, v3");
    let vop3 = [0xD401_0106, 0x0001_E501]; // v_cmp_lt_f32 to s6, |v1| < 1.0
    assert_eq!(one(&vop3, false), "v_cmp_lt_f32 s6, abs(v1), 1.0");
    assert_eq!(one(&vop3, true), "v_cmp_lt_f32 s[6:7], abs(v1), 1.0");
    assert_eq!(
        one(&[0x0202_0480], false),
        "v_cndmask_b32 v1, 0, v2, vcc_lo"
    );
    assert_eq!(
        one(&[0x5006_0801], true),
        "v_add_co_ci_u32 v3, vcc, s1, v4, vcc"
    );
}

#[test]
fn vector_modifiers() {
    assert_eq!(
        one(&[0x1004_0AFF, 0x3F00_0001], true),
        "v_mul_f32 v2, lit(0x3f000001), v5"
    );
    assert_eq!(
        one(&[0xD54B_8004, 0x2BC0_0501], true),
        "v_fma_f32 v4, -v1, s2, 0.5 mul:2 clamp"
    );
    assert_eq!(
        one(&[0x7E02_16F9, 0x0605_0602], true),
        "v_cvt_f32_f16 v1, v2 src0_sel:WORD_1"
    );
    assert_eq!(
        one(&[0x7E06_02E9, 0x0539_7701], true),
        "v_mov_b32 v3, v1 dpp8:[7,6,5,4,3,2,1,0]"
    );
    assert_eq!(
        one(&[0x7E0A_02FA, 0xFF08_B106], true),
        "v_mov_b32 v5, v6 quad_perm:[1,0,3,2] bound_ctrl:1"
    );
    assert_eq!(
        one(&[0xCC20_0C01, 0x2C10_0702], true),
        "v_fma_mix_f32 v1, -sel_hi(v2), s3, abs(v4)"
    );
}

#[test]
fn memory_and_export() {
    assert_eq!(
        one(&[0xE034_7010, 0x0102_0402], true),
        "buffer_load_dwordx2 v[4:5], v[2:3], s[8:11], s1 idxen offen offset:16 glc"
    );
    assert_eq!(
        one(&[0xD8DC_0302, 0x0600_0001], true),
        "ds_read2_b32 v[6:7], v1 offset0:2 offset1:3"
    );
    assert_eq!(
        one(&[0xF800_1C0F, 0x0000_0302], true),
        "exp mrt0, v2, v2, v3, v3 done compr vm"
    );
}

#[test]
fn one_sided_packed_negation_is_printed() {
    // Encodings from llvm-mc -mcpu=gfx1030. A source negated in both
    // halves keeps the `-` prefix; one-sided bits get LLVM's suffixes.
    let got = [
        one(&[0xCC0F_4001, 0x1A02_0702], true),
        one(&[0xCC0F_4001, 0x3A02_0702], true),
        one(&[0xCC0F_4201, 0x1A02_0702], true),
        one(&[0xCC0F_4101, 0x7A02_0702], true),
    ];
    assert_eq!(
        got,
        [
            "v_pk_add_f16 v1, v2, v3",
            "v_pk_add_f16 v1, v2, v3 neg_lo:[1,0]",
            "v_pk_add_f16 v1, v2, v3 neg_hi:[0,1]",
            "v_pk_add_f16 v1, -v2, v3 neg_lo:[0,1]",
        ]
    );
}

#[test]
fn fields_the_instruction_lacks_are_not_printed() {
    // Encodings from llvm-mc -mcpu=gfx1030; s_get_waveid_in_workgroup is gfx1010 only.
    let got = [
        one(&[0xF490_0500, 0], true),
        one(&[0xF4A8_0140, 0], true),
        one(&[0xDC30_8000, 0x007D_0002], true),
        one(&[0xE031_1000, 0x0001_0001], true),
        one(&[0x7E02_02F9, 0x0005_0602], true),
    ];
    assert_eq!(
        got,
        [
            "s_memtime s[20:21]",
            "s_get_waveid_in_workgroup s5",
            "global_load_dword v0, v[2:3], off",
            "buffer_load_dword v1, s[4:7], s0 offen lds",
            "v_mov_b32 v1, v2 src0_sel:WORD_1",
        ]
    );
}

#[test]
fn image_d16_data_is_packed() {
    // Three f16 channels occupy two VGPRs; NSA lists the addresses.
    let words = [0xF09C_070A, 0x8024_0801, 0x0000_0005];
    assert_eq!(
        one(&words, true),
        "image_sample_lz v[8:9], [v1,v5], s[16:23], s[4:7] dmask:0x7 dim:SQ_RSRC_IMG_2D d16"
    );
    let code: Vec<u8> = words
        .iter()
        .chain(&[0xBF81_0000])
        .flat_map(|w| w.to_le_bytes())
        .collect();
    let Fields::Image { data, addr, .. } = decode(&code).unwrap()[0].fields(true) else {
        panic!("not an image instruction");
    };
    assert_eq!(data, Operand::Vector { reg: 8, dwords: 2 });
    assert_eq!(addr.len(), 2);
}
