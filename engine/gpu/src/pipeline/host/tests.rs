use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::state::{host_blend, sanitize_samples, sanitize_targets};
use super::*;

/// A triangle mesh shader of our own and a pixel shader for it.
const MS: &str = "struct V { float4 p : SV_Position; };
    [outputtopology(\"triangle\")] [numthreads(1, 1, 1)]
    void main(out vertices V v[3], out indices uint3 t[1]) {
        SetMeshOutputCounts(3, 1);
        v[0].p = float4(0, 0, 0, 1);
        v[1].p = float4(1, 0, 0, 1);
        v[2].p = float4(0, 1, 0, 1);
        t[0] = uint3(0, 1, 2);
    }";
pub(super) const PS: &str = "float4 main() : SV_Target { return 1; }";

/// The PC `dxc.exe` (`OATHKEEP_PC_DXC`, else the newest Windows SDK's).
pub(super) fn dxc() -> Option<PathBuf> {
    let dxc = match std::env::var_os("OATHKEEP_PC_DXC") {
        Some(p) => PathBuf::from(p),
        None => oathkeep_shader::emit::validator::Validator::default_path()?
            .with_file_name("dxc.exe"),
    };
    dxc.exists().then_some(dxc)
}

pub(super) fn compile(dxc: &Path, dir: &Path, src: &str, profile: &str) -> Vec<u8> {
    let (hlsl, out) = (
        dir.join(format!("{profile}.hlsl")),
        dir.join(format!("{profile}.dxil")),
    );
    std::fs::write(&hlsl, src).unwrap();
    let status = std::process::Command::new(dxc)
        .args(["-nologo", "-T", profile, "-E", "main", "-Fo"])
        .arg(&out)
        .arg(&hlsl)
        .status()
        .unwrap();
    assert!(status.success(), "dxc failed for {profile}");
    std::fs::read(out).unwrap()
}

pub(super) fn code(bytes: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    }
}

/// A mesh pipeline made once is loaded from the library the next time,
/// not compiled again (a stream without a topology never loads).
#[test]
fn a_mesh_pipeline_loads_from_the_library() {
    let Some(host) = host() else {
        return; // no host GPU here
    };
    if !host.mesh_shaders() {
        eprintln!("no host mesh shader support; skipped");
        return;
    }
    let Some(dxc) = dxc() else {
        eprintln!("no PC dxc.exe; skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ms = compile(&dxc, dir.path(), MS, "ms_6_5");
    let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
    let (root, root_blob) = root_signature(&host.device, &[], &[], false).unwrap();
    let mut g = GraphicsState::default();
    // This pipeline has only a color target, with no depth buffer.
    g.depth.DepthEnable = false.into();
    g.targets.NumRenderTargets = 1;
    g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    let name = super::super::library::name("mesh test", &root_blob, &[&ms, &ps], &g.describe());
    mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, false, &name).expect("made");
    let loaded = super::super::library::LOADED.load(Ordering::Relaxed);
    mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, false, &name).expect("made again");
    assert!(
        super::super::library::LOADED.load(Ordering::Relaxed) > loaded,
        "the mesh pipeline was compiled again instead of loaded from the library"
    );
}

fn targets(formats: &[DXGI_FORMAT]) -> D3D12_RT_FORMAT_ARRAY {
    let mut t = D3D12_RT_FORMAT_ARRAY {
        NumRenderTargets: formats.len() as u32,
        ..Default::default()
    };
    t.RTFormats[..formats.len()].copy_from_slice(formats);
    t
}

fn blending(independent: bool) -> D3D12_BLEND_DESC {
    let mut b = GraphicsState::default().blend;
    b.IndependentBlendEnable = independent.into();
    for t in &mut b.RenderTarget {
        t.BlendEnable = true.into();
        t.SrcBlend = D3D12_BLEND_SRC_ALPHA;
        t.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
    }
    b
}

#[test]
fn integer_targets_do_not_blend() {
    let t = targets(&[
        DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R8_UINT,
    ]);
    for independent in [false, true] {
        let host = host_blend(&blending(independent), &t);
        assert!(host.IndependentBlendEnable.as_bool());
        assert!(host.RenderTarget[0].BlendEnable.as_bool());
        assert!(host.RenderTarget[1].BlendEnable.as_bool());
        assert!(!host.RenderTarget[2].BlendEnable.as_bool());
        // The rest of the target's state is the title's.
        assert_eq!(host.RenderTarget[2].SrcBlend, D3D12_BLEND_SRC_ALPHA);
        assert_eq!(host.RenderTarget[0].DestBlend, D3D12_BLEND_INV_SRC_ALPHA);
    }
}

#[test]
fn blend_states_without_integer_blending_are_kept() {
    // No integer target.
    let b = blending(false);
    let t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R11G11B10_FLOAT]);
    assert_eq!(host_blend(&b, &t), b);
    // An integer target that does not blend.
    let mut b = blending(true);
    b.RenderTarget[1].BlendEnable = false.into();
    let t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_UINT]);
    assert_eq!(host_blend(&b, &t), b);
    // Integer formats past the targets in use.
    let mut t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM]);
    t.RTFormats[1] = DXGI_FORMAT_R8_UINT;
    assert_eq!(host_blend(&blending(false), &t), blending(false));
}

