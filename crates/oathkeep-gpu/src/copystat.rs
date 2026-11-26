use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) enum C {
    Submissions,
    Copies,
    CopyBytes,
    Refreshes,
    RefreshBytes,
    Skipped,
    SkippedBytes,
    Runs,
    RunUs,
    Queries,
    QueryUs,
    MoveUs,
    Unmapped,
    Watches,
    WatchUs,
    Checked,
    Unchanged,
    Fallbacks,
    GuardFaults,
    TexWatched,
    TexHashed,
    TexChanged,
    Frames,
    RecordCalls,
    RecordNs,
    PipelineWaitUs,
    DynamicAsked,
    DynamicRefused,
    DynamicNs,
    NoteNs,
    GpuWrites,
    SnapshotUs,
    BackCompared,
    SparseUs,
    GuardUs,
    WholeBytes,
    BackSubmissions,
    PaceWaits,
    PaceUs,
    UntilRecordUs,
    UntilSubmitUs,
    TailUs,
    ResetNs,
    BackScanUs,
    BackDiffer,
    BackSyncUs,
    TexUploads,
    TexSkipped,
    Forced,
    GpuWritesLeftOut,
    GpuWritesLeftOutBytes,
    WaitChecks,
    WaitCheckNs,
    HostLists,
    HostListNs,
    PartsTogether,
    WholeUs,
    CardUnder2,
    Card2To8,
    Card8To32,
    CardOver32,
    CardUnder2Us,
    Card2To8Us,
    Card8To32Us,
    CardOver32Us,
    NothingBack,
    NothingBackUs,
    WaitsForGroups,
    WaitsForGroupsSkipped,
    WaitsForBuffers,
    IndirectBounded,
    PrepAskOnceUs,
    PrepRunDeferredUs,
    PrepPreludeUs,
    PrepBatchFlushUs,
    PrepSnapshotsUs,
    PrepCardPrepareWriteUs,
    PrepCardFillUploadUs,
    PrepCardRecordUs,
    PrepOtherUs,
    GuardReadFastN,
    GuardReadFastUs,
    GuardPreAliasUs,
    GuardWriteWaitUs,
    GuardProtectUs,
    GuardAliasApplyUs,
    GuardRetHotPages,
    GuardRetPartlyPages,
    GuardRetGuardedPages,
    CardScanPages,
    CardScanHotPages,
    CardScanReadUs,
    CardScanSparseFaults,
    CardScanHashUs,
    CardScanChangedBytes,
    MoveDeferredUs,
    MoveShadowUs,
    CopyHotUs,
    CopyHotBytes,
    IndHits,
    IndMissFirst,
    IndMissStep,
    IndNonWinDispatch,
    IndNonWinMax,
    IndNonWinCount,
    IndUavEmitted,
    RecBindUs,
    RecEarlyTexUs,
    RecTablesUs,
    RecTablesMakeAtUs,
    RecMemoryReadyUs,
    RecIndirectUs,
    TablesFreshOver,
    TablesFreshCbv,
    TablesFreshHit,
    TablesFreshSpanSum,
    TablesFreshUsedSum,
    TablesFreshUncacheable,
    TablesCbvUnaligned,
    TablesCbvShort,
    TablesCbvNoShadow,
    CbvScratchHits,
    CbvScratchMisses,
}
const COUNTERS: usize = 136;
static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

pub(crate) fn add(c: C, n: u64) {
    COUNT[c as usize].fetch_add(n, Relaxed);
}

pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("OATHKEEP_COPY_MODE").as_deref() {
        Ok("plain") => 1,
        Ok("none") => 2,
        _ => 0,
    })
}

type Ranges = HashMap<(usize, usize), (u64, u64, u64, u32)>;
static RANGES: Mutex<Option<Ranges>> = Mutex::new(None);

static WHOLE: Mutex<Option<HashMap<(usize, usize), (u64, u64)>>> = Mutex::new(None);

pub(crate) fn whole(src: usize, len: usize, us: u64) {
    add(C::WholeUs, us);
    let mut map = WHOLE.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0));
    e.0 += 1;
    e.1 += us;
}

static LONGEST: Mutex<(u64, String)> = Mutex::new((0, String::new()));

static WAITS_BY_WORK: Mutex<Option<HashMap<String, (u64, u64)>>> = Mutex::new(None);

static COMMANDS_BY_WORK: Mutex<Option<HashMap<String, (u64, u64, u64)>>> = Mutex::new(None);

pub(crate) fn command_took(op: &str, ms: f64) {
    let us = (ms * 1e3) as u64;
    let mut by = COMMANDS_BY_WORK.lock().unwrap_or_else(|e| e.into_inner());
    let e = by
        .get_or_insert_with(HashMap::new)
        .entry(command_key(op))
        .or_insert((0, 0, 0));
    e.0 += 1;
    e.1 += us;
    e.2 = e.2.max(us);
}

fn command_key(op: &str) -> String {
    let head = op.split(" | ").next().unwrap_or(op);
    let word = head.split(' ').next().unwrap_or("");
    match (head.find('['), head.find(']')) {
        (Some(a), Some(b)) if a < b => format!("{word} {}", &head[a..=b]),
        _ => head.split(' ').take(2).collect::<Vec<_>>().join(" "),
    }
}

pub(crate) fn card_waited(us: u64, what: impl FnOnce() -> String) {
    let (n, t) = match us {
        0..2_000 => (C::CardUnder2, C::CardUnder2Us),
        2_000..8_000 => (C::Card2To8, C::Card2To8Us),
        8_000..32_000 => (C::Card8To32, C::Card8To32Us),
        _ => (C::CardOver32, C::CardOver32Us),
    };
    add(n, 1);
    add(t, us);
    let text = what();
    {
        let mut by = WAITS_BY_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let e = by
            .get_or_insert_with(HashMap::new)
            .entry(text.clone())
            .or_insert((0, 0));
        e.0 += 1;
        e.1 += us;
    }
    let mut longest = LONGEST.lock().unwrap_or_else(|e| e.into_inner());
    if us > longest.0 {
        *longest = (us, text);
    }
}

pub(crate) fn barrier_test() -> u8 {
    static TEST: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *TEST.get_or_init(|| match std::env::var("OATHKEEP_BARRIER_TEST").as_deref() {
        Ok("groups") => 1,
        Ok("indirect") => 2,
        Ok("both") => 3,
        _ => 0,
    })
}

