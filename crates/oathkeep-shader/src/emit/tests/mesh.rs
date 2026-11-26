use crate::dxbc::{Container, FourCC};
use crate::emit::container::SigElement;
use crate::emit::{HostShader, container, emit};
use crate::native::{HwStage, NativeProgram, StageIo};
use crate::{ShaderError, io, ir, resource, structure};

/// The LLVM text `dxc -dumpbin` prints for a DXIL container, when dxc
/// is installed.
#[cfg(windows)]

use super::control::constant_color;

/// Triangle (0, 1, 2), each corner's 9-bit index in its 10-bit field.
pub(super) const TRIANGLE: u32 = 1 << 10 | 2 << 20;
/// The edge flags of the three corners (bits 9, 19 and 29).
const EDGE_FLAGS: u32 = 1 << 9 | 1 << 19 | 1 << 29;
/// The culling bit.
const CULL: u32 = 1 << 31;

/// A one-wave (32-lane) triangle mesh program whose lanes all export
/// primitive `prim` and, as vertex `lane`, the position
/// (lane & 1 ? 3 : -1, lane & 2 ? 3 : -1, 0.5, 1): triangle (0, 1, 2)
/// covers the whole target.
fn triangle_mesh(prim: u32) -> Result<HostShader, ShaderError> {
    triangle_mesh_to(prim, 4.0)
}

