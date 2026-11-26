use crate::dxbc::{Container, FourCC};
use crate::emit::container::SigElement;
use crate::emit::bitcode::Attr;
use crate::emit::shader::Shader;
use crate::emit::{container, emit};
use crate::native::{ComputeIo, RootParam, HwStage, NativeProgram, StageIo};
use crate::{ShaderError, io, ir, resource, structure};

use super::mesh::{TRIANGLE, triangle_mesh_to};

/// A wave64 compute program: `body`, then a store of v1 to a UAV.
fn stores_v1(body: &[u32]) -> Result<(), ShaderError> {
    use crate::{ResourceType, RootType, UsedResource};
    // s_mov_b32 s1, 0; s_load_dwordx4 s[8:11], s[0:1], 0
    let mut words = vec![0xBE81_0380, 0xF408_0200, 0xFA00_0000];
    words.extend_from_slice(body);
    // v_lshlrev_b32 v3, 2, v0; buffer_store_dword v1, v3, s[8:11], 0 offen;
    // s_endpgm
    words.extend_from_slice(&[0x3406_0082, 0xE070_1000, 0x8002_0103, 0xBF81_0000]);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 4,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, None).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let st = structure::structure(&p).unwrap();
    let container = crate::dxbc::build(&[]);
    let c = Container::parse(&container).unwrap();
    emit(&c, &native, &p, &st, &io, &res).map(|_| ())
}

/// The high dword of a wave64 mask rewritten alone (`s_mov_b32 vcc_hi`
/// or `exec_hi`): lanes 32-63 took the mask the low dword was first
/// written with (the compare's, EXEC's on entry). Now the store refuses;
/// without the write it translates.
#[test]
fn a_mask_whose_high_dword_is_rewritten_is_refused() {
    // v_cmp_eq_u32 vcc, 0, v0; v_mov_b32 v2, 11; v_cndmask_b32 v1, 0, v2, vcc
    let select = [0x7D84_0080, 0x7E04_028B, 0x0202_0480];
    // The same with v_add_co_ci_u32 v1, vcc, 0, v2, vcc.
    let carry = [0x7D84_0080, 0x7E04_028B, 0x5002_0480];
    // v_mov_b32 v1, 7
    let mov = [0x7E02_0287];
    for body in [&select[..], &carry, &mov] {
        stores_v1(body).unwrap();
    }
    // s_mov_b32 vcc_hi, 0 after the compare.
    let select_hi = [0x7D84_0080, 0xBEEB_0380, 0x7E04_028B, 0x0202_0480];
    let carry_hi = [0x7D84_0080, 0xBEEB_0380, 0x7E04_028B, 0x5002_0480];
    // s_mov_b32 exec_hi, 0; v_mov_b32 v1, 7, then the same after
    // s_mov_b64 s[2:3], exec.
    let exec_hi = [0xBEFF_0380, 0x7E02_0287];
    let exec_hi_saved = [0xBE82_047E, 0xBEFF_0380, 0x7E02_0287];
    for body in [&select_hi[..], &carry_hi, &exec_hi, &exec_hi_saved] {
        assert!(stores_v1(body).is_err(), "{body:x?}");
    }
}

/// `ds_bpermute_b32` stores nothing: each lane reads the lane its address
/// names. It stopped as "LDS the program does not allocate" (sticky lips)
/// or "an LDS operation" (the checkerboard shadow masks).
#[test]
fn a_backward_permute_reads_the_lane_its_address_names() {
    // v_mov_b32 v2, 11; ds_bpermute_b32 v1, v0, v2
    stores_v1(&[0x7E04_028B, 0xDACC_0000, 0x0100_0200]).unwrap();
}

/// A carry-in reading a join of compares after a divergent branch. A
/// lane that skipped the inner compare has VCC clear there (VOPC clears
/// inactive lanes), not the first compare's bit, so the join is no
/// per-lane phi: lowering refuses it.
#[test]
fn a_carry_in_from_a_divergent_join_is_refused() {
    let body = [
        0x7E04_028B, // v_mov_b32 v2, 11
        0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
        0xBE82_376A, // s_andn1_saveexec_b64 s[2:3], vcc
        0xBF88_0001, // s_cbranch_execz 1
        0x7D84_0081, // v_cmp_eq_u32 vcc, 1, v0
        0x88FE_027E, // s_or_b64 exec, exec, s[2:3]
        0x5002_0480, // v_add_co_ci_u32 v1, vcc, 0, v2, vcc
    ];
    let e = stores_v1(&body).unwrap_err().to_string();
    assert!(e.contains("a carry-in from an untracked mask"), "{e}");
}

/// A pixel shader writing a constant colour.
#[cfg(windows)]
pub(super) fn constant_color() -> Vec<u8> {
    let mut s = Shader::new(true);
    s.outputs.push(SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    });
    let b = s.entry;
    for (c, v) in [1.0f32, 0.5, 0.25, 1.0].into_iter().enumerate() {
        let args = [
            s.m.i32(0),
            s.m.i32(0),
            s.m.i8(c as u8),
            s.m.f32(v.to_bits()),
        ];
        s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
    }
    s.m.terminate(b, crate::emit::bitcode::Term::Ret);
    s.finish()
}

