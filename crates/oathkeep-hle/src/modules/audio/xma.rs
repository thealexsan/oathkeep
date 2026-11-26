// The XMA decoder.
//
// An XMA context (16 words in the shared context array) names up to two
// input buffers of 2 KiB packets, a ring of 256-byte output blocks the
// hardware fills with 16-bit PCM, and the read offset the title advances
// by command. Words and bit fields, as titles write them:
// 0: input 0 size in packets (0-11), loop count (12-19, 255 forever),
//    valid inputs (20-21), output size in blocks (22-26), write block
//    (27-31, hardware);
// 1: input 1 size (0-11), loop end subframe (12-13), loop start subframes
//    to skip (17-19), subframes to skip at the start (24-26), sample rate
//    (27-28: 24, 32, 44.1, 48 kHz), stereo (29), output writable (31);
// 2: read position in bits (0-25), error status (26-30), error (31);
// 3: loop start in bits (0-25); 4: loop end in bits (0-25), current input
//    (31); 5, 6: input addresses; 7: output address; 8: overlap buffer;
//    9: read block (0-4).
// Titles read back only the write block, "writable" and the read block,
// and take whatever lies between read and write.
// shortcut: each enabled context decodes from a cold state and the overlap
// buffer is ignored (titles copy it between consecutive contexts of one
// voice); keep decoder state there if a title's seams turn out audible.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock, RwLock, RwLockReadGuard};
use std::time::Instant;

use oathkeep_formats::xma;

use super::heap::heap_bytes;
use super::lock;

pub(super) const ENABLE_XMA: u32 = 5;
const ENABLE_XMAS: u32 = 6;
pub(super) const DISABLE_XMA: u32 = 7;
const DISABLE_XMAS: u32 = 8;
pub(super) const ADVANCE_XMA_READ: u32 = 18;

const BLOCK_BYTES: usize = 256;
pub(super) const SAMPLES_PER_BLOCK: usize = BLOCK_BYTES / 2;
/// Error status: the input ran out.
pub(super) const INPUT_INVALID: u32 = 4;

pub(super) struct Voice {
    pub(super) decoder: Option<xma::Decoder>,
    pub(super) pcm: VecDeque<i16>,
    /// Samples per channel still to drop (start or loop subframe skips).
    pub(super) skip: usize,
    pub(super) ended: bool,
    pub(super) trace: Trace,
}

/// What a voice did, for the debug log when it is disabled.
pub(super) struct Trace {
    pub(super) since: Instant,
    /// Samples per channel written to the ring.
    pub(super) produced: usize,
    /// Blocks the title took (command 18), how often, and how often it
    /// found the ring less than full.
    pub(super) taken: usize,
    pub(super) takes: u32,
    pub(super) partial_takes: u32,
    pub(super) frames: u32,
    pub(super) bad_frames: u32,
    /// The loudest sample written.
    pub(super) peak: i16,
    /// Milliseconds after enabling when the input ran out.
    pub(super) ended_ms: Option<u128>,
}

pub(super) struct Xma {
    /// Host address of the context array.
    pub(super) array: usize,
    /// One entry per context, `Some` while enabled. Each has its own lock,
    /// so a command for one voice never waits for others to decode (the
    /// title sends them from its time-critical mixer thread).
    pub(super) voices: Vec<Mutex<Option<Voice>>>,
}

impl Xma {
    pub(super) const EMPTY: Xma = Xma {
        array: 0,
        voices: Vec::new(),
    };
}

/// Written only when the context arrays are allocated or released.
pub(super) static XMA: RwLock<Xma> = RwLock::new(Xma::EMPTY);

pub(super) fn xma() -> RwLockReadGuard<'static, Xma> {
    XMA.read().unwrap_or_else(|e| e.into_inner())
}

/// A context's words in title memory.
#[derive(Clone, Copy)]
pub(super) struct Context(pub(super) *mut u32);

impl Context {
    pub(super) fn get(self, word: usize) -> u32 {
        // SAFETY: inside the context array; the title reads and writes it
        // concurrently, like the hardware's.
        unsafe { self.0.add(word).read_volatile() }
    }

