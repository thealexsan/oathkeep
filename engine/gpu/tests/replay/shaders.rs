//! HLSL shaders and pipeline builders for the replay integration test scene.

use oathkeep_gpu::replay::record;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR, s, w};

pub const DRAW: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), CBV(b1), DescriptorTable(SRV(t0, numDescriptors=unbounded)), DescriptorTable(Sampler(s0))"
cbuffer Pick : register(b0) { uint pick; uint pad; };
cbuffer Place : register(b1) { float4 place; };
Texture2D all[] : register(t0);
SamplerState point_sampler : register(s0);
struct V { float4 at : SV_Position; float2 uv : UV; };
[RootSignature(RS)]
V vs(uint i : SV_VertexID) {
    float2 c = float2(i & 1, (i >> 1) & 1);
    if (i == 3) c = float2(1, 0);
    if (i == 4) c = float2(0, 1);
    if (i == 5) c = float2(1, 1);
    V v;
    v.uv = c;
    v.at = float4(place.xy + c * place.zw, 0.5, 1);
    return v;
}
float4 ps(V v) : SV_Target { return all[NonUniformResourceIndex(pick)].SampleLevel(point_sampler, v.uv, 0); }
"#;

/// Triangles of vertices and indices.
pub const PLAIN: &str = r#"
#define RS "RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT)"
struct V { float4 at : SV_Position; float3 color : COLOR; };
[RootSignature(RS)]
V vs(float2 at : POSITION, float3 color : COLOR1) { V v; v.at = float4(at, 0.4, 1); v.color = color; return v; }
float4 ps(V v) : SV_Target { return float4(v.color, 1); }
"#;

/// A quad from a mesh shader, its texture picked from the heap by index.
pub const MESH: &str = r#"
#define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), RootConstants(num32BitConstants=5, b0)"
cbuffer C : register(b0) { float4 place; uint pick; };
struct V { float4 at : SV_Position; float2 uv : UV; };
[RootSignature(RS)]
[numthreads(1, 1, 1)]
[outputtopology("triangle")]
void ms(out vertices V v[4], out indices uint3 t[2]) {
    SetMeshOutputCounts(4, 2);
    for (uint i = 0; i < 4; i++) {
        float2 c = float2(i & 1, i >> 1);
        v[i].at = float4(place.xy + c * place.zw, 0.3, 1);
        v[i].uv = c;
    }
    t[0] = uint3(0, 1, 2);
    t[1] = uint3(2, 1, 3);
}
[RootSignature(RS)]
float4 ps(V v) : SV_Target {
    Texture2D<float4> t = ResourceDescriptorHeap[pick];
    return t.Load(int3(v.uv * 16, 0)) * 0.5 + float4(v.uv, 0, 0) * 0.5;
}
"#;

/// Writes the arguments of indirect draws (two records: six vertices, and
/// one far too many that the clamp cuts to none).
pub const ARGS: &str = r#"
#define RS "SRV(t0), UAV(u0)"
ByteAddressBuffer from : register(t0);
RWByteAddressBuffer args : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    args.Store4(0, uint4(from.Load(0), 1, 0, 0));
    args.Store4(16, uint4(4000000000u, 1, 0, 0));
}
"#;

/// The argument clamp's parameters, with a simple bound.
pub const CLAMP: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), SRV(t1), UAV(u0), UAV(u1)"
cbuffer C : register(b0) { uint kind; uint stride; uint records; uint args_offset; uint has_count; uint count_offset; };
ByteAddressBuffer from : register(t0);
ByteAddressBuffer count : register(t1);
RWByteAddressBuffer to : register(u0);
RWByteAddressBuffer budget : register(u1);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= records) return;
    uint4 a = from.Load4(args_offset + id.x * stride);
    if (a.x > 1000) a.x = 0;
    to.Store4(id.x * stride, a);
    uint was;
    budget.InterlockedAdd(0, a.x, was);
}
"#;

/// Adds what the CPU gives to what was there, and writes it on.
pub const ADD: &str = r#"
#define RS "SRV(t0), DescriptorTable(UAV(u0)), UAV(u1)"
ByteAddressBuffer from : register(t0);
RWTexture2D<float4> image : register(u0);
RWByteAddressBuffer sum : register(u1);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint at = (id.y * 16 + id.x) * 4;
    uint v = from.Load(at) + sum.Load(at) * 3;
    sum.Store(at, v);
    image[id.xy] = image[id.xy] + float4((v & 15) / 255.0, 0, 1 / 255.0, 1);
}
"#;

pub struct Made {
    pub root: ID3D12RootSignature,
    pub pso: ID3D12PipelineState,
}

