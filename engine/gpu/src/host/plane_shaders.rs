use super::*;

pub(super) const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
pub(super) const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
pub(super) const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;
/// Compute shaders for copying a depth-stencil texture's depth plane back
/// into its colour plane twin (16-bit quantized steps or 32-bit floats).
pub(super) const DEPTH_TO_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), UAV(u0))"
Texture2DArray<float> depth_tex : register(t0);
RWTexture2DArray<uint> plane_uav : register(u0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void depth_to_plane(uint3 id : SV_DispatchThreadID) {
    uint w, h, a;
    plane_uav.GetDimensions(w, h, a);
    if (id.x < w && id.y < h) {
        float d = saturate(depth_tex.Load(int4(int2(id.xy), 0, 0)));
        plane_uav[uint3(id.xy, 0)] = uint(round(d * 65535.0));
    }
}
"#;
pub(super) const DEPTH_TO_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), UAV(u0))"
Texture2DArray<float> depth_tex : register(t0);
RWTexture2DArray<float> plane_uav : register(u0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void depth_to_plane(uint3 id : SV_DispatchThreadID) {
    uint w, h, a;
    plane_uav.GetDimensions(w, h, a);
    if (id.x < w && id.y < h) {
        plane_uav[uint3(id.xy, 0)] = depth_tex.Load(int4(int2(id.xy), 0, 0));
    }
}
"#;

/// Views of twins the pass reads (and depth-to-plane SRV/UAV pairs), in a
/// heap of ours used as a ring of 2-descriptor slots.
/// shortcut: a view must last until its list has run; with a handful of
/// passes a frame the ring of 256 pairs is far from coming round.
pub(super) const PLANE_VIEWS: u32 = 512;

/// What the pass draws with: the heap of its views of twins, per kind of
/// twin (16-bit steps or floats) and depth format the graphics root signature
/// and pipeline, and per kind of twin the compute root signature and pipeline
/// for copying depth back into the plane twin (`None`: the host refused).
pub(super) struct PlanePass {
    pub(super) views: ID3D12DescriptorHeap,
    pub(super) cpu: usize,
    pub(super) gpu: u64,
    pub(super) step: u32,
    pub(super) next: AtomicU32,
    pub(super) made: Mutex<Vec<(bool, i32, Option<PlanePipeline>)>>,
    pub(super) copy_back: Mutex<Vec<(bool, Option<PlanePipeline>)>>,
}

/// The pass's root signature and pipeline for one kind of twin and one
/// depth format.
pub(super) type PlanePipeline = (ID3D12RootSignature, ID3D12PipelineState);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for PlanePass {}
// SAFETY: as above.
unsafe impl Sync for PlanePass {}

pub(super) fn plane_pass() -> Option<&'static PlanePass> {
    static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: PLANE_VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(PlanePass {
                cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind),
                views,
                next: AtomicU32::new(0),
                made: Mutex::default(),
                copy_back: Mutex::default(),
            })
        }
    })
    .as_ref()
}

/// Compiles function `entry` of `source` for `target` with the system's
/// compiler, as the indirect-argument clamp is (`crate::indirect`).
pub(crate) fn compile(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("oathkeep"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("a shader of ours: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

