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

use super::super::host::{Dispatch, Host, Root, Stride, View};
use super::super::lower::intersect::TEST_MEMORY;
use crate::bvh::{self, Ray, tests as nodes};

/// Threads of one group, each with its own record.

use super::intersect::{BEFORE, NODES, RECORD, THREADS, build};
pub(super) use super::intersect::Descriptor;

pub(super) type Case = Option<(usize, u32, Ray)>;

/// The scene: node dwords (a 32-bit box node takes 32, the others 16) and
/// where each node starts, in dwords from the first node.
pub(super) struct Scene {
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

    /// Adds a node of `dwords` (16 or 32) and returns its index.
    fn add(&mut self, node: [u32; 28], dwords: usize) -> usize {
        self.starts.push(self.words.len());
        let mut padded = [0u32; 32];
        padded[..28].copy_from_slice(&node);
        // Bytes the instruction must not read as part of the node: what
        // follows a 64-byte node, and a box node's last 16 bytes.
        for w in padded.iter_mut().skip(if dwords == 16 { 16 } else { 28 }) {
            *w = 0x7fc0_dead;
        }
        self.words.extend(&padded[..dwords]);
        self.starts.len() - 1
    }

    /// The pointer of node `n` with `kind`, counted from the first node.
    fn pointer(&self, n: usize, kind: u32) -> u32 {
        (4 * self.starts[n] as u32) >> 3 | kind
    }

    /// What the instruction reads of node `n`.
    fn node(&self, n: usize) -> [u32; 28] {
        std::array::from_fn(|k| self.words.get(self.starts[n] + k).copied().unwrap_or(0))
    }
}

/// A small generator of repeatable numbers.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    /// A float in -1..1 in steps of 1/4096 (exactly representable).
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

/// The scene and batches of cases over it.
pub(super) fn scene() -> (Scene, Vec<Vec<Case>>) {
    let mut s = Scene::new();
    let mut g = Numbers(0x5eed);
    const UNIT: [f32; 6] = [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
    let moved =
        |b: [f32; 6], by: [f32; 3]| -> [f32; 6] { std::array::from_fn(|k| b[k] + by[k % 3]) };
    let off = [f32::NAN; 6];
    let mut box_nodes = Vec::new();
    // The boxes of the reference's own tests, in both widths.
    let lined = [
        moved(UNIT, [30.0, 0.0, 0.0]),
        moved(UNIT, [10.0, 0.0, 0.0]),
        moved(UNIT, [10.0, 5.0, 0.0]),
        moved(UNIT, [20.0, 0.0, 0.0]),
    ];
    let ids = [0x105, 0x20c, 0x304, 0x400];
    box_nodes.push((s.add(nodes::box32(ids, lined), 32), 5));
    box_nodes.push((s.add(nodes::box16(ids, lined), 16), 4));
    // Boxes around the origin, one switched off, one child missing.
    let around = [
        UNIT,
        moved(UNIT, [0.5, 0.5, 0.5]),
        off,
        [-4.0, -0.25, -0.25, 4.0, 0.25, 0.25],
    ];
    let ids2 = [8, bvh::NONE, 24, 0x1fff_fffc];
    box_nodes.push((s.add(nodes::box32(ids2, around), 32), 5));
    box_nodes.push((s.add(nodes::box16(ids2, around), 16), 4));
    // A child that is not there with inverted infinite bounds (the title's
    // builder leaves such slots): every ray "enters" it at minus infinity.
    let mut empty = lined;
    empty[1] = [f32::INFINITY, f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    let ids3 = [0x105, bvh::NONE, 0x304, 0x400];
    box_nodes.push((s.add(nodes::box32(ids3, empty), 32), 5));
    // Random boxes.
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
    // Triangles: one of the reference's tests, a pair as the title's
    // builder stores it, all four triangles of a full node with turned
    // ids, random ones.
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
    // I and J per triangle: (1, 2), (2, 0), (0, 1), (1, 2).
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
    // Rays of the reference's tests against the lined-up boxes.
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
    // Random rays at every box node, half of them aimed at a box.
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
    // Rays at every triangle: aimed inside, at an edge, at a vertex, past
    // it, from both sides, along each axis.
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
                // The triangle lies behind.
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
    // Kinds the instruction does not read, and threads masked out.
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

/// The buffer for one batch: the threads' records, then the nodes. Node
/// pointers count from `base` before the first node, in bytes.
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

pub(super) fn expected(scene: &Scene, case: &Case, sort: bool) -> [u32; 4] {
    match case {
        Some((n, kind, ray)) => bvh::intersect(&scene.node(*n), *kind, ray, sort),
        None => BEFORE,
    }
}

/// Sets the tests' title memory for this thread until dropped.
pub(super) struct Memory;

impl Memory {
    pub(super) fn at(base: u64) -> Self {
        // Heap slot 0: the buffer of records and nodes.
        TEST_MEMORY.set(Some((0, base)));
        Self
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        TEST_MEMORY.set(None);
    }
}

/// Runs every batch on the hardware adapter and WARP and compares with
/// the reference. `pointers` is what node pointers count from, in bytes
/// before the first node.
pub(super) fn run(wide: bool, descriptor: Descriptor, pointers: u64, sort: bool) {
    let (shader, res) = build(wide, descriptor).expect("the program lowers");
    assert_eq!(res.bvh.len(), 1);
    let (scene, batches) = scene();
    // The reference must see a bit of everything, or the comparison
    // proves little.
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
    // Sorting must matter to some case, or the unsorted run proves nothing.
    assert!(
        all.iter()
            .any(|c| expected(&scene, c, true) != expected(&scene, c, false)),
        "no case tells sorted from unsorted"
    );
    let _one = super::valu_helpers::ONE_AT_A_TIME
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
                    // A thread past the batch: its record is zeros, so it
                    // is masked out.
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

