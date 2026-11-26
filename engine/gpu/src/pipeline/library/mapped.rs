use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::{LIBRARY_FILE, NEXT_FILE};

/// Whether the library's file is mapped ([`Blob`]) and saved through a
/// mapping. On by default; `OATHKEEP_LIBRARY_MAP=0` uses buffers, as before.
pub(super) fn mapped() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LIBRARY_MAP").is_none_or(|v| v != "0"))
}

/// Whether the mapped library file's handle is kept open while it is
/// mapped (see the module's notes: without it a save in the same run turns
/// the mapping's pages into zeros as they leave memory). On by default;
/// `OATHKEEP_LIBRARY_HANDLE=0` closes it after mapping, as before.
fn holds() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LIBRARY_HANDLE").is_none_or(|v| v != "0"))
}

/// A whole file mapped into memory: read-only (the library's file) or
/// writable (the file a save fills). Unmapped when dropped.
pub(super) struct Mapped {
    pub(super) at: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    pub(super) len: usize,
    /// The file, for a mapping made by [`Mapped::open`] ([`holds`]): open
    /// until after the view is unmapped, so that the file a rename took
    /// the name from keeps its contents for the view.
    _file: Option<std::fs::File>,
}

impl Mapped {
    /// Maps `file` (`len` bytes, not zero) for reading, or for reading and
    /// writing.
    fn map(file: &std::fs::File, len: usize, write: bool) -> Option<Mapped> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READONLY, PAGE_READWRITE,
        };
        if len == 0 {
            return None;
        }
        // SAFETY: a file we hold open; the view keeps the file's pages
        // after the mapping object and the file are closed.
        let at = unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                if write { PAGE_READWRITE } else { PAGE_READONLY },
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return None;
            }
            let at = MapViewOfFile(mapping, if write { FILE_MAP_WRITE } else { FILE_MAP_READ }, 0, 0, 0);
            CloseHandle(mapping);
            at
        };
        (!at.Value.is_null()).then_some(Mapped { at, len, _file: None })
    }

    /// The file at `path`, mapped for reading; `None` if it is not there,
    /// is empty or cannot be mapped.
    fn open(path: &std::path::Path) -> Option<Mapped> {
        Mapped::open_holding(path, holds())
    }

    /// As [`Mapped::open`]; `hold` says whether the file stays open with
    /// the mapping.
    pub(super) fn open_holding(path: &std::path::Path, hold: bool) -> Option<Mapped> {
        // Opened as the standard library opens a file to read: others may
        // read, write, rename and delete it meanwhile, so a save still
        // renames the new library over this one.
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        let mut mapped = Mapped::map(&file, len, false)?;
        if hold {
            mapped._file = Some(file);
        }
        Some(mapped)
    }

    /// A new file of `len` bytes at `path` (replacing what is there),
    /// mapped for writing.
    pub(super) fn create(path: &std::path::Path, len: usize) -> Option<Mapped> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .ok()?;
        file.set_len(len as u64).ok()?;
        Mapped::map(&file, len, true)
    }

    pub(super) fn bytes(&self) -> &[u8] {
        // SAFETY: the view holds `len` readable bytes until dropped.
        unsafe { std::slice::from_raw_parts(self.at.Value.cast(), self.len) }
    }

    /// Only for a view made by [`Mapped::create`].
    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: a writable view of `len` bytes, ours alone.
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: our view.
        unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.at) };
    }
}

/// The bytes a library is made from: the host reads them for as long as
/// the library lives, so they are kept beside it.
pub(super) enum Blob {
    /// No file (an empty library).
    None,
    /// The file read into memory (`OATHKEEP_LIBRARY_MAP=0`, tests, or a file
    /// that could not be mapped).
    Held(Vec<u8>),
    /// The file itself, mapped.
    Mapped(Mapped),
}

impl Blob {
    /// The library file in `dir`: a library a save left beside it
    /// ([`NEXT_FILE`]) takes its place first.
    pub(super) fn of(dir: &std::path::Path) -> Blob {
        let path = dir.join(LIBRARY_FILE);
        let next = dir.join(NEXT_FILE);
        if next.exists() && std::fs::rename(&next, &path).is_err() {
            tracing::warn!("the host pipeline library saved last could not take the old one's place");
        }
        if mapped()
            && let Some(file) = Mapped::open(&path)
        {
            return Blob::Mapped(file);
        }
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => Blob::Held(bytes),
            _ => Blob::None,
        }
    }

    pub(super) fn bytes(&self) -> &[u8] {
        match self {
            Blob::None => &[],
            Blob::Held(bytes) => bytes,
            Blob::Mapped(file) => file.bytes(),
        }
    }
}

