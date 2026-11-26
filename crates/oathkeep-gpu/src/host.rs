use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    queue: ID3D12CommandQueue,
    copy_queue: Option<ID3D12CommandQueue>,
    copy_fence: Option<ID3D12Fence>,
    copy_wait_queue: Option<ID3D12CommandQueue>,
    copy_wait_fence: Option<ID3D12Fence>,
    fence: ID3D12Fence,
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
    adapter: Option<IDXGIAdapter3>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

static RETIRED: Mutex<Vec<(u64, Retired)>> = Mutex::new(Vec::new());
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Retired(#[allow(dead_code)] Box<dyn std::any::Any>);
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

pub(crate) fn now_ms() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

pub(crate) fn moment() -> u64 {
    (crate::frame::current() << 32) | ((now_ms() / 100) & 0xffff_ffff)
}

pub(crate) fn long_ago(moment: u64, ms: u64, frames: u64) -> bool {
    let now = self::moment();
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
}

pub(crate) fn available_physical_ram() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullAvailPhys)
}

pub(crate) fn available_commit() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullAvailPageFile)
}

pub(crate) fn commit_idle_ms(avail_commit: Option<u64>) -> u64 {
    const MIB: u64 = 1024 * 1024;
    const NORMAL_MS: u64 = 5_000;
    const ELEVATED_MS: u64 = 3_000;
    const MODERATE_MS: u64 = 1_500;
    const HIGH_MS: u64 = 500;
    const CRITICAL_MS: u64 = 100;
    let Some(avail) = avail_commit else {
        return NORMAL_MS;
    };
    if avail < 1536 * MIB {
        CRITICAL_MS
    } else if avail < 2560 * MIB {
        HIGH_MS
    } else if avail < 3584 * MIB {
        MODERATE_MS
    } else if avail < 4608 * MIB {
        ELEVATED_MS
    } else {
        NORMAL_MS
    }
}

pub(crate) fn commit_idle_frames(avail_commit: Option<u64>) -> u64 {
    const MIB: u64 = 1024 * 1024;
    let Some(avail) = avail_commit else {
        return 4;
    };
    if avail < 1536 * MIB {
        1
    } else if avail < 2560 * MIB {
        2
    } else {
        4
    }
}

pub(crate) fn vram_idle_ms(video_memory: Option<[u64; 4]>) -> u64 {
    const NORMAL_MS: u64 = 5_000;
    const MODERATE_MS: u64 = 2_500;
    const HIGH_MS: u64 = 1_000;
    let Some([budget, used, ..]) = video_memory else {
        return NORMAL_MS;
    };
    if budget == 0 {
        return NORMAL_MS;
    }
    let used_pct = (u128::from(used) * 100) / u128::from(budget);
    if used_pct >= 85 {
        HIGH_MS
    } else if used_pct >= 75 {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

pub(crate) fn ram_idle_ms(avail_phys: Option<u64>) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    const NORMAL_MS: u64 = 10_000;
    const MODERATE_MS: u64 = 5_000;
    const HIGH_MS: u64 = 2_000;
    let Some(avail) = avail_phys else {
        return NORMAL_MS;
    };
    if avail < 2 * GIB {
        HIGH_MS
    } else if avail < 4 * GIB {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

pub(crate) fn references(object: &impl Interface) -> u32 {
    type Method = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;
    let raw = object.as_raw();
    // SAFETY: a live COM object, whose first three methods are those of
    // `IUnknown` (query, add a reference, release one); the pair leaves the
    // count as it was, and the release returns it. The first is not called.
    unsafe {
        let methods = *raw.cast::<*const [Method; 3]>();
        ((*methods)[1])(raw);
        ((*methods)[2])(raw)
    }
}

fn sweep() {
    crate::inplace::tick();
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 250 {
        return;
    }
    let avail_commit = available_commit();
    let interval = if avail_commit.is_some_and(|c| c < (2560u64 << 20)) {
        250
    } else {
        1000
    };
    if now < last + interval
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    crate::pipeline::trim_library();
    crate::shadow::let_go_idle();
    crate::texture::let_go_idle();
    crate::resource::let_go_idle();
    if let Some(avail) = avail_commit
        && avail < (3u64 << 30)
    {
        crate::pipeline::trim_shaders();
        if avail < (2560u64 << 20) {
            drain_spare(false);
        }
        if avail < (2u64 << 30) {
            crate::list::trim_idle_to(4);
            trim_prelude_pool(2);
        } else {
            crate::list::trim_idle();
            trim_prelude_pool(4);
        }
    }
}

pub(crate) fn reclaim_for_oom() {
    static RECLAIMING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(2)) {
            return;
        }
        *last = Some(now);
    }
    if RECLAIMING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            RECLAIMING.store(false, Ordering::Release);
        }
    }
    let _guard = Guard;
    tracing::warn!("reclaim_for_oom: trimming idle caches and retired resources after E_OUTOFMEMORY");
    crate::pipeline::trim_library();
    crate::pipeline::trim_shaders();
    crate::shadow::let_go_idle_urgent();
    crate::texture::let_go_idle_urgent();
    crate::resource::let_go_idle_urgent();
    crate::list::trim_idle_to(0);
    trim_prelude_pool(0);
    drain_spare(true);
    if let Ok(mut guard) = CARD_WORK.try_lock()
        && let Some(work) = guard.as_mut()
    {
        drop(work.fetched.take());
        drop(std::mem::take(&mut work.upload_pool));
    }
    bury(FINISHED.load(Ordering::Acquire));
}

pub(crate) fn retire<T: 'static>(object: T) {
    let started = STARTED.load(Ordering::Acquire);
    if started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((started, Retired(Box::new(object))));
}

pub(crate) fn retire_batch<T: 'static>(objects: Vec<T>) {
    if objects.is_empty() {
        return;
    }
    let count = objects.len() as u64;
    let started = STARTED.load(Ordering::Acquire);
    if started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(count, Ordering::Relaxed);
        drop(objects);
        return;
    }
    RETIREMENTS[1].fetch_add(count, Ordering::Relaxed);
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((started, Retired(Box::new(objects))));
}

fn bury(finished: u64) {
    let done: Vec<(u64, Retired)> = {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        if retired.is_empty() {
            return;
        }
        let (done, keep) = std::mem::take(&mut *retired)
            .into_iter()
            .partition(|(at, _)| *at <= finished);
        *retired = keep;
        done
    };
    drop(done);
}

static RETIREMENTS: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

static TRAIL: Mutex<std::collections::VecDeque<(u64, u32, String)>> =
    Mutex::new(std::collections::VecDeque::new());
const TRAIL_MOST: usize = 96;

fn trail_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TRAIL").is_none_or(|v| v != "0"))
}

pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
}

pub(crate) fn thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

pub(crate) fn trail(what: impl FnOnce() -> String) {
    if !trail_on() {
        return;
    }
    let entry = (now_us(), thread_id(), what());
    let mut trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    if trail.len() >= TRAIL_MOST {
        trail.pop_front();
    }
    trail.push_back(entry);
}

static SUBMITTING: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

fn submitting(phase: u64, lists: usize) {
    SUBMITTING[1].store(lists as u64, Ordering::Relaxed);
    SUBMITTING[2].store(now_us(), Ordering::Relaxed);
    SUBMITTING[0].store(phase, Ordering::Relaxed);
}

pub(crate) fn removal_context() -> String {
    use std::fmt::Write;
    let now = now_us();
    let ms = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
    let phase = match SUBMITTING[0].load(Ordering::Relaxed) {
        0 => "between submissions (nothing of ours was running on the card)",
        1 => "preparing a submission (nothing of ours was running on the card)",
        2 => "waiting for the card to run a submission",
        _ => "copying back what a submission wrote (nothing of ours was running on the card)",
    };
    let mut text = format!(
        "the submitting thread was {phase}, of {} lists, for {:.1} ms; {}; objects let go at once {} and after a running submission {}; submissions started {} finished {}",
        SUBMITTING[1].load(Ordering::Relaxed),
        ms(SUBMITTING[2].load(Ordering::Relaxed)),
        crate::pipeline::library_removal_note(),
        RETIREMENTS[0].load(Ordering::Relaxed),
        RETIREMENTS[1].load(Ordering::Relaxed),
        STARTED.load(Ordering::Relaxed),
        FINISHED.load(Ordering::Relaxed),
    );
    let trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    let _ = write!(text, "; the last {} things done, newest last:", trail.len());
    for (at, thread, what) in trail.iter() {
        let _ = write!(
            text,
            "\n  {:.1} ms before, thread {thread}: {what}",
            ms(*at)
        );
    }
    text
}

pub(crate) fn log_removal_context() {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !trail_on() || SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::error!("when the host device went: {}", removal_context());
}

static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

static COPY_BACK: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

static CARD_CLOCK_US: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

static WORKING: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

fn working(on: bool) -> bool {
    let now = now_us() + 1;
    let since = WORKING[1].swap(if on { now } else { 0 }, Ordering::Relaxed);
    if since != 0 {
        WORKING[0].fetch_add(now.saturating_sub(since), Ordering::Relaxed);
    }
    since != 0
}

fn worked() -> u64 {
    let since = WORKING[1].load(Ordering::Relaxed);
    let now = if since != 0 {
        (now_us() + 1).saturating_sub(since)
    } else {
        0
    };
    WORKING[0].load(Ordering::Relaxed) + now
}

fn stood(since: std::time::Instant) {
    CARD_CLOCK_US[2].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
}

struct CardClock {
    fence: ID3D12Fence,
    state: Mutex<(u64, Option<(std::time::Instant, u64)>)>,
    sent: std::sync::Condvar,
}

// SAFETY: a fence (free-threaded) behind a lock.
unsafe impl Send for CardClock {}
// SAFETY: as above.
unsafe impl Sync for CardClock {}

fn card_clock_watch(clock: &'static CardClock) {
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        return;
    };
    let mut seen = 0;
    loop {
        let target = {
            let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.0 == seen {
                state = clock.sent.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.0
        };
        // SAFETY: the clock's fence and this thread's event.
        unsafe {
            if clock.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        seen = target;
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == target && state.1.is_none() {
            state.1 = Some((std::time::Instant::now(), worked()));
        }
    }
}

fn card_clock_line(frames: f64, ms: f64) {
    let [idle, behind, stood] =
        [0, 1, 2].map(|i| CARD_CLOCK_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
    tracing::info!(
        "the card's clock per frame: nothing of ours queued {idle:.1} ms (the card about {:.0}% busy by this count), \
         {behind:.1} ms of that while the submitting thread was working (the card idle behind us), the rest while the title \
         had not submitted; the submitting thread stood waiting for the card {stood:.1} ms",
        if ms > 0.0 {
            (100.0 * (1.0 - idle / ms)).clamp(0.0, 100.0)
        } else {
            0.0
        }
    );
}

pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

pub(crate) static MADE: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

pub(crate) static ALIVE: [std::sync::atomic::AtomicU64; 8] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 8];

pub(crate) static LISTS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

#[cfg(feature = "diagnostics")]
fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    c.PagefileUsage as u64
}

static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
            log_removal_context();
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => {
                text.push_str("; a diagnostics build with OATHKEEP_D3D12_DEBUG=1 names the command")
            }
            None => {}
        }
    }
    tracing::error!("{text}");
    #[cfg(test)]
    eprintln!("host GPU failure: {text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        let debug =
            cfg!(feature = "diagnostics") && std::env::var_os("OATHKEEP_D3D12_DEBUG").is_some();
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            if std::env::var_os("OATHKEEP_D3D12_GBV").is_some()
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                }
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        if cfg!(feature = "diagnostics") && std::env::var_os("OATHKEEP_D3D12_WARP").is_some() {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        if std::env::var_os("OATHKEEP_D3D12_BACKGROUND").is_some_and(|v| v == "0")
            && let Ok(device6) = device.cast::<ID3D12Device6>()
        {
            let set = device6.SetBackgroundProcessingMode(
                D3D12_BACKGROUND_PROCESSING_MODE_DISABLE_BACKGROUND_WORK,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL,
                None,
                None,
            );
            tracing::info!(
                ok = set.is_ok(),
                "the host's background shader work is switched off (OATHKEEP_D3D12_BACKGROUND=0)"
            );
        }
        let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        if device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut o1).cast(),
                size_of_val(&o1) as u32,
            )
            .is_ok()
        {
            tracing::info!(
                min = o1.WaveLaneCountMin,
                max = o1.WaveLaneCountMax,
                "host wave lanes"
            );
            oathkeep_shader::emit::set_host_max_wave(o1.WaveLaneCountMax);
        }
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let (copy_queue, copy_fence, copy_wait_queue, copy_wait_fence) =
            if std::env::var_os("OATHKEEP_RELAY_COPY_QUEUE").is_none_or(|v| v != "0") {
                match (
                    device.CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_COPY,
                        ..Default::default()
                    }),
                    device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE),
                ) {
                    (Ok(q), Ok(f)) => {
                        let (wq, wf) = match (
                            device.CreateCommandQueue::<ID3D12CommandQueue>(
                                &D3D12_COMMAND_QUEUE_DESC {
                                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                                    ..Default::default()
                                },
                            ),
                            device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE),
                        ) {
                            (Ok(wq), Ok(wf)) => (Some(wq), Some(wf)),
                            _ => (None, None),
                        };
                        (Some(q), Some(f), wq, wf)
                    }
                    _ => (None, None, None, None),
                }
            } else {
                (None, None, None, None)
            };
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        let adapter = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()))
            .ok();
        tracing::info!("host D3D12 device ready");
        Ok(Host {
            device,
            queue,
            copy_queue,
            copy_fence,
            copy_wait_queue,
            copy_wait_fence,
            fence,
            submitted: Mutex::new(0),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(None),
            adapter,
        })
    }
}

pub(crate) fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(
                    VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                    at.cast()
                );
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(
                        VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old),
                        0
                    );
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(
                        VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                        at.cast()
                    );
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    use super::*;

    #[test]
    fn warp_adapter_accepts_only_software() {
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }
}

#[derive(Default)]
struct IndirectPool {
    buffer: Option<ID3D12Resource>,
    cap: u64,
    used: u64,
    in_indirect: bool,
}

#[derive(Default)]
struct GpuScratchPool {
    buffer: Option<ID3D12Resource>,
    gpu: u64,
    cap: u64,
    used: u64,
    in_read: bool,
}

const GPU_SCRATCH_RING: usize = 16;

struct GpuScratchRing {
    slots: [GpuScratchPool; GPU_SCRATCH_RING],
    next: usize,
}

impl Default for GpuScratchRing {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| GpuScratchPool::default()),
            next: 0,
        }
    }
}

#[derive(Default)]
struct ShadowWrites {
    seqs: Vec<(u64, u64)>,
    deferring: bool,
    pending: Vec<u64>,
}

pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    deferred: Mutex<Deferred>,
    scratch: Mutex<Scratch>,
    states: Mutex<States>,
    twins: Mutex<std::collections::HashMap<usize, TwinUse>>,
    has_twins: std::sync::atomic::AtomicBool,
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    budget: OnceLock<Option<ID3D12Resource>>,
    indirect_pool: Mutex<IndirectPool>,
    gpu_scratch_pool: Mutex<GpuScratchRing>,
    prelude_scratch_pool: Mutex<GpuScratchPool>,
    shadow_writes: Mutex<ShadowWrites>,
    bindless_write_seq: std::sync::atomic::AtomicU64,
    copy_seq: std::sync::atomic::AtomicU64,
    reset_gen: std::sync::atomic::AtomicU64,
    uav_dirty: std::sync::atomic::AtomicBool,
    indirect_uav_dirty: std::sync::atomic::AtomicBool,
    indirect_epoch: std::sync::atomic::AtomicU64,
    _alive: Counted,
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

struct StoredPrelude(Prelude);

// SAFETY: D3D12 command allocators and command lists are free-threaded;
// a pooled prelude is untouched while parked in `PRELUDE_POOL`.
unsafe impl Send for StoredPrelude {}

struct PooledPrelude(Option<Prelude>);

// SAFETY: D3D12 command allocators and command lists are free-threaded;
// each pooled prelude is exclusively owned while in flight and returned to the
// pool only once its submission's GPU fence has completed.
unsafe impl Send for PooledPrelude {}

static PRELUDE_POOL: Mutex<Vec<StoredPrelude>> = Mutex::new(Vec::new());

impl Drop for PooledPrelude {
    fn drop(&mut self) {
        if let Some(prelude) = self.0.take() {
            let overflow = {
                let mut pool = PRELUDE_POOL.lock().unwrap_or_else(|e| e.into_inner());
                if pool.len() < 64 {
                    pool.push(StoredPrelude(prelude));
                    None
                } else {
                    Some(prelude)
                }
            };
            drop(overflow);
        }
    }
}

fn take_prelude(host: &Host) -> Option<PooledPrelude> {
    let popped = PRELUDE_POOL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop()
        .map(|stored| PooledPrelude(Some(stored.0)));
    popped.or_else(|| host.make_prelude().map(|p| PooledPrelude(Some(p))))
}

fn trim_prelude_pool(keep: usize) {
    let dropped: Vec<StoredPrelude> = {
        let mut pool = PRELUDE_POOL.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() > keep {
            pool.drain(keep..).collect()
        } else {
            Vec::new()
        }
    };
    drop(dropped);
}

#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

type States =
    std::collections::HashMap<usize, (u32, u32, std::sync::Arc<crate::resource::HostTexture>)>;

struct TwinUse {
    depth: std::sync::Arc<crate::resource::HostTexture>,
    twin: std::sync::Arc<crate::resource::Twin>,
    needs: [bool; 2],
    behind: [Option<bool>; 2],
    writing: bool,
    late: bool,
}

const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;

const PLANE_VIEWS: u32 = 256;

struct PlanePass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: Mutex<Vec<(bool, i32, Option<PlanePipeline>)>>,
}

type PlanePipeline = (ID3D12RootSignature, ID3D12PipelineState);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for PlanePass {}
// SAFETY: as above.
unsafe impl Sync for PlanePass {}

fn plane_pass() -> Option<&'static PlanePass> {
    static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: PLANE_VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(PlanePass {
                cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind),
                views,
                next: AtomicU32::new(0),
                made: Mutex::default(),
            })
        }
    })
    .as_ref()
}

pub(crate) fn compile(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("oathkeep"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("a shader of ours: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

impl PlanePass {
    fn pipeline(&self, steps: bool, format: DXGI_FORMAT) -> Option<PlanePipeline> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, pipeline)) = made.iter().find(|m| (m.0, m.1) == (steps, format.0)) {
            return pipeline.clone();
        }
        let pipeline = match Self::make(steps, format) {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    steps,
                    format = format.0,
                    "no pipeline to draw a depth plane from its twin: such planes stay as they are"
                );
                None
            }
        };
        made.push((steps, format.0, pipeline.clone()));
        pipeline
    }

    fn make(steps: bool, format: DXGI_FORMAT) -> windows::core::Result<PlanePipeline> {
        use windows::core::s;
        let device = &host().ok_or_else(windows::core::Error::empty)?.device;
        let vs = compile(PLANE_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let source = if steps { PLANE_STEPS } else { PLANE_FLOATS };
        let ps = compile(source, s!("plane"), s!("ps_5_1"))?;
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        // SAFETY: the root signature is inside the pixel shader; the
        // description points at live data and never releases its pointer.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

fn record_plane_pass(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) {
    let (Some(host), Some(pass)) = (host(), plane_pass()) else {
        return;
    };
    // SAFETY: live resources.
    let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
    let steps = twin.Format.0 == 53;
    let format = DXGI_FORMAT(match of.Format.0 {
        44 => 45,
        19 => 20,
        typed => typed,
    });
    let Some((root, pipeline)) = pass.pipeline(steps, format) else {
        return;
    };
    for slice in 0..u32::from(of.DepthOrArraySize) {
        for mip in 0..u32::from(of.MipLevels) {
            let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                        MostDetailedMip: mip,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let target = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                        MipSlice: mip,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
                ..Default::default()
            };
            let target = host.dsv(depth, Some(&target));
            let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: pass.cpu + (slot * pass.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: pass.gpu + u64::from(slot * pass.step),
            };
            // SAFETY: an open list; a slot of our heap; live resources and
            // objects (the pass's own live as long as the process).
            unsafe {
                host.device
                    .CreateShaderResourceView(plane, Some(&view), cpu);
                list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                list.SetGraphicsRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetGraphicsRootDescriptorTable(0, gpu);
                list.OMSetRenderTargets(0, None, false, Some(&target));
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.IASetPrimitiveTopology(
                    windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
                );
                list.DrawInstanced(3, 1, 0, 0);
            }
            crate::resource::PLANES[0].fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn record_twin_copy(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    twin: &crate::resource::Twin,
    colour: bool,
) {
    let (from, to) = if colour {
        (depth, &twin.colour.texture)
    } else {
        (&twin.colour.texture, depth)
    };
    crate::resource::TWINS[2 + usize::from(colour)].fetch_add(1, Ordering::Relaxed);
    if colour && twin.plane.is_some() {
        crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
    }
    let Some(through) = &twin.through else {
        // SAFETY: an open list and two live textures.
        unsafe { list.CopyResource(to, from) };
        return;
    };
    let Some(host) = host() else { return };
    // SAFETY: a live texture.
    let of = unsafe { depth.GetDesc() };
    let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
    let mut rows = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); planes as usize];
    // SAFETY: a plain query writing `planes` footprints.
    unsafe {
        host.device.GetCopyableFootprints(
            &of,
            planes,
            planes,
            0,
            Some(rows.as_mut_ptr()),
            None,
            None,
            None,
        );
    }
    let (first_from, first_to) = if colour { (planes, 0) } else { (0, planes) };
    let subresource = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    };
    let placed = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(through) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, source, dest) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live resources; the buffer was made as big
    // as these footprints, and rests in the common state between copies.
    unsafe {
        list.ResourceBarrier(&[transition(through, common, dest)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_from + i as u32;
            list.CopyTextureRegion(&placed(row), 0, 0, 0, &subresource(from, index), None);
        }
        list.ResourceBarrier(&[transition(through, dest, source)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_to + i as u32;
            list.CopyTextureRegion(&subresource(to, index), 0, 0, 0, &placed(row), None);
        }
        list.ResourceBarrier(&[transition(through, source, common)]);
    }
}

pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("OATHKEEP_D3D12_DRED").is_some()
    })
}

static MESH_GROUPS: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_GPU_TIMING").is_some())
}

pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

struct Counted {
    fresh: std::sync::atomic::AtomicBool,
}

impl Counted {
    fn new() -> Counted {
        ALIVE[0].fetch_add(1, Ordering::Relaxed);
        LISTS[3].fetch_add(1, Ordering::Relaxed);
        Counted {
            fresh: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn used(&self) {
        if self.fresh.swap(false, Ordering::Relaxed) {
            LISTS[3].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        ALIVE[0].fetch_sub(1, Ordering::Relaxed);
        self.used();
    }
}

struct Chunk {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

// SAFETY: a D3D12 resource (free-threaded) and its persistent mapping,
// written only by the list that holds the chunk.
unsafe impl Send for Chunk {}

#[derive(Default)]
struct Scratch {
    chunks: Vec<(Chunk, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

impl Drop for Scratch {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.chunks));
    }
}

const SCRATCH_CHUNK: u64 = 4 << 20;
const SCRATCH_FIRST: u64 = 256 << 10;
const SPARE_MOST: u64 = 128 << 20;

fn scratch_pool() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_SCRATCH_POOL").is_none_or(|v| v != "0"))
}

struct Spare<T> {
    kept: Vec<(T, u64)>,
    bytes: u64,
}

impl<T> Spare<T> {
    const fn new() -> Self {
        Spare {
            kept: Vec::new(),
            bytes: 0,
        }
    }

    fn take(&mut self, size: u64) -> Option<T> {
        let at = self.kept.iter().rposition(|(_, s)| *s == size)?;
        self.bytes -= size;
        Some(self.kept.remove(at).0)
    }

    fn give(&mut self, thing: T, size: u64, most: u64) -> Option<T> {
        if self.bytes + size > most {
            return Some(thing);
        }
        self.bytes += size;
        self.kept.push((thing, size));
        None
    }
}

static SPARE: Mutex<Spare<Chunk>> = Mutex::new(Spare::new());

fn take_chunk(host: &Host, size: u64) -> Option<Chunk> {
    if scratch_pool()
        && let Some(chunk) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
    {
        ALIVE[3].fetch_sub(1, Ordering::Relaxed);
        ALIVE[4].fetch_sub(size, Ordering::Relaxed);
        ALIVE[5].fetch_add(1, Ordering::Relaxed);
        return Some(chunk);
    }
    let (buffer, data) = host.upload_buffer(size)?;
    name(&buffer, || "list scratch".into());
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    ALIVE[1].fetch_add(1, Ordering::Relaxed);
    ALIVE[2].fetch_add(size, Ordering::Relaxed);
    Some(Chunk {
        _buffer: buffer,
        data: data as usize,
        gpu,
    })
}

pub(crate) struct SendDrop<T>(pub(crate) T);

// SAFETY: D3D12 COM objects and our host wrappers are free-threaded and may be
// released on a background thread once no command list references them.
unsafe impl<T> Send for SendDrop<T> {}

pub(crate) fn defer_drop<T: Send + 'static>(value: T) {
    static DROPPER: OnceLock<Option<std::sync::mpsc::Sender<Box<dyn Send>>>> = OnceLock::new();
    let tx = DROPPER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn Send>>();
        match std::thread::Builder::new()
            .name("oathkeep-drop".into())
            .spawn(move || {
                while let Ok(item) = rx.recv() {
                    drop(item);
                }
            }) {
            Ok(_) => Some(tx),
            Err(_) => None,
        }
    });
    if let Some(tx) = tx {
        let _ = tx.send(Box::new(value));
    }
}

fn give_chunks(chunks: Vec<(Chunk, u64)>) {
    if chunks.is_empty() {
        return;
    }
    let mut freed = Vec::new();
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        for (chunk, size) in chunks {
            let back = if scratch_pool() {
                spare.give(chunk, size, SPARE_MOST)
            } else {
                Some(chunk)
            };
            match back {
                None => {
                    ALIVE[3].fetch_add(1, Ordering::Relaxed);
                    ALIVE[4].fetch_add(size, Ordering::Relaxed);
                }
                Some(chunk) => {
                    ALIVE[1].fetch_sub(1, Ordering::Relaxed);
                    ALIVE[2].fetch_sub(size, Ordering::Relaxed);
                    freed.push(chunk);
                }
            }
        }
    }
    if !freed.is_empty() {
        defer_drop(SendDrop(freed));
    }
}

fn drain_spare(sync_drop: bool) {
    let freed: Vec<(Chunk, u64)> = {
        let mut spare = if sync_drop {
            let Ok(guard) = SPARE.try_lock() else {
                return;
            };
            guard
        } else {
            SPARE.lock().unwrap_or_else(|e| e.into_inner())
        };
        if spare.kept.is_empty() {
            return;
        }
        spare.bytes = 0;
        std::mem::take(&mut spare.kept)
    };
    let count = freed.len() as u64;
    let bytes: u64 = freed.iter().map(|(_, s)| *s).sum();
    ALIVE[3].fetch_sub(count, Ordering::Relaxed);
    ALIVE[4].fetch_sub(bytes, Ordering::Relaxed);
    ALIVE[1].fetch_sub(count, Ordering::Relaxed);
    ALIVE[2].fetch_sub(bytes, Ordering::Relaxed);
    if sync_drop {
        drop(freed);
    } else {
        defer_drop(SendDrop(freed));
    }
}

const CARD_UPLOAD_CHUNK: u64 = 16 << 20;
const CARD_UPLOAD_POOL_MAX: u64 = 64 << 20;
const CARD_UPLOAD_WIDE_MIN: u64 = 2 << 20;
const CARD_UPLOAD_SPLIT: usize = 1 << 20;

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct CardPrepUs {
    prepare_write: u64,
    fill_upload: u64,
    record: u64,
}

/// Splits `(shadow_idx, ranges)` upload spans into upload chunks of at most `max_chunk` bytes,
/// returning `(chunk_total_bytes, Vec<(shadow_idx, src_offset, len, dst_offset)>)` per chunk.
fn plan_upload_chunks<S>(
    dirty: &[(S, Vec<(usize, usize)>)],
    max_chunk: usize,
) -> Vec<(u64, Vec<(usize, usize, usize, usize)>)> {
    let cap = max_chunk.max(1);
    let mut chunks: Vec<(u64, Vec<(usize, usize, usize, usize)>)> = Vec::new();
    let mut cur: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut cur_bytes = 0usize;
    for (si, (_, ranges)) in dirty.iter().enumerate() {
        for &(mut a, b) in ranges {
            while a < b {
                let room = cap - cur_bytes;
                if room == 0 {
                    chunks.push((cur_bytes as u64, std::mem::take(&mut cur)));
                    cur_bytes = 0;
                    continue;
                }
                let take = (b - a).min(room);
                cur.push((si, a, take, cur_bytes));
                cur_bytes += take;
                a += take;
            }
        }
    }
    if !cur.is_empty() {
        chunks.push((cur_bytes as u64, cur));
    }
    chunks
}

struct CardWork {
    sync: Vec<Prelude>,
    check: Vec<Prelude>,
    fetch: Prelude,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    merge_root: ID3D12RootSignature,
    merge_pso: ID3D12PipelineState,
    differs: (ID3D12Resource, usize, u64),
    fetched: Option<(ID3D12Resource, usize, u64)>,
    upload_pool: Vec<(ID3D12Resource, usize, u64)>,
    taken: u32,
    out: u32,
}

// SAFETY: D3D12 objects (free-threaded) and persistent mappings, used only
// under the submission lock.
unsafe impl Send for CardWork {}

static CARD_WORK: Mutex<Option<CardWork>> = Mutex::new(None);

pub(crate) const CARD_COMPARE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint compare;  // 0: the hashes are stored; else compared with those stored (2: and then stored)
};
ByteAddressBuffer now : register(t0);
// 16 bytes per piece of the copy: the hash of its bytes when they were
// last sent up (or hashed whole, the first time).
RWByteAddressBuffer hashes : register(u0);
RWByteAddressBuffer differs : register(u1);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= pieces) return;
    uint piece = first + id.x;
    uint lo = piece * 65536u;
    lo = lo > skew ? lo - skew : 0u;
    uint hi = min((piece + 1u) * 65536u - skew, size);
    lo = lo & ~15u;
    hi = (hi + 15u) & ~15u;
    uint4 h = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu);
    [loop]
    for (uint at = lo; at < hi; at += 16u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    if (compare != 0u) {
        differs.Store(out_at + id.x * 4u, any(h != hashes.Load4(piece * 16u)) ? 1u : 0u);
        // With the copy-back on its own thread: the next compare of the
        // piece finds what is written from here on.
        if (compare == 2u) hashes.Store4(piece * 16u, h);
    } else {
        hashes.Store4(piece * 16u, h);
    }
}
"#;

pub(crate) const CARD_MASKED_MERGE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), UAV(u0)"
cbuffer Merge : register(b0) {
    uint dst_off;  // 4-byte aligned byte offset in dst (card buffer)
    uint data_off; // 4-byte aligned byte offset of new bytes in upload buffer
    uint mask_off; // 4-byte aligned byte offset of byte mask (0xFF=write, 0x00=keep)
    uint dwords;   // number of 32-bit dwords to merge
};
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= dwords) return;
    uint off = id.x * 4u;
    uint m = src.Load(mask_off + off);
    if (m == 0u) return;
    uint v = src.Load(data_off + off);
    if (m == 0xFFFFFFFFu) {
        dst.Store(dst_off + off, v);
    } else {
        uint cur = dst.Load(dst_off + off);
        dst.Store(dst_off + off, (cur & ~m) | (v & m));
    }
}
"#;