/// Compiles with the Windows SDK's dxc, if there is one.
fn dxc(source: &str, entry: &str, target: &str) -> Option<Vec<u8>> {
    let kits = std::fs::read_dir("C:/Program Files (x86)/Windows Kits/10/bin").ok()?;
    let exe = kits
        .flatten()
        .map(|e| e.path().join("x64/dxc.exe"))
        .filter(|p| p.exists())
        .max()?;
    let dir = tempfile::tempdir().ok()?;
    let (from, to) = (dir.path().join("a.hlsl"), dir.path().join("a.dxil"));
    std::fs::write(&from, source).ok()?;
    let out = std::process::Command::new(exe)
        .args(["-T", target, "-E", entry, "-Fo"])
        .arg(&to)
        .arg(&from)
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read(to).ok()
}

/// The mesh pipeline, from a stream, by way of a pipeline library; and the
/// signature of its indirect draws. `None` without dxc or mesh shaders.
pub fn mesh(
    device: &ID3D12Device,
    blend: D3D12_BLEND_DESC,
) -> Option<(Made, ID3D12CommandSignature)> {
    #[repr(C, align(8))]
    struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
    #[repr(C)]
    struct Stream {
        root: Sub<*mut std::ffi::c_void>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D3D12_DEPTH_STENCIL_DESC>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
    }
    let (ms, ps) = (dxc(MESH, "ms", "ms_6_6")?, dxc(MESH, "ps", "ps_6_6")?);
    // SAFETY: plain queries and creation; the stream holds whole subobjects.
    unsafe {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        let size = size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS7>() as u32;
        device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS7, (&raw mut o).cast(), size)
            .ok()?;
        if o.MeshShaderTier == D3D12_MESH_SHADER_TIER_NOT_SUPPORTED {
            return None;
        }
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &ms).unwrap();
        let mut formats = D3D12_RT_FORMAT_ARRAY {
            NumRenderTargets: 1,
            ..Default::default()
        };
        formats.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let mut stream = Stream {
            root: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                root.as_raw(),
            ),
            ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, code(&ms)),
            ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, code(&ps)),
            blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, blend),
            mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, u32::MAX),
            raster: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER,
                D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    ..Default::default()
                },
            ),
            depth: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
                D3D12_DEPTH_STENCIL_DESC::default(),
            ),
            targets: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
                formats,
            ),
            dsv: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
                DXGI_FORMAT_D32_FLOAT,
            ),
            samples: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
                DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
            ),
            topology: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            ),
        };
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: size_of::<Stream>(),
            pPipelineStateSubobjectStream: (&raw mut stream).cast(),
        };
        let library: ID3D12PipelineLibrary1 = device
            .cast::<ID3D12Device1>()
            .unwrap()
            .CreatePipelineLibrary::<ID3D12PipelineLibrary>(&[])
            .unwrap()
            .cast()
            .unwrap();
        {
            let made: ID3D12PipelineState = device
                .cast::<ID3D12Device2>()
                .unwrap()
                .CreatePipelineState(&desc)
                .unwrap();
            library.StorePipeline(w!("mesh"), &made).unwrap();
        }
        let pso: ID3D12PipelineState = library.LoadPipeline(w!("mesh"), &desc).unwrap();
        record::name(&pso, "a mesh pipeline");
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH,
            ..Default::default()
        };
        let mut signature: Option<ID3D12CommandSignature> = None;
        let desc = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: 12,
            NumArgumentDescs: 1,
            pArgumentDescs: &argument,
            NodeMask: 0,
        };
        device
            .CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut signature)
            .unwrap();
        Some((Made { root, pso }, signature.unwrap()))
    }
}

pub fn compile(source: &str, entry: PCSTR, target: PCSTR) -> Vec<u8> {
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: the source and names live through the call; the blobs' own
    // bytes.
    unsafe {
        let r = D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            None,
            None,
            None,
            entry,
            target,
            1 << 20,
            0,
            &mut code,
            Some(&mut errors),
        );
        if r.is_err() {
            let e = errors.unwrap();
            let text =
                std::slice::from_raw_parts(e.GetBufferPointer().cast::<u8>(), e.GetBufferSize());
            panic!("{}", String::from_utf8_lossy(text));
        }
        let code = code.unwrap();
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
            .to_vec()
    }
}

pub fn code(bytes: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    }
}

pub fn compute(device: &ID3D12Device, source: &str) -> Made {
    let cs = compile(source, s!("main"), s!("cs_5_1"));
    // SAFETY: plain creation.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &cs).unwrap();
        let pso = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: code(&cs),
                ..Default::default()
            })
            .unwrap();
        Made { root, pso }
    }
}
