//! Process and address-space memory diagnostics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::alloc::{big_blocks, owned};

/// The process's memory in bytes: in physical memory, and committed
/// (private bytes).
pub(crate) fn process_memory() -> (u64, u64) {
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

/// The PC's memory in bytes: physical memory free, and what is left to
/// commit.
fn system_memory() -> (u64, u64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    (m.ullAvailPhys, m.ullAvailPageFile)
}

/// Whether the address space is walked for the memory map line
/// ([`log_memory_map`]). On by default (this module is in diagnostics
/// builds only); `OATHKEEP_MEMORY_MAP=0` leaves the walk out.
fn memory_map() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_MEMORY_MAP").is_none_or(|v| v != "0"))
}

/// Asks which of the pages in `asked` (address, answer) are in physical
/// memory and adds the bytes each stands for (kind, bytes) to its kind.
fn in_memory(asked: &mut Vec<[usize; 2]>, stands: &mut Vec<(usize, u64)>, present: &mut [u64; 10]) {
    use windows_sys::Win32::System::ProcessStatus::K32QueryWorkingSetEx;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: our own process; `asked` holds that many entries of two
    // words each, as the call fills them in.
    let ok = unsafe {
        K32QueryWorkingSetEx(GetCurrentProcess(), asked.as_mut_ptr().cast(), (asked.len() * 16) as u32)
    } != 0;
    if ok {
        for (a, &(kind, bytes)) in asked.iter().zip(stands.iter()) {
            // Bit 0 of the answer: the page is in the process's memory.
            if a[1] & 1 != 0 {
                present[kind] += bytes;
            }
        }
    }
    asked.clear();
    stands.clear();
}

/// Logs what the process has committed by kind, from a walk of its address
/// space, and how much of each kind is in physical memory (one page in
/// sixteen is asked about). The kinds tell the title's own memory from
/// ours: the title's heaps and page views lie at console addresses (below
/// 1 TiB), everything the host allocates lies far above. Run 30, seen
/// from outside at 18.0 GB private: 4.6 GB the title's heaps, 2.6 GB in
/// 162 segments of the process heap, 2.7 GB in 707 allocations of 4 MiB
/// (windows of title memory), 0.46 GB the pipeline library's file; mapped
/// and so not private: 6.2 GB the title's page pool, 2.0 GB the driver's
/// shader cache file.
/// shortcut: the walk takes about 0.6 s on a thread of its own, and a
/// query of a region of gigabytes holds up other threads' memory calls
/// for its length (89 ms for the 6.2 GB pool), so it runs with every
/// sixth memory line only.
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
    /// Console addresses lie below this.
    const HOST: usize = 1 << 40;
    /// One page asked about stands for this much.
    const STEP: usize = 64 << 10;
    let started = std::time::Instant::now();
    // Allocation, start, bytes, committed, type, protection.
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
    // Each allocation's whole size, reserved parts included.
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
        .map(|k| format!("{} {} MB ({} MB in memory)", KINDS[k], committed[k] >> 20, present[k] >> 20))
        .collect();
    tracing::info!(
        "memory map ({} regions, {} ms): {}",
        regions.len(),
        started.elapsed().as_millis(),
        text.join("; ")
    );
    // The private allocations the kinds above leave as "other" (and the
    // write-combined ones), grouped by their size: the sizes name their
    // owners (the pipeline library's file to the byte, a snapshot buffer
    // of a title heap's size, the driver's blocks). Committed bytes by
    // allocation first, then allocations by (kind, whole size).
    let mut by_allocation: HashMap<usize, (usize, u64)> = HashMap::new();
    for r in regions.iter().filter(|r| r.3) {
        let k = kind(r);
        if k == 5 || k == 6 {
            by_allocation.entry(r.0).or_insert((k, 0)).1 += r.2 as u64;
        }
    }
    let mut groups: HashMap<(usize, usize), (u64, u64)> = HashMap::new();
    for (base, &(k, bytes)) in &by_allocation {
        let group = groups.entry((k, whole.get(base).copied().unwrap_or(0))).or_default();
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

/// Logs where the memory is: the process and the PC, then what this crate
/// holds by kind (alive now), then the host buffers made since the start.
pub(super) fn log_memory() {
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
    let kinds: Vec<u64> = crate::resource::KINDS.iter().map(|k| k.load(Relaxed)).collect();
    let card = crate::host::host()
        .and_then(|h| h.video_memory())
        .unwrap_or_default();
    // Everything the system says of the two memory groups, beside the
    // budget and use the lines below print.
    if let Some(h) = crate::host::host() {
        h.log_video_memory("with the memory line");
    }
    // What else holds memory: the map, on a thread of its own, with every
    // sixth line; and what the shader table and the pipeline library keep.
    static LINES: AtomicU64 = AtomicU64::new(0);
    static WALKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if memory_map() && LINES.fetch_add(1, Relaxed) % 6 == 5 && !WALKING.swap(true, Relaxed) {
        let walk = std::thread::Builder::new().name("victory-memory-map".into()).spawn(|| {
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
    {
        let [textures, staging] = crate::pool::memory();
        tracing::info!("memory in blocks (OATHKEEP_TEXTURE_POOL=0 turns them off): {textures} MB of textures, {staging} MB of their staging buffers");
    }
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
    // What the line above leaves open: what is alive of the buffers "made
    // so far", what the windows hold and what holds them, and what our own
    // code has allocated.
    let alive: Vec<u64> = crate::host::ALIVE.iter().map(|a| a.load(Relaxed)).collect();
    let window = crate::shadow::window_use();
    let views = crate::descriptor::views_kept();
    let own: Vec<u64> = (0..8).map(owned).collect();
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
    // Round 2: what the view table still holds and what goes around it,
    // what host lists cost, the card's share that is the layer's own or
    // the title's no more, and which big blocks are ours.
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
    // Round 3: whether windows hold the same memory more than once (they
    // can, once they are cut finer than 4 MiB), and how many host lists
    // the title made and never used.
    tracing::info!(
        "memory, windows: the {:.0} MB of windows lie over {:.0} MB of title memory (the rest is \
         memory that is in more than one window, and is copied once for each); host lists never \
         reset since they were made: {}",
        mb(copies[3]),
        mb(crate::shadow::window_cover()),
        lists[3],
    );
}