static COMPARED: Mutex<Option<HashMap<(usize, usize), (u64, u64)>>> = Mutex::new(None);
static CARD_BACK: Mutex<Option<HashMap<(usize, usize), (u64, u64, u64, u64)>>> = Mutex::new(None);

pub(crate) fn compared(src: usize, len: usize, written: usize) {
    let mut map = COMPARED.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0));
    e.0 += 1;
    e.1 += written as u64;
}

pub(crate) fn compared_batch(entries: &[(usize, usize, usize)]) {
    if entries.is_empty() {
        return;
    }
    let mut map = COMPARED.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    for &(src, len, written) in entries {
        let e = map.entry((src, len)).or_insert((0, 0));
        e.0 += 1;
        e.1 += written as u64;
    }
}

pub(crate) fn card_back(src: usize, len: usize, pieces: usize, fetched: usize, written: usize) {
    let mut map = CARD_BACK.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0, 0, 0));
    e.0 += 1;
    e.1 += pieces as u64;
    e.2 += fetched as u64;
    e.3 += written as u64;
}

pub(crate) fn range(src: usize, len: usize, us: u64, kind: u32) {
    let mut map = RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0, 0, kind));
    e.0 += 1;
    e.1 += len as u64;
    e.2 += us;
}

static OWN: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

fn block_class(size: usize) -> usize {
    match size {
        0..0x1000 => 0,
        0x1000..0x1_0000 => 1,
        0x1_0000..0x10_0000 => 2,
        _ => 3,
    }
}

fn own(size: usize, made: bool) {
    let class = block_class(size);
    if made {
        OWN[class].fetch_add(size as u64, Relaxed);
        OWN[4 + class].fetch_add(1, Relaxed);
    } else {
        OWN[class].fetch_sub(size as u64, Relaxed);
        OWN[4 + class].fetch_sub(1, Relaxed);
    }
}

static BIG_AT: [std::sync::atomic::AtomicUsize; 64] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 64];
static BIG_BYTES: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
const BIG: usize = 32 << 20;

fn big(at: *mut u8, size: usize, made: bool) {
    if size < BIG {
        return;
    }
    for (place, bytes) in BIG_AT.iter().zip(&BIG_BYTES) {
        if made {
            if place
                .compare_exchange(0, at as usize, Relaxed, Relaxed)
                .is_ok()
            {
                bytes.store(size as u64, Relaxed);
                return;
            }
        } else if place.load(Relaxed) == at as usize {
            bytes.store(0, Relaxed);
            place.store(0, Relaxed);
            return;
        }
    }
}

fn big_blocks() -> String {
    let mut sizes: Vec<u64> = BIG_BYTES
        .iter()
        .map(|b| b.load(Relaxed))
        .filter(|&b| b != 0)
        .collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let text: Vec<String> = sizes
        .iter()
        .map(|b| format!("{:.2}", *b as f64 / (1 << 20) as f64))
        .collect();
    if text.is_empty() {
        "none".into()
    } else {
        text.join(", ")
    }
}

struct Counting;

// SAFETY: every call goes to the system's allocator unchanged; only
// counters are kept beside it.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let at = unsafe { std::alloc::System.alloc(layout) };
        if !at.is_null() {
            own(layout.size(), true);
            big(at, layout.size(), true);
        }
        at
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let at = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !at.is_null() {
            own(layout.size(), true);
            big(at, layout.size(), true);
        }
        at
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: std::alloc::Layout) {
        own(layout.size(), false);
        big(at, layout.size(), false);
        // SAFETY: the caller's contract, passed on.
        unsafe { std::alloc::System.dealloc(at, layout) };
    }

    unsafe fn realloc(&self, at: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let moved = unsafe { std::alloc::System.realloc(at, layout, size) };
        if !moved.is_null() {
            own(layout.size(), false);
            own(size, true);
            big(at, layout.size(), false);
            big(moved, size, true);
        }
        moved
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

#[cfg(test)]
mod command_tests {
    #[test]
    fn commands_are_named_by_their_pipeline() {
        use super::command_key;
        assert_eq!(
            command_key(
                "dispatch 182x1x1 [compute 24f989b98dcb03f0] | 24f989b98dcb03f0: u4=0xa6b080 [heap 341380: x]"
            ),
            "dispatch [compute 24f989b98dcb03f0]"
        );
        assert_eq!(
            command_key(
                "indirect kind 10 max 1 at 0xd9358fce0 [mesh 0750cb2d975c7563, pixel 085d374c5a795549] | t"
            ),
            "indirect [mesh 0750cb2d975c7563, pixel 085d374c5a795549]"
        );
        assert_eq!(
            command_key("texture upload 0x271b61c28b0 (2048x2048x1 f99 mips 10) sub 0"),
            "texture upload"
        );
        assert_eq!(
            command_key("alias copy of 0x26f66aca780 into 0x26fbdc93890"),
            "alias copy"
        );
    }
}

#[cfg(test)]
mod own_tests {
    use super::{OWN, block_class};
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn blocks_count_in_their_size_class() {
        assert_eq!(block_class(0), 0);
        assert_eq!(block_class(4095), 0);
        assert_eq!(block_class(4096), 1);
        assert_eq!(block_class(65535), 1);
        assert_eq!(block_class(65536), 2);
        assert_eq!(block_class((1 << 20) - 1), 2);
        assert_eq!(block_class(1 << 20), 3);
    }

    #[test]
    fn big_blocks_are_listed_while_they_live() {
        let size = (48usize << 20) + 4096;
        let listed = |at: usize| {
            super::BIG_AT
                .iter()
                .zip(&super::BIG_BYTES)
                .any(|(place, bytes)| {
                    place.load(Relaxed) == at && bytes.load(Relaxed) == size as u64
                })
        };
        let block = vec![0u8; size];
        let at = block.as_ptr() as usize;
        assert!(listed(at));
        assert!(super::big_blocks().contains("48.00"));
        drop(block);
        assert!(!listed(at));
    }

    #[test]
    fn a_block_is_counted_while_it_lives() {
        let block = vec![1u8; 8 << 20];
        assert!(OWN[3].load(Relaxed) >= 8 << 20);
        assert!(OWN[7].load(Relaxed) >= 1);
        drop(block);
        assert!(OWN[7].load(Relaxed) < u64::MAX / 2, "no count below zero");
    }
}

fn process_memory() -> (u64, u64) {
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
    (c.WorkingSetSize as u64, c.PagefileUsage as u64)
}

fn system_memory() -> (u64, u64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    (m.ullAvailPhys, m.ullAvailPageFile)
}

fn memory_map() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_MEMORY_MAP").is_none_or(|v| v != "0"))
}

