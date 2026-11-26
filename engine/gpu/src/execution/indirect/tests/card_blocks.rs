use super::super::*;
use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, s};


/// Loads far outside their views (ignored: it may lose the device, and
/// says which way did): raw, typed, constant and texture loads at the
/// ends of 32 bits, uniform and per lane, and samples at coordinates,
/// levels and gradients that are no numbers. Each way is a submission of
/// its own.
#[test]
#[ignore]
fn loads_far_outside_views() {
    let Some(mut gpu) = WarpHarness::on(std::env::var_os("OATHKEEP_BENCH_WARP").is_some()) else {
        return;
    };
    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4,b0), DescriptorTable(SRV(t0,numDescriptors=4),CBV(b1)), UAV(u0), StaticSampler(s0,filter=FILTER_MIN_MAG_MIP_LINEAR,addressU=TEXTURE_ADDRESS_WRAP,addressV=TEXTURE_ADDRESS_WRAP)"
cbuffer C : register(b0) { uint how; uint far; uint lane; uint out_at; }
ByteAddressBuffer raw : register(t0);
Buffer<float4> typed : register(t1);
Texture2D<float4> tex : register(t2);
struct Block { uint4 d[4096]; };
ConstantBuffer<Block> block : register(b1);
SamplerState s : register(s0);
RWByteAddressBuffer o : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 t : SV_DispatchThreadID) {
uint at = far + t.x * lane * 4;
float4 c = 0;
if (how == 0) c = asfloat(raw.Load4(at));
if (how == 1) c = asfloat(raw.Load(at + 4092));
if (how == 2) c = typed.Load(at);
if (how == 3) c = asfloat(block.d[at]);
if (how == 4) c = tex.Load(int3(at, 0, 0));
if (how == 5) c = tex.Load(int3(0, 0, at));
if (how == 6) c = tex.SampleLevel(s, asfloat(uint2(at, at)), 0);
if (how == 7) c = tex.SampleLevel(s, float2(0.5, 0.5), asfloat(at));
if (how == 8) c = tex.SampleGrad(s, float2(0.5, 0.5), asfloat(at).xx, asfloat(at).xx);
uint was;
o.InterlockedAdd(any(asuint(c) != 0) ? 4 : 0, 1, was);
}
"#;
    let pass = compile_cs(&gpu.device, SOURCE).expect("the pass must compile");
    let (buffer, data, gpu_at) = gpu.shared_buffer(1 << 16);
    // SAFETY: the mapping.
    unsafe { std::ptr::write_bytes(data, 0x3f, 1 << 16) };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 64,
        Height: 64,
        DepthOrArraySize: 1,
        MipLevels: 7,
        Format: DXGI_FORMAT(28),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    // SAFETY: plain creation; slots of our heap, live objects.
    let (_texture, heap) = unsafe {
        let mut made: Option<ID3D12Resource> = None;
        gpu.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                None,
                &mut made,
            )
            .unwrap();
        let texture = made.unwrap();
        let heap: ID3D12DescriptorHeap = gpu
            .device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 5,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .unwrap();
        let step = gpu.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        let at = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + i * step };
        for (i, format, count, flags) in
            [(0, 39, 1024, D3D12_BUFFER_SRV_FLAG_RAW), (1, 2, 256, D3D12_BUFFER_SRV_FLAG_NONE)]
        {
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(format),
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV { FirstElement: 0, NumElements: count, StructureByteStride: 0, Flags: flags },
                },
            };
            gpu.device.CreateShaderResourceView(&buffer, Some(&view), at(i));
        }
        gpu.device.CreateShaderResourceView(&texture, None, at(2));
        gpu.device.CreateShaderResourceView(&texture, None, at(3));
        gpu.device.CreateConstantBufferView(
            Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: gpu_at, SizeInBytes: 4096 }),
            at(4),
        );
        (texture, heap)
    };
    let names = [
        "raw, 4 dwords",
        "raw, offset added",
        "typed",
        "constant buffer",
        "texel",
        "level of a texel",
        "sample",
        "level of a sample",
        "gradients of a sample",
    ];
    for (how, name) in names.iter().enumerate() {
        for far in [
            0u32,
            0x1000,
            0x7fff_fff0,
            0x8000_0000,
            0xffff_f000,
            0xffff_fff0,
            0xffff_fffc,
            0x7fc0_0000,
            0x7f80_0000,
            0xff80_0000,
            0x7f7f_ffff,
        ] {
            for lane in [0u32, 1] {
                let (_out, out_ptr, out_gpu) = gpu.shared_buffer(1 << 12);
                // SAFETY: the mapping; our open list, live objects.
                unsafe {
                    std::ptr::write_bytes(out_ptr, 0, 1 << 12);
                    gpu.list.SetDescriptorHeaps(&[Some(heap.clone())]);
                    gpu.list.SetComputeRootSignature(&pass.root);
                    gpu.list.SetPipelineState(&pass.pso);
                    gpu.list.SetComputeRoot32BitConstants(0, 4, [how as u32, far, lane, 0].as_ptr().cast(), 0);
                    gpu.list.SetComputeRootDescriptorTable(1, heap.GetGPUDescriptorHandleForHeapStart());
                    gpu.list.SetComputeRootUnorderedAccessView(2, out_gpu);
                    gpu.list.Dispatch(1, 1, 1);
                    // (Not `submit_and_wait`, which expects to go on.)
                    gpu.list.Close().unwrap();
                    gpu.submitted += 1;
                    let cmd: ID3D12CommandList = gpu.list.cast().unwrap();
                    gpu.queue.ExecuteCommandLists(&[Some(cmd)]);
                    let _ = gpu.queue.Signal(&gpu.fence, gpu.submitted);
                    let _ = gpu.fence.SetEventOnCompletion(gpu.submitted, gpu.event);
                    WaitForSingleObject(gpu.event, 8_000);
                }
                // SAFETY: the mapping; plain queries.
                let (zeros, others, device) = unsafe {
                    (
                        out_ptr.cast::<u32>().read_volatile(),
                        out_ptr.cast::<u32>().add(1).read_volatile(),
                        gpu.device.GetDeviceRemovedReason(),
                    )
                };
                println!(
                    "{name} at {far:#x}, lanes {lane}: zeros {zeros}, others {others}{}",
                    if device.is_ok() { "" } else { " DEVICE LOST" }
                );
                if device.is_err() {
                    return;
                }
                // SAFETY: the card is done with them.
                unsafe {
                    gpu.allocator.Reset().unwrap();
                    gpu.list.Reset(&gpu.allocator, None).unwrap();
                }
            }
        }
    }
}

