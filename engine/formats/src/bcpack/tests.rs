use super::*;
use super::bc7::Bits;
use super::plain::*;
use super::rans::*;

#[test]
fn the_shipped_tables_parse() {
    assert!(super::Tables::builtin().is_ok());
}

/// Tables we made: uniform models, identity ranks, a BC7 code per mode,
/// empty dictionaries.
fn tables() -> Vec<u8> {
    let mut t = Vec::new();
    for m in 0..MODELS {
        let g = m / 16;
        for b in 0..BUCKETS {
            let f: u16 = if b < g + 2 { 1 << (15 - g) } else { 0 };
            t.extend_from_slice(&f.to_le_bytes());
        }
    }
    t.extend((0..=255u8).map(|v| v & 0x77)); // rank1 (unused by the tests beyond 2-bit fields)
    t.extend(0..64u8); // rank3a
    t.extend(0..=255u8); // rank3c
    t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4)); // rank4: low 3 bits, next 3 bits
    t.extend(0..64u8); // rank5
    for code in 0..512u16 {
        // Code c: mode 6 (one subset), the only one the tests use.
        let ctrl: u16 = if code == 0 { 0x46 } else { 0 };
        t.extend_from_slice(&ctrl.to_le_bytes());
    }
    t.resize(TABLES_SIZE, 0);
    t
}

/// A lane's symbols as one fragment: uniform model, rANS from 2^24, no
/// reset, so the lane keeps 2^24 when it ends.
fn fragment(width: u8, symbols: &[u8]) -> Vec<u8> {
    let f = 1u32 << (15 - width);
    let mut x = LOW;
    let mut bytes = Vec::new();
    for &s in symbols.iter().rev() {
        let (freq, start) = if s == 0 {
            (f - 1, 1)
        } else {
            (f, u32::from(s) * f)
        };
        while x >= (freq << 16) {
            bytes.push(x as u8);
            x >>= 8;
        }
        x = ((x / freq) << 16) + x % freq + start;
    }
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.reverse();
    let mut out = vec![0];
    if bytes.len() > 256 {
        out[0] |= 0x20;
        let n = bytes.len() - 257;
        out.extend_from_slice(&[n as u8, (n >> 8) as u8]);
    } else {
        out.push((bytes.len() - 1) as u8);
    }
    out.extend(bytes);
    out
}

/// Interleaves per-lane fragments the way the decoder takes bytes. A lane
/// that has finished still asks: a filler fragment (two payload bytes)
/// brings its state to 2^24 with a byte latched, which parks it for good.
fn stream(t: &Tables, widths: &[u8], per: usize, records: &[Vec<u8>]) -> Vec<u8> {
    let stride = widths.len() * per;
    let mut queues: Vec<std::collections::VecDeque<u8>> = (0..widths.len())
        .map(|j| {
            let syms: Vec<u8> = records
                .iter()
                .flat_map(|r| r[j * per..j * per + per].to_vec())
                .collect();
            let mut bytes = fragment(widths[j], &syms);
            bytes.extend([0, 1, 0, 0]);
            bytes.into()
        })
        .collect();
    let mut out = Vec::new();
    let mut check = vec![0; records.len() * stride];
    let fetch = |j: usize| {
        let b = queues[j]
            .pop_front()
            .expect("a parked lane asked for input");
        out.push(b);
        Some(b)
    };
    lanes(t, widths, per, records.len(), fetch, &mut check).unwrap();
    assert_eq!(check, records.concat());
    out
}

const BC4_LANES: [u8; 12] = [7, 7, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5];

fn zigzag(v: i32) -> u8 {
    if v >= 0 {
        (2 * v) as u8
    } else {
        (-2 * v - 1) as u8
    }
}

