use std::borrow::Cow;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use oathkeep_formats::bcpack::{self, Tables};

use crate::modules::xasync::{E_INVALIDARG, E_NOTIMPL};
use crate::modules::xgameruntime::HRESULT;

use super::files::File;
use super::{
    BCPACK, END_OF_FILE, INVALID_DATA, Instance, MEMORY_SOURCE, PAGE, PAGE_DESTINATION, Request,
    ZLIB, from_win32, text,
};

/// Reads, inflates and/or unpacks, and writes one request.
pub(super) fn execute(request: &Request) -> Result<(), HRESULT> {
    let options = request.options;
    let fail = |hr: HRESULT, why: &str| {
        tracing::warn!(
            options = format_args!("{options:#x}"),
            offset = request.offset,
            source_size = request.source_size,
            destination_size = request.destination_size,
            intermediate_size = request.intermediate_size,
            tag = request.tag,
            name = text(request.name),
            hr = format_args!("{:#010x}", hr as u32),
            "dstorage: request failed: {why}"
        );
        hr
    };
    if options & !(ZLIB | BCPACK | PAGE_DESTINATION | MEMORY_SOURCE) != 0 {
        return Err(fail(E_NOTIMPL, "unsupported options"));
    }
    if request.source.is_null() || request.destination.is_null() {
        return Err(fail(E_INVALIDARG, "no source or destination"));
    }
    let bc = ((options & BCPACK) >> 1) as u8;
    let size = request.destination_size as usize;
    if options & (ZLIB | BCPACK) == 0 {
        if request.source_size > request.destination_size {
            return Err(fail(E_INVALIDARG, "source larger than destination"));
        }
        let size = request.source_size as usize;
        return into_destination(request, size, |out| read_into(request, out).map(|()| size))
            .map_err(|hr| fail(hr, "copying"));
    }
    let tables = if bc != 0 { Some(bcpack_tables()?) } else { None };
    let source = read_source(request).map_err(|hr| fail(hr, "reading the source"))?;
    // The console's order: zlib first, then BCPack.
    let source = if bc != 0 && options & ZLIB != 0 {
        let mut packed = vec![0; request.intermediate_size as usize];
        let n = inflate(&source, &mut packed).map_err(|hr| fail(hr, "zlib"))?;
        packed.truncate(n);
        Cow::Owned(packed)
    } else {
        source
    };
    into_destination(request, size, |out| match tables {
        Some(tables) => bcpack::decode(bc, &source, out, tables)
            .map(|()| out.len())
            .map_err(|e| fail(INVALID_DATA, &e.to_string())),
        None => inflate(&source, out).map_err(|hr| fail(hr, "zlib")),
    })?;
    tracing::trace!(
        options = format_args!("{options:#x}"),
        offset = request.offset,
        size,
        "dstorage: request done"
    );
    Ok(())
}

/// Inflates a zlib stream into `out`, returning the bytes written. A stream
/// that ends short of `out` passes: the title's chunks fill it exactly, and
/// nothing says that is required.
pub(super) fn inflate(source: &[u8], out: &mut [u8]) -> Result<usize, HRESULT> {
    thread_local! {
        static INFLATE: std::cell::RefCell<zlib_rs::Inflate> =
            std::cell::RefCell::new(zlib_rs::Inflate::new(true, 15));
    }
    INFLATE.with_borrow_mut(|state| {
        state.reset(true);
        // `Inflate::decompress` cannot distinguish a valid empty stream from
        // an insufficient buffer when `avail_out == 0`, so use a 1-byte scratch
        // destination and verify `written <= out_len` afterward.
        let out_len = out.len();
        let mut scratch = [0u8; 1];
        let dest = if out_len == 0 { &mut scratch[..] } else { out };
        let (mut in_pos, mut out_pos) = (0usize, 0usize);
        let status = loop {
            match state.decompress(
                &source[in_pos..],
                &mut dest[out_pos..],
                zlib_rs::InflateFlush::NoFlush,
            ) {
                Ok(zlib_rs::Status::StreamEnd) => break Ok(zlib_rs::Status::StreamEnd),
                Ok(zlib_rs::Status::Ok) => {
                    let next_in = state.total_in() as usize;
                    let next_out = state.total_out() as usize;
                    if next_in == in_pos && next_out == out_pos {
                        break Ok(zlib_rs::Status::BufError);
                    }
                    in_pos = next_in;
                    out_pos = next_out;
                }
                other => break other,
            }
        };
        let written = state.total_out() as usize;
        match status {
            Ok(zlib_rs::Status::StreamEnd) if written <= out_len => Ok(written),
            other => {
                tracing::debug!(?other, "dstorage: zlib stream");
                Err(INVALID_DATA)
            }
        }
    })
}