fn rehash_runs() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CARD_REHASH_RUNS").is_none_or(|v| v != "0"))
}

static CARD_PASSES: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

fn wide_merge() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WIDE_MERGE").is_none_or(|v| v != "0"))
}

struct CardRange {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    declared: (usize, usize),
    span: crate::shadow::CardSpan,
}

struct CardCompare {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    span: crate::shadow::CardSpan,
    at: u32,
    compared: bool,
}

struct CardPlan {
    ranges: Vec<CardRange>,
    compares: Vec<CardCompare>,
    sync: Option<ID3D12CommandList>,
    check: Option<ID3D12CommandList>,
    _synced: Vec<std::sync::Arc<crate::shadow::Shadow>>,
    written_shadows: Vec<usize>,
    lists: Vec<(bool, Prelude)>,
    uploads: Vec<(ID3D12Resource, usize, u64)>,
    order_seq: u64,
}

enum Step {
    Copy(usize, usize, usize),
    Fill(usize, usize, usize),
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    Run(Box<dyn Fn() + Send>),
    Host(Box<dyn Fn() + Send>),
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    Texture(std::sync::Arc<crate::texture::Staging>),
    AfterTextures(std::sync::Arc<dyn Fn() + Send + Sync>),
}

#[derive(Default)]
struct Deferred {
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    upload_bytes: u64,
    kept: Vec<windows::core::IUnknown>,
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    textures: std::collections::HashSet<usize>,
    writes: Vec<(usize, crate::queue::Written, usize)>,
    backs: Vec<(ID3D12Resource, usize, [usize; 3])>,
    prelude_gpu_copies: Vec<(ID3D12Resource, u64, ID3D12Resource, u64, u64)>,
    budget_zeroed: bool,
}

impl Deferred {
    fn hold_upload(&mut self, buffer: ID3D12Resource, size: u64) {
        ALIVE[6].fetch_add(1, Ordering::Relaxed);
        ALIVE[7].fetch_add(size, Ordering::Relaxed);
        self.upload_bytes += size;
        self.uploads.push(buffer);
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
    }
}

pub(crate) fn late() -> bool {
    late_stage() > 0
}

fn late_stage() -> u8 {
    static STAGE: OnceLock<u8> = OnceLock::new();
    *STAGE.get_or_init(|| {
        if std::env::var_os("OATHKEEP_GPU_TIMING").is_some() {
            return 0;
        }
        match std::env::var("OATHKEEP_LATE_WAIT").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            Ok("3") => 3,
            Err(_) if crate::shadow::relay() => 3,
            _ => 0,
        }
    })
}

fn late_exact() -> bool {
    late_stage() > 1
}

pub(crate) fn late_free_present() -> bool {
    late_stage() > 2
}

pub(crate) fn relay() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late_stage() > 0 && crate::shadow::relay())
}

fn relay_exact() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay() && std::env::var_os("OATHKEEP_RELAY").is_none_or(|v| v == "2"))
}

static RELAY_MOVED: std::sync::Condvar = std::sync::Condvar::new();

static LINE_LANDED: std::sync::Condvar = std::sync::Condvar::new();

struct QueueSync {
    order: u64,
    /// Latest `(self.fence, order_seq)` submitted on `self.queue` that writes each card shadow.
    last_3d_write: std::collections::HashMap<usize, (u64, u64)>,
    /// Latest `copy_fence` value submitted on `self.copy_queue` that reads each card shadow.
    last_copy_read: std::collections::HashMap<usize, u64>,
    /// Latest `copy_wait_fence` value submitted on `self.copy_wait_queue` that reads each card shadow.
    last_copy_wait_read: std::collections::HashMap<usize, u64>,
    copy_sent: u64,
    copy_wait_sent: u64,
}

impl QueueSync {
    fn new() -> Self {
        Self {
            order: 0,
            last_3d_write: std::collections::HashMap::new(),
            last_copy_read: std::collections::HashMap::new(),
            last_copy_wait_read: std::collections::HashMap::new(),
            copy_sent: 0,
            copy_wait_sent: 0,
        }
    }

    fn next_order(&mut self) -> u64 {
        self.order += 1;
        self.order
    }

    fn record_3d_writes(
        &mut self,
        shadows: &[usize],
        fence: u64,
        order_seq: u64,
        completed_3d: u64,
    ) {
        for &ptr in shadows {
            self.last_3d_write.insert(ptr, (fence, order_seq));
        }
        if self.last_3d_write.len() > 128 {
            self.last_3d_write.retain(|_, &mut (f, _)| f > completed_3d);
        }
    }

    fn record_copy_reads(&mut self, shadows: &[usize], copy_sent: u64, completed_copy: u64) {
        for &ptr in shadows {
            self.last_copy_read.insert(ptr, copy_sent);
        }
        if self.last_copy_read.len() > 128 {
            self.last_copy_read.retain(|_, &mut f| f > completed_copy);
        }
    }

    fn record_copy_wait_reads(
        &mut self,
        shadows: &[usize],
        copy_wait_sent: u64,
        completed_copy_wait: u64,
    ) {
        for &ptr in shadows {
            self.last_copy_wait_read.insert(ptr, copy_wait_sent);
        }
        if self.last_copy_wait_read.len() > 128 {
            self.last_copy_wait_read
                .retain(|_, &mut f| f > completed_copy_wait);
        }
    }

    fn max_copy_read_for(&self, shadows: &[usize]) -> u64 {
        shadows
            .iter()
            .filter_map(|ptr| self.last_copy_read.get(ptr).copied())
            .max()
            .unwrap_or(0)
    }

    fn max_copy_wait_read_for(&self, shadows: &[usize]) -> u64 {
        shadows
            .iter()
            .filter_map(|ptr| self.last_copy_wait_read.get(ptr).copied())
            .max()
            .unwrap_or(0)
    }

    fn is_shadow_busy(&self, ptr: usize, completed_3d: u64) -> bool {
        self.last_3d_write
            .get(&ptr)
            .is_some_and(|&(f, _)| f > completed_3d)
    }

    /// Computes `(max_wait_fence, per_shadow_fetched_after)` for a copy-queue fetch batch.
    /// Any shadow with a pending 3D write (`write_fence > completed_3d`) contributes its
    /// `write_fence` to `max_wait_fence` and advances its `fetched_after` to `write_order`.
    fn sync_for_copy_fetch(
        &self,
        shadows: &[usize],
        completed_3d: u64,
        landed_seq: u64,
    ) -> (u64, std::collections::HashMap<usize, u64>) {
        let mut max_wait_fence = 0u64;
        let mut after_by_shadow = std::collections::HashMap::with_capacity(shadows.len());
        for &ptr in shadows {
            let after = match self.last_3d_write.get(&ptr).copied() {
                Some((write_fence, write_order)) => {
                    if write_fence > completed_3d && write_fence > max_wait_fence {
                        max_wait_fence = write_fence;
                    }
                    landed_seq.max(write_order)
                }
                None => landed_seq,
            };
            after_by_shadow.insert(ptr, after);
        }
        (max_wait_fence, after_by_shadow)
    }
}

static QUEUE_SYNC: Mutex<Option<QueueSync>> = Mutex::new(None);

fn queue_sync() -> std::sync::MutexGuard<'static, Option<QueueSync>> {
    QUEUE_SYNC.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_queue_sync<R>(f: impl FnOnce(&mut QueueSync) -> R) -> R {
    let mut guard = queue_sync();
    f(guard.get_or_insert_with(QueueSync::new))
}

static RELAY_STATS: [std::sync::atomic::AtomicU64; 13] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 13];

fn relay_skip_uncommitted() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("OATHKEEP_RELAY_SKIP_UNCOMMITTED").is_none_or(|v| v != "0")
    })
}

/// Caches the most recent `VirtualQuery` region while scanning sorted intervals so a large
/// uncommitted `MEM_RESERVE` pool costs a single query across hundreds of pieces.
#[derive(Default)]
struct ReadableCache {
    base: usize,
    end: usize,
    readable: bool,
}

impl ReadableCache {
    /// Returns `true` if `[addr, addr + len)` contains at least one committed, readable byte
    /// (`MEM_COMMIT`, neither `PAGE_NOACCESS` nor `PAGE_GUARD`). Conservatively returns `true`
    /// if `VirtualQuery` fails.
    fn any_readable(&mut self, addr: usize, len: usize) -> bool {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
        };
        if len == 0 {
            return false;
        }
        let end = addr.saturating_add(len);
        let mut cur = addr;
        while cur < end {
            if cur >= self.base && cur < self.end {
                if self.readable {
                    return true;
                }
                cur = self.end;
                continue;
            }
            // SAFETY: plain query filling a local `MEMORY_BASIC_INFORMATION`.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let n = unsafe { VirtualQuery(cur as *const _, &mut info, size_of_val(&info)) };
            if n != size_of_val(&info) || info.RegionSize == 0 {
                return true;
            }
            let r_base = info.BaseAddress as usize;
            let r_end = r_base.saturating_add(info.RegionSize);
            let readable =
                info.State == MEM_COMMIT && (info.Protect & (PAGE_NOACCESS | PAGE_GUARD)) == 0;
            self.base = r_base;
            self.end = r_end;
            self.readable = readable;
            if readable {
                return true;
            }
            if r_end <= cur {
                return true;
            }
            cur = r_end;
        }
        false
    }
}

/// Computes the highest `RELAY_FETCHED` sequence safe to prune from `card.sent` without dropping
/// entries still needed by any in-flight deferred copy fetch.
fn safe_relay_fetched_seq(
    max_landed_seq: u64,
    pending_min_afters: impl IntoIterator<Item = u64>,
) -> u64 {
    match pending_min_afters.into_iter().min() {
        Some(min_after) => min_after.saturating_sub(1),
        None => max_landed_seq,
    }
}

fn landable(line: &[Option<u64>], completed: u64) -> usize {
    line.iter()
        .position(|f| f.is_some_and(|f| f > completed))
        .unwrap_or(line.len())
}

#[cfg(test)]
fn flagged_pieces(flags: &[(usize, u32, bool)]) -> std::collections::HashSet<(usize, u32)> {
    flags.iter().filter(|f| f.2).map(|f| (f.0, f.1)).collect()
}

fn merge_piece_intervals(ranges: &mut Vec<(usize, usize)>) {
    if ranges.len() <= 1 {
        ranges.retain(|&(lo, hi)| lo < hi);
        return;
    }
    ranges.sort_unstable();
    let mut write = 0usize;
    for i in 0..ranges.len() {
        let (lo, hi) = ranges[i];
        if lo >= hi {
            continue;
        }
        if write > 0 && lo <= ranges[write - 1].1 {
            ranges[write - 1].1 = ranges[write - 1].1.max(hi);
        } else {
            ranges[write] = (lo, hi);
            write += 1;
        }
    }
    ranges.truncate(write);
}

fn coalesce_copy_regions(pieces: &[(usize, usize, usize, u64)]) -> Vec<(usize, usize, usize, u64)> {
    let mut out: Vec<(usize, usize, usize, u64)> = Vec::with_capacity(pieces.len());
    for &(shadow_idx, lo, hi, dst_at) in pieces {
        if lo >= hi {
            continue;
        }
        match out.last_mut() {
            Some(last)
                if last.0 == shadow_idx
                    && last.2 == lo
                    && last.3 + (last.2 - last.1) as u64 == dst_at =>
            {
                last.2 = hi;
            }
            _ => out.push((shadow_idx, lo, hi, dst_at)),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn should_defer_landing(
    has_waiters: bool,
    has_urgent_items: bool,
    max_busy_fence: u64,
    completed: u64,
    lists: usize,
    late_most: usize,
    results_low: bool,
    defer_count: u32,
    defer_elapsed_ms: u64,
) -> bool {
    const MAX_DEFER_ROUNDS: u32 = 4;
    const MAX_DEFER_MS: u64 = 6;
    const MAX_DEFER_FENCE_AHEAD: u64 = 2;
    !has_waiters
        && !has_urgent_items
        && max_busy_fence > completed
        && max_busy_fence - completed <= MAX_DEFER_FENCE_AHEAD
        && lists < (late_most / 2).max(1)
        && !results_low
        && defer_count < MAX_DEFER_ROUNDS
        && defer_elapsed_ms < MAX_DEFER_MS
}

#[cfg(test)]
mod relay_tests {
    use super::{
        QueueSync, coalesce_copy_regions, flagged_pieces, landable, merge_piece_intervals,
        should_defer_landing,
    };

    #[test]
    fn flights_land_in_the_order_they_were_sent() {
        let line = [Some(5), None, None, Some(6), None];
        assert_eq!(landable(&line, 4), 0);
        assert_eq!(landable(&line, 5), 3);
        assert_eq!(landable(&line, 6), 5);
        assert_eq!(landable(&line, u64::MAX), 5);
        assert_eq!(landable(&[None, None], 0), 2);
        assert_eq!(landable(&[None, Some(9), None], 8), 1);
        assert_eq!(landable(&[], 3), 0);
        assert_eq!(landable(&line[3..], 5), 0);
        assert_eq!(landable(&line[3..], 6), 2);
    }

    #[test]
    fn a_piece_flagged_under_one_range_is_fetched_under_all() {
        let (a, b) = (0x1000usize, 0x2000usize);
        let flags = [
            (a, 3, true),
            (a, 4, false),
            (a, 3, false),
            (b, 3, false),
            (b, 9, true),
        ];
        let fetch = flagged_pieces(&flags);
        assert!(fetch.contains(&(a, 3)));
        assert!(!fetch.contains(&(a, 4)));
        assert!(!fetch.contains(&(b, 3)));
        assert!(fetch.contains(&(b, 9)));
        assert_eq!(fetch.len(), 2);
    }

    #[test]
    fn piece_intervals_merge_overlapping_subranges_and_keep_disjoint() {
        let mut ranges = vec![
            (0x8000, 0x10000),
            (0x0000, 0x4000),
            (0x2000, 0x5000),
            (0x5000, 0x6000),
            (0x9000, 0x9000),
            (0x7000, 0x9000),
        ];
        merge_piece_intervals(&mut ranges);
        assert_eq!(ranges, [(0x0000, 0x6000), (0x7000, 0x10000)]);
    }

    #[test]
    fn copy_regions_coalesce_adjacent_pieces_of_same_shadow() {
        let pieces = [
            (0usize, 0x0000usize, 0x10000usize, 0u64),
            (0, 0x10000, 0x20000, 0x10000),
            (0, 0x20000, 0x28000, 0x20000),
            (0, 0x40000, 0x50000, 0x28000),
            (1, 0x50000, 0x60000, 0x38000),
            (1, 0x60000, 0x70000, 0x48000),
        ];
        assert_eq!(
            coalesce_copy_regions(&pieces),
            [
                (0, 0x0000, 0x28000, 0),
                (0, 0x40000, 0x50000, 0x28000),
                (1, 0x50000, 0x70000, 0x38000),
            ]
        );
    }

    #[test]
    fn relay_defers_bounded_bursts_and_lands_immediately_when_urgent() {
        assert!(should_defer_landing(false, false, 10, 8, 12, 256, false, 0, 0));
        assert!(should_defer_landing(false, false, 10, 8, 12, 256, false, 3, 5));
        assert!(!should_defer_landing(true, false, 10, 8, 12, 256, false, 0, 0));
        assert!(!should_defer_landing(false, true, 10, 8, 12, 256, false, 0, 0));
        assert!(!should_defer_landing(false, false, 8, 8, 12, 256, false, 0, 0));
        assert!(!should_defer_landing(false, false, 10, 8, 128, 256, false, 0, 0));
        assert!(!should_defer_landing(false, false, 10, 8, 12, 256, true, 0, 0));
        assert!(!should_defer_landing(false, false, 10, 8, 12, 256, false, 4, 0));
        assert!(!should_defer_landing(false, false, 10, 8, 12, 256, false, 1, 6));
        assert!(!should_defer_landing(false, false, 15, 8, 12, 256, false, 0, 0));
    }

    #[test]
    fn queue_sync_classifies_busy_shadows_and_tracks_cross_queue_fences() {
        let mut sync = QueueSync::new();
        let (a, b, c) = (0x1000usize, 0x2000usize, 0x3000usize);
        let o1 = sync.next_order();
        let o2 = sync.next_order();
        assert_eq!((o1, o2), (1, 2));

        // Shadow `a` finished at fence 5 (order 1); shadow `b` is still in flight at fence 9 (order 2).
        sync.record_3d_writes(&[a], 5, o1, 4);
        sync.record_3d_writes(&[b], 9, o2, 6);

        // At completed_3d = 6: `a` is idle, `b` is busy, `c` was never written.
        assert!(!sync.is_shadow_busy(a, 6));
        assert!(sync.is_shadow_busy(b, 6));
        assert!(!sync.is_shadow_busy(c, 6));

        // Immediate copy batch (`a`, `c`) needs NO 3D wait (wait_fence == 0).
        let (wait_now, after_now) = sync.sync_for_copy_fetch(&[a, c], 6, 1);
        assert_eq!(wait_now, 0);
        assert_eq!(after_now[&a], 1);
        assert_eq!(after_now[&c], 1);

        // Busy copy batch (`b`) waits ONLY for fence 9 (not future unrelated flights) and advances `after` to o2.
        let (wait_busy, after_busy) = sync.sync_for_copy_fetch(&[b], 6, 1);
        assert_eq!(wait_busy, 9);
        assert_eq!(after_busy[&b], o2);

        // Record copy reads and verify subsequent 3D writes know which copy_fence to wait for.
        sync.record_copy_reads(&[a, c], 10, 0);
        sync.record_copy_reads(&[b], 11, 0);
        assert_eq!(sync.max_copy_read_for(&[a]), 10);
        assert_eq!(sync.max_copy_read_for(&[a, b]), 11);
        assert_eq!(sync.max_copy_read_for(&[0x9999]), 0);
    }

    #[test]
    fn readable_cache_skips_uncommitted_and_detects_committed_pages() {
        use super::ReadableCache;
        use windows_sys::Win32::System::Memory::*;
        const SIZE: usize = 4 << 16; // 256 KiB
        // SAFETY: isolated private test reservation freed before return.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS) as usize;
            assert_ne!(base, 0);
            let mut cache = ReadableCache::default();
            // Whole reserved span is uncommitted -> skipped.
            assert!(!cache.any_readable(base, SIZE));
            assert!(!cache.any_readable(base + 0x10000, 0x10000));
            assert!(!cache.any_readable(base + 0x20000, 0x10000));

            // Commit a single 4 KiB page inside the third 64 KiB piece.
            let committed_page = base + 0x20000 + 0x3000;
            assert_eq!(
                VirtualAlloc(committed_page as *const _, 4096, MEM_COMMIT, PAGE_READWRITE),
                committed_page as *mut _
            );
            let mut cache = ReadableCache::default();
            assert!(!cache.any_readable(base, 0x20000));
            assert!(cache.any_readable(base + 0x20000, 0x10000));
            assert!(!cache.any_readable(base + 0x20000, 0x3000));
            assert!(cache.any_readable(committed_page, 64));
            assert!(!cache.any_readable(base + 0x30000, 0x10000));

            // PAGE_NOACCESS on committed page -> not readable.
            let mut old = 0u32;
            assert_ne!(
                VirtualProtect(committed_page as *const _, 4096, PAGE_NOACCESS, &mut old),
                0
            );
            let mut cache = ReadableCache::default();
            assert!(!cache.any_readable(base + 0x20000, 0x10000));

            // PAGE_GUARD on committed page -> not readable.
            assert_ne!(
                VirtualProtect(
                    committed_page as *const _,
                    4096,
                    PAGE_READWRITE | PAGE_GUARD,
                    &mut old
                ),
                0
            );
            let mut cache = ReadableCache::default();
            assert!(!cache.any_readable(base + 0x20000, 0x10000));

            assert_ne!(VirtualFree(base as *mut _, 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn safe_relay_fetched_seq_retains_entries_for_pending_copies() {
        use super::safe_relay_fetched_seq;
        // No pending copies: safe to advance all the way to max_landed_seq.
        assert_eq!(safe_relay_fetched_seq(10, []), 10);
        // Pending slot with min_after = 8 keeps all sent entries with seq > 7 (>= 8).
        assert_eq!(safe_relay_fetched_seq(10, [8]), 7);
        // Multiple pending slots use the minimum min_after across all of them.
        assert_eq!(safe_relay_fetched_seq(15, [12, 9, 14]), 8);
        assert_eq!(safe_relay_fetched_seq(5, [0]), 0);
    }

    #[test]
    fn upload_chunks_pack_and_split_large_ranges_without_dropping_bytes() {
        use super::plan_upload_chunks;
        let dirty = vec![
            (0usize, vec![(0usize, 10usize), (20, 35)]), // 10 + 15 = 25
            (1usize, vec![(100, 140)]),                  // 40
        ];
        let chunks = plan_upload_chunks(&dirty, 16);
        // Total bytes = 65 -> 5 chunks: 16, 16, 16, 16, 1.
        let sizes: Vec<u64> = chunks.iter().map(|(sz, _)| *sz).collect();
        assert_eq!(sizes, vec![16, 16, 16, 16, 1]);
        // Reconstruct copied ranges per shadow and check dst offsets are packed contiguously.
        let mut total_by_shadow = [0usize; 2];
        for (sz, items) in &chunks {
            let mut cursor = 0usize;
            for &(si, _a, len, dst_off) in items {
                assert_eq!(dst_off, cursor);
                cursor += len;
                total_by_shadow[si] += len;
            }
            assert_eq!(cursor as u64, *sz);
        }
        assert_eq!(total_by_shadow, [25, 40]);
        assert!(plan_upload_chunks::<usize>(&[], 16).is_empty());
    }

    #[test]
    fn copy_wait_slots_and_split_overlapping_spans_behave_as_expected() {
        use super::{COPY_WAIT_SLOTS, split_overlapping_spans};
        assert!(COPY_WAIT_SLOTS >= 8);
        let mut spans = vec![(0x1000, 0x2000), (0x4000, 0x5000)];
        // Disjoint write does not disturb spans.
        assert!(split_overlapping_spans(&mut spans, 0x2000, 0x4000).is_empty());
        assert_eq!(spans, vec![(0x1000, 0x2000), (0x4000, 0x5000)]);
        // Overlapping write splits cleanly.
        let hit = split_overlapping_spans(&mut spans, 0x1800, 0x4800);
        assert_eq!(hit, vec![(0x1800, 0x2000), (0x4000, 0x4800)]);
        assert_eq!(spans, vec![(0x1000, 0x1800), (0x4800, 0x5000)]);
    }
}

fn late_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("OATHKEEP_LATE_WAIT_LISTS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(512)
            .max(1)
    })
}

fn late_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LATE_WAIT_WRITES").is_none_or(|v| v != "0"))
}

type Declared = (std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>);

struct Sent {
    number: u64,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    writes: Vec<(usize, crate::queue::Written, usize)>,
    preludes: Vec<PooledPrelude>,
}

struct Flight {
    number: u64,
    fence: u64,
    lists: Vec<crate::list::Detached>,
    order: Vec<usize>,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    _preludes: Vec<PooledPrelude>,
}

enum Late {
    Run(Flight),
    Write(usize, crate::queue::Written, usize),
    Signal(usize, u64),
}

struct Line {
    line: Vec<Late>,
    written: Vec<(usize, usize)>,
    pending_written: Vec<(usize, usize)>,
    pending_writes: Vec<(usize, crate::queue::Written, usize)>,
    pending_copies: usize,
    lists: usize,
    target: u64,
    settling: bool,
    poke: bool,
    waiters: usize,
    landed: Vec<(Vec<crate::list::Detached>, Vec<usize>)>,
    in_hand: Vec<(usize, crate::queue::Written, usize)>,
}

// SAFETY: D3D12 objects (free-threaded) and host lists nothing records
// into; touched under the lock.
unsafe impl Send for Line {}

static LINE: Mutex<Line> = Mutex::new(Line {
    line: Vec::new(),
    written: Vec::new(),
    pending_written: Vec::new(),
    pending_writes: Vec::new(),
    pending_copies: 0,
    lists: 0,
    target: 0,
    settling: false,
    poke: false,
    waiters: 0,
    landed: Vec::new(),
    in_hand: Vec::new(),
});

fn rebuild_line_written(line: &mut Line) {
    line.written.clear();
    line.lists = 0;
    let mut ranges = line.pending_written.clone();
    for item in &line.line {
        if let Late::Run(flight) = item {
            line.lists += flight.order.len().max(1);
            ranges.extend(flight.back.iter().flat_map(|(_, r)| r.iter().copied()));
        }
    }
    join_ranges(&mut line.written, ranges);
}
static LINE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

fn line() -> std::sync::MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(|e| e.into_inner())
}

const WHY_SYNC: usize = 2;
const WHY_CARD_DONE: usize = 3;
const WHY_VALUE: usize = 4;
const WHY_STALE: usize = 5;
const WHY_READ: usize = 6;
const WHY_RUN: usize = 7;
const WHY_MOST: usize = 8;
const WHY_WRITES: usize = 9;
const WHY_FENCE: usize = 10;

static LATE_STATS: [std::sync::atomic::AtomicU64; 18] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 18];

static LATE_WAITED: [std::sync::atomic::AtomicU64; 11] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 11];

fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let at = ranges.partition_point(|r| r.1 <= from);
    ranges.get(at).is_some_and(|r| r.0 < to)
}

fn join_ranges(ranges: &mut Vec<(usize, usize)>, more: impl IntoIterator<Item = (usize, usize)>) {
    ranges.extend(more.into_iter().filter(|r| r.0 < r.1));
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges.iter() {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    *ranges = out;
}

fn flying() -> bool {
    if !late() {
        return false;
    }
    let line = line();
    line.settling || line.pending_copies > 0 || line.line.iter().any(|l| matches!(l, Late::Run(_)))
}

fn late_blocked(runs: &[&HostList]) -> Option<usize> {
    let written = {
        let line = line();
        if line.written.is_empty() {
            return None;
        }
        line.written.clone()
    };
    blocked_by(&written, runs, true)
}

fn blocked_by(written: &[(usize, usize)], runs: &[&HostList], count: bool) -> Option<usize> {
    if written.is_empty() {
        return None;
    }
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    if touches(written, src, src + len) || touches(written, dst, dst + len) {
                        if count {
                            LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                &Step::Fill(_, src, len) => {
                    if touches(written, src, src + len) {
                        if count {
                            let hit = LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "diagnostics")]
                            if hit < 8 {
                                let existing = crate::shadow::find_existing(src as u64, len as u64);
                                tracing::info!(
                                    hit,
                                    src = format_args!("{src:#x}"),
                                    len,
                                    has_shadow = existing.is_some(),
                                    ever_written = existing.as_ref().is_some_and(|s| s.ever_written()),
                                    "WHY_READ Step::Fill stall"
                                );
                            }
                            #[cfg(not(feature = "diagnostics"))]
                            let _ = hit;
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if touches(written, at, at + len) {
                        if count {
                            LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Run(_) => return Some(WHY_RUN),
                Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(_) | Step::AfterTextures(_) => {}
            }
        }
    }
    None
}

fn stale_under_flights(runs: &[&HostList]) -> bool {
    use std::sync::Arc;
    let mut spans: Vec<(Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)> = Vec::new();
    let mut index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::Refresh(shadow, address, len) = step
                && shadow.writes_pending()
            {
                let range = (*address, *address + *len);
                let key = Arc::as_ptr(shadow) as usize;
                match index.get(&key) {
                    Some(&at) => spans[at].1.push(range),
                    None => {
                        index.insert(key, spans.len());
                        spans.push((shadow.clone(), vec![range]));
                    }
                }
            }
        }
    }
    for (_, ranges) in &mut spans {
        let mut merged = Vec::new();
        join_ranges(&mut merged, std::mem::take(ranges));
        *ranges = merged;
    }
    if late_exact() {
        let mut runs_left = 256;
        return spans.iter().any(|(shadow, ranges)| {
            ranges
                .iter()
                .any(|&(from, to)| shadow.merge_under_writes(from, to - from, &mut runs_left))
        });
    }
    spans.iter().any(|(shadow, ranges)| {
        ranges
            .iter()
            .any(|&(from, to)| shadow.stale_under_writes(from, to - from))
    })
}

pub(crate) fn late_signal(fence: usize, value: u64) -> bool {
    if !late() {
        return false;
    }
    let mut line = line();
    if line.line.is_empty() && !line.settling && line.pending_copies == 0 {
        return false;
    }
    line.line.push(Late::Signal(fence, value));
    LATE_STATS[14].fetch_add(1, Ordering::Relaxed);
    drop(line);
    LINE_MOVED.notify_one();
    RELAY_MOVED.notify_one();
    true
}

pub(crate) fn late_met(wait: &crate::list::ValueWait) -> bool {
    let (address, bytes) = wait.place();
    {
        let line = line();
        if line.line.is_empty() && line.in_hand.is_empty() && line.pending_copies == 0 {
            return false;
        }
        let mut unsure = touches(&line.written, address, address + bytes);
        let writes = line
            .in_hand
            .iter()
            .map(|(at, what, n)| (at, what, n))
            .chain(line.pending_writes.iter().map(|(at, what, n)| (at, what, n)))
            .chain(line.line.iter().filter_map(|item| match item {
                Late::Write(at, what, n) => Some((at, what, n)),
                _ => None,
            }));
        for (at, what, n) in writes {
            if *at >= address + bytes || address >= *at + *n {
                continue;
            }
            match what {
                crate::queue::Written::Value(v) if *at == address && *n >= bytes => {
                    if wait.holds_for(*v) {
                        LATE_STATS[13].fetch_add(1, Ordering::Relaxed);
                        return true;
                    }
                }
                _ => unsure = true,
            }
        }
        if !unsure {
            return false;
        }
    }
    settle_now(WHY_VALUE);
    wait.met()
}

pub(crate) fn settle_now(why: usize) {
    if !late() {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
    host.settle(&mut submitted, why);
}

pub(crate) fn settle_for_present() {
    settle_now(WHY_SYNC);
}
pub(crate) fn settle_for_fence() {
    settle_now(WHY_FENCE);
}

fn land() {
    let landed = std::mem::take(&mut line().landed);
    for (lists, order) in landed {
        crate::list::recycle(lists, &order);
    }
}

fn late_helper() {
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        tracing::warn!("no event for the thread that settles submissions sent ahead");
        return;
    };
    loop {
        let target = {
            let mut line = line();
            loop {
                if line.poke {
                    line.poke = false;
                    break None;
                }
                if !relay() && !line.line.is_empty() && !line.settling {
                    break Some(line.target);
                }
                line = LINE_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(target) = target else {
            land();
            crate::queue::release_held(false);
            continue;
        };
        // SAFETY: our fence and event.
        unsafe {
            if host.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        {
            let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            if line().target != target {
                continue;
            }
            host.settle(&mut submitted, WHY_CARD_DONE);
        }
        land();
        crate::queue::release_held(false);
    }
}

fn late_threads() {
    static HELPER: std::sync::Once = std::sync::Once::new();
    HELPER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("oathkeep-late-wait".into()).spawn(late_helper) {
            tracing::warn!(error = %e, "no thread to settle submissions sent ahead: they are settled by the calls that follow");
        }
        tracing::info!(
            most = late_most(),
            writes = late_writes(),
            "submissions are sent to the card without a wait for each (OATHKEEP_LATE_WAIT=1)"
        );
        if relay() {
            if let Err(e) = std::thread::Builder::new().name("oathkeep-relay".into()).spawn(relay_worker) {
                fail(format_args!("no thread to land submissions sent ahead (OATHKEEP_RELAY=1): {e}"));
            }
            tracing::info!(
                exact = relay_exact(),
                "what shaders wrote is copied back by a thread of its own, beside the submitting thread (OATHKEEP_RELAY=1)"
            );
        }
    });
}

fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
    let mut line = line();
    if done(&line) {
        return;
    }
    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
    LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
    let was_working = working(false);
    let waiting = std::time::Instant::now();
    line.waiters += 1;
    RELAY_MOVED.notify_one();
    while !done(&line) {
        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
    }
    line.waiters = line.waiters.saturating_sub(1);
    drop(line);
    stood(waiting);
    working(was_working);
    let us = waiting.elapsed().as_micros() as u64;
    LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
    LATE_WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
    SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
}

const COPY_WAIT_SLOTS: usize = 8;

struct PendingCopyFetch {
    copy_sent: u64,
    isolated_wait: bool,
    dst_data: usize,
    items: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)>,
    after_by_shadow: std::collections::HashMap<usize, u64>,
    shadow_ptrs: Vec<usize>,
    landed_seq: u64,
    min_after: u64,
    written_ranges: Vec<(usize, usize)>,
    deferred_done: Vec<Declared>,
    deferred_sync: Vec<Late>,
}

