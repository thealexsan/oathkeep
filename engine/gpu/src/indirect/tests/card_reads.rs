use super::super::*;
use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, s};


/// Whether the card can read a texture's descriptor half written
/// (ignored: a measurement of the card, `OATHKEEP_BENCH_S` seconds a way,
/// 5). Two slots change between a texture of 64x64 and one of 2048x2048
/// as fast as a thread can write them, while groups that each load the
/// descriptor anew ask its size and read its last texel: size and texel
/// of two textures are one descriptor half written. (Both lie in one heap
/// that is there as a whole: a read past the small one finds memory.)
#[test]
#[ignore]
fn descriptors_written_under_readers() {
    let Some(mut gpu) = WarpHarness::on(std::env::var_os("OATHKEEP_BENCH_WARP").is_some()) else {
        return;
    };
    let seconds = std::env::var("OATHKEEP_BENCH_S").ok().and_then(|v| v.parse().ok()).unwrap_or(5u64);
    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=1,b0), DescriptorTable(SRV(t0,numDescriptors=2,flags=DESCRIPTORS_VOLATILE)), UAV(u0)"
cbuffer C : register(b0) { uint odd; }
Texture2D<float4> t[2] : register(t0);
RWByteAddressBuffer o : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 g : SV_GroupID, uint i : SV_GroupIndex) {
uint w, h, n;
uint which = (g.x + odd) & 1;
t[which].GetDimensions(0, w, h, n);
uint r = (uint)(t[which].Load(int3(w - 1, h - 1, 0)).x * 255.0 + 0.5);
// 0: small, 1: large, 2: no size of the two, 3: small with another texel, 4: large with another
uint kind = (w == 64 && h == 64) ? (r == 17 ? 0 : 3) : (w == 2048 && h == 2048) ? (r == 34 ? 1 : 4) : 2;
if (i == 0) {
    uint was;
    o.InterlockedAdd(kind * 4, 1, was);
    if (kind > 1) { o.Store(32, w); o.Store(36, h); o.Store(40, r); }
}
}
"#;
    let pass = compile_cs(&gpu.device, SOURCE).expect("the pass must compile");
    let texture = |size: u64| D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: size,
        Height: size as u32,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT(28),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    // SAFETY: plain creation; each texture in its own half of the heap.
    let (heap, textures) = unsafe {
        let mut heap: Option<ID3D12Heap> = None;
        gpu.device
            .CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: 64 << 20,
                    Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    Alignment: 0,
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                },
                &mut heap,
            )
            .unwrap();
        let heap = heap.unwrap();
        let textures = [(0u64, 64u64), (32 << 20, 2048)].map(|(at, size)| {
            let mut made: Option<ID3D12Resource> = None;
            gpu.device
                .CreatePlacedResource(&heap, at, &texture(size), D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made)
                .unwrap();
            made.unwrap()
        });
        (heap, textures)
    };
    // Fill them: 17 and 34 in every byte.
    for (texture, (size, byte)) in textures.iter().zip([(64u32, 17u8), (2048, 34)]) {
        let pitch = size * 4;
        let (from, data, _) = gpu.shared_buffer(u64::from(pitch) * u64::from(size));
        // SAFETY: the mapping holds the bytes; an open list, live resources.
        unsafe {
            std::ptr::write_bytes(data, byte, (pitch * size) as usize);
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&from),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT(28),
                            Width: size,
                            Height: size,
                            Depth: 1,
                            RowPitch: pitch,
                        },
                    },
                },
            };
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            gpu.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            gpu.list.ResourceBarrier(&[crate::host::transition(
                texture,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            )]);
        }
        gpu.submit_and_wait();
    }
    let view_heap = |visible: bool| -> ID3D12DescriptorHeap {
        // SAFETY: plain heap creation.
        unsafe {
            gpu.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 4,
                Flags: if visible { D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE } else { D3D12_DESCRIPTOR_HEAP_FLAG_NONE },
                NodeMask: 0,
            })
        }
        .unwrap()
    };
    let (shown, kept) = (view_heap(true), view_heap(false));
    // SAFETY: live heaps.
    let (to, from, step) = unsafe {
        (
            shown.GetCPUDescriptorHandleForHeapStart().ptr,
            kept.GetCPUDescriptorHandleForHeapStart().ptr,
            gpu.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize,
        )
    };
    let at = |base: usize, i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: base + i * step };
    for (i, texture) in textures.iter().enumerate() {
        // SAFETY: slots of our heaps, live textures.
        unsafe {
            gpu.device.CreateShaderResourceView(texture, None, at(from, i));
            gpu.device.CreateShaderResourceView(texture, None, at(to, i));
        }
    }
    let bytes = |base: usize, i: usize| {
        // SAFETY: a heap's handle; what it points at is read if it is memory.
        let mut out = vec![0u8; step];
        crate::host::read_into((base + i * step) as *const u8, &mut out).then_some(out)
    };
    println!("step {step}, the heaps' handles point at {:#x} and {:#x} (shown, kept)", to, from);
    for i in 0..2 {
        println!("  shown {i}: {:02x?}", bytes(to, i));
        println!("  kept  {i}: {:02x?}", bytes(from, i));
    }
    let same = bytes(to, 0).is_some() && bytes(to, 0) == bytes(from, 0) && bytes(to, 1) == bytes(from, 1);
    println!("the shown heap's memory is the descriptors' own bytes: {same}");
    let images = [bytes(from, 0), bytes(from, 1)];
    // SAFETY: a live heap.
    let table = unsafe { shown.GetGPUDescriptorHandleForHeapStart() };
    for way in ["view creation", "copies from a CPU heap", "one 32 byte store"] {
        if way.starts_with("one") && !(same && step == 32) {
            continue;
        }
        let (_out, out_ptr, out_gpu) = gpu.shared_buffer(1 << 12);
        // SAFETY: the mapping.
        unsafe { std::ptr::write_bytes(out_ptr, 0, 1 << 12) };
        let stop = std::sync::atomic::AtomicBool::new(false);
        let writes = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|scope| {
            let device = gpu.device.clone();
            let (stop, writes, textures, images) = (&stop, &writes, &textures, &images);
            scope.spawn(move || {
                let mut k = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for slot in 0..2 {
                        let which = (k + slot) & 1;
                        // SAFETY: slots of our heaps, live textures.
                        unsafe {
                            match way.as_bytes()[0] {
                                b'v' => device.CreateShaderResourceView(&textures[which], None, at(to, slot)),
                                b'c' => device.CopyDescriptorsSimple(
                                    1,
                                    at(to, slot),
                                    at(from, which),
                                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                                ),
                                _ => store32(to + slot * step, images[which].as_deref().unwrap()),
                            }
                        }
                    }
                    k += 1;
                    writes.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
                }
            });
            let started = std::time::Instant::now();
            let mut odd = 0u32;
            while started.elapsed().as_secs() < seconds {
                // SAFETY: our open list, live objects.
                unsafe {
                    gpu.list.SetDescriptorHeaps(&[Some(shown.clone())]);
                    gpu.list.SetComputeRootSignature(&pass.root);
                    gpu.list.SetPipelineState(&pass.pso);
                    gpu.list.SetComputeRootDescriptorTable(1, table);
                    gpu.list.SetComputeRootUnorderedAccessView(2, out_gpu);
                    for _ in 0..8 {
                        gpu.list.SetComputeRoot32BitConstants(0, 1, (&raw const odd).cast(), 0);
                        gpu.list.Dispatch(65_535, 1, 1);
                        odd += 1;
                    }
                }
                gpu.submit_and_wait();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        // SAFETY: the mapping, the card is done.
        let found: Vec<u32> = (0..11).map(|i| unsafe { out_ptr.cast::<u32>().add(i).read_volatile() }).collect();
        println!(
            "{way}: {} writes; groups that saw the small texture {}, the large {}; no size of the two {}, the small size with another texel {}, the large size with another texel {}; the last such: {}x{} texel {}",
            writes.load(std::sync::atomic::Ordering::Relaxed),
            found[0],
            found[1],
            found[2],
            found[3],
            found[4],
            found[8],
            found[9],
            found[10]
        );
        // SAFETY: a plain query.
        println!("  device: {:?}", unsafe { gpu.device.GetDeviceRemovedReason() });
    }
    drop(heap);
}

