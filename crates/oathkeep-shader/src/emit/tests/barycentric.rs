use crate::dxbc::{Container, FourCC};
use crate::emit::container::SigElement;
use crate::emit::shader::Shader;
use super::super::signature::signature;
use crate::emit::{HostShader, container, emit, validator};
use crate::native::{PsInput, HwStage, NativeProgram, StageIo, Usage};
use crate::{ShaderError, io, ir, resource, structure};

/// `SPI_PS_INPUT_ADDR` bits: PERSP_CENTER, LINEAR_CENTER.

const PERSP_CENTER: u32 = 1 << 1;
const LINEAR_CENTER: u32 = 1 << 5;

/// `exp mrt0, v<r>, v<r>, v<r>, v<r> done vm`.
fn exp_mrt0(r: u32) -> [u32; 2] {
    [0xF800_180F, r * 0x0101_0101]
}

/// VOP2 `v_add_f32 v<dst>, v<a>, v<b>`.
fn v_add_f32(dst: u32, a: u32, b: u32) -> u32 {
    3 << 25 | dst << 17 | b << 9 | (256 + a)
}

fn element(name: &str, kind: u8, row: u32) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col: 0,
        cols: 4,
        usage: 0,
        half: false,
    }
}

/// Translates a pixel program with the input VGPRs `input_addr`
/// enables and one float4 TEXCOORD0 attribute (attr 0, row 0).
fn pixel(words: &[u32], input_addr: u32) -> Result<HostShader, ShaderError> {
    pixel_with_inputs(words, input_addr, &[element("TEXCOORD", 0, 0)])
}

fn pixel_with_inputs(words: &[u32], input_addr: u32, inputs: &[container::SigElement]) -> Result<HostShader, ShaderError> {
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
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
    let mut target = element("SV_Target", 16, 0);
    target.usage = 15;
    let isg1 = container::signature(inputs, true);
    let osg1 = container::signature(&[target], false);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    let res = resource::track(&p, &native.layout, native.stage, None)?;
    let io = io::raise(&p, &native)?;
    let st = structure::structure(&p)?;
    emit(&c, &native, &p, &st, &io, &res)
}

#[test]
fn an_unused_packed_input_matches_its_provoking_vertex_sibling() {
    let [e0, e1] = exp_mrt0(3);
    // v_interp_mov_f32 v3, p0, attr0.z followed by exp mrt0.
    let words = [0xC80E_0202, e0, e1, 0xBF81_0000];
    let xy = container::SigElement { cols: 2, ..element("TEXCOORD", 0, 0) };
    let zw = container::SigElement { col: 2, cols: 2, ..element("NORMAL", 0, 0) };
    let shader = pixel_with_inputs(&words, PERSP_CENTER, &[xy, zw]).unwrap();
    let c = Container::parse(&shader.dxil).unwrap();
    let psv = c.part(FourCC::PSV0).unwrap().data;
    for columns in [0x42, 0x62] {
        let record = [1, 0, columns, 0, 3, 1];
        assert!(psv.windows(record.len()).any(|w| w == record));
    }
    let inputs = c.part(FourCC::ISG1).unwrap().data;
    assert_eq!((inputs[8 + 25], inputs[8 + 32 + 25]), (0, 4));
    validate(&shader);
}

/// A pixel program's first-lane loop (no table entry in it, so not a waterfall the tracker
/// knows) asks for helper lanes, which is valid DXIL (upstream #946's program).
#[test]
fn a_pixel_first_lane_loop_serves_helper_lanes() {
    let shader = pixel(
        &[
            0xD765_0000, 0x0001_00C1, // v_mbcnt_lo_u32_b32 v0, -1, 0
            0x7E02_0280, // v_mov_b32 v1, 0
            0xBE97_037E, // s_mov_b32 s23, exec_lo
            0x7E18_0500, // v_readfirstlane_b32 s12, v0
            0x7D84_000C, // v_cmp_eq_u32 vcc, s12, v0
            0xBE95_3C6A, // s_and_saveexec_b32 s21, vcc_lo
            0xBF80_0000, // s_nop 0
            0x7E02_02FF, 0x3F80_0000, // v_mov_b32 v1, 1.0
            0x897E_157E, // s_xor_b32 exec_lo, exec_lo, s21
            0xBF89_FFF8, // s_cbranch_execnz loop
            0xBEFE_0317, // s_mov_b32 exec_lo, s23
            0xF800_180F, 0x0101_0101, // exp mrt0 v1 v1 v1 v1 done vm
            0xBF81_0000, // s_endpgm
        ],
        0,
    )
    .unwrap();
    validate(&shader);
}

/// The translation's `SFI0` features and `ISG1` rows.
fn parts(shader: &HostShader) -> (u64, Vec<SigElement>) {
    let c = Container::parse(&shader.dxil).unwrap();
    let sfi0 = c.part(FourCC::SFI0).unwrap().data.try_into().unwrap();
    (
        u64::from_le_bytes(sfi0),
        signature(c.part(FourCC::ISG1).unwrap().data).unwrap(),
    )
}

