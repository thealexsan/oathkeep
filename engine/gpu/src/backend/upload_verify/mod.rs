//! Diagnostics (`OATHKEEP_UPCOPY_CHECK=1`; off by default): whether what goes
//! up to a copy in the card's memory is older than what the card holds.
//!
//! What every send up rests on: outside ranges shaders may be writing, and
//! but for what the title wrote since, a copy's CPU side holds what its card
//! side holds. Two looks at that, both on the card:
//!
//! - **at each send**: a refresh tells what the CPU's side held before
//!   ([`note`]) and what it took. Ahead of the send, in its place on the
//!   queue, the card's words are compared with the former. A word the title
//!   did not change (both are equal) and the card holds another value of is
//!   one the send puts an older value over;
//! - **at rest**: every few seconds, when the flights were settled for the
//!   sweep and nothing is in the air, every copy is compared whole with its
//!   CPU side. A word that differs is one a later send would undo. The log
//!   line says which buffers, what stands on the card, on the CPU's side and
//!   in title memory, and whether the range was declared as written at all
//!   (if not: a shader writes through a view nobody declared) and fetched
//!   lately (if so: what came down was not what the card ended with).
//!
//! The looks cost memory and time (the bytes sent twice more, and each
//! copy read over the bus at rest): the check is not for runs that are timed.

use std::collections::{BTreeMap, HashMap};

use windows::Win32::Foundation::CloseHandle;

use super::*;
use crate::memory::shadow::Shadow;

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_UPCOPY_CHECK").is_some_and(|v| v == "1"))
}

/// One dispatch looks at up to a piece: `words` words of the card's buffer
/// from `at`, beside `was` (what the CPU's side held when both were known to
/// be alike) and `new` (what the title has there now). Where the two are
/// equal and the card's word is another one, it is counted, and the first
/// found is kept: its offset, the card's value, the CPU's.
const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=5, b0), SRV(t0), SRV(t1), UAV(u0)"
cbuffer Look : register(b0) { uint at; uint words; uint was; uint now; uint found_at; };
ByteAddressBuffer card : register(t0);
ByteAddressBuffer cpu : register(t1);
RWByteAddressBuffer found : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= words) return;
    uint k = id.x * 4u, n;
    uint c = card.Load(at + k), w = cpu.Load(was + k);
    if (c == w || cpu.Load(now + k) != w) return;
    found.InterlockedAdd(found_at, 1u, n);
    if (n == 0u) found.Store3(found_at + 4u, uint3(at + k, c, w));
}
"#;

/// Bytes of an answer.
const ANSWER: u64 = 16;
const PIECE: usize = 64 << 10;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

fn pass(host: &Host) -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let code = compile(SOURCE, windows::core::s!("main"), windows::core::s!("cs_5_1")).ok()?;
        // SAFETY: the code lives through the calls; the root signature is
        // inside it.
        unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).ok()?;
            let pso: ID3D12PipelineState = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .ok()?;
            Some(Pass { root, pso })
        }
    })
    .as_ref()
}

/// A piece a refresh took: its offset in the copy, what the CPU's side held
/// and what it holds now.
pub(crate) struct Taken {
    at: usize,
    was: Vec<u8>,
    new: Vec<u8>,
}

/// Refreshes since the last send, per copy (by its number).
static NOTED: Mutex<BTreeMap<u64, Vec<Taken>>> = Mutex::new(BTreeMap::new());

/// At most this much is looked at per submission.
const MOST: usize = 128 << 20;

