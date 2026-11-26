use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use crate::host::host;

use super::card_alloc::{CARDED, cards};
use super::{Card, Shadow};
use super::window_grid::{WINDOW, fine, holder, longest_tail_to, private_vad_bounds, tail_bounds, tails, window, window_of};

/// The most bytes of windows outside buffers whose copies go to the card's
/// memory (`OATHKEEP_CARD_WINDOWS_MB`; 0, the default, none).
///
/// Windows are two thirds of the PC memory the card shares (run 104: 861
/// windows, 3,552 MB of 5,400), and every draw that takes vertices, indices
/// or constants from the title's own allocator reads them over the bus.
/// Shaders write none of them (run 91: "0.0 MB compared in 0 ranges of
/// windows"), so a window on the card needs only what a card copy has
/// already: the CPU's side takes title memory as before, and what changed
/// in it goes up before each submission. No hashes, no compare, no fetch,
/// unless a shader does declare a write to it one day.
///
/// A window goes to the card while there is room under this number and
/// while a fifth of the card's budget is free, as title buffers do
/// (`card_for`): textures keep their room, and once they have filled the
/// card later windows stay where they are. The CPU's side stays in the PC's
/// memory, so the memory shared with the card does not shrink by this
/// alone; what moves is what shaders read.
fn window_card_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_CARD_WINDOWS_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0) << 20
    })
}

/// Bytes alive of windows' card sides ([`window_card_most`]).
pub(super) static WINDOW_CARD_BYTES: AtomicU64 = AtomicU64::new(0);

/// A buffer in the card's memory for a window of `size` bytes, if it is to
/// be there ([`window_card_most`]).
fn window_card_for(size: u64) -> Option<ID3D12Resource> {
    let there = window_room_for(size);
    if there.is_some() || !windows_own() || !cards() {
        return there;
    }
    // Two sides all the same, the card's in the PC's memory.
    let host = host()?;
    if !host.card_ready() {
        return None;
    }
    let (buffer, _) = host.shared_buffer(size)?;
    WINDOW_CARD_BYTES.fetch_add(size, Ordering::Relaxed);
    Some(buffer)
}

/// The most bytes of windows outside buffers that lie in the card's memory
/// and are filled by the CPU through the bus (`OATHKEEP_WINDOWS_BAR_MB`; 0, the
/// default, none): one buffer (`D3D12_HEAP_TYPE_GPU_UPLOAD`), no CPU's side, no
/// copy to send and no list of changes, unlike [`window_card_most`]. The CPU
/// only writes them, as no shader is declared to write a window; if one were,
/// its snapshot and compare would read through the bus, right but slow. In a
/// captured frame the lists take 26 ms of the card instead of 29, the frame 32
/// instead of 36; filling them costs the CPU what it did.
///
/// Windows go there while a fifth of the card's budget is free, as with
/// [`window_room_for`], which this comes before.
fn window_mapped_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_WINDOWS_BAR_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0) << 20
    })
}

/// Bytes alive of windows in card memory the CPU maps.
pub(super) static WINDOW_MAPPED_BYTES: AtomicU64 = AtomicU64::new(0);

/// A buffer for a window of `size` bytes in card memory the CPU maps, if it is
/// to be there ([`window_mapped_most`]).
fn window_mapped_for(size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    let most = window_mapped_most();
    if most == 0 {
        return None;
    }
    let host = host()?;
    if let Some([budget, used, ..]) = host.video_memory()
        && used + size > budget / 10 * 8
    {
        return None;
    }
    if WINDOW_MAPPED_BYTES.fetch_add(size, Ordering::Relaxed) + size > most {
        WINDOW_MAPPED_BYTES.fetch_sub(size, Ordering::Relaxed);
        return None;
    }
    let made = host.mapped_card_buffer(size, true);
    if made.is_none() {
        WINDOW_MAPPED_BYTES.fetch_sub(size, Ordering::Relaxed);
    }
    made
}

/// Whether windows outside buffers have two sides too, so that submissions
/// in the air read the bytes they were prepared with (`OATHKEEP_WINDOWS_OWN=1`;
/// off by default: it takes the windows' bytes again, 3.3 GB in capture
/// replay2, and what changes in them under submissions in the air is what the
/// title's CPU writes, which it does not while they read it).
fn windows_own() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WINDOWS_OWN").is_some_and(|v| v == "1"))
}

