use super::*;

const S_ENDPGM: u32 = 0xBF81_0000;
/// s_branch / s_cbranch_scc0 with a dword offset relative to the next instruction.
const fn sopp(op: u32, simm16: i16) -> u32 {
    0xBF80_0000 | op << 16 | simm16 as u16 as u32
}

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn summary(code: &[u32]) -> Vec<(usize, usize, &'static str)> {
    decode(&bytes(code))
        .unwrap()
        .iter()
        .map(|i| (i.offset, i.words.len(), i.name().unwrap_or("?")))
        .collect()
}

#[test]
fn flat_offsets_ignore_bit_eleven() {
    let offset = |word0: u32| {
        let code = bytes(&[word0, 0x0000_0001, S_ENDPGM]);
        match decode(&code).unwrap()[0].fields(false) {
            Fields::Flat { offset, .. } => offset,
            fields => panic!("{fields:?}"),
        }
    };
    // flat_load_dword v0, v[1:2]: the FLAT offset is 11-bit and
    // unsigned, so bit 11 is ignored (0x800 -> 0, 0xfff -> 0x7ff).
    assert_eq!(offset(0xDC30_0800), 0);
    assert_eq!(offset(0xDC30_0FFF), 0x7FF);
}

#[test]
fn tables_are_sorted() {
    for t in [
        opcodes::SOP2,
        opcodes::SOPK,
        opcodes::SOP1,
        opcodes::SOPC,
        opcodes::SOPP,
        opcodes::SMEM,
        opcodes::VOP2,
        opcodes::VOP1,
        opcodes::VOPC,
        opcodes::VOP3,
        opcodes::VOP3P,
        opcodes::VINTRP,
        opcodes::DS,
        opcodes::MUBUF,
        opcodes::MTBUF,
        opcodes::MIMG,
        opcodes::FLAT,
        opcodes::GLOBAL,
        opcodes::SCRATCH,
    ] {
        assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
    }
}

#[test]
fn lengths_and_names() {
    let code = [
        0xBE80_03FF,
        0x3F80_0000, // s_mov_b32 s0, literal
        0x0600_0000, // v_add_f32 v0, s0, v0
        0xD54B_0000,
        0x03FE_0000,
        0x4000_0000, // v_fma_f32 with a literal src2
        0xF080_0F0A,
        0,
        0, // image_sample (2D) with one extra address dword
        0xF800_080F,
        0, // exp
        S_ENDPGM,
    ];
    assert_eq!(
        summary(&code),
        [
            (0, 2, "s_mov_b32"),
            (8, 1, "v_add_f32"),
            (12, 3, "v_fma_f32"),
            (24, 3, "image_sample"),
            (36, 2, "exp"),
            (44, 1, "s_endpgm"),
        ]
    );
}

#[test]
fn follows_branches_and_skips_data() {
    let code = [
        sopp(0x04, 1), // s_cbranch_scc0 -> 8
        sopp(0x02, 1), // s_branch -> 12
        S_ENDPGM,
        S_ENDPGM,
        S_ENDPGM, // data follows the program end
        0xFFFF_FFFF,
    ];
    let found: Vec<_> = summary(&code).iter().map(|i| i.0).collect();
    assert_eq!(found, [0, 4, 8, 12]);
}

#[test]
fn follows_far_jumps() {
    let code = [
        0xBE88_1F00, // s_getpc_b64 s[8:9]
        0x8008_FF08,
        20,          // s_add_u32 s8, s8, 20  -> 4 + 20 = 24
        0x8209_8009, // s_addc_u32 s9, s9, 0
        0xBE80_2008, // s_setpc_b64 s[8:9]
        0xFFFF_FFFF, // not code
        S_ENDPGM,
    ];
    let found: Vec<_> = summary(&code).iter().map(|i| (i.0, i.2)).collect();
    assert_eq!(
        found,
        [
            (0, "s_getpc_b64"),
            (4, "s_add_u32"),
            (12, "s_addc_u32"),
            (16, "s_setpc_b64"),
            (24, "s_endpgm"),
        ]
    );
    // Through other registers (a handover to another stage): the path ends.
    assert_eq!(summary(&[0xBE80_2018, S_ENDPGM]).len(), 1);
}

#[test]
fn follows_far_jumps_backward() {
    // The compiler's backward far jump adds a negative offset: the low
    // dword as a 32-bit literal, the high dword as -1 with the carry.
    let code = [
        sopp(0x02, 1), // s_branch -> 8, past the jump target
        S_ENDPGM,      // the far jump's target: only it reaches here
        0xBE88_1F00,   // s_getpc_b64 s[8:9]
        0x8008_FF08,
        0xFFFF_FFF8, // s_add_u32 s8, s8, -8 -> 12 - 8 = 4
        0x8209_C109, // s_addc_u32 s9, s9, -1
        0xBE80_2008, // s_setpc_b64 s[8:9]
    ];
    let found: Vec<_> = summary(&code).iter().map(|i| (i.0, i.2)).collect();
    assert_eq!(
        found,
        [
            (0, "s_branch"),
            (4, "s_endpgm"),
            (8, "s_getpc_b64"),
            (12, "s_add_u32"),
            (20, "s_addc_u32"),
            (24, "s_setpc_b64"),
        ]
    );
}

#[test]
fn rejects_bad_code() {
    assert!(decode(&[0; 3]).is_err(), "size not a multiple of 4");
    assert!(
        decode(&bytes(&[sopp(0x02, 100)])).is_err(),
        "branch out of range"
    );
    assert!(
        decode(&bytes(&[0xBE80_03FF])).is_err(),
        "literal past the end"
    );
    assert!(
        decode(&bytes(&[0xE400_0000, 0])).is_err(),
        "unknown encoding"
    );
    // s_cbranch_scc0 into the literal of the s_mov_b32 at 4.
    assert!(decode(&bytes(&[sopp(0x04, 1), 0xBE80_03FF, S_ENDPGM, S_ENDPGM])).is_err());
}
