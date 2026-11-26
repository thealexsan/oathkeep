//! The ray-tracing node test end to end: a synthetic compute program that
//! runs `image_bvh64_intersect_ray` (or the 32-bit form) once per thread,
//! lowered to DXIL, validated by `dxil.dll` when present, run on the host
//! GPU and WARP, and compared dword for dword with the CPU reference
//! ([`crate::bvh`]).
//!
//! The nodes are in a raw buffer. Two ways to name it are run: the
//! buffer's descriptor from the program's table (a resource of the host,
//! [`crate::resource::BvhNodes::Resource`]), and the title's way, a
//! descriptor of numbers over title memory with the structure's address in
//! the node pointer or in the descriptor. These tests have no host that
//! keeps copies of title memory and their directory, so for the second
//! they put a stand-in under the node reader
//! (`lower::intersect::TEST_MEMORY`); without it the same program reads
//! through the directory, which is only checked to lower and validate.

use super::super::{HostShader, lower, validator};
use crate::bvh::{self};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::resource::BvhNodes;
use crate::{ShaderError, io, ir, resource, structure};

/// Threads of one group, each with its own record.

use super::intersect_scene::{Memory, run};

pub(super) const THREADS: u32 = 32;
/// Bytes of a thread's record: the node pointer (two dwords), the ray's
/// extent, origin, direction and inverse direction, and whether the
/// thread runs the instruction.
pub(super) const RECORD: u32 = 64;
/// Where the nodes start in the buffer.
pub(super) const NODES: u32 = THREADS * RECORD;
/// What the result registers hold before the instruction.
pub(super) const BEFORE: [u32; 4] = [1, 2, 3, 4];

/// How the program names the nodes' memory.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Descriptor {
    /// The buffer's descriptor from the table.
    Table,
    /// Numbers, as the title builds it: dwords 0 and 1 (the base address
    /// in 256-byte units and the box sorting bit); the size and the rest of
    /// the flags as the title sets them.
    Numbers(u32, u32),
}

/// The descriptor the title builds for the 64-bit form: the whole address
/// space, sorted boxes, barycentrics returned.
pub(super) const WHOLE: Descriptor = Descriptor::Numbers(0, bvh::BOX_SORT);

/// The program: per thread, the record's twelve operand dwords in v1-v12,
/// the instruction's result in v13-v16 (under a lane mask of the threads
/// whose record says so), stored to the UAV at 16 bytes a thread.
pub(super) fn program(wide: bool, descriptor: Descriptor) -> Vec<u32> {
    let mut w = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
        0x3428_0086, // v_lshlrev_b32 v20, 6, v0
        0xE038_1000,
        0x8002_0114, // buffer_load_dwordx4 v[1:4], v20, s[8:11], 0 offen
        0xE038_1010,
        0x8002_0514, // buffer_load_dwordx4 v[5:8], v20, s[8:11], 0 offen offset:16
        0xE038_1020,
        0x8002_0914, // buffer_load_dwordx4 v[9:12], v20, s[8:11], 0 offen offset:32
        0xE030_1030,
        0x8002_1114, // buffer_load_dword v17, v20, s[8:11], 0 offen offset:48
    ];
    for k in 0..4u32 {
        // v_mov_b32 v13+k, 1+k
        w.push(0x7E00_0200 | (13 + k) << 17 | (0x81 + k));
    }
    let rsrc = match descriptor {
        Descriptor::Table => 8,
        Descriptor::Numbers(word0, word1) => {
            // s_mov_b32 s24, word0
            w.extend([0xBE98_03FF, word0]);
            if word1 == bvh::BOX_SORT {
                // As the title makes it.
                w.push(0xBE99_0B81); // s_brev_b32 s25, 1
            } else {
                w.extend([0xBE99_03FF, word1]); // s_mov_b32 s25, word1
            }
            w.extend([
                0xBE9A_03C1, // s_mov_b32 s26, -1
                0xBE9B_03FF,
                0x8900_03FF, // s_mov_b32 s27, 0x890003ff
            ]);
            24
        }
    };
    w.extend([
        0xBE94_037E, // s_mov_b32 s20, exec_lo
        0x7DAA_2280, // v_cmpx_ne_u32 exec_lo, 0, v17
    ]);
    let first = 0x0000_0D01 | (rsrc >> 2) << 16;
    if wide {
        // image_bvh64_intersect_ray v[13:16], [v1 .. v12], s[rsrc:rsrc+3]
        w.extend([0xF19C_9F07, first, 0x0504_0302, 0x0908_0706, 0x000C_0B0A]);
    } else {
        // image_bvh_intersect_ray v[13:16], [v1, v3 .. v12], s[rsrc:rsrc+3]
        w.extend([0xF198_9F07, first, 0x0605_0403, 0x0A09_0807, 0x0000_0C0B]);
    }
    w.extend([
        0xBEFE_0314, // s_mov_b32 exec_lo, s20
        0x342A_0084, // v_lshlrev_b32 v21, 4, v0
        0xE078_1000,
        0x8003_0D15, // buffer_store_dwordx4 v[13:16], v21, s[12:15], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    w
}

pub(super) fn native(words: &[u32]) -> NativeProgram {
    let table = |kind, at| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 4,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 22,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![table(ResourceType::Srv, 0), table(ResourceType::Uav, 8)],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [THREADS, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// Lowers the program; the DXIL is validated when the validator is there.
pub(super) fn build(
    wide: bool,
    descriptor: Descriptor,
) -> Result<(HostShader, resource::Resources), ShaderError> {
    build_compute_shader(&native(&program(wide, descriptor)))
}

/// `native`, a compute program, translated (the tests' own way: no container).
pub(super) fn build_compute_shader(
    native: &NativeProgram,
) -> Result<(HostShader, resource::Resources), ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })?;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views.clone(),
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    Ok((shader, res))
}