/// [`window_card_for`] in the card's own memory.
fn window_room_for(size: u64) -> Option<ID3D12Resource> {
    let most = window_card_most();
    if most == 0 || !cards() {
        return None;
    }
    let host = host()?;
    if !host.card_ready() {
        return None;
    }
    if let Some([budget, used, ..]) = host.video_memory()
        && used + size > budget / 10 * 8
    {
        return None;
    }
    if WINDOW_CARD_BYTES.fetch_add(size, Ordering::Relaxed) + size > most {
        WINDOW_CARD_BYTES.fetch_sub(size, Ordering::Relaxed);
        return None;
    }
    let buffer = host.uav_buffer(size);
    if buffer.is_none() {
        WINDOW_CARD_BYTES.fetch_sub(size, Ordering::Relaxed);
    }
    buffer
}

/// A copy of the memory `[start, start + len)` (an arena of
/// [`crate::memory`]'s, its directory, title memory around a small buffer)
/// for the programs that find memory by address, on the card: the structures
/// rays are traced through, read with every step of every ray.
///
/// What shaders write there by address is not brought back, so the copy is
/// the only place that has it. The CPU's side holds the bytes as they were
/// last taken, and only dwords that changed since go up.
pub(crate) fn by_address(start: u64, len: u64) -> Option<Arc<Shadow>> {
    let host = host()?;
    if !cards() || !host.card_ready() {
        return None;
    }
    // At most so much on the card's own memory (4 GiB unless
    // `OATHKEEP_MEMORY_CARD_MB` says otherwise), and while it has room; past
    // that the side shaders see is in the PC's memory.
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    static USED: AtomicU64 = AtomicU64::new(0);
    let most = *MOST.get_or_init(|| {
        let mb = std::env::var("OATHKEEP_MEMORY_CARD_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        mb.unwrap_or(4096) << 20
    });
    let room = host.video_memory().is_none_or(|[budget, used, ..]| used + len < budget / 100 * 97);
    let (staging, data) = host.shared_buffer(len)?;
    let on_card = if room && USED.fetch_add(len, Ordering::Relaxed) + len <= most {
        host.uav_buffer(len)
    } else {
        None
    };
    let buffer = match on_card {
        Some(buffer) => buffer,
        None => host.shared_buffer(len)?.0,
    };
    crate::host::name(&staging, || format!("title memory {start:#x}+{len:#x} by address, the CPU's side"));
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x} by address"));
    // (Counted down with the windows' on the card when it goes.)
    WINDOW_CARD_BYTES.fetch_add(len, Ordering::Relaxed);
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let mut shadow = Shadow::new(buffer, data, gpu, start, len);
    shadow.window = true;
    shadow.changes_only.store(true, Ordering::Relaxed);
    shadow.card = Some(Card {
        staging,
        before: Mutex::default(),
        before_gpu: AtomicU64::new(0),
        dirty: Mutex::default(),
        sent: Mutex::default(),
        touched: Default::default(),
        size: len,
    });
    let shadow = Arc::new(shadow);
    CARDED.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&shadow));
    Some(shadow)
}

/// Copies of title memory outside buffers, by (start, length).
pub(super) static LOOSE: std::sync::RwLock<BTreeMap<(u64, u64), Arc<Shadow>>> = std::sync::RwLock::new(BTreeMap::new());

