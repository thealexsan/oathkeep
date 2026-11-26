fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// `s_ttracedata_imm 0x1234` as LLVM's GFX10 disassembler tests have it
/// (`gfx10_sopp.txt`: `0x34,0x12,0xa8,0xbf`), SOPP opcode 0x28 as in
/// AMD's RDNA2 ISA. A thread-trace marker, which translation drops.
#[test]
fn s_ttracedata_imm_is_sopp_0x28() {
    let code = bytes(&[0xBFA8_1234, 0xBF81_0000]);
    let decoded = super::super::decode(&code).unwrap();
    assert_eq!(decoded[0].name(), Some("s_ttracedata_imm"));
    assert!(crate::ir::build(&code, false, 1).is_ok());
}
