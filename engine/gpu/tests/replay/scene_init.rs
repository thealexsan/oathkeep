//! Scene state and D3D12 object initialization for the replay integration test.

use oathkeep_gpu::replay::{play, record};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain3;
use windows::Win32::System::Threading::CreateEventW;
use windows::core::s;

use crate::d3d::{
    COMMON, FROM, In, READ, SIZE, TO, UAV, buffer_desc, committed, swap_chain, texture_desc,
};
use crate::shaders::{ADD, ARGS, CLAMP, DRAW, Made, PLAIN, code, compile, compute, mesh};

pub struct Scene {
    pub(crate) device: ID3D12Device,
    pub(crate) queue: ID3D12CommandQueue,
    pub(crate) fence: ID3D12Fence,
    pub(crate) event: HANDLE,
    pub(crate) value: u64,
    pub(crate) lists: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)>,
    pub(crate) draw: Made,
    pub(crate) plain: Made,
    pub(crate) mesh: Option<(Made, ID3D12CommandSignature)>,
    pub(crate) vertices: (ID3D12Resource, *mut u8),
    pub(crate) samplers: ID3D12DescriptorHeap,
    pub(crate) still: Vec<(&'static str, ID3D12Resource, D3D12_RESOURCE_STATES)>,
    pub(crate) copies: (
        ID3D12CommandQueue,
        ID3D12CommandAllocator,
        ID3D12GraphicsCommandList,
    ),
    pub(crate) args: Made,
    pub(crate) clamp: Made,
    pub(crate) add: Made,
    pub(crate) signature: ID3D12CommandSignature,
    pub(crate) views: ID3D12DescriptorHeap,
    pub(crate) step: usize,
    pub(crate) rtvs: ID3D12DescriptorHeap,
    pub(crate) dsvs: ID3D12DescriptorHeap,
    pub(crate) target: ID3D12Resource,
    pub(crate) _depth: ID3D12Resource,
    pub(crate) fixed: ID3D12Resource,
    pub(crate) one: ID3D12Resource,
    pub(crate) other: ID3D12Resource,
    pub(crate) _heap: ID3D12Heap,
    pub(crate) image: ID3D12Resource,
    pub(crate) sum: ID3D12Resource,
    pub(crate) arguments: ID3D12Resource,
    pub(crate) scratch: ID3D12Resource,
    pub(crate) budget: ID3D12Resource,
    pub(crate) shared: (ID3D12Resource, *mut u8),
    pub(crate) constants: (ID3D12Resource, *mut u8),
    pub(crate) texels: (ID3D12Resource, *mut u8),
    pub(crate) back: (ID3D12Resource, *mut u8),
    pub(crate) swap: Option<(IDXGISwapChain3, HWND)>,
}