fn in_memory(asked: &mut Vec<[usize; 2]>, stands: &mut Vec<(usize, u64)>, present: &mut [u64; 10]) {
    use windows_sys::Win32::System::ProcessStatus::K32QueryWorkingSetEx;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: our own process; `asked` holds that many entries of two
    // words each, as the call fills them in.
    let ok = unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            asked.as_mut_ptr().cast(),
            (asked.len() * 16) as u32,
        )
    } != 0;
    if ok {
        for (a, &(kind, bytes)) in asked.iter().zip(stands.iter()) {
            if a[1] & 1 != 0 {
                present[kind] += bytes;
            }
        }
    }
    asked.clear();
    stands.clear();
}

fn log_memory_map() {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_FREE, MEM_IMAGE, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_WRITECOMBINE,
        VirtualQuery,
    };
    const KINDS: [&str; 10] = [
        "the title's heaps (private, console addresses)",
        "views of the title's pages (mapped, console addresses)",
        "the title's page pool (mapped, 1 GB and more)",
        "process heap segments (private, 15.8 MB each)",
        "windows of title memory and other 4 MiB allocations",
        "write-combined (buffers shared with the card)",
        "other private allocations of 1 MB and more",
        "other private allocations under 1 MB",
        "files and other mappings at host addresses",
        "programs and libraries",
    ];
    const HOST: usize = 1 << 40;
    const STEP: usize = 64 << 10;
    let started = std::time::Instant::now();
    type Region = (usize, usize, usize, bool, u32, u32);
    let mut regions: Vec<Region> = Vec::new();
    let mut at = 0usize;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(at as *const _, &mut m, size_of_val(&m)) } == 0 {
            break;
        }
        if m.State != MEM_FREE {
            regions.push((
                m.AllocationBase as usize,
                m.BaseAddress as usize,
                m.RegionSize,
                m.State == MEM_COMMIT,
                m.Type,
                m.Protect,
            ));
        }
        let next = (m.BaseAddress as usize).saturating_add(m.RegionSize);
        if next <= at {
            break;
        }
        at = next;
    }
    let mut whole: HashMap<usize, usize> = HashMap::new();
    for r in &regions {
        *whole.entry(r.0).or_default() += r.2;
    }
    let kind = |r: &Region| -> usize {
        let size = whole.get(&r.0).copied().unwrap_or(0);
        let console = r.0 < HOST;
        if r.4 == MEM_IMAGE {
            9
        } else if r.4 == MEM_MAPPED {
            match (console, size >= 1 << 30) {
                (false, _) => 8,
                (true, true) => 2,
                (true, false) => 1,
            }
        } else if console {
            0
        } else if size == 0xfd_0000 {
            3
        } else if size == 4 << 20 {
            4
        } else if r.5 & PAGE_WRITECOMBINE != 0 {
            5
        } else if size >= 1 << 20 {
            6
        } else {
            7
        }
    };
    let mut committed = [0u64; 10];
    let mut present = [0u64; 10];
    let mut asked: Vec<[usize; 2]> = Vec::with_capacity(4096);
    let mut stands: Vec<(usize, u64)> = Vec::with_capacity(4096);
    for r in regions.iter().filter(|r| r.3) {
        let k = kind(r);
        committed[k] += r.2 as u64;
        let mut offset = 0;
        while offset < r.2 {
            asked.push([r.1 + offset, 0]);
            stands.push((k, STEP.min(r.2 - offset) as u64));
            if asked.len() == 4096 {
                in_memory(&mut asked, &mut stands, &mut present);
            }
            offset += STEP;
        }
    }
    if !asked.is_empty() {
        in_memory(&mut asked, &mut stands, &mut present);
    }
    let text: Vec<String> = (0..KINDS.len())
        .filter(|&k| committed[k] > 0)
        .map(|k| {
            format!(
                "{} {} MB ({} MB in memory)",
                KINDS[k],
                committed[k] >> 20,
                present[k] >> 20
            )
        })
        .collect();
    tracing::info!(
        "memory map ({} regions, {} ms): {}",
        regions.len(),
        started.elapsed().as_millis(),
        text.join("; ")
    );
    let mut by_allocation: HashMap<usize, (usize, u64)> = HashMap::new();
    for r in regions.iter().filter(|r| r.3) {
        let k = kind(r);
        if k == 5 || k == 6 {
            by_allocation.entry(r.0).or_insert((k, 0)).1 += r.2 as u64;
        }
    }
    let mut groups: HashMap<(usize, usize), (u64, u64)> = HashMap::new();
    for (base, &(k, bytes)) in &by_allocation {
        let group = groups
            .entry((k, whole.get(base).copied().unwrap_or(0)))
            .or_default();
        group.0 += 1;
        group.1 += bytes;
    }
    let mut groups: Vec<((usize, usize), (u64, u64))> = groups.into_iter().collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.1.1));
    let biggest: Vec<String> = groups
        .iter()
        .take(20)
        .map(|&((k, size), (n, bytes))| {
            format!(
                "{n} of {:.2} MB{}: {} MB",
                size as f64 / (1 << 20) as f64,
                if k == 5 { " write-combined" } else { "" },
                bytes >> 20
            )
        })
        .collect();
    tracing::info!(
        "memory map, other private allocations by size, the {} biggest groups of {} (committed): {}",
        biggest.len(),
        groups.len(),
        biggest.join("; ")
    );
}