#[test]
#[cfg(windows)]
fn stub_pipeline_is_created() {
    let host = match crate::emit::host::Host::new(false) {
        Ok(host) => host,
        Err(e) => {
            eprintln!("no SM 6.6 device ({e}); skipped");
            return;
        }
    };
    let ps = crate::emit::HostShader {
        stage: crate::HwStage::Ps,
        dxil: constant_color(),
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        root_uavs: Vec::new(),
        root_indexed_views: Vec::new(),
    };
    let file = |var: &str, stage| {
        std::env::var(var).ok().map(|p| crate::emit::HostShader {
            stage,
            dxil: std::fs::read(p).unwrap(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            root_indexed_views: Vec::new(),
        })
    };
    let ps = file("OATHKEEP_REF_PS", crate::HwStage::Ps).unwrap_or(ps);
    let vs = file("OATHKEEP_REF_VS", crate::HwStage::Vs)
        .unwrap_or_else(|| crate::emit::host::stub_vs(&ps).unwrap());
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut vs.dxil.clone()).unwrap();
    }
    host.pipeline(&vs, &ps).unwrap();
    // The runtime checks the container hash: no hash or a wrong one is
    // refused. (Recent runtimes also take sixteen 0x01 bytes, a bypass
    // value; ours works with older ones too.)
    let mut wrong = ps.dxil[4..20].to_vec();
    wrong[0] ^= 1;
    for digest in [&[0u8; 16][..], &wrong] {
        let mut other = ps.clone();
        other.dxil[4..20].copy_from_slice(digest);
        assert!(
            host.pipeline(&vs, &other).is_err(),
            "digest {digest:02x?} accepted"
        );
    }
}

#[test]
#[cfg(windows)]
fn constant_color_validates() {
    let mut c = constant_color();
    let path = crate::emit::validator::Validator::default_path();
    let Ok(mut v) = crate::emit::validator::Validator::load(path.as_deref()) else {
        eprintln!("no dxil.dll; skipped");
        return;
    };
    let ours = c[4..20].to_vec();
    v.validate(&mut c).unwrap();
    assert_eq!(c[4..20], ours[..], "our hash differs from the validator's");
}

/// A pixel program of `words` with no user data.
pub(super) fn emit_native(words: &[u32], input_addr: u32) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 4,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// A pixel program that takes its neighbour's position across its quad
/// (`ds_swizzle_b32`, as the console's compiler makes derivatives) and
/// exports the differences in x and y: 1 and -1 in every pixel of a
/// triangle, those at its edges too, whose neighbours are helpers.
#[test]
#[cfg(windows)]
fn quad_neighbours_at_a_triangles_edge_are_read() {
    use crate::emit::host::{Draw, Host, Root};
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_SHADER_VISIBILITY_MESH, D3D12_SHADER_VISIBILITY_PIXEL,
    };
    let _one = super::valu_helpers::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // v2, v3: position x, y (input address: POS_X and POS_Y after the
    // two perspective barycentrics).
    let swizzle = |dst: u32, src: u32, pattern: u32| {
        [
            0xD800_0000 | 0x35 << 18 | 0x8000 | pattern,
            dst << 24 | src,
        ]
    };
    let mut words = vec![0xBEFC_0380u32]; // s_mov_b32 m0, 0
    words.extend(swizzle(4, 2, 0xB1)); // lanes 1 0 3 2
    words.extend(swizzle(5, 3, 0x4E)); // lanes 2 3 0 1
    words.extend(swizzle(6, 2, 0xA0)); // lanes 0 0 2 2
    words.extend([
        0x0808_0504,  // v_sub_f32 v4, v4, v2
        0x080A_0705,  // v_sub_f32 v5, v5, v3
        0x080C_0506,  // v_sub_f32 v6, v6, v2
        0xF800_180F,
        0x0302_0504, // exp mrt0 v4, v5, v2, v3 done vm
        0xBF81_0000,
    ]);
    let _ = 6;
    let mut native = emit_native(&words, 1 << 1 | 1 << 8 | 1 << 9);
    native.vgprs = 8;
    let out = SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    };
    let position = SigElement {
        name: "SV_Position".into(),
        index: 0,
        kind: 3,
        comp: 9,
        interp: 4,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 3,
        half: false,
    };
    let isg1 = container::signature(&[position], true);
    let osg1 = container::signature(&[out], false);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let ps = emit(&c, &native, &p, &st, &io, &res).unwrap();
    if let Some(path) = crate::emit::validator::Validator::default_path() {
        let mut v = crate::emit::validator::Validator::load(Some(&path)).unwrap();
        v.validate(&mut ps.dxil.clone()).unwrap();
    }
    let ms = triangle_mesh_to(TRIANGLE, 1.3).unwrap();
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            continue;
        };
        let Ok(root) = host.root_signature_for(&[
            (&ms, D3D12_SHADER_VISIBILITY_MESH),
            (&ps, D3D12_SHADER_VISIBILITY_PIXEL),
        ]) else {
            continue;
        };
        let Ok(pso) = host.mesh_pipeline_with(&root, &ms.dxil, &ps.dxil) else {
            eprintln!("no mesh pipeline on {label}; skipped");
            continue;
        };
        let texels = host
            .draw(
                &root,
                &pso,
                &Draw {
                    roots: vec![(0, Root::Constants(vec![0])), (1, Root::Constants(vec![0]))],
                    textures: &[],
                    samplers: &[],
                    vertices: &[],
                    stride: 0,
                    size: (16, 16),
                    mesh_groups: 1,
                },
            )
            .unwrap_or_else(|e| panic!("{label}: draw: {e}"));
        let lit: Vec<(usize, &[f32; 4])> = texels.iter().enumerate().filter(|t| t.1[2] != 0.0).collect();
        assert!((20..240).contains(&lit.len()), "{label}: {} texels lit", lit.len());
        let bad: Vec<_> = lit
            .iter()
            .filter(|(n, t)| {
                let x = if n % 2 == 0 { 1.0 } else { -1.0 };
                let y = if n / 16 % 2 == 0 { 1.0 } else { -1.0 };
                (t[0], t[1]) != (x, y)
            })
            .collect();
        assert!(bad.is_empty(), "{label}: {} of {} texels: {:?}", bad.len(), lit.len(), bad[0]);
    }
}

