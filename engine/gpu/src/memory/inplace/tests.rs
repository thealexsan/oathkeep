use std::collections::BTreeMap;
use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;
use windows_sys::Win32::System::Memory as mem;

use super::census::census_of;
use super::rig::{Rig, fill, same};
use super::views::cover_in;
use super::{
    Cover, K64, MEM_WRITE_WATCH, cover, place_part, private_whole, probe, shown_bytes, shown_runs,
    title_memory_view, views_gone,
};

/// Views as the runtime tells them: a range inside one view, or over
/// views that continue each other in one section, is one run; over
/// views from elsewhere it is scattered; over a gap it has holes.
#[test]
fn the_views_say_how_much_of_a_range_is_there() {
    const K64: usize = 64 << 10;
    // Addresses nothing has: only the record is asked.
    let at = 0x7ff1_0000_0000_usize;
    for (block, blocks) in [(0, 1), (1, 1), (4, 2)] {
        title_memory_view(at + block * K64, blocks * K64, 7, 0);
    }
    let shown = |from: usize, len: usize| shown_bytes(at + from, len);
    assert_eq!(shown(0, 8 * K64), Some(4 * K64 as u64));
    assert_eq!(shown(100, K64), Some(K64 as u64));
    assert_eq!(shown(K64 + 100, 2 * K64), Some(K64 as u64 - 100));
    assert_eq!(shown(5 * K64 + 1, 10), Some(10));
    assert_eq!(shown(2 * K64, 2 * K64), None);
    assert_eq!(shown(6 * K64, 2 * K64), None);
    // The same as runs: views that touch are one, cut to the range.
    let runs = |from: usize, len: usize| shown_runs(at + from, len);
    assert_eq!(runs(0, 8 * K64), vec![(at, 2 * K64), (at + 4 * K64, 2 * K64)]);
    assert_eq!(runs(100, K64), vec![(at + 100, K64)]);
    assert_eq!(runs(5 * K64 + 1, 10), vec![(at + 5 * K64 + 1, 10)]);
    assert_eq!(runs(2 * K64, 2 * K64), vec![]);
    views_gone(at + K64, K64);
    assert_eq!(shown(0, 2 * K64), Some(K64 as u64));
    assert_eq!(runs(0, 2 * K64), vec![(at, K64)]);
    views_gone(at, 8 * K64);
    assert_eq!(shown(0, 8 * K64), None);
}

#[test]
fn views_say_what_one_placed_buffer_could_show() {
    let mut views = BTreeMap::new();
    let at = 0x7_0000_0000usize;
    views.insert(at, (4 * K64, 9, 16 * K64 as u64));
    // Continues the first in the section.
    views.insert(at + 4 * K64, (2 * K64, 9, 20 * K64 as u64));
    // Follows in address, from elsewhere in the section.
    views.insert(at + 6 * K64, (K64, 9, 3 * K64 as u64));
    // After a gap of one page.
    views.insert(at + 8 * K64, (K64, 9, 4 * K64 as u64));
    assert_eq!(cover_in(&views, at + K64, 2 * K64), Cover::Run(9, 17 * K64 as u64));
    assert_eq!(cover_in(&views, at + K64, 5 * K64), Cover::Run(9, 17 * K64 as u64));
    assert_eq!(cover_in(&views, at, 7 * K64), Cover::Scattered);
    assert_eq!(cover_in(&views, at + 6 * K64, 3 * K64), Cover::Holes);
    assert_eq!(cover_in(&views, at + 7 * K64, K64), Cover::None);
    assert_eq!(cover_in(&views, at - K64, 2 * K64), Cover::None);
}

/// What the runtime forgets when memory is mapped anew.
#[test]
fn remapped_memory_forgets_its_views() {
    let at = 0x7_4000_0000usize;
    title_memory_view(at, 4 * K64, 77, 0);
    title_memory_view(at + 4 * K64, K64, 77, 4 * K64 as u64);
    assert_eq!(cover(at, 5 * K64), Cover::Run(77, 0));
    // One page inside the first view goes: the whole view is forgotten
    // (the runtime maps and tells again what stays of it).
    views_gone(at + K64, K64);
    assert_eq!(cover(at, K64), Cover::None);
    assert_eq!(cover(at + 4 * K64, K64), Cover::Run(77, 4 * K64 as u64));
    views_gone(at + 4 * K64, K64);
    assert_eq!(cover(at + 4 * K64, K64), Cover::None);
}