    pub(super) fn set(self, word: usize, value: u32) {
        // SAFETY: as above.
        unsafe { self.0.add(word).write_volatile(value) }
    }

    pub(super) fn field(self, word: usize, shift: u32, bits: u32) -> u32 {
        (self.get(word) >> shift) & ((1 << bits) - 1)
    }

    pub(super) fn set_field(self, word: usize, shift: u32, bits: u32, value: u32) {
        let mask = ((1u32 << bits) - 1) << shift;
        self.set(word, (self.get(word) & !mask) | ((value << shift) & mask));
    }

    fn channels(self) -> usize {
        1 + self.field(1, 29, 1) as usize
    }
}

impl Xma {
    fn context(&self, index: u32) -> Option<Context> {
        ((index as usize) < self.voices.len() && self.array != 0)
            .then(|| Context((self.array + index as usize * 64) as *mut u32))
    }

    fn enable(&self, index: u32) {
        let Some(context) = self.context(index) else {
            return;
        };
        let mut slot = lock(&self.voices[index as usize]);
        context.set_field(2, 26, 6, 0);
        tracing::debug!(
            index,
            words = ?(0..10).map(|w| format!("{:08x}", context.get(w))).collect::<Vec<_>>(),
            "XMA voice enabled"
        );
        #[cfg(feature = "diagnostics")]
        dump_input(index, context);
        *slot = Some(Voice {
            decoder: None,
            pcm: VecDeque::new(),
            skip: context.field(1, 24, 3) as usize * xma::SUBFRAME_SAMPLES,
            ended: false,
            trace: Trace {
                since: Instant::now(),
                produced: 0,
                taken: 0,
                takes: 0,
                partial_takes: 0,
                frames: 0,
                bad_frames: 0,
                peak: 0,
                ended_ms: None,
            },
        });
        drop(slot);
        wake_decoder();
    }

    fn disable(&self, index: u32) {
        let Some(context) = self.context(index) else {
            return;
        };
        let Some(voice) = lock(&self.voices[index as usize]).take() else {
            return;
        };
        let t = &voice.trace;
        tracing::debug!(
            index,
            ms = t.since.elapsed().as_millis(),
            produced = t.produced,
            taken = t.taken * SAMPLES_PER_BLOCK / context.channels(),
            takes = t.takes,
            partial_takes = t.partial_takes,
            frames = t.frames,
            bad_frames = t.bad_frames,
            peak = t.peak,
            input_ended_ms = ?t.ended_ms,
            read_bit = context.field(2, 0, 26),
            input_bits = context.field(0, 0, 12) * xma::PACKET_BYTES as u32 * 8,
            "XMA voice disabled"
        );
    }

    /// Moves the title's read block on, which frees output space.
    fn advance_read(&self, index: u32, blocks: u32) {
        let Some(context) = self.context(index) else {
            return;
        };
        // The decoder writes the same words.
        let mut slot = lock(&self.voices[index as usize]);
        let size = context.field(0, 22, 5).max(1);
        // The count is the title's: reduced first, so the sum stays small.
        let read = (context.field(9, 0, 5) + blocks % size) % size;
        context.set_field(9, 0, 5, read);
        if blocks > 0 {
            context.set_field(1, 31, 1, 1);
        }
        if let Some(voice) = &mut *slot {
            let t = &mut voice.trace;
            t.taken += blocks as usize;
            t.takes += 1;
            t.partial_takes += u32::from(blocks < size);
        }
        drop(slot);
        wake_decoder();
    }

    /// Fills every enabled context's output as far as it has room.
    pub(super) fn pump(&self) {
        let start = Instant::now();
        let mut active = 0;
        for index in 0..self.voices.len() as u32 {
            if let (Some(context), Some(voice)) =
                (self.context(index), &mut *lock(&self.voices[index as usize]))
            {
                pump(context, voice);
                active += 1;
            }
        }
        let took = start.elapsed();
        if took.as_millis() >= 4 {
            tracing::debug!(active, ms = took.as_millis(), "slow XMA decoding pass");
        }
    }
}