struct CopyWaitSlot {
    prelude: Prelude,
    event: HANDLE,
    buffer: Option<(ID3D12Resource, usize, u64)>,
    pending: Option<PendingCopyFetch>,
}

struct RelayFetch {
    list: Prelude,
    copy_list: Option<Prelude>,
    copy_wait_slots: Vec<CopyWaitSlot>,
    pending_order: std::collections::VecDeque<usize>,
    copy_wake_event: HANDLE,
    max_landed_seq: u64,
    fence: ID3D12Fence,
    event: HANDLE,
    sent: u64,
    copy_event: HANDLE,
    buffer: Option<(ID3D12Resource, usize, u64)>,
    copy_buffer: Option<(ID3D12Resource, usize, u64)>,
}

impl RelayFetch {
    fn has_pending(&self) -> bool {
        !self.pending_order.is_empty()
    }

    fn oldest_pending_copy_sent(&self) -> Option<(u64, bool)> {
        self.pending_order.front().and_then(|&idx| {
            self.copy_wait_slots[idx]
                .pending
                .as_ref()
                .map(|p| (p.copy_sent, p.isolated_wait))
        })
    }

    fn has_pending_isolated_for_shadow(&self, ptr: usize) -> bool {
        self.pending_order.iter().any(|&idx| {
            self.copy_wait_slots[idx]
                .pending
                .as_ref()
                .is_some_and(|p| p.isolated_wait && p.shadow_ptrs.contains(&ptr))
        })
    }

    fn pending_written_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        for &idx in &self.pending_order {
            if let Some(p) = &self.copy_wait_slots[idx].pending {
                ranges.extend(p.written_ranges.iter().copied());
            }
        }
        let mut merged = Vec::new();
        join_ranges(&mut merged, ranges);
        merged
    }

    fn pending_deferred_writes(&self) -> Vec<(usize, crate::queue::Written, usize)> {
        let mut writes = Vec::new();
        for &idx in &self.pending_order {
            if let Some(p) = &self.copy_wait_slots[idx].pending {
                for item in &p.deferred_sync {
                    if let Late::Write(at, what, bytes) = item {
                        writes.push((*at, *what, *bytes));
                    }
                }
            }
        }
        writes
    }

    fn safe_fetched_seq(&self) -> u64 {
        safe_relay_fetched_seq(
            self.max_landed_seq,
            self.pending_order
                .iter()
                .filter_map(|&idx| self.copy_wait_slots[idx].pending.as_ref().map(|p| p.min_after)),
        )
    }

    fn last_pending_pos_for_shadows(&self, shadow_ptrs: &[usize]) -> Option<usize> {
        if self.pending_order.is_empty() || shadow_ptrs.is_empty() {
            return None;
        }
        self.pending_order.iter().rposition(|&idx| {
            self.copy_wait_slots[idx].pending.as_ref().is_some_and(|p| {
                p.shadow_ptrs
                    .iter()
                    .any(|ptr| shadow_ptrs.contains(ptr))
            })
        })
    }

    fn free_slot(&self) -> Option<usize> {
        self.copy_wait_slots
            .iter()
            .position(|slot| slot.pending.is_none())
    }
}

enum RelayAction {
    Land(Vec<Late>, std::collections::HashSet<usize>, bool),
    DrainAllPending,
    Wait3d(u64),
    Wait3dAndCopy(u64, u64, bool),
    WaitCopyOnly(u64, bool),
}

fn relay_worker() {
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        fail(format_args!(
            "no event for the thread that lands submissions sent ahead (OATHKEEP_RELAY=1)"
        ));
    };
    let mut fetch: Option<RelayFetch> = None;
    let mut defer_count = 0u32;
    let mut defer_since: Option<std::time::Instant> = None;
    let mut fences: Vec<Option<u64>> = Vec::new();
    loop {
        if fetch.as_ref().is_some_and(RelayFetch::has_pending) {
            host.drain_completed_pending_copies(&mut fetch);
        }
        let oldest_pending = fetch
            .as_ref()
            .and_then(RelayFetch::oldest_pending_copy_sent);
        let action = {
            let mut line = line();
            loop {
                if line.line.is_empty() {
                    if let Some((oldest_copy, isolated)) = oldest_pending {
                        if line.waiters > 0 {
                            break RelayAction::DrainAllPending;
                        }
                        break RelayAction::WaitCopyOnly(oldest_copy, isolated);
                    }
                    defer_count = 0;
                    defer_since = None;
                    line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                fences.clear();
                fences.extend(line.line.iter().map(|l| match l {
                    Late::Run(flight) => Some(flight.fence),
                    _ => None,
                }));
                // SAFETY: a plain query.
                let completed = unsafe { host.fence.GetCompletedValue() };
                let n = landable(&fences, completed);
                if n == 0 {
                    if let Some((oldest_copy, isolated)) = oldest_pending {
                        if line.waiters > 0 {
                            break RelayAction::DrainAllPending;
                        }
                        break RelayAction::Wait3dAndCopy(
                            fences[0].unwrap_or(0),
                            oldest_copy,
                            isolated,
                        );
                    }
                    break RelayAction::Wait3d(fences[0].unwrap_or(0));
                }
                let mut busy_shadows = std::collections::HashSet::new();
                let mut max_busy_fence = 0u64;
                if n < line.line.len() {
                    let mut landed_card_shadows = std::collections::HashSet::new();
                    for item in &line.line[..n] {
                        if let Late::Run(flight) = item
                            && let Some(card) = &flight.card
                        {
                            for r in &card.ranges {
                                landed_card_shadows
                                    .insert(std::sync::Arc::as_ptr(&r.shadow) as usize);
                            }
                        }
                    }
                    for item in &line.line[n..] {
                        if let Late::Run(flight) = item
                            && let Some(card) = &flight.card
                        {
                            let mut touches_landed = false;
                            for &ptr in &card.written_shadows {
                                busy_shadows.insert(ptr);
                                if landed_card_shadows.contains(&ptr) {
                                    touches_landed = true;
                                }
                            }
                            if touches_landed && flight.fence > max_busy_fence {
                                max_busy_fence = flight.fence;
                            }
                        }
                    }
                }
                let has_urgent = line.line[..n].iter().any(|item| match item {
                    Late::Signal(..) | Late::Write(..) => true,
                    Late::Run(flight) => flight
                        .order
                        .iter()
                        .any(|&i| !flight.lists[i].host().deferred().backs.is_empty()),
                });
                let elapsed_ms = defer_since.map_or(0, |t| t.elapsed().as_millis() as u64);
                if should_defer_landing(
                    line.waiters > 0,
                    has_urgent,
                    max_busy_fence,
                    completed,
                    line.lists,
                    late_most(),
                    host.card_results_low(),
                    defer_count,
                    elapsed_ms,
                ) {
                    if defer_since.is_none() {
                        defer_since = Some(std::time::Instant::now());
                    }
                    defer_count += 1;
                    if let Some((oldest_copy, isolated)) = oldest_pending {
                        break RelayAction::Wait3dAndCopy(max_busy_fence, oldest_copy, isolated);
                    }
                    break RelayAction::Wait3d(max_busy_fence);
                }
                defer_count = 0;
                defer_since = None;
                let has_waiters = line.waiters > 0;
                line.settling = true;
                let items: Vec<Late> = line.line.drain(..n).collect();
                line.in_hand = items
                    .iter()
                    .filter_map(|l| match l {
                        Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                        _ => None,
                    })
                    .collect();
                break RelayAction::Land(items, busy_shadows, has_waiters);
            }
        };
        match action {
            RelayAction::Land(items, busy_shadows, has_waiters) => {
                host.relay_land(items, &busy_shadows, &mut fetch, has_waiters);
            }
            RelayAction::DrainAllPending => {
                host.drain_all_pending_copies(&mut fetch);
            }
            RelayAction::Wait3d(target) => {
                let waiting = std::time::Instant::now();
                // SAFETY: the host's fence and this thread's event.
                let waited = unsafe {
                    let _ = ResetEvent(event);
                    host.fence.SetEventOnCompletion(target, event).map(|()| {
                        WaitForSingleObject(event, INFINITE);
                    })
                };
                if waited.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            RelayAction::Wait3dAndCopy(target, oldest_copy, isolated) => {
                let waiting = std::time::Instant::now();
                let copy_wake = fetch.as_ref().map(|f| f.copy_wake_event);
                let copy_fence = if isolated {
                    host.copy_wait_fence.as_ref().or(host.copy_fence.as_ref())
                } else {
                    host.copy_fence.as_ref()
                };
                // SAFETY: the host's 3D/copy fences and this thread's events.
                unsafe {
                    let _ = ResetEvent(event);
                    let ok_3d = host.fence.SetEventOnCompletion(target, event).is_ok();
                    let ok_copy = copy_wake.zip(copy_fence).is_some_and(|(ev, cf)| {
                        let _ = ResetEvent(ev);
                        cf.SetEventOnCompletion(oldest_copy, ev).is_ok()
                    });
                    if ok_3d && ok_copy {
                        let handles = [event, copy_wake.unwrap()];
                        WaitForMultipleObjects(&handles, false, 2);
                    } else if ok_3d {
                        WaitForSingleObject(event, 2);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            RelayAction::WaitCopyOnly(oldest_copy, isolated) => {
                let waiting = std::time::Instant::now();
                let copy_wake = fetch.as_ref().map(|f| f.copy_wake_event);
                let copy_fence = if isolated {
                    host.copy_wait_fence.as_ref().or(host.copy_fence.as_ref())
                } else {
                    host.copy_fence.as_ref()
                };
                // SAFETY: the host's copy fence and this thread's copy wake event.
                unsafe {
                    if let Some((ev, cf)) = copy_wake.zip(copy_fence) {
                        let _ = ResetEvent(ev);
                        if cf.SetEventOnCompletion(oldest_copy, ev).is_ok() {
                            WaitForSingleObject(ev, 2);
                        } else {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
    }
}

fn relay_hand_back(back: &[Declared]) {
    use std::sync::Arc;
    let mut work: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, declared) in back {
        if shadow.carded() {
            continue;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        join_ranges(&mut spans, declared.iter().copied());
        for &(from, to) in &spans {
            shadow.relay_compared(from, to - from);
        }
        work.extend(
            spans
                .into_iter()
                .map(|(from, to)| (shadow.clone(), from, to)),
        );
    }
    if work.is_empty() {
        return;
    }
    #[cfg(feature = "diagnostics")]
    let scanning = std::time::Instant::now();
    let found = crate::shadow::find_all(&work);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::BackScanUs,
        scanning.elapsed().as_micros() as u64,
    );
    let mut non_empty_ranges = 0usize;
    let mut found_bytes = 0usize;
    for pieces in &found {
        if !pieces.is_empty() {
            non_empty_ranges += 1;
            found_bytes += pieces.iter().map(|&(_, n)| n).sum::<usize>();
        }
    }
    if non_empty_ranges == 0 {
        #[cfg(feature = "diagnostics")]
        {
            let entries: Vec<(usize, usize, usize)> = work
                .iter()
                .map(|(_, from, to)| (*from, to - from, 0))
                .collect();
            crate::copystat::compared_batch(&entries);
        }
        return;
    }
    let mut written_per_work = vec![0usize; work.len()];
    let mut by_shadow: Vec<Vec<(usize, Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)>> =
        Vec::new();
    let mut shadow_pos: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (idx, ((shadow, _, _), pieces)) in work.iter().zip(found).enumerate() {
        if pieces.is_empty() {
            continue;
        }
        let ptr = Arc::as_ptr(shadow) as usize;
        match shadow_pos.get(&ptr) {
            Some(&pos) => by_shadow[pos].push((idx, shadow.clone(), pieces)),
            None => {
                shadow_pos.insert(ptr, by_shadow.len());
                by_shadow.push(vec![(idx, shadow.clone(), pieces)]);
            }
        }
    }
    if wide_merge() && by_shadow.len() >= 2 && (non_empty_ranges >= 4 || found_bytes >= (2 << 20)) {
        let counts: Arc<Vec<std::sync::atomic::AtomicUsize>> = Arc::new(
            (0..work.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
        );
        let worker_count = by_shadow.len().min(6);
        let mut buckets: Vec<Vec<(usize, Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)>> =
            (0..worker_count).map(|_| Vec::new()).collect();
        let mut bucket_bytes = vec![0usize; worker_count];
        for group in by_shadow {
            let bytes: usize = group
                .iter()
                .flat_map(|(_, _, ps)| ps.iter())
                .map(|&(_, n)| n)
                .sum();
            let dst = bucket_bytes
                .iter()
                .enumerate()
                .min_by_key(|(_, b)| **b)
                .map_or(0, |(i, _)| i);
            bucket_bytes[dst] += bytes;
            buckets[dst].extend(group);
        }
        let jobs: Vec<Box<dyn FnOnce() + Send>> = buckets
            .into_iter()
            .filter(|b| !b.is_empty())
            .map(|chunk| {
                let counts = counts.clone();
                Box::new(move || {
                    let (mut now, mut title) = (Vec::new(), Vec::new());
                    for (idx, shadow, pieces) in chunk {
                        let mut written = 0usize;
                        for (at, n) in pieces {
                            written += shadow.relay_merge(at, n, &mut now, &mut title);
                        }
                        if written > 0 {
                            counts[idx].store(written, Ordering::Relaxed);
                        }
                    }
                }) as Box<dyn FnOnce() + Send>
            })
            .collect();
        crate::shadow::wide_run(jobs);
        for (dst, src) in written_per_work.iter_mut().zip(counts.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
    } else {
        let (mut now, mut title) = (Vec::new(), Vec::new());
        for group in by_shadow {
            for (idx, shadow, pieces) in group {
                let mut written = 0usize;
                for (at, n) in pieces {
                    written += shadow.relay_merge(at, n, &mut now, &mut title);
                }
                written_per_work[idx] = written;
            }
        }
    }
    let total_written: usize = written_per_work.iter().sum();
    if total_written > 0 {
        COPY_BACK[1].fetch_add(total_written as u64, Ordering::Relaxed);
    }
    #[cfg(feature = "diagnostics")]
    {
        let entries: Vec<(usize, usize, usize)> = work
            .iter()
            .zip(written_per_work)
            .map(|((_, from, to), written)| (*from, to - from, written))
            .collect();
        crate::copystat::compared_batch(&entries);
    }
}

fn late_line(frames: f64) {
    if !late() {
        return;
    }
    let s: Vec<f64> = LATE_STATS
        .iter()
        .map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames)
        .collect();
    tracing::info!(
        "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
         settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
         title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
         a texture {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
         bottom-of-pipe writes (OATHKEEP_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}; \
         {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
        s[0],
        s[1],
        if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
        s[12] / 1000.0,
        s[WHY_SYNC],
        s[WHY_CARD_DONE],
        s[WHY_VALUE],
        s[WHY_STALE],
        s[WHY_READ],
        s[15],
        s[16],
        s[17],
        s[WHY_RUN],
        late_most(),
        s[WHY_MOST],
        s[WHY_WRITES],
        s[WHY_FENCE],
        s[13],
        s[14],
    );
    {
        let w: Vec<f64> = LATE_WAITED
            .iter()
            .map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames)
            .collect();
        tracing::info!(
            "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
             the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
             title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}",
            w[WHY_SYNC],
            w[WHY_CARD_DONE],
            w[WHY_VALUE],
            w[WHY_STALE],
            w[WHY_READ],
            w[WHY_RUN],
            w[WHY_MOST],
            w[WHY_WRITES],
            w[WHY_FENCE],
        );
    }
    if relay() {
        let r: Vec<f64> = RELAY_STATS
            .iter()
            .map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames)
            .collect();
        tracing::info!(
            "the relay per frame (OATHKEEP_RELAY=1): {:.1} flights landed in {:.1} landings by the thread that copies back, \
             beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
             fetches of card copies' pieces (copy queue {:.1}x {:.1} ms, 3D queue {:.1}x {:.1} ms), skipped {:.0} uncommitted \
             pieces ({:.1} MB), deferred {:.1} busy copy fetches ({:.1} drained without waiting), and was {:.1} ms landing in all \
             (the frame line's \"copying back\" is its time; \"settles\" above are waits of other threads for it)",
            r[1],
            r[0],
            r[2] / 1000.0,
            r[3] / 1000.0,
            r[5],
            r[6] / 1000.0,
            r[7],
            r[8] / 1000.0,
            r[9],
            r[10] / 1e6,
            r[11],
            r[12],
            r[4] / 1000.0,
        );
    }
    if late_exact() {
        let [pages, runs, bytes] = crate::shadow::merged_stats().map(|n| n as f64 / frames);
        tracing::info!(
            "title bytes put in under flights' shader writes per frame (OATHKEEP_LATE_WAIT=2): {pages:.0} pages looked at, \
             {runs:.0} changed runs, {:.2} MB",
            bytes / (1 << 20) as f64
        );
    }
}

#[cfg(test)]
mod late_tests {
    use super::{continues, join_ranges, touches};

    #[test]
    fn copies_join_only_end_to_end_and_apart() {
        assert!(continues((1000, 5000, 16), (1016, 5016, 32)));
        assert!(continues((5000, 1000, 16), (5016, 1016, 32)));
        assert!(!continues((1000, 5000, 16), (1020, 5016, 32)));
        assert!(!continues((1000, 5000, 16), (1016, 5020, 32)));
        assert!(!continues((1000, 5000, 16), (1000, 5000, 16)));
        assert!(!continues((1000, 1016, 16), (1016, 1032, 16)));
        assert!(!continues((1016, 1000, 16), (1032, 1016, 16)));
        assert!(continues((1000, 1032, 16), (1016, 1048, 16)));
        assert!(!continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
    }

    #[test]
    fn ranges_touch_where_they_overlap() {
        let mut ranges = Vec::new();
        join_ranges(
            &mut ranges,
            [(40, 50), (10, 20), (20, 25), (45, 60), (7, 7)],
        );
        assert_eq!(ranges, [(10, 25), (40, 60)]);
        join_ranges(&mut ranges, [(25, 40)]);
        assert_eq!(ranges, [(10, 60)]);
        let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
        for (from, to, hit) in [
            (0, 10, false),
            (0, 11, true),
            (24, 25, true),
            (25, 40, false),
            (25, 41, true),
            (59, 200, true),
            (60, 100, false),
            (100, 101, true),
            (101, 500, false),
            (30, 30, false),
            (50, 50, false),
        ] {
            assert_eq!(touches(&ranges, from, to), hit, "{from}..{to}");
        }
        assert!(!touches(&[], 0, usize::MAX));
    }
}

fn join_copies() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_JOIN_COPIES").is_none_or(|v| v != "0"))
}

static COPIES_JOINED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

fn continues(last: (usize, usize, usize), next: (usize, usize, usize)) -> bool {
    let ((d, s, n), (dst, src, len)) = (last, next);
    let Some(all) = n.checked_add(len) else {
        return false;
    };
    d.checked_add(n) == Some(dst)
        && s.checked_add(n) == Some(src)
        && (d.checked_add(all).is_some_and(|end| end <= s)
            || s.checked_add(all).is_some_and(|end| end <= d))
}

fn declared_writes(runs: &[&HostList]) -> Vec<Declared> {
    let mut back: Vec<Declared> = Vec::new();
    let mut index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::GpuWrite(shadow, address, len) = step {
                let range = (*address, *address + *len);
                let ptr = std::sync::Arc::as_ptr(shadow) as usize;
                match index.get(&ptr) {
                    Some(&at) => back[at].1.push(range),
                    None => {
                        index.insert(ptr, back.len());
                        back.push((shadow.clone(), vec![range]));
                    }
                }
            }
        }
    }
    for (_, ranges) in &mut back {
        if ranges.len() > 1 {
            ranges.sort_unstable();
            ranges.dedup();
        }
    }
    back
}

pub(crate) fn walls_trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("OATHKEEP_WALLS_TRACE").is_some_and(|v| v == "1" || v == "2")
    })
}

fn read_backs(list: &HostList) {
    for (buffer, at, [row, rows, pitch]) in &list.deferred().backs {
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of ours, written by the list that has
        // run; the title's destination holds every row (checked when the
        // copy was recorded).
        unsafe {
            if buffer.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                let from = data.cast::<u8>().cast_const();
                for line in 0..*rows {
                    std::ptr::copy_nonoverlapping(
                        from.add(line * row.next_multiple_of(256)),
                        (*at as *mut u8).add(line * pitch),
                        *row,
                    );
                }
                if walls_trace() && *rows == 1 && *row <= 96 {
                    let mut values = [0f32; 24];
                    std::ptr::copy_nonoverlapping(
                        from,
                        values.as_mut_ptr().cast::<u8>(),
                        *row & !3,
                    );
                    tracing::info!(
                        frame = crate::frame::current(),
                        to = format_args!("{:#x}", *at),
                        bytes = *row,
                        values = ?&values[..*row / 4],
                        "walls trace: a read-back lands in title memory"
                    );
                }
                buffer.Unmap(0, None);
            }
        }
    }
}

fn hand_back(back: &[Declared]) {
    if crate::shadow::wide_back() {
        let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for (shadow, declared) in back {
            if shadow.carded() {
                continue;
            }
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            work.extend(
                spans
                    .into_iter()
                    .map(|(from, to)| (shadow.clone(), from, to)),
            );
        }
        #[cfg(feature = "diagnostics")]
        let scanning = std::time::Instant::now();
        let found = crate::shadow::find_all(&work);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::BackScanUs,
            scanning.elapsed().as_micros() as u64,
        );
        for ((shadow, from, to), found) in work.iter().zip(found) {
            let written = shadow.copy_back_with(*from, to - from, Some(found));
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written);
        }
        for (shadow, declared) in back {
            shadow.copy_back_done(declared);
        }
        return;
    }
    for (shadow, declared) in back {
        if shadow.carded() {
            shadow.copy_back_done(declared);
            continue;
        }
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (from, to) in sorted {
            match spans.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => spans.push((from, to)),
            }
        }
        for (from, to) in spans {
            let written = shadow.copy_back(from, to - from);
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
        }
        shadow.copy_back_done(declared);
    }
}