/// The BCPack format's constant tables, parsed once for the process. If
/// they do not parse every BCPack request fails: one warning, then debug.
fn bcpack_tables() -> Result<&'static Tables, HRESULT> {
    static TABLES: OnceLock<Result<Tables, String>> = OnceLock::new();
    static WARNED: AtomicBool = AtomicBool::new(false);
    match TABLES.get_or_init(load_bcpack_tables) {
        Ok(tables) => Ok(tables),
        Err(why) if !WARNED.swap(true, Ordering::Relaxed) => {
            tracing::warn!("dstorage: BCPack requests fail: {why} (more at debug)");
            Err(E_NOTIMPL)
        }
        Err(_) => {
            tracing::debug!("dstorage: BCPack request failed: no tables");
            Err(E_NOTIMPL)
        }
    }
}

/// The tables Oathkeep ships with.
#[cfg(not(test))]
fn load_bcpack_tables() -> Result<Tables, String> {
    Tables::builtin().map_err(|e| format!("the built-in BCPack tables: {e}"))
}

/// Tests decode streams made against tables of their own, from this folder.
#[cfg(test)]
static BCPACK_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

#[cfg(test)]
pub(super) fn use_bcpack_tables(dir: std::path::PathBuf) {
    let _ = BCPACK_DIR.set(dir);
}

#[cfg(test)]
fn load_bcpack_tables() -> Result<Tables, String> {
    let dir = BCPACK_DIR.get().ok_or("no folder set for the test tables")?;
    let bytes = std::fs::read(dir.join("tables.bin")).map_err(|e| e.to_string())?;
    Tables::parse(&bytes).map_err(|e| e.to_string())
}

/// A compressed request's source: borrowed from memory, or read from the
/// file.
fn read_source(request: &Request) -> Result<Cow<'static, [u8]>, HRESULT> {
    let size = request.source_size as usize;
    if request.options & MEMORY_SOURCE != 0 {
        // SAFETY: the title's source holds `size` bytes until completion.
        let bytes = unsafe { std::slice::from_raw_parts(request.source.cast(), size) };
        return Ok(Cow::Borrowed(bytes));
    }
    let mut bytes = vec![0; size];
    read_into(request, &mut bytes)?;
    Ok(Cow::Owned(bytes))
}

/// Nanoseconds workers spent reading files, for the debug summary.
pub(super) static READING: AtomicU64 = AtomicU64::new(0);

/// How often a read the host refuses is tried again (about 0.25 s in all).
const READ_RETRIES: u32 = 8;

/// Fills `out` (the source size) from the request's file or memory.
fn read_into(request: &Request, out: &mut [u8]) -> Result<(), HRESULT> {
    if request.options & MEMORY_SOURCE != 0 {
        // SAFETY: the title's source holds `out.len()` bytes until completion.
        out.copy_from_slice(unsafe { std::slice::from_raw_parts(request.source.cast(), out.len()) });
        return Ok(());
    }
    // SAFETY: a file object we created; the title keeps it open.
    let files = &unsafe { &*request.source.cast::<Instance<File>>() }
        .state
        .files;
    let started = std::time::Instant::now();
    let (mut done, mut tries) = (0, 0);
    let result = loop {
        if done == out.len() {
            break Ok(());
        }
        let error = match files.read_at(&mut out[done..], request.offset + done as u64) {
            Ok(0) => break Err(END_OF_FILE),
            Ok(n) => {
                done += n;
                continue;
            }
            Err(error) => error,
        };
        // Title memory the GPU side guards again in the moment between our
        // opening it and the kernel's write fails the read (and a busy host
        // can refuse one): opened again and tried again, since the title
        // never learns of a failed read.
        tracing::warn!(
            %error,
            offset = request.offset,
            size = out.len(),
            done,
            tries,
            "dstorage: a read of the file failed"
        );
        if tries == READ_RETRIES {
            break Err(from_win32(&error));
        }
        std::thread::sleep(std::time::Duration::from_millis(1 << tries));
        tries += 1;
        oathkeep_gpu::kernel_will_write(out.as_ptr() as usize + done, out.len() - done);
    };
    READING.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    result
}

/// Runs `fill` on the destination's first `len` bytes; `fill` returns how
/// many it produced. GPU-visible destinations are host memory like the
/// rest (GPU address = CPU address), so an address is written in place;
/// physical pages are too when they are consecutive pages of one
/// allocation, and otherwise go through a buffer.
pub(super) fn into_destination(
    request: &Request,
    len: usize,
    mut fill: impl FnMut(&mut [u8]) -> Result<usize, HRESULT>,
) -> Result<(), HRESULT> {
    if request.options & PAGE_DESTINATION == 0 {
        // Raw reads land through the kernel, which does not fault into the
        // GPU side's guard on title buffers; open them first, and until the
        // bytes are there.
        let writing = oathkeep_gpu::kernel_writing(request.destination as usize, len);
        // SAFETY: the title's destination holds `destination_size` bytes, at
        // least `len`, and nothing else writes it until completion.
        let out = unsafe { std::slice::from_raw_parts_mut(request.destination.cast::<u8>(), len) };
        let filled = fill(out).map(drop);
        drop(writing);
        // Again now that the bytes are there: a texture looked at in between
        // was uploaded without them (`xmem::pages_written`).
        if crate::modules::xmem::stream_notice() {
            oathkeep_gpu::kernel_will_write(request.destination as usize, len);
        } else {
            oathkeep_gpu::title_memory_written(request.destination as usize, len, false);
        }
        return filled;
    }
    let offset = (request.page_offset & 0xffff) as usize;
    let list = request.destination.cast();
    let mut pages = page_list(list, offset, len);
    // The title sees its destination through the pages its list names: if
    // it names others by the time the data is written, they get it as well.
    // (Said aloud: by the title's protocol a submitted request's list stays
    // as it is.)
    for _ in 0..3 {
        into_pages(&pages, offset, len, &mut fill)?;
        let now = read_list(list, offset, len);
        if now == pages || !crate::modules::xmem::pages_known(&now, offset, len) {
            break;
        }
        tracing::warn!(
            list = format_args!("{list:p}"),
            offset,
            len,
            first = pages.first(),
            first_now = now.first(),
            "dstorage: a destination's page list changed during the write; written to its new pages too"
        );
        pages = now;
    }
    Ok(())
}

