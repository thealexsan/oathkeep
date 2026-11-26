use super::{View, view_at};
use super::flips::LATE;
use super::spans::STALE;

/// Writes of a slot with another view of the placed texture it showed,
/// since the start: the slot kept showing the old view.
pub(crate) static KEPT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a slot written with another view of the placed texture it
/// shows keeps showing the old one until the new is made;
/// `OATHKEEP_KEEP_VIEWS=0` makes it null at once, as before.
pub(super) fn keep_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_KEEP_VIEWS").is_none_or(|v| v != "0"))
}

/// What the texture view a slot is written with has to do with the one it
/// shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kin {
    /// A view of the same placed texture ([`same_placed_texture`]).
    Same,
    /// A texture of the same description placed elsewhere (moved, or one of
    /// its kind).
    Moved,
    /// One of the same format and shape in another size: the same texture
    /// with more or fewer levels, as a title streams it.
    Sized,
    /// A view of the same dimension, of another texture.
    Other,
    /// A view no shader that reads the one can read.
    None,
}

impl Kin {
    /// Whether the slot goes on showing what it shows, by [`keep_shown`].
    pub(super) fn kept(self, keep: u8) -> bool {
        match self {
            Kin::Same => true,
            Kin::Moved | Kin::Sized => keep >= 1,
            Kin::Other => keep >= 2,
            Kin::None => false,
        }
    }
}

/// [`Kin`] of the view `old`, which a slot shows, and `new`.
pub(super) fn kin(old: &View, new: &View) -> Kin {
    if same_placed_texture(old, new) {
        return Kin::Same;
    }
    if let (View::Placed(false, a, from), View::Placed(false, b, to)) = (old, new) {
        if a[0] == 1 || b[0] == 1 || from[1] != to[1] {
            return Kin::None;
        }
        // (Width, 64 bits, height, depth and levels, format: words 4 to 8.)
        if a[0] == b[0] && a[2..13] == b[2..13] {
            return Kin::Moved;
        }
        let (wide, high) = ([a[4], b[4]].map(u64::from), [a[6], b[6]].map(u64::from));
        return if from[0] == to[0] && a[0] == b[0] && wide[0] * high[1] == wide[1] * high[0] {
            Kin::Sized
        } else {
            Kin::Other
        };
    }
    let dimension = |view: &View| match view {
        View::Placed(false, resource, desc) if resource[0] != 1 => desc[1],
        View::Shader(r, Some(desc)) if crate::d3d12::resource::buffer_bytes(*r as crate::d3d12::object::This).is_none() => desc[1],
        _ => 0,
    };
    match (dimension(old), dimension(new)) {
        (0, _) | (_, 0) => Kin::None,
        (a, b) if a == b => Kin::Other,
        _ => Kin::None,
    }
}

/// Whether a slot that is written with a view of another texture goes on
/// showing the one it shows until the new one is made and its upload has run
/// (`OATHKEEP_KEEP_SHOWN`): 0: never, it reads as zeros, then as the neutral
/// texture; 1: one of the same kind in another place or size; 2: any of the
/// same dimension.
pub(super) fn keep_shown() -> u8 {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::diagnostics::ab_toggle::pick("OATHKEEP_KEEP_SHOWN") {
        return if on { 2 } else { 0 };
    }
    static KEEP: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *KEEP.get_or_init(|| std::env::var("OATHKEEP_KEEP_SHOWN").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1))
}

/// Slots that showed a texture and were written with a view of another,
/// since the start, by [`Kin`] (moved, sized, other, none), and those of
/// them that kept what they showed.
static REWRITTEN: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

pub(super) fn rewritten(kin: Kin, kept: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    let class = match kin {
        Kin::Moved | Kin::Same => 0,
        Kin::Sized => 1,
        Kin::Other => 2,
        Kin::None => 3,
    };
    REWRITTEN[class].fetch_add(1, Relaxed);
    REWRITTEN[4].fetch_add(u64::from(kept), Relaxed);
    let n: u64 = REWRITTEN[..4].iter().map(|n| n.load(Relaxed)).sum();
    if n.is_multiple_of(5000) {
        tracing::info!(
            "slots that showed a texture and were written with another, since the start: {} of the same kind elsewhere, {} of another size, {} others, {} of another dimension; {} of them kept what they showed meanwhile (OATHKEEP_KEEP_SHOWN)",
            REWRITTEN[0].load(Relaxed),
            REWRITTEN[1].load(Relaxed),
            REWRITTEN[2].load(Relaxed),
            REWRITTEN[3].load(Relaxed),
            REWRITTEN[4].load(Relaxed)
        );
    }
}

