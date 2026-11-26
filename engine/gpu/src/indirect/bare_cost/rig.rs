use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, D3D_PRIMITIVE_TOPOLOGY};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, s};

use crate::host::{compile, transition};


use super::{Between, DRAWS, DRAW, HEIGHT, WIDTH, WORK};

pub(super) struct Rig {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    submitted: u64,
    event: HANDLE,
    root: ID3D12RootSignature,
    pipelines: [ID3D12PipelineState; 2],
    work_root: ID3D12RootSignature,
    work: ID3D12PipelineState,
    words: ID3D12Resource,
    other: ID3D12Resource,
    depth: ID3D12Resource,
    /// Kept alive: the views below name them.
    _kept: Vec<ID3D12Resource>,
    _target_heaps: [ID3D12DescriptorHeap; 2],
    heap_pairs: [[Option<ID3D12DescriptorHeap>; 2]; 2],
    tables: [D3D12_GPU_DESCRIPTOR_HANDLE; 2],
    targets: [D3D12_CPU_DESCRIPTOR_HANDLE; 2],
    depth_written: D3D12_CPU_DESCRIPTOR_HANDLE,
    depth_tested: D3D12_CPU_DESCRIPTOR_HANDLE,
    stamps: ID3D12QueryHeap,
    back: ID3D12Resource,
    pub(super) card: String,
}

fn texture(
    device: &ID3D12Device,
    (width, height): (u32, u32),
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(width),
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Flags: flags,
        ..Default::default()
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: plain texture creation.
    unsafe {
        device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
            .unwrap();
    }
    out.unwrap()
}

fn buffer(
    device: &ID3D12Device,
    size: u64,
    kind: D3D12_HEAP_TYPE,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: kind,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: plain buffer creation.
    unsafe {
        device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
            .unwrap();
    }
    out.unwrap()
}

fn heap(device: &ID3D12Device, kind: D3D12_DESCRIPTOR_HEAP_TYPE, count: u32, visible: bool) -> ID3D12DescriptorHeap {
    let flags = if visible {
        D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE
    } else {
        D3D12_DESCRIPTOR_HEAP_FLAG_NONE
    };
    // SAFETY: plain heap creation.
    unsafe {
        device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: count,
                Flags: flags,
                NodeMask: 0,
            })
            .unwrap()
    }
}

fn pipeline(device: &ID3D12Device, root: &ID3D12RootSignature, vs: &[u8], ps: &[u8]) -> ID3D12PipelineState {
    let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    };
    let mut blend = D3D12_BLEND_DESC::default();
    blend.RenderTarget[0].RenderTargetWriteMask = 0xf;
    let mut formats = [DXGI_FORMAT(0); 8];
    formats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    // SAFETY: the description points at live data and never releases
    // its pointer to the root signature.
    unsafe {
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(root),
            VS: code(vs),
            PS: code(ps),
            BlendState: blend,
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            // Depth is tested and never written, so the texture may be
            // in the write state or in the read state under one view.
            DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: formats,
            DSVFormat: DXGI_FORMAT_D32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        device.CreateGraphicsPipelineState(&desc).unwrap()
    }
}