impl Scene {
    pub fn new(device: &ID3D12Device, window: bool) -> Scene {
        // SAFETY: plain creation throughout.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let lists = (0..3)
                .map(|_| {
                    let allocator: ID3D12CommandAllocator = device
                        .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                        .unwrap();
                    let list: ID3D12GraphicsCommandList = device
                        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                        .unwrap();
                    list.Close().unwrap();
                    (allocator, list)
                })
                .collect();
            let (vs, ps) = (
                compile(DRAW, s!("vs"), s!("vs_5_1")),
                compile(DRAW, s!("ps"), s!("ps_5_1")),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &vs).unwrap();
            let mut blend = D3D12_BLEND_DESC::default();
            blend.RenderTarget[0].RenderTargetWriteMask = 15;
            let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs),
                PS: code(&ps),
                BlendState: blend,
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                DSVFormat: DXGI_FORMAT_D32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            desc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
            let pso: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc).unwrap();
            record::name(&pso, "a drawing pipeline");
            let draw = Made { root, pso };
            let (vs, ps) = (
                compile(PLAIN, s!("vs"), s!("vs_5_1")),
                compile(PLAIN, s!("ps"), s!("ps_5_1")),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &vs).unwrap();
            let element = |name, index, format, at| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: name,
                SemanticIndex: index,
                Format: format,
                InputSlot: 0,
                AlignedByteOffset: at,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            };
            let elements = [
                element(s!("POSITION"), 0, DXGI_FORMAT_R32G32_FLOAT, 0),
                element(s!("COLOR"), 1, DXGI_FORMAT_R32G32B32_FLOAT, 8),
            ];
            desc.pRootSignature = std::mem::transmute_copy(&root);
            (desc.VS, desc.PS) = (code(&vs), code(&ps));
            desc.InputLayout = D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: elements.as_ptr(),
                NumElements: 2,
            };
            let pso: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc).unwrap();
            let plain = Made { root, pso };
            let clamp = compute(device, CLAMP);
            record::name(&clamp.pso, play::CLAMP);
            let argument = D3D12_INDIRECT_ARGUMENT_DESC {
                Type: D3D12_INDIRECT_ARGUMENT_TYPE_DRAW,
                ..Default::default()
            };
            let mut signature: Option<ID3D12CommandSignature> = None;
            let desc = D3D12_COMMAND_SIGNATURE_DESC {
                ByteStride: 16,
                NumArgumentDescs: 1,
                pArgumentDescs: &argument,
                NodeMask: 0,
            };
            device
                .CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut signature)
                .unwrap();
            let heap = |kind, count, flags| -> ID3D12DescriptorHeap {
                device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: flags,
                        NodeMask: 0,
                    })
                    .unwrap()
            };
            let samplers = heap(
                D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                4,
                D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            );
            let sampler = D3D12_SAMPLER_DESC {
                Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                MaxLOD: 8.0,
                ..Default::default()
            };
            let mut at = samplers.GetCPUDescriptorHandleForHeapStart();
            at.ptr += 2 * device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER)
                as usize;
            device.CreateSampler(&sampler, at);
            let views = heap(
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                5000,
                D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            );
            let rtvs = heap(
                D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                1,
                D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            );
            let dsvs = heap(
                D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                1,
                D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            );
            let rgba = DXGI_FORMAT_R8G8B8A8_UNORM;
            let (target, _) = committed(
                device,
                In::Card,
                &texture_desc(SIZE, rgba, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET),
                FROM,
            );
            let (depth, _) = committed(
                device,
                In::Card,
                &texture_desc(
                    SIZE,
                    DXGI_FORMAT_D32_FLOAT,
                    D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                ),
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
            );
            let (fixed, _) = committed(
                device,
                In::Card,
                &texture_desc(16, rgba, D3D12_RESOURCE_FLAG_NONE),
                TO,
            );
            let (image, _) = committed(
                device,
                In::Card,
                &texture_desc(16, rgba, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                UAV,
            );
            let mut memory: Option<ID3D12Heap> = None;
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: 4 << 20,
                Properties: D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
            };
            device.CreateHeap(&desc, &mut memory).unwrap();
            let memory = memory.unwrap();
            let placed = || {
                let mut made: Option<ID3D12Resource> = None;
                let d = texture_desc(16, rgba, D3D12_RESOURCE_FLAG_NONE);
                device
                    .CreatePlacedResource(&memory, 65536, &d, TO, None, &mut made)
                    .unwrap();
                made.unwrap()
            };
            // Two textures in one place.
            let (one, other) = (placed(), placed());
            let uav = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
            let card = |size| committed(device, In::Card, &buffer_desc(size, uav), COMMON).0;
            let (sum, arguments, scratch, budget) = (card(1024), card(4096), card(4096), card(256));
            let shared = committed(device, In::Shared, &buffer_desc(70_000, uav), COMMON);
            let constants = committed(
                device,
                In::Upload,
                &buffer_desc(8192, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_GENERIC_READ,
            );
            let texels = committed(
                device,
                In::Upload,
                &buffer_desc(65536, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_GENERIC_READ,
            );
            let back = committed(
                device,
                In::Back,
                &buffer_desc(1 << 20, D3D12_RESOURCE_FLAG_NONE),
                TO,
            );
            let vertices = committed(
                device,
                In::Upload,
                &buffer_desc(4096, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_GENERIC_READ,
            );
            let still = {
                let blocks = D3D12_RESOURCE_DESC {
                    MipLevels: 5,
                    DepthOrArraySize: 3,
                    ..texture_desc(64, DXGI_FORMAT_BC7_UNORM, D3D12_RESOURCE_FLAG_NONE)
                };
                let volume = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D,
                    DepthOrArraySize: 8,
                    MipLevels: 3,
                    ..texture_desc(16, DXGI_FORMAT_R16_FLOAT, D3D12_RESOURCE_FLAG_NONE)
                };
                let stencil = texture_desc(
                    32,
                    DXGI_FORMAT_R32G8X24_TYPELESS,
                    D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                );
                let (write, read) = (
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                );
                [
                    ("blocks", blocks, TO, READ),
                    ("volume", volume, TO, read),
                    ("stencil", stencil, write, write),
                ]
                .map(|(name, d, made_in, ends)| {
                    let (t, _) = committed(device, In::Card, &d, made_in);
                    record::name(&t, name);
                    (name, t, ends)
                })
            };
            let copies: (
                ID3D12CommandQueue,
                ID3D12CommandAllocator,
                ID3D12GraphicsCommandList,
            ) = {
                let queue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_COPY,
                        ..Default::default()
                    })
                    .unwrap();
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                    .unwrap();
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
                    .unwrap();
                list.Close().unwrap();
                (queue, allocator, list)
            };
            for (r, n) in [
                (&sum, "sum"),
                (&image, "image"),
                (&target, "target"),
                (&shared.0, "shared"),
                (&one, "one"),
                (&other, "other"),
            ] {
                record::name(r, n);
            }
            device.CreateRenderTargetView(&target, None, rtvs.GetCPUDescriptorHandleForHeapStart());
            device.CreateDepthStencilView(&depth, None, dsvs.GetCPUDescriptorHandleForHeapStart());
            let step = device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let swap = window.then(|| swap_chain(&queue));
            let scene = Scene {
                device: device.clone(),
                queue,
                fence,
                event: CreateEventW(None, false, false, None).unwrap(),
                value: 0,
                lists,
                draw,
                plain,
                mesh: mesh(device, blend),
                vertices,
                samplers,
                still: still.to_vec(),
                copies,
                args: compute(device, ARGS),
                clamp,
                add: compute(device, ADD),
                signature: signature.unwrap(),
                views,
                step,
                rtvs,
                dsvs,
                target,
                _depth: depth,
                fixed,
                one,
                other,
                _heap: memory,
                image,
                sum,
                arguments,
                scratch,
                budget,
                shared,
                constants,
                texels,
                back,
                swap,
            };
            // Null views everywhere, by copies of a slot made elsewhere.
            let block = heap(
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                16,
                D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            );
            let null = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: rgba,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            let from = block.GetCPUDescriptorHandleForHeapStart();
            for i in 0..16 {
                let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: from.ptr + i * step,
                };
                device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&null), at);
            }
            for i in (0..4992).step_by(16) {
                device.CopyDescriptorsSimple(
                    16,
                    scene.cpu(i),
                    from,
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                );
            }
            // The rest with the other copying method, in unlike pieces.
            let to = [scene.cpu(4992), scene.cpu(4994)];
            let sources = [
                from,
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: from.ptr + 4 * step,
                },
            ];
            device.CopyDescriptors(
                2,
                to.as_ptr(),
                Some([2u32, 6].as_ptr()),
                2,
                sources.as_ptr(),
                Some([4u32, 4].as_ptr()),
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            );
            scene.show(4001, &scene.other);
            scene.show(4002, &scene.one);
            device.CreateUnorderedAccessView(
                &scene.image,
                None::<&ID3D12Resource>,
                None,
                scene.cpu(7),
            );
            scene
        }
    }
}