fn element(format: DXGI_FORMAT, offset: u32) -> D3D12_INPUT_ELEMENT_DESC {
    D3D12_INPUT_ELEMENT_DESC {
        SemanticName: PCSTR(c"TEXCOORD".as_ptr().cast()),
        SemanticIndex: 0,
        Format: format,
        InputSlot: 0,
        AlignedByteOffset: offset,
        InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
        InstanceDataStepRate: 0,
    }
}

fn offsets(inputs: &[D3D12_INPUT_ELEMENT_DESC]) -> Vec<u32> {
    let state = GraphicsState {
        inputs: inputs.to_vec(),
        ..GraphicsState::default()
    };
    state.elements().iter().map(|e| e.offset).collect()
}

/// An appended element after one of 1 or 2 bytes started right after
/// it; D3D12 aligns it to min(4, its size), so the rectangle expansion
/// read its vertices from the wrong bytes.
#[test]
fn appended_elements_are_aligned() {
    const APPEND: u32 = D3D12_APPEND_ALIGNED_ELEMENT;
    // RG8 (2 bytes) then RG32 float: 4, not 2.
    let a = [
        element(DXGI_FORMAT_R8G8_UNORM, 0),
        element(DXGI_FORMAT_R32G32_FLOAT, APPEND),
    ];
    assert_eq!(offsets(&a), [0, 4]);
    // R8 then R16 float: 2, not 1; then R8 packs right after it.
    let b = [
        element(DXGI_FORMAT_R8_UNORM, 0),
        element(DXGI_FORMAT_R16_FLOAT, APPEND),
        element(DXGI_FORMAT_R8_UNORM, APPEND),
    ];
    assert_eq!(offsets(&b), [0, 2, 4]);
    // Already aligned layouts are unchanged.
    let c = [
        element(DXGI_FORMAT_R8G8B8A8_UNORM, 0),
        element(DXGI_FORMAT_R32G32_FLOAT, APPEND),
        element(DXGI_FORMAT_R32G32B32_FLOAT, APPEND),
    ];
    assert_eq!(offsets(&c), [0, 4, 12]);
}

/// An explicit offset near 4 GiB overflowed the next appended offset
/// (a panic in debug builds).
#[test]
fn a_huge_explicit_offset_does_not_overflow() {
    let inputs = [
        element(DXGI_FORMAT_R32G32B32A32_FLOAT, u32::MAX - 8),
        element(DXGI_FORMAT_R32_FLOAT, D3D12_APPEND_ALIGNED_ELEMENT),
    ];
    let _ = offsets(&inputs);
}

#[test]
fn pso_state_sanitization_fixes_invalid_targets_samples_and_blend() {
    let mut raw_targets = D3D12_RT_FORMAT_ARRAY {
        RTFormats: [DXGI_FORMAT_R8G8B8A8_UNORM; 8],
        NumRenderTargets: 2,
    };
    raw_targets.RTFormats[1] = DXGI_FORMAT_R8_UINT;
    let targets = sanitize_targets(&raw_targets);
    assert_eq!(targets.NumRenderTargets, 2);
    assert_eq!(targets.RTFormats[0], DXGI_FORMAT_R8G8B8A8_UNORM);
    assert_eq!(targets.RTFormats[1], DXGI_FORMAT_R8_UINT);
    for i in 2..8 {
        assert_eq!(targets.RTFormats[i], DXGI_FORMAT_UNKNOWN);
    }

    let over = D3D12_RT_FORMAT_ARRAY {
        RTFormats: [DXGI_FORMAT_R8G8B8A8_UNORM; 8],
        NumRenderTargets: 12,
    };
    assert_eq!(sanitize_targets(&over).NumRenderTargets, 8);

    let zero_samples = sanitize_samples(DXGI_SAMPLE_DESC { Count: 0, Quality: 5 });
    assert_eq!((zero_samples.Count, zero_samples.Quality), (1, 0));
    let msaa = sanitize_samples(DXGI_SAMPLE_DESC { Count: 4, Quality: 1 });
    assert_eq!((msaa.Count, msaa.Quality), (4, 1));

    // Integer RTV with shared blend + LogicOpEnable must disable BlendEnable
    // on the integer target and clear LogicOpEnable when IndependentBlendEnable turns on.
    let mut blend = D3D12_BLEND_DESC::default();
    blend.RenderTarget[0].BlendEnable = true.into();
    blend.RenderTarget[0].LogicOpEnable = true.into();
    let fixed = host_blend(&blend, &targets);
    assert!(fixed.IndependentBlendEnable.as_bool());
    assert!(fixed.RenderTarget[0].BlendEnable.as_bool());
    assert!(!fixed.RenderTarget[1].BlendEnable.as_bool());
    for rt in &fixed.RenderTarget {
        assert!(!rt.LogicOpEnable.as_bool());
    }
}
