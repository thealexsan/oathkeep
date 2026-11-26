use super::*;

pub(crate) fn available_physical_ram() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullAvailPhys)
}

/// How long host copies of textures the title fills must have been idle to
/// be let go, in milliseconds, and how many bytes of them go at most (the
/// longest idle first): none but those of ten minutes while what is in use
/// of the card (`used`: without what our own blocks have free, with what is
/// kept free for others) is under 88 % of its budget, else enough to come
/// down to 85 %. `OATHKEEP_TEXTURE_KEEP=0`: [`vram_idle_ms`]' older rule,
/// all that are idle; `=1`: that rule with its two longer times.
///
/// (By those times everything idle for 2.5 s went from 75 % of the budget
/// on: 61,000 of them in the first 300 s of session play-1125-0203, at 78 to
/// 80 %, of which 13,000 were made and uploaded again.)
pub(crate) fn vram_keep(budget: u64, used: u64) -> (u64, u64) {
    const KEPT_MS: u64 = 600_000;
    const IDLE_MS: u64 = 5_000;
    const SHORT_MS: u64 = 1_500;
    if budget == 0 {
        return (IDLE_MS, u64::MAX);
    }
    let share = |percent: u64| budget / 100 * percent;
    if used < share(88) {
        (KEPT_MS, u64::MAX)
    } else if used < share(95) {
        (IDLE_MS, used - share(85))
    } else {
        (SHORT_MS, used - share(85))
    }
}

/// Effective idle timeout (in milliseconds) for VRAM-backed host textures,
/// scaled down under VRAM pressure. Never returns less than 1,000 ms so
/// handles not yet recorded into a list or slot remain protected.
///
/// With room on the card nothing has to go: in session play-1125-0127 88,000
/// of them (31 GB) went after 5 s with 7 GB of the card free, and 24,700
/// (12 GB) had to be made and uploaded again.
/// `OATHKEEP_TEXTURE_KEEP=0`: 5 s there too, as before.
pub(crate) fn vram_idle_ms(video_memory: Option<[u64; 4]>) -> u64 {
    static KEEP: OnceLock<bool> = OnceLock::new();
    let keep = *KEEP.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_KEEP").is_none_or(|v| v != "0"));
    vram_idle_for(video_memory, keep)
}

pub(super) fn vram_idle_for(video_memory: Option<[u64; 4]>, keep: bool) -> u64 {
    const KEPT_MS: u64 = 600_000;
    const LONG_MS: u64 = 30_000;
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
    } else if !keep {
        NORMAL_MS
    } else if used_pct >= 68 {
        LONG_MS
    } else {
        KEPT_MS
    }
}

/// Whether new memory is made resident, and waited for, before anything can
/// name it (on by default; `OATHKEEP_WAIT_RESIDENT=0`: as before). A heap or
/// resource of the default kind is resident once it is made, but its
/// mapping into the card's address space is work of the system's that runs
/// later: only lists sent after it wait for it. Lists sent before run on,
/// and read our descriptor heaps as they are: a slot that shows a texture
/// in a new block is a page fault for them until the block is mapped. The
/// device losses of the night of 2026-11-25 were that (run n08: the fault
/// lay in a texture made in the same tenth of a second, in a new block).
pub(super) fn wait_resident() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WAIT_RESIDENT").is_none_or(|v| v != "0"))
}

/// The flag memory is made with that [`resident`] waits for.
pub(crate) fn not_resident() -> D3D12_HEAP_FLAGS {
    if wait_resident() { D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT } else { D3D12_HEAP_FLAG_NONE }
}

/// A queue of its own that new memory is waited for with ([`mapped`]).
pub(super) struct Aside {
    queue: ID3D12CommandQueue,
    list: ID3D12CommandList,
    fence: ID3D12Fence,
    event: windows::Win32::Foundation::HANDLE,
    value: u64,
    _keep: (ID3D12CommandAllocator, [ID3D12Resource; 2]),
}
// SAFETY: D3D12 objects are free-threaded, the event is waited on by one
// thread at a time (the lock).
unsafe impl Send for Aside {}