/// One thread's test: a node (by its index in the node list), the kind its
/// pointer carries, and the ray; `None` leaves the thread masked out.
#[test]
fn the_64_bit_node_test_in_a_host_buffer_matches_the_reference_on_the_card() {
    // Pointers count from the buffer's start.
    run(true, Descriptor::Table, u64::from(NODES), true);
    assert_eq!(
        build(true, Descriptor::Table).unwrap().1.bvh[0].1,
        BvhNodes::Resource
    );
}

#[test]
fn the_32_bit_node_test_in_a_host_buffer_matches_the_reference_on_the_card() {
    run(false, Descriptor::Table, u64::from(NODES), true);
}

/// Where the tests' title memory puts the buffer: past 4 GiB, on 256 bytes.
const BASE: u64 = 0x12_3456_7800;

#[test]
fn the_64_bit_node_test_over_title_memory_matches_the_reference_on_the_card() {
    // The title's way: one descriptor for all of memory, the structure's
    // address folded into a 64-bit pointer.
    let _memory = Memory::at(BASE);
    run(true, WHOLE, BASE + u64::from(NODES), true);
}

#[test]
fn the_32_bit_node_test_over_title_memory_matches_the_reference_on_the_card() {
    // The title's other way: the structure's address in the descriptor
    // (in 256-byte units, 40 bits), a 32-bit pointer from there. Here with
    // an address past 2^40 bytes, and without box sorting.
    let base = 0x312_3456_7800u64;
    let structure = base + u64::from(NODES);
    let _memory = Memory::at(base);
    let descriptor = Descriptor::Numbers((structure >> 8) as u32, (structure >> 40) as u32);
    run(false, descriptor, 0, false);
    // And sorted.
    let descriptor = Descriptor::Numbers(
        (structure >> 8) as u32,
        (structure >> 40) as u32 | bvh::BOX_SORT,
    );
    run(false, descriptor, 0, true);
}

#[test]
fn a_descriptor_of_numbers_is_title_memory_read_through_the_directory() {
    for wide in [true, false] {
        let native = native(&program(wide, WHOLE));
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("tracked");
        // The flags the title sets: sorted boxes, barycentrics, a
        // ray-tracing structure. No resource is taken for it: the buffer
        // loads' and the store's descriptors are all there is.
        assert_eq!(res.bvh.len(), 1);
        assert_eq!(
            res.bvh[0].1,
            BvhNodes::Memory {
                word1: 0x8000_0000,
                word3: 0x8900_03ff
            }
        );
        assert_eq!(res.descriptors.len(), 5);
        // Without the tests' stand-in the nodes are read as the title's
        // programs will read them: through the host's directory of title
        // memory. The program lowers and validates, and asks the host for
        // the directory and for the list of pages it found no copy of.
        let (shader, _) = build(wide, WHOLE).expect("lowers through the directory");
        assert!(shader.root_srvs.contains(&super::super::SLOT_MEMORY));
        assert!(shader.root_uavs.contains(&super::super::SLOT_FAULTS));
    }
}

#[test]
fn a_structure_that_returns_triangle_ids_is_refused_by_name() {
    // Triangle return mode 0 (bit 24 of dword 3 clear): the last two
    // result dwords would be a triangle id and a hit status.
    let mut words = program(true, WHOLE);
    let flags = words.iter().position(|&w| w == 0x8900_03FF).unwrap();
    words[flags] = 0x8800_03FF;
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("tracked");
    let io = io::raise(&p, &native).expect("io::raise");
    let r = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    match r {
        Err(ShaderError::Unsupported { what, .. }) => {
            assert_eq!(what, "a ray-tracing structure that returns triangle ids");
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}
