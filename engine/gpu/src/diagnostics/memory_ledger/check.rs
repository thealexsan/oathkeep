//! Descriptor decoding, periodic slot checking, and device-loss reporting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Entry, ROOTS, checks, on, with};

/// What a descriptor's 32 bytes name.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Names {
    Nothing,
    Texture { base: u64, meta: Option<u64>, kind: u32, width: u32, height: u32 },
    Buffer { base: u64, stride: u32, bytes: u64 },
}

/// The descriptor `bytes` hold, as AMD's cards have them since RDNA 2.
pub(crate) fn decode(bytes: &[u8; 32]) -> Names {
    let w = |i: usize| u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]);
    let kind = w(3) >> 28;
    if kind >= 8 {
        let base = (u64::from(w(0)) | u64::from(w(1) & 0xff) << 32) << 8;
        if base == 0 {
            return Names::Nothing;
        }
        // Compressed (dword 6, bit 21): where the metadata lies.
        let meta = (w(6) >> 21 & 1 != 0 && w(7) != 0).then(|| u64::from(w(7)) << 16 | u64::from(w(6) >> 24) << 8);
        let width = ((w(1) >> 30) | (w(2) & 0xfff) << 2) + 1;
        let height = (w(2) >> 14 & 0x3fff) + 1;
        return Names::Texture { base, meta, kind, width, height };
    }
    let base = u64::from(w(0)) | u64::from(w(1) & 0xffff) << 32;
    let stride = w(1) >> 16 & 0x3fff;
    let bytes = u64::from(w(2)) * u64::from(stride.max(1));
    if base == 0 || bytes == 0 {
        Names::Nothing
    } else {
        Names::Buffer { base, stride, bytes }
    }
}

/// What the book says of an address: alive, gone or never there.
fn fate(at: u64) -> Fate {
    with(|b| {
        if b.alive(at) {
            return Fate::Alive;
        }
        match b.gone_at(at, 1).first() {
            Some((e, gone)) => Fate::Gone(format!(
                "{} at {:#x}+{:#x} made at {:.1} s, gone at {:.1} s",
                e.what,
                e.start,
                e.size,
                e.made as f64 / 1e3,
                *gone as f64 / 1e3
            )),
            None => Fate::Never,
        }
    })
}

enum Fate {
    Alive,
    Gone(String),
    Never,
}

/// The addresses a descriptor reads first.
fn bases(names: Names) -> Vec<(&'static str, u64)> {
    match names {
        Names::Nothing => Vec::new(),
        Names::Texture { base, meta, .. } => {
            let mut out = vec![("texture", base)];
            out.extend(meta.map(|m| ("metadata", m)));
            out
        }
        Names::Buffer { base, bytes, .. } => vec![("buffer", base), ("buffer's end", base + bytes - 1)],
    }
}

/// Starts the thread of `OATHKEEP_SLOT_CHECK=1`, once.
pub(crate) fn check() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if !checks() || !on() {
        return;
    }
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new().name("slot check".into()).spawn(walk);
    });
}

/// Slots found, since the start: looked at, naming nothing, textures,
/// buffers, naming what is gone, naming what was never there.
static FOUND: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