/// Offsets `[at, at + was.len())` of `copy`'s CPU side held `was` and hold
/// `new` from here on. (Whole words: a word only partly in it is left out.)
pub(crate) fn note(copy: u64, at: usize, was: &[u8], new: &[u8]) {
    let skip = at.wrapping_neg() & 3;
    let n = was.len().min(new.len()).saturating_sub(skip) & !3;
    if n == 0 {
        return;
    }
    let at = at + skip;
    let (mut was, new) = (was[skip..skip + n].to_vec(), new[skip..skip + n].to_vec());
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let noted = noted.entry(copy).or_default();
    // What was noted before in this submission has not gone up yet: the card
    // knows what stood there before that. And what it took is not what goes
    // up where this took it again.
    for t in noted.iter_mut() {
        let (a, b) = (at.max(t.at), (at + n).min(t.at + t.was.len()));
        if a < b {
            was[a - at..b - at].copy_from_slice(&t.was[a - t.at..b - t.at]);
            t.new[a - t.at..b - t.at].copy_from_slice(&new[a - at..b - at]);
        }
    }
    noted.push(Taken { at, was, new });
}

/// A submission's look at what it sends up, until it has run.
pub(super) struct Looks {
    found: (ID3D12Resource, usize),
    _bytes: ID3D12Resource,
    /// Per answer: the copy, and the bytes looked at.
    of: Vec<(std::sync::Arc<Shadow>, usize)>,
}

/// Per copy, since the last line: sends looked at, their bytes, words an
/// older value went over, and the first few of them.
#[derive(Default)]
struct Stale {
    looked: u64,
    bytes: u64,
    sends: u64,
    words: u64,
    some: Vec<String>,
}

static STALE: Mutex<BTreeMap<(u64, u64), Stale>> = Mutex::new(BTreeMap::new());

