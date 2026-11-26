use super::{Error, HISTORY, LOW, ROUND_BYTES, Tables};

// Lanes.

#[derive(Clone, Copy, Default)]
pub(super) struct Lane {
    /// Symbol bits - 1 at the full alphabet.
    pub(super) width: u8,
    /// 0 header, 1 length (low byte), 2 length high byte, 3 payload.
    pub(super) phase: u8,
    pub(super) long: bool,
    pub(super) reset: bool,
    pub(super) model: u8,
    pub(super) need: bool,
    pub(super) latch: Option<u8>,
    pub(super) out: Option<u8>,
    pub(super) stalled: bool,
    pub(super) x: u32,
    pub(super) left: u32,
}

impl Lane {
    /// One step: take the latched byte (header, length or payload) and
    /// decode at most one symbol.
    pub(super) fn step(&mut self, t: &Tables) -> Result<(), Error> {
        if self.phase != 3 {
            let Some(b) = self.latch.take() else {
                return Ok(());
            };
            self.need = true;
            match self.phase {
                0 => {
                    // The top two bits shrink the alphabet; width >= 3.
                    let group = self.width - (b >> 6);
                    self.model = group * 16 + (b & 15);
                    self.reset = b & 0x10 != 0;
                    self.long = b & 0x20 != 0;
                    self.left = 0;
                    self.phase = 1;
                }
                1 if self.long => {
                    self.left = b.into();
                    self.phase = 2;
                }
                1 => {
                    self.left = u32::from(b) + 1;
                    self.phase = 3;
                }
                _ => {
                    self.left = (u32::from(b) << 8 | self.left) + 257;
                    self.phase = 3;
                }
            }
            return Ok(());
        }
        let mut x = self.x;
        if x < LOW
            && let Some(b) = self.latch.take()
        {
            self.left = self.left.wrapping_sub(1);
            x = x << 8 | u32::from(b);
        }
        self.need = self.left != 0 && self.latch.is_none();
        if x > LOW {
            let freq = &t.freq[usize::from(self.model)];
            let start = &t.start[usize::from(self.model)];
            let slot = x & 0xffff;
            // The bucket: starts are ascending, and 65536 past the model's last.
            let b = bucket_of(t, self.model, slot);
            // Bucket 0 holds symbol 0, which owns slots 1 .. f[0] - 1 (slot 0
            // is never used). Bucket b > 0 holds 2^(b-1) symbols of frequency
            // f[b] from start[b] on; offset and frequency are below 2^16, where
            // the reciprocal divides exactly.
            let (sym, f, rem) = if b == 0 {
                (0, freq[0] - 1, slot.wrapping_sub(1))
            } else {
                let offset = slot - start[b];
                let q = ((u64::from(offset) * t.recip[usize::from(self.model)][b]) >> 32) as u32;
                ((1u32 << (b - 1)) + q, freq[b], offset - q * freq[b])
            };
            self.x = (x >> 16).wrapping_mul(f).wrapping_add(rem);
            self.out = Some(sym as u8);
        } else if self.left != 0 {
            // At exactly 2^24 the lane neither reads nor decodes again; only a
            // lane that has delivered its symbols may end up there.
            self.x = x;
        } else {
            // The fragment ends; a byte read in this step does not reach the carried state.
            self.phase = 0;
            self.need = self.latch.is_none();
            if self.reset {
                self.x = 0;
            }
        }
        Ok(())
    }
}

/// The bucket holding `slot` (below 65536): the last whose start is at or
/// below it. The window table gives the bucket of the window's first slot;
/// later buckets that start inside the window are stepped over.
#[inline]
pub(super) fn bucket_of(t: &Tables, model: u8, slot: u32) -> usize {
    let start = &t.start[usize::from(model)];
    let mut b = usize::from(t.bucket[usize::from(model)][(slot >> 8) as usize]);
    while start[b + 1] <= slot {
        b += 1;
    }
    b
}