fn walk() {
    use Ordering::Relaxed;
    const CHUNK: u32 = 4096;
    let mut at = 0u32;
    // Slots said already, by slot and address.
    let mut said: HashMap<(u32, u64), u64> = HashMap::new();
    let mut suspects: Vec<(u32, [u8; 32], u64)> = Vec::new();
    let mut round = std::time::Instant::now();
    let mut never_said = 0;
    let mut gone_said = 0;
    loop {
        let read = crate::descriptors::host_heap::shown_bytes(at, CHUNK);
        let Some((first, bytes, end)) = read else {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        };
        for (i, d) in bytes.chunks_exact(32).enumerate() {
            let d: [u8; 32] = d.try_into().unwrap_or([0; 32]);
            let names = decode(&d);
            FOUND[0].fetch_add(1, Relaxed);
            FOUND[match names {
                Names::Nothing => 1,
                Names::Texture { .. } => 2,
                Names::Buffer { .. } => 3,
            }]
            .fetch_add(1, Relaxed);
            if bases(names).iter().any(|&(_, base)| !matches!(fate(base), Fate::Alive)) {
                suspects.push((first + i as u32, d, crate::backend::now_ms()));
            }
        }
        // Again a moment later: a slot between two states says nothing.
        let now = crate::backend::now_ms();
        let due: Vec<(u32, [u8; 32], u64)> = {
            let (due, later) = suspects.into_iter().partition(|s| now >= s.2 + 400);
            suspects = later;
            due
        };
        for (slot, was, since) in due {
            let Some((_, again, _)) = crate::descriptors::host_heap::shown_bytes(slot, 1) else { continue };
            if again != was {
                continue;
            }
            for (part, base) in bases(decode(&was)) {
                let (what, text) = match fate(base) {
                    Fate::Alive => continue,
                    Fate::Gone(text) => (4, text),
                    Fate::Never => (5, "nothing the book ever had".to_owned()),
                };
                FOUND[what].fetch_add(1, Relaxed);
                let times = said.entry((slot, base)).or_insert(0);
                *times += 1;
                let quota = if what == 4 { &mut gone_said } else { &mut never_said };
                if *times > 1 || *quota >= if what == 4 { 400 } else { 60 } {
                    continue;
                }
                *quota += 1;
                tracing::warn!(
                    "slot check: view slot {slot} names {part} {base:#x} ({:?}), {text}; seen since {:.1} s; {}",
                    decode(&was),
                    since as f64 / 1e3,
                    crate::descriptors::host_heap::slot_story(slot)
                );
            }
        }
        at = first + (bytes.len() / 32) as u32;
        if at >= end {
            at = 0;
            let f = [0, 1, 2, 3, 4, 5].map(|i| FOUND[i].swap(0, Relaxed));
            let (live, gone, unknown) = with(|b| (b.live.len(), b.gone.len(), b.unknown.clone()));
            tracing::info!(
                "slot check: a round of the view heap in {:.1} s: {} slots, {} null, {} textures, {} buffers; {} name what is gone, {} what the book never had; the book has {live} things alive and {gone} gone; textures without an address {unknown:?}",
                round.elapsed().as_secs_f64(),
                f[0], f[1], f[2], f[3], f[4], f[5]
            );
            round = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// For the report of a lost device: who has and had the faulting address,
/// and what names it.
pub(crate) fn at_loss(va: u64) -> String {
    if !on() {
        return String::new();
    }
    let mut text = with(|b| {
        let one =
            |e: &Entry| format!("{} {:#x}+{:#x} made at {:.3} s{}", e.what, e.start, e.size, e.made as f64 / 1e3, e.notes);
        let mut text = String::from("; BOOK: alive there:");
        for e in b.holders(va) {
            text.push_str(&format!(" [{}]", one(e)));
        }
        for (name, e) in ["below", "above"].iter().zip(b.neighbours(va)) {
            if let Some(e) = e {
                text.push_str(&format!(" {name} [{}, {:#x} away]", one(e), e.start.abs_diff(va)));
            }
        }
        text.push_str("; gone there, newest first:");
        for (e, gone) in b.gone_at(va, 12) {
            text.push_str(&format!(" [{} gone at {:.3} s]", one(e), *gone as f64 / 1e3));
        }
        text.push_str(&format!(
            "; now {:.3} s, submissions (begun, finished) {:?}, {} alive, {} gone",
            crate::backend::now_ms() as f64 / 1e3,
            crate::backend::submissions(),
            b.live.len(),
            b.gone.len()
        ));
        text
    });
    {
        let roots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
        let mut near: Vec<(u64, u64, u64)> = roots.iter().filter(|r| r.1 <= va && va - r.1 < 4 << 30).copied().collect();
        near.sort_by_key(|r| va - r.1);
        near.dedup_by_key(|r| r.1);
        for (at, gpu, bytes) in near.into_iter().take(4) {
            text.push_str(&format!("; root descriptor {gpu:#x}+{bytes:#x} ({:#x} below) at {:.1} s", va - gpu, at as f64 / 1e3));
        }
    }
    // Every slot that reaches the address, and the nearest below it.
    let mut reach: Vec<(u32, Names)> = Vec::new();
    let mut near: Vec<(u64, u32, Names)> = Vec::new();
    let mut at = 0;
    let began = std::time::Instant::now();
    while let Some((first, bytes, end)) = crate::descriptors::host_heap::shown_bytes(at, 16_384) {
        for (i, d) in bytes.chunks_exact(32).enumerate() {
            let slot = first + i as u32;
            let names = decode(&d.try_into().unwrap_or([0; 32]));
            let (base, len) = match names {
                Names::Nothing => continue,
                Names::Texture { base, meta, .. } => {
                    if meta.is_some_and(|m| va >= m && va - m < 32 << 20) {
                        reach.push((slot, names));
                    }
                    let len = with(|b| {
                        b.holders(base)
                            .iter()
                            .map(|e| e.start + e.size - base)
                            .min()
                            .or_else(|| b.gone_at(base, 1).first().map(|(e, _)| e.start + e.size - base))
                    });
                    (base, len.unwrap_or(64 << 20) + (64 << 10))
                }
                Names::Buffer { base, bytes, .. } => (base, bytes),
            };
            if va >= base && va - base < len {
                reach.push((slot, names));
            } else if va >= base && va - base < 4 << 30 {
                near.push((va - base, slot, names));
                if near.len() > 4096 {
                    near.sort_by_key(|n| n.0);
                    near.truncate(16);
                }
            }
        }
        at = first + (bytes.len() / 32) as u32;
        if at >= end || began.elapsed().as_secs() > 20 {
            break;
        }
    }
    text.push_str(&format!("; {} slots reach it (heap read in {:.1} s)", reach.len(), began.elapsed().as_secs_f64()));
    for (slot, names) in reach.iter().take(24) {
        let (Names::Texture { base, .. } | Names::Buffer { base, .. }) = *names else { continue };
        let fate = match fate(base) {
            Fate::Alive => "alive".to_owned(),
            Fate::Gone(text) => format!("GONE: {text}"),
            Fate::Never => "never in the book".to_owned(),
        };
        text.push_str(&format!("; slot {slot} {names:?} {fate} {}", crate::descriptors::host_heap::slot_story(*slot)));
    }
    near.sort_by_key(|n| n.0);
    for (away, slot, names) in near.iter().take(8) {
        text.push_str(&format!("; near: slot {slot} {names:?} {away:#x} below {}", crate::descriptors::host_heap::slot_story(*slot)));
    }
    text
}