#[test]
fn bc4_blocks_with_and_without_references() {
    let t = Tables::parse(&tables()).unwrap();
    // Block 0: endpoints 10, -3 (+128 stored), indices p -> p & 7.
    let mut b0 = vec![0, 0, zigzag(10), zigzag(-3)];
    b0.extend((0..8).map(|k| ((2 * k) & 7) | ((2 * k + 1) & 7) << 3));
    // Block 1 refers to block 0 for both: residual endpoints +1, +1; indices +1.
    let mut b1 = vec![0, 1, zigzag(1), zigzag(1)];
    b1.extend((0..8).map(|_| 1 | 1 << 3));
    let src = stream(&t, &BC4_LANES, 1, &[b0, b1]);
    let mut dst = [0u8; 16];
    decode(4, &src, &mut dst, &t).unwrap();
    let block = |e0: u8, e1: u8, ix: &dyn Fn(usize) -> usize| {
        let bits = (0..16).fold(0u64, |a, p| a | BC4_CODE[ix(p)] << (16 + 3 * p));
        (u64::from(e0) | u64::from(e1) << 8 | bits).to_le_bytes()
    };
    assert_eq!(dst[..8], block(138, 125, &|p| p & 7));
    assert_eq!(dst[8..], block(139, 126, &|p| (p + 1) & 7));
}

#[test]
fn lanes_take_bytes_in_schedule_order_and_long_fragments() {
    let t = Tables::parse(&tables()).unwrap();
    // Many blocks: long fragments (over 256 payload bytes) on the wide lanes.
    let records: Vec<Vec<u8>> = (0..400u32)
        .map(|i| {
            let mut r = vec![0, 0, (i * 7) as u8, (i * 13) as u8];
            r.extend((0..8).map(|k| ((i + k) * 5 % 64) as u8));
            r
        })
        .collect();
    let src = stream(&t, &BC4_LANES, 1, &records);
    let mut got = vec![0u8; records.len() * 12];
    let mut next = src.iter().copied();
    lanes(&t, &BC4_LANES, 1, records.len(), |_| next.next(), &mut got).unwrap();
    assert_eq!(got, records.concat());
}

#[test]
fn bc7_mode6_block() {
    let t = Tables::parse(&tables()).unwrap();
    // Control code 0 -> mode 6; endpoints R 8 bits (p-bit low), G B A 7 bits.
    let mut s = vec![0u8; 32];
    s[4] = zigzag(0x55); // R0 = 0x55: field 0x2a, p-bit 1
    s[5] = zigzag(-1); // R1 = 0xff (8 bits)
    s[6] = zigzag(3);
    s[10] = zigzag(0x7f >> 1);
    let src = stream(
        &t,
        &[3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4],
        2,
        &[s],
    );
    let mut dst = [0u8; 16];
    decode(7, &src, &mut dst, &t).unwrap();
    let mut w = Bits::default();
    w.put(1 << 6, 7);
    for (v, n) in [
        (0x2a, 7),
        (0x7f, 7),
        (3, 7),
        (0, 7),
        (0, 7),
        (0, 7),
        (0x3f, 7),
        (0, 7),
    ] {
        w.put(v, n);
    }
    w.put(1, 1);
    w.put(1, 1);
    assert_eq!(dst, w.v.to_le_bytes());
}

#[test]
fn malformed_input_is_an_error() {
    let t = Tables::parse(&tables()).unwrap();
    let records: Vec<Vec<u8>> = (0..64u8)
        .map(|i| vec![0, 0, i, i, 1, 2, 3, 4, 5, 6, 7, 8])
        .collect();
    let src = stream(&t, &BC4_LANES, 1, &records);
    let mut dst = vec![0u8; 64 * 8];
    decode(4, &src, &mut dst, &t).unwrap();
    // Truncated.
    assert!(decode(4, &src[..src.len() / 2], &mut dst, &t).is_err());
    // Headers promising more payload than the stream holds. (Other corrupt
    // headers decode to garbage: the format has no redundancy.)
    let headers = [[0x30; 8], [0xff; 8]].concat();
    assert!(decode(4, &headers, &mut dst[..8], &t).is_err());
    assert!(decode(4, &[], &mut dst, &t).is_err());
    // Destination not a whole number of blocks, empty, more blocks than coded.
    assert!(decode(4, &src, &mut dst[..12], &t).is_err());
    assert!(decode(4, &src, &mut [], &t).is_err());
    assert!(decode(4, &src, &mut vec![0u8; 65 * 8], &t).is_err());
    // Unsupported formats, bad tables.
    assert!(decode(2, &src, &mut dst, &t).is_err());
    assert!(Tables::parse(&[0; 16]).is_err());
    let mut no_tiling = tables();
    no_tiling[0] = 1;
    assert!(Tables::parse(&no_tiling).is_err());
}