fn validate(shader: &HostShader) {
    #[cfg(windows)]
    if let Ok(mut v) =
        validator::Validator::load(validator::Validator::default_path().as_deref())
    {
        v.validate(&mut shader.dxil.clone()).unwrap();
    }
    let _ = shader;
}

/// Custom interpolation reads I and J (v0, v1 with PERSP_CENTER) as
/// plain floats: they are channels y and z of `SV_Barycentrics`, the
/// weights of vertices 1 and 2.
#[test]
fn barycentrics_read_as_values_are_sv_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    let words = [v_add_f32(2, 0, 1), e0, e1, 0xBF81_0000];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
    let bary: Vec<_> = inputs.iter().filter(|e| e.kind == 28).collect();
    assert_eq!(bary.len(), 1, "{inputs:?}");
    assert_eq!((bary[0].index, bary[0].col, bary[0].cols), (0, 0, 3));
    // Not packed: the validator wants packing location -1.
    assert_eq!(bary[0].row, None);
    // Only y and z (I and J) are read.
    let isg1 = Container::parse(&shader.dxil).unwrap();
    let isg1 = isg1.part(FourCC::ISG1).unwrap().data;
    let rw: Vec<u8> = (0..2).map(|k| isg1[8 + 32 * k + 25]).collect();
    assert!(rw.contains(&0b110), "{rw:?}");
    validate(&shader);
}

/// Two kinds of barycentrics read directly: one element per mode.
#[test]
fn two_barycentric_modes_get_two_elements() {
    // v0-v1 perspective i, j; v2-v3 linear i, j.
    let [e0, e1] = exp_mrt0(4);
    let words = [v_add_f32(4, 0, 3), e0, e1, 0xBF81_0000];
    let shader = pixel(&words, PERSP_CENTER | LINEAR_CENTER).unwrap();
    let (_, inputs) = parts(&shader);
    let mut indices: Vec<u32> = inputs
        .iter()
        .filter(|e| e.kind == 28)
        .map(|e| e.index)
        .collect();
    indices.sort();
    assert_eq!(indices, [0, 1], "{inputs:?}");
    validate(&shader);
}

/// I and J only fed to `v_interp_*` stay host interpolation: no
/// barycentrics are declared.
#[test]
fn interpolation_alone_declares_no_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        e0,
        e1,
        0xBF81_0000,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_eq!(features & 0x2_0000, 0);
    assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
    validate(&shader);
}