/// Only private memory committed whole, read-write, is taken for a part.
#[test]
fn only_private_memory_committed_whole_is_a_part() {
    // SAFETY: an allocation of the test's own.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE, mem::PAGE_NOACCESS).cast::<u8>();
        assert!(!base.is_null());
        assert!(!private_whole(base as usize, K64), "reserved only");
        mem::VirtualAlloc(base.cast(), 4 * K64, mem::MEM_COMMIT, mem::PAGE_READWRITE);
        assert!(private_whole(base as usize, 4 * K64));
        assert!(private_whole(base as usize + K64, 2 * K64));
        assert!(!private_whole(base as usize, 5 * K64), "reaches into the reserved half");
        let mut old = 0;
        mem::VirtualProtect(base.cast(), K64, mem::PAGE_READONLY, &mut old);
        assert!(!private_whole(base as usize, 2 * K64), "a guarded page");
        mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
    }
}

pub(crate) fn warp() -> Option<ID3D12Device> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::*;
    // SAFETY: plain DXGI queries and device creation on the software
    // adapter; a card is never opened.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
        let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            return None;
        }
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
        device
    }
}

/// The probe, whole, on the software adapter: the same calls the card
/// gets. The runtime's and the kernel's rules hold there; the card's
/// driver is what a run with `OATHKEEP_INPLACE_PROBE=1` adds.
#[test]
fn warp_probe_opens_title_like_memory_and_the_gpu_sees_its_bytes() {
    let _turn = crate::execution::indirect::tests::warp_turn();
    let Some(device) = warp() else { return };
    if device.cast::<ID3D12Device13>().is_err() {
        eprintln!("no ID3D12Device13 here: parts of an allocation cannot be opened");
        return;
    }
    let probe = probe(&device, &|| None, 4);
    for line in &probe.lines {
        eprintln!("in place probe: {line}");
    }
    assert!(probe.part_reads, "the GPU reads private memory in place");
    assert!(probe.part_writes, "GPU writes land in private memory");
    assert!(probe.section_reads && probe.section_writes, "a committed section in place");
    assert!(probe.section_outlives_views, "the section's heap does not depend on views");
    assert!(probe.reserved_section_refused, "a reserved section is refused");
}

/// Stage 1's own call on the software adapter: a title buffer that
/// begins and ends off the 64 KiB lines is placed over its pages, the
/// GPU reads the buffer's bytes at the offset the views use, and its
/// write lands at the title's address. Memory that is not committed
/// whole is turned down without asking the host.
#[test]
fn warp_a_title_buffer_in_place_shows_its_bytes_at_its_offset() {
    let _turn = crate::execution::indirect::tests::warp_turn();
    let Some(device) = warp() else { return };
    if device.cast::<ID3D12Device13>().is_err() {
        return;
    }
    // SAFETY: an allocation of the test's own.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
        let (start, len) = (base as u64 + K64 as u64 + 0x1230, 2 * K64 as u64 + 5);
        let placed = place_part(&device, start, len).unwrap_or_else(|e| panic!("not placed: {e:?}"));
        assert_eq!(placed.bias, 0x1230);
        assert_eq!(placed.gpu, placed.buffer.GetGPUVirtualAddress() + 0x1230);
        assert_eq!(placed.buffer.GetDesc().Width, 3 * K64 as u64, "the pages that hold the buffer");
        let mut rig = Rig::new(&device).unwrap();
        fill(start as *mut u8, len as usize, 7);
        let got = rig.fetch(&placed.buffer, placed.bias, len as usize).unwrap();
        assert!(same(start as *const u8, &got), "the GPU reads the title buffer's bytes");
        rig.store(&placed.buffer, placed.bias + 16, &[0xa5; 64]).unwrap();
        assert!(same((start + 16) as *const u8, &[0xa5; 64]), "the GPU's write is at the title's address");
        drop(placed);
        // Half of it decommitted: turned down, the host not asked.
        // The heap lets go of the pages a moment after its last
        // reference, if the adapter is busy with other tests' work.
        let gone = (0..500).any(|_| {
            let done = mem::VirtualFree(base.add(2 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            if !done {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            done
        });
        assert!(gone, "the heap keeps the pages");
        assert!(matches!(place_part(&device, start, len), Err(None)));
        mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
    }
}

/// The census names each kind of memory.
#[test]
fn census_tells_private_memory_from_views() {
    // SAFETY: an allocation of the test's own.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as u64;
        let view = 0x7_8000_0000u64;
        title_memory_view(view as usize, 2 * K64, 5, 0);
        title_memory_view(view as usize + 2 * K64, K64, 5, 9 * K64 as u64);
        let lines = census_of(&[(base, 2 * K64 as u64, false), (base + 4 * K64 as u64, K64 as u64, true), (view, K64 as u64, true), (view, 3 * K64 as u64, false)]);
        let text = lines.join("\n");
        assert!(text.contains("census, title buffers: 2 of"), "{text}");
        assert!(text.contains("census,   private, committed whole (a part can be opened): 1 of"), "{text}");
        assert!(text.contains("census,   view memory, scattered pages: 1 of"), "{text}");
        assert!(text.contains("census,   view memory, one run of a section (one placed buffer): 1 of"), "{text}");
        views_gone(view as usize, 3 * K64);
        mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
    }
}
