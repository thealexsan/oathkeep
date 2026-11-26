use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use crate::backend::host;

use super::card_alloc::{CARDED, card_all, card_for, card_limits, cards};
use super::loose::{LOOSE, loose};
use super::{Card, Shadow};
use super::window_grid::{WINDOW, fine};

pub(super) struct Entry {
    pub(super) end: u64,
    pub(super) shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
///
/// Read by every view a draw names, on all recording threads at once, and
/// written when a buffer comes, goes or gets its copy: readers share it.
static BUFFERS: std::sync::RwLock<BTreeMap<(u64, usize), Entry>> = std::sync::RwLock::new(BTreeMap::new());

pub(super) fn buffers() -> std::sync::RwLockWriteGuard<'static, BTreeMap<(u64, usize), Entry>> {
    let map = BUFFERS.write().unwrap_or_else(|e| e.into_inner());
    // What threads remember of it ([`FOUND`]) is of before.
    BUFFERS_CHANGED.fetch_add(1, Ordering::SeqCst);
    map
}

/// How often [`BUFFERS`] was taken to be changed.
static BUFFERS_CHANGED: AtomicU64 = AtomicU64::new(1);

/// Answers a thread remembers.
const FOUND_MOST: usize = 1024;

/// An ask (address, bytes), the count of changes it was answered at, and the
/// answer.
type Found = (u64, u64, u64, std::sync::Weak<Shadow>);

thread_local! {
    /// The last answers of [`lookup`] out of [`BUFFERS`], by a hash of the
    /// ask: the same buffers are asked about draw after draw, from five
    /// threads, each time through the map's lock and a dozen of its nodes
    /// (7 % of the recording threads' time, profile waits-1).
    static FOUND: std::cell::RefCell<Vec<Found>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether [`FOUND`] is kept (`OATHKEEP_QUICK_FIND=0`: not).
fn quick_find() -> bool {
    crate::toggled!("OATHKEEP_QUICK_FIND")
}

fn found_at(address: u64, len: u64) -> usize {
    let h = (address ^ len.rotate_left(32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> 54) as usize % FOUND_MOST
}

/// What this thread remembers of an ask, if nothing changed since.
pub(super) fn found(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let changed = BUFFERS_CHANGED.load(Ordering::SeqCst);
    let shadow = FOUND.with(|f| {
        let f = f.borrow();
        let (a, n, at, shadow) = f.get(found_at(address, len))?;
        ((*a, *n, *at) == (address, len, changed)).then(|| shadow.upgrade()).flatten()
    })?;
    // Whoever takes the map to let a copy go that nobody holds finds it held
    // now, or had counted before: then it is not handed out.
    std::sync::atomic::fence(Ordering::SeqCst);
    (BUFFERS_CHANGED.load(Ordering::SeqCst) == changed).then_some(shadow)
}

/// Remembers an answer given while `changed` was the count.
fn remember(address: u64, len: u64, changed: u64, shadow: &Arc<Shadow>) {
    FOUND.with(|f| {
        let mut f = f.borrow_mut();
        if f.is_empty() {
            f.resize_with(FOUND_MOST, || (0, 0, 0, std::sync::Weak::new()));
        }
        f[found_at(address, len)] = (address, len, changed, Arc::downgrade(shadow));
    });
}

pub(super) fn buffers_read() -> std::sync::RwLockReadGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.read().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    buffers().remove(&(start, resource));
}

/// The title buffer holding `address`, as (start, length), without making
/// its copy (diagnostics: which buffer a dropped program's root view names).
#[cfg(feature = "diagnostics")]
pub(crate) fn extent(address: u64) -> Option<(u64, u64)> {
    buffers()
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end > address)
        .map(|(k, e)| (k.0, e.end - k.0))
}

/// Title memory `[at, at + len)` is about to show something else or be
/// given up: what is in place there (`crate::memory::inplace`) is taken out of the
/// tables, so that the next ask makes a copy or opens the memory anew, and
/// goes when no list and no slot holds it any more. Returns how many.
pub(crate) fn in_place_let_go(at: u64, len: u64) -> u64 {
    let end = at.saturating_add(len);
    let mut gone: Vec<Arc<Shadow>> = Vec::new();
    {
        let mut map = LOOSE.write().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<(u64, u64)> = map
            .range((at.saturating_sub(WINDOW), 0)..(end, 0))
            .filter(|(key, shadow)| shadow.place.is_some() && key.0 + key.1 > at)
            .map(|(key, _)| *key)
            .collect();
        gone.extend(keys.into_iter().filter_map(|key| map.remove(&key)));
    }
    {
        // As `lookup` finds a buffer: among the 256 that start below.
        let mut map = buffers();
        for (_, entry) in map.range_mut(..(end, 0)).rev().take(256) {
            if entry.end > at && entry.shadow.as_ref().is_some_and(|s| s.place.is_some()) {
                gone.extend(entry.shadow.take());
            }
        }
    }
    let n = gone.len() as u64;
    for shadow in gone {
        crate::backend::retire(shadow);
    }
    n
}

