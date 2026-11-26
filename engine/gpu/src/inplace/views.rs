// The title's views of its physical pages, as the runtime maps them: the
// census needs them to say which view memory is one run of a section (one
// placed buffer could show it) and which is scattered pages.

use std::collections::BTreeMap;
use std::ffi::c_void;

use windows_sys::Win32::System::Memory as mem;

use super::K64;

pub(super) struct Views {
    /// By address: bytes, the section's handle, the offset in it.
    pub(super) views: BTreeMap<usize, (usize, usize, u64)>,
    /// By section and byte offset of a 64 KiB page: the address of that
    /// page in every view that shows it. A write through one view of a
    /// page is a write of the bytes every other view shows
    /// (`crate::watch` guards them all).
    pages: BTreeMap<(usize, u64), Vec<usize>>,
    /// Of those, how many are shown more than once.
    pages_twice: usize,
    /// Sections by handle: bytes, and whether made committed.
    pub(super) sections: Vec<(usize, u64, bool)>,
    /// Our own view of a whole section, by its handle (0: the host gave
    /// none), made when first asked for ([`beside`]) and kept.
    whole: Vec<(usize, usize)>,
}

/// Read by many threads at once (write faults, landings), changed when the
/// runtime maps or unmaps.
static VIEWS: std::sync::RwLock<Views> = std::sync::RwLock::new(Views {
    views: BTreeMap::new(),
    pages: BTreeMap::new(),
    pages_twice: 0,
    sections: Vec::new(),
    whole: Vec::new(),
});

pub(super) fn views() -> std::sync::RwLockReadGuard<'static, Views> {
    VIEWS.read().unwrap_or_else(|e| e.into_inner())
}

fn views_mut() -> std::sync::RwLockWriteGuard<'static, Views> {
    VIEWS.write().unwrap_or_else(|e| e.into_inner())
}

/// The runtime made a section of `bytes` for physical pages; `committed`
/// if with `SEC_COMMIT` (only such a section can be opened as a heap).
pub fn title_memory_section(section: usize, bytes: u64, committed: bool) {
    let mut v = views_mut();
    v.sections.retain(|s| s.0 != section);
    // A handle's number given out again: the view was of the section that
    // had it before (it stays mapped: someone may be writing through it).
    v.whole.retain(|w| w.0 != section);
    v.sections.push((section, bytes, committed));
}

/// The runtime lets go of a section, and of its handle: no view the title
/// sees is of it any more, and [`beside`]'s view goes.
pub fn title_memory_section_gone(section: usize) {
    let whole = {
        let mut v = views_mut();
        let at = v.whole.iter().position(|w| w.0 == section);
        at.map(|i| v.whole.swap_remove(i).1)
    };
    if let Some(base) = whole.filter(|&base| base != 0) {
        // SAFETY: a view of ours. Who writes through it found the section
        // in a view the title saw and holds off its unmapping
        // (`crate::watch::title_memory_reading`), which came before this.
        unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut c_void }) };
    }
}

/// The runtime mapped `[at, at + len)` as a view of `section` from
/// `offset` on. Call after [`crate::title_memory_remapped`] of the range.
pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    {
        let mut v = views_mut();
        v.views.insert(at, (len, section, offset));
        for k in 0..len.div_ceil(K64) {
            let shown = v.pages.entry((section, offset + (k * K64) as u64)).or_default();
            shown.push(at + k * K64);
            let twice = shown.len() == 2;
            v.pages_twice += usize::from(twice);
        }
    }
    // This view is writable from now on: a guard over the same bytes at
    // another view must look at them again, and the pages are noted written
    // there (`crate::watch`).
    crate::watch::views_mapped(section, offset, len);
}

/// What `[at, at + len)` showed is gone ([`crate::title_memory_remapped`]):
/// the views that touch it are forgotten (the runtime tells again those it
/// maps anew).
pub(crate) fn views_gone(at: usize, len: usize) {
    let mut v = views_mut();
    if v.views.is_empty() {
        return;
    }
    let end = at.saturating_add(len);
    let first = v.views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at).map(|(base, _)| *base);
    let gone: Vec<usize> = first.into_iter().chain(v.views.range(at.saturating_add(1)..end).map(|(base, _)| *base)).collect();
    for base in gone {
        let Some((bytes, section, offset)) = v.views.remove(&base) else {
            continue;
        };
        for k in 0..bytes.div_ceil(K64) {
            let key = (section, offset + (k * K64) as u64);
            if let Some(pages) = v.pages.get_mut(&key) {
                let page = base + k * K64;
                let before = pages.len();
                pages.retain(|&shown| shown != page);
                let after = pages.len();
                if after == 0 {
                    v.pages.remove(&key);
                }
                v.pages_twice = v.pages_twice.saturating_sub(usize::from(before >= 2 && after < 2));
            }
        }
    }
}

