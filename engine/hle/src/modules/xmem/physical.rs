use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE, SEC_COMMIT, SEC_RESERVE,
};
use windows_sys::core::BOOL;

use super::mapping::{Call, counted, lean};
use super::on_wine;
use super::stats::commit;

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

pub(super) const PAGE: usize = 64 << 10;

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

pub(super) struct Section {
    pub(super) handle: HANDLE,
    pages: usize,
    /// A view of the whole section, kept ([`Section::window`]); 0 if the
    /// host gave none.
    window: std::sync::OnceLock<usize>,
    /// A bit per page: committed, as far as [`Section::window`] asked.
    committed: Box<[std::sync::atomic::AtomicU64]>,
}

impl Section {
    /// Where bytes of `n` pages from page `index` on can be written,
    /// whether or not the pages are mapped anywhere, committed.
    ///
    /// Every DirectStorage write mapped a view of its pages, committed it,
    /// faulted each of its 4 KiB pages in and unmapped it again, all of it
    /// under the process's one address-space lock, with 24 workers, the
    /// title's own mapping calls (about 100,000 views of one page) and our
    /// guards asking for the same lock. One view of the section, kept,
    /// costs that once per page.
    /// `OATHKEEP_SECTION_WINDOW=0`: a view for each write, as before.
    pub(super) fn window(&self, index: usize, n: usize) -> Option<usize> {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        static ON: LazyLock<bool> =
            LazyLock::new(|| std::env::var_os("OATHKEEP_SECTION_WINDOW").is_none_or(|v| v != "0"));
        if !*ON || index + n > self.pages {
            return None;
        }
        let base = *self.window.get_or_init(|| {
            // SAFETY: the whole of a section we own.
            unsafe { MapViewOfFile(self.handle, FILE_MAP_WRITE, 0, 0, self.pages * PAGE) }.Value as usize
        });
        if base == 0 {
            return None;
        }
        let missing = |page: usize| self.committed[page / 64].load(Ordering::Acquire) >> (page % 64) & 1 == 0;
        let mut page = index;
        while page < index + n {
            if !missing(page) {
                page += 1;
                continue;
            }
            let run = (page..index + n).take_while(|&p| missing(p)).count();
            if run == 0 {
                continue; // another worker committed it meanwhile
            }
            if !commit(base + page * PAGE, run * PAGE) {
                return None;
            }
            for p in page..page + run {
                self.committed[p / 64].fetch_or(1 << (p % 64), Ordering::AcqRel);
            }
            page += run;
        }
        Some(base + index * PAGE)
    }

    /// Commits the pages from `index` on that a view at `base` shows, unless
    /// all of them are ([`lean`]): committed for one view is for all.
    pub(super) fn commit_shown(&self, index: usize, base: usize, len: usize) -> bool {
        use std::sync::atomic::Ordering;
        let pages = index..(index + len / PAGE).min(self.pages);
        let bit = |p: usize| (&self.committed[p / 64], 1 << (p % 64));
        let done = || {
            pages.clone().all(|p| {
                let (word, bit) = bit(p);
                word.load(Ordering::Acquire) & bit != 0
            })
        };
        if lean() && done() {
            return true;
        }
        if !counted(Call::Commit, || commit(base, len)) {
            return false;
        }
        for p in pages {
            let (word, bit) = bit(p);
            word.fetch_or(bit, Ordering::AcqRel);
        }
        true
    }
}

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        if let Some(&base) = self.window.get()
            && base != 0
        {
            // SAFETY: our view; whoever writes through it holds the section.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: base as *mut c_void,
                })
            };
        }
        oathkeep_gpu::title_memory_section_gone(self.handle as usize);
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
pub(super) struct Slice {
    pub(super) section: Arc<Section>,
    pub(super) index: usize,
}

#[derive(Default)]
pub(super) struct Physical {
    /// Mapped views by address: their length and what they show.
    pub(super) views: BTreeMap<usize, (usize, Slice)>,
    /// Placeholders that are some view's own, left by its unmapping: address
    /// and length. The title may have joined one with others since.
    pub(super) holes: HashMap<usize, usize>,
}

impl Physical {
    /// What the page at `at` shows.
    pub(super) fn slice_at(&self, at: usize) -> Option<Slice> {
        self.views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
    }
}

/// The title's pages by number.
pub(super) struct Pages {
    next_page: usize,
    pub(super) pages: HashMap<usize, Slice>,
}

