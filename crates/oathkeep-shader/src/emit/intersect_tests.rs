use super::host::{Dispatch, Host, Root, Stride, View};
use super::lower::intersect::TEST_MEMORY;
use super::{HostShader, lower, validator};
use crate::bvh::{self, Ray, tests as nodes};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::resource::BvhNodes;
use crate::{ShaderError, io, ir, resource, structure};

const THREADS: u32 = 32;
const RECORD: u32 = 64;
const NODES: u32 = THREADS * RECORD;
const BEFORE: [u32; 4] = [1, 2, 3, 4];

#[derive(Clone, Copy, PartialEq)]
enum Descriptor {
    Table,
    Numbers(u32, u32),
}

const WHOLE: Descriptor = Descriptor::Numbers(0, bvh::BOX_SORT);

fn program(wide: bool, descriptor: Descriptor) -> Vec<u32> {
    let mut w = vec![
        0xBE85_0380,
        0xF408_0202,
        0xFA00_0000,
        0xF408_0302,
        0xFA00_0020,
        0x3428_0086,
        0xE038_1000,
        0x8002_0114,
        0xE038_1010,
        0x8002_0514,
        0xE038_1020,
        0x8002_0914,
        0xE030_1030,
        0x8002_1114,
    ];
    for k in 0..4u32 {
        w.push(0x7E00_0200 | (13 + k) << 17 | (0x81 + k));
    }
    let rsrc = match descriptor {
        Descriptor::Table => 8,
        Descriptor::Numbers(word0, word1) => {
            w.extend([0xBE98_03FF, word0]);
            if word1 == bvh::BOX_SORT {
                w.push(0xBE99_0B81);
            } else {
                w.extend([0xBE99_03FF, word1]);
            }
            w.extend([0xBE9A_03C1, 0xBE9B_03FF, 0x8900_03FF]);
            24
        }
    };
    w.extend([0xBE94_037E, 0x7DAA_2280]);
    let first = 0x0000_0D01 | (rsrc >> 2) << 16;
    if wide {
        w.extend([0xF19C_9F07, first, 0x0504_0302, 0x0908_0706, 0x000C_0B0A]);
    } else {
        w.extend([0xF198_9F07, first, 0x0605_0403, 0x0A09_0807, 0x0000_0C0B]);
    }
    w.extend([
        0xBEFE_0314,
        0x342A_0084,
        0xE078_1000,
        0x8003_0D15,
        0xBF81_0000,
    ]);
    w
}