/// Runs `fill` on `len` bytes of `pages`, from `offset` into the first.
fn into_pages(
    pages: &[usize],
    offset: usize,
    len: usize,
    mut fill: impl FnMut(&mut [u8]) -> Result<usize, HRESULT>,
) -> Result<(), HRESULT> {
    // Consecutive pages of one allocation (the usual case): written in
    // place through one view.
    if let Some(mut window) = crate::modules::xmem::page_window(pages, offset, len) {
        let filled = fill(window.bytes()).map(drop);
        crate::modules::xmem::pages_written(pages, offset, len);
        return filled;
    }
    let mut data = vec![0; len];
    let n = fill(&mut data)?;
    // A page the title has just made, or a view the host refuses for a moment (commit
    // or address space short while a load maps many at once), can come right a little
    // later: tried again before the request fails (`OATHKEEP_DSTORAGE_RETRIES`, default 12,
    // about 1.5 s in all).
    static RETRIES: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let retries = *RETRIES.get_or_init(|| {
        std::env::var("OATHKEEP_DSTORAGE_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(12)
    });
    let mut written = crate::modules::xmem::write_physical(pages, offset, &data[..n]);
    let mut tries = 0;
    while !written && tries < retries {
        std::thread::sleep(std::time::Duration::from_millis((2u64 << tries.min(6)).min(128)));
        tries += 1;
        written = crate::modules::xmem::write_physical(pages, offset, &data[..n]);
    }
    if written {
        crate::modules::xmem::pages_written(pages, offset, n);
        if tries > 0 {
            tracing::info!(offset, len, tries, "dstorage: a destination's physical pages written after a retry");
        }
        Ok(())
    } else {
        let known = crate::modules::xmem::pages_known(pages, offset, len);
        tracing::warn!(known, last_error = std::io::Error::last_os_error().raw_os_error(), "dstorage: retries used up");
        // Said aloud: the title never reads a request's result
        // (it only polls for completion), so a destination left unwritten
        // is a model or texture that silently holds whatever its pages
        // held before.
        tracing::warn!(
            offset,
            len,
            pages = pages.len(),
            "dstorage: a destination's physical pages could not be written (unknown page or a host failure); its request fails"
        );
        Err(E_INVALIDARG)
    }
}

/// The page numbers `list` holds now, one per 64 KiB of destination.
pub(super) fn read_list(list: *const usize, offset: usize, len: usize) -> Vec<usize> {
    (0..(offset + len).div_ceil(PAGE))
        // SAFETY: the title passes a page number per 64 KiB of
        // destination, which holds at least `len` bytes from `offset`;
        // it may be writing them.
        .map(|i| unsafe { list.add(i).read_volatile() })
        .collect()
}

/// The request's page numbers. A list is the part of the title's own page
/// table (one number per 64 KiB of its address range) that describes the
/// destination, filled in when the title submits the batch, so it is valid
/// here. It is only ever overwritten: an address the title unmapped keeps
/// the number of the page it had, which the title maps elsewhere soon after,
/// so a number the title owns says nothing about whether it is the right
/// one ([`super::queue::half_submits`]). Page 0 does (an address never mapped yet): a list
/// with a page the title does not own is read again for a while, up to
/// 1.5 s; what it holds then is used as it is.
fn page_list(list: *const usize, offset: usize, len: usize) -> Vec<usize> {
    let mut pages = read_list(list, offset, len);
    if crate::modules::xmem::pages_known(&pages, offset, len) {
        return pages;
    }
    // Timed from here: what took long before was the lock on the page table
    // (which the title's mapping calls hold), not the list.
    let started = std::time::Instant::now();
    let mut known = false;
    while !known && started.elapsed().as_millis() < 1500 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        pages = read_list(list, offset, len);
        known = crate::modules::xmem::pages_known(&pages, offset, len);
    }
    tracing::warn!(
        ms = started.elapsed().as_millis() as u64,
        known,
        "dstorage: a destination's page list was not filled in yet"
    );
    pages
}