fn pump(context: Context, voice: &mut Voice) {
    let size = context.field(0, 22, 5);
    if size == 0 {
        return;
    }
    while context.field(1, 31, 1) == 1 {
        // A skipped frame or an input boundary can advance the decoder without
        // producing samples. Publish only complete blocks, except the final
        // padded tail. Limit empty steps so a broken looping voice cannot hold
        // its lock or stall other voices for the whole decoding pass.
        for _ in 0..8 {
            if voice.pcm.len() >= SAMPLES_PER_BLOCK || !decode(context, voice) {
                break;
            }
        }
        if voice.pcm.is_empty() || (voice.pcm.len() < SAMPLES_PER_BLOCK && !voice.ended) {
            return;
        }
        // The write block is the title's too: taken modulo the ring before
        // the first write, which would otherwise land past the ring's end.
        let write = context.field(0, 27, 5) % size;
        let address = context.get(7) as usize + write as usize * BLOCK_BYTES;
        let Some(out) = heap_bytes(address as u32, BLOCK_BYTES) else {
            return;
        };
        for pair in out.as_chunks_mut::<2>().0 {
            let sample = voice.pcm.pop_front().unwrap_or(0);
            voice.trace.peak = voice.trace.peak.max(sample.saturating_abs());
            pair.copy_from_slice(&sample.to_le_bytes());
        }
        voice.trace.produced += SAMPLES_PER_BLOCK / context.channels();
        let write = (write + 1) % size;
        context.set_field(0, 27, 5, write);
        // The read block is the title's: taken modulo the ring like the
        // write block and `advance_read`'s. Compared as it is, one at or
        // past the end would never be met, and a stream that loops would
        // keep this voice (and its lock) filling forever.
        if write == context.field(9, 0, 5) % size {
            // Full: nothing more until the title advances its read block.
            context.set_field(1, 31, 1, 0);
        }
    }
}

/// Decodes the next frame of the current input into `voice.pcm`; false
/// when there is nothing left to decode.
pub(super) fn decode(context: Context, voice: &mut Voice) -> bool {
    let valid = context.field(0, 20, 2);
    if voice.ended {
        // The title can refill either input while the context stays enabled.
        // Flush the previous tail once and leave the ring idle until a valid
        // buffer is published. The decoder resets its overlap when flushed.
        if valid == 0 {
            return false;
        }
        voice.ended = false;
        if context.field(2, 26, 5) == INPUT_INVALID {
            context.set_field(2, 26, 6, 0);
        }
    }
    let channels = context.channels();
    let mut current = context.field(4, 31, 1);
    if valid & (1 << current) == 0 {
        if valid & (1 << (1 - current)) == 0 {
            // Out of input: the overlap tail, then nothing.
            if let Some(decoder) = &mut voice.decoder {
                let mut tail = Vec::new();
                decoder.flush(&mut tail);
                voice.pcm.extend(tail);
            }
            voice.ended = true;
            voice.trace.ended_ms = Some(voice.trace.since.elapsed().as_millis());
            context.set_field(2, 26, 5, INPUT_INVALID);
            context.set_field(2, 31, 1, 1);
            return !voice.pcm.is_empty();
        }
        current = 1 - current;
        context.set_field(4, 31, 1, current);
        context.set_field(2, 0, 26, 0);
    }
    let (address, packets) = match current {
        0 => (context.get(5), context.field(0, 0, 12)),
        _ => (context.get(6), context.field(1, 0, 12)),
    };
    let Some(data) = heap_bytes(address & !0x7ff, packets as usize * xma::PACKET_BYTES) else {
        context.set_field(0, 20 + current, 1, 0);
        return true;
    };
    let mut at = context.field(2, 0, 26) as usize;
    if at < 32 {
        at = 32 + xma::packet(data, 0).map_or(0, |p| p.first_frame_bit as usize);
    }
    let Some(frame) = xma::read_frame(data, at) else {
        // This input is used up.
        context.set_field(0, 20 + current, 1, 0);
        return true;
    };
    let decoder = voice.decoder.get_or_insert_with(|| {
        const RATES: [u32; 4] = [24_000, 32_000, 44_100, 48_000];
        xma::Decoder::new(channels, RATES[context.field(1, 27, 2) as usize])
    });
    let mut pcm = Vec::with_capacity(xma::FRAME_SAMPLES * channels);
    voice.trace.frames += 1;
    if let Err(e) = decoder.decode(&frame.data, frame.bits, &mut pcm) {
        voice.trace.bad_frames += 1;
        if voice.trace.bad_frames <= 2 {
            tracing::debug!(
                at,
                bits = frame.bits,
                head = ?&frame.data[..frame.data.len().min(8)],
                %e,
                "XMA frame did not decode"
            );
        }
    }
    let mut next = frame.next.unwrap_or(0);
    let loops = context.field(0, 12, 8);
    let loop_end = context.field(4, 0, 26) as usize;
    let ends_loop = loop_end != 0
        && (frame.start == loop_end
            || (frame.start < loop_end && frame.next.is_none_or(|n| loop_end < n)));
    let looping = loops > 0 && ends_loop;
    if looping {
        let keep = (context.field(1, 12, 2) as usize + 1) * xma::SUBFRAME_SAMPLES * channels;
        pcm.truncate(keep);
        next = context.field(3, 0, 26) as usize;
        if loops != 255 {
            context.set_field(0, 12, 8, loops - 1);
        }
    }
    // What is still to skip is this frame's (the stream's first, or a
    // loop's first when it is its last too).
    let skip = (voice.skip * channels).min(pcm.len());
    voice.skip -= skip / channels;
    voice.pcm.extend(&pcm[skip..]);
    if looping {
        // The loop's own skip is of its first frame, decoded next: not of
        // this one, its last.
        voice.skip = context.field(1, 17, 3) as usize * xma::SUBFRAME_SAMPLES;
    }
    if next == 0 {
        context.set_field(0, 20 + current, 1, 0);
    }
    context.set_field(2, 0, 26, next as u32);
    true
}

