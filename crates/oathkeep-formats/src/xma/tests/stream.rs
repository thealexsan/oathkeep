use super::*;
use super::super::packet::Cursor;

#[test]
fn packet_rejects_out_of_range_indices_and_partial_headers() {
    assert!(packet(&[0; 4], usize::MAX / PACKET_BYTES + 1).is_none());
    assert!(packet(&[0; 3], 0).is_none());
}

#[test]
fn payload_bytes_match_bit_reads_at_packet_edges() {
    let mut data: Vec<u8> = (0..3 * PACKET_BYTES).map(|i| (i * 37 + 11) as u8).collect();
    data[..4].copy_from_slice(&1u32.to_be_bytes()); // packet 1 belongs to another stream
    data[PACKET_BYTES..2 * PACKET_BYTES].fill(0xee);
    for edge in [HEADER_BITS + 8, PACKET_BITS, 3 * PACKET_BITS] {
        for pos in edge - 8..=edge {
            let mut bytes = Cursor { data: &data, pos };
            let mut bits = Cursor { data: &data, pos };
            let expected = bits.bits(8).map(|v| v as u8);
            assert_eq!(bytes.byte(), expected, "bit {pos}");
            if expected.is_some() {
                assert_eq!(bytes.pos, bits.pos);
            }
        }
    }
}

#[test]
fn packet_headers_and_frame_walk() {
    // Two packets: frame A (100 bits) and B (16300 bits, crossing into
    // packet 1 by 48 bits), then C (60 bits) in packet 1; the rest is
    // padding.
    let mut data = vec![0xffu8; 2 * PACKET_BYTES];
    let header = |frames: u32, first: u32| (frames << 26) | (first << 11);
    let bits = [(0, header(2, 0), 32), (PACKET_BITS, header(1, 48), 32)];
    let put = |data: &mut Vec<u8>, at: usize, value: u32, width: u32| {
        for i in 0..width as usize {
            let bit = (value >> (width as usize - 1 - i)) & 1;
            let p = at + i;
            data[p / 8] = (data[p / 8] & !(1 << (7 - p % 8))) | ((bit as u8) << (7 - p % 8));
        }
    };
    for &(at, v, w) in &bits {
        put(&mut data, at, v, w);
    }
    put(&mut data, 32, 100, 15);
    put(&mut data, 132, 16300, 15);
    let c_start = PACKET_BITS + 32 + 48;
    put(&mut data, c_start, 60, 15);
    let a = read_frame(&data, 32).unwrap();
    assert_eq!((a.bits, a.next), (100, Some(132)));
    let b = read_frame(&data, 132).unwrap();
    assert_eq!((b.bits, b.next), (16300, Some(c_start)));
    // Its last bits come from after packet 1's header.
    assert_eq!(b.data.len(), 16300usize.div_ceil(8));
    assert_eq!(b.data.last(), Some(&0xf0)); // unused bits stay zero
    let c = read_frame(&data, c_start).unwrap();
    assert_eq!(c.bits, 60);
    // C was packet 1's only frame: the next would be in packet 2.
    assert_eq!(c.next, None);
}

/// Deterministic noise: xorshift64, as in BCPack's noise test.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// `len` bytes of even noise, or of mostly zeros (`density` 1) or
    /// mostly ones (2), which take other paths through a frame.
    fn bytes(&mut self, len: usize, density: usize) -> Vec<u8> {
        (0..len)
            .map(|_| {
                let r = self.next();
                let (a, b, c) = (r as u8, (r >> 8) as u8, (r >> 16) as u8);
                match density {
                    0 => a,
                    1 => a & b & c,
                    _ => a | b | c,
                }
            })
            .collect()
    }
}

/// Writes `value` as `n` bits, MSB first, at bit `pos`, over what is there.
fn set_bits(data: &mut [u8], pos: usize, n: u32, value: u32) {
    for i in 0..n as usize {
        let (p, bit) = (pos + i, (value >> (n as usize - 1 - i)) & 1);
        data[p / 8] = (data[p / 8] & !(0x80 >> (p % 8))) | ((bit as u8) << (7 - p % 8));
    }
}