pub(super) fn loose(address: u64, end: u64, unsized_ask: bool, any: bool) -> Option<Arc<Shadow>> {
    let (cell, cell_len) = window_of(address, end, WINDOW);
    let cell_end = cell + cell_len;
    let search_floor = address.saturating_sub(WINDOW);
    let named = window_of(address, end, if unsized_ask { WINDOW } else { window() });
    // Under the lock (`read` or `write`), with the handing out: `let_go_idle`
    // takes `LOOSE.write()`, so cloning `Arc<Shadow>` under `LOOSE.read()` is
    // mutually exclusive with eviction.
    let note = |shadow: &Shadow| {
        // (Read first: the lines are shared by all recording threads.)
        let now = crate::host::moment();
        if shadow.used.load(Ordering::Relaxed) != now {
            shadow.used.store(now, Ordering::Relaxed);
        }
        if shadow.asked[0].load(Ordering::Relaxed) > address {
            shadow.asked[0].fetch_min(address, Ordering::Relaxed);
        }
        if shadow.asked[1].load(Ordering::Relaxed) < end {
            shadow.asked[1].fetch_max(end, Ordering::Relaxed);
        }
        if unsized_ask {
            shadow.unsized_asks.fetch_add(1, Ordering::Relaxed);
        }
    };
    let mut queried_vad: Option<Option<(u64, u64)>> = None;
    // Fast path under shared `LOOSE.read()` for all window modes (`named`, `holder`, and `tails`).
    {
        let map = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        if unsized_ask && tails() {
            let (full_step, _) = tail_bounds(address, end, None);
            let full_hit = longest_tail_to(
                map.range((search_floor, 0)..=(full_step, u64::MAX)).map(|(k, _)| *k),
                search_floor,
                full_step,
                cell_end,
            );
            if let Some(key) = full_hit
                && let Some(shadow) = map.get(&key)
            {
                note(shadow);
                return Some(shadow.clone());
            }
            let vad = private_vad_bounds(address, cell_end);
            queried_vad = Some(vad);
            if vad.is_some() {
                let (step, reach) = tail_bounds(address, end, vad);
                let vad_hit = longest_tail_to(
                    map.range((search_floor, 0)..=(step, u64::MAX)).map(|(k, _)| *k),
                    search_floor,
                    step,
                    reach,
                );
                if let Some(key) = vad_hit
                    && let Some(shadow) = map.get(&key)
                {
                    note(shadow);
                    return Some(shadow.clone());
                }
            }
        } else if any && fine() && !unsized_ask {
            if let Some(shadow) = map.get(&named) {
                note(shadow);
                return Some(shadow.clone());
            }
            let there = map.range((search_floor, 0)..=(address, u64::MAX)).map(|(k, _)| *k);
            if let Some(key) = holder(there, address, end)
                && let Some(shadow) = map.get(&key)
            {
                note(shadow);
                return Some(shadow.clone());
            }
        } else if let Some(shadow) = map.get(&named) {
            note(shadow);
            return Some(shadow.clone());
        }
    }
    let vad = if unsized_ask && tails() {
        queried_vad.unwrap_or_else(|| private_vad_bounds(address, cell_end))
    } else {
        None
    };
    let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
    let (start, len) = if unsized_ask && tails() {
        let (step, reach) = tail_bounds(address, end, vad);
        let there = map.range((search_floor, 0)..=(step, u64::MAX)).map(|(key, _)| *key);
        longest_tail_to(there, search_floor, step, reach).unwrap_or((step, reach - step))
    } else if any && fine() && !unsized_ask {
        let there = map.range((search_floor, 0)..=(address, u64::MAX)).map(|(key, _)| *key);
        holder(there, address, end).unwrap_or(named)
    } else {
        named
    };
    if let Some(shadow) = map.get(&(start, len)) {
        note(shadow);
        return Some(shadow.clone());
    }
    // In place, if asked for and the window's memory can be opened: the
    // window is the title's own memory, with nothing to copy.
    if let Some(placed) = crate::inplace::place_for(start, len, true) {
        let shadow = Arc::new(Shadow::placed(placed, start, len, true));
        note(&shadow);
        map.insert((start, len), shadow.clone());
        return Some(shadow);
    }
    // One buffer in the card's memory, filled by the CPU through the bus.
    if let Some((buffer, data)) = window_mapped_for(len) {
        crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x} mapped on the card"));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        let mut shadow = Shadow::new(buffer, data, gpu, start, len);
        shadow.window = true;
        shadow.mapped = true;
        let shadow = Arc::new(shadow);
        note(&shadow);
        map.insert((start, len), shadow.clone());
        return Some(shadow);
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // With room for windows on the card (`window_card_most`): shaders get a
    // buffer there, and the one just made is the CPU's side.
    let (buffer, card) = match window_card_for(len) {
        Some(on_card) => {
            crate::host::name(&on_card, || format!("title memory {start:#x}+{len:#x} on the card"));
            let card = Card {
                staging: buffer,
                before: Mutex::default(),
                before_gpu: AtomicU64::new(0),
                dirty: Mutex::default(),
                sent: Mutex::default(),
                touched: Default::default(),
                size: len,
            };
            (on_card, Some(card))
        }
        None => (buffer, None),
    };
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let mut shadow = Shadow::new(buffer, data, gpu, start, len);
    shadow.window = true;
    shadow.card = card;
    let shadow = Arc::new(shadow);
    if shadow.card.is_some() {
        CARDED.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&shadow));
    }
    note(&shadow);
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

/// Seeds a newly created or recycled CPU-mapped buffer slot with title bytes
/// from `[start, start + len)` and zeroes trailing alignment padding up to
/// `aligned_size`. `copied` remains zeroed so the next `Shadow::refresh` still
/// tracks and refreshes subsequent title writes normally.
pub(super) fn seed_mapped_bytes(data: *mut u8, aligned_size: u64, start: u64, len: u64) {
    let total = aligned_size as usize;
    let copy_len = (len as usize).min(total);
    if total == 0 || data.is_null() {
        return;
    }
    // SAFETY: `data` points to `total` mapped bytes owned exclusively by this shadow.
    let dst = unsafe { std::slice::from_raw_parts_mut(data, total) };
    if copy_len > 0 && !crate::host::read_into(start as *const u8, &mut dst[..copy_len]) {
        dst[..copy_len].fill(0);
    }
    if copy_len < total {
        dst[copy_len..].fill(0);
    }
}