/// Decodes `per` symbols per block on every lane into `records` (one record
/// of `widths.len() * per` symbols per block), taking input from `fetch`
/// (given the lane that takes the byte) in the order of the schedule: each
/// round the lanes step in order and take at most 8 bytes between them.
// shortcut: one lane step at a time (BC1 roughly 30-60 MB/s, BC7 15-20 MB/s
// on one core); if texture streaming needs more, run the schedule only to
// split the input per lane and decode the lanes apart.
pub(super) fn lanes(
    t: &Tables,
    widths: &[u8],
    per: usize,
    blocks: usize,
    mut fetch: impl FnMut(usize) -> Option<u8>,
    records: &mut [u8],
) -> Result<(), Error> {
    let mut lanes = [Lane::default(); 16];
    let lanes = &mut lanes[..widths.len()];
    for (lane, &width) in lanes.iter_mut().zip(widths) {
        lane.width = width;
        lane.need = true;
    }
    let stride = widths.len() * per;
    let full = (1u32 << widths.len()) - 1;
    let (mut pos, mut done, mut idle) = (0, 0u32, 0);
    // Where symbol `pos` of lane 0 goes; lane j's is `per * j` further on.
    let mut at = 0;
    while pos < blocks * per {
        let (mut taken, mut moved) = (0, false);
        for (j, lane) in lanes.iter_mut().enumerate() {
            let bit = 1 << j;
            let before = taken;
            if !lane.stalled {
                lane.step(t)?;
            }
            let waiting = lane.out.is_some() && done & bit != 0;
            if lane.need && !waiting && taken < ROUND_BYTES {
                // Past the end nothing arrives; in a valid stream only finished lanes ask there.
                if let Some(b) = fetch(j) {
                    lane.latch = Some(b);
                    taken += 1;
                }
            }
            lane.stalled = (lane.need && before >= ROUND_BYTES) || waiting;
            if !lane.stalled
                && let Some(sym) = lane.out.take()
            {
                records[at + j * per] = sym;
                done |= bit;
                moved = true;
            }
        }
        if done == full {
            pos += 1;
            at += if pos.is_multiple_of(per) {
                stride - per + 1
            } else {
                1
            };
            done = 0;
            moved = true;
        }
        // Nothing read and nothing delivered for a few rounds: no lane can go on.
        idle = if taken > 0 || moved { 0 } else { idle + 1 };
        if idle > 3 {
            return Err(Error("the stream ends early or is corrupt"));
        }
    }
    Ok(())
}

// Blocks.

/// The record's endpoint and index references.
pub(super) fn references(s: &[u8], paired: bool) -> (usize, usize) {
    let (d, ix) = if paired {
        (interleave(s[0], s[1]), interleave(s[2], s[3]))
    } else {
        (s[0], s[1])
    };
    let delta = (d >> 1) ^ (d & 1).wrapping_neg();
    (usize::from(ix.wrapping_add(delta)), usize::from(ix))
}

/// Two 4-bit symbols, the first on the even bits.
pub(super) fn interleave(a: u8, b: u8) -> u8 {
    let spread = |v: u8| (0..4).fold(0, |acc, k| acc | (v >> k & 1) << (2 * k));
    spread(a) | spread(b) << 1
}

/// Block `i`'s reference `r`: 0 none, else `r` blocks back, before the first
/// block into the dictionary.
pub(super) fn pick<'a, T>(i: usize, r: usize, history: &'a [T], dictionary: &'a [T], zero: &'a T) -> &'a T {
    match r {
        0 => zero,
        r if r <= i => &history[(i - r) % HISTORY],
        r => &dictionary[r - i - 1],
    }
}

/// Zigzag residual (0, -1, 1, -2, ...) modulo 2^bits.
pub(super) fn zz(v: u8, bits: u32) -> u8 {
    let mask = ((1u32 << bits) - 1) as u8;
    ((v >> 1) ^ if v & 1 != 0 { mask } else { 0 }) & mask
}
