use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use crate::backend::host;

use super::loose::{WINDOW_CARD_BYTES, WINDOW_MAPPED_BYTES};
use super::{PIECE, Shadow};

/// Whether the host copies of big title buffers are in the card's own
/// memory (on by default since run 75 showed it holds: the BVH skinning
/// list went from 50.5 to 4.5 ms on the card; `OATHKEEP_CARD_BUFFERS=0`
/// keeps every copy in the PC's memory, as before. Read when a copy is
/// made, so it takes a fresh start).
///
/// Every host copy was a buffer in the PC's memory that the card reads
/// and writes over the bus, so that our code can fill it and compare it.
/// The title's shaders read almost all their buffer data from a few big
/// title buffers (in the gameplay frame of run 69, 190-330 MB of distinct
/// ranges of the 174 MB heap per list against 0-13 MB elsewhere), and a
/// skinning dispatch of ONE group of 32 threads took 1.8 ms on the card
/// (run 70): the time of some 170 loads one after the other, not of the
/// arithmetic.
///
/// With the switch, a title buffer of [`card_limits`]' size or more has two
/// sides. Shaders see `Shadow::buffer`, in the card's memory. The CPU's
/// side (`Card::staging`, what `Shadow::data` maps) takes title memory as
/// before, and the ranges that changed in it are copied to the card's side
/// before each submission (`Host::card_before`). What shaders wrote is
/// found on the card: after the submission's lists a compute pass of ours
/// compares each declared range, by 64 KiB piece, with the card side's
/// bytes from before (`Card::before`), and only the pieces that differ are
/// fetched and merged into title memory, byte by byte as
/// [`Shadow::copy_back`] does (`Host::card_back`, [`Shadow::card_merge`]).
/// No snapshot is taken and nothing is compared on the CPU for such a copy.
///
/// What it assumes is what [`keeps`] assumes: shaders write a copy only in
/// ranges declared as written. A write outside them is seen by later
/// shaders, as before, and goes back to the title the next time a declared
/// range holds its piece (before: never).
pub(crate) fn cards() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_BUFFERS").is_none_or(|v| v != "0"))
}

/// Default `(min_mb, max_mb)` for [`card_limits`] chosen from the adapter's
/// local VRAM budget (`DXGI_QUERY_VIDEO_MEMORY_INFO.Budget`), in bytes.
///
/// - Unknown (`None`, e.g. WARP) or `< 6 GiB`: `(16, 512)` MiB.
/// - `>= 6 GiB` and `<= 12 GiB` (8–12 GB cards, whose WDDM budget is ~7.0–11.2
///   GiB): `(1, 512)` MiB. `Card::before` stores only 16-byte per-64-KiB hashes
///   (`0.024%` overhead), `max_mb` stays strictly capped at 512 MiB (already
///   reserved from the texture pool by [`card_room_kept`]), and [`card_tier_most`]
///   reserves the upper 128 MiB of that 512 MiB cap for `>= 8 MiB` buffers so
///   small buffers never crowd out heavy shader-written buffers.
/// - `> 12 GiB` and `< 20 GiB` (16 GB cards, whose WDDM budget is typically
///   ~13.5–15.2 GiB): `(1, 2048)` MiB.
/// - `>= 20 GiB` (24–32+ GB cards): `(1, (budget_mb / 4).min(8192))` MiB,
///   capping at 25% of local budget (at most 8 GiB).
pub(super) fn default_card_limits_mb(local_vram_budget: Option<u64>) -> (u64, u64) {
    let Some(budget_bytes) = local_vram_budget else {
        return (16, 512);
    };
    let budget_mb = budget_bytes >> 20;
    const SIX_GIB_MB: u64 = 6 * 1024;
    const TWELVE_GIB_MB: u64 = 12 * 1024;
    const TWENTY_GIB_MB: u64 = 20 * 1024;
    if budget_mb < SIX_GIB_MB {
        (16, 512)
    } else if budget_mb <= TWELVE_GIB_MB {
        (1, 640)
    } else if budget_mb < TWENTY_GIB_MB {
        (1, 2048)
    } else {
        (1, (budget_mb / 4).min(8192))
    }
}

