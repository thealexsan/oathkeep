use super::super::*;
use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, s};


/// The compare of copies in the card's memory (`crate::backend`), on the
/// software adapter. Told 1 it flags the pieces whose bytes are not
/// what was hashed last and leaves the hashes: a piece a shader changed
/// and a later one changed back then looks untouched, which is right
/// only if the piece was fetched after both. Told 2 (submissions sent
/// ahead with the relay, whose pieces are fetched flight by flight) it
/// stores the hash it made: each compare flags what changed since the
/// compare before it, also a change back.
#[test]
fn warp_card_compare_stores_the_hash_it_made_when_told() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let compare = compile_cs(&warp.device, crate::backend::CARD_COMPARE).expect("the card compare must compile on WARP");
    const PIECE: usize = 64 << 10;
    const PIECES: u32 = 3;
    let size = PIECE * PIECES as usize;
    let (_now, now_ptr, now_gpu) = warp.shared_buffer(size as u64);
    let (_hashes, _, hashes_gpu) = warp.shared_buffer(16 * u64::from(PIECES));
    let (_differs, differs_ptr, differs_gpu) = warp.shared_buffer(64);
    let content: Vec<u8> = (0..size).map(|i| (i * 31 + i / 251) as u8).collect();
    // SAFETY: the mapping holds `size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(content.as_ptr(), now_ptr, size) };
    // One pass over the three pieces; what it left in `differs` (7s
    // where it stored nothing).
    let run = |warp: &mut WarpHarness, mode: u32| -> [u32; 3] {
        // SAFETY: our open list, live buffers, the compare's own root
        // signature; the mappings hold what is read and written.
        unsafe {
            for piece in 0..PIECES as usize {
                differs_ptr.cast::<u32>().add(piece).write_volatile(7);
            }
            warp.list.SetComputeRootSignature(&compare.root);
            warp.list.SetPipelineState(&compare.pso);
            let constants = [0u32, 0, size as u32, PIECES, 0, mode];
            warp.list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            warp.list.SetComputeRootShaderResourceView(1, now_gpu);
            warp.list.SetComputeRootUnorderedAccessView(2, hashes_gpu);
            warp.list.SetComputeRootUnorderedAccessView(3, differs_gpu);
            warp.list.Dispatch(PIECES, 1, 1);
        }
        warp.submit_and_wait();
        // SAFETY: as above.
        let found = [0, 1, 2].map(|piece| unsafe { differs_ptr.cast::<u32>().add(piece).read_volatile() });
        if mode == 0 {
            return found;
        }
        // Above the flag, the hash: as `host::torn` makes it of the bytes.
        for (piece, d) in found.iter().enumerate() {
            // SAFETY: inside the mapping, which the card has read.
            let bytes = unsafe { std::slice::from_raw_parts(now_ptr.add(piece * PIECE).cast_const(), PIECE) };
            assert_eq!(d >> 1, crate::backend::piece_hash(bytes), "piece {piece}");
        }
        found.map(|d| d & 1)
    };
    let at = PIECE + 1234;
    // SAFETY (each): inside the mapping.
    let set = |value: u8| unsafe { now_ptr.add(at).write_volatile(value) };
    // 0: the hashes are stored, nothing is flagged.
    assert_eq!(run(&mut warp, 0), [7, 7, 7]);
    assert_eq!(run(&mut warp, 1), [0, 0, 0]);
    // A change in the second piece: flagged, and with 1 flagged again
    // (the hash stays the old one).
    set(content[at] ^ 0x40);
    assert_eq!(run(&mut warp, 1), [0, 1, 0]);
    assert_eq!(run(&mut warp, 1), [0, 1, 0]);
    // Changed back: with 1 the piece looks untouched.
    set(content[at]);
    assert_eq!(run(&mut warp, 1), [0, 0, 0]);
    // With 2 the change is flagged once, and the hash is the new one.
    set(content[at] ^ 0x40);
    assert_eq!(run(&mut warp, 2), [0, 1, 0]);
    assert_eq!(run(&mut warp, 2), [0, 0, 0]);
    // Changed back: flagged, where 1 saw nothing.
    set(content[at]);
    assert_eq!(run(&mut warp, 2), [0, 1, 0]);
    assert_eq!(run(&mut warp, 2), [0, 0, 0]);
}

