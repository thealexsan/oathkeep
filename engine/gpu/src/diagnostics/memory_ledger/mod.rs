//! Who has what memory of the card, and who had it (diagnostics).
//!
//! A device loss names a faulting address and, from a short list, what was
//! freed there. A texture gone for minutes is in "nothing named", and which
//! descriptor still names it is not told at all. Here every buffer, texture
//! and texture block of ours stands with its address from when it is made,
//! and stays with the time it went ([`made`], [`block`]); a texture's address
//! is the base in the descriptor of its first view, which the card's heaps
//! hold as plain bytes (measured: `indirect::tests::descriptors_written_under_readers`).
//!
//! - At a device loss ([`at_loss`]): who owns the faulting address and who
//!   owned it, newest first, and every slot of the view heap whose
//!   descriptor reaches it.
//! - `OATHKEEP_SLOT_CHECK=1`: a thread walks the view heap and says which
//!   slots name memory that is gone, while nothing is lost yet ([`check`]).
//!
//! The book is kept with device-removed data on (`OATHKEEP_D3D12_DRED`) or the
//! check; `OATHKEEP_BOOK=0` keeps none.
//!
//! What the descriptors' bits mean is AMD's (RDNA 2 and 3); on another card
//! nothing is decoded as an address we know, and nothing is said.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

mod check;
#[cfg(test)]
mod tests;
mod watch;

pub(crate) use check::{at_loss, check};
use watch::{texture_address, watch};

/// Entries of what is gone that are kept.
const GONE: usize = 400_000;
/// Longer than anything of ours: how far below an address its owner starts.
const LONGEST: u64 = 2 << 30;
/// Entries longer than this are few and looked through one by one.
const LONG: u64 = 64 << 20;

#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) start: u64,
    pub(super) size: u64,
    pub(super) what: String,
    pub(super) made: u64,
    /// What was told of it since ([`note`]).
    pub(super) notes: String,
}

#[derive(Default)]
pub(super) struct Book {
    next: u64,
    pub(super) live: HashMap<u64, Entry>,
    by_start: BTreeMap<(u64, u64), ()>,
    long: Vec<u64>,
    /// By the object's address, for a name given after it was made.
    pub(super) by_object: HashMap<usize, u64>,
    pub(super) gone: VecDeque<(Entry, u64)>,
    /// Objects whose address could not be told, by what they are.
    pub(super) unknown: BTreeMap<String, u64>,
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);
/// Root descriptors given by address lately: time, address, bytes.
pub(super) static ROOTS: Mutex<VecDeque<(u64, u64, u64)>> = Mutex::new(VecDeque::new());

/// Tests that want the book.
#[cfg(test)]
pub(super) static FORCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn on() -> bool {
    #[cfg(test)]
    if FORCED.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("OATHKEEP_BOOK").is_none_or(|v| v != "0")
            && !cfg!(test)
            && (crate::backend::dred_any() || checks())
    })
}

/// `OATHKEEP_SLOT_CHECK=1`.
pub(crate) fn checks() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_SLOT_CHECK").is_some_and(|v| v != "0"))
}

pub(super) fn with<T>(f: impl FnOnce(&mut Book) -> T) -> T {
    f(BOOK.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Book::default))
}

impl Book {
    pub(super) fn add(&mut self, object: usize, start: u64, size: u64, what: String, now: u64) -> u64 {
        self.next += 1;
        let id = self.next;
        self.by_start.insert((start, id), ());
        if size > LONG {
            self.long.push(id);
        }
        self.live.insert(id, Entry { start, size, what, made: now, notes: String::new() });
        if object != 0 {
            self.by_object.insert(object, id);
        }
        id
    }

    pub(super) fn remove(&mut self, id: u64, object: usize, now: u64) {
        if let Some(entry) = self.live.remove(&id) {
            self.by_start.remove(&(entry.start, id));
            if entry.size > LONG {
                self.long.retain(|&l| l != id);
            }
            if self.gone.len() >= GONE {
                self.gone.pop_front();
            }
            self.gone.push_back((entry, now));
        }
        if self.by_object.get(&object) == Some(&id) {
            self.by_object.remove(&object);
        }
    }

    /// Whether something alive holds `at`.
    pub(super) fn alive(&self, at: u64) -> bool {
        let holds = |id: &u64| self.live.get(id).is_some_and(|e| at >= e.start && at - e.start < e.size);
        self.by_start
            .range((at.saturating_sub(LONG), 0)..=(at, u64::MAX))
            .rev()
            .any(|((_, id), ())| holds(id))
            || self.long.iter().any(holds)
    }