/// `packets` packets of one stream: back-to-back frames of noise after
/// their length fields, 300 to 4299 bits long (so every packet but the
/// last has a frame start, and at most 55 do), then padding of ones.
/// The headers count the frames that begin in each packet. Also gives
/// the number of frames.
fn noise_stream(noise: &mut Noise, packets: usize, density: usize) -> (Vec<u8>, usize) {
    const PAYLOAD: usize = PACKET_BITS - HEADER_BITS;
    let mut data = noise.bytes(packets * PACKET_BYTES, density);
    // Where payload bit `q` of the stream is in the buffer.
    let at = |q: usize| q / PAYLOAD * PACKET_BITS + HEADER_BITS + q % PAYLOAD;
    let mut headers = vec![(0u32, 0u32); packets];
    let (mut q, mut frames) = (0, 0);
    loop {
        let len = 300 + noise.below(4000);
        if q + len > packets * PAYLOAD {
            break;
        }
        let header = &mut headers[q / PAYLOAD];
        if header.0 == 0 {
            header.1 = (q % PAYLOAD) as u32;
        }
        header.0 += 1;
        for i in 0..LENGTH_BITS {
            let bit = (len >> (LENGTH_BITS - 1 - i)) as u32 & 1;
            set_bits(&mut data, at(q + i as usize), 1, bit);
        }
        q += len;
        frames += 1;
    }
    for padding in q..packets * PAYLOAD {
        set_bits(&mut data, at(padding), 1, 1);
    }
    for (p, &(count, first)) in headers.iter().enumerate() {
        set_bits(&mut data, p * PACKET_BITS, 32, count << 26 | first << 11);
    }
    (data, frames)
}

/// Packets of noise, streams of frames of noise, and such streams with
/// headers and bits damaged: walking the frames from the first packet
/// and decoding each, mono and stereo, gives a frame of samples per
/// frame and ends within the frames the buffer can hold (each is over
/// 15 bits and the walk only moves on).
#[test]
fn noise_packets_walk_and_decode() {
    let mut noise = Noise(0x2545_f491_4f6c_dd1d);
    for round in 0..60 {
        let packets = 1 + round % 4;
        let density = [0, 0, 1, 2][round / 3 % 4];
        let (mut data, written) = match round % 3 {
            0 => (noise.bytes(packets * PACKET_BYTES, density), 0),
            _ => noise_stream(&mut noise, packets, density),
        };
        if round % 3 == 2 {
            for _ in 0..1 + noise.below(3) {
                let p = noise.below(packets);
                set_bits(&mut data, p * PACKET_BITS, 32, noise.next() as u32);
            }
            for _ in 0..noise.below(16) {
                let bit = noise.below(data.len() * 8);
                data[bit / 8] ^= 0x80 >> (bit % 8);
            }
        }
        let budget = data.len() * 8 / 16;
        for channels in [1, 2] {
            let mut decoder = Decoder::new(channels, 48_000);
            let per_frame = FRAME_SAMPLES * channels;
            let (mut pcm, mut frames) = (Vec::new(), 0);
            let mut at = packet(&data, 0).map(|p| HEADER_BITS + p.first_frame_bit as usize);
            while let Some(frame) = at.and_then(|bit| read_frame(&data, bit)) {
                frames += 1;
                let moves_on = frame.next.is_none_or(|next| next > frame.start);
                assert!(frames <= budget && moves_on, "round {round}");
                let _ = decoder.decode(&frame.data, frame.bits, &mut pcm);
                assert_eq!(pcm.len(), frames * per_frame, "round {round}");
                at = frame.next;
            }
            if round % 3 == 1 {
                // A clean stream: every frame is walked.
                assert_eq!(frames, written, "round {round}");
            }
            decoder.flush(&mut pcm);
            assert_eq!(pcm.len(), frames * per_frame + per_frame / 2);
        }
    }
}