/// 32 bytes to `to` (32 byte aligned) in one store, then a fence.
///
/// # Safety
/// `to` is 32 bytes of memory that may be written.
unsafe fn store32(to: usize, bytes: &[u8]) {
    #[target_feature(enable = "avx")]
    unsafe fn store(to: usize, bytes: *const u8) {
        use std::arch::x86_64::*;
        // SAFETY: the caller's.
        unsafe {
            _mm256_stream_si256(to as *mut __m256i, _mm256_loadu_si256(bytes.cast()));
            _mm_sfence();
        }
    }
    assert!(std::is_x86_feature_detected!("avx") && bytes.len() == 32 && to.is_multiple_of(32));
    // SAFETY: asked for above; the caller's memory.
    unsafe { store(to, bytes.as_ptr()) }
}

/// What a texture read in another dimension than its view's gives
/// (ignored: a measurement of the card): a view of a small 2D texture
/// read as an array, a volume and a cube, and a small volume read as 2D,
/// far out. The texture lies at the start of a heap whose other megabytes
/// hold other bytes: a texel of those is a read past the texture.
#[test]
#[ignore]
fn textures_read_in_another_dimension() {
    let Some(mut gpu) = WarpHarness::on(std::env::var_os("OATHKEEP_BENCH_WARP").is_some()) else {
        return;
    };
    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=2,b0), DescriptorTable(SRV(t0,numDescriptors=4,flags=DESCRIPTORS_VOLATILE)), UAV(u0), StaticSampler(s0,filter=FILTER_MIN_MAG_MIP_POINT,addressU=TEXTURE_ADDRESS_WRAP,addressV=TEXTURE_ADDRESS_WRAP,addressW=TEXTURE_ADDRESS_WRAP)"