/// Effective `CARD_BYTES[0]` ceiling for a candidate allocation of `size` bytes
/// within `most`. On `<= 1 GiB` card-buffer caps (such as 640 MiB on 8–12 GB
/// cards), sub-1-MiB buffers (e.g. from `OATHKEEP_CARD_BUFFERS_MIN_KB=64`) may
/// use only the first half of `most`, and `1..8 MiB` buffers up to `7/8` of
/// `most`, reserving the top eighth (80 MiB of 640 MiB) for `>= 8 MiB`
/// buffers so hundreds of tiny early buffers do not push 8–50 MiB UAV buffers
/// onto CPU snapshots.
pub(super) fn card_tier_most(size: u64, most: u64, all: bool) -> u64 {
    const MIB: u64 = 1 << 20;
    if all || !(128 * MIB..=1024 * MIB).contains(&most) {
        return most;
    }
    if size < MIB {
        most / 2
    } else if size < 8 * MIB {
        most / 8 * 7
    } else {
        most
    }
}

/// Resolve `(min_bytes, max_bytes)` from the adapter's local VRAM budget and
/// optional explicit MiB overrides from `OATHKEEP_CARD_BUFFERS_MIN_MB` /
/// `OATHKEEP_CARD_BUFFERS_MB`.
pub(super) fn resolve_card_limits(
    local_vram_budget: Option<u64>,
    min_mb_override: Option<u64>,
    max_mb_override: Option<u64>,
) -> (u64, u64) {
    const MIB: u64 = 1 << 20;
    let (default_min_mb, default_max_mb) = default_card_limits_mb(local_vram_budget);
    let min_bytes = min_mb_override.unwrap_or(default_min_mb).saturating_mul(MIB).max(MIB);
    let max_bytes = max_mb_override.unwrap_or(default_max_mb).saturating_mul(MIB);
    (min_bytes, max_bytes)
}

/// The smallest title buffer whose copy goes to the card's memory, and the
/// most bytes of such copies (their "before" twins not counted: a copy
/// shaders write takes its size twice), both in bytes:
/// `OATHKEEP_CARD_BUFFERS_MIN_MB` and `OATHKEEP_CARD_BUFFERS_MB`.
/// Defaults scale with the adapter's local VRAM budget (16 MiB / 512 MiB on
/// `<= 12 GiB` budgets, 4 MiB / 2048 MiB on 16 GB cards, 1 MiB / up to 8192 MiB
/// on `>= 20 GiB` budgets), and explicit env vars always override.
pub(super) fn card_limits() -> (u64, u64) {
    static LIMITS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *LIMITS.get_or_init(|| {
        let env_mb = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let vram_budget = crate::backend::host().and_then(|h| h.video_memory()).map(|[budget, ..]| budget);
        let (mut min_bytes, max_bytes) = resolve_card_limits(
            vram_budget,
            env_mb("OATHKEEP_CARD_BUFFERS_MIN_MB"),
            env_mb("OATHKEEP_CARD_BUFFERS_MB"),
        );
        // `OATHKEEP_CARD_BUFFERS_MIN_KB` (after fxceless c36971b2) says the
        // least size in KiB, so that it can go under 1 MiB: 64 is every title
        // buffer. Copies under 1 MiB are nearly all there are and few bytes;
        // in the PC's memory every load, store and atomic a shader does on
        // them crosses the bus.
        if let Some(kib) = std::env::var("OATHKEEP_CARD_BUFFERS_MIN_KB").ok().and_then(|v| v.trim().parse::<u64>().ok())
            && kib > 0
        {
            min_bytes = kib << 10;
        }
        if card_all() {
            min_bytes = 0;
        }
        tracing::info!(
            vram_budget_mb = vram_budget.map(|b| b >> 20),
            card_buffers_min_kb = min_bytes >> 10,
            card_buffers_max_mb = max_bytes >> 20,
            "card buffer limits"
        );
        (min_bytes, max_bytes)
    })
}

