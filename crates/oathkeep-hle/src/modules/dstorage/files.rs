use std::os::windows::fs::FileExt;
use std::os::windows::io::FromRawHandle;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::modules::xgameruntime::{Methods, missing};

use super::{add_ref_instance, release};

// Files.

pub(super) struct File {
    pub(super) files: Files,
}

/// How files are read (`OATHKEEP_DSTORAGE_IO`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Io {
    /// `one`: all workers through the one handle, as before. The host does
    /// the reads of a handle one after the other.
    One,
    /// `handles`: a handle for each worker.
    Handles,
    /// `overlapped`: one handle, whose reads the host does side by side.
    Overlapped,
    /// `direct`: a handle for each worker, past the host's file cache.
    Direct,
}

impl Io {
    pub(super) fn chosen() -> Io {
        static CHOSEN: OnceLock<Io> = OnceLock::new();
        *CHOSEN.get_or_init(|| {
            let io = match std::env::var("OATHKEEP_DSTORAGE_IO").as_deref().map(str::trim) {
                Ok("one") => Io::One,
                Ok("handles") => Io::Handles,
                Ok("overlapped") => Io::Overlapped,
                Ok("direct") => Io::Direct,
                _ => Io::DEFAULT,
            };
            tracing::info!(?io, "dstorage: how files are read (OATHKEEP_DSTORAGE_IO)");
            io
        })
    }

    const DEFAULT: Io = Io::Handles;
}

/// A file as the workers read it: the handle the title opened, and those
/// made from it when a worker first reads.
pub(super) struct Files {
    first: std::fs::File,
    others: Box<[OnceLock<Option<std::fs::File>>]>,
    io: Io,
}

impl Files {
    pub(super) fn new(first: std::fs::File, io: Io) -> Files {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let others = match io {
            Io::One => 0,
            Io::Overlapped => 1,
            Io::Handles | Io::Direct => workers,
        };
        Files { first, others: (0..others).map(|_| OnceLock::new()).collect(), io }
    }

    #[cfg(test)]
    pub(super) fn open_for_test(path: &str, io: Io) -> Files {
        Files::new(std::fs::File::open(path).unwrap(), io)
    }

    /// Reads at `offset`; as [`FileExt::seek_read`].
    pub(super) fn read_at(&self, out: &mut [u8], offset: u64) -> std::io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED};
        let flags = match self.io {
            Io::One | Io::Handles => 0,
            Io::Overlapped => FILE_FLAG_OVERLAPPED,
            Io::Direct => FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING,
        };
        let other = match self.others.len() {
            0 => None,
            n => self.others[this_thread() % n].get_or_init(|| reopen(&self.first, flags)).as_ref(),
        };
        match (other, self.io) {
            (Some(file), Io::Overlapped) => read_overlapped(file, out, offset),
            (Some(file), Io::Direct) => read_direct(file, out, offset),
            (file, _) => file.unwrap_or(&self.first).seek_read(out, offset),
        }
    }
}

/// A number for this thread: 0 for the first that asks, and so on.
fn this_thread() -> usize {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    thread_local! {
        static NUMBER: usize = NEXT.fetch_add(1, Ordering::Relaxed) as usize;
    }
    NUMBER.with(|n| *n)
}

/// Another handle to `file`, for reading; `None` if the host gives none
/// (then the first one is used).
fn reopen(file: &std::fs::File, flags: u32) -> Option<std::fs::File> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, ReOpenFile};
    // SAFETY: a file we hold open.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), GENERIC_READ, FILE_SHARE_READ, flags) };
    if handle == INVALID_HANDLE_VALUE {
        tracing::warn!(error = %std::io::Error::last_os_error(), flags, "dstorage: a file could not be opened again");
        return None;
    }
    // SAFETY: a handle just opened, ours alone.
    Some(unsafe { std::fs::File::from_raw_handle(handle) })
}

/// A read through a handle opened for overlapped reads, waited for.
fn read_overlapped(file: &std::fs::File, out: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_HANDLE_EOF, ERROR_IO_PENDING, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::CreateEventW;
    /// Set when this thread's read is done.
    struct Event(HANDLE);
    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: our own event.
            unsafe { CloseHandle(self.0) };
        }
    }
    thread_local! {
        static DONE: Event =
            // SAFETY: a plain manual-reset event.
            Event(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) });
    }
    DONE.with(|done| {
        if done.0.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut overlapped = OVERLAPPED { hEvent: done.0, ..Default::default() };
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        let len = out.len().min(1 << 30) as u32;
        let mut n = 0;
        // SAFETY: `out` and `overlapped` stay until the read is done, which
        // is waited for.
        let mut read = unsafe {
            ReadFile(file.as_raw_handle(), out.as_mut_ptr(), len, std::ptr::null_mut(), &mut overlapped)
        } != 0;
        let mut error = std::io::Error::last_os_error();
        if read || error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            // SAFETY: as above.
            read = unsafe { GetOverlappedResult(file.as_raw_handle(), &overlapped, &mut n, 1) } != 0;
            error = std::io::Error::last_os_error();
        }
        match error.raw_os_error() {
            _ if read => Ok(n as usize),
            Some(code) if code == ERROR_HANDLE_EOF as i32 => Ok(0),
            _ => Err(error),
        }
    })
}

/// A read past the file cache: of the whole sectors around the bytes, into
/// a buffer of this thread's, unless `out` takes them as they are.
fn read_direct(file: &std::fs::File, out: &mut [u8], offset: u64) -> std::io::Result<usize> {
    /// Every disk's sectors are at most this; offsets, sizes and addresses
    /// are multiples of it.
    const SECTOR: usize = 4096;
    /// Read at a time.
    const MOST: usize = 1 << 20;
    let sector = SECTOR as u64;
    if offset % sector == 0 && out.len() % SECTOR == 0 && out.as_ptr() as usize % SECTOR == 0 {
        return read_overlapped(file, out, offset);
    }
    thread_local! {
        static BUFFER: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; MOST + SECTOR]);
    }
    BUFFER.with_borrow_mut(|buffer| {
        let skip = buffer.as_ptr().align_offset(SECTOR);
        let before = (offset % sector) as usize;
        let len = (before + out.len()).next_multiple_of(SECTOR).min(MOST);
        let aligned = &mut buffer[skip..skip + len];
        let read = read_overlapped(file, aligned, offset - before as u64)?;
        let n = read.saturating_sub(before).min(out.len());
        out[..n].copy_from_slice(&aligned[before..before + n]);
        Ok(n)
    })
}

pub(super) static FILE_VTABLE: Methods<8> = Methods([
    missing::<0> as *const (),
    add_ref_instance::<File> as *const (),
    release::<File> as *const (),
    missing::<3> as *const (),
    missing::<4> as *const (),
    missing::<5> as *const (),
    missing::<6> as *const (),
    missing::<7> as *const (),
]);