/// The mapped library file's view (address, bytes; 0: not mapped), when a
/// pipeline was last loaded from the library (`crate::host::now_ms`), and
/// whether the view's pages were let go since ([`trim`]).
pub(super) static VIEW: [std::sync::atomic::AtomicUsize; 2] = [const { std::sync::atomic::AtomicUsize::new(0) }; 2];
pub(super) static LOADED_AT: AtomicU64 = AtomicU64::new(0);
pub(super) static TRIMMED: AtomicBool = AtomicBool::new(false);
/// The bytes of the view let go so far, for the memory line.
pub(crate) static TRIMMED_BYTES: AtomicU64 = AtomicU64::new(0);
/// When the view's pages were last let go (`crate::host::now_ms`), and how
/// often and when a save of this run last renamed a new library over the
/// mapped file: for [`removal_note`].
static TRIMMED_AT: AtomicU64 = AtomicU64::new(0);
pub(super) static REPLACED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The library calls running now: thread, what (load, make, store,
/// serialise), the pipeline, and since when (`crate::host::now_us`).
static CALLS: Mutex<Vec<(u32, &'static str, String, u64)>> = Mutex::new(Vec::new());

thread_local! {
    /// The pipeline this thread is making (its shaders), for [`CALLS`].
    pub(super) static LABEL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Names the pipeline this thread makes next.
pub(crate) fn label(text: String) {
    LABEL.with(|l| *l.borrow_mut() = text);
}

/// A library call of this thread, listed in [`CALLS`] until dropped.
pub(super) struct Call(u32, u64);

pub(super) fn call(what: &'static str, name: &str) -> Call {
    let (thread, since) = (crate::host::thread_id(), crate::host::now_us());
    let label = LABEL.with(|l| l.borrow().clone());
    CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((thread, what, format!("{name} {label}"), since));
    Call(thread, since)
}

impl Drop for Call {
    fn drop(&mut self) {
        CALLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| (c.0, c.3) != (self.0, self.1));
    }
}

/// For the report when the host device is removed: the library calls
/// running at that moment, whether this run replaced the mapped file and
/// let its pages go, and how many pages of the mapping read as zeros now.
/// A library holds no page of zeros (the 259 MB one of run 83 has no run of
/// 2,048 zero bytes): each such page is one whose contents were lost (see
/// the module's notes), and a pipeline stored there is what the driver was
/// handed.
pub(crate) fn removal_note() -> String {
    use std::fmt::Write;
    let now = crate::host::now_us();
    let mut text = String::from("pipeline library: ");
    {
        let calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        if calls.is_empty() {
            text.push_str("no call running");
        }
        for (i, (thread, what, name, since)) in calls.iter().enumerate() {
            let _ = write!(
                text,
                "{}{what} of {name} running for {:.1} ms on thread {thread}",
                if i == 0 { "" } else { ", " },
                now.saturating_sub(*since) as f64 / 1000.0
            );
        }
    }
    let (at, len) = (VIEW[0].load(Ordering::Relaxed), VIEW[1].load(Ordering::Relaxed));
    if at == 0 {
        text.push_str("; its file is not mapped");
        return text;
    }
    let ago = |ms: u64| crate::host::now_ms().saturating_sub(ms) as f64 / 1000.0;
    let _ = write!(
        text,
        "; its file is mapped ({len} bytes), the handle {}",
        if holds() { "kept open" } else { "closed (OATHKEEP_LIBRARY_HANDLE=0)" }
    );
    match REPLACED[0].load(Ordering::Relaxed) {
        0 => text.push_str("; this run has not replaced the file"),
        n => {
            let _ = write!(
                text,
                "; this run replaced the file under the mapping {n} times, last {:.1} s ago",
                ago(REPLACED[1].load(Ordering::Relaxed))
            );
        }
    }
    if TRIMMED_BYTES.load(Ordering::Relaxed) > 0 {
        let _ = write!(text, "; its pages were last let go {:.1} s ago", ago(TRIMMED_AT.load(Ordering::Relaxed)));
    }
    // Within two seconds: pages not in memory come from the disk.
    let started = std::time::Instant::now();
    // SAFETY: the view lives as long as the library, which is never dropped.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
    let (mut zero, mut looked, mut first) = (0u64, 0u64, None);
    for (n, page) in bytes.chunks(4096).enumerate() {
        if n % 4096 == 0 && started.elapsed() > std::time::Duration::from_secs(2) {
            break;
        }
        looked += 1;
        if page.iter().all(|&b| b == 0) {
            zero += 1;
            if first.is_none() {
                first = Some(n);
            }
        }
    }
    let _ = write!(text, "; {zero} of the {looked} pages looked at read as zeros now");
    if let Some(first) = first {
        let _ = write!(text, " (the first is page {first}): the library's contents were lost there");
    }
    text
}

/// Lets the mapped library file's pages go from the process's memory once
/// no pipeline has been loaded for ten seconds. The host reads nearly the
/// whole file while the early pipelines load (run 65: all 1,490 MB of it
/// were in the process's memory a minute in, and 1,300 MB still at the
/// end, on a PC with 2 GB free); after that it reads the file again only
/// when a new pipeline loads. Pages let go stay in Windows' file cache
/// while nothing else wants the memory, and come back on the next read.
/// Called once a second by the sweep; does its work on a thread of its own
/// (hundreds of thousands of pages), once after each burst of loads.
/// `OATHKEEP_LIBRARY_TRIM=0`: the pages stay until Windows takes them.
pub(crate) fn trim() {
    static ON: OnceLock<bool> = OnceLock::new();
    let (at, len) = (VIEW[0].load(Ordering::Relaxed), VIEW[1].load(Ordering::Relaxed));
    if at == 0
        || TRIMMED.load(Ordering::Relaxed)
        || !*ON.get_or_init(|| std::env::var_os("OATHKEEP_LIBRARY_TRIM").is_none_or(|v| v != "0"))
    {
        return;
    }
    let quiet_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    if crate::host::now_ms() < LOADED_AT.load(Ordering::Relaxed) + quiet_ms
        || TRIMMED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    let work = move || {
        // SAFETY: the view lives as long as the library, which is never
        // dropped. Unlocking pages that are not locked fails, as expected,
        // and takes them out of the working set: the documented way to let
        // pages of a mapped file go.
        unsafe { windows_sys::Win32::System::Memory::VirtualUnlock(at as *const _, len) };
        TRIMMED_BYTES.fetch_add(len as u64, Ordering::Relaxed);
        TRIMMED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
        crate::host::trail(|| format!("pipeline library: the mapped file's pages let go from memory ({len} bytes)"));
    };
    if std::thread::Builder::new().name("victory-library-trim".into()).spawn(work).is_err() {
        TRIMMED.store(false, Ordering::Relaxed);
    }
}
