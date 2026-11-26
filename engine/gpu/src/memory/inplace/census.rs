// The census: which of the title memory that has host copies today could
// be opened, and by which opening.

use std::ffi::c_void;

use windows_sys::Win32::System::Memory as mem;

use super::rig::{Rig, hr, working_set_mb};
use super::views::{Cover, cover, views};
use super::{From, K64, open, place, private_whole};

/// Counts and bytes by kind, in the order of [`KINDS`].
#[derive(Default, Clone, Copy)]
struct Tally([(u64, u64); 6]);

const KINDS: [&str; 6] = [
    "private, committed whole (a part can be opened)",
    "view memory, one run of a section (one placed buffer)",
    "view memory, scattered pages",
    "view memory with unmapped pages",
    "private but not committed whole, or guarded",
    "other",
];

fn kind_of(at: usize, len: usize, known: &mut Vec<(usize, usize)>) -> usize {
    match cover(at, len) {
        Cover::Run(..) => return 1,
        Cover::Scattered => return 2,
        Cover::Holes => return 3,
        Cover::None => {}
    }
    let end = at + len;
    if known.iter().any(|&(from, to)| from <= at && end <= to) {
        return 0;
    }
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    if n == 0 {
        return 5;
    }
    let reach = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    if info.Type == mem::MEM_PRIVATE && info.State == mem::MEM_COMMIT && info.Protect == mem::PAGE_READWRITE {
        known.push((info.BaseAddress as usize, reach));
        return if reach >= end { 0 } else { 4 };
    }
    match info.Type {
        mem::MEM_PRIVATE => 4,
        mem::MEM_MAPPED => 3,
        _ => 5,
    }
}

/// The census lines for `spans` (start, bytes, whether a window outside
/// buffers).
pub(super) fn census_of(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let mut known = Vec::new();
    let (mut buffers, mut windows) = (Tally::default(), Tally::default());
    for &(start, len, window) in spans {
        let kind = kind_of(start as usize, len as usize, &mut known);
        let tally = if window { &mut windows } else { &mut buffers };
        tally.0[kind].0 += 1;
        tally.0[kind].1 += len;
    }
    let mut lines = Vec::new();
    for (name, tally) in [("title buffers", buffers), ("windows outside buffers", windows)] {
        let all = tally.0.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        lines.push(format!("census, {name}: {} of {} MB", all.0, all.1 >> 20));
        for (kind, &(n, bytes)) in KINDS.iter().zip(&tally.0) {
            if n > 0 {
                lines.push(format!("census,   {kind}: {n} of {} MB", bytes >> 20));
            }
        }
    }
    let v = views();
    let (count, bytes) = (v.views.len(), v.views.values().map(|view| view.0 as u64).sum::<u64>());
    let pages = v.views.values().filter(|view| view.0 == K64).count();
    let big = v.views.values().map(|view| view.0).max().unwrap_or(0);
    lines.push(format!(
        "census, the title's views of physical pages: {count} of {} MB; {pages} of them one 64 KiB page; the longest {} MB",
        bytes >> 20,
        big >> 20
    ));
    for &(_, bytes, committed) in &v.sections {
        lines.push(format!(
            "census, a section of {} MB, {}",
            bytes >> 20,
            if committed { "committed: it can be opened as one heap" } else { "reserved: refused as a heap until it is made with SEC_COMMIT" }
        ));
    }
    lines
}

/// With [`super::probe_level`] 2: one part of the title's own private memory
/// (the start of the biggest such title buffer, 4 MiB at most) is opened
/// for a moment, a buffer placed, 64 KiB read by the GPU and compared
/// with what the CPU reads there (the title may be writing it meanwhile:
/// the count of equal bytes is given), and all of it released. While the
/// heap stands (milliseconds) a `VirtualFree` of the title's on those
/// pages would fail.
pub(super) fn real_part(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let Some(host) = crate::backend::host() else {
        return Vec::new();
    };
    let mut known = Vec::new();
    let found = spans
        .iter()
        .filter(|s| !s.2 && s.1 >= 2 * K64 as u64)
        .filter(|s| kind_of(s.0 as usize, s.1 as usize, &mut known) == 0)
        .max_by_key(|s| s.1);
    let Some(&(start, len, _)) = found else {
        return vec!["the title's memory: no title buffer in private, committed memory to try".to_owned()];
    };
    let at = (start as usize).next_multiple_of(K64);
    let bytes = ((start + len) as usize).saturating_sub(at).min(4 << 20) & !(K64 - 1);
    if bytes == 0 || !private_whole(at, bytes) {
        return vec!["the title's memory: the buffer found holds no whole 64 KiB page that is still committed".to_owned()];
    }
    let mut lines = Vec::new();
    let (set, started) = (working_set_mb(), std::time::Instant::now());
    let opened = open(&host.device, From::Part(at, bytes));
    lines.push(format!(
        "the title's memory, {} KiB at {at:#x} (of the title buffer {start:#x}+{len:#x}): {}; in {:.1} ms; working set {set} -> {} MB",
        bytes >> 10,
        hr(&opened),
        started.elapsed().as_secs_f64() * 1000.0,
        working_set_mb()
    ));
    if let Ok(heap) = &opened {
        let placed = place(&host.device, heap, 0, bytes as u64);
        lines.push(format!("  a buffer placed over it: {}", hr(&placed)));
        if let (Ok(buffer), Ok(mut rig)) = (&placed, Rig::new(&host.device)) {
            let mut before = vec![0u8; K64];
            crate::backend::copy_mapped(before.as_mut_ptr(), at as *const u8, K64, false);
            let got = rig.fetch(buffer, 0, K64);
            let mut after = vec![0u8; K64];
            crate::backend::copy_mapped(after.as_mut_ptr(), at as *const u8, K64, false);
            match got {
                Ok(got) => {
                    let equal = (0..K64).filter(|&i| got[i] == before[i] || got[i] == after[i]).count();
                    let zeros = got.iter().filter(|&&b| b == 0).count();
                    lines.push(format!(
                        "  the GPU read 64 KiB of the title's own bytes: {equal} of {K64} equal to what the CPU read just before or just after ({zeros} of them zero)"
                    ));
                }
                Err(e) => lines.push(format!("  the GPU's read failed: {e}")),
            }
        }
    }
    lines
}