impl Rig {
    /// The default adapter, as the layer takes it; `None` without one
    /// or on a software adapter.
    pub(super) fn new() -> Option<Self> {
        let vs = compile(DRAW, s!("corner"), s!("vs_5_1")).ok()?;
        let ps = [
            compile(DRAW, s!("first"), s!("ps_5_1")).ok()?,
            compile(DRAW, s!("second"), s!("ps_5_1")).ok()?,
        ];
        let cs = compile(WORK, s!("work"), s!("cs_5_1")).ok()?;
        // SAFETY: plain object creation on a device of our own; the
        // views are made in heaps that live as long as the rig.
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            let device = device?;
            let desc = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
                .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()))
                .ok()?
                .cast::<IDXGIAdapter1>()
                .ok()?
                .GetDesc1()
                .ok()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                return None;
            }
            let card = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_owned();
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .ok()?;
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;

            let root: ID3D12RootSignature = device.CreateRootSignature(0, &vs[..]).ok()?;
            let pipelines = [
                pipeline(&device, &root, &vs, &ps[0]),
                pipeline(&device, &root, &vs, &ps[1]),
            ];
            let work_root: ID3D12RootSignature = device.CreateRootSignature(0, &cs[..]).ok()?;
            let work: ID3D12PipelineState = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&work_root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: cs.as_ptr().cast(),
                        BytecodeLength: cs.len(),
                    },
                    ..Default::default()
                })
                .ok()?;

            let size = (WIDTH, HEIGHT);
            let colour = |state| texture(&device, size, DXGI_FORMAT_R8G8B8A8_UNORM, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, state);
            let target = [
                colour(D3D12_RESOURCE_STATE_RENDER_TARGET),
                colour(D3D12_RESOURCE_STATE_RENDER_TARGET),
            ];
            let other = colour(D3D12_RESOURCE_STATE_RENDER_TARGET);
            let depth = texture(
                &device,
                size,
                DXGI_FORMAT_D32_FLOAT,
                D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
            );
            let image = texture(
                &device,
                (4, 4),
                DXGI_FORMAT_R8G8B8A8_UNORM,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            );
            let words = buffer(
                &device,
                64 << 10,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            let back = buffer(
                &device,
                16,
                D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            let mut stamps: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut stamps,
                )
                .ok()?;
            let stamps = stamps?;

            // Target and depth views.
            let rtvs = heap(&device, D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 2, false);
            let rtv_step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
            let mut targets = [rtvs.GetCPUDescriptorHandleForHeapStart(); 2];
            targets[1].ptr += rtv_step;
            device.CreateRenderTargetView(&target[0], None, targets[0]);
            device.CreateRenderTargetView(&target[1], None, targets[1]);
            let dsvs = heap(&device, D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 2, false);
            let dsv_step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV) as usize;
            let depth_written = dsvs.GetCPUDescriptorHandleForHeapStart();
            let mut depth_tested = depth_written;
            depth_tested.ptr += dsv_step;
            device.CreateDepthStencilView(&depth, None, depth_written);
            let tested = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: DXGI_FORMAT_D32_FLOAT,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                Flags: D3D12_DSV_FLAG_READ_ONLY_DEPTH,
                ..Default::default()
            };
            device.CreateDepthStencilView(&depth, Some(&tested), depth_tested);

            // Two pairs of shader-visible heaps: views (one view of the
            // small texture each) and samplers (none used).
            let view_heaps = [
                heap(&device, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, true),
                heap(&device, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, true),
            ];
            let sampler_heaps = [
                heap(&device, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 1, true),
                heap(&device, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 1, true),
            ];
            for views in &view_heaps {
                device.CreateShaderResourceView(&image, None, views.GetCPUDescriptorHandleForHeapStart());
            }
            let tables = [
                view_heaps[0].GetGPUDescriptorHandleForHeapStart(),
                view_heaps[1].GetGPUDescriptorHandleForHeapStart(),
            ];
            let heap_pairs = [
                [Some(view_heaps[0].clone()), Some(sampler_heaps[0].clone())],
                [Some(view_heaps[1].clone()), Some(sampler_heaps[1].clone())],
            ];

            // The first list: both targets and the depth texture cleared.
            list.ClearRenderTargetView(targets[0], &[0.0; 4], None);
            list.ClearRenderTargetView(targets[1], &[0.0; 4], None);
            list.ClearDepthStencilView(depth_written, D3D12_CLEAR_FLAG_DEPTH, 1.0, 0, None);
            let [first_target, second_target] = target;
            let mut rig = Rig {
                queue,
                allocator,
                list,
                fence,
                submitted: 0,
                event,
                root,
                pipelines,
                work_root,
                work,
                words,
                other,
                depth,
                _kept: vec![first_target, second_target, image],
                _target_heaps: [rtvs, dsvs],
                heap_pairs,
                tables,
                targets,
                depth_written,
                depth_tested,
                stamps,
                back,
                card,
            };
            rig.finish();
            Some(rig)
        }
    }

    /// Closes the open list, runs it, waits for the card and opens the
    /// list again.
    fn finish(&mut self) {
        self.submitted += 1;
        // SAFETY: our list, queue and fence; the allocator is reset
        // only after the card finished everything recorded from it.
        unsafe {
            self.list.Close().unwrap();
            let run: ID3D12CommandList = self.list.cast().unwrap();
            self.queue.ExecuteCommandLists(&[Some(run)]);
            self.queue.Signal(&self.fence, self.submitted).unwrap();
            self.fence.SetEventOnCompletion(self.submitted, self.event).unwrap();
            WaitForSingleObject(self.event, INFINITE);
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
        }
    }

    /// What every list starts with: a new list has nothing set.
    fn begin(&self) {
        let l = &self.list;
        // SAFETY: an open list and live objects and views.
        unsafe {
            l.SetDescriptorHeaps(&self.heap_pairs[0]);
            l.SetGraphicsRootSignature(&self.root);
            l.SetPipelineState(&self.pipelines[0]);
            l.SetGraphicsRootDescriptorTable(0, self.tables[0]);
            l.OMSetRenderTargets(1, Some(&self.targets[0]), false, Some(&self.depth_tested));
            l.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: WIDTH as f32,
                Height: HEIGHT as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            l.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: WIDTH as i32,
                bottom: HEIGHT as i32,
            }]);
            // A triangle list.
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
        }
    }

    /// One batch of [`DRAWS`] draws in `lists` lists run one by one,
    /// with `between` recorded before every draw: the microseconds a
    /// draw took on the card, and the milliseconds recording took.
    pub(super) fn batch(&mut self, between: Between, lists: u32) -> (f64, f64) {
        let (target, read, written, tested) = (
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATE_DEPTH_READ,
        );
        let started = std::time::Instant::now();
        let mut drawn = 0u32;
        for part in 0..lists {
            let l = &self.list;
            // SAFETY: an open list; our query heap; live objects.
            unsafe {
                if part == 0 {
                    l.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                }
            }
            self.begin();
            // SAFETY: an open list with everything set; live objects.
            // Every state moved is moved back within the batch (the
            // draws of a list are an even number).
            unsafe {
                for _ in 0..DRAWS / lists {
                    let odd = (drawn % 2) as usize;
                    match between {
                        Between::Nothing => {}
                        Between::Pipeline => l.SetPipelineState(&self.pipelines[odd]),
                        Between::BarrierPair => {
                            l.ResourceBarrier(&[transition(&self.other, target, read)]);
                            l.ResourceBarrier(&[transition(&self.other, read, target)]);
                        }
                        Between::DepthMove => {
                            let (from, to) = if odd == 0 { (written, tested) } else { (tested, written) };
                            l.ResourceBarrier(&[transition(&self.depth, from, to)]);
                        }
                        Between::Heaps => {
                            l.SetDescriptorHeaps(&self.heap_pairs[odd]);
                            l.SetGraphicsRootDescriptorTable(0, self.tables[odd]);
                        }
                        Between::Targets => {
                            l.OMSetRenderTargets(1, Some(&self.targets[odd]), false, Some(&self.depth_tested));
                        }
                        Between::Compute | Between::ComputeWithWaits => {
                            let waits = between == Between::ComputeWithWaits;
                            if waits {
                                l.ResourceBarrier(&[super::super::uav_barrier()]);
                            }
                            l.SetComputeRootSignature(&self.work_root);
                            l.SetPipelineState(&self.work);
                            l.SetComputeRootUnorderedAccessView(0, self.words.GetGPUVirtualAddress());
                            l.Dispatch(1, 1, 1);
                            if waits {
                                l.ResourceBarrier(&[super::super::uav_barrier()]);
                            }
                            l.SetPipelineState(&self.pipelines[0]);
                        }
                    }
                    l.DrawInstanced(3, 1, 0, 0);
                    drawn += 1;
                }
                if part + 1 < lists {
                    // The next list of the batch: this one is sent on
                    // its own and the list is opened again at once
                    // (the allocator is not reset before the card is
                    // done with all of them).
                    l.Close().unwrap();
                    let run: ID3D12CommandList = l.cast().unwrap();
                    self.queue.ExecuteCommandLists(&[Some(run)]);
                    l.Reset(&self.allocator, None).unwrap();
                } else {
                    l.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                    l.ResolveQueryData(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.back, 0);
                }
            }
        }
        let recording = started.elapsed().as_secs_f64() * 1e3;
        self.finish();
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: 16 };
        // SAFETY: a read-back buffer the finished list resolved two
        // timestamps into.
        let ticks = unsafe {
            self.back.Map(0, Some(&range), Some(&mut data)).unwrap();
            let stamps = std::slice::from_raw_parts(data.cast::<u64>(), 2).to_vec();
            self.back.Unmap(0, None);
            stamps[1].saturating_sub(stamps[0])
        };
        // SAFETY: a plain query of our queue.
        let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1).max(1);
        (ticks as f64 * 1e6 / frequency as f64 / f64::from(drawn.max(1)), recording)
    }
}

