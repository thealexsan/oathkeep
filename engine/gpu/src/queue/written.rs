use super::{E_INVALIDARG, HRESULT, S_OK, This};

/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
pub(super) extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                let now = (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64;
                #[cfg(all(windows, feature = "diagnostics"))]
                crate::host::spans::stamped(at as usize, now);
                as_loaded(now, at as usize, gpu_load())
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::timeline::told("written when its list had run", at as usize, value);
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
            #[cfg(windows)]
            windows_sys::Win32::System::Threading::WakeByAddressAll(at.cast());
        }
    }
}

/// What the title's GPU time is made to look like, in ticks
/// (`OATHKEEP_GPU_LOAD_MS`; unset: as it is).
///
/// The title asks for four GPU timestamps a frame, in pairs of 16 bytes, the
/// start in the second eight and the end in the first. All of them are written
/// when the frame's last submissions land, within microseconds, so every pair
/// says the card did nothing.
fn gpu_load() -> u64 {
    static LOAD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LOAD.get_or_init(|| {
        let ms = std::env::var("OATHKEEP_GPU_LOAD_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        ms * (TIMESTAMP_FREQUENCY / 1000)
    })
}

/// The timestamp written at `at` when the clock says `now`, if a pair's start
/// is to lie `load` ticks before its end.
pub(super) fn as_loaded(now: u64, at: usize, load: u64) -> u64 {
    if at & 8 != 0 {
        now.saturating_sub(load).max(1)
    } else {
        now
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. The title streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Done at once: every earlier
/// submission has run.
pub(super) extern "system" fn copy_page_mappings(
    _: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's CopyPageMappings");
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    let mut ok = true;
    let many = crate::resource::many_page_mapper();
    let mut gathered: Vec<(u64, u64, u64)> = Vec::new();
    let mut map = |to: u64, from: u64, len: u64| match many {
        Some(_) => {
            gathered.push((to, from, len));
            true
        }
        None => map(to, from, len),
    };
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let source = pool + u64::from(r.start) * PAGE;
            ok &= match r.kind {
                0 if inside(r.count) => map(at, source, len),
                1 if inside(1) => (0..u64::from(r.count)).all(|k| map(at + k * PAGE, source, PAGE)),
                2 => map(at, 0, len),
                3 => true,
                _ => false,
            };
            at += len;
        }
    }
    if let Some(many) = many {
        ok &= many(&gathered);
    }
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed"));
    }
    S_OK
}