fn native(words: &[u32]) -> NativeProgram {
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

fn build(
    wide: bool,
    descriptor: Descriptor,
) -> Result<(HostShader, resource::Resources), ShaderError> {
    let native = native(&program(wide, descriptor));
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
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

type Case = Option<(usize, u32, Ray)>;

struct Scene {
    words: Vec<u32>,
    starts: Vec<usize>,
}

impl Scene {
    fn new() -> Self {
        Self {
            words: Vec::new(),
            starts: Vec::new(),
        }
    }

    fn add(&mut self, node: [u32; 28], dwords: usize) -> usize {
        self.starts.push(self.words.len());
        let mut padded = [0u32; 32];
        padded[..28].copy_from_slice(&node);
        for w in padded.iter_mut().skip(if dwords == 16 { 16 } else { 28 }) {
            *w = 0x7fc0_dead;
        }
        self.words.extend(&padded[..dwords]);
        self.starts.len() - 1
    }

    fn pointer(&self, n: usize, kind: u32) -> u32 {
        (4 * self.starts[n] as u32) >> 3 | kind
    }

    fn node(&self, n: usize) -> [u32; 28] {
        std::array::from_fn(|k| self.words.get(self.starts[n] + k).copied().unwrap_or(0))
    }
}

struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn unit(&mut self) -> f32 {
        (self.next() % 8193) as f32 / 4096.0 - 1.0
    }

    fn vector(&mut self, scale: f32) -> [f32; 3] {
        [
            self.unit() * scale,
            self.unit() * scale,
            self.unit() * scale,
        ]
    }
}

fn scene() -> (Scene, Vec<Vec<Case>>) {
    let mut s = Scene::new();
    let mut g = Numbers(0x5eed);
    const UNIT: [f32; 6] = [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
    let moved =
        |b: [f32; 6], by: [f32; 3]| -> [f32; 6] { std::array::from_fn(|k| b[k] + by[k % 3]) };
    let off = [f32::NAN; 6];
    let mut box_nodes = Vec::new();
    let lined = [
        moved(UNIT, [30.0, 0.0, 0.0]),
        moved(UNIT, [10.0, 0.0, 0.0]),
        moved(UNIT, [10.0, 5.0, 0.0]),
        moved(UNIT, [20.0, 0.0, 0.0]),
    ];
    let ids = [0x105, 0x20c, 0x304, 0x400];
    box_nodes.push((s.add(nodes::box32(ids, lined), 32), 5));
    box_nodes.push((s.add(nodes::box16(ids, lined), 16), 4));
    let around = [
        UNIT,
        moved(UNIT, [0.5, 0.5, 0.5]),
        off,
        [-4.0, -0.25, -0.25, 4.0, 0.25, 0.25],
    ];
    let ids2 = [8, bvh::NONE, 24, 0x1fff_fffc];
    box_nodes.push((s.add(nodes::box32(ids2, around), 32), 5));
    box_nodes.push((s.add(nodes::box16(ids2, around), 16), 4));
    for n in 0..6 {
        let boxes: [[f32; 6]; 4] = std::array::from_fn(|_| {
            let (c, h) = (g.vector(8.0), g.vector(2.0).map(f32::abs));
            std::array::from_fn(|k| c[k % 3] + if k < 3 { -h[k] } else { h[k - 3] })
        });
        let ids: [u32; 4] = std::array::from_fn(|k| 0x1000 * (n + 1) + 8 * k as u32 + 5);
        if n % 2 == 0 {
            box_nodes.push((s.add(nodes::box32(ids, boxes), 32), 5));
        } else {
            box_nodes.push((s.add(nodes::box16(ids, boxes), 16), 4));
        }
    }
    let mut triangle_nodes = Vec::new();
    let front = [[0.0, 2.0, 5.0], [2.0, -2.0, 5.0], [-2.0, -2.0, 5.0]];
    triangle_nodes.push((s.add(nodes::triangles(&front, 9), 16), 0));
    let pair = s.add(
        nodes::triangles(
            &[front[0], front[1], front[2], [2.0, 2.0, 5.0]],
            9 | 0x80 | 6 << 8,
        ),
        16,
    );
    triangle_nodes.extend([(pair, 0), (pair, 1)]);
    let five = [
        [-3.0, -1.0, 6.0],
        [-1.0, 2.0, 6.5],
        [1.0, -1.0, 6.0],
        [3.0, 2.0, 6.5],
        [5.0, -1.0, 7.0],
    ];
    let full = s.add(nodes::triangles(&five, 9 | 2 << 8 | 4 << 16 | 9 << 24), 16);
    triangle_nodes.extend((0..4).map(|k| (full, k)));
    for _ in 0..5 {
        let c = g.vector(6.0);
        let v: [[f32; 3]; 3] = std::array::from_fn(|_| {
            let d = g.vector(3.0);
            [c[0] + d[0], c[1] + d[1], c[2] + d[2]]
        });
        triangle_nodes.push((s.add(nodes::triangles(&v, 9), 16), 0));
    }
    let mut cases: Vec<Case> = Vec::new();
    for &(n, kind) in &box_nodes[..2] {
        for (origin, direction, extent) in [
            ([0.0; 3], [1.0, 0.0, 0.0], 1000.0),
            ([0.0; 3], [1.0, 0.0, 0.0], 19.5),
            ([0.0; 3], [1.0, 0.0, 0.0], 19.0),
            ([0.0; 3], [-1.0, 0.0, 0.0], 1000.0),
            ([40.0, 0.0, 0.0], [-2.0, 0.0, 0.0], 1000.0),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], 1000.0),
            ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], 1000.0),
            ([0.0, 0.5, 0.25], [1.0, -0.0, 0.0], 1000.0),
            ([0.0; 3], [0.0; 3], 1000.0),
        ] {
            cases.push(Some((n, kind, Ray::new(origin, direction, extent))));
        }
    }
    for &(n, kind) in &box_nodes {
        let words = s.node(n);
        let boxes = if kind == 5 {
            bvh::boxes32(&words)
        } else {
            bvh::boxes16(&words)
        };
        for k in 0..12 {
            let origin = g.vector(12.0);
            let direction = if k % 2 == 0 {
                let b = boxes[(k / 2) % 4];
                if b[0].is_nan() {
                    g.vector(1.0)
                } else {
                    std::array::from_fn(|a| (b[a] + b[3 + a]) / 2.0 - origin[a] + g.unit() / 2.0)
                }
            } else {
                g.vector(1.0)
            };
            let extent = if k % 3 == 0 { 6.0 } else { 100.0 };
            cases.push(Some((n, kind, Ray::new(origin, direction, extent))));
        }
    }
    for &(n, kind) in &triangle_nodes {
        let p = bvh::triangle_vertices(&s.node(n), kind);
        let at = |w: [f32; 3]| -> [f32; 3] {
            std::array::from_fn(|a| w[0] * p[0][a] + w[1] * p[1][a] + w[2] * p[2][a])
        };
        for (k, w) in [
            [0.5, 0.25, 0.25],
            [0.125, 0.75, 0.125],
            [0.5, 0.5, 0.0],
            [0.0, 0.5, 0.5],
            [1.0, 0.0, 0.0],
            [0.75, 0.75, -0.5],
            [-0.25, 0.5, 0.75],
            [0.25, 0.25, 0.5],
        ]
        .into_iter()
        .enumerate()
        {
            let target = at(w);
            let origin = if k % 2 == 0 { [0.0; 3] } else { g.vector(16.0) };
            let mut direction: [f32; 3] = std::array::from_fn(|a| target[a] - origin[a]);
            if k == 7 {
                direction = direction.map(|d| -d);
            }
            cases.push(Some((n, kind, Ray::new(origin, direction, 100.0))));
        }
        for axis in 0..3 {
            for sign in [1.0f32, -1.0] {
                let target = at([0.25, 0.5, 0.25]);
                let mut origin = target;
                origin[axis] -= 8.0 * sign;
                let mut direction = [0.0; 3];
                direction[axis] = sign;
                cases.push(Some((n, kind, Ray::new(origin, direction, 100.0))));
            }
        }
    }
    let ray = Ray::new([0.0; 3], [0.0, 0.0, 1.0], 100.0);
    cases.extend([
        Some((triangle_nodes[0].0, 6, ray)),
        Some((triangle_nodes[0].0, 7, ray)),
        None,
        Some((
            box_nodes[0].0,
            5,
            Ray::new([0.0; 3], [1.0, 0.0, 0.0], 1000.0),
        )),
        None,
    ]);
    let batches = cases
        .chunks(THREADS as usize)
        .map(<[Case]>::to_vec)
        .collect();
    (s, batches)
}