/// What the passes over copies in the card's memory cost on the card
/// (ignored: a measurement): rounds of a few passes of some pieces and a
/// wait, as a frame has them. `OATHKEEP_BENCH_CS=<file>` times another
/// source of the shader.
#[test]
#[ignore]
fn card_compare_costs() {
    let Some(mut gpu) = WarpHarness::on(false) else {
        return;
    };
    let other = std::env::var("OATHKEEP_BENCH_CS").ok().map(|f| std::fs::read_to_string(f).unwrap());
    let source: &'static str = other.map_or(crate::backend::CARD_COMPARE, |s| Box::leak(s.into_boxed_str()));
    let walked_alone = source.contains("SV_DispatchThreadID");
    let compare = compile_cs(&gpu.device, source).expect("the compare must compile");
    let now = gpu.default_uav(256 << 20);
    let hashes = gpu.default_uav(64 << 10);
    let differs = gpu.default_uav(64 << 10);
    for (rounds, passes, pieces) in [(80u32, 5u32, 1u32), (80, 5, 5), (80, 5, 64), (80, 5, 500), (1, 1, 4000)] {
        let mut best = f64::MAX;
        for _ in 0..6 {
            // SAFETY: our open list, live buffers, the compare's own
            // root signature; the pieces lie in the buffers.
            unsafe {
                gpu.list.SetComputeRootSignature(&compare.root);
                gpu.list.SetPipelineState(&compare.pso);
                gpu.list.SetComputeRootShaderResourceView(1, now.GetGPUVirtualAddress());
                gpu.list.SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
                gpu.list.SetComputeRootUnorderedAccessView(3, differs.GetGPUVirtualAddress());
                for _ in 0..rounds {
                    for pass in 0..passes {
                        let first = if pieces * passes < 4096 { pass * pieces } else { 0 };
                        let constants = [first, 0u32, 256 << 20, pieces, 0, 1];
                        gpu.list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                        gpu.list.Dispatch(if walked_alone { pieces.div_ceil(64) } else { pieces }, 1, 1);
                    }
                    gpu.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(None),
                            }),
                        },
                        ..Default::default()
                    }]);
                }
            }
            let t = std::time::Instant::now();
            gpu.submit_and_wait();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        let mb = f64::from(rounds * passes * pieces) / 16.0;
        println!("{rounds} rounds of {passes} passes of {pieces} pieces: {best:.2} ms ({mb:.0} MB, {:.0} GB/s)", mb / best / 1.024);
    }
}

/// What raw loads through views that are not raw give past the view's
/// end (ignored: a measurement of the card). A buffer view of 4 KiB in a
/// buffer of 1 MiB whose every dword says where it lies: zeros past the
/// view, or the bytes there (whatever the card has in that place).
#[test]
#[ignore]
fn raw_loads_past_views() {
    let Some(mut gpu) = WarpHarness::on(std::env::var_os("OATHKEEP_BENCH_WARP").is_some()) else {
        return;
    };
    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4,b0), DescriptorTable(SRV(t0,numDescriptors=8)), UAV(u0), DescriptorTable(CBV(b1,numDescriptors=8))"