/// Where the title sees its pages: [`Physical::views`] the other way.
/// Locked apart from them, and only for a moment ([`PAGES`]).
#[derive(Default)]
pub(super) struct Shown {
    /// The addresses that show a page, by the page's section and index.
    /// Views of more than [`SHORT`] pages are in `long`.
    pages: HashMap<(usize, usize), Vec<usize>>,
    /// Address, bytes, section, first page.
    long: Vec<(usize, usize, usize, usize)>,
}

/// Pages of a view noted page by page in [`Shown::pages`].
const SHORT: usize = 64;

/// Taken after [`PHYSICAL`] where both are.
pub(super) static SHOWN: LazyLock<RwLock<Shown>> = LazyLock::new(Default::default);

pub(super) fn shown_mut() -> RwLockWriteGuard<'static, Shown> {
    SHOWN.write().unwrap_or_else(|e| e.into_inner())
}

impl Shown {
    pub(super) fn note(&mut self, base: usize, len: usize, slice: &Slice) {
        let section = Arc::as_ptr(&slice.section) as usize;
        if len / PAGE > SHORT {
            self.long.push((base, len, section, slice.index));
            return;
        }
        for k in 0..len / PAGE {
            self.pages.entry((section, slice.index + k)).or_default().push(base + k * PAGE);
        }
    }

    pub(super) fn forget(&mut self, base: usize, len: usize, slice: &Slice) {
        if len / PAGE > SHORT {
            self.long.retain(|view| view.0 != base);
            return;
        }
        let section = Arc::as_ptr(&slice.section) as usize;
        for k in 0..len / PAGE {
            let key = (section, slice.index + k);
            if let Some(at) = self.pages.get_mut(&key) {
                at.retain(|&at| at != base + k * PAGE);
                if at.is_empty() {
                    self.pages.remove(&key);
                }
            }
        }
    }

    /// Where the title sees the page at `slice`: addresses and bytes.
    pub(super) fn at(&self, slice: &Slice) -> Vec<(usize, usize)> {
        let section = Arc::as_ptr(&slice.section) as usize;
        let mut out: Vec<(usize, usize)> =
            self.pages.get(&(section, slice.index)).into_iter().flatten().map(|&at| (at, PAGE)).collect();
        for &(base, len, of, index) in &self.long {
            if of == section && (index..index + len / PAGE).contains(&slice.index) {
                out.push((base + (slice.index - index) * PAGE, PAGE));
            }
        }
        out
    }
}

/// The views, changed by the title's mapping calls; held for as long as
/// those take (the host maps, commits and unmaps under it).
static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(Default::default);

pub(super) fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The title's pages. Apart from the views: DirectStorage looks pages up at
/// every request, from 24 workers, and used to wait there for the title's
/// mapping calls (12,000 waits of 1 to 170 ms in a load of 90 s). Written
/// only where the title allocates or frees pages. Taken after [`PHYSICAL`]
/// where both are.
static PAGES: LazyLock<RwLock<Pages>> = LazyLock::new(|| {
    RwLock::new(Pages {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        pages: HashMap::new(),
    })
});

pub(super) fn page_table() -> RwLockReadGuard<'static, Pages> {
    PAGES.read().unwrap_or_else(|e| e.into_inner())
}

fn page_table_mut() -> RwLockWriteGuard<'static, Pages> {
    PAGES.write().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): the title asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    // For the GPU side's in-place census: only a committed section can be
    // opened as a host heap.
    oathkeep_gpu::title_memory_section(handle as usize, size as u64, on_wine());
    let section = Arc::new(Section {
        handle,
        pages: n,
        window: std::sync::OnceLock::new(),
        committed: (0..n.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
    });
    let mut table = page_table_mut();
    let first = table.next_page;
    table.next_page += n * FRAMES;
    for index in 0..n {
        let slice = Slice {
            section: section.clone(),
            index,
        };
        table.pages.insert(first + index * FRAMES, slice);
    }
    // Title memory is touched without our locks: a fault there may wait for
    // a mapping call, which takes them.
    drop(table);
    for index in 0..n {
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(first + index * FRAMES) };
    }
    1
}

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    // SAFETY: the title passes `n` page numbers.
    let numbers = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
    let mut table = page_table_mut();
    // Dropped without the lock: the last page of a section unmaps its window.
    let gone: Vec<Slice> = numbers.iter().filter_map(|number| table.pages.remove(number)).collect();
    drop(table);
    drop(gone);
    1
}