/// New memory of the card is mapped into its address space when this
/// returns: a copy of a few bytes is sent to a queue of ours after the
/// memory was made, which the system starts only when all mappings asked
/// for so far are done, and waited for. (`MakeResident` came back before
/// that: run n11 was lost as n08 was with it alone.) Best done off a hot
/// path: a tenth of a millisecond to some.
pub(crate) fn mapped(device: &ID3D12Device) -> windows::core::Result<()> {
    static ASIDE: Mutex<Option<Aside>> = Mutex::new(None);
    if !wait_resident() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut aside = ASIDE.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: plain object creation; the objects the list names are kept.
    unsafe {
        if aside.is_none() {
            let buffer = |kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| -> windows::core::Result<ID3D12Resource> {
                let mut made: Option<ID3D12Resource> = None;
                device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 256,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    state,
                    None,
                    &mut made,
                )?;
                made.ok_or_else(windows::core::Error::empty)
            };
            let from = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let to = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                ..Default::default()
            })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.CopyBufferRegion(&to, 0, &from, 0, 16);
            list.Close()?;
            *aside = Some(Aside {
                queue,
                list: list.cast()?,
                fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
                event: CreateEventW(None, false, false, None)?,
                value: 0,
                _keep: (allocator, [from, to]),
            });
        }
        let Some(a) = aside.as_mut() else { return Ok(()) };
        a.queue.ExecuteCommandLists(&[Some(a.list.clone())]);
        a.value += 1;
        a.queue.Signal(&a.fence, a.value)?;
        if a.fence.GetCompletedValue() < a.value {
            a.fence.SetEventOnCompletion(a.value, a.event)?;
            WaitForSingleObject(a.event, INFINITE);
        }
    }
    let us = started.elapsed().as_micros() as u64;
    MAPPED[0].fetch_add(1, Ordering::Relaxed);
    MAPPED[1].fetch_add(us, Ordering::Relaxed);
    MAPPED[2].fetch_max(us, Ordering::Relaxed);
    let n = MAPPED[0].load(Ordering::Relaxed);
    if n == 1 || n.is_multiple_of(200) {
        tracing::info!(
            "new card memory waited for until it is mapped (OATHKEEP_WAIT_RESIDENT=0: not), since the start: {n} times, {:.1} ms, the longest {:.1} ms",
            MAPPED[1].load(Ordering::Relaxed) as f64 / 1000.0,
            MAPPED[2].load(Ordering::Relaxed) as f64 / 1000.0
        );
    }
    Ok(())
}

/// Times, microseconds and the longest of [`mapped`].
pub(super) static MAPPED: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// New memory (made with [`not_resident`]) is resident when this returns.
pub(crate) fn resident(device: &ID3D12Device, object: &impl Interface) -> windows::core::Result<()> {
    if !wait_resident() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let object: ID3D12Pageable = object.cast()?;
    // SAFETY: a live heap or resource of the device's.
    let made = unsafe { device.MakeResident(&[Some(object)]) };
    let us = started.elapsed().as_micros() as u64;
    RESIDENT[0].fetch_add(1, Ordering::Relaxed);
    RESIDENT[1].fetch_add(us, Ordering::Relaxed);
    RESIDENT[2].fetch_max(us, Ordering::Relaxed);
    static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = RESIDENT[0].load(Ordering::Relaxed);
    if n >= 1000 * (SAID.load(Ordering::Relaxed) + 1) {
        SAID.store(n / 1000, Ordering::Relaxed);
        tracing::info!(
            "new memory waited for (OATHKEEP_WAIT_RESIDENT=0: not), since the start: {n} times, {:.1} ms, the longest {:.1} ms",
            RESIDENT[1].load(Ordering::Relaxed) as f64 / 1000.0,
            RESIDENT[2].load(Ordering::Relaxed) as f64 / 1000.0
        );
    }
    made
}