#[test]
fn noise_never_panics() {
    let t = Tables::parse(&tables()).unwrap();
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for round in 0..300 {
        let len = round * 7 % 900;
        let src: Vec<u8> = (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        for bc in [1, 3, 4, 5, 6, 7] {
            let mut dst = vec![0u8; 16 * (1 + round % 40)];
            let _ = decode(bc, &src, &mut dst, &t);
        }
    }
}

/// The window table finds the bucket a scan of the starts finds, for
/// every slot of every model, in the shipped tables and in ours (whose
/// buckets are wide) and in tables whose buckets are one slot wide.
#[test]
fn bucket_lookup_matches_a_scan_of_the_starts() {
    let mut narrow = tables();
    for m in 0..MODELS {
        // Buckets 1.. of one slot each where they fit; bucket 0 takes the rest.
        let used = m / 16 + 2;
        let rest = (1u32 << 16) - (1..used).map(|b| 1u32 << (b - 1)).sum::<u32>();
        for b in 0..used {
            let f = if b == 0 { rest } else { 1 };
            narrow[(m * BUCKETS + b) * 2..][..2].copy_from_slice(&(f as u16).to_le_bytes());
        }
    }
    for t in [
        Tables::builtin().unwrap(),
        Tables::parse(&tables()).unwrap(),
        Tables::parse(&narrow).unwrap(),
    ] {
        for model in 0..MODELS as u8 {
            let start = &t.start[usize::from(model)];
            for slot in 0..1u32 << 16 {
                let scan = start[1..].iter().filter(|&&s| s <= slot).count();
                assert_eq!(
                    bucket_of(&t, model, slot),
                    scan,
                    "model {model} slot {slot}"
                );
            }
        }
    }
}

/// One rANS decode as first written: scan for the bucket, divide for the
/// symbol, rebuild its frequency and start from the symbol.
fn reference_decode(t: &Tables, model: u8, x: u32) -> (u8, u32) {
    let (freq, start) = (&t.freq[usize::from(model)], &t.start[usize::from(model)]);
    let slot = x & 0xffff;
    let b = start[1..BUCKETS].iter().filter(|&&s| s <= slot).count();
    let sym = match b {
        0 => 0,
        1 => 1,
        _ => (1u32 << (b - 1)) + (slot - start[b]) / freq[b],
    } as u8;
    let (f, c) = if sym == 0 {
        (freq[0] - 1, 1)
    } else {
        let k = (8 - sym.leading_zeros()) as usize;
        let f = freq[k];
        (
            f,
            ((u32::from(sym) - (1 << (k - 1))) * f + start[k]) & 0xffff,
        )
    };
    (
        sym,
        (x >> 16).wrapping_mul(f).wrapping_sub(c).wrapping_add(slot),
    )
}

/// A payload step decodes the symbol and state the first version did, for
/// every model and slot of the shipped tables and ours, at assorted states.
#[test]
fn payload_step_matches_the_reference_decode() {
    for t in [
        Tables::builtin().unwrap(),
        Tables::parse(&tables()).unwrap(),
    ] {
        for model in 0..MODELS as u8 {
            for high in [0x100u32, 0x1234, 0xffff] {
                for slot in 0..1u32 << 16 {
                    let x = high << 16 | slot;
                    if x <= LOW {
                        continue;
                    }
                    let mut lane = Lane {
                        phase: 3,
                        model,
                        x,
                        left: 1,
                        ..Default::default()
                    };
                    lane.step(&t).unwrap();
                    let (sym, next) = reference_decode(&t, model, x);
                    assert_eq!(
                        (lane.out, lane.x),
                        (Some(sym), next),
                        "model {model} x {x:#x}"
                    );
                }
            }
        }
    }
}

/// Pseudo-random records that fit each lane's alphabet; BC7 keeps control
/// code 0 (mode 6), the only one the synthetic tables define.
fn random_records(widths: &[u8], per: usize, blocks: usize, bc7: bool) -> Vec<Vec<u8>> {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    (0..blocks)
        .map(|_| {
            let mut r: Vec<u8> = (0..widths.len() * per)
                .map(|k| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let alphabet = 1u32 << (widths[k / per] + 1);
                    (seed as u32 % alphabet) as u8
                })
                .collect();
            if bc7 {
                r[30] = 0;
                r[31] = 0;
            }
            r
        })
        .collect()
}