/// Whether lists on the card may read a texture of a block made after
/// they were sent, through a slot they read by index (ignored: a
/// measurement that loses the device, so each way runs in a process of
/// its own; `OATHKEEP_BENCH_S` seconds a way, 30; `OATHKEEP_BENCH_N` runs of
/// each, 3). Threads (`OATHKEEP_BENCH_THREADS`, 4) make blocks of 64 MiB, a texture in each and its
/// view, and copies the view into a slot; long dispatches that read all
/// the slots are always on the card.
pub(super) fn new_blocks_under_readers() {
    if let Ok(way) = std::env::var("OATHKEEP_BLOCK_WAY") {
        return new_blocks(&way);
    }
    let times: u32 = std::env::var("OATHKEEP_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let seconds = std::env::var("OATHKEEP_BENCH_S").unwrap_or_else(|_| "30".into());
    // "never": never resident, what must lose the device. "fresh-": no
    // block goes (280 of them, 18 GB), so each has addresses no block had.
    for way in ["fresh-made", "fresh-waited", "fresh-enqueued", "made", "waited", "enqueued", "never"] {
        let fresh = way.starts_with("fresh-");
        for n in 0..if way == "never" { 1 } else if fresh { times * 5 } else { times } {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["indirect::tests::new_blocks_under_readers", "--exact", "--ignored", "--nocapture"])
                .env("OATHKEEP_BLOCK_WAY", way)
                .env("OATHKEEP_BENCH_S", if fresh { "3" } else { &seconds })
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let said = |what: &str| text.lines().find(|l| l.contains(what)).unwrap_or("(nothing said)").trim().to_owned();
            println!("{way}, run {n}: {}; {} (exit {:?})", said("dispatches a"), said("blocks,"), out.status.code());
        }
    }
}

