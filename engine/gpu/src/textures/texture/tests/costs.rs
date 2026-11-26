//! How long this PC's card takes to make the allocations a staged texture needs, for
//! `cargo test -p oathkeep-gpu allocation_costs -- --ignored --nocapture`.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

fn texture(size: u32) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(size),
        Height: size,
        DepthOrArraySize: 1,
        MipLevels: 0,
        Format: DXGI_FORMAT_BC7_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    }
}

fn buffer(size: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    }
}

fn committed(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, flags: D3D12_HEAP_FLAGS, desc: &D3D12_RESOURCE_DESC) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
    let state = if kind == D3D12_HEAP_TYPE_UPLOAD { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COMMON };
    let mut out = None;
    // SAFETY: plain creation.
    unsafe { device.CreateCommittedResource(&heap, flags, desc, state, None, &mut out) }.expect("committed");
    out.unwrap()
}

fn timed(what: &str, n: usize, threads: usize, make: &(dyn Fn(usize) -> ID3D12Resource + Sync)) {
    let began = std::time::Instant::now();
    let all: Vec<_> = std::thread::scope(|s| {
        (0..threads)
            .map(|t| s.spawn(move || (t..n).step_by(threads).map(make).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    let took = began.elapsed().as_secs_f64() * 1e3;
    println!("{what}, {threads} thread(s): {n} in {took:.1} ms, {:.0} us each", took * 1e3 / n as f64);
    drop(all);
}

#[test]
#[ignore = "a measurement, not a check"]
fn look_costs() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    const BLOCK: usize = 64 << 10;
    for (name, blocks, there) in [("350 KB, all there", 6, 6), ("22 MB, all there", 350, 350), ("22 MB, 1 MB there", 350, 16)] {
        let make = || {
            // SAFETY: a reservation of our own, of which the first part
            // is committed and touched.
            unsafe {
                let at = VirtualAlloc(std::ptr::null(), blocks * BLOCK, MEM_RESERVE, PAGE_READWRITE);
                VirtualAlloc(at, there * BLOCK, MEM_COMMIT, PAGE_READWRITE);
                std::ptr::write_bytes(at.cast::<u8>(), 1, there * BLOCK);
                at as usize
            }
        };
        let (asked, noted) = (make(), make());
        for block in 0..there {
            crate::title_memory_view(noted + block * BLOCK, BLOCK, 9, 0);
        }
        for (how, at) in [("asking the host", asked), ("by the views", noted)] {
            const N: u32 = 200;
            let began = std::time::Instant::now();
            for _ in 0..N {
                assert_eq!(super::super::mapped_bytes(at as *const u8, blocks * BLOCK), (there * BLOCK) as u64);
            }
            println!("{name}, {how}: {:.1} us a look", began.elapsed().as_secs_f64() * 1e6 / f64::from(N));
        }
        crate::memory::inplace::views_gone(noted, blocks * BLOCK);
    }
}

#[test]
fn a_digest_tells_bytes_apart() {
    use super::super::digest;
    let zeros = [0u8; 400];
    let mut seen: Vec<super::super::Digest> = (0..400).map(|n| digest(&zeros[..n])).collect();
    for at in 0..400 {
        for value in [1u8, 0x80, 0xff] {
            let mut bytes = zeros;
            bytes[at] = value;
            seen.push(digest(&bytes));
        }
    }
    let text: Vec<u8> = (0..400u32).map(|i| (i * 37 >> 2) as u8).collect();
    for at in 0..399 {
        let mut bytes = text.clone();
        bytes.swap(at, at + 1);
        if bytes != text {
            seen.push(digest(&bytes));
        }
    }
    seen.push(digest(&text));
    let n = seen.len();
    seen.sort_unstable_by_key(|d| d.0);
    seen.dedup();
    assert_eq!(seen.len(), n);
    assert_eq!(digest(&text), digest(&text.clone()));
}

#[test]
#[ignore = "a measurement, not a check"]
fn cold_texture_costs() {
    let Some(host) = crate::backend::host() else { return };
    let desc = [3u32, 0, 0, 0, 1024, 0, 1024, 0xb_0001, 98, 1, 0, 0x105, 0, 0];
    // SAFETY: 56 bytes in the description's layout.
    let desc = unsafe { desc.as_ptr().cast::<crate::d3d12::resource::ResourceDesc>().read_unaligned() };
    let layout = crate::d3d12::resource::texture_layout(&desc).unwrap().planes[0].layout.clone();
    let size = layout.size as usize;
    let title: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let (_buffer, up) = host.upload_buffer(2 * layout.size).unwrap();
    // SAFETY: the mapping.
    let up = unsafe { std::slice::from_raw_parts_mut(up, 2 * size) };
    let mut read = vec![0u8; size];
    const N: u32 = 200;
    let per_mb = |t: std::time::Duration| t.as_secs_f64() * 1e3 / (f64::from(N) * size as f64 / 1e6);
    let timed = |what: &str, f: &mut dyn FnMut()| {
        f();
        let started = std::time::Instant::now();
        (0..N).for_each(|_| f());
        println!("{what}: {:.3} ms/MB", per_mb(started.elapsed()));
    };
    timed("read through the kernel or a safe copy", &mut || {
        std::hint::black_box(crate::backend::read_into(title.as_ptr(), &mut read));
    });
    timed("blake3", &mut || {
        std::hint::black_box(blake3::hash(&title));
    });
    timed("digest", &mut || {
        std::hint::black_box(super::super::digest(&title));
    });
    timed("detiled into upload memory", &mut || {
        let mut at = 0;
        for (mip, m) in layout.mips.iter().enumerate() {
            let pitch = (m.width as usize * 16).next_multiple_of(256);
            let len = pitch * m.height as usize;
            layout.detile(&title, mip as u32, 0, &mut up[at..at + len], pitch, len).unwrap();
            at += len.next_multiple_of(512);
        }
    });
    timed("copied into upload memory", &mut || up[..size].copy_from_slice(&title));
    if let Some(file) = std::env::var_os("OATHKEEP_BENCH_FILE") {
        use std::os::windows::fs::{FileExt, OpenOptionsExt};
        const NO_BUFFERING: u32 = 0x2000_0000;
        let file = std::fs::OpenOptions::new().read(true).custom_flags(NO_BUFFERING).open(file).unwrap();
        let len = file.metadata().unwrap().len();
        let mut block = vec![0u8; size + 4096];
        let skew = block.as_ptr().align_offset(4096);
        let n = size / 4096 * 4096;
        let mut at = 0u64;
        timed("read from the disk, one request at a time", &mut || {
            at = (at + 7919 * 4096 * 64) % (len - n as u64) / 4096 * 4096;
            file.seek_read(&mut block[skew..skew + n], at).unwrap();
        });
    }
}

#[test]
#[ignore = "a measurement, not a check"]
fn allocation_costs() {
    let Some(host) = crate::backend::host() else { return };
    let device = &host.device;
    let not_zeroed = D3D12_HEAP_FLAG_CREATE_NOT_ZEROED;
    const N: usize = 1000;
    for size in [128u32, 512, 1024] {
        let (t, b) = (texture(size), buffer(u64::from(size * size) * 4 / 3 + 65536));
        println!("-- BC7 {size}x{size}");
        for threads in [1, 4, 8] {
            timed("texture committed", N, threads, &|_| committed(device, D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_FLAG_NONE, &t));
            timed("texture committed not zeroed", N, threads, &|_| committed(device, D3D12_HEAP_TYPE_DEFAULT, not_zeroed, &t));
            let mapped = |flags| {
                let b = &b;
                move |_| {
                    let r = committed(device, D3D12_HEAP_TYPE_UPLOAD, flags, b);
                    let mut data = std::ptr::null_mut();
                    // SAFETY: a live upload buffer.
                    unsafe { r.Map(0, None, Some(&mut data)) }.unwrap();
                    // SAFETY: the first byte of the mapping (its first touch).
                    unsafe { data.cast::<u8>().write_volatile(1) };
                    r
                }
            };
            timed("staging committed and mapped", N, threads, &mapped(D3D12_HEAP_FLAG_NONE));
            timed("staging committed and mapped, not zeroed", N, threads, &mapped(not_zeroed));
        }
        // SAFETY: plain query.
        let info = unsafe { device.GetResourceAllocationInfo(0, &[t]) };
        let per_heap = ((64u64 << 20) / info.SizeInBytes).max(1) as usize;
        let heaps: Vec<ID3D12Heap> = (0..N.div_ceil(per_heap))
            .map(|_| {
                let desc = D3D12_HEAP_DESC {
                    SizeInBytes: per_heap as u64 * info.SizeInBytes,
                    Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | not_zeroed,
                    ..Default::default()
                };
                let mut heap = None;
                // SAFETY: plain creation.
                unsafe { device.CreateHeap(&desc, &mut heap) }.expect("heap");
                heap.unwrap()
            })
            .collect();
        let place = |i: usize| {
            let mut out: Option<ID3D12Resource> = None;
            let at = (i % per_heap) as u64 * info.SizeInBytes;
            // SAFETY: a part of a live heap no other texture has.
            unsafe { device.CreatePlacedResource(&heaps[i / per_heap], at, &t, D3D12_RESOURCE_STATE_COMMON, None, &mut out) }.expect("placed");
            out.unwrap()
        };
        timed("texture placed in 64 MB heaps", N, 1, &place);
        timed("texture placed in 64 MB heaps", N, 8, &place);
    }
}