/// The 64 KiB pages of `[start, end)` that the title shows at other
/// addresses too, in address order, each with those; not the ones it shows
/// in very many views (`crate::watch::MANY_VIEWS`).
pub(crate) fn shown_elsewhere(start: usize, end: usize) -> Vec<(usize, Vec<usize>)> {
    let v = views();
    let mut out = Vec::new();
    if start >= end || v.pages_twice == 0 {
        return out;
    }
    let first = v.views.range(..=start).next_back().filter(|(base, view)| *base + view.0 > start).map(|(base, _)| *base);
    for (&base, &(bytes, section, offset)) in v.views.range(first.unwrap_or(start)..end) {
        let from = start.max(base) & !(K64 - 1);
        for page in (from..end.min(base + bytes)).step_by(K64) {
            let Some(shown) = v.pages.get(&(section, offset + (page - base) as u64)) else {
                continue;
            };
            if (2..=crate::watch::MANY_VIEWS).contains(&shown.len()) {
                out.push((page, shown.iter().copied().filter(|&at| at != page).collect()));
            }
        }
    }
    out
}

/// The address of the 64 KiB page shown at `page` in every title view that
/// shows it, `page`'s own address included; empty if `page` is not a view.
pub(crate) fn pages_of(page: usize) -> Vec<usize> {
    let v = views();
    let Some((&base, &(bytes, section, offset))) = v.views.range(..=page).next_back() else {
        return Vec::new();
    };
    let within = page.wrapping_sub(base);
    if within >= bytes || !within.is_multiple_of(K64) {
        return Vec::new();
    }
    v.pages.get(&(section, offset + within as u64)).cloned().unwrap_or_default()
}

/// The title views that show any of the physical bytes `[offset, offset +
/// len)` of `section`, as `(address, bytes)` ranges in address order; not
/// those of a page shown in very many views (`MANY_VIEWS`).
pub(crate) fn views_of_section(section: usize, offset: u64, len: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }
    let v = views();
    let end = offset.saturating_add(len as u64);
    let mut pages: Vec<usize> = Vec::new();
    let mut at = offset - offset % K64 as u64;
    while at < end {
        if let Some(shown) = v.pages.get(&(section, at))
            && shown.len() <= crate::watch::MANY_VIEWS
        {
            pages.extend(shown.iter().copied());
        }
        match at.checked_add(K64 as u64) {
            Some(next) => at = next,
            None => break,
        }
    }
    pages.sort_unstable();
    pages.dedup();
    let mut runs: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
    for page in pages {
        match runs.last_mut() {
            Some((first, bytes)) if *first + *bytes == page => *bytes += K64,
            _ => runs.push((page, K64)),
        }
    }
    runs
}

/// How many bytes of `[at, at + len)` views of physical pages show; `None`
/// if no view touches it (nothing is mapped there, or memory of another
/// kind). The runtime's own record of what it mapped: no question to the
/// host.
pub(crate) fn shown_bytes(at: usize, len: usize) -> Option<u64> {
    let end = at.saturating_add(len);
    let v = views();
    let first = v.views.range(..=at).next_back();
    let rest = v.views.range(at.saturating_add(1)..end.max(at.saturating_add(1)));
    let (mut there, mut any) = (0u64, false);
    for (&base, view) in first.into_iter().chain(rest) {
        let (from, to) = (base.max(at), base.saturating_add(view.0).min(end));
        if from < to {
            there += (to - from) as u64;
            any = true;
        }
    }
    any.then_some(there)
}

/// What of `[at, at + len)` views of physical pages show, in order, pieces
/// that touch as one: (address, bytes). Empty: nothing, or memory of another
/// kind.
pub(crate) fn shown_runs(at: usize, len: usize) -> Vec<(usize, usize)> {
    let end = at.saturating_add(len);
    let v = views();
    let first = v.views.range(..=at).next_back();
    let rest = v.views.range(at.saturating_add(1)..end.max(at.saturating_add(1)));
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (&base, view) in first.into_iter().chain(rest) {
        let (from, to) = (base.max(at), base.saturating_add(view.0).min(end));
        if from >= to {
            continue;
        }
        match runs.last_mut() {
            Some((run, bytes)) if *run + *bytes == from => *bytes += to - from,
            _ => runs.push((from, to - from)),
        }
    }
    runs
}