fn buffer(scene: &Scene, batch: &[Case], base: u64) -> Vec<u32> {
    let mut data = vec![0u32; (NODES / 4) as usize];
    for (t, case) in batch.iter().enumerate() {
        let Some((n, kind, ray)) = case else { continue };
        let r = &mut data[t * (RECORD / 4) as usize..][..13];
        let pointer = (base >> 3) + u64::from(scene.pointer(*n, *kind));
        (r[0], r[1]) = (pointer as u32, (pointer >> 32) as u32);
        r[2] = ray.extent.to_bits();
        for a in 0..3 {
            r[3 + a] = ray.origin[a].to_bits();
            r[6 + a] = ray.direction[a].to_bits();
            r[9 + a] = ray.inverse[a].to_bits();
        }
        r[12] = 1;
    }
    data.extend(&scene.words);
    data
}

fn expected(scene: &Scene, case: &Case, sort: bool) -> [u32; 4] {
    match case {
        Some((n, kind, ray)) => bvh::intersect(&scene.node(*n), *kind, ray, sort),
        None => BEFORE,
    }
}

struct Memory;

impl Memory {
    fn at(base: u64) -> Self {
        TEST_MEMORY.set(Some((0, base)));
        Self
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        TEST_MEMORY.set(None);
    }
}