/// For the in-place census (`crate::memory::inplace`): the title buffers (start,
/// bytes, false) and the windows outside buffers that have a copy (start,
/// bytes, true).
pub(crate) fn spans() -> Vec<(u64, u64, bool)> {
    let mut spans: Vec<(u64, u64, bool)> = buffers_read().iter().map(|(key, entry)| (key.0, entry.end - key.0, false)).collect();
    let loose = LOOSE.write().unwrap_or_else(|e| e.into_inner());
    spans.extend(loose.keys().map(|&(start, len)| (start, len, true)));
    spans
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use. A `len` of 0 or 1 is an ask without a length
/// (a root view: the caller reads on to the copy's end).
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    lookup(address, len, false)
}

/// As [`find`], for an ask that is done with the copy when its list has
/// run (a draw's vertex and index buffers, indirect arguments): outside
/// buffer objects it takes a window that is there already and holds the
/// range ([`holder`]), and makes one of its own only if none does.
///
/// [`find`] names a window by the ask alone, which a descriptor slot needs
/// (the slot shows the window its view found when it was made, and the
/// pass that brings it up to date must find the same one). A draw needs no
/// such thing: it binds the copy it found and brings that copy up to date.
/// Named by the ask alone, every draw's range was a window of its own
/// wherever the windows are cut finer than 4 MiB: draws that bind
/// different ranges of the same memory then each copied their range, and
/// windows nothing asked for again for ten seconds were let go and made
/// (and copied) anew. Run 71, on a 256 KiB grid: 350 MB copied per frame
/// against 120 MB, 7,087 write-watch askings against 2,600, and 5,263
/// windows (4.5 GB) let go in two minutes against 393 in a whole run.
pub(crate) fn find_any(address: u64, len: u64) -> Option<Arc<Shadow>> {
    // With whole windows (the default) there is one window to a cell and
    // nothing to choose: exactly `find`.
    if !fine() {
        return find(address, len);
    }
    lookup(address, len, true)
}

/// The copy [`find`] gives for a range inside a title buffer, if it was made.
pub(crate) fn peek(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    buffers_read().range(..=(address, usize::MAX)).rev().take(64).find(|(_, e)| e.end >= end).and_then(|(_, e)| e.shadow.clone())
}

fn lookup(address: u64, len: u64, any: bool) -> Option<Arc<Shadow>> {
    let quick = quick_find();
    if quick && let Some(shadow) = found(address, len) {
        return Some(shadow);
    }
    let end = address.checked_add(len.max(1))?;
    {
        let map = buffers_read();
        match map.range(..=(address, usize::MAX)).rev().take(64).find(|(_, e)| e.end >= end) {
            // The copy is made already (nearly always).
            Some((_, Entry { shadow: Some(shadow), .. })) => {
                if quick {
                    // (Nobody changes the map while this lock is held.)
                    remember(address, len, BUFFERS_CHANGED.load(Ordering::SeqCst), shadow);
                }
                return Some(shadow.clone());
            }
            Some(_) => {}
            None => {
                drop(map);
                return loose(address, end, len <= 1, any);
            }
        }
    }
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end, len <= 1, any);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let size = len.next_multiple_of(1 << 16);
        // In place, if asked for and the memory can be opened
        // (`crate::memory::inplace::place_for`): the buffer is the title's own
        // memory. Not the big ones `cards` puts in the card's memory: those
        // are read too often to stay in the PC's.
        let big = cards() && (size >= card_limits().0 || card_all());
        if !big && let Some(placed) = crate::memory::inplace::place_for(key.0, len, false) {
            entry.shadow = Some(Arc::new(Shadow::placed(placed, key.0, len, false)));
            return entry.shadow.clone();
        }
        let (buffer, data) = host()?.shared_buffer(size)?;
        crate::backend::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // A big one, with `cards`: shaders get a buffer in the card's own
        // memory, and the one just made is the CPU's side.
        let (buffer, card) = match card_for(size) {
            Some(on_card) => {
                crate::backend::name(&on_card, || format!("title buffer {:#x}+{len:#x} on the card", key.0));
                tracing::info!(
                    start = format_args!("{:#x}", key.0),
                    len,
                    "a buffer's host copy is in the card's memory"
                );
                let card = Card {
                    staging: buffer,
                    before: Mutex::default(),
                    before_gpu: AtomicU64::new(0),
                    dirty: Mutex::default(),
                    sent: Mutex::default(),
                touched: Default::default(),
                    size,
                };
                (on_card, Some(card))
            }
            None => (buffer, None),
        };
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        let mut shadow = Shadow::new(buffer, data, gpu, key.0, len);
        shadow.card = card;
        let shadow = Arc::new(shadow);
        if shadow.card.is_some() {
            CARDED.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&shadow));
        }
        entry.shadow = Some(shadow);
    }
    entry.shadow.clone()
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}
