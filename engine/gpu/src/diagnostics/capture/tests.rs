#[test]
fn crc_matches_the_png_check_value() {
    assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
}

/// R16G16B16A16_UNORM is integers, not half floats; 89 is 10:10:10:2,
/// not B8G8R8A8.
#[test]
fn unorm16_and_xr_bias_targets_decode_as_their_formats() {
    // Red 0x8000 (a half-float -0), green 0x4000, blue 0.
    let unorm16 = [0x00, 0x80, 0x00, 0x40, 0, 0, 0xff, 0xff];
    assert_eq!(super::targets::to_rgba(11, 1, 1, 8, &unorm16), Some(vec![255, 127, 0, 255]));
    // Red 0x3ff, green 0x1ff, blue 0, alpha 3.
    let xr = (0x3ffu32 | 0x1ff << 10 | 3 << 30).to_le_bytes();
    assert_eq!(super::targets::to_rgba(89, 1, 1, 4, &xr), Some(vec![255, 127, 0, 255]));
    // B8G8R8A8 still swaps to RGBA.
    assert_eq!(super::targets::to_rgba(87, 1, 1, 4, &[1, 2, 3, 4]), Some(vec![3, 2, 1, 255]));
}