/// Bytes alive of copies' card sides, and of their "before" twins.
pub(super) static CARD_BYTES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether the texture queue leaves room on the card for the card copies
/// of title buffers that are not made yet
/// (`OATHKEEP_CARD_ROOM_FOR_BUFFERS=1`; off by default).
///
/// The copies are made in one burst when the world loads (run 189: 94 of
/// its 95 within two seconds, right before the first world frame), and
/// [`card_for`] makes one only while the card is under eight tenths of its
/// budget. That held as long as the textures picked by index filled the
/// card after the burst. Since the texture queue keeps packed neighbours
/// (52b8060) it fills the card while the world loads: 85% and more at the
/// burst in runs 190 to 192, so one copy of 95 was made, the CPU looked
/// for shader writes in 600 to 720 MB a frame where it had been 30, and
/// the frame went from about 240 to 370-470 ms.
///
/// With the switch, the texture queue's rule (`resource::room`) counts the
/// part of [`card_limits`]' second number that no copy has taken yet as
/// used, and a copy may be made up to nine tenths of the budget: the room
/// it takes was kept for it. After the burst what is kept is what the
/// burst left of the limit (87 MB of 1,024 in run 189).
///
/// On by default from the build after run 199
/// (`OATHKEEP_CARD_ROOM_FOR_BUFFERS=0`: off, as before). Run 193 had it on and still made 31 copies of 95: the copies
/// and the world's render targets (+1,603 MB) arrive in the same first
/// world frame and raced for the 448 MB left under the line, and a copy
/// turned down is not asked for again (`lookup` asks once), while its room
/// went on being kept (616 MB to the end of the run). So now: the texture
/// queue also keeps room for the targets (`resource::targets_room_kept`),
/// a copy is made up to 97 hundredths of the budget (its room was kept),
/// and the room of a copy turned down is given up ([`CARD_REFUSED`]).
fn card_room_kept_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_ROOM_FOR_BUFFERS").is_none_or(|v| v != "0"))
}

/// Title buffers big enough for a copy in the card's memory that were
/// turned down for want of room, since the start, and their bytes: their
/// copies stay in the PC's memory for as long as the title keeps the
/// buffers, so no room is kept for them any more.
static CARD_REFUSED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The bytes of the card's memory the texture queue leaves free for card
/// copies of title buffers not made yet ([`card_room_kept_on`]): 0 with
/// the switch off or without card copies.
pub(crate) fn card_room_kept() -> u64 {
    if !card_room_kept_on() || !cards() {
        return 0;
    }
    // Not more than such copies came to in any session, whatever they may
    // come to: on a card of 24 GB a quarter of its budget was kept for them,
    // 5 GB that never came (941 MB in 97 copies after fifteen minutes), and
    // no texture was made any more with 5 GB of the card free (session
    // play-1125-0127: 1,453 passes refused, the ground and people black).
    const SEEN: u64 = 1 << 30;
    card_limits().1.min(SEEN).saturating_sub(CARD_BYTES[0].load(Ordering::Relaxed) + CARD_REFUSED[1].load(Ordering::Relaxed))
}

/// The copies in the card's memory.
pub(super) static CARDED: Mutex<Vec<std::sync::Weak<Shadow>>> = Mutex::new(Vec::new());

/// Since the last frame line: bytes copied to card sides and the ranges
/// they were; pieces compared on the card, pieces fetched and their
/// bytes; the waits for the card the fetches added and their microseconds.
pub(crate) static CARD_STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(crate) fn live_span_bytes() -> u64 {
    CARD_BYTES[0].load(Ordering::Relaxed)
        + WINDOW_CARD_BYTES.load(Ordering::Relaxed)
        + WINDOW_MAPPED_BYTES.load(Ordering::Relaxed)
}

/// [`CARD_STATS`], taken; then the copies in the card's memory, their
/// bytes and their "before" twins' bytes.
pub(crate) fn card_stats() -> [u64; 10] {
    let mut out = [0u64; 10];
    for (i, stat) in CARD_STATS.iter().enumerate() {
        out[i] = stat.swap(0, Ordering::Relaxed);
    }
    out[7] = CARDED.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|w| w.strong_count() > 0).count() as u64;
    out[8] = CARD_BYTES[0].load(Ordering::Relaxed)
        + WINDOW_CARD_BYTES.load(Ordering::Relaxed)
        + WINDOW_MAPPED_BYTES.load(Ordering::Relaxed);
    out[9] = CARD_BYTES[1].load(Ordering::Relaxed);
    out
}

