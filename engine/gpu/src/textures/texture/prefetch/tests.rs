use std::sync::Arc;
use windows::Win32::Graphics::Direct3D12::*;

use super::worker::PriorityQueue;
use super::{Taken, claim, drain, send, there};

/// The texels of a placed texture arrive by the copy queue, all levels of
/// them, and what waits for them is called then; a second asking finds the
/// first on its way, and a later one is the list's.
#[test]
fn a_placed_texture_is_filled_ahead_of_the_lists() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    let Some(host) = crate::backend::host() else { return };
    // 256x256 BC7, nine levels.
    let desc = [3u32, 0, 0, 0, 256, 0, 256, 0x9_0001, 98, 1, 0, 0x105, 0, 0];
    let whole = crate::d3d12::resource::placed_bytes(&desc).unwrap() as usize;
    // SAFETY: memory of our own that stays.
    let at = unsafe { VirtualAlloc(std::ptr::null(), whole, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) }.cast::<u8>();
    // SAFETY: as above.
    unsafe { (0..whole).for_each(|i| at.add(i).write((i * 13 + i / 509) as u8)) };
    let texture = crate::d3d12::resource::texture_at(at as u64, &desc).unwrap();
    crate::d3d12::resource::want_level(texture, 0);
    let copy = crate::d3d12::resource::host_copy(texture).unwrap();
    if !crate::memory::pool::fresh(&copy.texture) {
        return;
    }
    assert!(!there(texture));
    let Some(Taken::New(job)) = claim(texture, 0, u64::MAX) else { panic!("not taken") };
    let (told, heard) = std::sync::mpsc::channel();
    let also = told.clone();
    let Some(Taken::Known(same)) = claim(texture, 0, u64::MAX) else { panic!("not known") };
    assert!(Arc::ptr_eq(&job, &same));
    job.then(move || told.send(1).unwrap());
    same.then(move || also.send(2).unwrap());
    send(&job);
    let wait = std::time::Duration::from_secs(20);
    assert_eq!((heard.recv_timeout(wait), heard.recv_timeout(wait)), (Ok(1), Ok(2)));
    assert!(there(texture) && claim(texture, 0, u64::MAX).is_none());
    // Level 0 as the card has it, and as the layout says.
    let layout = crate::d3d12::resource::texture_memory(texture)
        .and_then(|(_, d)| crate::d3d12::resource::texture_layout(&d).map(|t| t.planes[0].layout.clone()))
        .unwrap();
    let (pitch, rows) = (1024usize, 64usize);
    let mut expected = vec![0u8; pitch * rows];
    // SAFETY: the memory above.
    let tiled = unsafe { std::slice::from_raw_parts(at, whole) };
    layout.detile(tiled, 0, 0, &mut expected, pitch, pitch * rows).unwrap();
    let back = host.read_back_buffer((pitch * rows) as u64).unwrap();
    let list = host.new_list().unwrap();
    list.close();
    list.reset();
    let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(98),
            Width: 256,
            Height: 256,
            Depth: 1,
            RowPitch: pitch as u32,
        },
    };
    let to = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(&back) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
    };
    let from = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&copy.texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    };
    // SAFETY: an open list, live resources; the mapping spans the buffer.
    unsafe {
        list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        list.close();
        host.execute(&[&list]);
        let mut data = std::ptr::null_mut();
        back.Map(0, None, Some(&mut data)).unwrap();
        let seen = std::slice::from_raw_parts(data.cast::<u8>(), pitch * rows);
        assert!(seen == expected.as_slice(), "level 0's blocks");
        back.Unmap(0, None);
    }
}

#[test]
fn drain_waits_until_then_callbacks_have_run() {
    use std::sync::atomic::AtomicBool;
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    let Some(_host) = crate::backend::host() else { return };
    let desc = [3u32, 0, 0, 0, 128, 0, 128, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
    let whole = crate::d3d12::resource::placed_bytes(&desc).unwrap() as usize;
    // SAFETY: memory of our own that stays for the test.
    let at = unsafe { VirtualAlloc(std::ptr::null(), whole, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) }.cast::<u8>();
    // SAFETY: as above.
    unsafe { std::ptr::write_bytes(at, 0x5a, whole) };
    let texture = crate::d3d12::resource::texture_at(at as u64, &desc).unwrap();
    crate::d3d12::resource::want_level(texture, 0);
    let copy = crate::d3d12::resource::host_copy(texture).unwrap();
    if !crate::memory::pool::fresh(&copy.texture) {
        return;
    }
    let Some(Taken::New(job)) = claim(texture, 0, u64::MAX) else { panic!("not taken") };
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    job.then(move || flag.store(true, std::sync::atomic::Ordering::Release));
    send(&job);
    assert!(drain() >= 1);
    assert!(ran.load(std::sync::atomic::Ordering::Acquire), "job.then callback must run before drain() returns");
    assert_eq!(drain(), 0);
}

#[test]
fn priority_queue_promotes_draw_demanded_jobs_ahead_of_background() {
    let mut q = PriorityQueue::new();
    q.push(1, (1u64, 4u64 << 20), false);
    q.push(2, (2u64, 4u64 << 20), false);
    q.push(3, (3u64, 64u64 << 10), true);
    q.push(4, (4u64, 4u64 << 20), false);

    // Draw demands serial 4: it jumps ahead of both background jobs AND
    // queued compact urgent jobs.
    assert!(q.promote(4));
    assert!(!q.promote(999));

    let first = q.pop_batch(16 << 20, 2, |&(_, bytes)| bytes);
    assert_eq!(first.iter().map(|&(id, _)| id).collect::<Vec<_>>(), vec![4, 3]);

    let rest = q.pop_batch(16 << 20, 8, |&(_, bytes)| bytes);
    assert_eq!(rest.iter().map(|&(id, _)| id).collect::<Vec<_>>(), vec![1, 2]);
    assert!(q.is_empty());
}