/// Diagnostics: whether a texture that a slot showed until its new one came
/// holds the new one's bytes (the title moved it), by the bytes their host
/// copies were filled from: equal, different, not known.
#[cfg(feature = "diagnostics")]
pub(super) fn same_bytes(old: u64, new: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    static SEEN: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
    let view = |key: u64| view_at(key as u32, (key >> 32) as u32);
    let (Some(old), Some(new)) = (view(old), view(new)) else {
        return;
    };
    let held = |view: &View| {
        let View::Placed(false, resource, desc) = view else {
            return None;
        };
        let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        crate::d3d12::resource::texture_held(crate::d3d12::resource::texture_at_made(address, resource)?)
    };
    let class = match kin(&old, &new) {
        Kin::Moved => 0,
        Kin::Same | Kin::None => return,
        _ => 3,
    };
    let which = match (held(&old), held(&new)) {
        (Some(a), Some(b)) if a == b => 0,
        (Some(a), Some(b)) if a.0 == b.0 => 1,
        _ => 2,
    };
    SEEN[class + which].fetch_add(1, Relaxed);
    let n: u64 = SEEN.iter().map(|n| n.load(Relaxed)).sum();
    if n.is_multiple_of(1000) {
        let seen: Vec<u64> = SEEN.iter().map(|n| n.load(Relaxed)).collect();
        tracing::info!(
            "slots that went on showing another texture until the new one came, since the start: of one of the same kind elsewhere {} had the new one's bytes, {} others, {} not known; of others {}, {}, {}",
            seen[0], seen[1], seen[2], seen[3], seen[4], seen[5]
        );
    }
}

/// For `crate::diagnostics::pop_watch`, since the start: slots made null as they were
/// written with another texture, such that kept the old one, slots given the
/// neutral texture, slots made null as their memory was another's.
#[cfg(feature = "diagnostics")]
pub(crate) fn pop_causes() -> [u64; 4] {
    use std::sync::atomic::Ordering::Relaxed;
    let all: u64 = REWRITTEN[..4].iter().map(|n| n.load(Relaxed)).sum();
    let kept = REWRITTEN[4].load(Relaxed);
    [all.saturating_sub(kept), kept, LATE[3].load(Relaxed), STALE[0].load(Relaxed)]
}

/// Whether a slot already made from its record is told so without the lock
/// (`OATHKEEP_QUICK_MIRROR=0`: with it, as before).
pub(super) fn quick_mirror() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_QUICK_MIRROR").is_none_or(|v| v != "0"))
}

/// The records last seen of a heap, held ([`HostHeap::copied_begin`]).
pub(crate) struct Copied<'a> {
    pub(super) at: *mut u8,
    pub(super) len: usize,
    pub(super) step: usize,
    pub(super) _held: std::sync::MutexGuard<'a, Vec<u8>>,
}

// SAFETY: the bytes are reached through `changes` alone, whose callers take
// parts apart from one another; the guard stays with the thread that has it.
unsafe impl Sync for Copied<'_> {}

impl Copied<'_> {
    /// The slots of `records` (as the title has them now, from slot `first`
    /// on) that are not the ones last seen, which they are from here on.
    /// (`records`: as the caller read them, through the kernel, which fails on
    /// a page the title unmapped meanwhile where a read in place would fault:
    /// the title unmaps heap pages too.)
    ///
    /// # Safety
    /// No other call beside this one has any of these slots.
    pub(crate) unsafe fn changes(&self, first: u32, records: &[u8], out: &mut Vec<u32>) {
        let at = first as usize * self.step;
        let end = (at + records.len()).min(self.len);
        if at >= end {
            return;
        }
        // SAFETY: within the records held; the caller's slots alone.
        let last = unsafe { std::slice::from_raw_parts_mut(self.at.add(at), end - at) };
        changed_records(last, &records[..end - at], self.step, first, out);
    }
}

/// The records (`step` bytes each, the first is slot `first`) that differ
/// between `last` and `records` (of one length) go into `out` and `last`.
/// Most of a changed page is as it was: it is looked at sixteen records at a
/// time.
pub(super) fn changed_records(last: &mut [u8], records: &[u8], step: usize, first: u32, out: &mut Vec<u32>) {
    const AT_A_TIME: usize = 16;
    let block = AT_A_TIME * step;
    for (b, (old, new)) in last.chunks_mut(block).zip(records.chunks(block)).enumerate() {
        if old == new {
            continue;
        }
        for (n, (old, new)) in old.chunks_exact_mut(step).zip(new.chunks_exact(step)).enumerate() {
            if old != new {
                old.copy_from_slice(new);
                out.push(first + (b * AT_A_TIME + n) as u32);
            }
        }
    }
}


/// Whether both are shader views of one texture the title placed in
/// memory: the same address and the same resource description, whatever
/// part of it each shows.
pub(super) fn same_placed_texture(a: &View, b: &View) -> bool {
    match (a, b) {
        (View::Placed(false, resource, desc), View::Placed(false, other, other_desc)) => {
            // (Words 1 and 13 are the filler of the description's layout, not
            // part of what a texture is: `crate::d3d12::resource::texture_at`.)
            let same = |from: usize, to: usize| resource[from..to] == other[from..to];
            resource[0] != 1 && resource[0] == other[0] && same(2, 13) && desc[10..12] == other_desc[10..12]
        }
        _ => false,
    }
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::d3d12::resource::texture_at(address, resource)?;
    crate::d3d12::resource::want_level(texture, crate::d3d12::resource::first_mip(desc));
    Some(View::Shader(texture as usize, desc[..10].try_into().ok()))
}