fn log_memory() {
    use std::sync::atomic::Ordering::Relaxed;
    let mb = |v: u64| v as f64 / (1 << 20) as f64;
    let (working, private) = process_memory();
    let (free, left) = system_memory();
    let copies = crate::shadow::memory();
    let staged = crate::texture::memory();
    let textures = [0, 1].map(|i| crate::resource::HOST_TEXTURES[i].load(Relaxed));
    let made: Vec<u64> = crate::host::MADE.iter().map(|m| m.load(Relaxed)).collect();
    let windows = crate::shadow::windows();
    let stagings = crate::texture::let_go();
    let census = crate::resource::census();
    let kinds: Vec<u64> = crate::resource::KINDS
        .iter()
        .map(|k| k.load(Relaxed))
        .collect();
    let card = crate::host::host()
        .and_then(|h| h.video_memory())
        .unwrap_or_default();
    if let Some(h) = crate::host::host() {
        h.log_video_memory("with the memory line");
    }
    static LINES: AtomicU64 = AtomicU64::new(0);
    static WALKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if memory_map() && LINES.fetch_add(1, Relaxed) % 6 == 5 && !WALKING.swap(true, Relaxed) {
        let walk = std::thread::Builder::new()
            .name("oathkeep-memory-map".into())
            .spawn(|| {
                log_memory_map();
                WALKING.store(false, Relaxed);
            });
        if walk.is_err() {
            WALKING.store(false, Relaxed);
        }
    }
    let kept = crate::pipeline::kept();
    tracing::info!(
        "memory kept beside the lines below: {} shader containers {:.0} MB, {} translations \
         {:.0} MB; the pipeline library's file {:.0} MB (its last save wrote {:.0} MB)",
        kept[0],
        mb(kept[1]),
        kept[2],
        mb(kept[3]),
        mb(kept[4]),
        mb(kept[5]),
    );
    tracing::info!(
        "memory of textures: {} title textures have no host copy yet ({:.0} MB of title memory); \
         host copies the host renders to or writes: {} alive {:.0} MB; host copies the title \
         fills: {} alive {:.0} MB ({} of them placed), {} let go {:.0} MB and {} made again \
         {:.0} MB since the start; {} host copies made in all; passes that made none for lack of \
         room: {}; the card: {:.0} MB in use of a budget of {:.0} MB, {:.0} MB of the PC's memory \
         shared with it in use",
        census[2],
        mb(census[3]),
        census[0],
        mb(census[1]),
        census[4],
        mb(census[5]),
        census[6],
        kinds[2],
        mb(kinds[3]),
        kinds[4],
        mb(kinds[5]),
        kinds[6],
        crate::draw::HEAP_TEXTURES_FULL.load(Relaxed),
        mb(card[1]),
        mb(card[0]),
        mb(card[3]),
    );
    let targets = crate::resource::targets_census();
    tracing::info!(
        "host copies the sweep never lets go: {} the host renders to or writes {:.0} MB ({} of them placed {:.0} MB);          of them {} have title memory that is not mapped now {:.0} MB, {} were not used for 30 s and are held by no          list and no slot {:.0} MB, {} are both {:.0} MB; and {} copies of textures the title fills that the host          copied into {:.0} MB",
        targets[0],
        mb(targets[1]),
        targets[10],
        mb(targets[11]),
        targets[2],
        mb(targets[3]),
        targets[4],
        mb(targets[5]),
        targets[6],
        mb(targets[7]),
        targets[8],
        mb(targets[9]),
    );
    tracing::info!(
        "memory let go since the start: {} windows outside buffers {:.0} MB, {} staging buffers \
         {:.0} MB; of the windows alive, {:.0} MB lie between the lowest and highest address \
         asked for in each",
        windows[0],
        mb(windows[1]),
        stagings[0],
        mb(stagings[1]),
        mb(windows[2]),
    );
    tracing::info!(
        "memory: {:.0} MB private ({:.0} MB of it in memory; the PC has {:.0} MB free and {:.0} MB \
         left to commit); host copies of title memory: {} alive {:.0} MB, of them {} windows \
         outside buffers {:.0} MB; snapshot buffers kept: {} reserving {:.0} MB; staging of \
         textures: {} alive {:.0} MB, {} kept by their texture; host textures: {} alive {:.0} MB, \
         {} placed in title memory; made so far: {} upload buffers {:.0} MB, {} shared buffers \
         {:.0} MB, {} card buffers {:.0} MB",
        mb(private),
        mb(working),
        mb(free),
        mb(left),
        copies[0],
        mb(copies[1]),
        copies[2],
        mb(copies[3]),
        copies[4],
        mb(copies[5]),
        staged[0],
        mb(staged[1]),
        staged[2],
        textures[0],
        mb(textures[1]),
        crate::resource::placed_textures(),
        made[0],
        mb(made[1]),
        made[2],
        mb(made[3]),
        made[4],
        mb(made[5]),
    );
    tracing::info!(
        "memory, placed textures: {} of them were asked for again with other filler words in their description          (one resource and host copy each since the filler left the key; OATHKEEP_PLACED_FILLER=1 keeps it)",
        crate::resource::placed_filler_twins()
    );
    let alive: Vec<u64> = crate::host::ALIVE.iter().map(|a| a.load(Relaxed)).collect();
    let window = crate::shadow::window_use();
    let views = crate::descriptor::views_kept();
    let own: Vec<u64> = OWN.iter().map(|o| o.load(Relaxed)).collect();
    tracing::info!(
        "memory, what holds it: {} host lists alive; their scratch memory: {} chunks {:.0} MB, of          them {} chunks {:.0} MB spare ({} taken from the spare ones so far); their other upload          buffers: {} alive {:.0} MB; windows outside buffers (grid {} KiB): {:.0} MB of their          pages ever copied, {} shown by descriptor slots {:.0} MB, {} held by lists or the pass          over the heap {:.0} MB, {} asked for without a length {:.0} MB; buffer views of the heap          whose slot showed another copy than the one brought up to date: {}; distinct views          kept: {} ({} free) {:.0} MB; our own allocations: {:.0} MB in {} blocks (under 4 KiB          {:.0} MB in {}, to 64 KiB {:.0} MB in {}, to 1 MiB {:.0} MB in {}, larger {:.0} MB in {})",
        alive[0],
        alive[1],
        mb(alive[2]),
        alive[3],
        mb(alive[4]),
        alive[5],
        alive[6],
        mb(alive[7]),
        window[7] >> 10,
        mb(window[0]),
        window[1],
        mb(window[2]),
        window[3],
        mb(window[4]),
        window[5],
        mb(window[6]),
        crate::draw::HEAP_VIEWS_ELSEWHERE.load(Relaxed),
        views[0],
        views[1],
        mb(views[2]),
        mb(own[..4].iter().sum()),
        own[4..].iter().sum::<u64>(),
        mb(own[0]),
        own[4],
        mb(own[1]),
        own[5],
        mb(own[2]),
        own[6],
        mb(own[3]),
        own[7],
    );
    let lists: Vec<u64> = crate::host::LISTS.iter().map(|l| l.load(Relaxed)).collect();
    tracing::info!(
        "memory, more: views named whole in their records since the start: {} constant buffer \
         views, {} views of placed memory; of the view table's entries {} are constant buffer \
         views and {} views of placed memory; shapes of placed views kept: {}; host lists: {} \
         made by the host, {} preludes made for them, private bytes grew {:.0} MB while \
         lists were made; twins of depth textures: {:.0} MB; host copies of \
         placed textures whose title memory is not mapped any more (estimate): {:.0} MB, of \
         them still shown by a descriptor slot {:.0} MB; the pipeline library's mapped file \
         let go from memory so far: {:.0} MB; our own blocks of 32 MB and more, in MB: {}",
        views[6],
        views[7],
        views[3],
        views[4],
        views[5],
        lists[0],
        lists[1],
        lists[2] as i64 as f64 / (1 << 20) as f64,
        mb(census[7]),
        mb(census[8]),
        mb(census[9]),
        mb(kept[6]),
        big_blocks(),
    );
    tracing::info!(
        "memory, windows: the {:.0} MB of windows lie over {:.0} MB of title memory (the rest is \
         memory that is in more than one window, and is copied once for each); host lists never \
         reset since they were made: {}",
        mb(copies[3]),
        mb(crate::shadow::window_cover()),
        lists[3],
    );
}