/// Frames of noise, mono and stereo at each sample rate, with bit
/// counts up to the end of the data: every decode gives one frame of
/// samples, whether the frame decodes or not.
#[test]
fn noise_frames_decode_to_a_frame_of_samples() {
    let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
    for channels in [1, 2] {
        for rate in [24_000, 32_000, 44_100, 48_000] {
            let mut decoder = Decoder::new(channels, rate);
            let mut pcm = Vec::new();
            for round in 0..48 {
                let data = noise.bytes(round * 89 % 4096, [0, 0, 1, 2][round % 4]);
                let bits = match round % 2 {
                    0 => data.len() * 8,
                    _ => noise.below(data.len() * 8 + 1),
                };
                pcm.clear();
                let _ = decoder.decode(&data, bits, &mut pcm);
                assert_eq!(
                    pcm.len(),
                    FRAME_SAMPLES * channels,
                    "{channels} channels, {rate} Hz, round {round}"
                );
                if round % 16 == 15 {
                    decoder.reset();
                }
            }
            pcm.clear();
            decoder.flush(&mut pcm);
            assert_eq!(pcm.len(), FRAME_SAMPLES / 2 * channels);
        }
    }
}

/// Bit counts past the end of a frame's data, up to `usize::MAX`: the
/// decoding ends, with a frame of samples. A stereo tile header that ran
/// out of data used to loop on the zeros past it for good (found by
/// this test).
#[test]
fn noise_frames_with_bit_counts_past_the_data() {
    let mut noise = Noise(0xd1b5_4a32_d192_ed03);
    for channels in [1, 2] {
        let mut decoder = Decoder::new(channels, 48_000);
        let mut pcm = Vec::new();
        for round in 0..48 {
            let len = [0, 1, 2, 3, 5, 8, 40, 300][round % 8];
            let data = noise.bytes(len, [0, 0, 1, 2][round / 8 % 4]);
            let bits = match round % 3 {
                0 => data.len() * 8 + 1 + noise.below(1 << 16),
                1 => usize::MAX / 2 + noise.below(1 << 20),
                _ => usize::MAX,
            };
            pcm.clear();
            let _ = decoder.decode(&data, bits, &mut pcm);
            assert_eq!(
                pcm.len(),
                FRAME_SAMPLES * channels,
                "{channels} channels, round {round}"
            );
        }
    }
}

/// `read_frame` from any bit of a buffer of noise and past it, and
/// `packet` at any index: a frame read stays inside its limits.
#[test]
fn noise_offsets_read_a_frame_or_nothing() {
    let mut noise = Noise(0x6a09_e667_f3bc_c908);
    for round in 0..24 {
        let packets = 1 + round % 3;
        let data = noise.bytes(packets * PACKET_BYTES, [0, 0, 1, 2][round % 4]);
        let all = data.len() * 8;
        let edges = [
            0,
            1,
            HEADER_BITS - 1,
            HEADER_BITS,
            PACKET_BITS - LENGTH_BITS as usize,
            PACKET_BITS - 1,
            PACKET_BITS,
            all - 1,
            all,
            all + 1,
            usize::MAX / 2,
            usize::MAX - PACKET_BITS + 1,
            usize::MAX,
        ];
        let mut starts = edges.to_vec();
        starts.extend((0..32).map(|_| noise.below(all)));
        for start in starts {
            if let Some(frame) = read_frame(&data, start) {
                let lengths = LENGTH_BITS as usize + 1..0x7fff;
                assert!(lengths.contains(&frame.bits), "start {start}");
                assert_eq!(frame.data.len(), frame.bits.div_ceil(8));
                if let Some(next) = frame.next {
                    assert!(next > start && next / 8 < data.len(), "start {start}");
                }
            }
        }
        let far = usize::MAX / PACKET_BYTES;
        for index in [0, packets - 1, packets, far, usize::MAX] {
            assert_eq!(packet(&data, index).is_some(), index < packets, "{index}");
        }
    }
}