fn run(wide: bool, descriptor: Descriptor, pointers: u64, sort: bool) {
    let (shader, res) = build(wide, descriptor).expect("the program lowers");
    assert_eq!(res.bvh.len(), 1);
    let (scene, batches) = scene();
    let all: Vec<&Case> = batches.iter().flatten().collect();
    let results: Vec<[u32; 4]> = all.iter().map(|c| expected(&scene, c, sort)).collect();
    let boxes = |hits: usize| {
        all.iter()
            .zip(&results)
            .filter(|(c, r)| {
                matches!(c, Some((_, 4 | 5, _)))
                    && r.iter().filter(|&&x| x != bvh::NONE).count() == hits
            })
            .count()
    };
    let triangles = |hit: bool| {
        all.iter()
            .zip(&results)
            .filter(|(c, r)| {
                matches!(c, Some((_, 0..=3, _))) && (r[0] != f32::INFINITY.to_bits()) == hit
            })
            .count()
    };
    assert!(
        boxes(0) > 5 && boxes(1) > 5 && boxes(2) > 5 && boxes(3) > 0,
        "box cases"
    );
    assert!(
        triangles(true) > 30 && triangles(false) > 30,
        "triangle cases"
    );
    assert!(
        all.iter()
            .any(|c| expected(&scene, c, true) != expected(&scene, c, false)),
        "no case tells sorted from unsorted"
    );
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        for (b, batch) in batches.iter().enumerate() {
            let data = buffer(&scene, batch, pointers);
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(vec![0; 5]))],
                        views: &[
                            View::Buffer {
                                data: &data,
                                stride: Stride::Raw,
                            },
                            View::RawUav(4 * THREADS),
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            for t in 0..THREADS as usize {
                let case = batch.get(t).copied().flatten();
                let want = match batch.get(t) {
                    Some(c) => expected(&scene, c, sort),
                    None => BEFORE,
                };
                let got = &out[0][4 * t..4 * t + 4];
                assert_eq!(
                    got,
                    want,
                    "{label}, batch {b} thread {t}: {case:x?}\n got {:x?} = {:?}\nwant {:x?} = {:?}",
                    got,
                    got.iter().map(|&x| f32::from_bits(x)).collect::<Vec<_>>(),
                    want,
                    want.map(f32::from_bits),
                );
            }
        }
        ran += 1;
    }
    assert!(ran > 0, "no SM 6.6 device available");
}

#[test]
fn the_64_bit_node_test_in_a_host_buffer_matches_the_reference_on_the_card() {
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

const BASE: u64 = 0x12_3456_7800;

#[test]
fn the_64_bit_node_test_over_title_memory_matches_the_reference_on_the_card() {
    let _memory = Memory::at(BASE);
    run(true, WHOLE, BASE + u64::from(NODES), true);
}

#[test]
fn the_32_bit_node_test_over_title_memory_matches_the_reference_on_the_card() {
    let base = 0x312_3456_7800u64;
    let structure = base + u64::from(NODES);
    let _memory = Memory::at(base);
    let descriptor = Descriptor::Numbers((structure >> 8) as u32, (structure >> 40) as u32);
    run(false, descriptor, 0, false);
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
        assert_eq!(res.bvh.len(), 1);
        assert_eq!(
            res.bvh[0].1,
            BvhNodes::Memory {
                word1: 0x8000_0000,
                word3: 0x8900_03ff
            }
        );
        assert_eq!(res.descriptors.len(), 5);
        let (shader, _) = build(wide, WHOLE).expect("lowers through the directory");
        assert!(shader.root_srvs.contains(&super::SLOT_MEMORY));
        assert!(shader.root_uavs.contains(&super::SLOT_FAULTS));
    }
}

#[test]
fn a_structure_that_returns_triangle_ids_is_refused_by_name() {
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
