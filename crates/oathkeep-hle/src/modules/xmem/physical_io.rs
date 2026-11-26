use std::sync::{Arc, LazyLock};

use windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS;

use super::physical::{PAGE, SHOWN, Section, Slice, page_table};
use super::stats::commit;

/// See [`pages_written`].
pub(crate) fn stream_notice() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("OATHKEEP_STREAM_NOTICE").is_none_or(|v| v != "0"));
    *ON
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
pub(super) struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    pub(super) fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not mapped for a write");
            return None;
        }
        let view = View { at, len };
        if !commit(at.Value as usize, len) {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not committed for a write");
            return None;
        }
        Some(view)
    }

    pub(super) fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table. `None`
/// if a page is unknown or `pages` ends early.
pub(super) fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let table = page_table();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let Some(slice) = table.pages.get(page) else {
            tracing::warn!(page, offset, len, "a physical page the title does not own (any more)");
            return None;
        };
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether the title owns every page under bytes `[offset, offset + len)`.
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    let table = page_table();
    pages
        .get(offset / PAGE..(offset + len).div_ceil(PAGE))
        .is_some_and(|pages| pages.iter().all(|page| table.pages.contains_key(page)))
}

/// Where the title sees the pages under bytes `[offset, offset + len)` now,
/// in order (for the streaming trace: whether that changed while a request
/// wrote them).
pub(crate) fn pages_shown(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let numbers = pages.get(offset / PAGE..(offset + len).div_ceil(PAGE)).unwrap_or_default();
    let slices: Vec<Slice> = {
        let table = page_table();
        numbers.iter().filter_map(|n| table.pages.get(n)).cloned().collect()
    };
    let shown = SHOWN.read().unwrap_or_else(|e| e.into_inner());
    let mut at: Vec<(usize, usize)> = slices.iter().flat_map(|slice| shown.at(slice)).collect();
    at.sort_unstable();
    at
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let k = (n * PAGE - within).min(data.len() - done);
        if let Some(at) = slice.section.window(slice.index, *n) {
            // SAFETY: committed bytes of the section's view, which `runs`
            // holds; the title's other uses of them race as on the console.
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), (at + within) as *mut u8, k) };
        } else {
            let Some(mut view) = View::map(slice, *n) else {
                return false;
            };
            view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        }
        (within, done) = (0, done + k);
    }
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: Window,
    within: usize,
    len: usize,
}

enum Window {
    /// In the section's own view, which the section holds.
    Kept(usize, #[expect(dead_code, reason = "held for the view")] Arc<Section>),
    Own(View),
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        match &mut self.view {
            // SAFETY: committed bytes of the section's view, ours alone as
            // far as the runtime goes (other views alias them, as the
            // console's do).
            Window::Kept(at, _) => unsafe { std::slice::from_raw_parts_mut((*at + within) as *mut u8, len) },
            Window::Own(view) => &mut view.bytes()[within..within + len],
        }
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    let view = match slice.section.window(slice.index, *n) {
        Some(at) => Window::Kept(at, slice.section.clone()),
        None => Window::Own(View::map(slice, *n)?),
    };
    Some(PageWindow {
        view,
        within: offset % PAGE,
        len,
    })
}

/// The title addresses that show bytes `[offset, offset + len)` of `pages`
/// now, byte for byte, as runs (address, bytes) in address order: a write
/// to the pages through a view of our own ([`page_window`],
/// [`write_physical`]) lands there too, and nothing that watches those
/// addresses sees it. Empty if no view shows them. Byte for byte, not by
/// whole pages: the title packs several small textures into one page and
/// fills each with a request of its own.
pub(crate) fn shown_at(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(len) else {
        return Vec::new();
    };
    let first = offset / PAGE;
    let Some(touched) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let slices: Vec<Option<Slice>> = {
        let table = page_table();
        touched.iter().map(|page| table.pages.get(page).cloned()).collect()
    };
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    {
        let shown = SHOWN.read().unwrap_or_else(|e| e.into_inner());
        for (i, slice) in slices.iter().enumerate() {
            let Some(slice) = slice else { continue };
            // The bytes of this page that the write covers.
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            pieces.extend(shown.at(slice).into_iter().map(|(at, _)| (at + lo, hi - lo)));
        }
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, bytes) in pieces {
        match runs.last_mut() {
            Some((start, n)) if *start + *n == at => *n += bytes,
            _ => runs.push((at, bytes)),
        }
    }
    runs
}

/// DirectStorage has put bytes `[offset, offset + len)` of `pages` into
/// memory, through a view of our own: the GPU side is told where the title
/// sees them, so that what it copied there before they arrived (a texture
/// uploaded when its view was written holds whatever the pages held then)
/// is copied again. Call when the bytes are there.
/// `OATHKEEP_STREAM_NOTICE=0`: not told; what is then done is
/// `OATHKEEP_PAGE_WRITES`' to say (`oathkeep_gpu::title_memory_written`).
pub(crate) fn pages_written(pages: &[usize], offset: usize, len: usize) {
    if len == 0 {
        return;
    }
    for (at, bytes) in shown_at(pages, offset, len) {
        if stream_notice() {
            oathkeep_gpu::kernel_will_write(at, bytes);
        } else {
            oathkeep_gpu::title_memory_written(at, bytes, true);
        }
    }
}