/// The fast paths decode exactly what one lane step at a time does, on
/// every format's lane layout, with short and long fragments.
#[test]
fn every_layout_round_trips_through_the_lanes() {
    let t = Tables::parse(&tables()).unwrap();
    let layouts: [(&[u8], usize); 6] = [
        (&[7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7], 1),
        (&[3, 3, 7, 5, 5, 5, 5, 4, 5, 4, 7, 7], 2),
        (&BC4_LANES, 1),
        (&[3, 3, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5], 2),
        (&[3, 3, 7, 7, 7, 4, 4, 4, 5, 5, 5, 7, 7, 7, 7, 4], 2),
        (&[3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4], 2),
    ];
    for (widths, per) in layouts {
        for blocks in [1, 2, 17, 300, 1000] {
            let records = random_records(widths, per, blocks, false);
            let src = stream(&t, widths, per, &records);
            let mut got = vec![0u8; records.concat().len()];
            let mut next = src.iter().copied();
            lanes(&t, widths, per, blocks, |_| next.next(), &mut got).unwrap();
            assert_eq!(got, records.concat(), "{widths:?} x{per}, {blocks} blocks");
        }
    }
}

/// Decode throughput on synthetic streams (no game data):
/// `cargo test --release -p oathkeep-formats bcpack_throughput -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn bcpack_throughput() {
    let t = Tables::parse(&tables()).unwrap();
    let cases: [(u8, &[u8], usize, usize); 3] = [
        (1, &[7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7], 1, 8),
        (4, &BC4_LANES, 1, 8),
        (7, &[3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4], 2, 16),
    ];
    for (bc, widths, per, block) in cases {
        // One fragment per lane holds at most 65792 bytes.
        let blocks = 1 << 14;
        let records = random_records(widths, per, blocks, bc == 7);
        let src = stream(&t, widths, per, &records);
        let mut dst = vec![0u8; blocks * block];
        decode(bc, &src, &mut dst, &t).unwrap();
        let runs = 40;
        let started = std::time::Instant::now();
        for _ in 0..runs {
            decode(bc, std::hint::black_box(&src), &mut dst, &t).unwrap();
        }
        let secs = started.elapsed().as_secs_f64() / f64::from(runs);
        println!(
            "BC{bc}: {blocks} blocks, {:.1} MB out/s, {:.1} ns/block",
            (blocks * block) as f64 / secs / 1e6,
            secs * 1e9 / blocks as f64
        );
    }
}

/// Compares with reference outputs made outside the repository:
/// `OATHKEEP_BCPACK_TABLES` (the table file) and `OATHKEEP_BCPACK_CASES`, a
/// directory of `<name>.bc<n>` packed chunks with `<name>.out` beside them.
#[test]
#[ignore = "needs the format tables and reference chunks"]
fn reference_chunks() {
    let tables = std::env::var_os("OATHKEEP_BCPACK_TABLES").expect("OATHKEEP_BCPACK_TABLES");
    let cases = std::env::var_os("OATHKEEP_BCPACK_CASES").expect("OATHKEEP_BCPACK_CASES");
    let t = Tables::parse(&std::fs::read(tables).unwrap()).unwrap();
    let mut n = 0;
    for entry in std::fs::read_dir(cases).unwrap() {
        let path = entry.unwrap().path();
        let Some(bc) = path
            .extension()
            .and_then(|e| e.to_str()?.strip_prefix("bc")?.parse().ok())
        else {
            continue;
        };
        let want = std::fs::read(path.with_extension("out")).unwrap();
        let mut got = vec![0u8; want.len()];
        decode(bc, &std::fs::read(&path).unwrap(), &mut got, &t).unwrap();
        assert!(got == want, "{}", path.display());
        n += 1;
    }
    assert!(n > 0);
}

/// A dictionary entry stands in for a block the decoder made: a set with
/// 0 index bits holds no indices, and pixel 0 is the anchor (top bit clear).
#[test]
fn bc7_dictionary_indices_line_up() {
    let t = Tables::builtin().unwrap();
    for (k, (_, ix)) in t.bc7.iter().enumerate() {
        let anchor = 0..1 << (ix.bits[0] - 1);
        let ok = (ix.bits[1] != 0 || ix.ix[1] == [0; 16]) && anchor.contains(&ix.ix[0][0]);
        assert!(ok, "entry {k}: {:?}", ix.ix);
    }
}