/// The bytes of the hashes kept on the card for a copy of `size` bytes: 16
/// for each 64 KiB piece (a copy that does not start on a piece boundary
/// has one piece more).
pub(crate) fn card_hash_bytes(size: u64) -> u64 {
    (size / PIECE as u64 + 2) * 16
}

/// A buffer of `size` bytes in the card's memory for a title buffer's
/// copy, if the copy is to be there: [`cards`] is on, the size is within
/// [`card_limits`] and below 1 GiB (the compare counts in 32 bits), and the
/// host has what it takes (`Host::card_ready`).
pub(super) fn card_for(size: u64) -> Option<ID3D12Resource> {
    let there = card_room_for(size);
    if there.is_some() || !card_all() || size >= 1 << 30 {
        return there;
    }
    // Two sides all the same, the card's in the PC's memory.
    let host = host()?;
    if !cards() || !host.card_ready() {
        return None;
    }
    let (buffer, _) = host.shared_buffer(size)?;
    CARD_BYTES[0].fetch_add(size, Ordering::Relaxed);
    Some(buffer)
}

/// Whether every title buffer's copy has two sides, however small
/// (`OATHKEEP_CARD_ALL=1`; off by default).
///
/// A copy with one side, in the PC's memory, is what the card reads while
/// our refreshes write it: a submission that is prepared changes it under
/// the ones in the air ahead of it. The title's CPU does not write what they
/// still read, but its own buffer copies are done by us at preparing
/// (`crate::backend::upload_snapshot` has the order of those right for copies with two
/// sides), and so are the bytes put in beside shaders' writes.
pub(super) fn card_all() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_ALL").is_some_and(|v| v == "1"))
}

/// [`card_for`] in the card's own memory.
fn card_room_for(size: u64) -> Option<ID3D12Resource> {
    if !cards() {
        return None;
    }
    let (least, most) = card_limits();
    if size < least || size >= 1 << 30 {
        return None;
    }
    let host = host()?;
    if !host.card_ready() {
        return None;
    }
    // Only while the card has room: with this copy on it, a fifth of the
    // budget the system gives us must still be free. Copies are made when
    // the world loads, before the textures fill the card; the texture
    // queue's own rule (`resource::room`: nine tenths of the budget) counts
    // them, so the card's use stays where it was and the textures' share
    // shrinks by the copies' bytes, at most `card_limits`' second number.
    // (With `card_room_kept_on` the texture queue kept this copy's room
    // free, and the copy may go up to the queue's own line.)
    let hundredths = if card_room_kept_on() { 97 } else { 80 };
    if let Some([budget, used, ..]) = host.video_memory()
        && used + size > budget / 100 * hundredths
    {
        // Said, with the numbers it was decided by: run 193 made 31 copies
        // of 95 and no line named the others. The first 64, then every
        // 32nd.
        let n = CARD_REFUSED[0].fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = CARD_REFUSED[1].fetch_add(size, Ordering::Relaxed) + size;
        if n <= 64 || n.is_multiple_of(32) {
            tracing::info!(
                "no card copy for a title buffer of {} KiB: the card has {} MB in use of a budget of {} MB and a copy is made up to \
                 {} MB; the copies made hold {} MB of at most {} MB; {} buffers turned down so far ({} MB), frame {}",
                size >> 10,
                used >> 20,
                budget >> 20,
                (budget / 100 * hundredths) >> 20,
                CARD_BYTES[0].load(Ordering::Relaxed) >> 20,
                most >> 20,
                n,
                bytes >> 20,
                crate::d3d12::frame::current()
            );
        }
        return None;
    }
    let tier_most = card_tier_most(size, most, card_all());
    if CARD_BYTES[0].fetch_add(size, Ordering::Relaxed) + size > tier_most {
        CARD_BYTES[0].fetch_sub(size, Ordering::Relaxed);
        return None;
    }
    let buffer = host.uav_buffer(size);
    if buffer.is_none() {
        CARD_BYTES[0].fetch_sub(size, Ordering::Relaxed);
    }
    buffer
}