/// The triangle with its far corners at `reach` - 1.
pub(super) fn triangle_mesh_to(prim: u32, reach: f32) -> Result<HostShader, ShaderError> {
    // (the second corner's y comes out as 2 x its lane bit: half of it)
    let [x, y] = [reach.to_bits(), (reach / 2.0).to_bits()];
    let words = [
        0xD765_0001u32, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0x0001_00C1,
        0x3604_0281, // v_and_b32 v2, 1, v1
        0x3606_0282, // v_and_b32 v3, 2, v1
        0x7E04_0D02, // v_cvt_f32_u32 v2, v2
        0x7E06_0D03, // v_cvt_f32_u32 v3, v3
        0x1008_04FF, // v_mul_f32 v4, reach, v2
        x,
        0x0608_08F3, // v_add_f32 v4, -1.0, v4
        0x100A_06FF, // v_mul_f32 v5, reach / 2, v3
        y,
        0x060A_0AF3, // v_add_f32 v5, -1.0, v5
        0x7E0C_02F0, // v_mov_b32 v6, 0.5
        0x7E0E_02F2, // v_mov_b32 v7, 1.0
        0x7E12_02FF, // v_mov_b32 v9, prim
        prim,
        0xF800_0941, // exp prim v9, off, off, off done
        0x0000_0009,
        0xF800_08CF, // exp pos0 v4, v5, v6, v7 done
        0x0706_0504,
        0xBF81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Ms,
        wave64: false,
        vgprs: 10,
        layout: vec![],
        used: vec![],
        io: StageIo::Ms(crate::native::MeshIo {
            outputs: vec![],
            clip_cntl: 0,
            threads: 32,
            max_vertices: 32,
            max_primitives: 32,
            topology: 2,
            lds_bytes: 0,
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let position = SigElement {
        name: "SV_Position".into(),
        index: 0,
        kind: 3,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 0xF,
        half: false,
    };
    let osg1 = container::signature(&[position], false);
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes)?;
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    emit(&c, &native, &p, &st, &io, &res)
}

/// A primitive export's corners are 9-bit indices: the edge flag above
/// each (bits 9, 19, 29) is no part of it. Masking ten bits made a
/// flagged corner index 512 or more, past any mesh program's vertices
/// (the draw test below shows the triangle). Flagged, unflagged and
/// culled exports translate, and dxil.dll, where there is one, takes
/// each module.
#[test]
fn primitive_export_indices_drop_the_edge_flags() {
    #[cfg(windows)]
    let mut validator = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    )
    .ok();
    for prim in [
        TRIANGLE,
        TRIANGLE | EDGE_FLAGS,
        TRIANGLE | EDGE_FLAGS | CULL,
    ] {
        let ms = triangle_mesh(prim).unwrap_or_else(|e| panic!("{prim:#x}: {e}"));
        #[cfg(windows)]
        if let Some(v) = validator.as_mut() {
            v.validate(&mut ms.dxil.clone())
                .unwrap_or_else(|e| panic!("{prim:#x}: {e}"));
        }
    }
}

/// A triangle mesh program on the host GPU and on WARP (where they have
/// mesh shaders): triangle (0, 1, 2) covers the target with its corners'
/// edge flags set as without them; with the culling bit, nothing is lit.
#[test]
#[cfg(windows)]
fn edge_flagged_triangles_draw_as_unflagged_ones() {
    use crate::emit::host::{Draw, Host, Root};
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS7, D3D12_FEATURE_DATA_D3D12_OPTIONS7,
        D3D12_MESH_SHADER_TIER_NOT_SUPPORTED, D3D12_SHADER_VISIBILITY_MESH,
    };
    let _one = super::valu_helpers::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let ps = constant_color();
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        // SAFETY: the query writes the structure of the given size.
        let queried = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        if queried.is_err() || options.MeshShaderTier == D3D12_MESH_SHADER_TIER_NOT_SUPPORTED {
            eprintln!("no mesh shaders on the {label} device; skipped");
            continue;
        }
        for (prim, culled) in [
            (TRIANGLE, false),
            (TRIANGLE | EDGE_FLAGS, false),
            (TRIANGLE | EDGE_FLAGS | CULL, true),
        ] {
            let ms = triangle_mesh(prim).unwrap();
            let root = host
                .root_signature_for(&[(&ms, D3D12_SHADER_VISIBILITY_MESH)])
                .unwrap();
            let pso = host
                .mesh_pipeline_with(&root, &ms.dxil, &ps)
                .unwrap_or_else(|e| panic!("{label}: mesh pipeline: {e}"));
            let texels = host
                .draw(
                    &root,
                    &pso,
                    &Draw {
                        roots: vec![(0, Root::Constants(vec![0]))],
                        textures: &[],
                        samplers: &[],
                        vertices: &[],
                        stride: 0,
                        size: (16, 16),
                        mesh_groups: 1,
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: draw: {e}"));
            let lit = texels.iter().filter(|t| t[3] != 0.0).count();
            if culled {
                assert_eq!(lit, 0, "{label}, {prim:#x}: a culled triangle lit texels");
                continue;
            }
            assert!(
                texels.iter().all(|t| *t == [1.0, 0.5, 0.25, 1.0]),
                "{label}, {prim:#x}: {lit} of 256 texels lit"
            );
        }
        eprintln!("{label}: edge-flagged triangles drew");
        ran += 1;
    }
    if ran == 0 {
        eprintln!("no device with mesh shaders; skipped");
    }
}

/// A pixel shader interpolating at sample positions (PERSP_SAMPLE
/// barycentrics, `SPI_PS_INPUT_ADDR` bit 0) runs at sample frequency:
/// DXC's PSV0 says so in PSInfo.SampleFrequency (runtime info byte 1)
/// whenever an input is interpolated per sample.
#[test]
fn sample_interpolated_inputs_set_psv_sample_frequency() {
    let words = [
        0xC808_0000u32, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001,    // v_interp_p2_f32 v2, v1, attr0.x
        0xF800_180F,
        0x0202_0202, // exp mrt0, v2, v2, v2, v2 done vm
        0xBF81_0000,
    ];
    let psv_of = |input_addr: u32| {
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: crate::native::StageIo::Ps {
                inputs: vec![crate::native::PsInput {
                    usage: crate::native::Usage::Generic,
                    row: 0,
                    mask: 0b1111,
                    attr: 0,
                    flat: false,
                    fp16: false,
                    pack2: false,
                    row2: 0,
                }],
                input_addr,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let el = |name: &str, kind, usage| SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage,
            half: false,
        };
        let isg1 = crate::emit::container::signature(&[el("TEXCOORD", 0, 0)], true);
        let osg1 = crate::emit::container::signature(&[el("SV_Target", 16, 15)], false);
        let bytes =
            crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res =
            crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        let out = emit(&c, &native, &p, &st, &io, &res).unwrap();
        // dxil.dll, where there is one, checks PSV0 against the module.
        #[cfg(windows)]
        if let Ok(mut v) = crate::emit::validator::Validator::load(
            crate::emit::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
        let host = Container::parse(&out.dxil).unwrap();
        host.part(FourCC::PSV0).unwrap().data.to_vec()
    };
    // Runtime info starts after its size; PSInfo is {DepthOutput, SampleFrequency}.
    let center = psv_of(1 << 1);
    assert_eq!(center[4 + 1], 0, "center interpolation is per pixel");
    let sample = psv_of(1 << 0);
    assert_eq!(sample[4 + 1], 1, "PSInfo.SampleFrequency");
}

/// A pixel shader reading channel z of an input whose element starts at
/// w. Looking the channel up subtracted the element's first channel
/// before checking the channel lies in it, so in a build with overflow
/// checks (tests, debug) any read below an element that starts at z or
/// w panicked, whichever row it was in (found by random-input testing).
#[test]
fn reading_a_channel_below_an_input_element_does_not_underflow() {
    // v_interp_mov_f32 v3, p0, attr0.z; s_endpgm
    let words = [0xC80E_0202u32, 0xBF81_0000];
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 4,
        layout: vec![],
        used: vec![],
        io: crate::native::StageIo::Ps {
            inputs: vec![crate::native::PsInput {
                usage: crate::native::Usage::Generic,
                row: 0,
                mask: 0b1000,
                attr: 0,
                flat: false,
                fp16: false,
                pack2: false,
                row2: 0,
            }],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let w_only = SigElement {
        name: "TEXCOORD".into(),
        index: 0,
        kind: 0,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 3,
        cols: 1,
        usage: 0,
        half: false,
    };
    let isg1 = crate::emit::container::signature(&[w_only], true);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = crate::io::raise(&p, &native).unwrap();
    // Translated or refused with a reason; it must not panic.
    let _ = emit(&c, &native, &p, &st, &io, &res);
}