fn now_us() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
}

static PRESENTED_AT: AtomicU64 = AtomicU64::new(0);
static SUBMITTED_AT: AtomicU64 = AtomicU64::new(0);
static RECORDED_SINCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static SUBMITTED_SINCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[derive(Default, Clone, Copy)]
struct Submitter {
    submissions: u64,
    between: u64,
    between_user: u64,
    between_kernel: u64,
    between_ours: u64,
    lock: u64,
    inside_user: u64,
    inside_kernel: u64,
}

static SUBMITTERS: Mutex<Vec<(u32, Submitter)>> = Mutex::new(Vec::new());
static RECORDERS: Mutex<Vec<(u32, u64, u64)>> = Mutex::new(Vec::new());

thread_local! {
    static LEFT: std::cell::Cell<Option<(u64, u64, u64)>> = const { std::cell::Cell::new(None) };
    static ENTERED: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
    static OURS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static UNSENT: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

fn micros(t: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)) / 10
}

fn thread_times() -> (u64, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: our own thread and four valid out pointers.
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    (micros(user), micros(kernel))
}

fn process_times() -> (u64, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: our own process and four valid out pointers.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    (micros(user), micros(kernel))
}

fn submitter(all: &mut Vec<(u32, Submitter)>, id: u32) -> &mut Submitter {
    let at = match all.iter().position(|s| s.0 == id) {
        Some(at) => at,
        None => {
            all.push((id, Submitter::default()));
            all.len() - 1
        }
    };
    &mut all[at].1
}

pub(crate) fn recorded(started: std::time::Instant) {
    let ns = started.elapsed().as_nanos() as u64;
    add(C::RecordCalls, 1);
    add(C::RecordNs, ns);
    if !RECORDED_SINCE.load(Relaxed) && !RECORDED_SINCE.swap(true, Relaxed) {
        let began = now_us().saturating_sub(ns / 1000);
        add(
            C::UntilRecordUs,
            began.saturating_sub(PRESENTED_AT.load(Relaxed)),
        );
    }
    OURS.with(|o| o.set(o.get() + ns));
    let (unsent, calls) = UNSENT.with(|u| u.get());
    let (unsent, calls) = (unsent + ns, calls + 1);
    if unsent < 2_000_000 {
        UNSENT.with(|u| u.set((unsent, calls)));
        return;
    }
    UNSENT.with(|u| u.set((0, 0)));
    let id = thread_id();
    let mut all = RECORDERS.lock().unwrap_or_else(|e| e.into_inner());
    match all.iter_mut().find(|r| r.0 == id) {
        Some(r) => (r.1, r.2) = (r.1 + unsent, r.2 + calls),
        None => all.push((id, unsent, calls)),
    }
}

pub(crate) fn submission_begins() {
    let now = now_us();
    if !SUBMITTED_SINCE.load(Relaxed) && !SUBMITTED_SINCE.swap(true, Relaxed) {
        add(
            C::UntilSubmitUs,
            now.saturating_sub(PRESENTED_AT.load(Relaxed)),
        );
    }
    let (user, kernel) = thread_times();
    ENTERED.with(|e| e.set((user, kernel)));
    let ours = OURS.with(|o| o.replace(0));
    if let Some((left, u, k)) = LEFT.with(|l| l.get()) {
        let mut all = SUBMITTERS.lock().unwrap_or_else(|e| e.into_inner());
        let s = submitter(&mut all, thread_id());
        s.between += now.saturating_sub(left);
        s.between_user += user.saturating_sub(u);
        s.between_kernel += kernel.saturating_sub(k);
        s.between_ours += ours;
    }
}

pub(crate) fn submission_ends(lock: u64) {
    let now = now_us();
    SUBMITTED_AT.store(now, Relaxed);
    let (user, kernel) = thread_times();
    let (u, k) = ENTERED.with(|e| e.get());
    LEFT.with(|l| l.set(Some((now, user, kernel))));
    OURS.with(|o| o.set(0));
    let mut all = SUBMITTERS.lock().unwrap_or_else(|e| e.into_inner());
    let s = submitter(&mut all, thread_id());
    s.submissions += 1;
    s.lock += lock;
    s.inside_user += user.saturating_sub(u);
    s.inside_kernel += kernel.saturating_sub(k);
}

pub(crate) fn present_begins() {
    add(
        C::TailUs,
        now_us().saturating_sub(SUBMITTED_AT.load(Relaxed)),
    );
}

pub(crate) fn presented() {
    add(C::Frames, 1);
    PRESENTED_AT.store(now_us(), Relaxed);
    RECORDED_SINCE.store(false, Relaxed);
    SUBMITTED_SINCE.store(false, Relaxed);
}

fn page_faults() -> u64 {
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
    u64::from(c.PageFaultCount)
}

