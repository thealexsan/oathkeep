use super::{PIECE, merge, packed_len};

#[test]
fn a_piece_takes_what_changed() {
    let mut title = [7u8; PIECE];
    let mut ours = vec![0u8; PIECE];
    assert_eq!(merge(&title, &mut ours, true), PIECE / 4);
    assert_eq!(ours, title);
    assert_eq!(merge(&title, &mut ours, false), 0);
    // A dword of it.
    title[41] = 8;
    assert_eq!(merge(&title, &mut ours, false), 1);
    assert_eq!(ours, title);
}

#[test]
fn a_packed_structure_is_known_by_its_header() {
    let header = |version: u32, nodes: u32| u64::from(version) | u64::from(nodes) << 32;
    assert_eq!(packed_len(header(0x8102_0006, 0x9a1a)), Some(0x9a1b * 64));
    // Unpacked, a top level, another version, no size, too big.
    assert_eq!(packed_len(header(0x8002_0006, 0x9a1a)), None);
    assert_eq!(packed_len(header(0x0000_0006, 5)), None);
    assert_eq!(packed_len(header(0x8102_0007, 5)), None);
    assert_eq!(packed_len(header(0x8102_0006, 0)), None);
    assert_eq!(packed_len(header(0x8102_0006, 1 << 21)), None);
}