/// Records the looks at what `dirty` sends up into `list`, ahead of the
/// sends.
pub(super) fn record(
    host: &Host,
    list: &ID3D12GraphicsCommandList,
    dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)],
) -> Option<Looks> {
    let mut noted = std::mem::take(&mut *NOTED.lock().unwrap_or_else(|e| e.into_inner()));
    let mut all: Vec<(std::sync::Arc<Shadow>, Taken)> = Vec::new();
    let mut total = 0;
    for (shadow, _) in dirty {
        for taken in noted.remove(&shadow.id()).unwrap_or_default() {
            total += 2 * taken.was.len();
            if total <= MOST {
                all.push((shadow.clone(), taken));
            }
        }
    }
    if all.is_empty() {
        return None;
    }
    let pass = pass(host)?;
    let (bytes, to) = host.shared_buffer(total.min(MOST) as u64)?;
    let size = (all.len() as u64 * ANSWER).next_multiple_of(256);
    let (found, answers) = host.shared_buffer(size)?;
    // SAFETY: mappings of the sizes asked for; an open list, whose compute
    // state is set whole by what follows; live buffers, the ranges in them.
    unsafe {
        std::ptr::write_bytes(answers, 0, size as usize);
        list.SetComputeRootSignature(&pass.root);
        list.SetPipelineState(&pass.pso);
        list.SetComputeRootShaderResourceView(2, bytes.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(3, found.GetGPUVirtualAddress());
        let mut at = 0usize;
        for (i, (shadow, taken)) in all.iter().enumerate() {
            let n = taken.was.len();
            std::ptr::copy_nonoverlapping(taken.was.as_ptr(), to.add(at), n);
            std::ptr::copy_nonoverlapping(taken.new.as_ptr(), to.add(at + n), n);
            list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
            let constants = [taken.at as u32, (n / 4) as u32, at as u32, (at + n) as u32, (i as u64 * ANSWER) as u32];
            list.SetComputeRoot32BitConstants(0, 5, constants.as_ptr().cast(), 0);
            list.Dispatch((n / 4).div_ceil(64) as u32, 1, 1);
            at += 2 * n;
        }
        // The sends come after the looks.
        let mut seen = std::collections::HashSet::new();
        let done: Vec<D3D12_RESOURCE_BARRIER> = all
            .iter()
            .filter(|(s, _)| seen.insert(std::sync::Arc::as_ptr(s) as usize))
            .map(|(s, _)| transition(&s.buffer, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_COPY_DEST))
            .collect();
        list.ResourceBarrier(&done);
    }
    Some(Looks {
        found: (found, answers as usize),
        _bytes: bytes,
        of: all.into_iter().map(|(s, t)| (s, t.was.len())).collect(),
    })
}

impl Looks {
    /// Whether `shadow` was looked at: it is left in `COPY_DEST`.
    pub(super) fn hold(&self, shadow: &Shadow) -> bool {
        self.of.iter().any(|(s, _)| s.id() == shadow.id())
    }

    /// The submission has run: what it found is counted.
    pub(super) fn read(self) {
        // SAFETY: the mapping holds an answer for each; the card has run.
        let answers = unsafe { std::slice::from_raw_parts(self.found.1 as *const [u32; 4], self.of.len()) };
        let mut stale = STALE.lock().unwrap_or_else(|e| e.into_inner());
        for ((shadow, bytes), &[words, at, card, cpu]) in self.of.iter().zip(answers) {
            let (start, len) = shadow.span();
            let e = stale.entry((start, len)).or_default();
            e.looked += 1;
            e.bytes += *bytes as u64;
            if words > 0 {
                e.sends += 1;
                e.words += u64::from(words);
                if e.some.len() < 4 {
                    e.some.push(format!("{words} words from +{at:#x} (card {card:#x}, sent {cpu:#x}; {})", known(shadow.id(), at as usize)));
                }
            }
        }
        line(&mut stale);
    }
}

/// Once a second: what was found since.
fn line(stale: &mut BTreeMap<(u64, u64), Stale>) {
    static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let said = SAID.load(Ordering::Relaxed);
    if now < said + 1000 || SAID.compare_exchange(said, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let all = std::mem::take(stale);
    let sum = |f: fn(&Stale) -> u64| all.values().map(f).sum::<u64>();
    tracing::info!(
        frame = crate::d3d12::frame::current(),
        "up-copy check: {} pieces ({} KB) sent up again in {} buffers; {} of them put an older value over {} words the card had written",
        sum(|s| s.looked),
        sum(|s| s.bytes) >> 10,
        all.len(),
        sum(|s| s.sends),
        sum(|s| s.words),
    );
    let mut worst: Vec<_> = all.into_iter().filter(|(_, s)| s.words > 0).collect();
    worst.sort_by_key(|(_, s)| std::cmp::Reverse(s.words));
    for ((start, len), s) in worst.into_iter().take(12) {
        tracing::warn!(
            "up-copy check: older values sent up to {start:#x}+{len:#x}: {} words in {} pieces: {}",
            s.words,
            s.sends,
            s.some.join(", ")
        );
    }
}

/// Of a copy: the ranges shaders were declared to write (joined), and the
/// frame each piece was last fetched in.
#[derive(Default)]
struct Known {
    declared: Vec<(usize, usize)>,
    fetched: HashMap<usize, u64>,
}

static KNOWN: Mutex<BTreeMap<u64, Known>> = Mutex::new(BTreeMap::new());

/// Offsets `[from, to)` of a copy were declared as written.
pub(super) fn declared(copy: u64, from: usize, to: usize) {
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let known = known.entry(copy).or_default();
    if !known.declared.iter().any(|r| r.0 <= from && to <= r.1) {
        join_ranges(&mut known.declared, [(from, to)]);
    }
}

/// A piece from offset `at` on of a copy was fetched.
pub(super) fn fetched(copy: u64, at: usize) {
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    known.entry(copy).or_default().fetched.insert(at / PIECE, crate::d3d12::frame::current());
}

fn known(copy: u64, at: usize) -> String {
    let all = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(known) = all.get(&copy) else {
        return "no shader was declared to write the buffer".into();
    };
    if !touches(&known.declared, at, at + 4) {
        return "never declared as written there".into();
    }
    // (Fetched pieces begin where title memory's do; the copy may begin
    // inside one.)
    let last = [at / PIECE, (at / PIECE).saturating_sub(1)].iter().filter_map(|p| known.fetched.get(p)).max().copied();
    match last {
        Some(frame) => format!("declared; last fetched {} frames ago", crate::d3d12::frame::current().saturating_sub(frame)),
        None => "declared; never fetched".into(),
    }
}

mod checks;
mod rest;
#[cfg(test)]
mod tests;

pub(super) use checks::{order, reads};
pub(super) use rest::at_rest;