/// How the views cover `[at, at + len)`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Cover {
    /// No view holds its first byte.
    None,
    /// Views hold a part of it only.
    Holes,
    /// Views hold all of it, from more than one place.
    Scattered,
    /// All of it is one run of one section: the section and the offset of
    /// `at` in it.
    Run(usize, u64),
}

pub(super) fn cover_in(views: &BTreeMap<usize, (usize, usize, u64)>, at: usize, len: usize) -> Cover {
    let end = at + len.max(1);
    let Some((&base, &(bytes, section, offset))) = views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at) else {
        return Cover::None;
    };
    let first = (section, offset + (at - base) as u64);
    let (mut reached, mut one) = (base + bytes, true);
    while reached < end {
        let Some(&(bytes, s, o)) = views.get(&reached) else {
            return Cover::Holes;
        };
        one &= s == first.0 && o == first.1 + (reached - at) as u64;
        reached += bytes;
    }
    if one { Cover::Run(first.0, first.1) } else { Cover::Scattered }
}

/// [`Cover`] of `[at, at + len)` by the title's views as they are now.
pub(crate) fn cover(at: usize, len: usize) -> Cover {
    cover_in(&views().views, at, len)
}

/// Another address of the title memory `[at, at + len)`, if it is a view of
/// the title's pages, all of it one run of a section: in a view of ours of
/// the whole section, which is never guarded (`crate::watch`) nor changed.
///
/// The section's pages are reserved and the runtime commits each before it
/// shows it, which holds for every view of the section. The caller holds
/// `crate::watch::title_memory_reading`: until it lets go the title sees
/// the same pages at `at`.
pub(crate) fn beside(at: usize, len: usize) -> Option<usize> {
    let known = {
        let v = views();
        let Cover::Run(section, offset) = cover_in(&v.views, at, len) else {
            return None;
        };
        v.whole.iter().find(|w| w.0 == section).map(|w| (w.1, offset))
    };
    if let Some((base, offset)) = known {
        return (base != 0).then(|| base + offset as usize);
    }
    let mut v = views_mut();
    let Cover::Run(section, offset) = cover_in(&v.views, at, len) else {
        return None;
    };
    let base = match v.whole.iter().find(|w| w.0 == section) {
        Some(w) => w.1,
        None => {
            let &(_, bytes, _) = v.sections.iter().find(|s| s.0 == section)?;
            // SAFETY: the whole of a section the runtime made; a view the
            // title sees is of it, so its handle is open.
            let base = unsafe { mem::MapViewOfFile(section as _, mem::FILE_MAP_WRITE, 0, 0, bytes as usize) }.Value as usize;
            if base == 0 {
                tracing::warn!(error = %std::io::Error::last_os_error(), "no view of the title's pages beside its own");
            }
            v.whole.push((section, base));
            base
        }
    };
    (base != 0).then(|| base + offset as usize)
}

/// Our own address of the 64 KiB page the title sees at `page` ([`beside`]),
/// and every address the title sees it at (0: no more); `None` if the page is
/// in no view of a section, or in very many. While the caller holds
/// `crate::watch::title_memory_reading`.
pub(crate) fn beside_page(page: usize) -> Option<(usize, [usize; 8])> {
    for asked in [false, true] {
        {
            let v = views();
            let (&base, &(bytes, section, offset)) = v.views.range(..=page).next_back()?;
            let within = page.wrapping_sub(base);
            if within >= bytes || !within.is_multiple_of(K64) {
                return None;
            }
            let offset = offset + within as u64;
            if let Some(whole) = v.whole.iter().find(|w| w.0 == section) {
                let mut shown = [0; 8];
                let all = v.pages.get(&(section, offset))?;
                shown.get_mut(..all.len())?.copy_from_slice(all);
                return (whole.1 != 0).then(|| (whole.1 + offset as usize, shown));
            }
        }
        if asked || beside(page, K64).is_none() {
            return None;
        }
    }
    None
}

/// Whether `[at, at + len)` is private memory, all committed and
/// read-write: what [`super::From::Part`] takes. One question to Windows (it
/// walks the page tables to the end of the pages that share the first
/// one's state: a millisecond in the title's big blocks, so ask when a
/// copy is made, not when it is used).
pub(crate) fn private_whole(at: usize, len: usize) -> bool {
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == mem::MEM_COMMIT
        && info.Type == mem::MEM_PRIVATE
        && info.Protect == mem::PAGE_READWRITE
        && (info.BaseAddress as usize).saturating_add(info.RegionSize) >= at.saturating_add(len)
}