fn new_blocks(way: &str) {
    let Some(gpu) = WarpHarness::on(false) else {
        println!("no card: 0 blocks,");
        return;
    };
    let (way, fresh) = match way.strip_prefix("fresh-") {
        Some(way) => (way, true),
        None => (way, false),
    };
    const SLOTS: usize = 64;
    const BLOCK: u64 = 64 << 20;
    let seconds = std::env::var("OATHKEEP_BENCH_S").ok().and_then(|v| v.parse().ok()).unwrap_or(30u64);
    const SOURCE: &str = r#"
#define RS "DescriptorTable(SRV(t0,numDescriptors=64,flags=DESCRIPTORS_VOLATILE|DATA_VOLATILE)), UAV(u0)"
Texture2D<float4> t[64] : register(t0);
RWByteAddressBuffer o : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 g : SV_GroupID, uint i : SV_GroupIndex) {
uint which = g.x & 63;
float4 c = 0;
for (uint k = 0; k < 8; k++)
    c += t[(which + k * 8) & 63].Load(int3((i + k * 13) & 127, (g.y + k * 29) & 255, 0));
if (c.x < -1) o.Store(0, 1);
}
"#;
    let pass = compile_cs(&gpu.device, SOURCE).expect("the pass must compile");
    // SAFETY: plain heap creation.
    let heap = |visible: bool, n: u32| -> ID3D12DescriptorHeap {
        unsafe {
            gpu.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: n,
                Flags: if visible { D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE } else { D3D12_DESCRIPTOR_HEAP_FLAG_NONE },
                NodeMask: 0,
            })
        }
        .unwrap()
    };
    let (shown, beside) = (heap(true, SLOTS as u32), heap(false, 65));
    // SAFETY: live heaps.
    let (to, from, step) = unsafe {
        (
            shown.GetCPUDescriptorHandleForHeapStart().ptr,
            beside.GetCPUDescriptorHandleForHeapStart().ptr,
            gpu.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize,
        )
    };
    let null = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT(28),
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() },
        },
    };
    let slot = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: to + i * step };
    let own = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: from + i * step };
    // SAFETY: slots of our heaps.
    unsafe {
        gpu.device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&null), own(0));
        for i in 0..SLOTS {
            gpu.device.CopyDescriptorsSimple(1, slot(i), own(0), D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        }
    }
    let (_out, _, out_gpu) = gpu.shared_buffer(1 << 12);
    // SAFETY: a live heap.
    let table = unsafe { shown.GetGPUDescriptorHandleForHeapStart() };
    let list = |dispatches: u32| -> (ID3D12CommandAllocator, ID3D12GraphicsCommandList) {
        // SAFETY: plain creation and recording.
        unsafe {
            let allocator: ID3D12CommandAllocator =
                gpu.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                gpu.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            list.SetDescriptorHeaps(&[Some(shown.clone())]);
            list.SetComputeRootSignature(&pass.root);
            list.SetPipelineState(&pass.pso);
            list.SetComputeRootDescriptorTable(0, table);
            list.SetComputeRootUnorderedAccessView(1, out_gpu);
            for _ in 0..dispatches {
                list.Dispatch(1024, 64, 1);
            }
            list.Close().unwrap();
            (allocator, list)
        }
    };
    // Lists of some 5 ms, always five sent: some run while a slot changes.
    let mut sent = 0u64;
    let mut dispatches = 8;
    let ms = loop {
        let one = list(dispatches);
        let began = std::time::Instant::now();
        // SAFETY: a closed list of ours; waited for.
        unsafe {
            gpu.queue.ExecuteCommandLists(&[Some(one.1.cast().unwrap())]);
            sent += 1;
            gpu.queue.Signal(&gpu.fence, sent).unwrap();
            let _ = gpu.fence.SetEventOnCompletion(sent, gpu.event);
            WaitForSingleObject(gpu.event, 10_000);
        }
        let ms = began.elapsed().as_secs_f64() * 1e3;
        if ms >= 4.0 || dispatches >= 1 << 14 {
            break ms;
        }
        dispatches *= 2;
    };
    println!("{dispatches} dispatches a list, {ms:.1} ms each");
    let lists: Vec<_> = (0..6).map(|_| list(dispatches)).collect();
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
    let sent = AtomicU64::new(sent);
    let stop = AtomicBool::new(false);
    // Blocks; microseconds making a block, the longest; waiting, the longest.
    let counts: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    let threads: usize = std::env::var("OATHKEEP_BENCH_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let started = std::time::Instant::now();
    let lost = std::thread::scope(|scope| {
        for thread in 0..threads {
            let (device, fence) = (gpu.device.clone(), gpu.fence.clone());
            let (sent, stop, counts) = (&sent, &stop, &counts);
            scope.spawn(move || {
                // Slots of its own, and a staged view of its own.
                let mine = SLOTS / threads;
                let slot = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: to + (thread * mine + i) * step };
                let own = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: from + i * step };
                let wait = way != "made";
                let mut alive: Vec<(ID3D12Heap, ID3D12Resource)> = Vec::new();
                let mut n = 0u64;
                while !stop.load(SeqCst) {
                    // SAFETY: plain creation; slots of our heaps, this thread's.
                    unsafe {
                        let mut block: Option<ID3D12Heap> = None;
                        let flags = D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES
                            | if wait { D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT } else { D3D12_HEAP_FLAG_NONE };
                        let heap = D3D12_HEAP_DESC {
                            SizeInBytes: BLOCK,
                            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                            Alignment: 0,
                            Flags: flags,
                        };
                        let began = std::time::Instant::now();
                        if device.CreateHeap(&heap, &mut block).is_err() {
                            break;
                        }
                        let us = began.elapsed().as_micros() as u64;
                        counts[1].fetch_add(us, SeqCst);
                        counts[2].fetch_max(us, SeqCst);
                        let block = block.unwrap();
                        let pageable: ID3D12Pageable = block.cast().unwrap();
                        let began = std::time::Instant::now();
                        if way == "waited" && device.MakeResident(&[Some(pageable.clone())]).is_err() {
                            break;
                        }
                        if way == "enqueued" {
                            let device: ID3D12Device3 = device.cast().unwrap();
                            let paged: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
                            let asked =
                                device.EnqueueMakeResident(D3D12_RESIDENCY_FLAG_NONE, &[Some(pageable)], &paged, 1);
                            if asked.is_err() {
                                break;
                            }
                            while paged.GetCompletedValue() < 1 {
                                std::hint::spin_loop();
                            }
                        }
                        let us = began.elapsed().as_micros() as u64;
                        counts[3].fetch_add(us, SeqCst);
                        counts[4].fetch_max(us, SeqCst);
                        let desc = D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                            Width: 128,
                            Height: 256,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT(28),
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            ..Default::default()
                        };
                        let mut texture: Option<ID3D12Resource> = None;
                        // Somewhere in the block, as the pool's pieces are.
                        let at = (n * 7 % 900) << 16;
                        if device
                            .CreatePlacedResource(&block, at, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture)
                            .is_err()
                        {
                            break;
                        }
                        let texture = texture.unwrap();
                        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
                        device.CreateShaderResourceView(&texture, None, own(1 + thread));
                        device.CopyDescriptorsSimple(1, slot(n as usize % mine), own(1 + thread), kind);
                        alive.push((block, texture));
                        n += 1;
                        counts[0].fetch_add(1, SeqCst);
                        if fresh {
                            if alive.len() >= 280 / threads {
                                break;
                            }
                        } else if alive.len() >= 24 {
                            // Nothing shows or reads them any more when they go.
                            for i in 0..mine {
                                device.CopyDescriptorsSimple(1, slot(i), own(0), kind);
                            }
                            let until = sent.load(SeqCst) + 1;
                            while fence.GetCompletedValue() < until && !stop.load(SeqCst) {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            if fence.GetCompletedValue() < until {
                                break;
                            }
                            alive.clear();
                        }
                    }
                }
                // The lists sent last may read them: the process goes next.
                std::mem::forget(alive);
            });
        }
        let lost = loop {
            // SAFETY: closed lists of ours, each run again only when it has run.
            unsafe {
                let done = gpu.fence.GetCompletedValue();
                if done == u64::MAX || gpu.device.GetDeviceRemovedReason().is_err() {
                    break true;
                }
                while sent.load(SeqCst) < done + 5 {
                    let n = sent.load(SeqCst);
                    let list: ID3D12CommandList = lists[(n % 6) as usize].1.cast().unwrap();
                    gpu.queue.ExecuteCommandLists(&[Some(list)]);
                    let _ = gpu.queue.Signal(&gpu.fence, n + 1);
                    sent.store(n + 1, SeqCst);
                }
            }
            if started.elapsed().as_secs() >= seconds {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_micros(300));
        };
        stop.store(true, SeqCst);
        lost
    });
    let sent = sent.into_inner();
    let counts = counts.map(|c| c.into_inner());
    let blocks = counts[0];
    // SAFETY: a plain query.
    let reason = unsafe { gpu.device.GetDeviceRemovedReason() };
    println!(
        "{blocks} blocks, {sent} lists in {:.1} s; making {:.0} us a block, the longest {}; waiting {:.0} us, the longest {}; {}",
        started.elapsed().as_secs_f64(),
        counts[1] as f64 / blocks.max(1) as f64,
        counts[2],
        counts[3] as f64 / blocks.max(1) as f64,
        counts[4],
        if lost { format!("LOST: {reason:?}") } else { "the device lives".to_owned() }
    );
    if !lost {
        // SAFETY: waits for what was sent.
        unsafe {
            let _ = gpu.fence.SetEventOnCompletion(sent, gpu.event);
            WaitForSingleObject(gpu.event, 10_000);
        }
    }
}