impl Host {
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    pub(crate) fn depth_bounds(&self) -> bool {
        static BOUNDS: OnceLock<bool> = OnceLock::new();
        *BOUNDS.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
            // SAFETY: writes `options` of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS2,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let supported = ok && options.DepthBoundsTestSupported.as_bool();
            tracing::info!(supported, "host depth bounds support");
            supported
        })
    }

    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let before = private_bytes();
        let list = self.make_list();
        LISTS[0].fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        LISTS[2].fetch_add(private_bytes().wrapping_sub(before), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::HostLists, 1);
            add(C::HostListNs, started.elapsed().as_nanos() as u64);
        }
        list
    }

    fn make_prelude(&self) -> Option<Prelude> {
        // SAFETY: plain object creation.
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator = self
                    .device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &allocator,
                    None,
                )?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        match made {
            Ok(prelude) => Some(prelude),
            Err(e) => {
                tracing::warn!(error = %e, "no host list for a prelude");
                None
            }
        }
    }

    fn make_copy_prelude(&self) -> Option<Prelude> {
        // SAFETY: plain object creation.
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator = self
                    .device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_COPY,
                    &allocator,
                    None,
                )?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        made.ok()
    }

    fn make_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            Ok(HostList {
                allocator,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                has_twins: std::sync::atomic::AtomicBool::new(false),
                pipelines: Mutex::default(),
                budget: OnceLock::new(),
                indirect_pool: Mutex::default(),
                gpu_scratch_pool: Mutex::default(),
                prelude_scratch_pool: Mutex::default(),
                shadow_writes: Mutex::default(),
                bindless_write_seq: std::sync::atomic::AtomicU64::new(0),
                copy_seq: std::sync::atomic::AtomicU64::new(0),
                reset_gen: std::sync::atomic::AtomicU64::new(1),
                uav_dirty: std::sync::atomic::AtomicBool::new(true),
                indirect_uav_dirty: std::sync::atomic::AtomicBool::new(true),
                indirect_epoch: std::sync::atomic::AtomicU64::new(1),
                _alive: Counted::new(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    fn card_clock(&self) -> Option<&'static CardClock> {
        static CLOCK: OnceLock<Option<CardClock>> = OnceLock::new();
        static WATCH: std::sync::Once = std::sync::Once::new();
        let clock = CLOCK
            .get_or_init(|| {
                // SAFETY: a plain fence on our device.
                let fence = unsafe {
                    self.device
                        .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
                }
                .ok()?;
                Some(CardClock {
                    fence,
                    state: Mutex::new((0, None)),
                    sent: std::sync::Condvar::new(),
                })
            })
            .as_ref()?;
        WATCH.call_once(|| {
            if let Err(e) = std::thread::Builder::new().name("oathkeep-card-clock".into()).spawn(move || card_clock_watch(clock)) {
                tracing::warn!(error = %e, "no thread for the card's clock: the frame line's idle times stay 0");
            }
        });
        Some(clock)
    }

    fn card_sent(&self) {
        let Some(clock) = self.card_clock() else {
            return;
        };
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        // SAFETY: our queue and the clock's fence.
        let _ = unsafe { self.queue.Signal(&clock.fence, state.0) };
        if let Some((since, worked_then)) = state.1.take() {
            CARD_CLOCK_US[0].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
            CARD_CLOCK_US[1].fetch_add(worked().saturating_sub(worked_then), Ordering::Relaxed);
        }
        drop(state);
        clock.sent.notify_one();
    }

    pub(crate) fn card_ready(&self) -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match self.make_card_work() {
            Ok(work) => {
                *CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no host copies in the card's memory: the compare could not be made");
                false
            }
        })
    }

    fn make_card_work(&self) -> windows::core::Result<CardWork> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        let mut merge_code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours.
        unsafe {
            D3DCompile(
                CARD_COMPARE.as_ptr().cast(),
                CARD_COMPARE.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
            D3DCompile(
                CARD_MASKED_MERGE.as_ptr().cast(),
                CARD_MASKED_MERGE.len(),
                windows::core::s!("card masked merge"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut merge_code,
                None,
            )?;
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let merge_code = merge_code.ok_or_else(windows::core::Error::empty)?;
        let list = || self.make_prelude().ok_or_else(windows::core::Error::empty);
        const RESULTS: u64 = 1 << 20;
        let (differs, data) = self
            .shared_buffer(RESULTS)
            .ok_or_else(windows::core::Error::empty)?;
        name(&differs, || "card compare results".into());
        // SAFETY: the blob's own bytes; the root signature is inside the
        // shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState =
                self.device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    })?;
            let m_bytes = std::slice::from_raw_parts(
                merge_code.GetBufferPointer().cast::<u8>(),
                merge_code.GetBufferSize(),
            );
            let merge_root: ID3D12RootSignature = self.device.CreateRootSignature(0, m_bytes)?;
            let merge_pso: ID3D12PipelineState =
                self.device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&merge_root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: m_bytes.as_ptr().cast(),
                            BytecodeLength: m_bytes.len(),
                        },
                        ..Default::default()
                    })?;
            Ok(CardWork {
                sync: vec![list()?],
                check: vec![list()?],
                fetch: list()?,
                root,
                pso,
                merge_root,
                merge_pso,
                differs: (differs, data as usize, RESULTS),
                fetched: None,
                upload_pool: Vec::new(),
                taken: 0,
                out: 0,
            })
        }
    }

    fn card_before(
        &self,
        runs: &[&HostList],
        flown: bool,
        #[cfg(feature = "diagnostics")] prep: &mut CardPrepUs,
    ) -> Option<CardPlan> {
        use std::sync::Arc;
        if !crate::shadow::cards() {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let prep_write_start = std::time::Instant::now();
        let mut declared: Vec<(Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)> = Vec::new();
        let mut decl_index: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, address, len) = step
                    && shadow.carded()
                {
                    let range = (*address, *address + *len);
                    let ptr = Arc::as_ptr(shadow) as usize;
                    match decl_index.get(&ptr) {
                        Some(&at) => declared[at].1.push(range),
                        None => {
                            decl_index.insert(ptr, declared.len());
                            declared.push((shadow.clone(), vec![range]));
                        }
                    }
                }
            }
        }
        for (shadow, ranges) in &mut declared {
            let mut merged = Vec::new();
            join_ranges(&mut merged, std::mem::take(ranges));
            for &(from, to) in &merged {
                shadow.card_prepare_write(from, to);
            }
            *ranges = merged;
        }
        let sent_as = if relay() {
            with_queue_sync(QueueSync::next_order)
        } else {
            0
        };
        let (mut dirty, masked) = crate::shadow::card_dirty(sent_as);
        #[cfg(feature = "diagnostics")]
        {
            prep.prepare_write += prep_write_start.elapsed().as_micros() as u64;
        }
        if declared.is_empty() && dirty.is_empty() && masked.is_empty() {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let record_phase_start = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let mut fill_upload_us = 0u64;
        let mut written_shadows: Vec<usize> =
            Vec::with_capacity(declared.len() + dirty.len() + masked.len());
        let mut seen_written: std::collections::HashSet<usize> =
            std::collections::HashSet::with_capacity(declared.len() + dirty.len() + masked.len());
        for (s, _) in &declared {
            let ptr = Arc::as_ptr(s) as usize;
            if seen_written.insert(ptr) {
                written_shadows.push(ptr);
            }
        }
        for (s, ranges) in &dirty {
            if !ranges.is_empty() {
                let ptr = Arc::as_ptr(s) as usize;
                if seen_written.insert(ptr) {
                    written_shadows.push(ptr);
                }
            }
        }
        for (s, items) in &masked {
            if !items.is_empty() {
                let ptr = Arc::as_ptr(s) as usize;
                if seen_written.insert(ptr) {
                    written_shadows.push(ptr);
                }
            }
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_mut()?;
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for (shadow, _) in &declared {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::card_hash_bytes(size))?;
                name(&before, || {
                    "title buffer on the card, hashes of its pieces".into()
                });
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new()));
                }
            }
        }
        let mut used: Vec<(bool, Prelude)> = Vec::new();
        let mut plan_uploads: Vec<(ID3D12Resource, usize, u64)> = Vec::new();
        let mut sync_cmd: Option<ID3D12CommandList> = None;
        if !dirty.is_empty() || !masked.is_empty() {
            let Some(sync) = work.sync.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!(
                    "no host list to bring host copies in the card's memory up to date"
                ));
            };
            let (allocator, list) = &sync;
            let total_copy: u64 = dirty
                .iter()
                .flat_map(|(_, rs)| rs.iter())
                .map(|&(a, b)| (b - a) as u64)
                .sum();
            let take_upload = |pool: &mut Vec<(ID3D12Resource, usize, u64)>,
                               want: u64|
             -> (ID3D12Resource, usize, u64) {
                if let Some((idx, _)) = pool
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, _, sz))| *sz >= want)
                    .min_by_key(|(_, (_, _, sz))| *sz)
                {
                    return pool.swap_remove(idx);
                }
                let cap = want
                    .max(256 << 10)
                    .next_multiple_of(256 << 10)
                    .min(CARD_UPLOAD_CHUNK);
                let Some((buf, ptr)) = self.upload_buffer(cap) else {
                    fail(format_args!(
                        "no upload scratch buffer ({cap} bytes) to update host copies on the card"
                    ));
                };
                name(&buf, || "card upload scratch".into());
                (buf, ptr as usize, cap)
            };
            let emit_hashes = |list: &ID3D12GraphicsCommandList,
                               copied_in_this_list: &std::collections::HashSet<usize>,
                               uav_in_this_list: &std::collections::HashSet<usize>| {
                let mut targets: Vec<(&Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)> =
                    Vec::with_capacity(dirty.len() + masked.len());
                let mut target_idx: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::with_capacity(dirty.len() + masked.len());
                for (shadow, ranges) in &dirty {
                    let ptr = Arc::as_ptr(shadow) as usize;
                    let idx = *target_idx.entry(ptr).or_insert_with(|| {
                        let i = targets.len();
                        targets.push((shadow, Vec::new()));
                        i
                    });
                    targets[idx].1.extend_from_slice(ranges);
                }
                for (shadow, items) in &masked {
                    let ptr = Arc::as_ptr(shadow) as usize;
                    let idx = *target_idx.entry(ptr).or_insert_with(|| {
                        let i = targets.len();
                        targets.push((shadow, Vec::new()));
                        i
                    });
                    for item in items {
                        targets[idx]
                            .1
                            .push((item.offset, item.offset + item.data.len()));
                    }
                }
                for (shadow, ranges) in &targets {
                    let Some((Some(before), size)) = shadow.card_sides() else {
                        continue;
                    };
                    let ptr = Arc::as_ptr(shadow) as usize;
                    // SAFETY: live buffers and compute root signature/PSO.
                    unsafe {
                        if uav_in_this_list.contains(&ptr) {
                            list.ResourceBarrier(&[transition(
                                &shadow.buffer,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                            )]);
                        } else if copied_in_this_list.contains(&ptr) {
                            list.ResourceBarrier(&[transition(
                                &shadow.buffer,
                                D3D12_RESOURCE_STATE_COPY_DEST,
                                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                            )]);
                        }
                        list.SetComputeRootSignature(&work.root);
                        list.SetPipelineState(&work.pso);
                        list.SetComputeRootShaderResourceView(
                            1,
                            shadow.buffer.GetGPUVirtualAddress(),
                        );
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(
                            3,
                            work.differs.0.GetGPUVirtualAddress(),
                        );
                        let runs: Vec<crate::shadow::CardSpan> =
                            if whole.iter().any(|s| Arc::ptr_eq(s, shadow)) {
                                vec![shadow.card_run(0, usize::MAX)]
                            } else {
                                ranges.iter().map(|&(a, b)| shadow.card_run(a, b)).collect()
                            };
                        let runs = if rehash_runs() {
                            crate::shadow::joined_piece_runs(runs)
                        } else {
                            runs
                        };
                        CARD_PASSES[0].fetch_add(runs.len() as u64, Ordering::Relaxed);
                        CARD_PASSES[1].fetch_add(
                            runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>(),
                            Ordering::Relaxed,
                        );
                        for run in runs {
                            let constants = [run.first, run.skew, size as u32, run.pieces, 0, 0];
                            list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                            list.Dispatch(run.pieces.div_ceil(64), 1, 1);
                        }
                    }
                }
            };
            // SAFETY: the list's last run finished (it ran ahead of a
            // submission that was waited for, and came back after that);
            // live buffers, and the ranges lie in them. All upload chunks
            // are recorded into ONE command list with zero mid-call GPU waits.
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    if total_copy == 0 && masked.is_empty() {
                        allocator.Reset()?;
                        list.Reset(allocator, None)?;
                        let empty = std::collections::HashSet::new();
                        emit_hashes(list, &empty, &empty);
                        list.Close()?;
                        list.cast()
                    } else {
                        for (_, ranges) in &dirty {
                            crate::shadow::CARD_STATS[1]
                                .fetch_add(ranges.len() as u64, Ordering::Relaxed);
                        }
                        let chunks = if total_copy > 0 {
                            plan_upload_chunks(&dirty, CARD_UPLOAD_CHUNK as usize)
                        } else {
                            Vec::new()
                        };
                        plan_uploads.reserve(chunks.len());
                        for &(chunk_bytes, _) in &chunks {
                            plan_uploads.push(take_upload(&mut work.upload_pool, chunk_bytes));
                        }
                        #[cfg(feature = "diagnostics")]
                        let fill_start = std::time::Instant::now();
                        if total_copy >= CARD_UPLOAD_WIDE_MIN {
                            let mut tasks: Vec<(Arc<crate::shadow::Shadow>, usize, usize, usize)> =
                                Vec::new();
                            for (ci, (_, items)) in chunks.iter().enumerate() {
                                let base_ptr = plan_uploads[ci].1;
                                for &(si, a, len, dst_off) in items {
                                    let shadow = &dirty[si].0;
                                    let mut off = 0usize;
                                    while off < len {
                                        let part = (len - off).min(CARD_UPLOAD_SPLIT);
                                        tasks.push((
                                            shadow.clone(),
                                            a + off,
                                            part,
                                            base_ptr + dst_off + off,
                                        ));
                                        off += part;
                                    }
                                }
                            }
                            if tasks.len() >= 2 {
                                let worker_count = tasks.len().min(6);
                                let mut buckets: Vec<
                                    Vec<(Arc<crate::shadow::Shadow>, usize, usize, usize)>,
                                > = (0..worker_count).map(|_| Vec::new()).collect();
                                let mut bucket_bytes = vec![0usize; worker_count];
                                tasks.sort_unstable_by(|a, b| b.2.cmp(&a.2));
                                for task in tasks {
                                    let dst = bucket_bytes
                                        .iter()
                                        .enumerate()
                                        .min_by_key(|(_, b)| **b)
                                        .map_or(0, |(i, _)| i);
                                    bucket_bytes[dst] += task.2;
                                    buckets[dst].push(task);
                                }
                                let jobs: Vec<Box<dyn FnOnce() + Send>> = buckets
                                    .into_iter()
                                    .filter(|b| !b.is_empty())
                                    .map(|bucket| {
                                        Box::new(move || {
                                            for (shadow, a, len, dst_ptr) in bucket {
                                                shadow.card_fill_upload(a, len, dst_ptr as *mut u8);
                                            }
                                        }) as Box<dyn FnOnce() + Send>
                                    })
                                    .collect();
                                crate::shadow::wide_run(jobs);
                            } else {
                                for (shadow, a, len, dst_ptr) in tasks {
                                    shadow.card_fill_upload(a, len, dst_ptr as *mut u8);
                                }
                            }
                        } else if total_copy > 0 {
                            for (ci, (_, items)) in chunks.iter().enumerate() {
                                let base_ptr = plan_uploads[ci].1;
                                for &(si, a, len, dst_off) in items {
                                    dirty[si].0.card_fill_upload(
                                        a,
                                        len,
                                        (base_ptr + dst_off) as *mut u8,
                                    );
                                }
                            }
                        }
                        // Pack GPU masked-merge payloads `(data, mask)` into upload buffers.
                        let mut masked_ops: Vec<(usize, usize, u32, u32, u32, u32)> = Vec::new();
                        let mut total_masked_bytes = 0u64;
                        if !masked.is_empty() {
                            let max_chunk = CARD_UPLOAD_CHUNK as usize;
                            let mut m_chunks: Vec<(u64, Vec<(usize, usize, usize)>)> = Vec::new();
                            let mut cur: Vec<(usize, usize, usize)> = Vec::new();
                            let mut cur_bytes = 0usize;
                            for (mi, (_, items)) in masked.iter().enumerate() {
                                for (ii, item) in items.iter().enumerate() {
                                    let need = item.data.len() * 2;
                                    if need == 0 || need > max_chunk {
                                        continue;
                                    }
                                    if cur_bytes + need > max_chunk && !cur.is_empty() {
                                        m_chunks.push((cur_bytes as u64, std::mem::take(&mut cur)));
                                        cur_bytes = 0;
                                    }
                                    cur.push((mi, ii, cur_bytes));
                                    cur_bytes += need;
                                    total_masked_bytes += item.data.len() as u64;
                                }
                            }
                            if !cur.is_empty() {
                                m_chunks.push((cur_bytes as u64, cur));
                            }
                            for (chunk_bytes, items) in m_chunks {
                                let upload_idx = plan_uploads.len();
                                plan_uploads.push(take_upload(&mut work.upload_pool, chunk_bytes));
                                let base_ptr = plan_uploads[upload_idx].1;
                                for (mi, ii, off) in items {
                                    let item = &masked[mi].1[ii];
                                    let len = item.data.len();
                                    let data_dst = (base_ptr + off) as *mut u8;
                                    let mask_dst = (base_ptr + off + len) as *mut u8;
                                    std::ptr::copy_nonoverlapping(item.data.as_ptr(), data_dst, len);
                                    std::ptr::copy_nonoverlapping(item.mask.as_ptr(), mask_dst, len);
                                    masked_ops.push((
                                        mi,
                                        upload_idx,
                                        item.offset as u32,
                                        off as u32,
                                        (off + len) as u32,
                                        (len / 4) as u32,
                                    ));
                                }
                            }
                        }
                        #[cfg(feature = "diagnostics")]
                        {
                            fill_upload_us += fill_start.elapsed().as_micros() as u64;
                        }
                        crate::shadow::CARD_STATS[0]
                            .fetch_add(total_copy + total_masked_bytes, Ordering::Relaxed);
                        if !masked_ops.is_empty() {
                            crate::shadow::CARD_STATS[1]
                                .fetch_add(masked_ops.len() as u64, Ordering::Relaxed);
                        }
                        allocator.Reset()?;
                        list.Reset(allocator, None)?;
                        let mut copied_shadows: std::collections::HashSet<usize> =
                            std::collections::HashSet::with_capacity(dirty.len());
                        for (ci, (_, items)) in chunks.iter().enumerate() {
                            let upload_buf = &plan_uploads[ci].0;
                            for &(si, a, len, dst_off) in items {
                                let shadow = &dirty[si].0;
                                list.CopyBufferRegion(
                                    &shadow.buffer,
                                    a as u64,
                                    upload_buf,
                                    dst_off as u64,
                                    len as u64,
                                );
                                copied_shadows.insert(Arc::as_ptr(shadow) as usize);
                            }
                        }
                        let mut uav_shadows: std::collections::HashSet<usize> =
                            std::collections::HashSet::with_capacity(masked.len());
                        if !masked_ops.is_empty() {
                            for (shadow, items) in &masked {
                                if items.is_empty() {
                                    continue;
                                }
                                let ptr = Arc::as_ptr(shadow) as usize;
                                if uav_shadows.insert(ptr) {
                                    let before_state = if copied_shadows.remove(&ptr) {
                                        D3D12_RESOURCE_STATE_COPY_DEST
                                    } else {
                                        D3D12_RESOURCE_STATE_COMMON
                                    };
                                    list.ResourceBarrier(&[transition(
                                        &shadow.buffer,
                                        before_state,
                                        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    )]);
                                }
                            }
                            list.SetComputeRootSignature(&work.merge_root);
                            list.SetPipelineState(&work.merge_pso);
                            for &(mi, upload_idx, dst_off, data_off, mask_off, dwords) in &masked_ops {
                                if dwords == 0 {
                                    continue;
                                }
                                let shadow = &masked[mi].0;
                                let constants = [dst_off, data_off, mask_off, dwords];
                                list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
                                list.SetComputeRootShaderResourceView(
                                    1,
                                    plan_uploads[upload_idx].0.GetGPUVirtualAddress(),
                                );
                                list.SetComputeRootUnorderedAccessView(
                                    2,
                                    shadow.buffer.GetGPUVirtualAddress(),
                                );
                                list.Dispatch(dwords.div_ceil(64), 1, 1);
                            }
                        }
                        emit_hashes(list, &copied_shadows, &uav_shadows);
                        list.Close()?;
                        list.cast()
                    }
                })()
            };
            match recorded {
                Ok(cmd) => sync_cmd = Some(cmd),
                Err(e) => fail(format_args!(
                    "host copies in the card's memory could not be brought up to date ({e})"
                )),
            }
            used.push((false, sync));
        }
        let mut plan = CardPlan {
            ranges: Vec::new(),
            compares: Vec::new(),
            sync: sync_cmd,
            check: None,
            _synced: dirty
                .into_iter()
                .map(|(shadow, _)| shadow)
                .chain(masked.into_iter().map(|(shadow, _)| shadow))
                .collect(),
            written_shadows,
            lists: used,
            uploads: plan_uploads,
            order_seq: sent_as,
        };
        let mut at = work.taken;
        for (shadow, spans) in &declared {
            let twin = shadow
                .card_sides()
                .is_some_and(|(before, _)| before.is_some());
            let mut shadow_spans: Vec<crate::shadow::CardSpan> = Vec::with_capacity(spans.len());
            for &(from, to) in spans {
                let Some(span) = shadow.card_span(from, to) else {
                    continue;
                };
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (from, to),
                    span,
                });
                shadow_spans.push(span);
            }
            // Coalesce overlapping/adjacent 64 KiB piece runs per shadow so each piece
            // is hashed at most once per submission in `CARD_COMPARE`.
            for run in crate::shadow::joined_piece_runs(shadow_spans) {
                let compared = twin && u64::from(at + run.pieces) * 4 <= work.differs.2;
                plan.compares.push(CardCompare {
                    shadow: shadow.clone(),
                    span: run,
                    at,
                    compared,
                });
                if compared {
                    at += run.pieces;
                }
            }
        }
        work.taken = at;
        if plan.compares.iter().any(|c| c.compared) {
            let Some(check) = work.check.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!(
                    "no host list for the compare of host copies in the card's memory"
                ));
            };
            let (allocator, list) = &check;
            // SAFETY: the list's last run finished (its submission was
            // waited for, and it came back after that); live buffers; the
            // compare's own root signature.
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                    list.SetComputeRootSignature(&work.root);
                    list.SetPipelineState(&work.pso);
                    list.SetComputeRootUnorderedAccessView(
                        3,
                        work.differs.0.GetGPUVirtualAddress(),
                    );
                    for c in plan.compares.iter().filter(|c| c.compared) {
                        let Some((Some(before), size)) = c.shadow.card_sides() else {
                            continue;
                        };
                        let constants = [
                            c.span.first,
                            c.span.skew,
                            size as u32,
                            c.span.pieces,
                            c.at * 4,
                            if flown { 2 } else { 1 },
                        ];
                        list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                        list.SetComputeRootShaderResourceView(
                            1,
                            c.shadow.buffer.GetGPUVirtualAddress(),
                        );
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        list.Dispatch(c.span.pieces.div_ceil(64), 1, 1);
                        CARD_PASSES[2].fetch_add(1, Ordering::Relaxed);
                    }
                    list.Close()?;
                    list.cast()
                })()
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => fail(format_args!(
                    "the compare of host copies in the card's memory could not be recorded ({e})"
                )),
            }
            plan.lists.push((true, check));
        }
        work.out += 1;
        #[cfg(feature = "diagnostics")]
        {
            let total_after_prepare = record_phase_start.elapsed().as_micros() as u64;
            prep.fill_upload += fill_upload_us;
            prep.record += total_after_prepare.saturating_sub(fill_upload_us);
        }
        Some(plan)
    }

    fn card_results_low(&self) -> bool {
        CARD_WORK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|work| u64::from(work.taken) * 8 > work.differs.2)
    }

    fn card_done(&self, plan: CardPlan) {
        let mut excess = Vec::new();
        {
            let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            let Some(work) = work.as_mut() else {
                return;
            };
            for (check, list) in plan.lists {
                if check {
                    work.check.push(list)
                } else {
                    work.sync.push(list)
                }
            }
            let mut pool_bytes: u64 = work.upload_pool.iter().map(|(_, _, sz)| *sz).sum();
            for item in plan.uploads {
                if pool_bytes + item.2 <= CARD_UPLOAD_POOL_MAX {
                    pool_bytes += item.2;
                    work.upload_pool.push(item);
                } else {
                    excess.push(item.0);
                }
            }
            work.out = work.out.saturating_sub(1);
            if !relay() || work.out == 0 {
                work.taken = 0;
            }
        }
        if !excess.is_empty() {
            defer_drop(SendDrop(excess));
        }
    }

    fn wait_copy_queue_on_3d(&self, shadows: &[usize]) -> windows::core::Result<()> {
        if shadows.is_empty() || (self.copy_fence.is_none() && self.copy_wait_fence.is_none()) {
            return Ok(());
        }
        let (need_copy, need_copy_wait) = with_queue_sync(|sync| {
            (
                sync.max_copy_read_for(shadows),
                sync.max_copy_wait_read_for(shadows),
            )
        });
        // SAFETY: plain fence queries and cross-queue GPU waits on live host queues.
        unsafe {
            if need_copy > 0
                && let Some(copy_fence) = &self.copy_fence
                && copy_fence.GetCompletedValue() < need_copy
            {
                self.queue.Wait(copy_fence, need_copy)?;
            }
            if need_copy_wait > 0
                && let Some(copy_wait_fence) = &self.copy_wait_fence
                && copy_wait_fence.GetCompletedValue() < need_copy_wait
            {
                self.queue.Wait(copy_wait_fence, need_copy_wait)?;
            }
        }
        Ok(())
    }

    fn make_relay_fetch(&self) -> Option<RelayFetch> {
        let list = self.make_prelude()?;
        let copy_list = self.copy_queue.as_ref().and_then(|_| self.make_copy_prelude());
        // SAFETY: plain objects of our own.
        unsafe {
            let mut copy_wait_slots = Vec::new();
            if self.copy_queue.is_some() {
                for _ in 0..COPY_WAIT_SLOTS {
                    if let (Some(prelude), Ok(event)) = (
                        self.make_copy_prelude(),
                        CreateEventW(None, false, false, None),
                    ) {
                        copy_wait_slots.push(CopyWaitSlot {
                            prelude,
                            event,
                            buffer: None,
                            pending: None,
                        });
                    }
                }
            }
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            let copy_event = CreateEventW(None, false, false, None).ok()?;
            let copy_wake_event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                copy_list,
                copy_wait_slots,
                pending_order: std::collections::VecDeque::with_capacity(COPY_WAIT_SLOTS),
                copy_wake_event,
                max_landed_seq: 0,
                fence,
                event,
                sent: 0,
                copy_event,
                buffer: None,
                copy_buffer: None,
            })
        }
    }

    fn sync_line_pending(f: &RelayFetch, notify: bool) {
        let pending_copies = f.pending_order.len();
        let pending_written = f.pending_written_ranges();
        let pending_writes = f.pending_deferred_writes();
        {
            let mut line = line();
            line.pending_copies = pending_copies;
            line.pending_written = pending_written;
            line.pending_writes = pending_writes;
            rebuild_line_written(&mut line);
        }
        if notify {
            LINE_LANDED.notify_all();
        }
    }

    fn finish_pending_slot(&self, f: &mut RelayFetch, slot_idx: usize, wait_blocking: bool) -> bool {
        let Some((copy_sent, isolated_wait)) = f.copy_wait_slots[slot_idx]
            .pending
            .as_ref()
            .map(|p| (p.copy_sent, p.isolated_wait))
        else {
            return true;
        };
        let Some(copy_fence) = (if isolated_wait {
            self.copy_wait_fence.as_ref().or(self.copy_fence.as_ref())
        } else {
            self.copy_fence.as_ref()
        }) else {
            return false;
        };
        // SAFETY: plain fence query and optional event wait on this thread's slot event.
        let completed = unsafe { copy_fence.GetCompletedValue() };
        if completed < copy_sent {
            if !wait_blocking {
                return false;
            }
            let wait_start = std::time::Instant::now();
            let ev = f.copy_wait_slots[slot_idx].event;
            // SAFETY: copy queue fence and this slot's own event; loop guards against stale wakes.
            let waited = unsafe {
                (|| -> windows::core::Result<()> {
                    while copy_fence.GetCompletedValue() < copy_sent {
                        let _ = ResetEvent(ev);
                        copy_fence.SetEventOnCompletion(copy_sent, ev)?;
                        WaitForSingleObject(ev, INFINITE);
                    }
                    Ok(())
                })()
            };
            if let Err(e) = waited {
                fail(format_args!(
                    "what shaders wrote to the card's memory could not be awaited on the copy queue ({e}, OATHKEEP_RELAY=1)"
                ));
            }
            let us = wait_start.elapsed().as_micros() as u64;
            RELAY_STATS[5].fetch_add(1, Ordering::Relaxed);
            RELAY_STATS[6].fetch_add(us, Ordering::Relaxed);
            RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
            crate::shadow::CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            crate::shadow::CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
        } else {
            RELAY_STATS[12].fetch_add(1, Ordering::Relaxed);
        }
        let pending = f.copy_wait_slots[slot_idx].pending.take().unwrap();
        let merge_items = pending
            .items
            .into_iter()
            .map(|(shadow, lo, hi, at)| {
                let ptr = std::sync::Arc::as_ptr(&shadow) as usize;
                let after = pending
                    .after_by_shadow
                    .get(&ptr)
                    .copied()
                    .unwrap_or(pending.landed_seq);
                (shadow, lo, hi, pending.dst_data + at as usize, after)
            })
            .collect();
        let written = Self::merge_relay_pieces(merge_items);
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        for (shadow, ranges) in pending.deferred_done {
            shadow.copy_back_done(&ranges);
        }
        for item in pending.deferred_sync {
            match item {
                Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                Late::Signal(fence, value) => {
                    crate::fence::set(fence as crate::object::This, value)
                }
                Late::Run(_) => {}
            }
        }
        true
    }

    fn drain_pending_slots(
        &self,
        f: &mut RelayFetch,
        up_to: usize,
        wait_blocking: bool,
        sync_line: bool,
    ) -> usize {
        let mut drained = 0usize;
        while drained < up_to {
            let Some(&slot_idx) = f.pending_order.front() else {
                break;
            };
            if !self.finish_pending_slot(f, slot_idx, wait_blocking) {
                break;
            }
            f.pending_order.pop_front();
            drained += 1;
        }
        if drained > 0 {
            let safe = f.safe_fetched_seq();
            if safe > 0 {
                crate::shadow::relay_fetched(safe);
            }
            if sync_line {
                Self::sync_line_pending(f, true);
            }
        }
        drained
    }

    fn drain_completed_pending_copies(&self, fetch: &mut Option<RelayFetch>) -> usize {
        let Some(f) = fetch.as_mut() else {
            return 0;
        };
        let n = f.pending_order.len();
        self.drain_pending_slots(f, n, false, true)
    }

    fn drain_all_pending_copies(&self, fetch: &mut Option<RelayFetch>) -> usize {
        let Some(f) = fetch.as_mut() else {
            return 0;
        };
        let n = f.pending_order.len();
        self.drain_pending_slots(f, n, true, true)
    }

    fn merge_relay_pieces(pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, usize, u64)>) -> usize {
        use std::sync::Arc;
        if pieces.is_empty() {
            return 0;
        }
        // Group pieces by shadow so each shadow runs a single batched merge (snapshot `skip` once,
        // coalesce `watch::writing` and `clear_hot_hashes`, and fan out across workers even when
        // a single large shadow dominates the landing).
        let mut by_shadow: Vec<(Arc<crate::shadow::Shadow>, Vec<(usize, usize, usize, u64)>, usize)> =
            Vec::new();
        let mut shadow_pos: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for (shadow, lo, hi, src, after) in pieces {
            let len = hi - lo;
            let ptr = Arc::as_ptr(&shadow) as usize;
            match shadow_pos.get(&ptr) {
                Some(&idx) => {
                    by_shadow[idx].1.push((lo, src, len, after));
                    by_shadow[idx].2 += len;
                }
                None => {
                    shadow_pos.insert(ptr, by_shadow.len());
                    by_shadow.push((shadow, vec![(lo, src, len, after)], len));
                }
            }
        }
        let merge_one = |shadow: &Arc<crate::shadow::Shadow>,
                         items: &[(usize, usize, usize, u64)],
                         fetched: usize,
                         parallel: bool|
         -> usize {
            let slices: Vec<(usize, &[u8], u64)> = items
                .iter()
                .map(|&(lo, src, len, after)| {
                    // SAFETY: the mapping of the fetched bytes; the copy into it has completed
                    // before `merge_relay_pieces` is called, and every job joins before returning.
                    let gpu = unsafe { std::slice::from_raw_parts(src as *const u8, len) };
                    (lo, gpu, after)
                })
                .collect();
            let merged = shadow.card_merge_relay_batch(&slices, parallel);
            shadow.walls_merged(fetched, merged);
            #[cfg(feature = "diagnostics")]
            {
                let (s_start, s_len) = shadow.span();
                crate::copystat::card_back(
                    s_start as usize,
                    s_len as usize,
                    items.len(),
                    fetched,
                    merged,
                );
            }
            merged
        };
        let wide = wide_merge();
        let mut large = Vec::new();
        let mut small = Vec::new();
        for group in by_shadow {
            if wide && (group.1.len() >= 16 || group.2 >= (1 << 20)) {
                large.push(group);
            } else {
                small.push(group);
            }
        }
        let mut total_written = 0usize;
        for (shadow, items, fetched) in large {
            total_written += merge_one(&shadow, &items, fetched, true);
        }
        let small_pieces: usize = small.iter().map(|g| g.1.len()).sum();
        if wide && small.len() >= 2 && small_pieces >= 8 {
            let written_atomic = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let worker_count = small.len().min(6);
            let mut buckets: Vec<Vec<(Arc<crate::shadow::Shadow>, Vec<(usize, usize, usize, u64)>, usize)>> =
                (0..worker_count).map(|_| Vec::new()).collect();
            let mut bucket_bytes = vec![0usize; worker_count];
            small.sort_unstable_by(|a, b| b.2.cmp(&a.2));
            for group in small {
                let dst = bucket_bytes
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, b)| **b)
                    .map_or(0, |(i, _)| i);
                bucket_bytes[dst] += group.2;
                buckets[dst].push(group);
            }
            let jobs: Vec<Box<dyn FnOnce() + Send>> = buckets
                .into_iter()
                .filter(|b| !b.is_empty())
                .map(|chunk| {
                    let written_atomic = written_atomic.clone();
                    Box::new(move || {
                        let mut local = 0usize;
                        for (shadow, items, fetched) in chunk {
                            local += merge_one(&shadow, &items, fetched, false);
                        }
                        if local > 0 {
                            written_atomic.fetch_add(local, Ordering::Relaxed);
                        }
                    }) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::wide_run(jobs);
            total_written += written_atomic.load(Ordering::Relaxed);
        } else {
            for (shadow, items, fetched) in small {
                total_written += merge_one(&shadow, &items, fetched, false);
            }
        }
        total_written
    }

    fn relay_card_back(
        &self,
        plans: &[&CardPlan],
        busy_shadows: &std::collections::HashSet<usize>,
        fetch: &mut Option<RelayFetch>,
        must_drain: bool,
    ) -> Option<(usize, std::collections::HashSet<usize>)> {
        use std::sync::Arc;
        let landed_seq = plans.iter().map(|p| p.order_seq).max().unwrap_or(0);
        if let Some(f) = fetch.as_mut() {
            f.max_landed_seq = f.max_landed_seq.max(landed_seq);
            let n = f.pending_order.len();
            if n > 0 {
                self.drain_pending_slots(f, n, false, false);
            }
        }
        let differs = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            work.as_ref()
                .map(|work| (work.differs.1, (work.differs.2 / 4) as usize))
        };
        // Collect declared `(lo, hi)` byte intervals per `(shadow_ptr, piece_idx)` and merge
        // overlapping/adjacent sub-ranges within each 64 KiB piece so no byte is fetched or
        // merged more than once per landing.
        let mut by_piece: std::collections::HashMap<
            (usize, u32),
            (Arc<crate::shadow::Shadow>, Vec<(usize, usize)>),
        > = std::collections::HashMap::new();
        if let Some((differs, dwords)) = differs {
            // SAFETY: the mapping of the results. The compares of these
            // plans have run, and their dwords are given to no other plan
            // before these are done with (`card_done`).
            let differs = unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) };
            let mut plan_flagged: std::collections::HashSet<(usize, u32)> =
                std::collections::HashSet::new();
            for plan in plans {
                plan_flagged.clear();
                for cmp in &plan.compares {
                    let copy = Arc::as_ptr(&cmp.shadow) as usize;
                    for p in 0..cmp.span.pieces {
                        if !cmp.compared
                            || differs.get((cmp.at + p) as usize).is_none_or(|&d| d != 0)
                        {
                            plan_flagged.insert((copy, cmp.span.first + p));
                        }
                    }
                    crate::shadow::CARD_STATS[2]
                        .fetch_add(u64::from(cmp.span.pieces), Ordering::Relaxed);
                }
                if plan_flagged.is_empty() {
                    continue;
                }
                for r in &plan.ranges {
                    let copy = Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let piece_idx = r.span.first + p;
                        if !plan_flagged.contains(&(copy, piece_idx)) {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo < hi {
                            by_piece
                                .entry((copy, piece_idx))
                                .or_insert_with(|| (r.shadow.clone(), Vec::new()))
                                .1
                                .push((lo, hi));
                        }
                    }
                }
            }
        }
        if by_piece.is_empty() {
            if must_drain && let Some(f) = fetch.as_mut() {
                let n = f.pending_order.len();
                self.drain_pending_slots(f, n, true, false);
            }
            let safe = fetch
                .as_ref()
                .map_or(landed_seq, RelayFetch::safe_fetched_seq);
            if safe > 0 {
                crate::shadow::relay_fetched(safe);
            }
            return None;
        }
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        let Some(f) = fetch.as_mut() else {
            fail(format_args!(
                "no list, fence or event to fetch what shaders wrote to the card's memory (OATHKEEP_RELAY=1)"
            ));
        };
        f.max_landed_seq = f.max_landed_seq.max(landed_seq);
        let can_copy_queue = self.copy_queue.is_some()
            && self.copy_fence.is_some()
            && f.copy_list.is_some();
        let can_split_copy = can_copy_queue && !f.copy_wait_slots.is_empty();
        // Sort by `(shadow_ptr, piece_idx)` so `ReadableCache` resolves contiguous VM
        // reservations with a single `VirtualQuery` instead of once per piece.
        let mut sorted_pieces: Vec<((usize, u32), (Arc<crate::shadow::Shadow>, Vec<(usize, usize)>))> =
            by_piece.into_iter().collect();
        sorted_pieces.sort_unstable_by_key(|&((ptr, piece_idx), _)| (ptr, piece_idx));

        // Filter pieces outside `queue_sync()` so `VirtualQuery` and `card.dirty`/`sent`
        // locks never stall 3D queue submissions; then lock `queue_sync()` briefly with a
        // fresh `completed_3d` only to partition into `fast_now` vs `fast_wait`.
        let skip_uncommitted = relay_skip_uncommitted();
        let mut readable_cache = ReadableCache::default();
        let mut skipped_pieces = 0u64;
        let mut skipped_bytes = 0u64;
        let mut live_pieces: Vec<(usize, Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for ((ptr, _), (shadow, mut intervals)) in sorted_pieces {
            merge_piece_intervals(&mut intervals);
            let s_start = shadow.span().0 as usize;
            for (lo, hi) in intervals {
                let len = hi - lo;
                if (skip_uncommitted && !readable_cache.any_readable(s_start + lo, len))
                    || !shadow.card_has_old_piece(lo, len)
                    || shadow.card_piece_is_superseded(lo, hi, landed_seq)
                {
                    skipped_pieces += 1;
                    skipped_bytes += len as u64;
                    continue;
                }
                live_pieces.push((ptr, shadow.clone(), lo, hi));
            }
        }
        let mut fast_now: Vec<(Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        let mut fast_wait: Vec<(Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        let mut slow: Vec<(Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        if !live_pieces.is_empty() {
            if can_copy_queue {
                // SAFETY: plain fence query.
                let completed_3d = unsafe { self.fence.GetCompletedValue() };
                let guard = queue_sync();
                for (ptr, shadow, lo, hi) in live_pieces {
                    let busy = busy_shadows.contains(&ptr)
                        || f.has_pending_isolated_for_shadow(ptr)
                        || guard
                            .as_ref()
                            .is_some_and(|sync| sync.is_shadow_busy(ptr, completed_3d));
                    if busy && can_split_copy {
                        fast_wait.push((shadow, lo, hi, 0));
                    } else {
                        fast_now.push((shadow, lo, hi, 0));
                    }
                }
            } else {
                for (_, shadow, lo, hi) in live_pieces {
                    slow.push((shadow, lo, hi, 0));
                }
            }
        }
        if skipped_pieces > 0 {
            RELAY_STATS[9].fetch_add(skipped_pieces, Ordering::Relaxed);
            RELAY_STATS[10].fetch_add(skipped_bytes, Ordering::Relaxed);
        }
        if fast_now.is_empty() && fast_wait.is_empty() && slow.is_empty() {
            if must_drain {
                let n = f.pending_order.len();
                self.drain_pending_slots(f, n, true, false);
            }
            let safe = f.safe_fetched_seq();
            if safe > 0 {
                crate::shadow::relay_fetched(safe);
            }
            return None;
        }
        let prepare_batch = |batch: &mut [(Arc<crate::shadow::Shadow>, usize, usize, u64)]| -> u64 {
            batch.sort_unstable_by_key(|p| (Arc::as_ptr(&p.0) as usize, p.1));
            let mut total = 0u64;
            for p in batch.iter_mut() {
                p.3 = total;
                total += (p.2 - p.1) as u64;
            }
            total
        };
        let fast_now_total = prepare_batch(&mut fast_now);
        let fast_wait_total = prepare_batch(&mut fast_wait);
        let slow_total = prepare_batch(&mut slow);
        let total = fast_now_total + fast_wait_total + slow_total;
        let piece_count = fast_now.len() + fast_wait.len() + slow.len();

        // Pipeline `fast_now` asynchronously into a `copy_wait_slots` entry on `copy_queue`
        // whenever no thread is actively waiting (`!must_drain`) so the relay thread never
        // blocks synchronously on PCIe copy-queue roundtrips between landings. Shadows that
        // already have an in-flight `isolated_wait` fetch were routed to `fast_wait` above,
        // so `fast_now` stays strictly FIFO on `copy_queue` + `pending_order`.
        let async_fast_now = !fast_now.is_empty() && !must_drain && can_split_copy;

        // Keep per-shadow merge ordering strictly FIFO when `fast_now` merges synchronously.
        if !fast_now.is_empty() && !async_fast_now && f.has_pending() {
            let mut fast_now_ptrs: Vec<usize> = fast_now
                .iter()
                .map(|(s, ..)| Arc::as_ptr(s) as usize)
                .collect();
            fast_now_ptrs.sort_unstable();
            fast_now_ptrs.dedup();
            if let Some(pos) = f.last_pending_pos_for_shadows(&fast_now_ptrs) {
                self.drain_pending_slots(f, pos + 1, true, false);
            }
        }

        let ensure_readback =
            |slot: &mut Option<(ID3D12Resource, usize, u64)>, need: u64| -> Option<(ID3D12Resource, usize)> {
                if need == 0 {
                    return None;
                }
                if slot.as_ref().is_none_or(|b| b.2 < need) {
                    let size = need.next_multiple_of(4 << 20);
                    *slot = self
                        .read_back_mapped(size)
                        .map(|(buffer, data)| (buffer, data as usize, size));
                }
                let Some((buffer, data, _)) = slot.clone() else {
                    fail(format_args!(
                        "no memory to fetch what shaders wrote to the card's memory ({need} bytes, OATHKEEP_RELAY=1)"
                    ));
                };
                Some((buffer, data))
            };
        let fast_now_sync_buf = if !async_fast_now {
            ensure_readback(&mut f.copy_buffer, fast_now_total)
        } else {
            None
        };
        let slow_buf = ensure_readback(&mut f.buffer, slow_total);

        let build_copy_regions =
            |batch: &[(Arc<crate::shadow::Shadow>, usize, usize, u64)]| -> (Vec<(usize, usize, usize, u64)>, Vec<usize>) {
                let mut shadow_by_ptr: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();
                let mut shadow_ptrs = Vec::new();
                for (i, (s, ..)) in batch.iter().enumerate() {
                    let ptr = Arc::as_ptr(s) as usize;
                    shadow_by_ptr.entry(ptr).or_insert_with(|| {
                        shadow_ptrs.push(ptr);
                        i
                    });
                }
                let keyed: Vec<(usize, usize, usize, u64)> = batch
                    .iter()
                    .map(|(s, lo, hi, at)| (shadow_by_ptr[&(Arc::as_ptr(s) as usize)], *lo, *hi, *at))
                    .collect();
                (coalesce_copy_regions(&keyed), shadow_ptrs)
            };

        let submit_copy_batch = |batch: &[(Arc<crate::shadow::Shadow>, usize, usize, u64)],
                                 prelude: &Prelude,
                                 dst_buf: &ID3D12Resource,
                                 use_wait_queue: bool|
         -> (u64, bool, Vec<usize>, std::collections::HashMap<usize, u64>) {
            let isolated_wait =
                use_wait_queue && self.copy_wait_queue.is_some() && self.copy_wait_fence.is_some();
            let copy_queue = if isolated_wait {
                self.copy_wait_queue.as_ref().unwrap()
            } else {
                self.copy_queue.as_ref().unwrap()
            };
            let copy_fence = if isolated_wait {
                self.copy_wait_fence.as_ref().unwrap()
            } else {
                self.copy_fence.as_ref().unwrap()
            };
            let (allocator, list) = prelude;
            let (copy_regions, shadow_ptrs) = build_copy_regions(batch);
            // SAFETY: this copy list's previous run was awaited; buffers decay to COMMON
            // after every `ExecuteCommandLists` on `self.queue`; cross-queue `Wait` under
            // `QUEUE_SYNC` orders this read strictly after any pending 3D write to these
            // shadows and strictly before any subsequent 3D write to these shadows.
            let submitted = unsafe {
                (|| -> windows::core::Result<(u64, bool, Vec<usize>, std::collections::HashMap<usize, u64>)> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for &(shadow_idx, lo, hi, at) in &copy_regions {
                        list.CopyBufferRegion(
                            dst_buf,
                            at,
                            &batch[shadow_idx].0.buffer,
                            lo as u64,
                            (hi - lo) as u64,
                        );
                    }
                    list.Close()?;
                    let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                    let completed_3d = self.fence.GetCompletedValue();
                    let completed_copy = copy_fence.GetCompletedValue();
                    let mut sync_guard = queue_sync();
                    let sync = sync_guard.get_or_insert_with(QueueSync::new);
                    let (wait_fence, after_by_shadow) =
                        sync.sync_for_copy_fetch(&shadow_ptrs, completed_3d, landed_seq);
                    if wait_fence > completed_3d {
                        copy_queue.Wait(&self.fence, wait_fence)?;
                    }
                    copy_queue.ExecuteCommandLists(&lists);
                    let copy_sent = if isolated_wait {
                        sync.copy_wait_sent += 1;
                        let sent = sync.copy_wait_sent;
                        copy_queue.Signal(copy_fence, sent)?;
                        sync.record_copy_wait_reads(&shadow_ptrs, sent, completed_copy);
                        sent
                    } else {
                        sync.copy_sent += 1;
                        let sent = sync.copy_sent;
                        copy_queue.Signal(copy_fence, sent)?;
                        sync.record_copy_reads(&shadow_ptrs, sent, completed_copy);
                        sent
                    };
                    Ok((copy_sent, isolated_wait, shadow_ptrs, after_by_shadow))
                })()
            };
            match submitted {
                Ok(res) => res,
                Err(e) => fail(format_args!(
                    "what shaders wrote to the card's memory could not be fetched on the copy queue ({e}, OATHKEEP_RELAY=1)"
                )),
            }
        };

        let mut waits = 0u64;
        let mut wait_us_total = 0u64;
        let mut total_written = 0usize;
        let mut deferred_ptrs: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut last_deferred_slot: Option<usize> = None;

        let mut enqueue_copy_slot =
            |f: &mut RelayFetch,
             batch: Vec<(Arc<crate::shadow::Shadow>, usize, usize, u64)>,
             need: u64,
             use_wait_queue: bool| {
                let slot_idx = if let Some(idx) = f.free_slot() {
                    Some(idx)
                } else {
                    let n = f.pending_order.len();
                    self.drain_pending_slots(f, n, false, false);
                    if let Some(idx) = f.free_slot() {
                        Some(idx)
                    } else {
                        self.drain_pending_slots(f, 1, true, false);
                        f.free_slot()
                    }
                };
                let Some(slot_idx) = slot_idx else {
                    return;
                };
                let (dst_buf, dst_data) =
                    ensure_readback(&mut f.copy_wait_slots[slot_idx].buffer, need).unwrap();
                let (copy_sent, isolated_wait, shadow_ptrs, after_by_shadow) = submit_copy_batch(
                    &batch,
                    &f.copy_wait_slots[slot_idx].prelude,
                    &dst_buf,
                    use_wait_queue,
                );
                let min_after = after_by_shadow
                    .values()
                    .copied()
                    .min()
                    .unwrap_or(landed_seq);
                let mut written_ranges = Vec::new();
                join_ranges(
                    &mut written_ranges,
                    batch.iter().map(|(s, lo, hi, _)| {
                        let start = s.span().0 as usize;
                        (start + *lo, start + *hi)
                    }),
                );
                deferred_ptrs.extend(shadow_ptrs.iter().copied());
                f.copy_wait_slots[slot_idx].pending = Some(PendingCopyFetch {
                    copy_sent,
                    isolated_wait,
                    dst_data,
                    items: batch,
                    after_by_shadow,
                    shadow_ptrs,
                    landed_seq,
                    min_after,
                    written_ranges,
                    deferred_done: Vec::new(),
                    deferred_sync: Vec::new(),
                });
                f.pending_order.push_back(slot_idx);
                RELAY_STATS[11].fetch_add(1, Ordering::Relaxed);
                last_deferred_slot = Some(slot_idx);
            };

        // Submit `fast_now` (on `copy_queue` with no 3D wait, pipelined into a slot when
        // `!must_drain`) and `fast_wait` (pipelined on isolated `copy_wait_queue` behind the
        // busy 3D fence so it never head-of-line blocks `fast_now`).
        let fast_now_submitted = if async_fast_now {
            enqueue_copy_slot(f, std::mem::take(&mut fast_now), fast_now_total, false);
            None
        } else if !fast_now.is_empty() {
            let (dst_buf, dst_data) = fast_now_sync_buf.as_ref().unwrap();
            let (copy_sent, _, _, after_by_shadow) =
                submit_copy_batch(&fast_now, f.copy_list.as_ref().unwrap(), dst_buf, false);
            Some((copy_sent, *dst_data, after_by_shadow))
        } else {
            None
        };
        if !fast_wait.is_empty() {
            enqueue_copy_slot(f, fast_wait, fast_wait_total, true);
        }
        let mut deferred_result = last_deferred_slot.map(|slot_idx| (slot_idx, deferred_ptrs));

        let slow_submitted = if !slow.is_empty() {
            f.sent += 1;
            let ((allocator, list), fence, event, sent) =
                (f.list.clone(), f.fence.clone(), f.event, f.sent);
            let (buffer, slow_data) = slow_buf.as_ref().unwrap();
            let (copy_regions, _) = build_copy_regions(&slow);
            // SAFETY: fallback 3D queue path when copy queue is disabled; list's last run
            // was awaited by this thread.
            let submitted = unsafe {
                (|| -> windows::core::Result<u64> {
                    allocator.Reset()?;
                    list.Reset(&allocator, None)?;
                    for &(shadow_idx, lo, hi, at) in &copy_regions {
                        list.CopyBufferRegion(
                            buffer,
                            at,
                            &slow[shadow_idx].0.buffer,
                            lo as u64,
                            (hi - lo) as u64,
                        );
                    }
                    list.Close()?;
                    let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                    let after = with_queue_sync(|sync| {
                        self.queue.ExecuteCommandLists(&lists);
                        sync.order
                    });
                    self.card_sent();
                    self.queue.Signal(&fence, sent)?;
                    Ok(after)
                })()
            };
            match submitted {
                Ok(after) => Some((fence, event, sent, *slow_data, after)),
                Err(e) => fail(format_args!(
                    "what shaders wrote to the card's memory could not be fetched ({e}, OATHKEEP_RELAY=1)"
                )),
            }
        } else {
            None
        };

        if let Some((copy_sent, dst_data, after_by_shadow)) = fast_now_submitted {
            let copy_fence = self.copy_fence.as_ref().unwrap();
            let wait_start = std::time::Instant::now();
            // SAFETY: copy queue fence and this thread's copy event.
            let waited = unsafe {
                (|| -> windows::core::Result<()> {
                    while copy_fence.GetCompletedValue() < copy_sent {
                        let _ = ResetEvent(f.copy_event);
                        copy_fence.SetEventOnCompletion(copy_sent, f.copy_event)?;
                        WaitForSingleObject(f.copy_event, INFINITE);
                    }
                    Ok(())
                })()
            };
            if let Err(e) = waited {
                fail(format_args!(
                    "what shaders wrote to the card's memory could not be awaited on the copy queue ({e}, OATHKEEP_RELAY=1)"
                ));
            }
            let us = wait_start.elapsed().as_micros() as u64;
            RELAY_STATS[5].fetch_add(1, Ordering::Relaxed);
            RELAY_STATS[6].fetch_add(us, Ordering::Relaxed);
            wait_us_total += us;
            waits += 1;
            let items = fast_now
                .into_iter()
                .map(|(shadow, lo, hi, at)| {
                    let ptr = Arc::as_ptr(&shadow) as usize;
                    let after = after_by_shadow.get(&ptr).copied().unwrap_or(landed_seq);
                    (shadow, lo, hi, dst_data + at as usize, after)
                })
                .collect();
            total_written += Self::merge_relay_pieces(items);
        }

        if let Some((fence, event, sent, slow_data, slow_after)) = slow_submitted {
            let wait_start = std::time::Instant::now();
            // SAFETY: 3D queue fence and this thread's event.
            let waited = unsafe {
                fence.SetEventOnCompletion(sent, event).map(|()| {
                    WaitForSingleObject(event, INFINITE);
                })
            };
            if let Err(e) = waited {
                fail(format_args!(
                    "what shaders wrote to the card's memory could not be awaited ({e}, OATHKEEP_RELAY=1)"
                ));
            }
            let us = wait_start.elapsed().as_micros() as u64;
            RELAY_STATS[7].fetch_add(1, Ordering::Relaxed);
            RELAY_STATS[8].fetch_add(us, Ordering::Relaxed);
            wait_us_total += us;
            waits += 1;
            let items = slow
                .into_iter()
                .map(|(shadow, lo, hi, at)| (shadow, lo, hi, slow_data + at as usize, slow_after))
                .collect();
            total_written += Self::merge_relay_pieces(items);
        }

        // Opportunistically reap any completed pending copy slots (non-blocking), or drain
        // all pending slots synchronously when a waiter / fence signal / memory write needs
        // full memory visibility right now.
        if f.has_pending() {
            let n = f.pending_order.len();
            self.drain_pending_slots(f, n, must_drain, false);
            if must_drain {
                deferred_result = None;
            } else if let Some((slot_idx, _)) = &deferred_result
                && f.copy_wait_slots[*slot_idx].pending.is_none()
            {
                deferred_result = None;
            }
        }

        RELAY_STATS[3].fetch_add(wait_us_total, Ordering::Relaxed);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add(piece_count as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(waits, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(wait_us_total, Ordering::Relaxed);
        }
        COPY_BACK[1].fetch_add(total_written as u64, Ordering::Relaxed);
        let safe = f.safe_fetched_seq();
        if safe > 0 {
            crate::shadow::relay_fetched(safe);
        }
        deferred_result
    }

    fn relay_land(
        &self,
        items: Vec<Late>,
        busy_shadows: &std::collections::HashSet<usize>,
        fetch: &mut Option<RelayFetch>,
        has_waiters: bool,
    ) {
        let landing = std::time::Instant::now();
        // Only force synchronous draining of in-flight deferred copy fetches when a thread
        // is actively waiting (`settle_for_present`, `settle_for_fence`, `wait_for_value`).
        // Otherwise, attach `Late::Write`/`Late::Signal` to the newest pending copy slot so
        // strict GPU-writeback-before-signal visibility is preserved without stalling the
        // relay thread on a later 3D flight.
        let must_drain = has_waiters;
        let had_flights;
        {
            let flights: Vec<&Flight> = items
                .iter()
                .filter_map(|l| match l {
                    Late::Run(flight) => Some(flight),
                    _ => None,
                })
                .collect();
            had_flights = !flights.is_empty();
            if had_flights {
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = flights
                        .iter()
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    fail(format_args!(
                        "host GPU device removed after {} submissions sent ahead (OATHKEEP_RELAY=1) of {} lists{}{}",
                        flights.len(),
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                let plans: Vec<&CardPlan> =
                    flights.iter().filter_map(|f| f.card.as_ref()).collect();
                let deferred = self.relay_card_back(&plans, busy_shadows, fetch, must_drain);
                let mut back: Vec<Declared> = Vec::new();
                let mut back_index: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();
                for flight in &flights {
                    for (shadow, ranges) in &flight.back {
                        if shadow.carded() {
                            continue;
                        }
                        let ptr = std::sync::Arc::as_ptr(shadow) as usize;
                        match back_index.get(&ptr) {
                            Some(&at) => back[at].1.extend_from_slice(ranges),
                            None => {
                                back_index.insert(ptr, back.len());
                                back.push((shadow.clone(), ranges.clone()));
                            }
                        }
                    }
                }
                for (_, ranges) in &mut back {
                    if ranges.len() > 1 {
                        ranges.sort_unstable();
                        ranges.dedup();
                    }
                }
                relay_hand_back(&back);
                if let Some((slot_idx, deferred_ptrs)) = deferred
                    && let Some(f) = fetch.as_mut()
                    && let Some(pending) = f.copy_wait_slots[slot_idx].pending.as_mut()
                {
                    for flight in &flights {
                        for (shadow, ranges) in &flight.back {
                            let ptr = std::sync::Arc::as_ptr(shadow) as usize;
                            if deferred_ptrs.contains(&ptr) {
                                pending.deferred_done.push((shadow.clone(), ranges.clone()));
                            } else {
                                shadow.copy_back_done(ranges);
                            }
                        }
                    }
                } else {
                    for flight in &flights {
                        for (shadow, ranges) in &flight.back {
                            shadow.copy_back_done(ranges);
                        }
                    }
                }
                COPY_BACK[0].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
                RELAY_STATS[0].fetch_add(1, Ordering::Relaxed);
                RELAY_STATS[1].fetch_add(flights.len() as u64, Ordering::Relaxed);
            }
        }
        if let Some(f) = fetch.as_mut()
            && f.has_pending()
            && (must_drain || !had_flights)
        {
            let n = f.pending_order.len();
            self.drain_pending_slots(f, n, must_drain, false);
        }
        let defer_sync_slot = fetch
            .as_ref()
            .and_then(|f| f.pending_order.back().copied());
        let mut last = 0;
        let mut landed = Vec::new();
        for item in items {
            match item {
                Late::Run(flight) => {
                    last = flight.number;
                    for &i in &flight.order {
                        read_backs(flight.lists[i].host());
                    }
                    if let Some(card) = flight.card {
                        self.card_done(card);
                    }
                    landed.push((flight.lists, flight.order));
                }
                sync_item @ (Late::Write(..) | Late::Signal(..)) => {
                    if let Some(slot_idx) = defer_sync_slot
                        && let Some(f) = fetch.as_mut()
                        && let Some(pending) = f.copy_wait_slots[slot_idx].pending.as_mut()
                    {
                        pending.deferred_sync.push(sync_item);
                    } else {
                        match sync_item {
                            Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                            Late::Signal(fence, value) => {
                                crate::fence::set(fence as crate::object::This, value)
                            }
                            Late::Run(_) => {}
                        }
                    }
                }
            }
        }
        if last > 0 {
            FINISHED.store(last, Ordering::Release);
            bury(last);
        }
        self.log_messages();
        RELAY_STATS[4].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
        let (pending_copies, pending_written, pending_writes) =
            fetch.as_ref().map_or((0, Vec::new(), Vec::new()), |f| {
                (
                    f.pending_order.len(),
                    f.pending_written_ranges(),
                    f.pending_deferred_writes(),
                )
            });
        {
            let mut line = line();
            line.landed.extend(landed);
            line.in_hand.clear();
            line.pending_copies = pending_copies;
            line.pending_written = pending_written;
            line.pending_writes = pending_writes;
            rebuild_line_written(&mut line);
            line.settling = false;
            line.poke = true;
        }
        LINE_MOVED.notify_all();
        LINE_LANDED.notify_all();
    }

    fn card_back(
        &self,
        plan: &CardPlan,
        submitted: &mut u64,
        seen: &mut std::collections::HashSet<(usize, usize, usize)>,
    ) {
        if plan.ranges.is_empty() {
            return;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        // SAFETY: the mapping of the results, which the compare (it has
        // run) wrote.
        let differs = unsafe {
            std::slice::from_raw_parts(work.differs.1 as *const u32, (work.differs.2 / 4) as usize)
        };
        let mut flagged: std::collections::HashSet<(usize, u32)> =
            std::collections::HashSet::new();
        for cmp in &plan.compares {
            let copy = std::sync::Arc::as_ptr(&cmp.shadow) as usize;
            for p in 0..cmp.span.pieces {
                if !cmp.compared || differs.get((cmp.at + p) as usize).is_none_or(|&d| d != 0) {
                    flagged.insert((copy, cmp.span.first + p));
                }
            }
            crate::shadow::CARD_STATS[2].fetch_add(u64::from(cmp.span.pieces), Ordering::Relaxed);
        }
        let mut by_piece: std::collections::HashMap<(usize, u32), (usize, Vec<(usize, usize)>)> =
            std::collections::HashMap::new();
        if !flagged.is_empty() {
            for (i, r) in plan.ranges.iter().enumerate() {
                let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                for p in 0..r.span.pieces {
                    let piece_idx = r.span.first + p;
                    if !flagged.contains(&(copy, piece_idx)) {
                        continue;
                    }
                    let (lo, hi) = r.shadow.card_piece(&r.span, p);
                    if lo < hi {
                        by_piece
                            .entry((copy, piece_idx))
                            .or_insert_with(|| (i, Vec::new()))
                            .1
                            .push((lo, hi));
                    }
                }
            }
        }
        let mut pieces: Vec<(usize, usize, usize, u64)> = Vec::new();
        for ((copy, _), (range_idx, mut intervals)) in by_piece {
            merge_piece_intervals(&mut intervals);
            for (lo, hi) in intervals {
                if seen.insert((copy, lo, hi)) {
                    pieces.push((range_idx, lo, hi, 0));
                }
            }
        }
        pieces.sort_unstable_by_key(|&(i, lo, _, _)| {
            (std::sync::Arc::as_ptr(&plan.ranges[i].shadow) as usize, lo)
        });
        let mut total = 0u64;
        for p in &mut pieces {
            p.3 = total;
            total += (p.2 - p.1) as u64;
        }
        let mut written = vec![0usize; plan.ranges.len()];
        if !pieces.is_empty() {
            if work.fetched.as_ref().is_none_or(|f| f.2 < total) {
                let size = total.next_multiple_of(4 << 20);
                work.fetched = self
                    .read_back_mapped(size)
                    .map(|(buffer, data)| (buffer, data as usize, size));
            }
            let Some((fetched, data, _)) = &work.fetched else {
                fail(format_args!(
                    "no memory to fetch what shaders wrote to the card's memory ({total} bytes)"
                ));
            };
            let copy_regions: Vec<(usize, usize, usize, u64)> = {
                let mut shadow_by_ptr: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();
                for &(i, ..) in &pieces {
                    shadow_by_ptr
                        .entry(std::sync::Arc::as_ptr(&plan.ranges[i].shadow) as usize)
                        .or_insert(i);
                }
                let keyed: Vec<(usize, usize, usize, u64)> = pieces
                    .iter()
                    .map(|&(i, lo, hi, at)| {
                        let key = shadow_by_ptr
                            [&(std::sync::Arc::as_ptr(&plan.ranges[i].shadow) as usize)];
                        (key, lo, hi, at)
                    })
                    .collect();
                coalesce_copy_regions(&keyed)
            };
            let waiting = std::time::Instant::now();
            let (allocator, list) = &work.fetch;
            // SAFETY: the list's last run was waited for; live buffers; the
            // fence and event are ours, under the submission lock.
            let ran = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for &(i, lo, hi, at) in &copy_regions {
                        list.CopyBufferRegion(
                            fetched,
                            at,
                            &plan.ranges[i].shadow.buffer,
                            lo as u64,
                            (hi - lo) as u64,
                        );
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    self.card_sent();
                    *submitted += 1;
                    self.queue.Signal(&self.fence, *submitted)?;
                    self.fence.SetEventOnCompletion(*submitted, self.event)?;
                    let was_working = working(false);
                    WaitForSingleObject(self.event, INFINITE);
                    stood(waiting);
                    working(was_working);
                    Ok(())
                })()
            };
            if let Err(e) = ran {
                fail(format_args!(
                    "what shaders wrote to the card's memory could not be fetched ({e})"
                ));
            }
            {
                use crate::shadow::CARD_STATS;
                CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
                CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
                CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
                CARD_STATS[6].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            let fetched_ptr = *data;
            let mut by_shadow: Vec<Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, usize, u64)>> =
                Vec::new();
            let mut shadow_pos: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            for &(i, lo, hi, at) in &pieces {
                let shadow = plan.ranges[i].shadow.clone();
                let ptr = std::sync::Arc::as_ptr(&shadow) as usize;
                let item = (shadow, i, lo, hi, at);
                match shadow_pos.get(&ptr) {
                    Some(&idx) => by_shadow[idx].push(item),
                    None => {
                        shadow_pos.insert(ptr, by_shadow.len());
                        by_shadow.push(vec![item]);
                    }
                }
            }
            if wide_merge() && by_shadow.len() >= 2 && pieces.len() >= 8 {
                let counts: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>> =
                    std::sync::Arc::new(
                        (0..plan.ranges.len())
                            .map(|_| std::sync::atomic::AtomicUsize::new(0))
                            .collect(),
                    );
                let worker_count = by_shadow.len().min(8);
                let mut buckets: Vec<Vec<_>> = (0..worker_count).map(|_| Vec::new()).collect();
                let mut bucket_bytes = vec![0usize; worker_count];
                for group in by_shadow {
                    let bytes: usize = group.iter().map(|p| p.3 - p.2).sum();
                    let dst = bucket_bytes
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, b)| **b)
                        .map_or(0, |(idx, _)| idx);
                    bucket_bytes[dst] += bytes;
                    buckets[dst].extend(group);
                }
                let jobs: Vec<Box<dyn FnOnce() + Send>> = buckets
                    .into_iter()
                    .filter(|b| !b.is_empty())
                    .map(|chunk| {
                        let counts = counts.clone();
                        Box::new(move || {
                            let mut title = Vec::new();
                            for (shadow, i, lo, hi, at) in chunk {
                                // SAFETY: the mapping of the fetched bytes;
                                // the copy into it has run, and the caller
                                // waits for this job.
                                let gpu = unsafe {
                                    std::slice::from_raw_parts(
                                        (fetched_ptr + at as usize) as *const u8,
                                        hi - lo,
                                    )
                                };
                                let merged = shadow.card_merge(lo, gpu, &mut title);
                                shadow.walls_merged(gpu.len(), merged);
                                #[cfg(feature = "diagnostics")]
                                {
                                    let (s_start, s_len) = shadow.span();
                                    crate::copystat::card_back(
                                        s_start as usize,
                                        s_len as usize,
                                        1,
                                        gpu.len(),
                                        merged,
                                    );
                                }
                                counts[i].fetch_add(merged, Ordering::Relaxed);
                            }
                        }) as Box<dyn FnOnce() + Send>
                    })
                    .collect();
                crate::shadow::wide_run(jobs);
                for (written, count) in written.iter_mut().zip(counts.iter()) {
                    *written += count.load(Ordering::Relaxed);
                }
            } else {
                let mut title = Vec::new();
                for group in by_shadow {
                    for (shadow, i, lo, hi, at) in group {
                        // SAFETY: the mapping of the fetched bytes, `total` of
                        // them; the copy into it has run.
                        let gpu = unsafe {
                            std::slice::from_raw_parts(
                                (fetched_ptr + at as usize) as *const u8,
                                hi - lo,
                            )
                        };
                        let merged = shadow.card_merge(lo, gpu, &mut title);
                        shadow.walls_merged(gpu.len(), merged);
                        #[cfg(feature = "diagnostics")]
                        {
                            let (s_start, s_len) = shadow.span();
                            crate::copystat::card_back(
                                s_start as usize,
                                s_len as usize,
                                1,
                                gpu.len(),
                                merged,
                            );
                        }
                        written[i] += merged;
                    }
                }
            }
        }
        for (r, written) in plan.ranges.iter().zip(written) {
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            let (from, to) = r.declared;
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
            let _ = (from, to);
        }
    }

    pub(crate) fn execute(&self, runs: &[&HostList]) {
        let _ = self.submit(runs, false);
    }

    pub(crate) fn execute_late(&self, lists: Vec<crate::list::Detached>, order: Vec<usize>) {
        if order.is_empty() {
            crate::list::recycle(lists, &order);
            return;
        }
        late_threads();
        let sent = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            self.submit(&runs, true)
        };
        let Some((mut submitted, sent)) = sent else {
            crate::list::recycle(lists, &order);
            return;
        };
        let told = !sent.writes.is_empty();
        let most = {
            let mut line = line();
            let Line {
                line: items,
                written,
                lists: flying,
                target,
                ..
            } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied()),
            );
            *flying += order.len();
            *target = *submitted;
            items.push(Late::Run(Flight {
                number: sent.number,
                fence: *target,
                lists,
                order,
                card: sent.card,
                back: sent.back,
                _preludes: sent.preludes,
            }));
            items.extend(
                sent.writes
                    .into_iter()
                    .map(|(at, what, bytes)| Late::Write(at, what, bytes)),
            );
            *flying >= late_most()
        };
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        RELAY_MOVED.notify_one();
        if told && !late_writes() {
            self.settle(&mut submitted, WHY_WRITES);
        } else if most && relay_exact() {
            relay_wait(WHY_MOST, |line| line.lists < late_most());
        } else if most {
            self.settle(&mut submitted, WHY_MOST);
        } else {
            LINE_MOVED.notify_one();
        }
        drop(submitted);
        land();
    }

    fn execute_own(&self, list: &HostList) -> u64 {
        late_threads();
        let Some((submitted, sent)) = self.submit(&[list], true) else {
            return 0;
        };
        let number = sent.number;
        {
            let mut line = line();
            let Line {
                line: items,
                written,
                lists: flying,
                target,
                ..
            } = &mut *line;
            join_ranges(
                written,
                sent.back
                    .iter()
                    .flat_map(|(_, ranges)| ranges.iter().copied()),
            );
            *flying += 1;
            *target = *submitted;
            items.push(Late::Run(Flight {
                number,
                fence: *target,
                lists: Vec::new(),
                order: Vec::new(),
                card: sent.card,
                back: sent.back,
                _preludes: sent.preludes,
            }));
            items.extend(
                sent.writes
                    .into_iter()
                    .map(|(at, what, bytes)| Late::Write(at, what, bytes)),
            );
        }
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        drop(submitted);
        number
    }

    fn settle(&self, submitted: &mut u64, why: usize) {
        if !late() {
            return;
        }
        if relay() {
            relay_wait(why, |line| {
                line.line.is_empty() && !line.settling && line.pending_copies == 0
            });
            return;
        }
        let was_working = working(true);
        let mut counted = false;
        loop {
            let items = {
                let mut line = line();
                if line.line.is_empty() {
                    line.settling = false;
                    break;
                }
                line.settling = true;
                line.written.clear();
                line.lists = 0;
                std::mem::take(&mut line.line)
            };
            let flights = items.iter().filter(|i| matches!(i, Late::Run(_))).count();
            if !counted {
                counted = true;
                LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
            }
            LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
            if flights > 0 {
                submitting(2, flights);
                let waiting = std::time::Instant::now();
                // SAFETY: the fence and event are ours, under the
                // submission lock; `submitted` is the last value signalled.
                unsafe {
                    match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            working(false);
                            WaitForSingleObject(self.event, INFINITE);
                            stood(waiting);
                            working(true);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "submissions sent ahead not waited for")
                        }
                    }
                }
                let gpu = waiting.elapsed();
                LATE_STATS[12].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                LATE_WAITED[why.min(10)].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::card_waited(gpu.as_micros() as u64, || {
                    format!("{flights} submissions sent ahead, settled together")
                });
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = items
                        .iter()
                        .filter_map(|i| match i {
                            Late::Run(f) => Some(f),
                            _ => None,
                        })
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    fail(format_args!(
                        "host GPU device removed after {flights} submissions sent ahead (OATHKEEP_LATE_WAIT=1) of {} lists{}{}",
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                submitting(3, flights);
                let copying = std::time::Instant::now();
                let mut seen = std::collections::HashSet::new();
                let mut back: Vec<Declared> = Vec::new();
                let mut back_index: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();
                for item in &items {
                    let Late::Run(flight) = item else {
                        continue;
                    };
                    if let Some(card) = &flight.card {
                        self.card_back(card, submitted, &mut seen);
                    }
                    for (shadow, ranges) in &flight.back {
                        let ptr = std::sync::Arc::as_ptr(shadow) as usize;
                        match back_index.get(&ptr) {
                            Some(&at) => back[at].1.extend_from_slice(ranges),
                            None => {
                                back_index.insert(ptr, back.len());
                                back.push((shadow.clone(), ranges.clone()));
                            }
                        }
                    }
                }
                for (_, ranges) in &mut back {
                    if ranges.len() > 1 {
                        ranges.sort_unstable();
                        ranges.dedup();
                    }
                }
                hand_back(&back);
                COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            let mut last = 0;
            let mut landed = Vec::new();
            for item in items {
                match item {
                    Late::Run(flight) => {
                        last = flight.number;
                        for &i in &flight.order {
                            read_backs(flight.lists[i].host());
                        }
                        if let Some(card) = flight.card {
                            self.card_done(card);
                        }
                        landed.push((flight.lists, flight.order));
                    }
                    Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                    Late::Signal(fence, value) => {
                        crate::fence::set(fence as crate::object::This, value)
                    }
                }
            }
            if last > 0 {
                FINISHED.store(last, Ordering::Release);
                bury(last);
            }
            line().landed.extend(landed);
        }
        if counted {
            self.log_messages();
            submitting(0, 0);
            if why != WHY_CARD_DONE {
                line().poke = true;
                LINE_MOVED.notify_one();
            }
        }
        working(was_working);
    }

    fn submit<'a>(
        &'a self,
        runs: &[&HostList],
        late: bool,
    ) -> Option<(std::sync::MutexGuard<'a, u64>, Sent)> {
        #[cfg(feature = "diagnostics")]
        let called = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_begins();
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let was_working = working(true);
        submitting(1, runs.len());
        if !late {
            self.settle(&mut submitted, WHY_SYNC);
        } else if let Some(why) = late_blocked(runs) {
            if relay_exact() && why == WHY_READ {
                relay_wait(why, |line| blocked_by(&line.written, runs, false).is_none());
            } else {
                self.settle(&mut submitted, why);
            }
        }
        let behind = late && flying();
        static SWEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !behind {
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        } else if late_free_present() {
            let now = now_ms();
            let last_swept = SWEPT.load(Ordering::Relaxed);
            if now > last_swept + 500 {
                let behind_sweep_ms =
                    if available_commit().is_some_and(|c| c < (2560u64 << 20)) {
                        500
                    } else {
                        1500
                    };
                if now > last_swept + behind_sweep_ms {
                    self.settle(&mut submitted, WHY_SYNC);
                    sweep();
                    SWEPT.store(now_ms(), Ordering::Relaxed);
                }
            }
        }
        let behind = late && flying();
        crate::descriptor::make_waiting();
        let this_submission = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        crate::shadow::preparing(this_submission);
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let lock = (started - called).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        let mut settled = std::time::Duration::ZERO;
        #[cfg(feature = "diagnostics")]
        let (
            mut prep_ask_once,
            mut prep_run_deferred,
            mut prep_prelude,
            mut prep_batch_flush,
            mut prep_snapshots,
            mut prep_card,
        ) = (0u64, 0u64, 0u64, 0u64, 0u64, CardPrepUs::default());
        if crate::watch::once() {
            #[cfg(feature = "diagnostics")]
            let t = std::time::Instant::now();
            let mut spans: std::collections::HashMap<usize, (usize, usize)> =
                std::collections::HashMap::new();
            for list in runs {
                for step in &list.deferred().copies {
                    match step {
                        Step::Refresh(shadow, address, len)
                            if !shadow.unwatched() && !shadow.guarded() =>
                        {
                            let span = spans
                                .entry(std::sync::Arc::as_ptr(shadow) as usize)
                                .or_insert((*address, *address + *len));
                            *span = (span.0.min(*address), span.1.max(*address + *len));
                        }
                        Step::Texture(staging) => {
                            if let Some((from, to)) = staging.watch_range() {
                                let span = spans
                                    .entry(std::sync::Arc::as_ptr(staging) as usize)
                                    .or_insert((from, to));
                                *span = (span.0.min(from), span.1.max(to));
                            }
                        }
                        _ => {}
                    }
                }
            }
            let ranges: Vec<(usize, usize)> = spans.into_values().collect();
            crate::watch::ask_once(&ranges);
            #[cfg(feature = "diagnostics")]
            {
                prep_ask_once += t.elapsed().as_micros() as u64;
            }
            if behind && stale_under_flights(runs) {
                let settling = std::time::Instant::now();
                self.settle(&mut submitted, WHY_STALE);
                settled += settling.elapsed();
                #[cfg(feature = "diagnostics")]
                let t2 = std::time::Instant::now();
                crate::watch::ask_once(&ranges);
                #[cfg(feature = "diagnostics")]
                {
                    prep_ask_once += t2.elapsed().as_micros() as u64;
                }
            }
        } else if behind && stale_under_flights(runs) {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_STALE);
            settled += settling.elapsed();
        }
        let mut preludes: Vec<PooledPrelude> = Vec::new();
        for list in runs {
            #[cfg(feature = "diagnostics")]
            let t_def = std::time::Instant::now();
            list.run_deferred(&mut batch);
            #[cfg(feature = "diagnostics")]
            {
                prep_run_deferred += t_def.elapsed().as_micros() as u64;
            }
            firsts.push(lists.len());
            #[cfg(feature = "diagnostics")]
            let t_pre = std::time::Instant::now();
            if let Some((prelude_cmd, pooled)) = list.prelude(self) {
                lists.push(Some(prelude_cmd));
                preludes.push(pooled);
            }
            #[cfg(feature = "diagnostics")]
            {
                prep_prelude += t_pre.elapsed().as_micros() as u64;
            }
            lists.push(list.list.cast().ok());
        }
        {
            #[cfg(feature = "diagnostics")]
            let t_flush = std::time::Instant::now();
            batch.flush();
            crate::watch::ask_once_end();
            #[cfg(feature = "diagnostics")]
            {
                prep_batch_flush += t_flush.elapsed().as_micros() as u64;
            }
        }
        {
            #[cfg(feature = "diagnostics")]
            let snapshots = std::time::Instant::now();
            crate::shadow::take_snapshots();
            #[cfg(feature = "diagnostics")]
            {
                let us = snapshots.elapsed().as_micros() as u64;
                prep_snapshots += us;
                crate::copystat::add(crate::copystat::C::SnapshotUs, us);
            }
        }
        let flown = late && relay();
        if flown && self.card_results_low() {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_MOST);
            settled += settling.elapsed();
        }
        #[cfg(feature = "diagnostics")]
        let mut card = self.card_before(runs, flown, &mut prep_card);
        #[cfg(not(feature = "diagnostics"))]
        let mut card = self.card_before(runs, flown);
        let sync_cmd = card.as_mut().and_then(|c| c.sync.take());
        if let Some(check) = card.as_ref().and_then(|c| c.check.clone()) {
            lists.push(Some(check));
        }
        let prepared = std::time::Instant::now();
        let total_prep_us = (prepared - started).saturating_sub(settled).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            let accounted = prep_ask_once
                + prep_run_deferred
                + prep_prelude
                + prep_batch_flush
                + prep_snapshots
                + prep_card.prepare_write
                + prep_card.fill_upload
                + prep_card.record;
            add(C::PrepAskOnceUs, prep_ask_once);
            add(C::PrepRunDeferredUs, prep_run_deferred);
            add(C::PrepPreludeUs, prep_prelude);
            add(C::PrepBatchFlushUs, prep_batch_flush);
            add(C::PrepSnapshotsUs, prep_snapshots);
            add(C::PrepCardPrepareWriteUs, prep_card.prepare_write);
            add(C::PrepCardFillUploadUs, prep_card.fill_upload);
            add(C::PrepCardRecordUs, prep_card.record);
            add(C::PrepOtherUs, total_prep_us.saturating_sub(accounted));
        }
        if late {
            *submitted += 1;
            // SAFETY: closed lists; the fence is ours. `sync_cmd` runs in its own
            // `ExecuteCommandLists` call before `lists` so card buffers decay back
            // to `COMMON` before title command lists bind them.
            let signalled = unsafe {
                let completed_3d = self.fence.GetCompletedValue();
                let mut sync_guard = queue_sync();
                let sync = sync_guard.get_or_insert_with(QueueSync::new);
                let wait_res = if let Some(c) = &card {
                    let mut res = Ok(());
                    if let Some(copy_fence) = &self.copy_fence {
                        let need_copy = sync.max_copy_read_for(&c.written_shadows);
                        if need_copy > 0 && copy_fence.GetCompletedValue() < need_copy {
                            res = self.queue.Wait(copy_fence, need_copy);
                        }
                    }
                    if res.is_ok()
                        && let Some(copy_wait_fence) = &self.copy_wait_fence
                    {
                        let need_wait = sync.max_copy_wait_read_for(&c.written_shadows);
                        if need_wait > 0 && copy_wait_fence.GetCompletedValue() < need_wait {
                            res = self.queue.Wait(copy_wait_fence, need_wait);
                        }
                    }
                    res
                } else {
                    Ok(())
                };
                if wait_res.is_ok() {
                    if let Some(sync_list) = sync_cmd {
                        self.queue.ExecuteCommandLists(&[Some(sync_list)]);
                    }
                    self.queue.ExecuteCommandLists(&lists);
                    let sig = self.queue.Signal(&self.fence, *submitted);
                    if sig.is_ok()
                        && let Some(c) = &card
                    {
                        sync.record_3d_writes(
                            &c.written_shadows,
                            *submitted,
                            c.order_seq,
                            completed_3d,
                        );
                    }
                    sig
                } else {
                    wait_res
                }
            };
            self.card_sent();
            if let Err(e) = signalled {
                fail(format_args!(
                    "a submission sent ahead could not be followed by a signal ({e})"
                ));
            }
            let back = declared_writes(runs);
            let writes = runs
                .iter()
                .flat_map(|l| l.deferred().writes.clone())
                .collect();
            SUBMISSION_US[0].fetch_add(total_prep_us, Ordering::Relaxed);
            submitting(0, 0);
            #[cfg(feature = "diagnostics")]
            crate::copystat::submission_ends(lock);
            let sent = Sent {
                number: this_submission,
                card,
                back,
                writes,
                preludes,
            };
            working(was_working);
            return Some((submitted, sent));
        }
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        if let Some(c) = &card
            && let Err(e) = self.wait_copy_queue_on_3d(&c.written_shadows)
        {
            tracing::warn!(error = %e, "host submission copy-queue wait failed");
        }
        if let Some(sync_list) = sync_cmd {
            // SAFETY: closed card sync list executed separately so buffers decay to COMMON.
            unsafe {
                self.queue.ExecuteCommandLists(&[Some(sync_list)]);
            }
        }
        submitting(2, runs.len());
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                self.queue.ExecuteCommandLists(group);
                self.card_sent();
                let waited = self
                    .queue
                    .Signal(&self.fence, *submitted)
                    .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                match waited {
                    Ok(()) => {
                        working(false);
                        WaitForSingleObject(self.event, INFINITE);
                        stood(started);
                        working(true);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            #[cfg(feature = "diagnostics")]
            if timing() {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                for (ms, op) in runs[i].slowest(frequency, usize::MAX) {
                    crate::copystat::command_took(&op, ms);
                }
            }
            #[cfg(feature = "diagnostics")]
            if timing() && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i]
                    .notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
                tracing::warn!(
                    ms = started.elapsed().as_millis(),
                    commands,
                    "slow host list"
                );
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        FINISHED.store(this_submission, Ordering::Release);
        bury(this_submission);
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::card_waited(gpu.as_micros() as u64, || {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| {
                    l.pipelines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .collect();
            let text = format!("{} lists, pipelines {pipelines:?}", runs.len());
            text.chars().take(1200).collect()
        });
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| {
                    l.pipelines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .collect();
            tracing::warn!(
                ms = gpu.as_millis(),
                lists = runs.len(),
                ?pipelines,
                "slow host submission"
            );
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        submitting(3, runs.len());
        let copying = std::time::Instant::now();
        if let Some(card) = &card {
            self.card_back(card, &mut submitted, &mut std::collections::HashSet::new());
        }
        let back = declared_writes(runs);
        hand_back(&back);
        if let Some(card) = card {
            self.card_done(card);
        }
        #[cfg(feature = "diagnostics")]
        if !back.is_empty() {
            crate::copystat::add(crate::copystat::C::BackSubmissions, 1);
        }
        #[cfg(feature = "diagnostics")]
        if back.is_empty() && runs.iter().all(|l| l.deferred().writes.is_empty()) {
            use crate::copystat::{C, add};
            add(C::NothingBack, 1);
            add(C::NothingBackUs, gpu.as_micros() as u64);
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        for list in runs {
            read_backs(list);
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
        submitting(0, 0);
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_ends(lock);
        if relay() {
            crate::shadow::relay_fetched(with_queue_sync(|sync| sync.order));
        }
        working(was_working);
        None
    }

    fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    pub(crate) fn rtv(
        &self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource, or none with a
        // description (a typed null view).
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC {
                                Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                                Count: STAMPS,
                                NodeMask: 0,
                            },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: (count + 1) * 8,
        };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (
                    ticks as f64 * 1e3 / frequency.max(1) as f64,
                    notes[i].0.chars().take(300).collect(),
                )
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        self.finish_indirect();
        self.twins_to_depth();
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count =
                (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list
                    .ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    pub(crate) fn mark_uav_dirty(&self) {
        self.uav_dirty.store(true, Ordering::Relaxed);
    }

    pub(crate) fn begin_draw_writes(&self) {
        let mut writes = self
            .shadow_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        writes.deferring = true;
        writes.pending.clear();
    }

    pub(crate) fn commit_draw_writes(&self) {
        let mut writes = self
            .shadow_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        writes.deferring = false;
        let pending = std::mem::take(&mut writes.pending);
        for shadow_id in pending {
            if let Some((_, seq)) = writes.seqs.iter_mut().find(|(id, _)| *id == shadow_id) {
                *seq = seq.wrapping_add(1);
            } else {
                writes.seqs.push((shadow_id, 1));
            }
        }
    }

    pub(crate) fn cancel_draw_writes(&self) {
        let mut writes = self
            .shadow_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        writes.deferring = false;
        writes.pending.clear();
    }

    pub(crate) fn note_shadow_gpu_write(&self, shadow_id: u64) {
        let mut writes = self
            .shadow_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if writes.deferring {
            if !writes.pending.contains(&shadow_id) {
                writes.pending.push(shadow_id);
            }
            return;
        }
        if let Some((_, seq)) = writes.seqs.iter_mut().find(|(id, _)| *id == shadow_id) {
            *seq = seq.wrapping_add(1);
        } else {
            writes.seqs.push((shadow_id, 1));
        }
    }

    pub(crate) fn shadow_write_seq(&self, shadow_id: u64) -> u64 {
        let writes = self
            .shadow_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        writes
            .seqs
            .iter()
            .find_map(|&(id, seq)| (id == shadow_id).then_some(seq))
            .unwrap_or(0)
    }

    pub(crate) fn note_bindless_gpu_write(&self) {
        self.bindless_write_seq.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn bindless_write_seq(&self) -> u64 {
        self.bindless_write_seq.load(Ordering::Relaxed)
    }

    pub(crate) fn copy_seq(&self) -> u64 {
        self.copy_seq.load(Ordering::Relaxed)
    }

    pub(crate) fn reset_gen(&self) -> u64 {
        self.reset_gen.load(Ordering::Relaxed)
    }

    pub(crate) fn indirect_epoch(&self) -> u64 {
        self.indirect_epoch.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_uav_clean(&self) {
        self.uav_dirty.store(false, Ordering::Relaxed);
        self.indirect_uav_dirty.store(false, Ordering::Relaxed);
        self.indirect_epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn take_uav_dirty(&self) -> bool {
        let was = self.uav_dirty.swap(false, Ordering::Relaxed);
        self.indirect_uav_dirty.store(false, Ordering::Relaxed);
        was
    }

    pub(crate) fn take_indirect_uav_dirty(&self) -> bool {
        self.indirect_uav_dirty.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn uav_barrier_if_dirty(&self) {
        if !self.take_uav_dirty() {
            return;
        }
        // SAFETY: an open direct command list; all-resource UAV barrier ensures prior
        // shader UAV writes in this list are visible to subsequent reads/copies.
        unsafe {
            self.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        pResource: std::mem::ManuallyDrop::new(None),
                    }),
                },
            }]);
        }
    }

    pub(crate) fn prelude_gpu_copy_scratch(
        &self,
        src: &ID3D12Resource,
        src_offset: u64,
        len: u64,
        align: u64,
    ) -> Option<(ID3D12Resource, u64, u64)> {
        let align = align.max(256);
        let alloc_bytes = len.max(align).next_multiple_of(align);
        let (scratch, dst_off, gpu_va, old_buf) = {
            let mut pool = self
                .prelude_scratch_pool
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let start = pool.used.next_multiple_of(align);
            if let Some(buf) = pool.buffer.clone()
                && start.saturating_add(alloc_bytes) <= pool.cap
            {
                pool.used = start + alloc_bytes;
                (buf, start, pool.gpu + start, None)
            } else {
                let new_cap = pool
                    .cap
                    .saturating_mul(2)
                    .max(alloc_bytes)
                    .max(64 << 10)
                    .next_multiple_of(64 << 10);
                let buffer = host()?.uav_buffer(new_cap)?;
                name(&buffer, || format!("prelude gpu scratch {new_cap:#x}"));
                // SAFETY: a live DEFAULT-heap buffer.
                let gpu = unsafe { buffer.GetGPUVirtualAddress() };
                let old_buf = pool.buffer.replace(buffer.clone());
                pool.gpu = gpu;
                pool.cap = new_cap;
                pool.used = alloc_bytes;
                (buffer, 0, gpu, old_buf)
            }
        };
        if let Some(old_buf) = old_buf {
            self.keep(&old_buf);
        }
        self.keep(src);
        if len > 0 {
            self.deferred()
                .prelude_gpu_copies
                .push((src.clone(), src_offset, scratch.clone(), dst_off, len));
        }
        Some((scratch, dst_off, gpu_va))
    }

    pub(crate) fn gpu_copy_scratch(
        &self,
        src: &ID3D12Resource,
        src_offset: u64,
        len: u64,
        align: u64,
    ) -> Option<(ID3D12Resource, u64, u64)> {
        let align = align.max(256);
        let alloc_bytes = len.max(align).next_multiple_of(align);
        let (scratch, dst_off, gpu_va, was_in_read, old_buf) = {
            let mut ring = self
                .gpu_scratch_pool
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let idx = ring.next % GPU_SCRATCH_RING;
            ring.next = ring.next.wrapping_add(1);
            let pool = &mut ring.slots[idx];
            let start = pool.used.next_multiple_of(align);
            if let Some(buf) = pool.buffer.clone()
                && start.saturating_add(alloc_bytes) <= pool.cap
            {
                let was_in_read = pool.in_read;
                pool.used = start + alloc_bytes;
                if len > 0 {
                    pool.in_read = true;
                }
                (buf, start, pool.gpu + start, was_in_read, None)
            } else {
                let new_cap = pool
                    .cap
                    .saturating_mul(2)
                    .max(alloc_bytes)
                    .max(64 << 10)
                    .next_multiple_of(64 << 10);
                let buffer = host()?.uav_buffer(new_cap)?;
                name(&buffer, || format!("gpu scratch {idx} {new_cap:#x}"));
                // SAFETY: a live DEFAULT-heap buffer.
                let gpu = unsafe { buffer.GetGPUVirtualAddress() };
                let old_buf = pool.buffer.replace(buffer.clone());
                pool.gpu = gpu;
                pool.cap = new_cap;
                pool.used = alloc_bytes;
                pool.in_read = len > 0;
                (buffer, 0, gpu, false, old_buf)
            }
        };
        if let Some(old_buf) = old_buf {
            self.keep(&old_buf);
        }
        self.keep(src);
        self.uav_barrier_if_dirty();
        if len > 0 {
            // SAFETY: an open direct command list; `scratch` is a live DEFAULT-heap buffer
            // (starting in COMMON on first use, or in GENERIC_READ after a prior copy), and
            // `src` is a live shadow buffer kept alive for this submission.
            unsafe {
                if was_in_read {
                    self.list.ResourceBarrier(&[transition(
                        &scratch,
                        D3D12_RESOURCE_STATE_GENERIC_READ,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    )]);
                }
                self.list
                    .CopyBufferRegion(&scratch, dst_off, src, src_offset, len);
                self.list.ResourceBarrier(&[transition(
                    &scratch,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                )]);
            }
        }
        Some((scratch, dst_off, gpu_va))
    }

    pub(crate) fn reset(&self) {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self._alive.used();
        self.uav_dirty.store(true, Ordering::Relaxed);
        self.indirect_uav_dirty.store(true, Ordering::Relaxed);
        self.indirect_epoch.fetch_add(1, Ordering::Relaxed);
        {
            let mut pool = self.indirect_pool.lock().unwrap_or_else(|e| e.into_inner());
            pool.used = 0;
            pool.in_indirect = false;
        }
        {
            let mut ring = self
                .gpu_scratch_pool
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            ring.next = 0;
            for pool in &mut ring.slots {
                pool.used = 0;
                pool.in_read = false;
            }
        }
        {
            let mut pool = self
                .prelude_scratch_pool
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pool.used = 0;
            pool.in_read = false;
        }
        {
            let mut writes = self
                .shadow_writes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            writes.seqs.clear();
            writes.deferring = false;
            writes.pending.clear();
        }
        self.bindless_write_seq.store(0, Ordering::Relaxed);
        self.copy_seq.store(0, Ordering::Relaxed);
        self.reset_gen.fetch_add(1, Ordering::Relaxed);
        let old_deferred = std::mem::take(&mut *self.deferred());
        let old_states =
            std::mem::take(&mut *self.states.lock().unwrap_or_else(|e| e.into_inner()));
        let old_twins = if self.has_twins.swap(false, Ordering::AcqRel) {
            std::mem::take(&mut *self.twins.lock().unwrap_or_else(|e| e.into_inner()))
        } else {
            std::collections::HashMap::new()
        };
        let old_pipelines =
            std::mem::take(&mut *self.pipelines.lock().unwrap_or_else(|e| e.into_inner()));
        #[cfg(feature = "diagnostics")]
        let old_notes = std::mem::take(&mut *self.notes.lock().unwrap_or_else(|e| e.into_inner()));
        let has_deferred = !old_deferred.copies.is_empty()
            || !old_deferred.uploads.is_empty()
            || !old_deferred.kept.is_empty()
            || !old_deferred.backs.is_empty()
            || !old_deferred.prelude_gpu_copies.is_empty()
            || !old_deferred.refreshed.is_empty()
            || !old_deferred.textures.is_empty()
            || !old_deferred.writes.is_empty();
        if has_deferred
            || !old_states.is_empty()
            || !old_twins.is_empty()
            || !old_pipelines.is_empty()
        {
            defer_drop(SendDrop((
                old_deferred,
                old_states,
                old_twins,
                old_pipelines,
            )));
        }
        #[cfg(feature = "diagnostics")]
        if !old_notes.is_empty() {
            defer_drop(old_notes);
        }
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
            if scratch_pool() {
                give_chunks(std::mem::take(&mut scratch.chunks));
            }
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::ResetNs,
            started.elapsed().as_nanos() as u64,
        );
    }

    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::host_resource(resource)?;
        let copy = crate::resource::host_copy(resource)?;
        let wanted = legal_state(texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some((texture, before))
            }
        }
    }

    fn settle(&self, copy: &std::sync::Arc<crate::resource::HostTexture>, wanted: u32) {
        let wanted = legal_state(&copy.texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let before = match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy.clone()));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some(before)
            }
        };
        drop(states);
        if let Some(before) = before {
            let barrier = transition(
                &copy.texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(wanted as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { self.list.ResourceBarrier(&[barrier]) };
        }
    }

    pub(crate) fn twin_use(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
    ) -> bool {
        const UNORDERED_ACCESS: u32 = 0x8;
        let Some(twin) = crate::resource::twin(resource) else {
            return false;
        };
        let Some(depth) = crate::resource::host_copy(resource) else {
            return false;
        };
        let side = usize::from(colour);
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        self.has_twins.store(true, Ordering::Release);
        let u = twins
            .entry(std::sync::Arc::as_ptr(&twin) as usize)
            .or_insert_with(|| TwinUse {
                depth: depth.clone(),
                twin: twin.clone(),
                needs: [false; 2],
                behind: [None; 2],
                writing: false,
                late: false,
            });
        let copy = match u.behind[side] {
            Some(behind) => behind,
            None => {
                u.needs[side] = true;
                false
            }
        };
        u.behind[side] = Some(false);
        match (write, colour) {
            (true, true) => u.writing = true,
            (true, false) => u.behind[1] = Some(true),
            (false, _) => {}
        }
        if copy && !colour && twin.plane.is_some() {
            u.late = true;
            crate::resource::PLANES[2].fetch_add(1, Ordering::Relaxed);
        }
        drop(twins);
        if copy {
            self.twin_copy(&depth, &twin, colour);
        } else if colour {
            self.settle(&twin.colour, UNORDERED_ACCESS);
        }
        if colour && let Some(plane) = &twin.plane {
            self.settle(plane, UNORDERED_ACCESS);
        }
        true
    }

    fn pass_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        let Some(plane) = &twin.plane else { return };
        self.settle(depth, DEPTH_WRITE);
        self.settle(plane, PIXEL_SHADER_RESOURCE);
        record_plane_pass(&self.list, &depth.texture, &plane.texture);
        self.settle(plane, UNORDERED_ACCESS);
    }

    fn twin_copy(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (from, to) = if colour {
            (depth, &twin.colour)
        } else {
            (&twin.colour, depth)
        };
        self.settle(from, COPY_SOURCE);
        self.settle(to, COPY_DEST);
        record_twin_copy(&self.list, &depth.texture, twin, colour);
        self.settle(&twin.colour, UNORDERED_ACCESS);
    }

    fn twins_to_depth(&self) {
        if !self.has_twins.load(Ordering::Acquire) {
            return;
        }
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let written: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true) || u.late)
            .map(|u| {
                let copy = u.behind[0] == Some(true);
                u.behind[0] = Some(false);
                u.late = false;
                (u.depth.clone(), u.twin.clone(), copy)
            })
            .collect();
        drop(twins);
        for (depth, twin, copy) in &written {
            if *copy {
                self.twin_copy(depth, twin, false);
            }
            self.pass_plane(depth, twin);
            self.settle(depth, SHADER_RESOURCE);
        }
    }

    pub(crate) fn twins_written(&self) {
        if !self.has_twins.load(Ordering::Acquire) {
            return;
        }
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values_mut().filter(|u| u.writing) {
            u.writing = false;
            u.behind[0] = Some(true);
        }
    }

    fn prelude(&self, host: &Host) -> Option<(ID3D12CommandList, PooledPrelude)> {
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let deferred = self.deferred();
        let prelude_copies = &deferred.prelude_gpu_copies;
        let host_state = |state: u32| D3D12_RESOURCE_STATES(state as i32);
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let mut syncs = Vec::new();
        if self.has_twins.load(Ordering::Acquire) {
            let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
            for u in twins.values() {
                let real = u.twin.behind.load(Ordering::Relaxed);
                let mut behind = [real & 1 != 0, real & 2 != 0];
                for side in [0, 1] {
                    if u.needs[side] && behind[side] {
                        syncs.push((u.depth.clone(), u.twin.clone(), side == 1));
                        behind = [false; 2];
                    }
                }
                let left = [0, 1].map(|side| u.behind[side].unwrap_or(behind[side]));
                let left = u32::from(left[0]) | (u32::from(left[1]) << 1);
                u.twin.behind.store(left, Ordering::Relaxed);
            }
        }
        let mut to_copy = Vec::new();
        let mut barriers = Vec::new();
        let mut copying = std::collections::HashMap::new();
        for (depth, twin, colour) in &syncs {
            let (from, to) = if *colour {
                (depth, &twin.colour)
            } else {
                (&twin.colour, depth)
            };
            for (texture, state) in [(from, COPY_SOURCE), (to, COPY_DEST)] {
                let key = std::sync::Arc::as_ptr(texture) as usize;
                let real = texture.state.load(Ordering::Relaxed);
                copying.insert(key, state);
                if real != state {
                    to_copy.push(transition(
                        &texture.texture,
                        host_state(real),
                        host_state(state),
                    ));
                    if !states.contains_key(&key) {
                        barriers.push(transition(
                            &texture.texture,
                            host_state(state),
                            host_state(real),
                        ));
                    }
                }
            }
        }
        barriers.extend(states.iter().filter_map(|(key, (first, last, copy))| {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            let real = copying.get(key).copied().unwrap_or(real);
            (real != *first)
                .then(|| transition(&copy.texture, host_state(real), host_state(*first)))
        }));
        if barriers.is_empty() && syncs.is_empty() && prelude_copies.is_empty() {
            return None;
        }
        let Some(pooled) = take_prelude(host) else {
            fail(format_args!("no host list for a state prelude"));
        };
        let (allocator, list) = pooled.0.as_ref().expect("pooled prelude");
        // SAFETY: the pooled prelude is not executing (returned to the pool
        // only after its submission's GPU fence completed); the textures and
        // scratch buffers are live for the duration of the submission.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !to_copy.is_empty() {
                    list.ResourceBarrier(&to_copy);
                }
                for (depth, twin, colour) in &syncs {
                    record_twin_copy(list, &depth.texture, twin, *colour);
                }
                if !prelude_copies.is_empty() {
                    // One all-resource UAV barrier ensures prior command lists' UAV writes
                    // in the same ExecuteCommandLists are visible to our hoisted CBV copies.
                    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                    }]);
                    let mut dsts: Vec<&ID3D12Resource> = Vec::new();
                    let mut i = 0;
                    while i < prelude_copies.len() {
                        let (src, src_off, dst, dst_off, len_ref) = &prelude_copies[i];
                        let mut len = *len_ref;
                        let src_raw = src.as_raw();
                        let dst_raw = dst.as_raw();
                        let mut cur_src_end = *src_off + len;
                        let mut cur_dst_end = *dst_off + len;
                        let mut j = i + 1;
                        while j < prelude_copies.len() {
                            let (nsrc, nsrc_off, ndst, ndst_off, nlen) = &prelude_copies[j];
                            if nsrc.as_raw() == src_raw
                                && ndst.as_raw() == dst_raw
                                && *nsrc_off == cur_src_end
                                && *ndst_off == cur_dst_end
                            {
                                len += *nlen;
                                cur_src_end += *nlen;
                                cur_dst_end += *nlen;
                                j += 1;
                            } else {
                                break;
                            }
                        }
                        list.CopyBufferRegion(dst, *dst_off, src, *src_off, len);
                        if !dsts.iter().any(|d| d.as_raw() == dst_raw) {
                            dsts.push(dst);
                        }
                        i = j;
                    }
                    for dst in dsts {
                        barriers.push(transition(
                            dst,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            D3D12_RESOURCE_STATE_GENERIC_READ,
                        ));
                    }
                }
                if !barriers.is_empty() {
                    list.ResourceBarrier(&barriers);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        let cmd: ID3D12CommandList = list.cast().ok()?;
        Some((cmd, pooled))
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        let dred = dred_on();
        if !dred && !timing() {
            crate::capture::op(std::ptr::from_ref(self) as usize, what);
            return;
        }
        {
            let started = std::time::Instant::now();
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps()
                && at < STAMPS - 1
            {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            crate::capture::op(std::ptr::from_ref(self) as usize, || what.clone());
            if dred {
                let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
                const UNICODE: u32 = 0;
                // SAFETY: an open list; the marker is copied when recorded.
                unsafe {
                    self.list
                        .SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
                };
            }
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
            crate::copystat::add(
                crate::copystat::C::NoteNs,
                started.elapsed().as_nanos() as u64,
            );
        }
    }

    pub(crate) fn untouched(&self) -> bool {
        {
            let deferred = self.deferred();
            if !(deferred.copies.is_empty()
                && deferred.uploads.is_empty()
                && deferred.kept.is_empty()
                && deferred.textures.is_empty()
                && deferred.writes.is_empty()
                && deferred.backs.is_empty())
            {
                return false;
            }
        }
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
            && !self.has_twins.load(Ordering::Acquire)
    }

    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.copy_seq.fetch_add(1, Ordering::Relaxed);
        let mut deferred = self.deferred();
        COPIES_JOINED[0].fetch_add(1, Ordering::Relaxed);
        let end = (dst as usize).saturating_add(len);
        deferred
            .refreshed
            .retain(|&(_, a, n)| a >= end || a.saturating_add(n) <= dst as usize);
        if let Some(Step::Copy(d, s, n)) = deferred.copies.last_mut()
            && continues((*d, *s, *n), (dst as usize, src as usize, len))
        {
            COPIES_JOINED[1].fetch_add(1, Ordering::Relaxed);
            if join_copies() {
                *n += len;
                return;
            }
        }
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
    }

    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len));
    }

    pub(crate) fn defer_read_back(
        &self,
        buffer: ID3D12Resource,
        dst: *mut u8,
        [row, rows, pitch]: [usize; 3],
    ) {
        self.deferred()
            .backs
            .push((buffer, dst as usize, [row, rows, pitch]));
    }

    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().hold_upload(buffer.clone(), size);
        Some((buffer, data))
    }

    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.deferred().hold_upload(buffer, len);
            return Some((gpu, data));
        }
        while s.current < s.chunks.len() && s.used + len > s.chunks[s.current].1 {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let size = if scratch_pool() && s.chunks.is_empty() && len <= SCRATCH_FIRST {
                SCRATCH_FIRST
            } else {
                SCRATCH_CHUNK
            };
            let chunk = take_chunk(host, size)?;
            s.chunks.push((chunk, size));
            s.used = 0;
        }
        let (data, gpu) = (s.chunks[s.current].0.data, s.chunks[s.current].0.gpu);
        let at = s.used;
        s.used += len;
        Some((gpu + at, (data + at as usize) as *mut u8))
    }

    pub(crate) fn scratch_kept(&self) -> u64 {
        let scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        scratch.chunks.iter().map(|c| c.1).sum()
    }

    pub(crate) fn budget(&self) -> Option<u64> {
        static ZERO: OnceLock<Option<ID3D12Resource>> = OnceLock::new();
        let buffer = self
            .budget
            .get_or_init(|| {
                let b = host()?.uav_buffer(256)?;
                name(&b, || "indirect budget".into());
                Some(b)
            })
            .as_ref()?;
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            let zero = ZERO
                .get_or_init(|| {
                    let (b, data) = host()?.upload_buffer(256)?;
                    name(&b, || "indirect budget zero".into());
                    // SAFETY: a freshly mapped 256-byte upload buffer.
                    unsafe { std::ptr::write_bytes(data, 0, 256) };
                    Some(b)
                })
                .as_ref()?;
            deferred.budget_zeroed = true;
            // SAFETY: an open command list, a zeroed 256-byte upload buffer,
            // and a 256-byte DEFAULT UAV buffer starting in COMMON.
            unsafe {
                self.list.CopyBufferRegion(buffer, 0, zero, 0, 256);
                self.list.ResourceBarrier(&[transition(
                    buffer,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
        }
        // SAFETY: a live buffer.
        Some(unsafe { buffer.GetGPUVirtualAddress() })
    }

    pub(crate) fn indirect_scratch(
        &self,
        size: u64,
    ) -> Option<(ID3D12Resource, u64, bool, bool)> {
        const ALIGN: u64 = 256;
        let alloc_bytes = size.max(4).next_multiple_of(ALIGN);
        let mut pool = self.indirect_pool.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(buf) = pool.buffer.clone() {
            if pool.used.saturating_add(alloc_bytes) <= pool.cap {
                let offset = pool.used;
                pool.used += alloc_bytes;
                return Some((buf, offset, pool.in_indirect, false));
            }
            if alloc_bytes <= pool.cap {
                pool.used = alloc_bytes;
                return Some((buf, 0, pool.in_indirect, true));
            }
            drop(buf);
            let new_cap = pool
                .cap
                .saturating_mul(2)
                .max(alloc_bytes)
                .next_multiple_of(64 << 10);
            let buffer = host()?.uav_buffer(new_cap)?;
            name(&buffer, || format!("indirect arguments {new_cap:#x}"));
            let old_buf = pool.buffer.replace(buffer.clone());
            let old_in_indirect = pool.in_indirect;
            pool.cap = new_cap;
            pool.used = alloc_bytes;
            pool.in_indirect = false;
            drop(pool);
            if let Some(old_buf) = old_buf {
                if old_in_indirect {
                    crate::indirect::done_on(&self.list, &old_buf);
                }
                self.keep(&old_buf);
            }
            return Some((buffer, 0, false, true));
        }
        let cap = alloc_bytes.max(64 << 10).next_multiple_of(64 << 10);
        let buffer = host()?.uav_buffer(cap)?;
        name(&buffer, || format!("indirect arguments {cap:#x}"));
        pool.buffer = Some(buffer.clone());
        pool.cap = cap;
        pool.used = alloc_bytes;
        pool.in_indirect = false;
        Some((buffer, 0, false, false))
    }

    pub(crate) fn mark_indirect_state(&self, in_indirect: bool) {
        let mut pool = self.indirect_pool.lock().unwrap_or_else(|e| e.into_inner());
        pool.in_indirect = in_indirect;
    }

    #[cfg(test)]
    pub(crate) fn mark_indirect_resource_done(&self, resource: &ID3D12Resource) {
        let raw = resource.as_raw();
        let mut pool = self.indirect_pool.lock().unwrap_or_else(|e| e.into_inner());
        if pool.buffer.as_ref().is_some_and(|b| b.as_raw() == raw) {
            pool.in_indirect = false;
        }
    }

    fn finish_indirect(&self) {
        let buf = {
            let mut pool = self.indirect_pool.lock().unwrap_or_else(|e| e.into_inner());
            if pool.in_indirect {
                pool.in_indirect = false;
                pool.buffer.clone()
            } else {
                None
            }
        };
        if let Some(buf) = buf {
            crate::indirect::done_on(&self.list, &buf);
        }
        if self.deferred().budget_zeroed
            && let Some(Some(budget)) = self.budget.get()
        {
            // SAFETY: an open command list and our live budget buffer.
            unsafe {
                self.list.ResourceBarrier(&[transition(
                    budget,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COMMON,
                )]);
            }
        }
    }

    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        self.copy_seq.fetch_add(1, Ordering::Relaxed);
        self.deferred().copies.push(Step::Run(Box::new(work)));
        self.deferred().refreshed.clear();
    }

    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    pub(crate) fn defer_texture(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.deferred().copies.push(Step::Texture(staging));
    }

    pub(crate) fn defer_after_textures(&self, work: impl Fn() + Send + Sync + 'static) {
        self.deferred()
            .copies
            .push(Step::AfterTextures(std::sync::Arc::new(work)));
    }

    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        let mut total_copied = 0u64;
        let mut copy_us = 0u64;
        #[cfg(feature = "diagnostics")]
        let mut copy_count = 0u64;
        #[cfg(feature = "diagnostics")]
        let mut refresh_count = 0u64;
        #[cfg(feature = "diagnostics")]
        let mut refresh_bytes = 0u64;
        let mut remapped_spans: Vec<(usize, usize)> = Vec::new();
        let mut copy_run_start: Option<std::time::Instant> = None;
        for step in &self.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    copy_run_start.get_or_insert_with(std::time::Instant::now);
                    #[cfg(feature = "diagnostics")]
                    {
                        copy_count += 1;
                    }
                    batch.title_written_range(dst, len);
                    copy_mapped_deferred(dst as *mut u8, src as *const u8, len);
                    crate::watch::wrote_after_writing(dst, len);
                    if len > 0 {
                        let end = dst.saturating_add(len);
                        match remapped_spans.last_mut() {
                            Some(last) if last.1 == dst => last.1 = end,
                            _ => remapped_spans.push((dst, end)),
                        }
                    }
                    total_copied += len as u64;
                }
                &Step::Fill(dst, src, len) => {
                    copy_run_start.get_or_insert_with(std::time::Instant::now);
                    #[cfg(feature = "diagnostics")]
                    {
                        copy_count += 1;
                    }
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    total_copied += len as u64;
                }
                Step::Refresh(shadow, address, len) => {
                    if let Some(t) = copy_run_start.take() {
                        copy_us += t.elapsed().as_micros() as u64;
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        refresh_count += 1;
                        refresh_bytes += *len as u64;
                    }
                    batch.add(shadow, *address, *len);
                }
                Step::Run(work) | Step::Host(work) => {
                    if let Some(t) = copy_run_start.take() {
                        copy_us += t.elapsed().as_micros() as u64;
                    }
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    work();
                    if matches!(step, Step::Run(_)) {
                        crate::watch::ask_once_end();
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                }
                Step::GpuWrite(shadow, address, len) => {
                    if let Some(t) = copy_run_start.take() {
                        copy_us += t.elapsed().as_micros() as u64;
                    }
                    batch.flush_one_range(shadow, *address, address.saturating_add(*len));
                    #[cfg(feature = "diagnostics")]
                    let snapshot = std::time::Instant::now();
                    shadow.writing(*address, *len);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::GpuWrites, 1);
                        add(C::SnapshotUs, snapshot.elapsed().as_micros() as u64);
                    }
                }
                Step::Texture(staging) => {
                    if let Some(t) = copy_run_start.take() {
                        copy_us += t.elapsed().as_micros() as u64;
                    }
                    let (start, len) = staging.memory();
                    if batch.texture_due_range(staging, start, len) {
                        batch.push_pending_texture(staging.clone(), start, len);
                    }
                }
                Step::AfterTextures(work) => {
                    if let Some(t) = copy_run_start.take() {
                        copy_us += t.elapsed().as_micros() as u64;
                    }
                    batch.after_textures.push(work.clone());
                }
            }
        }
        if let Some(t) = copy_run_start.take() {
            copy_us += t.elapsed().as_micros() as u64;
        }
        if !remapped_spans.is_empty() {
            crate::descriptor::host::remapped_batch(&remapped_spans);
        }
        if copy_us > 0 {
            PREPARING[0].fetch_add(copy_us, Ordering::Relaxed);
        }
        if total_copied > 0 {
            PREPARING[1].fetch_add(total_copied, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        {
            if copy_count > 0 {
                add(C::Copies, copy_count);
                add(C::CopyBytes, total_copied);
            }
            if refresh_count > 0 {
                add(C::Refreshes, refresh_count);
                add(C::RefreshBytes, refresh_bytes);
            }
        }
    }
}