cbuffer C : register(b0) { uint how; uint far; }
Texture2D<float4> flat_ : register(t0);
Texture2DArray<float4> layers : register(t1);
Texture3D<float4> volume : register(t2);
TextureCubeArray<float4> cube : register(t3);
SamplerState s : register(s0);
RWByteAddressBuffer o : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 t : SV_DispatchThreadID) {
uint2 at = uint2(t.x & 7, t.x >> 3);
float4 c = 0;
if (how == 0) c = flat_.Load(int3(at, 0));
if (how == 1) c = layers.Load(int4(at, far, 0));
if (how == 2) c = volume.Load(int4(at, far, 0));
if (how == 3) c = layers.SampleLevel(s, float3((at + 0.5) / 64, far), 0);
if (how == 4) c = volume.SampleLevel(s, float3((at + 0.5) / 64, (far + 0.5) / 4096.0), 0);
if (how == 5) c = cube.SampleLevel(s, float4(1, (at + 0.5) / 64, far), 0);
if (how == 6) c = flat_.Load(int3(at, far));
uint r = (uint)(c.x * 255.0 + 0.5), was;
// the texture's own bytes, zeros, anything else
o.InterlockedAdd(r == 17 ? 0 : r == 0 ? 4 : 8, 1, was);
if (r != 17 && r != 0) o.Store(12, r);
}
"#;
    let pass = compile_cs(&gpu.device, SOURCE).expect("the pass must compile");
    // SAFETY: plain creation, each thing in its own part of the heap.
    let (_heap, small) = unsafe {
        let mut heap: Option<ID3D12Heap> = None;
        gpu.device
            .CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: 256 << 20,
                    Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    Alignment: 0,
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                },
                &mut heap,
            )
            .unwrap();
        let heap = heap.unwrap();
        let place = |at: u64, size: u64| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: size,
                Height: size as u32,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT(28),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            gpu.device
                .CreatePlacedResource(&heap, at, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made)
                .unwrap();
            made.unwrap()
        };
        let small = place(0, 64);
        // The rest: 34 in every byte.
        let mut fill: Vec<(ID3D12Resource, u32, u8)> = vec![(small.clone(), 64, 17)];
        for k in 0..15u64 {
            fill.push((place((1 + k) << 24, 2048), 2048, 34));
        }
        for k in 1..16u64 {
            fill.push((place(k << 20, 512), 512, 34));
        }
        for (texture, size, byte) in fill {
            let pitch = size * 4;
            let (from, data, _) = gpu.shared_buffer(u64::from(pitch) * u64::from(size));
            std::ptr::write_bytes(data, byte, (pitch * size) as usize);
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&from),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT(28),
                            Width: size,
                            Height: size,
                            Depth: 1,
                            RowPitch: pitch,
                        },
                    },
                },
            };
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            gpu.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            gpu.list.ResourceBarrier(&[crate::host::transition(
                &texture,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            )]);
            gpu.submit_and_wait();
        }
        (heap, small)
    };
    // SAFETY: plain heap creation; slots of it, a live texture.
    let heap: ID3D12DescriptorHeap = unsafe {
        let heap: ID3D12DescriptorHeap = gpu
            .device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 4,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .unwrap();
        let step = gpu.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        for i in 0..4 {
            let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + i * step };
            // A 2D view of the small texture in every slot.
            gpu.device.CreateShaderResourceView(&small, None, at);
        }
        heap
    };
    for (how, name) in
        ["2D", "array, loaded", "volume, loaded", "array, sampled", "volume, sampled", "cube array", "2D, a level it has not"]
            .iter()
            .enumerate()
    {
        for far in [0u32, 1, 7, 63, 1000, 4000] {
            let (_out, out_ptr, out_gpu) = gpu.shared_buffer(1 << 12);
            // SAFETY: the mapping; our open list, live objects.
            unsafe {
                std::ptr::write_bytes(out_ptr, 0, 1 << 12);
                gpu.list.SetDescriptorHeaps(&[Some(heap.clone())]);
                gpu.list.SetComputeRootSignature(&pass.root);
                gpu.list.SetPipelineState(&pass.pso);
                gpu.list.SetComputeRoot32BitConstants(0, 2, [how as u32, far].as_ptr().cast(), 0);
                gpu.list.SetComputeRootDescriptorTable(1, heap.GetGPUDescriptorHandleForHeapStart());
                gpu.list.SetComputeRootUnorderedAccessView(2, out_gpu);
                gpu.list.Dispatch(1, 1, 1);
            }
            gpu.submit_and_wait();
            // SAFETY: the mapping, the card is done.
            let found: Vec<u32> = (0..4).map(|i| unsafe { out_ptr.cast::<u32>().add(i).read_volatile() }).collect();
            println!("{name} at {far}: its own texels {}, zeros {}, others {} (the last {})", found[0], found[1], found[2], found[3]);
        }
    }
    // SAFETY: a plain query.
    println!("device: {:?}", unsafe { gpu.device.GetDeviceRemovedReason() });
}