pub(crate) fn log(frames: f64) {
    let counted = COUNT[C::Frames as usize].swap(0, Relaxed);
    let frames = if counted > 0 { counted as f64 } else { frames };
    static FAULTS: AtomicU64 = AtomicU64::new(0);
    let faults = page_faults();
    let faults = faults - FAULTS.swap(faults, Relaxed).min(faults);
    let c: Vec<f64> = COUNT
        .iter()
        .map(|a| a.swap(0, Relaxed) as f64 / frames)
        .collect();
    let mb = |v: f64| v / 1e6;
    let ms = |v: f64| v / 1e3;
    tracing::info!(
        "copies per frame: {:.1} submissions; {:.0} title copies {:.1} MB; {:.0} refreshes {:.1} MB, \
         {:.0} skipped {:.1} MB; {:.0} runs {:.1} ms; {:.0} queries {:.2} ms, moves {:.2} ms, \
         {:.2} MB unmapped; {:.0} page faults; {:.0} watches {:.2} ms, {:.1} MB checked, {:.1} MB \
         unchanged; {:.1} fallbacks; {:.0} guard faults; textures {:.1} MB watched and \
         changed, {:.1} MB hashed, {:.1} MB of them changed",
        c[C::Submissions as usize],
        c[C::Copies as usize],
        mb(c[C::CopyBytes as usize]),
        c[C::Refreshes as usize],
        mb(c[C::RefreshBytes as usize]),
        c[C::Skipped as usize],
        mb(c[C::SkippedBytes as usize]),
        c[C::Runs as usize],
        ms(c[C::RunUs as usize]),
        c[C::Queries as usize],
        ms(c[C::QueryUs as usize]),
        ms(c[C::MoveUs as usize]),
        mb(c[C::Unmapped as usize]),
        faults as f64 / frames,
        c[C::Watches as usize],
        ms(c[C::WatchUs as usize]),
        mb(c[C::Checked as usize]),
        mb(c[C::Unchanged as usize]),
        c[C::Fallbacks as usize],
        c[C::GuardFaults as usize],
        mb(c[C::TexWatched as usize]),
        mb(c[C::TexHashed as usize]),
        mb(c[C::TexChanged as usize]),
    );
    tracing::info!(
        "recording per frame ({frames:.0} frames): {:.0} list calls {:.1} ms (all recording \
         threads); {:.1} ms waiting for pipelines; dynamic slots: {:.0} tables asked, {:.0} \
         refused, {:.1} ms searching; notes {:.1} ms; shader-written ranges: {:.0} steps in \
         {:.1} submissions, snapshots {:.1} ms, {:.1} MB compared back; sparse reads {:.1} ms; \
         guards {:.1} ms; {:.1} MB copied whole (memory not watched); {:.1} frame waits \
         sleeping {:.1} ms (all threads); ranges of shaders that cannot store to buffers: \
         {:.0} steps {:.1} MB left out",
        c[C::RecordCalls as usize],
        c[C::RecordNs as usize] / 1e6,
        ms(c[C::PipelineWaitUs as usize]),
        c[C::DynamicAsked as usize],
        c[C::DynamicRefused as usize],
        c[C::DynamicNs as usize] / 1e6,
        c[C::NoteNs as usize] / 1e6,
        c[C::GpuWrites as usize],
        c[C::BackSubmissions as usize],
        ms(c[C::SnapshotUs as usize]),
        mb(c[C::BackCompared as usize]),
        ms(c[C::SparseUs as usize]),
        ms(c[C::GuardUs as usize]),
        mb(c[C::WholeBytes as usize]),
        c[C::PaceWaits as usize],
        ms(c[C::PaceUs as usize]),
        c[C::GpuWritesLeftOut as usize],
        mb(c[C::GpuWritesLeftOutBytes as usize]),
    );
    let [together, written, mapped] =
        [0, 1, 2].map(|n| crate::watch::ALIASES[n].load(std::sync::atomic::Ordering::Relaxed));
    tracing::info!(
        "guards over several views of a page, since the start: {together} blocks guarded with their other views, \
         {written} views opened by a write through another, {mapped} ranges opened because a view of them was mapped"
    );
    static CPU: Mutex<(u64, u64)> = Mutex::new((0, 0));
    let (user, kernel) = process_times();
    let (user, kernel) = {
        let mut last = CPU.lock().unwrap_or_else(|e| e.into_inner());
        let spent = (user.saturating_sub(last.0), kernel.saturating_sub(last.1));
        *last = (user, kernel);
        (spent.0 as f64 / 1e3 / frames, spent.1 as f64 / 1e3 / frames)
    };
    tracing::info!(
        "frame account per frame: after a present {:.1} ms until the first list call and {:.1} ms \
         until the first submission; {:.1} ms from the last submission to the present call; \
         lists reset in {:.1} ms; the process ran {user:.0} ms user + {kernel:.0} ms kernel on \
         all threads; what shaders wrote: {:.1} ms finding {:.0} pieces that differ, {:.1} ms \
         putting them back; texture uploads: {:.0} recorded, {:.0} left out; {:.1} held \
         submissions run at the present with their waits not met; waits for memory values: \
         {:.0} looks {:.1} ms; {:.0} host lists made {:.1} ms; {:.0} parts of held submissions \
         ran alone that the next part could have run with",
        ms(c[C::UntilRecordUs as usize]),
        ms(c[C::UntilSubmitUs as usize]),
        ms(c[C::TailUs as usize]),
        c[C::ResetNs as usize] / 1e6,
        ms(c[C::BackScanUs as usize]),
        c[C::BackDiffer as usize],
        ms(c[C::BackSyncUs as usize]),
        c[C::TexUploads as usize],
        c[C::TexSkipped as usize],
        c[C::Forced as usize],
        c[C::WaitChecks as usize],
        c[C::WaitCheckNs as usize] / 1e6,
        c[C::HostLists as usize],
        c[C::HostListNs as usize] / 1e6,
        c[C::PartsTogether as usize],
    );
    tracing::info!(
        "preparing breakdown per frame: ask_once {:.1} ms, run_deferred {:.1} ms, prelude {:.1} ms, \
         batch_flush {:.1} ms, snapshots {:.1} ms, card_prepare_write {:.1} ms, \
         card_fill_upload {:.1} ms, card_record {:.1} ms, other {:.1} ms",
        ms(c[C::PrepAskOnceUs as usize]),
        ms(c[C::PrepRunDeferredUs as usize]),
        ms(c[C::PrepPreludeUs as usize]),
        ms(c[C::PrepBatchFlushUs as usize]),
        ms(c[C::PrepSnapshotsUs as usize]),
        ms(c[C::PrepCardPrepareWriteUs as usize]),
        ms(c[C::PrepCardFillUploadUs as usize]),
        ms(c[C::PrepCardRecordUs as usize]),
        ms(c[C::PrepOtherUs as usize]),
    );
    tracing::info!(
        "micro-diag guards/frame: read_fast {:.0}x {:.2} ms, pre_alias {:.2} ms, write_wait {:.2} ms, \
         protect {:.2} ms, alias_apply {:.2} ms; ret_pages: hot {:.0}, partly {:.0}, guarded {:.0}",
        c[C::GuardReadFastN as usize],
        ms(c[C::GuardReadFastUs as usize]),
        ms(c[C::GuardPreAliasUs as usize]),
        ms(c[C::GuardWriteWaitUs as usize]),
        ms(c[C::GuardProtectUs as usize]),
        ms(c[C::GuardAliasApplyUs as usize]),
        c[C::GuardRetHotPages as usize],
        c[C::GuardRetPartlyPages as usize],
        c[C::GuardRetGuardedPages as usize],
    );
    tracing::info!(
        "micro-diag scan_card/moves/frame: scan {:.0} pages ({:.0} hot), read {:.2} ms ({:.0} sparse faults), \
         hash {:.2} ms, changed {:.2} MB; moves: deferred {:.2} ms, shadow {:.2} ms; copy_hot {:.2} ms {:.2} MB",
        c[C::CardScanPages as usize],
        c[C::CardScanHotPages as usize],
        ms(c[C::CardScanReadUs as usize]),
        c[C::CardScanSparseFaults as usize],
        ms(c[C::CardScanHashUs as usize]),
        mb(c[C::CardScanChangedBytes as usize]),
        ms(c[C::MoveDeferredUs as usize]),
        ms(c[C::MoveShadowUs as usize]),
        ms(c[C::CopyHotUs as usize]),
        mb(c[C::CopyHotBytes as usize]),
    );
    tracing::info!(
        "micro-diag indirect/frame: hits {:.0}, miss_first {:.0}, miss_step {:.0}, \
         non_win(dispatch {:.0}, max>1 {:.0}, count {:.0}), uav_emitted {:.0}",
        c[C::IndHits as usize],
        c[C::IndMissFirst as usize],
        c[C::IndMissStep as usize],
        c[C::IndNonWinDispatch as usize],
        c[C::IndNonWinMax as usize],
        c[C::IndNonWinCount as usize],
        c[C::IndUavEmitted as usize],
    );
    tracing::info!(
        "micro-diag rec/tables/frame: bind {:.2} ms (early_tex {:.2} ms, tables {:.2} ms, make_at {:.2} ms, \
         mem_ready {:.2} ms), indirect {:.2} ms; fresh_tables: over {:.0}, cbv_fail {:.0} (unalign {:.0}, \
         short {:.0}, no_shadow {:.0}), hit {:.0}, uncacheable {:.0}, span_sum {:.0}, used_sum {:.0}; \
         cbv_scratch: hit {:.0}, miss {:.0}",
        ms(c[C::RecBindUs as usize]),
        ms(c[C::RecEarlyTexUs as usize]),
        ms(c[C::RecTablesUs as usize]),
        ms(c[C::RecTablesMakeAtUs as usize]),
        ms(c[C::RecMemoryReadyUs as usize]),
        ms(c[C::RecIndirectUs as usize]),
        c[C::TablesFreshOver as usize],
        c[C::TablesFreshCbv as usize],
        c[C::TablesCbvUnaligned as usize],
        c[C::TablesCbvShort as usize],
        c[C::TablesCbvNoShadow as usize],
        c[C::TablesFreshHit as usize],
        c[C::TablesFreshUncacheable as usize],
        c[C::TablesFreshSpanSum as usize],
        c[C::TablesFreshUsedSum as usize],
        c[C::CbvScratchHits as usize],
        c[C::CbvScratchMisses as usize],
    );
    let whole = WHOLE.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut whole: Vec<_> = whole.into_iter().flatten().collect();
    whole.sort_by_key(|&((_, len), (n, _))| std::cmp::Reverse(len as u64 * n));
    tracing::info!(
        "  copied whole (memory not watched): {:.1} ms per frame in {} ranges; the largest follow",
        ms(c[C::WholeUs as usize]),
        whole.len()
    );
    for ((src, len), (n, us)) in whole.into_iter().take(6) {
        tracing::info!(
            "  copied whole from {src:#x}+{len:#x}: {:.1}x/frame, {:.2} MB/frame, {:.2} ms/frame",
            n as f64 / frames,
            (len as u64 * n) as f64 / 1e6 / frames,
            us as f64 / 1e3 / frames
        );
    }
    let longest = std::mem::take(&mut *LONGEST.lock().unwrap_or_else(|e| e.into_inner()));
    tracing::info!(
        "  waiting for the card per frame: {:.1} submissions under 2 ms ({:.1} ms), {:.1} of 2-8 ms \
         ({:.1} ms), {:.1} of 8-32 ms ({:.1} ms), {:.1} longer ({:.1} ms); {:.1} submissions hand \
         nothing back to the title ({:.1} ms of the waiting); the longest since the last line: \
         {:.1} ms, {}",
        c[C::CardUnder2 as usize],
        ms(c[C::CardUnder2Us as usize]),
        c[C::Card2To8 as usize],
        ms(c[C::Card2To8Us as usize]),
        c[C::Card8To32 as usize],
        ms(c[C::Card8To32Us as usize]),
        c[C::CardOver32 as usize],
        ms(c[C::CardOver32Us as usize]),
        c[C::NothingBack as usize],
        ms(c[C::NothingBackUs as usize]),
        longest.0 as f64 / 1e3,
        longest.1
    );
    let by = WAITS_BY_WORK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let mut by: Vec<_> = by.into_iter().flatten().collect();
    by.sort_by_key(|&(_, (_, us))| std::cmp::Reverse(us));
    for (what, (n, us)) in by.into_iter().take(12) {
        tracing::info!(
            "  the card's time by what ran: {:.1} ms a frame in {:.1} submissions a frame: {}",
            us as f64 / 1e3 / frames,
            n as f64 / frames,
            what.chars().take(700).collect::<String>()
        );
    }
    let by = COMMANDS_BY_WORK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let mut by: Vec<_> = by.into_iter().flatten().collect();
    by.sort_by_key(|&(_, (_, us, _))| std::cmp::Reverse(us));
    if !by.is_empty() {
        let mut kinds: Vec<(String, u64, u64)> = Vec::new();
        for (what, (n, us, _)) in &by {
            let word = what.split(' ').next().unwrap_or("").to_owned();
            match kinds.iter_mut().find(|k| k.0 == word) {
                Some(k) => (k.1, k.2) = (k.1 + n, k.2 + us),
                None => kinds.push((word, *n, *us)),
            }
        }
        kinds.sort_by_key(|k| std::cmp::Reverse(k.2));
        let all: u64 = kinds.iter().map(|k| k.2).sum();
        let text: Vec<String> = kinds
            .iter()
            .map(|(word, n, us)| {
                format!(
                    "{word} {:.1} ms in {:.0}",
                    *us as f64 / 1e3 / frames,
                    *n as f64 / frames
                )
            })
            .collect();
        tracing::info!(
            "  the card's time by command (timing run), all {} kinds together: {:.1} ms a frame; by first word: {}",
            by.len(),
            all as f64 / 1e3 / frames,
            text.join(", ")
        );
    }
    for (what, (n, us, longest)) in by.into_iter().take(40) {
        tracing::info!(
            "  the card's time by command (timing run): {:.2} ms a frame in {:.1} commands a frame, the longest {:.2} ms: {what}",
            us as f64 / 1e3 / frames,
            n as f64 / frames,
            longest as f64 / 1e3
        );
    }
    tracing::info!(
        "  waits for every earlier shader write, per frame: {:.0} for the title's barrier groups ({:.0} skipped), {:.0} \
         for its barriers on buffers; {:.0} indirect commands bounded (a dispatch of ours between two \
         such waits each); barrier experiment {}",
        c[C::WaitsForGroups as usize],
        c[C::WaitsForGroupsSkipped as usize],
        c[C::WaitsForBuffers as usize],
        c[C::IndirectBounded as usize],
        barrier_test()
    );
    let compared = COMPARED.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut compared: Vec<_> = compared.into_iter().flatten().collect();
    compared.sort_by_key(|&((_, len), (n, _))| std::cmp::Reverse(len as u64 * n));
    tracing::info!(
        "  compared after submissions: {} ranges; the largest follow",
        compared.len()
    );
    for ((src, len), (n, written)) in compared.into_iter().take(8) {
        tracing::info!(
            "  compared {src:#x}+{len:#x}: {:.1}x/frame, {:.2} MB/frame, {:.3} MB/frame of it written back",
            n as f64 / frames,
            (len as u64 * n) as f64 / 1e6 / frames,
            written as f64 / 1e6 / frames
        );
    }
    let card_back = CARD_BACK.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut card_back: Vec<_> = card_back.into_iter().flatten().collect();
    card_back.sort_by_key(|&(_, (_, _, fetched, written))| std::cmp::Reverse((fetched, written)));
    if !card_back.is_empty() {
        tracing::info!(
            "  card copies fetched back: {} shadows; the largest follow",
            card_back.len()
        );
        for ((src, len), (n, pieces, fetched, written)) in card_back.into_iter().take(5) {
            tracing::info!(
                "  card copy back {src:#x}+{len:#x}: {:.1}x/frame ({:.0} pieces/frame), {:.2} MB/frame fetched, {:.3} MB/frame written back",
                n as f64 / frames,
                pieces as f64 / frames,
                fetched as f64 / 1e6 / frames,
                written as f64 / 1e6 / frames
            );
        }
    }
    let mut submitters = std::mem::take(&mut *SUBMITTERS.lock().unwrap_or_else(|e| e.into_inner()));
    submitters.sort_by_key(|s| std::cmp::Reverse(s.1.submissions));
    for (id, s) in submitters.into_iter().take(4) {
        let per = |v: u64| v as f64 / 1e3 / frames;
        tracing::info!(
            "  thread {id} submitted {:.1}x/frame: between its submissions {:.1} ms (running \
             {:.1} ms user + {:.1} ms kernel; {:.1} ms in list calls), in them running {:.1} ms \
             user + {:.1} ms kernel, {:.1} ms waiting for the submission lock",
            s.submissions as f64 / frames,
            per(s.between),
            per(s.between_user),
            per(s.between_kernel),
            s.between_ours as f64 / 1e6 / frames,
            per(s.inside_user),
            per(s.inside_kernel),
            per(s.lock),
        );
    }
    let mut recorders = std::mem::take(&mut *RECORDERS.lock().unwrap_or_else(|e| e.into_inner()));
    recorders.sort_by_key(|r| std::cmp::Reverse(r.1));
    let busiest: Vec<String> = recorders
        .iter()
        .take(6)
        .map(|r| {
            format!(
                "thread {} {:.0} ms in {:.0} calls",
                r.0,
                r.1 as f64 / 1e6 / frames,
                r.2 as f64 / frames
            )
        })
        .collect();
    tracing::info!(
        "  list calls by thread: {} threads; {}",
        recorders.len(),
        busiest.join(", ")
    );
    log_memory();
    let ranges = RANGES.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut ranges: Vec<_> = ranges.into_iter().flatten().collect();
    ranges.sort_by_key(|&(_, (_, bytes, _, _))| std::cmp::Reverse(bytes));
    let mut logged_keys: std::collections::HashSet<(usize, usize)> =
        std::collections::HashSet::new();
    let log_range = |label: &str, src: usize, len: usize, n: u64, bytes: u64, us: u64| {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(src as *const _, &mut info, size_of_val(&info)) };
        let kind = info.Type;
        tracing::info!(
            "  {label} {src:#x}+{len:#x} (memory {kind:#x}): {:.1}x/frame, {:.2} MB/frame, {:.2} ms/frame",
            n as f64 / frames,
            bytes as f64 / 1e6 / frames,
            us as f64 / 1e3 / frames
        );
    };
    for &((src, len), (n, bytes, us, _)) in ranges.iter().take(10) {
        logged_keys.insert((src, len));
        log_range("copied from", src, len, n, bytes, us);
    }
    ranges.sort_by_key(|&(_, (_, _, us, _))| std::cmp::Reverse(us));
    for ((src, len), (n, bytes, us, _)) in ranges
        .into_iter()
        .filter(|&(k, (_, _, us, _))| !logged_keys.contains(&k) && (us as f64 / 1e3 / frames) >= 0.5)
        .take(5)
    {
        log_range("slowest copy", src, len, n, bytes, us);
    }
}