fn wide_refresh() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WIDE_REFRESH").is_none_or(|v| v != "0"))
}

#[derive(Default)]
struct Batch {
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)>,
    index: std::collections::HashMap<usize, usize>,
    refresh_min: usize,
    refresh_max: usize,
    textures: std::collections::HashMap<usize, (usize, usize)>,
    textures_min: usize,
    textures_max: usize,
    pending_textures: Vec<(usize, usize, std::sync::Arc<crate::texture::Staging>)>,
    pending_tex_min: usize,
    pending_tex_max: usize,
    after_textures: Vec<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

fn split_overlapping_spans(
    spans: &mut Vec<(usize, usize)>,
    lo: usize,
    hi: usize,
) -> Vec<(usize, usize)> {
    if lo >= hi || !spans.iter().any(|&(a, b)| a < hi && b > lo) {
        return Vec::new();
    }
    let mut remaining = Vec::with_capacity(spans.len() + 1);
    let mut overlapping = Vec::new();
    for &(a, b) in spans.iter() {
        if b <= lo || a >= hi {
            remaining.push((a, b));
        } else {
            if a < lo {
                remaining.push((a, lo));
            }
            overlapping.push((a.max(lo), b.min(hi)));
            if b > hi {
                remaining.push((hi, b));
            }
        }
    }
    *spans = remaining;
    overlapping
}

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        if self.refreshes.is_empty() {
            self.refresh_min = address;
            self.refresh_max = end;
        } else {
            self.refresh_min = self.refresh_min.min(address);
            self.refresh_max = self.refresh_max.max(end);
        }
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        match self.index.get(&key) {
            Some(&at) => {
                crate::shadow::merge_refresh_ranges(&mut self.refreshes[at].1, address, end);
            }
            None => {
                self.index.insert(key, self.refreshes.len());
                self.refreshes.push((shadow.clone(), vec![(address, end)]));
            }
        }
    }

    fn flush_one_range(
        &mut self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        from: usize,
        to: usize,
    ) {
        if from >= to {
            return;
        }
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        let Some(&at) = self.index.get(&key) else {
            return;
        };
        let overlapping = split_overlapping_spans(&mut self.refreshes[at].1, from, to);
        if overlapping.is_empty() {
            return;
        }
        let shadow = if self.refreshes[at].1.is_empty() {
            self.index.remove(&key);
            let (shadow, _) = self.refreshes.swap_remove(at);
            if let Some(moved) = self.refreshes.get(at) {
                self.index
                    .insert(std::sync::Arc::as_ptr(&moved.0) as usize, at);
            }
            if self.refreshes.is_empty() {
                self.refresh_min = 0;
                self.refresh_max = 0;
            }
            shadow
        } else {
            self.refreshes[at].0.clone()
        };
        for (ov_from, ov_to) in overlapping {
            Self::update(&shadow, ov_from, ov_to);
        }
    }

    fn update(shadow: &crate::shadow::Shadow, from: usize, to: usize) {
        let started = std::time::Instant::now();
        let copied = shadow.update(from, to - from);
        let us = started.elapsed().as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::range(from, to - from, us, 0);
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.texture_due_range(staging, 0, usize::MAX)
    }

    fn texture_due_range<T>(&mut self, staging: &std::sync::Arc<T>, start: usize, len: usize) -> bool {
        let end = start.saturating_add(len);
        if self.textures.is_empty() {
            self.textures_min = start;
            self.textures_max = end;
        } else {
            self.textures_min = self.textures_min.min(start);
            self.textures_max = self.textures_max.max(end);
        }
        let key = std::sync::Arc::as_ptr(staging) as *const () as usize;
        self.textures.insert(key, (start, end)).is_none()
    }

    fn push_pending_texture(
        &mut self,
        staging: std::sync::Arc<crate::texture::Staging>,
        start: usize,
        len: usize,
    ) {
        let end = start.saturating_add(len);
        if self.pending_textures.is_empty() {
            self.pending_tex_min = start;
            self.pending_tex_max = end;
        } else {
            self.pending_tex_min = self.pending_tex_min.min(start);
            self.pending_tex_max = self.pending_tex_max.max(end);
        }
        self.pending_textures.push((start, end, staging));
    }

    fn title_written(&mut self) {
        self.flush();
        self.textures.clear();
        self.textures_min = 0;
        self.textures_max = 0;
    }

    fn title_written_range(&mut self, dst: usize, len: usize) {
        if len == 0 {
            return;
        }
        let end = dst.saturating_add(len);
        if !self.refreshes.is_empty() && dst < self.refresh_max && end > self.refresh_min {
            let mut overlapping_shadows = Vec::new();
            let mut i = 0;
            while i < self.refreshes.len() {
                let spans = &self.refreshes[i].1;
                if let (Some(&(lo, _)), Some(&(_, hi))) = (spans.first(), spans.last())
                    && (lo >= end || hi <= dst)
                {
                    i += 1;
                    continue;
                }
                let overlapping = split_overlapping_spans(&mut self.refreshes[i].1, dst, end);
                if !overlapping.is_empty() {
                    if self.refreshes[i].1.is_empty() {
                        let (shadow, _) = self.refreshes.swap_remove(i);
                        self.index
                            .remove(&(std::sync::Arc::as_ptr(&shadow) as usize));
                        if let Some(moved) = self.refreshes.get(i) {
                            self.index
                                .insert(std::sync::Arc::as_ptr(&moved.0) as usize, i);
                        }
                        overlapping_shadows.push((shadow, overlapping));
                    } else {
                        overlapping_shadows.push((self.refreshes[i].0.clone(), overlapping));
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            if self.refreshes.is_empty() {
                self.refresh_min = 0;
                self.refresh_max = 0;
            }
            for (shadow, spans) in overlapping_shadows {
                for (from, to) in spans {
                    Self::update(&shadow, from, to);
                }
            }
        }
        if !self.pending_textures.is_empty()
            && dst < self.pending_tex_max
            && end > self.pending_tex_min
        {
            let mut overlapping_textures = Vec::new();
            let mut i = 0;
            while i < self.pending_textures.len() {
                let (t_start, t_end, _) = self.pending_textures[i];
                if t_start < end && t_end > dst {
                    overlapping_textures.push(self.pending_textures.swap_remove(i).2);
                } else {
                    i += 1;
                }
            }
            if self.pending_textures.is_empty() {
                self.pending_tex_min = 0;
                self.pending_tex_max = 0;
            }
            for staging in overlapping_textures {
                staging.refresh();
            }
        }
        if !self.textures.is_empty() && dst < self.textures_max && end > self.textures_min {
            self.textures
                .retain(|_, &mut (t_start, t_end)| t_end <= dst || t_start >= end);
            if self.textures.is_empty() {
                self.textures_min = 0;
                self.textures_max = 0;
            }
        }
    }

    fn flush(&mut self) {
        self.index.clear();
        self.refresh_min = 0;
        self.refresh_max = 0;
        self.pending_tex_min = 0;
        self.pending_tex_max = 0;
        let mut ranges: Vec<(usize, usize)> = self
            .refreshes
            .iter()
            .filter(|r| !r.0.unwatched() && !r.0.guarded())
            .flat_map(|r| r.1.iter().copied())
            .collect();
        for (_, _, staging) in &self.pending_textures {
            if let Some(range) = staging.watch_range() {
                ranges.push(range);
            }
        }
        let ahead = ranges.len() > 1;
        if ahead {
            let started = std::time::Instant::now();
            crate::watch::ask_ahead(&ranges);
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        if wide_refresh() && self.refreshes.len() >= 2 {
            let started = std::time::Instant::now();
            let before = PREPARING[0].load(Ordering::Relaxed);
            let mut all: Vec<_> = self
                .refreshes
                .drain(..)
                .map(|(shadow, spans)| {
                    let bytes: usize = spans.iter().map(|&(a, b)| b.saturating_sub(a)).sum();
                    (bytes, shadow, spans)
                })
                .collect();
            all.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| b.2.len().cmp(&a.2.len())));
            let chunks = all.len().min(24);
            let mut buckets: Vec<(usize, Vec<(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)>)> =
                (0..chunks).map(|_| (0, Vec::new())).collect();
            for (bytes, shadow, spans) in all {
                let target = buckets
                    .iter()
                    .enumerate()
                    .min_by_key(|&(_, &(w, ref v))| (w, v.len()))
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                buckets[target].0 = buckets[target].0.saturating_add(bytes);
                buckets[target].1.push((shadow, spans));
            }
            buckets.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let jobs: Vec<Box<dyn FnOnce() + Send>> = buckets
                .into_iter()
                .filter(|(_, part)| !part.is_empty())
                .map(|(_, part)| {
                    Box::new(move || {
                        for (shadow, spans) in part {
                            for (from, to) in spans {
                                Self::update(&shadow, from, to);
                            }
                        }
                    }) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::wide_run(jobs);
            let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
            PREPARING[0].fetch_sub(
                summed.saturating_sub(started.elapsed().as_micros() as u64),
                Ordering::Relaxed,
            );
        } else {
            for (shadow, spans) in self.refreshes.drain(..) {
                for (from, to) in spans {
                    Self::update(&shadow, from, to);
                }
            }
        }
        if !self.pending_textures.is_empty() {
            if self.pending_textures.len() >= 4 {
                let started = std::time::Instant::now();
                let before = PREPARING[2].load(Ordering::Relaxed);
                let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                    .pending_textures
                    .drain(..)
                    .map(|(_, _, staging)| {
                        Box::new(move || staging.refresh()) as Box<dyn FnOnce() + Send>
                    })
                    .collect();
                crate::shadow::wide_run(jobs);
                let summed = PREPARING[2].load(Ordering::Relaxed).saturating_sub(before);
                PREPARING[2].fetch_sub(
                    summed.saturating_sub(started.elapsed().as_micros() as u64),
                    Ordering::Relaxed,
                );
            } else {
                for (_, _, staging) in self.pending_textures.drain(..) {
                    staging.refresh();
                }
            }
        }
        for work in self.after_textures.drain(..) {
            work();
        }
        if ahead {
            crate::watch::ask_ahead_done();
        }
    }
}

/// Fast path for `Step::Copy` in `run_deferred`: records write range and opens page guards
/// without locking `COMMIT_TITLE_MEMORY` or `descriptor::host::REMAPPED` up front; commits
/// title memory lazily only if `read_partial` faults on an uncommitted page.
fn copy_mapped_deferred(dst: *mut u8, src: *const u8, len: usize) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    static ASK: OnceLock<bool> = OnceLock::new();
    let ask_first =
        *ASK.get_or_init(|| std::env::var_os("OATHKEEP_COPY_ASK").is_some_and(|v| v == "1"));
    if ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    crate::watch::writing_copy(dst as usize, len);
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let mut done = read_partial(src, dst, len);
    if done < len {
        // Lazily demand-commit the remaining destination range and retry once.
        crate::watch::commit_title_memory((dst as usize).saturating_add(done), len - done);
        done += read_partial(src.wrapping_add(done), dst.wrapping_add(done), len - done);
    }
    #[cfg(feature = "diagnostics")]
    {
        let us = moved.elapsed().as_micros() as u64;
        crate::copystat::add(crate::copystat::C::MoveUs, us);
        crate::copystat::add(crate::copystat::C::MoveDeferredUs, us);
    }
    if done == len {
        return;
    }
    if !ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    #[cfg(feature = "diagnostics")]
    let sparse = std::time::Instant::now();
    let (src, dst, len) = (src.wrapping_add(done), dst.wrapping_add(done), len - done);
    read_pieces(src, dst, len);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::SparseUs,
        sparse.elapsed().as_micros() as u64,
    );
}

pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    static ASK: OnceLock<bool> = OnceLock::new();
    let ask_first = title_dst
        && *ASK.get_or_init(|| std::env::var_os("OATHKEEP_COPY_ASK").is_some_and(|v| v == "1"));
    if ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    if title_dst {
        crate::watch::writing(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let done = read_partial(src, dst, len);
    #[cfg(feature = "diagnostics")]
    {
        let us = moved.elapsed().as_micros() as u64;
        crate::copystat::add(crate::copystat::C::MoveUs, us);
        crate::copystat::add(crate::copystat::C::MoveShadowUs, us);
    }
    if done == len {
        return;
    }
    if title_dst && !ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    #[cfg(feature = "diagnostics")]
    let sparse = std::time::Instant::now();
    let (src, dst, len) = (src.wrapping_add(done), dst.wrapping_add(done), len - done);
    if !title_dst && len >= 4 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len);
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::SparseUs,
        sparse.elapsed().as_micros() as u64,
    );
}

fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: the query inspects an address and fills a local structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
        let n = (info.BaseAddress as usize)
            .saturating_add(info.RegionSize)
            .saturating_sub(at as usize)
            .min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining);
            break;
        }
        if info.State != MEM_COMMIT
            || info.Protect == 0
            || (info.Protect & (PAGE_NOACCESS | PAGE_GUARD)) != 0
        {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: this path is only used with a writable host destination.
            unsafe { to.write_bytes(0, n) };
        } else {
            let mut sub = 0;
            while sub < n {
                let done = read_partial(at.wrapping_add(sub), to.wrapping_add(sub), n - sub);
                sub += done;
                if sub < n {
                    // Faulted mid-committed region (e.g. sparse MEM_MAPPED / IN_PAGE_ERROR):
                    // zero only the faulting 4 KiB page and continue with read_partial.
                    let cur = (at as usize).wrapping_add(sub);
                    let page_rem = (4096 - (cur & 4095)).min(n - sub);
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, page_rem as u64);
                    // SAFETY: inside the writable host destination.
                    unsafe { to.wrapping_add(sub).write_bytes(0, page_rem) };
                    sub += page_rem;
                }
            }
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

pub(crate) fn read_into_zero_unmapped(src: *const u8, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }
    let done = read_partial(src, dst.as_mut_ptr(), dst.len());
    if done == dst.len() {
        return true;
    }
    read_sparse(
        src.wrapping_add(done),
        dst.as_mut_ptr().wrapping_add(done),
        dst.len() - done,
    );
    false
}