#[test]
fn a_standalone_first_half_reads_the_vertices() {
    let shader = first_half_pixel(false);
    let (_, inputs) = parts(&shader);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

#[test]
fn a_first_half_read_beside_its_pair_keeps_its_partial_value() {
    let shader = first_half_pixel(true);
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

fn first_half_pixel(paired: bool) -> HostShader {
    let [e0, e1] = exp_mrt0(2);
    let mut words = vec![0xC808_0000]; // v_interp_p1_f32 v2, v0, attr0.x
    if paired {
        words.extend([
            0x7E06_0302, // v_mov_b32 v3, v2 (keep the partial value)
            0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
            v_add_f32(2, 2, 3),
        ]);
    }
    words.extend([e0, e1, 0xBF81_0000]);
    pixel(&words, PERSP_CENTER).unwrap()
}

#[test]
#[cfg(windows)]
fn first_half_values_render_on_hardware_and_warp() {
    use crate::emit::bitcode::{Attr, Term};
    use crate::emit::host::{Draw, Host};
    let _one = super::valu_helpers::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Pass POSITION and TEXCOORD through, with TEXCOORD in the same
    // output row as the translated pixel shader's input signature.
    let mut s = Shader::new(false);
    s.inputs = vec![element("POSITION", 0, 0), element("TEXCOORD", 0, 1)];
    for e in &mut s.inputs {
        e.usage = 15;
    }
    s.outputs = vec![element("TEXCOORD", 0, 0), element("SV_Position", 3, 1)];
    for (i, e) in s.outputs.iter_mut().enumerate() {
        e.usage = 15;
        e.interp = if i == 1 { 4 } else { 2 };
    }
    for row in 0..2 {
        for col in 0..4 {
            let args = [s.m.i32(row), s.m.i32(0), s.m.i8(col), s.m.undef(s.t.i32)];
            let v = s.op(s.entry, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args);
            let args = [s.m.i32(1 - row), s.m.i32(0), s.m.i8(col), v];
            s.op(
                s.entry,
                "storeOutput.f32",
                Attr::NoUnwind,
                s.t.void,
                5,
                &args,
            );
        }
    }
    s.m.terminate(s.entry, Term::Ret);
    let vs = HostShader {
        stage: HwStage::Vs,
        dxil: s.finish(),
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        root_uavs: Vec::new(),
        root_indexed_views: Vec::new(),
    };
    validate(&vs);
    // One triangle covers an 8x8 target, with attribute values 2, 6, 10.
    let vertices = [
        -1.0, 1.0, 0.5, 1.0, 2.0, 0.0, 0.0, 0.0, -1.0, -3.0, 0.5, 1.0, 6.0, 0.0, 0.0, 0.0, 3.0,
        1.0, 0.5, 1.0, 10.0, 0.0, 0.0, 0.0,
    ];
    let mut ran = 0;
    for (backend, warp) in [("hardware", false), ("WARP", true)] {
        let Ok(host) = Host::new(warp) else { continue };
        let mut options =
            windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
        // SAFETY: a correctly sized output for the feature query.
        unsafe {
            host.device
                .CheckFeatureSupport(
                    windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_D3D12_OPTIONS3,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .unwrap();
        }
        if !options.BarycentricsSupported.as_bool() {
            eprintln!("no barycentrics on {backend}; skipped");
            continue;
        }
        for paired in [false, true] {
            let ps = first_half_pixel(paired);
            validate(&ps);
            let (root, pso) = host.pipeline(&vs, &ps).expect("interpolation pipeline");
            let out = host
                .draw(
                    &root,
                    &pso,
                    &Draw {
                        roots: Vec::new(),
                        textures: &[],
                        samplers: &[],
                        vertices: &vertices,
                        stride: 32,
                        size: (8, 8),
                        mesh_groups: 0,
                    },
                )
                .expect("draw partial interpolation");
            for (n, texel) in out.iter().enumerate() {
                let i = (n / 8) as f32 / 16.0 + 0.5 / 16.0;
                let j = (n % 8) as f32 / 16.0 + 0.5 / 16.0;
                let partial = 2.0 + i * 4.0;
                let want = if paired {
                    partial + 2.0 + i * 4.0 + j * 8.0
                } else {
                    partial
                };
                for &got in texel {
                    assert!(
                        (got - want).abs() < 1e-5,
                        "{backend} paired={paired} pixel={n}: got {got}, want {want}"
                    );
                }
            }
        }
        ran += 1;
        eprintln!("partial interpolation renders correctly on {backend}");
    }
    if ran == 0 {
        eprintln!("no SM 6.6 host with barycentrics; GPU checks skipped");
    }
}

/// Both: the attribute is interpolated and J is used as a value.
#[test]
fn interpolation_and_a_barycentric_value_together() {
    let [e0, e1] = exp_mrt0(3);
    let words = [
        0xC808_0000,        // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001,        // v_interp_p2_f32 v2, v1, attr0.x
        v_add_f32(3, 2, 1), // v3 = attribute + j
        e0,
        e1,
        0xBF81_0000,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    validate(&shader);
}
/// The `PSV0` interpolation mode of TEXCOORD0 (row 0, x-w, float).
fn texcoord_mode(shader: &HostShader) -> u8 {
    let c = Container::parse(&shader.dxil).unwrap();
    let psv = c.part(FourCC::PSV0).unwrap().data;
    let row = [1, 0, 0x44, 0, 3];
    let at = psv.windows(5).position(|w| w == row).unwrap();
    psv[at + 5]
}

const S_ENDPGM: u32 = 0xBF81_0000;

/// An interpolation whose i is computed (`v_mul_f32 v3, 0.5, v0`) reads
/// the attribute's vertices: TEXCOORD0 becomes constant, read with
/// `attributeAtVertex`; i and j are `SV_Barycentrics` values.
#[test]
fn computed_barycentrics_read_the_vertices() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0x1006_00F0, // v_mul_f32 v3, 0.5, v0
        0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

/// Weights from constants alone: the vertices, no `SV_Barycentrics`.
#[test]
fn constant_weights_need_no_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0x7E06_02F0, // v_mov_b32 v3, 0.5
        0xC808_0003, // v_interp_p1_f32 v2, v3, attr0.x
        0xC809_0003, // v_interp_p2_f32 v2, v3, attr0.x
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
    assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

/// The same attribute interpolated with the loaded i and j and with a
/// computed j: both from its vertices, the first weighed with
/// `SV_Barycentrics`.
#[test]
fn computed_and_loaded_barycentrics_on_one_attribute() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0xC810_0100, // v_interp_p1_f32 v4, v0, attr0.y
        0xC811_0101, // v_interp_p2_f32 v4, v1, attr0.y
        0x1006_02F0, // v_mul_f32 v3, 0.5, v1
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0003, // v_interp_p2_f32 v2, v3, attr0.x
        v_add_f32(2, 2, 4),
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (_, inputs) = parts(&shader);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
}

/// P10 of an attribute the program interpolates is P1 - P0.
#[test]
fn p10_of_an_interpolated_attribute() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        0xC812_0000, // v_interp_mov_f32 v4, p10, attr0.x
        v_add_f32(2, 2, 4),
        e0,
        e1,
        S_ENDPGM,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    assert_eq!(texcoord_mode(&shader), 1);
    validate(&shader);
    // Alone, what the hardware holds is not known.
    let [e0, e1] = exp_mrt0(4);
    let words = [0xC812_0000, e0, e1, S_ENDPGM];
    assert!(pixel(&words, PERSP_CENTER).is_err());
}
