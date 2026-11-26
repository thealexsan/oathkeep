mod imdct;
mod stream;

use super::*;

pub(super) fn put_bits(data: &mut [u8], pos: usize, n: u32, value: u32) {
    for i in 0..n as usize {
        if value >> (n as usize - 1 - i) & 1 == 1 {
            data[(pos + i) / 8] |= 1 << (7 - (pos + i) % 8);
        }
    }
}

/// A damaged packet whose first frame says it is 0 bits long: finding
/// the frame after a later one must give up, not walk in place forever
/// (found by random-input testing).
#[test]
fn a_zero_length_frame_before_the_start_ends_the_search() {
    let mut data = vec![0u8; PACKET_BYTES];
    // Two frames, the first at payload bit 0; its 15-bit length stays 0.
    put_bits(&mut data, 0, 32, 2 << 26);
    let start = HEADER_BITS + 100;
    put_bits(&mut data, start, LENGTH_BITS, 200);
    let frame = read_frame(&data, start).expect("the frame at `start` is well formed");
    assert_eq!(frame.bits, 200);
    assert_eq!(frame.next, None);
}

/// A frame that crosses a packet where no frame begins (a full
/// continuation) and ends in another such packet, followed by padding: the
/// next frame is the first one of the next packet that has frames, not the
/// padding the frame ended in.
#[test]
fn the_frame_after_continuation_packets_starts_in_the_next_packet_with_frames() {
    let mut data = vec![0u8; 4 * PACKET_BYTES];
    let header = |frames: u32, first: u32| (frames << 26) | (first << 11);
    put_bits(&mut data, 0, 32, header(2, 0));
    // Packet 1 is all of one frame; packet 2 holds only its last 64 bits.
    put_bits(&mut data, PACKET_BITS, 32, header(0, 0x7fff));
    put_bits(&mut data, 2 * PACKET_BITS, 32, header(0, 64));
    put_bits(&mut data, 3 * PACKET_BITS, 32, header(1, 0));
    // A first frame of 16000 bits, then one from the end of its payload
    // through packets 1 and 2.
    let second = HEADER_BITS + 16000;
    put_bits(&mut data, HEADER_BITS, LENGTH_BITS, 16000);
    let tail = PACKET_BITS - second;
    let bits = tail + (PACKET_BITS - HEADER_BITS) + 64;
    put_bits(&mut data, second, LENGTH_BITS, bits as u32);
    put_bits(&mut data, 3 * PACKET_BITS + HEADER_BITS, LENGTH_BITS, 100);
    let first = read_frame(&data, HEADER_BITS).unwrap();
    assert_eq!(first.next, Some(second));
    let frame = read_frame(&data, second).unwrap();
    assert_eq!(frame.bits, bits);
    assert_eq!(frame.next, Some(3 * PACKET_BITS + HEADER_BITS));
    assert_eq!(read_frame(&data, 3 * PACKET_BITS + HEADER_BITS).unwrap().next, None);
}