/// Walks and decodes saved input buffers (`OATHKEEP_XMA_STREAMS=<dir>` of
/// `*.xma` files with their `*.ctx` context words, made outside the
/// repository): the frames walked from the first packet must be the
/// frames the packet headers count, and every frame must decode.
#[test]
#[ignore = "needs streams outside the repository"]
fn walks_saved_streams() {
    let dir = std::env::var("OATHKEEP_XMA_STREAMS").unwrap();
    let mut bad = 0;
    let mut streams = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "xma") {
            continue;
        }
        streams += 1;
        let data = std::fs::read(&path).unwrap();
        let counted: u32 = (0..data.len() / PACKET_BYTES)
            .map(|i| packet(&data, i).unwrap().frames)
            .sum();
        // Context word 1: sample rate in bits 27-28, stereo in bit 29.
        let ctx = std::fs::read(path.with_extension("ctx")).unwrap();
        let word = u32::from_le_bytes(ctx[4..8].try_into().unwrap());
        let rate = [24_000, 32_000, 44_100, 48_000][(word >> 27 & 3) as usize];
        let mut decoder = Decoder::new(1 + (word >> 29 & 1) as usize, rate);
        let (mut walked, mut errors, mut peak, mut pcm) = (0, 0, 0i16, Vec::new());
        let mut at = Some(HEADER_BITS + packet(&data, 0).unwrap().first_frame_bit as usize);
        while let Some(frame) = at.and_then(|bit| read_frame(&data, bit)) {
            pcm.clear();
            errors += decoder.decode(&frame.data, frame.bits, &mut pcm).is_err() as u32;
            peak = pcm.iter().fold(peak, |p, s| p.max(s.saturating_abs()));
            walked += 1;
            at = frame.next;
        }
        println!(
            "{}: {} packets, {counted} frames counted, {walked} walked, {errors} bad, peak {peak}",
            path.display(),
            data.len() / PACKET_BYTES
        );
        bad += u32::from(walked != counted || errors > 0);
    }
    assert!(streams > 0, "OATHKEEP_XMA_STREAMS contains no .xma files");
    assert_eq!(bad, 0);
}

/// Decodes a stream's packets and compares with a reference decode of
/// the same file (16-bit PCM): `OATHKEEP_XMA_REFERENCE=<dir>` holding
/// `mono.xmadata`/`mono.ref` and `stereo.xmadata`/`stereo.ref` (made
/// outside the repository).
#[test]
#[ignore = "needs reference files outside the repository"]
fn matches_a_reference_decoder() {
    let dir = std::path::PathBuf::from(std::env::var("OATHKEEP_XMA_REFERENCE").unwrap());
    for (name, channels) in [("mono", 1), ("stereo", 2)] {
        let data = std::fs::read(dir.join(format!("{name}.xmadata"))).unwrap();
        let reference: Vec<i16> = std::fs::read(dir.join(format!("{name}.ref")))
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        let mut decoder = Decoder::new(channels, 48_000);
        let mut pcm = Vec::new();
        let mut at = Some(HEADER_BITS + packet(&data, 0).unwrap().first_frame_bit as usize);
        let mut errors = 0;
        while let Some(frame) = at.and_then(|bit| read_frame(&data, bit)) {
            errors += decoder.decode(&frame.data, frame.bits, &mut pcm).is_err() as usize;
            at = frame.next;
        }
        // Find the decoder delay, then measure the error.
        let best = (0..2048 / channels)
            .map(|d| {
                let n = reference.len().min(pcm.len() - d * channels);
                let e: f64 = (0..n)
                    .map(|i| (pcm[d * channels + i] as f64 - reference[i] as f64).powi(2))
                    .sum();
                (e, d, n)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap();
        let signal: f64 = reference.iter().map(|&s| (s as f64).powi(2)).sum();
        let snr = 10.0 * (signal / best.0.max(1.0)).log10();
        println!(
            "{name}: {} samples, {errors} bad frames, delay {} samples, SNR {snr:.1} dB",
            pcm.len() / channels,
            best.1
        );
        assert!(errors == 0 && snr > 40.0);
    }
}