/// `OATHKEEP_XMA_DUMP=<dir>`: saves the context words (`<n>-<index>.ctx`)
/// and input buffer 0 (`.xma`) of the first 32 voices enabled, to test
/// the decoder on a title's streams outside the title.
#[cfg(feature = "diagnostics")]
fn dump_input(index: u32, context: Context) {
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let Some(dir) = std::env::var_os("OATHKEEP_XMA_DUMP") else {
        return;
    };
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n >= 32 {
        return;
    }
    let dir = std::path::PathBuf::from(dir);
    let words: Vec<u8> = (0..16).flat_map(|w| context.get(w).to_le_bytes()).collect();
    let input = heap_bytes(
        context.get(5) & !0x7ff,
        context.field(0, 0, 12) as usize * xma::PACKET_BYTES,
    );
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{n:02}-{index}.ctx")), words);
    let _ = std::fs::write(dir.join(format!("{n:02}-{index}.xma")), input.unwrap_or_default());
}

/// Starts the decoding thread on the first enabled voice, and has it fill
/// rings now: a voice just enabled or read from.
fn wake_decoder() {
    static DECODER: OnceLock<std::thread::Thread> = OnceLock::new();
    DECODER
        .get_or_init(|| {
            std::thread::Builder::new()
                .name("victory XMA decoder".into())
                .spawn(|| {
                    loop {
                        xma().pump();
                        // A 256-byte output block lasts 2.7 ms at 48 kHz mono.
                        std::thread::park_timeout(std::time::Duration::from_millis(2));
                    }
                })
                .map_or_else(|_| std::thread::current(), |h| h.thread().clone())
        })
        .unpark();
}

/// Runs an XMA command; false for one this does not know.
pub(super) fn xma_command(command: u32, word: impl Fn(usize) -> Option<u32>) -> bool {
    let xma = xma();
    match command {
        ENABLE_XMA => xma.enable(word(0).unwrap_or(u32::MAX)),
        DISABLE_XMA => xma.disable(word(0).unwrap_or(u32::MAX)),
        ENABLE_XMAS | DISABLE_XMAS => {
            for i in 0..xma.voices.len() as u32 {
                if word(i as usize / 32).unwrap_or(0) & (1 << (i % 32)) != 0 {
                    match command {
                        ENABLE_XMAS => xma.enable(i),
                        _ => xma.disable(i),
                    }
                }
            }
        }
        ADVANCE_XMA_READ => xma.advance_read(word(0).unwrap_or(u32::MAX), word(1).unwrap_or(0)),
        _ => return false,
    }
    true
}