    /// The live entries that hold `at`, the one that starts last first.
    pub(super) fn holders(&self, at: u64) -> Vec<&Entry> {
        let from = at.saturating_sub(LONGEST);
        self.by_start
            .range((from, 0)..=(at, u64::MAX))
            .rev()
            .filter_map(|((_, id), ())| self.live.get(id))
            .filter(|e| at - e.start < e.size)
            .take(8)
            .collect()
    }

    /// The live entry that starts last at or below `at`, and the next above.
    pub(super) fn neighbours(&self, at: u64) -> [Option<&Entry>; 2] {
        let below = self.by_start.range(..=(at, u64::MAX)).next_back();
        let above = self.by_start.range((at + 1, 0)..).next();
        [below, above].map(|e| e.and_then(|((_, id), ())| self.live.get(id)))
    }

    /// What held `at` and is gone, newest first.
    pub(super) fn gone_at(&self, at: u64, most: usize) -> Vec<&(Entry, u64)> {
        self.gone.iter().rev().filter(|(e, _)| at >= e.start && at - e.start < e.size).take(most).collect()
    }
}

/// `resource` was made; `what` it is (a name given later replaces it).
pub(crate) fn made(resource: &ID3D12Resource, what: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    let Ok(object) = resource.cast::<ID3D12Object>() else { return };
    let raw = object.as_raw() as usize;
    if with(|b| b.by_object.contains_key(&raw)) {
        return named(&object, what);
    }
    // SAFETY: a live resource.
    let desc = unsafe { resource.GetDesc() };
    let (start, size) = if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
        // SAFETY: as above.
        (Some(unsafe { resource.GetGPUVirtualAddress() }), desc.Width)
    } else {
        let size = crate::backend::made_host()
            // SAFETY: a plain query.
            .map_or(0, |h| unsafe { h.device.GetResourceAllocationInfo(0, &[desc]) }.SizeInBytes);
        (texture_address(resource, &desc), if size == u64::MAX { 0 } else { size })
    };
    let Some(start) = start.filter(|&s| s != 0) else {
        return with(|b| *b.unknown.entry(format!("f{} flags {:#x}", desc.Format.0, desc.Flags.0)).or_default() += 1);
    };
    let what = what();
    let now = crate::backend::now_ms();
    let id = with(|b| b.add(raw, start, size.max(1), what, now));
    watch(&object, id);
}

/// A block of texture memory, found by the texture `resource` at `offset` of it.
pub(crate) fn block(heap: &ID3D12Heap, resource: &ID3D12Resource, offset: u64, bytes: u64) {
    if !on() {
        return;
    }
    let (Ok(heap), Ok(object)) = (heap.cast::<ID3D12Object>(), resource.cast::<ID3D12Object>()) else {
        return;
    };
    let raw = heap.as_raw() as usize;
    let now = crate::backend::now_ms();
    let id = with(|b| {
        let at = b.by_object.get(&(object.as_raw() as usize)).and_then(|id| b.live.get(id))?.start;
        // Known, unless the entry is of what had the pointer before.
        if b.by_object
            .get(&raw)
            .and_then(|id| b.live.get(id))
            .is_some_and(|e| at >= e.start && at - e.start < e.size)
        {
            return None;
        }
        Some(b.add(raw, at.checked_sub(offset)?, bytes, "a block of textures".into(), now))
    });
    if let Some(id) = id {
        watch(&heap, id);
    }
}

/// Something to say of the object at `raw` if it is in the book.
pub(crate) fn note(raw: usize, what: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    let what = what();
    with(|b| {
        if let Some(id) = b.by_object.get(&raw).copied()
            && let Some(e) = b.live.get_mut(&id)
            && e.notes.len() < 600
        {
            e.notes.push_str(&what);
        }
    });
}

/// A name for something made before.
pub(crate) fn named(object: &ID3D12Object, what: impl FnOnce() -> String) {
    let raw = object.as_raw() as usize;
    if with(|b| b.by_object.contains_key(&raw)) {
        let what = what();
        with(|b| {
            if let Some(id) = b.by_object.get(&raw).copied()
                && let Some(e) = b.live.get_mut(&id)
            {
                e.what = what;
            }
        });
    }
}

/// A root descriptor was set by address.
pub(crate) fn root(gpu: u64, bytes: u64) {
    if on() {
        let mut roots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
        if roots.len() >= 16_384 {
            roots.pop_front();
        }
        roots.push_back((crate::backend::now_ms(), gpu, bytes));
    }
}