/// Times, microseconds and the longest of [`resident`].
pub(super) static RESIDENT: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether memory the card was given stays with it while there is room (on
/// by default; `OATHKEEP_KEEP_FREED=0`: it is given back as before). A shader
/// that reads past a view (a raw load through a typed, structured or
/// constant buffer view has no end on AMD: `indirect::tests::raw_loads_past_views`)
/// or through a slot that shows wrong data reads whatever lies there, and
/// nothing happens; where memory was given back it is a page fault, and the
/// device is lost. Both losses of 2026-11-25 were reads in memory given back
/// seconds before (a block of textures; a list's scratch and a window of
/// title memory). Kept are: empty blocks of textures ([`crate::memory::pool`]) below
/// 85 % of the card's budget, windows of title memory and staging buffers
/// for five minutes with 12 GiB of the PC's memory free, and 1 GiB of
/// scratch chunks.
pub(crate) fn keep_freed() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_KEEP_FREED").is_none_or(|v| v != "0"))
}

/// Effective idle timeout (in milliseconds) for system-RAM caches (staging
/// buffers, loose shadows, and the mapped pipeline library), scaled down
/// when free physical RAM is low. Never returns less than 2,000 ms.
/// With plenty of it five minutes ([`keep_freed`]).
pub(crate) fn ram_idle_ms(avail_phys: Option<u64>) -> u64 {
    let effective = match (avail_phys, crate::memory::residency::headroom()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    ram_idle_for(effective, keep_freed() && !crate::memory::residency::limited_host_ram())
}

pub(super) fn ram_idle_for(avail_phys: Option<u64>, keep: bool) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    const KEPT_MS: u64 = 300_000;
    if keep && avail_phys.is_some_and(|avail| avail >= 12 * GIB) {
        return KEPT_MS;
    }
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

/// How many references are held on a host object (its COM count): ours
/// all, since the host's views and commands hold none. A buffer or texture
/// with one is held by its owner alone: no descriptor slot keeps it
/// (`HostHeap::kept`) and no list (`HostList::keep`).
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

/// Whether `sweep` should trim idle caches/allocations after `elapsed_ms`:
/// every 1 s when VRAM is pressured, or every 3 s when there is headroom.
pub(super) fn should_sweep_idle(pressured: bool, elapsed_ms: u64) -> bool {
    elapsed_ms >= if pressured { 1000 } else { 3000 }
}

/// Whether a behind-flights submission should force a GPU settle for `sweep`
/// after `elapsed_ms`: 1.5 s under VRAM pressure, 5 s otherwise.
pub(super) fn should_settle_for_sweep(pressured: bool, elapsed_ms: u64) -> bool {
    elapsed_ms > if pressured { 1500 } else { 5000 }
}

/// Once a second, before a submission starts: lets go of what nothing
/// names any more (copies of windows of title memory, staging buffers,
/// host copies of textures the title fills).
/// `OATHKEEP_SWEEP=0` keeps everything, as before.
pub(super) fn sweep() {
    // The in-place probe and census, if asked for (`OATHKEEP_INPLACE_PROBE`).
    crate::memory::inplace::tick();
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OATHKEEP_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static LAST_IDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 1000
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::diagnostics::memory_ledger::check();
    crate::memory::residency::refresh();
    let pressured = crate::memory::residency::pressured();
    if pressured {
        trim_spare_chunks();
    }
    crate::memory::pool::room(host().and_then(|h| h.video_memory()));
    let last_idle = LAST_IDLE.load(Ordering::Relaxed);
    if should_sweep_idle(pressured, now.saturating_sub(last_idle))
        && LAST_IDLE
            .compare_exchange(last_idle, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        spawn_idle_sweep();
    }
}

pub(super) fn run_idle_sweep() {
    crate::d3d12::pipeline::trim_library();
    crate::memory::shadow::let_go_idle();
    crate::textures::texture::let_go_idle();
    crate::d3d12::resource::let_go_idle();
}

/// Runs idle cache/resource trimming on a persistent background thread so the
/// submitting thread never stalls for tens of milliseconds on `sweep()`.
pub(super) fn spawn_idle_sweep() {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static TX: OnceLock<std::sync::mpsc::SyncSender<()>> = OnceLock::new();
    if upload_verify::on() {
        run_idle_sweep();
        return;
    }
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let _ = std::thread::Builder::new().name("oathkeep-sweep".into()).spawn(move || {
            while rx.recv().is_ok() {
                run_idle_sweep();
                BUSY.store(false, Ordering::Release);
            }
        });
        tx
    });
    if BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
        && tx.try_send(()).is_err()
    {
        BUSY.store(false, Ordering::Release);
    }
}