cbuffer C : register(b0) { uint which; uint first; uint step; uint at; }
ByteAddressBuffer v[8] : register(t0);
struct Block { uint4 d[4096]; };
ConstantBuffer<Block> c[8] : register(b1);
RWByteAddressBuffer o : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID, uint g : SV_GroupIndex) {
if (step == 0) { if (id.x == 0) o.Store(at, v[which].Load(first)); }
else if (step == 1) { if (id.x == 0) o.Store(at, c[which].d[first / 16][(first / 4) & 3]); }
else if (step == 16) { o.Store(at + id.x * 4, c[which].d[first / 16 + id.x][(first / 4) & 3]); }
else { o.Store(at + id.x * 4, v[which].Load(first + id.x * step)); }
}
"#;
    let pass = compile_cs(&gpu.device, SOURCE).expect("the pass must compile");
    let (buffer, data, _) = gpu.shared_buffer(1 << 20);
    for i in 0..(1usize << 18) {
        // SAFETY: inside the mapping.
        unsafe { data.cast::<u32>().add(i).write(0xA000_0000 | i as u32) };
    }
    let (_out, out_ptr, out_gpu) = gpu.shared_buffer(1 << 16);
    // SAFETY: plain heap creation, views of a live buffer.
    let heap: ID3D12DescriptorHeap = unsafe {
        gpu.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: 16,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })
    }
    .unwrap();
    // SAFETY: as above.
    let (cpu, table, step) = unsafe {
        (
            heap.GetCPUDescriptorHandleForHeapStart().ptr,
            heap.GetGPUDescriptorHandleForHeapStart(),
            gpu.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize,
        )
    };
    let names = ["raw", "structured 24 at 24", "structured 16", "typed u32", "typed u16", "typed 4 x u32", "constant buffer", "raw, moved to 8"];
    let view = |i: usize, format: i32, first: u64, count: u32, stride: u32, raw: bool, null: bool| {
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT(format),
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: first,
                    NumElements: count,
                    StructureByteStride: stride,
                    Flags: if raw { D3D12_BUFFER_SRV_FLAG_RAW } else { D3D12_BUFFER_SRV_FLAG_NONE },
                },
            },
        };
        let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu + i * step };
        // SAFETY: a slot of our heap, a live buffer.
        unsafe { gpu.device.CreateShaderResourceView(if null { None } else { Some(&buffer) }, Some(&desc), at) };
    };
    for base in [0, 8] {
        view(base, 39, 0, 1024, 0, true, false);
        view(base + 1, 0, 1, 170, 24, false, false);
        view(base + 2, 0, 0, 256, 16, false, false);
        view(base + 3, 42, 0, 1024, 0, false, false);
        view(base + 4, 57, 0, 2048, 0, false, false);
        view(base + 5, 3, 0, 256, 0, false, false);
        // 1,024 dwords from 0 made in a heap the card does not see, moved
        // on by 8 bytes (`descriptor::host::move_raw`), copied in.
        // SAFETY: plain heap creation, slots of live heaps.
        unsafe {
            let beside: ID3D12DescriptorHeap = gpu
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .unwrap();
            let at = beside.GetCPUDescriptorHandleForHeapStart();
            let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(39),
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: 0,
                        NumElements: 1024,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            gpu.device.CreateShaderResourceView(&buffer, Some(&desc), at);
            let moved = crate::descriptors::host_heap::move_raw(at, buffer.GetGPUVirtualAddress(), 4096, 8);
            println!("the raw view was moved: {moved}");
            gpu.device.CopyDescriptorsSimple(
                1,
                D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu + (base + 7) * step },
                at,
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            );
        }
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            // SAFETY: a live buffer.
            BufferLocation: unsafe { buffer.GetGPUVirtualAddress() },
            SizeInBytes: 4096,
        };
        // SAFETY: a slot of our heap.
        unsafe { gpu.device.CreateConstantBufferView(Some(&desc), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu + (base + 6) * step }) };
    }
    let table2 = D3D12_GPU_DESCRIPTOR_HANDLE { ptr: table.ptr + 8 * step as u64 };
    let offsets = [0u32, 4092, 4096, 4104, 8192, 65536, 524_288, (1 << 20) - 4];
    for mode in ["one load (uniform)", "a load a thread", "constant buffer load", "Constant buffer load a thread"] {
        println!("{mode}:");
        for (which, name) in names.iter().enumerate() {
            // SAFETY: our open list, live buffers.
            unsafe {
                std::ptr::write_bytes(out_ptr, 0x77, 1 << 16);
                gpu.list.SetDescriptorHeaps(&[Some(heap.clone())]);
                gpu.list.SetComputeRootSignature(&pass.root);
                gpu.list.SetPipelineState(&pass.pso);
                gpu.list.SetComputeRootDescriptorTable(1, table);
                gpu.list.SetComputeRootUnorderedAccessView(2, out_gpu);
                gpu.list.SetComputeRootDescriptorTable(3, table2);
                for (k, &offset) in offsets.iter().enumerate() {
                    let step = match mode.as_bytes()[0] {
                        b'o' => 0u32,
                        b'c' => 1,
                        b'C' => 16,
                        _ => 4,
                    };
                    // (A thread's loads end at the offset.)
                    let first = if step == 4 { offset.saturating_sub(63 * 4) } else { offset };
                    let constants = [which as u32, first, step, k as u32 * 256];
                    gpu.list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
                    gpu.list.Dispatch(1, 1, 1);
                }
            }
            gpu.submit_and_wait();
            let found: Vec<String> = offsets
                .iter()
                .enumerate()
                .map(|(k, &offset)| {
                    let thread = if mode.as_bytes()[0] == b'a' { (offset.min(63 * 4) / 4) as usize } else { 0 };
                    // SAFETY: inside the mapping.
                    let word = unsafe { out_ptr.cast::<u32>().add(k * 64 + thread).read_volatile() };
                    format!("{offset}: {word:#x}")
                })
                .collect();
            println!("  {name:20} {}", found.join("  "));
        }
    }
    // SAFETY: a plain query.
    println!("device: {:?}", unsafe { gpu.device.GetDeviceRemovedReason() });
}

