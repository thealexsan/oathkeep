use super::*;
use crate::object::{S_OK, slot};

#[cfg(all(windows, feature = "diagnostics"))]
#[test]
fn skip_hashes_accepts_full_hashes_only() {
    assert!(drops::parse_skip_hashes(" , ").is_empty());
    assert_eq!(
        drops::parse_skip_hashes(" 0123456789ABCDEF, 0123456789abcde, xyz, 0011223344556677,"),
        ["0123456789abcdef", "0011223344556677"]
    );
}

fn shaders(p: This) -> Vec<(&'static str, Vec<u8>)> {
    pipeline(p)
        .shaders
        .iter()
        .map(|s| (s.stage, s.container.clone()))
        .collect()
}

#[test]
fn pipelines_keep_their_shaders() {
    let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
    #[cfg(windows)]
    let mut full = {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs.as_ptr().cast(),
                BytecodeLength: vs.len(),
            },
            PS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: ps.as_ptr().cast(),
                BytecodeLength: ps.len(),
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            DSVFormat: DXGI_FORMAT_D32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 0, Quality: 0 },
            SampleMask: u32::MAX,
            ..Default::default()
        };
        d.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        d.RTVFormats[1] = DXGI_FORMAT_R8G8B8A8_UNORM; // trailing stale format
        d
    };
    #[cfg(windows)]
    let desc_ptr = (&raw const full).cast::<GraphicsHead>();
    #[cfg(not(windows))]
    let desc = {
        let none = || Bytecode {
            code: std::ptr::null(),
            size: 0,
        };
        GraphicsHead {
            root: std::ptr::null(),
            stages: [
                Bytecode { code: vs.as_ptr(), size: vs.len() },
                Bytecode { code: ps.as_ptr(), size: ps.len() },
                none(),
                none(),
                none(),
            ],
        }
    };
    #[cfg(not(windows))]
    let desc_ptr = &raw const desc;
    let mut p = std::ptr::null();
    assert_eq!(
        create_graphics(std::ptr::null(), desc_ptr, &PIPELINE_IIDS[0], &mut p),
        S_OK
    );
    assert_eq!(
        shaders(p),
        [("vertex", vs.to_vec()), ("pixel", ps.to_vec())]
    );
    #[cfg(windows)]
    {
        let g = pipeline(p).host.graphics.as_deref().expect("plain graphics state kept");
        assert_eq!(g.topology.0, 3);
        assert_eq!(g.targets.NumRenderTargets, 1);
        assert_eq!(g.targets.RTFormats[0].0, 28);
        assert_eq!(g.targets.RTFormats[1].0, 0, "trailing RTV format zeroed");
        assert_eq!(g.depth_format.0, 40);
        assert_eq!(g.samples.Count, 1, "SampleDesc.Count 0 normalized to 1");
    }
    // A second pipeline with the same pixel shader shares its container.
    let mut q = std::ptr::null();
    create_graphics(std::ptr::null(), desc_ptr, &PIPELINE_IIDS[0], &mut q);
    assert!(Arc::ptr_eq(
        &pipeline(p).shaders[1],
        &pipeline(q).shaders[1]
    ));
    assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
    assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    #[cfg(windows)]
    let _ = &mut full;
}

/// The stream layout the title builds (title+0x86f090): root
/// signature, five shader slots (the first a mesh shader), blend, sample
/// mask, rasterizer, depth-stencil 1, sample description, input layout,
/// topology, render target and depth formats: 0x2b0 bytes.
#[test]
fn streams_yield_their_shaders() {
    let (ms, ps) = ([9u8; 7], [4u8; 5]);
    let mut stream = vec![0u8; 0x2b0];
    let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
    for (at, kind) in [
        (0x10, 24),
        (0x28, 2),
        (0x40, 3),
        (0x58, 4),
        (0x70, 5),
        (0x88, 8),
        (0x1d8, 9),
        (0x1e0, 10),
        (0x210, 21),
        (0x250, 17),
        (0x260, 12),
        (0x278, 14),
        (0x280, 15),
        (0x2a8, 16),
    ] {
        put(at, kind);
    }
    put(0x18, ms.as_ptr() as u64);
    put(0x20, ms.len() as u64);
    put(0x30, ps.as_ptr() as u64);
    put(0x38, ps.len() as u64);
    // Topology type 3 (triangles), one render target of format 28.
    put(0x278, 14 | 3 << 32);
    put(0x280, 15 | 28 << 32);
    put(0x2a0, 1 << 32);
    let desc = StreamDesc {
        size: stream.len(),
        stream: stream.as_ptr(),
    };
    let mut p = std::ptr::null();
    let hr = create_stream(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
    assert_eq!(hr, S_OK);
    assert_eq!(shaders(p), [("mesh", ms.to_vec()), ("pixel", ps.to_vec())]);
    #[cfg(windows)]
    {
        let g = pipeline(p).host.graphics.as_deref().unwrap();
        assert_eq!(g.topology.0, 3);
        assert_eq!(
            (g.targets.NumRenderTargets, g.targets.RTFormats[0].0),
            (1, 28)
        );
    }
}