#[cfg(all(windows, target_arch = "x86_64"))]
core::arch::global_asm!(
    ".global oathkeep_safe_copy",
    ".global oathkeep_safe_copy_loop",
    ".global oathkeep_safe_copy_fail",
    "oathkeep_safe_copy:",
    "    push rdi",
    "    push rsi",
    "    mov rdi, rcx",
    "    mov rsi, rdx",
    "    mov rcx, r8",
    "    cld",
    "oathkeep_safe_copy_loop:",
    "    rep movsb",
    "    mov rax, r8",
    "    pop rsi",
    "    pop rdi",
    "    ret",
    "oathkeep_safe_copy_fail:",
    "    mov rax, r8",
    "    sub rax, rcx",
    "    pop rsi",
    "    pop rdi",
    "    ret",
);

#[cfg(all(windows, target_arch = "x86_64"))]
unsafe extern "C" {
    fn oathkeep_safe_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    static oathkeep_safe_copy_loop: u8;
    static oathkeep_safe_copy_fail: u8;
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn install_safe_read() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    const IN_PAGE_ERROR: NTSTATUS = 0xC000_0006u32 as NTSTATUS;
    const WRITE: usize = 1;
    unsafe extern "system" fn on_safe_read(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes valid exception and context pointers.
        let (r, ctx) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            && r.ExceptionCode != IN_PAGE_ERROR
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let loop_ip = (&raw const oathkeep_safe_copy_loop) as usize as u64;
        if ctx.Rip != loop_ip {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        if r.ExceptionCode == EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            && r.NumberParameters >= 2
            && r.ExceptionInformation[0] == WRITE
            && crate::watch::try_open_write(r.ExceptionInformation[1])
        {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        ctx.Rip = (&raw const oathkeep_safe_copy_fail) as usize as u64;
        EXCEPTION_CONTINUE_EXECUTION
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: registered first (first=1) so it intercepts safe-copy faults before
    // the crash-reporting VEH (`first=0`); only matches RIP == `oathkeep_safe_copy_loop`.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_safe_read));
    });
}

#[cfg(windows)]
pub fn is_safe_copy_ip(rip: u64) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        rip == (&raw const oathkeep_safe_copy_loop) as usize as u64
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = rip;
        false
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn safe_read_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_SAFE_READ").is_none_or(|v| v != "0"))
}

fn read_partial(src: *const u8, dst: *mut u8, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if src.is_null() || dst.is_null() {
        return 0;
    }
    #[cfg(all(windows, target_arch = "x86_64"))]
    if safe_read_on() {
        let (s, d) = (src as usize, dst as usize);
        let overlap_forward = d > s && d < s.saturating_add(len);
        if !overlap_forward {
            install_safe_read();
            // SAFETY: `dst` is mapped writable memory of `len` bytes; any fault
            // on `src` or `dst` at `oathkeep_safe_copy_loop` is caught by
            // `on_safe_read` and returns the number of bytes copied before faulting.
            return unsafe { oathkeep_safe_copy(dst, src, len) };
        }
    }
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the destination is mapped writable memory of `len` bytes; the
    // kernel checks the source.
    if unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
    {
        len
    } else {
        done
    }
}

fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    read_partial(src, dst, len) == len
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod safe_read_tests {
    use super::{read, read_partial};

    #[test]
    fn safe_read_copies_valid_and_recovers_on_unmapped() {
        use windows_sys::Win32::System::Memory::*;
        let src: Vec<u8> = (0..131_072).map(|i| (i * 31 + 7) as u8).collect();
        let mut dst = vec![0u8; src.len()];
        assert!(read(src.as_ptr(), dst.as_mut_ptr(), src.len()));
        assert_eq!(src, dst);

        assert!(!read(std::ptr::null(), dst.as_mut_ptr(), 64));
        assert!(!read(0x1000 as *const u8, dst.as_mut_ptr(), 64));

        // SAFETY: own test reservation freed before return.
        unsafe {
            let base = VirtualAlloc(
                std::ptr::null(),
                0x2_0000,
                MEM_RESERVE,
                PAGE_NOACCESS,
            )
            .cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), 0x1_0000, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            base.write_bytes(0xAB, 0x1_0000);
            let mut buf = vec![0u8; 0x2_0000];
            assert!(read(base, buf.as_mut_ptr(), 0x1_0000));
            assert!(buf[..0x1_0000].iter().all(|&b| b == 0xAB));
            buf.fill(0);
            let partial = read_partial(base, buf.as_mut_ptr(), 0x2_0000);
            assert_eq!(partial, 0x1_0000);
            assert!(buf[..0x1_0000].iter().all(|&b| b == 0xAB));
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn write_mapped_writes_valid_and_skips_unmapped_holes() {
        use windows_sys::Win32::System::Memory::*;
        // SAFETY: own test reservation freed before return.
        unsafe {
            let base = VirtualAlloc(
                std::ptr::null(),
                0x3_0000,
                MEM_RESERVE,
                PAGE_NOACCESS,
            )
            .cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), 0x1_0000, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            assert_eq!(
                VirtualAlloc(
                    base.add(0x2_0000).cast(),
                    0x1_0000,
                    MEM_COMMIT,
                    PAGE_READWRITE
                ),
                base.add(0x2_0000).cast()
            );
            let src: Vec<u8> = (0..0x3_0000).map(|i| (i * 13 + 5) as u8).collect();
            super::write_mapped(base, src.as_ptr(), src.len());
            let first = std::slice::from_raw_parts(base, 0x1_0000);
            let third = std::slice::from_raw_parts(base.add(0x2_0000), 0x1_0000);
            assert_eq!(first, &src[..0x1_0000]);
            assert_eq!(third, &src[0x2_0000..]);
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtWriteVirtualMemory(
        process: *mut std::ffi::c_void,
        base: *mut std::ffi::c_void,
        buffer: *const std::ffi::c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
}

fn direct_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_DIRECT_WRITES").is_none_or(|v| v != "0"))
}

pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    if len == 0 {
        return;
    }
    let direct = direct_writes();
    #[cfg(all(windows, target_arch = "x86_64"))]
    let (dst, src, len) = if direct && safe_read_on() {
        let done = read_partial(src, dst, len);
        if done == len {
            return;
        }
        (dst.wrapping_add(done), src.wrapping_add(done), len - done)
    } else {
        (dst, src, len)
    };
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        let mut done = 0;
        if direct {
            // SAFETY: as below; the kernel checks the destination and
            // writes nothing it cannot (the caller opened its guards:
            // `crate::watch::writing`).
            return unsafe {
                NtWriteVirtualMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) >= 0
            };
        }
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

impl Host {
    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    pub(crate) fn read_back_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a read-back buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no read-back buffer");
                None
            }
        }
    }

    fn read_back_mapped(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[2].fetch_add(1, Ordering::Relaxed);
        MADE[3].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        let make = |buffer: &mut Option<ID3D12Resource>, data: &mut *mut std::ffi::c_void| {
            // SAFETY: plain creation and persistent mapping of a read-back buffer.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        buffer,
                    )
                    .and_then(|()| {
                        let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                        b.Map(0, None, Some(data))
                    })
            }
        };
        let mut made = make(&mut buffer, &mut data);
        if let Err(ref e) = made
            && e.code() == windows::Win32::Foundation::E_OUTOFMEMORY
        {
            reclaim_for_oom();
            buffer = None;
            data = std::ptr::null_mut();
            made = make(&mut buffer, &mut data);
            if made.is_ok() {
                tracing::info!(size, "read-back buffer succeeded after reclaim_for_oom");
            }
        }
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no mapped read-back buffer");
                // SAFETY: plain status query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    log_removal_context();
                }
                None
            }
        }
    }

    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[0].fetch_add(1, Ordering::Relaxed);
        MADE[1].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        let make = |buffer: &mut Option<ID3D12Resource>, data: &mut *mut std::ffi::c_void| {
            // SAFETY: plain creation and mapping of an upload buffer.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_GENERIC_READ,
                        None,
                        buffer,
                    )
                    .and_then(|()| {
                        let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                        b.Map(0, None, Some(data))
                    })
            }
        };
        let mut made = make(&mut buffer, &mut data);
        if let Err(ref e) = made
            && e.code() == windows::Win32::Foundation::E_OUTOFMEMORY
        {
            reclaim_for_oom();
            buffer = None;
            data = std::ptr::null_mut();
            made = make(&mut buffer, &mut data);
            if made.is_ok() {
                tracing::info!(size, "upload buffer succeeded after reclaim_for_oom");
            }
        }
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                // SAFETY: plain status query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    log_removal_context();
                }
                None
            }
        }
    }
}

fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;
const COMPOSITE_SLOTS: usize = 64;

const PRESENT_VS: &str = r#"
struct Out {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};
Out corner(uint id : SV_VertexID) {
    Out o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}
"#;

const PRESENT_PS: &str = r#"
Texture2D<float4> base_tex : register(t0);
Texture2D<float4> over_tex : register(t1);
SamplerState linear_clamp : register(s0);
[RootSignature("DescriptorTable(SRV(t0, numDescriptors=2), visibility=SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP, visibility=SHADER_VISIBILITY_PIXEL)")]
float4 composite(float4 pos : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    float4 base = base_tex.SampleLevel(linear_clamp, uv, 0);
    float4 over = over_tex.SampleLevel(linear_clamp, uv, 0);
    float3 rgb = over.rgb + base.rgb * (1.0 - saturate(over.a));
    return float4(rgb, 1.0);
}
"#;

struct PresentCompositor {
    root: ID3D12RootSignature,
    pipelines: Vec<(i32, ID3D12PipelineState)>,
    vs: Vec<u8>,
    ps: Vec<u8>,
    srv_heap: ID3D12DescriptorHeap,
    srv_cpu: usize,
    srv_gpu: u64,
    srv_step: u32,
    srv_next: usize,
    _rtv_heap: ID3D12DescriptorHeap,
    rtv_cpu: usize,
    rtv_step: u32,
}

impl PresentCompositor {
    fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        use windows::core::s;
        let vs = compile(PRESENT_VS, s!("corner"), s!("vs_5_1"))?;
        let ps = compile(PRESENT_PS, s!("composite"), s!("ps_5_1"))?;
        // SAFETY: root signature embedded in compiled pixel shader; plain descriptor heaps.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let srv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: COMPOSITE_SLOTS as u32,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let srv_cpu = srv_heap.GetCPUDescriptorHandleForHeapStart().ptr;
            let srv_gpu = srv_heap.GetGPUDescriptorHandleForHeapStart().ptr;
            let srv_step =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let rtv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: BUFFERS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })?;
            let rtv_cpu = rtv_heap.GetCPUDescriptorHandleForHeapStart().ptr;
            let rtv_step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
            Ok(Self {
                root,
                pipelines: Vec::new(),
                vs,
                ps,
                srv_heap,
                srv_cpu,
                srv_gpu,
                srv_step,
                srv_next: 0,
                _rtv_heap: rtv_heap,
                rtv_cpu,
                rtv_step,
            })
        }
    }

    fn pipeline(
        &mut self,
        device: &ID3D12Device,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<ID3D12PipelineState> {
        if let Some((_, pso)) = self.pipelines.iter().find(|(f, _)| *f == format.0) {
            return Ok(pso.clone());
        }
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
        rtv_formats[0] = format;
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        // SAFETY: root signature and bytecode slices live through pipeline creation.
        let pso: ID3D12PipelineState = unsafe {
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&self.root),
                VS: code(&self.vs),
                PS: code(&self.ps),
                BlendState: blend,
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: false.into(),
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                RTVFormats: rtv_formats,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            device.CreateGraphicsPipelineState(&desc)?
        };
        self.pipelines.push((format.0, pso.clone()));
        Ok(pso)
    }
}

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    ahead: Vec<(HostList, u64)>,
    compositor: Option<PresentCompositor>,
    hwnd: HWND,
    meter: Meter,
}

struct Meter {
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
        }
    }

    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "oathkeep_gpu::frame", token = crate::frame::current(), "present");
        #[cfg(feature = "diagnostics")]
        crate::copystat::presented();
        self.frames += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        let [cpu, gpu] =
            [0, 1].map(|i| SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] =
            [0, 1].map(|i| COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames);
        let dropped = crate::draw::take_dropped() as f64 / frames;
        let indirect = crate::list::take_indirect() as f64 / frames;
        let mesh_largest = MESH_GROUPS[0].swap(0, Ordering::Relaxed);
        let mesh_groups = MESH_GROUPS[1].swap(0, Ordering::Relaxed) as f64 / frames;
        tracing::info!(
            "indirect mesh dispatches: the largest group count asked for {mesh_largest} (run: at most 65535), {mesh_groups:.0} groups asked per frame"
        );
        late_line(frames);
        card_clock_line(frames, ms);
        crate::shadow::back_line(frames);
        {
            let [passes, pieces, compares] =
                [0, 1, 2].map(|i| CARD_PASSES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if passes + compares > 0.0 {
                tracing::info!(
                    "our passes over copies in the card's memory per frame: {passes:.0} hash passes over {pieces:.0} pieces after \
                     ranges were sent up (OATHKEEP_CARD_REHASH_RUNS=1 joins them: now {}), {compares:.0} compare passes",
                    if rehash_runs() { "on" } else { "off" }
                );
            }
        }
        {
            let [own, asking, still, all] = [0, 1, 2, 3].map(|i| {
                crate::texture::REFRESH_WAITS[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
            });
            if all >= 1.0 {
                tracing::info!(
                    "texture refreshes per frame, all threads: {all:.1} ms, of them {own:.1} ms waiting for a texture another thread \
                     is refreshing, {asking:.1} ms asking the write watch and the guards, {still:.1} ms waiting for title memory to \
                     hold still (the runtime remapping it); the rest is reading, hashing and detiling"
                );
            }
            let [through, left] = [0, 1]
                .map(|i| crate::texture::UNLOCKED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if through + left > 0.0 {
                tracing::info!(
                    "texture refreshes that did not wait for title memory to hold still, per frame (OATHKEEP_TEXTURE_NO_WAIT=1): \
                     {through:.1} read through the kernel, {left:.1} left for the next refresh (a page was not there)"
                );
            }
        }
        {
            let [copies, joined] =
                [0, 1].map(|i| COPIES_JOINED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if copies > 0.0 {
                tracing::info!(
                    "title buffer copies recorded per frame: {copies:.0}, of them {joined:.0} continue the one before \
                     (OATHKEEP_JOIN_COPIES=1 makes those one copy with it: now {})",
                    if join_copies() { "on" } else { "off" }
                );
            }
        }
        let picked = crate::draw::HEAP_BUFFER_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
        let picked_ms =
            crate::draw::HEAP_BUFFER_MICROS.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
        let [twin_to_depth, twin_to_colour] =
            [2, 3].map(|i| crate::resource::TWINS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        let planes = crate::resource::PLANES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        let card = crate::shadow::card_stats();
        #[cfg(feature = "diagnostics")]
        let slow = crate::watch::slow_asks();
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            tracing::info!(
                "placed texture views: {} writes of another view of the texture a slot showed (it kept the old view until the new was made)",
                crate::descriptor::host::KEPT_VIEWS.load(Ordering::Relaxed)
            );
            crate::tiles::status();
            {
                let [queued, due, away, partly, oldest] = [0, 1, 2, 3, 4]
                    .map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                let [same, parts, tails] =
                    [0, 1, 2].map(|i| crate::draw::HEAP_TEXTURES_PARTLY[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, the queue: {queued} slots, {due} of them due (the oldest written {oldest} frames ago), \
                     {away} waiting for their memory, {partly} uploaded in part; since the start: {} slots found uploaded already by \
                     the draws that read them through tables, {} writes of tables' slots (due some frames later), {} slots taken out \
                     as read through a table, {parts} uploads of textures not all mapped ({tails} of them without their first page), \
                     {same} looks that found no more mapped; {} slots made stale by a newer view of other memory there, \
                     {} ranges of memory mapped, unmapped or written under texture views ({} dropped), {} slots made due by them, \
                     {} made slots found unmapped and made null",
                    crate::draw::HEAP_TEXTURES_TABLED.load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[0].load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[0].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[2].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[3].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[4].load(Ordering::Relaxed)
                );
            }
            {
                let mode = crate::watch::page_writes();
                let [writes, bytes, guarded, plain] =
                    [0, 1, 2, 3].map(|i| crate::watch::WRITTEN[i].load(Ordering::Relaxed));
                let [kept, dropped, slots, uploaded] = [0, 1, 2, 3]
                    .map(|i| crate::descriptor::host::PAGE_WRITES[i].load(Ordering::Relaxed));
                tracing::info!(
                    "writes behind the guards, since the start: {writes} DirectStorage writes into physical pages at addresses the                      title had mapped ({} MB); {guarded} guarded 64 KiB blocks of host copies lay under them ({}); {plain} reads to                      plain addresses finished ({}); {kept} ranges under texture views kept for the texture pass ({dropped} dropped);                      {slots} slots of placed textures held their bytes, {uploaded} of them uploaded before the write ({})                      (OATHKEEP_PAGE_WRITES: unset counts, 1 acts, guards, slots, all)",
                    bytes >> 20,
                    if mode & 1 != 0 {
                        "opened, taken again"
                    } else {
                        "left as they were"
                    },
                    if mode & 4 != 0 {
                        "told as well"
                    } else {
                        "counted only"
                    },
                    if mode & 2 != 0 {
                        "made due again"
                    } else {
                        "left as they were"
                    }
                );
            }
            #[cfg(feature = "diagnostics")]
            tracing::info!(
                "write-watch askings of a millisecond or more: {:.1} a frame, {:.1} ms a frame; the longest asking {:.1} ms",
                slow[0] as f64 / frames,
                slow[1] as f64 / 1e3 / frames,
                slow[2] as f64 / 1e3
            );
            if crate::shadow::cards() {
                let per_frame = |i: usize| card[i] as f64 / frames;
                tracing::info!(
                    "host copies in the card's memory: {} copies {} MB, their bytes from before {} MB; per frame: \
                     {:.1} MB sent up in {:.0} ranges, {:.0} pieces compared on the card, {:.0} of them fetched \
                     ({:.1} MB) with {:.1} more waits for the card ({:.1} ms)",
                    card[7],
                    card[8] >> 20,
                    card[9] >> 20,
                    per_frame(0) / 1e6,
                    per_frame(1),
                    per_frame(2),
                    per_frame(3),
                    per_frame(4) / 1e6,
                    per_frame(5),
                    per_frame(6) / 1e3
                );
            }
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect; heap buffers {picked:.1} passes {picked_ms:.1} ms;                  texture slots written so far: {placed} placed, {objects} of objects; \
                 textures picked by index: {picked_textures} uploaded so far ({picked_mb} MB), \
                 {picked_waiting} waiting, {picked_away} passed over unmapped; depth textures with                  unordered access: {du0} made, bound as depth {du1}x, cleared as depth {du2}x,                  unordered views refused {du3}x; buffer views left null as the host would refuse them: {refused}",
                picked_textures = crate::draw::HEAP_TEXTURES.load(Ordering::Relaxed),
                picked_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Ordering::Relaxed) >> 20,
                picked_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Ordering::Relaxed),
                picked_away = crate::draw::HEAP_TEXTURES_AWAY.load(Ordering::Relaxed),
                du0 = crate::resource::DEPTH_UNORDERED[0].load(Ordering::Relaxed),
                du1 = crate::resource::DEPTH_UNORDERED[1].load(Ordering::Relaxed),
                du2 = crate::resource::DEPTH_UNORDERED[2].load(Ordering::Relaxed),
                du3 = crate::resource::DEPTH_UNORDERED[3].load(Ordering::Relaxed),
                refused = format!(
                    "{}; depth twins: {} alive, {} depth textures left without one; twin copies per frame: {twin_to_depth:.1} into depth textures, {twin_to_colour:.1} into twins; depth planes: {planes:.1} drawn from their twins per frame, {} times not given back to a twin, {} times used before the pass",
                    crate::descriptor::host::REFUSED_VIEWS.load(Ordering::Relaxed),
                    crate::resource::TWINS[0].load(Ordering::Relaxed),
                    crate::resource::TWINS[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[2].load(Ordering::Relaxed)
                ),
                making = crate::pipeline::making(),
                placed = crate::descriptor::host::PLACED_TEXTURE_SLOTS.load(Ordering::Relaxed),
                objects = crate::descriptor::host::OBJECT_TEXTURE_SLOTS.load(Ordering::Relaxed)
            );
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::log(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | Oathkeep {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("oathkeep-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::wanted(frame) || crate::capture::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead
                    .iter()
                    .position(|(_, flown)| *flown <= finished)
                    .or_else(|| {
                        let list = self.new_list().ok()?;
                        list.close();
                        p.ahead.push((list, 0));
                        Some(p.ahead.len() - 1)
                    })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            let list = &copy.list;
            list.ResourceBarrier(&[
                transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
            list.CopyResource(&back, texture);
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::wanted(frame)
                .then(|| crate::capture::read_back(&self.device, list, texture))
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() {
                None
            } else {
                crate::capture::probe(&self.device, list, texture)
            };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            list.ResourceBarrier(&[
                transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ),
            ]);
            copy.close();
            match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                }
                None => self.execute(&[copy]),
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    pub(crate) fn present_planes(
        &self,
        planes: &[(&ID3D12Resource, u32)],
        frame: u64,
    ) -> Option<bool> {
        if planes.len() < 2 {
            let &(tex, state) = planes.first()?;
            return self.present(tex, state, frame);
        }
        let (base, base_state_u32) = planes[0];
        let (over, over_state_u32) = planes[1];
        // SAFETY: live resources.
        let (base_desc, over_desc) = unsafe { (base.GetDesc(), over.GetDesc()) };
        if base_desc
            .Flags
            .contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
            || over_desc
                .Flags
                .contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    "PresentX plane denies shader resource access; falling back to base plane"
                );
            });
            return self.present(base, base_state_u32, frame);
        }
        let Some(format) = swap_format(base_desc.Format) else {
            tracing::warn!(format = base_desc.Format.0, "cannot present this format");
            return None;
        };
        let over_format = swap_format(over_desc.Format).unwrap_or(over_desc.Format);
        let (width, height) = (base_desc.Width as u32, base_desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        if p.compositor.is_none() {
            match PresentCompositor::new(&self.device) {
                Ok(c) => p.compositor = Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "no multi-plane present compositor; falling back");
                    drop(presenter);
                    return self.present(base, base_state_u32, frame);
                }
            }
        }
        let comp = p.compositor.as_mut()?;
        let pso = match comp.pipeline(&self.device, format) {
            Ok(pso) => pso,
            Err(e) => {
                tracing::warn!(error = %e, format = format.0, "no multi-plane present pipeline");
                drop(presenter);
                return self.present(base, base_state_u32, frame);
            }
        };
        let base_state = D3D12_RESOURCE_STATES(base_state_u32 as i32);
        let over_state = D3D12_RESOURCE_STATES(over_state_u32 as i32);
        let same_resource = base.as_raw() == over.as_raw();
        // SAFETY: our swap chain, command list, descriptor heaps, and live source textures.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::wanted(frame) || crate::capture::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead
                    .iter()
                    .position(|(_, flown)| *flown <= finished)
                    .or_else(|| {
                        let list = self.new_list().ok()?;
                        list.close();
                        p.ahead.push((list, 0));
                        Some(p.ahead.len() - 1)
                    })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            let list = &copy.list;

            let srv_pair = comp.srv_next;
            comp.srv_next = (comp.srv_next + 2) % COMPOSITE_SLOTS;
            let srv_cpu0 = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: comp.srv_cpu + srv_pair * comp.srv_step as usize,
            };
            let srv_cpu1 = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: comp.srv_cpu + (srv_pair + 1) * comp.srv_step as usize,
            };
            let srv_gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: comp.srv_gpu + (srv_pair as u64) * u64::from(comp.srv_step),
            };
            let srv_desc = |fmt: DXGI_FORMAT| D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: fmt,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            self.device
                .CreateShaderResourceView(base, Some(&srv_desc(format)), srv_cpu0);
            self.device
                .CreateShaderResourceView(over, Some(&srv_desc(over_format)), srv_cpu1);

            let rtv = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: comp.rtv_cpu + (index * comp.rtv_step) as usize,
            };
            self.device.CreateRenderTargetView(&back, None, rtv);

            let mut pre = Vec::with_capacity(3);
            if base_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                pre.push(transition(
                    base,
                    base_state,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                ));
            }
            if !same_resource && over_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                pre.push(transition(
                    over,
                    over_state,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                ));
            }
            pre.push(transition(
                &back,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            ));
            list.ResourceBarrier(&pre);

            list.SetDescriptorHeaps(&[Some(comp.srv_heap.clone())]);
            list.SetGraphicsRootSignature(&comp.root);
            list.SetPipelineState(&pso);
            list.SetGraphicsRootDescriptorTable(0, srv_gpu);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            }]);
            list.IASetPrimitiveTopology(
                windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
            );
            list.DrawInstanced(3, 1, 0, 0);

            #[cfg(feature = "diagnostics")]
            let (need_capture, need_probe) = (crate::capture::wanted(frame), slot.is_none());
            #[cfg(not(feature = "diagnostics"))]
            let (need_capture, need_probe) = (false, false);

            #[cfg(feature = "diagnostics")]
            let mut capture = None;
            #[cfg(feature = "diagnostics")]
            let mut probe = None;
            if need_capture || need_probe {
                list.ResourceBarrier(&[transition(
                    &back,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                #[cfg(feature = "diagnostics")]
                {
                    if need_capture {
                        capture = crate::capture::read_back(&self.device, list, &back);
                    }
                    if need_probe {
                        probe = crate::capture::probe(&self.device, list, &back);
                    }
                }
            }

            let mut post = Vec::with_capacity(3);
            post.push(transition(
                &back,
                if need_capture || need_probe {
                    D3D12_RESOURCE_STATE_COPY_SOURCE
                } else {
                    D3D12_RESOURCE_STATE_RENDER_TARGET
                },
                D3D12_RESOURCE_STATE_PRESENT,
            ));
            if base_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                post.push(transition(
                    base,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    base_state,
                ));
            }
            if !same_resource && over_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                post.push(transition(
                    over,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    over_state,
                ));
            }
            list.ResourceBarrier(&post);
            copy.close();
            match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                }
                None => self.execute(&[copy]),
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        self.log_video_memory("right before the swap chain is made");
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                .cast::<IDXGISwapChain3>()?;
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            self.log_video_memory("right after the swap chain is made");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                ahead: Vec::new(),
                compositor: None,
                hwnd,
                meter: Meter::new(),
            })
        }
    }
}

fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    pub(crate) fn video_memory(&self) -> Option<[u64; 4]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        static LAST: [std::sync::atomic::AtomicU64; 3] =
            [const { std::sync::atomic::AtomicU64::new(0) }; 3];
        let now = [own.Budget, own.CurrentUsage, shared.CurrentUsage];
        let last = [0, 1, 2].map(|i| LAST[i].swap(now[i], Ordering::Relaxed));
        if last[0] != 0 && last[0].abs_diff(own.Budget) >= (256u64 << 20) {
            tracing::info!(
                "the system changed the budget of the card's memory from {} MB to {} MB (frame {}); at the last look {} MB of the card's and {} MB of the PC's shared memory were in use",
                last[0] >> 20,
                own.Budget >> 20,
                crate::frame::current(),
                last[1] >> 20,
                last[2] >> 20
            );
            self.log_video_memory("right after that change");
        }
        Some([
            own.Budget,
            own.CurrentUsage,
            shared.Budget,
            shared.CurrentUsage,
        ])
    }

    pub(crate) fn video_memory_groups(&self) -> Option<[[u64; 4]; 2]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        let all = |m: &DXGI_QUERY_VIDEO_MEMORY_INFO| {
            [
                m.Budget,
                m.CurrentUsage,
                m.AvailableForReservation,
                m.CurrentReservation,
            ]
        };
        Some([all(&own), all(&shared)])
    }

    pub(crate) fn log_video_memory(&self, when: &str) {
        let Some([own, shared]) = self.video_memory_groups() else {
            return;
        };
        tracing::info!(
            "video memory as the system counts it, {}: the card's own: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved; the PC's shared with the card: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved (the layer reserves none)",
            when,
            own[0] >> 20,
            own[1] >> 20,
            own[2] >> 20,
            own[3] >> 20,
            shared[0] >> 20,
            shared[1] >> 20,
            shared[2] >> 20,
            shared[3] >> 20
        );
    }

    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[2].fetch_add(1, Ordering::Relaxed);
        MADE[3].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        let make = |buffer: &mut Option<ID3D12Resource>, data: &mut *mut std::ffi::c_void| {
            // SAFETY: plain creation and mapping of a CPU-visible buffer.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        buffer,
                    )
                    .and_then(|()| {
                        let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                        b.Map(0, None, Some(data))
                    })
            }
        };
        let mut made = make(&mut buffer, &mut data);
        if let Err(ref e) = made
            && e.code() == windows::Win32::Foundation::E_OUTOFMEMORY
        {
            reclaim_for_oom();
            buffer = None;
            data = std::ptr::null_mut();
            made = make(&mut buffer, &mut data);
            if made.is_ok() {
                tracing::info!(size, "shared buffer succeeded after reclaim_for_oom");
            }
        }
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                // SAFETY: plain status query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    log_removal_context();
                }
                None
            }
        }
    }

    pub(crate) fn uav_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        MADE[4].fetch_add(1, Ordering::Relaxed);
        MADE[5].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let make = |buffer: &mut Option<ID3D12Resource>| {
            // SAFETY: plain creation of a device-local buffer.
            unsafe {
                self.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    buffer,
                )
            }
        };
        let mut made = make(&mut buffer);
        if let Err(ref e) = made
            && e.code() == windows::Win32::Foundation::E_OUTOFMEMORY
        {
            reclaim_for_oom();
            buffer = None;
            made = make(&mut buffer);
            if made.is_ok() {
                tracing::info!(size, "unordered-access buffer succeeded after reclaim_for_oom");
            }
        }
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no unordered-access buffer");
                // SAFETY: plain status query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    log_removal_context();
                }
                None
            }
        }
    }
}

fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!(
                    "; list {i} of {}, of its {}: {n}",
                    runs.len(),
                    notes.len()
                ));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

fn in_flight(
    history: &[D3D12_AUTO_BREADCRUMB_OP],
    markers: &[(u32, String)],
    done: usize,
) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done]
        .iter()
        .rposition(barrier)
        .map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    for i in from.max(done.saturating_sub(4))..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("oathkeep-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("Oathkeep");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Oathkeep"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod trail {
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::TRAIL_MOST + 5 {
            super::trail(|| format!("thing {n} done"));
        }
        let text = super::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
        assert!(
            !text.contains("thing 4 done"),
            "the oldest entries are dropped"
        );
    }
}

#[cfg(test)]
mod spare {
    use super::Spare;

    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

#[cfg(test)]
mod batch {
    use super::Batch;
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    #[test]
    fn title_written_range_only_invalidates_overlapping_textures() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new(1u32), Arc::new(2u32));
        assert!(batch.texture_due_range(&a, 0x1000, 0x400));
        assert!(batch.texture_due_range(&b, 0x4000, 0x400));
        // Non-overlapping write leaves both textures deduplicated.
        batch.title_written_range(0x2000, 0x400);
        assert!(!batch.texture_due_range(&a, 0x1000, 0x400));
        assert!(!batch.texture_due_range(&b, 0x4000, 0x400));
        // Write overlapping only `a` makes `a` due again while keeping `b`.
        batch.title_written_range(0x1200, 0x100);
        assert!(batch.texture_due_range(&a, 0x1000, 0x400));
        assert!(!batch.texture_due_range(&b, 0x4000, 0x400));
    }

    #[test]
    fn split_overlapping_spans_extracts_only_the_overlapping_slice() {
        let mut spans = vec![(0x1000, 0x5000), (0x8000, 0xa000)];
        let ov = super::split_overlapping_spans(&mut spans, 0x2000, 0x3000);
        assert_eq!(ov, vec![(0x2000, 0x3000)]);
        assert_eq!(spans, vec![(0x1000, 0x2000), (0x3000, 0x5000), (0x8000, 0xa000)]);

        let ov2 = super::split_overlapping_spans(&mut spans, 0x6000, 0x7000);
        assert!(ov2.is_empty());
        assert_eq!(spans, vec![(0x1000, 0x2000), (0x3000, 0x5000), (0x8000, 0xa000)]);

        let ov3 = super::split_overlapping_spans(&mut spans, 0x4000, 0x9000);
        assert_eq!(ov3, vec![(0x4000, 0x5000), (0x8000, 0x9000)]);
        assert_eq!(spans, vec![(0x1000, 0x2000), (0x3000, 0x4000), (0x9000, 0xa000)]);
    }

    #[test]
    fn read_into_zero_unmapped_reads_committed_prefix_and_zeroes_uncommitted_tail() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const PAGE_4K: usize = 4096;
        // SAFETY: reserve 2 4 KiB pages, commit only the first page, write known pattern, then free.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * PAGE_4K,
                MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        assert!(!base.is_null());
        let committed = unsafe { VirtualAlloc(base, PAGE_4K, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(committed, base);
        unsafe {
            std::ptr::write_bytes(base.cast::<u8>(), 0xab, PAGE_4K);
        }
        let mut out = vec![0x55u8; 2 * PAGE_4K];
        let fully_mapped = super::read_into_zero_unmapped(base.cast::<u8>(), &mut out);
        assert!(!fully_mapped);
        assert!(out[..PAGE_4K].iter().all(|&b| b == 0xab));
        assert!(out[PAGE_4K..].iter().all(|&b| b == 0));

        let mut head = vec![0u8; PAGE_4K];
        assert!(super::read_into_zero_unmapped(base.cast::<u8>(), &mut head));
        assert!(head.iter().all(|&b| b == 0xab));
        // SAFETY: freeing our own test reservation.
        unsafe {
            VirtualFree(base, 0, MEM_RELEASE);
        }
    }

    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false);
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn vram_idle_ms_scales_with_pressure() {
        use super::vram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        assert_eq!(vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
        assert_eq!(vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])), 1_000);
        assert_eq!(vram_idle_ms(None), 5_000);
        assert_eq!(vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        use super::ram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(ram_idle_ms(Some(0)), 2_000);
        assert_eq!(ram_idle_ms(None), 10_000);
    }

    #[test]
    fn commit_idle_thresholds_scale_with_pressure() {
        use super::{commit_idle_frames, commit_idle_ms};
        let mib = 1024 * 1024;
        assert_eq!(commit_idle_ms(None), 5_000);
        assert_eq!(commit_idle_ms(Some(5000 * mib)), 5_000);
        assert_eq!(commit_idle_ms(Some(4000 * mib)), 3_000);
        assert_eq!(commit_idle_ms(Some(3000 * mib)), 1_500);
        assert_eq!(commit_idle_ms(Some(2000 * mib)), 500);
        assert_eq!(commit_idle_ms(Some(1000 * mib)), 100);
        assert_eq!(commit_idle_frames(None), 4);
        assert_eq!(commit_idle_frames(Some(3000 * mib)), 4);
        assert_eq!(commit_idle_frames(Some(2000 * mib)), 2);
        assert_eq!(commit_idle_frames(Some(1000 * mib)), 1);
    }
}
