pub const NONE: u32 = 0xffff_ffff;

pub const TRIANGLE_VERTICES: [[u32; 3]; 4] = [[0, 1, 2], [1, 3, 2], [2, 3, 4], [2, 4, 0]];

pub const BOX_SORT: u32 = 1 << 31;
pub const RETURN_BARYCENTRICS: u32 = 1 << 24;
pub const DESCRIPTOR_TYPE: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    pub extent: f32,
    pub origin: [f32; 3],
    pub direction: [f32; 3],
    pub inverse: [f32; 3],
}

impl Ray {
    pub fn new(origin: [f32; 3], direction: [f32; 3], extent: f32) -> Self {
        Self {
            extent,
            origin,
            direction,
            inverse: direction.map(|d| 1.0 / d),
        }
    }
}

pub fn kind(pointer: u64) -> u32 {
    (pointer & 7) as u32
}

pub fn node_address(word0: u32, word1: u32, pointer: u64) -> u64 {
    let base = (u64::from(word1 & 0xff) << 32 | u64::from(word0)) << 8;
    base.wrapping_add((pointer >> 3) << 6)
}

pub fn intersect(words: &[u32; 28], kind: u32, ray: &Ray, sort: bool) -> [u32; 4] {
    match kind & 7 {
        4 => boxes(&children(words), &boxes16(words), ray, sort),
        5 => boxes(&children(words), &boxes32(words), ray, sort),
        k @ 0..=3 => triangle(&triangle_vertices(words, k), ray).map(f32::to_bits),
        _ => MISS.map(f32::to_bits),
    }
}

pub fn children(words: &[u32; 28]) -> [u32; 4] {
    [words[0], words[1], words[2], words[3]]
}

pub fn boxes16(words: &[u32; 28]) -> [[f32; 6]; 4] {
    std::array::from_fn(|b| {
        std::array::from_fn(|k| {
            let half = 6 * b + k;
            half_to_f32((words[4 + half / 2] >> (16 * (half % 2))) as u16)
        })
    })
}

pub fn boxes32(words: &[u32; 28]) -> [[f32; 6]; 4] {
    std::array::from_fn(|b| std::array::from_fn(|k| f32::from_bits(words[4 + 6 * b + k])))
}

pub fn vertex_map(k: u32) -> u32 {
    let v = TRIANGLE_VERTICES[(k & 3) as usize];
    v[0] | v[1] << 4 | v[2] << 8
}

pub fn triangle_vertices(words: &[u32; 28], k: u32) -> [[f32; 3]; 3] {
    let id = words[15] >> (8 * k);
    let (i, j) = (id & 3, id >> 2 & 3);
    let first = 3u32.wrapping_sub(i).wrapping_sub(j) & 3;
    [first, i, j].map(|n| {
        let vertex = (vertex_map(k) >> (4 * n) & 0xf) as usize;
        std::array::from_fn(|c| f32::from_bits(words[3 * vertex + c]))
    })
}

pub fn boxes(children: &[u32; 4], boxes: &[[f32; 6]; 4], ray: &Ray, sort: bool) -> [u32; 4] {
    #[allow(clippy::manual_clamp)]
    let inverse = ray.inverse.map(|x| x.max(-f32::MAX).min(f32::MAX));
    let mut ids = [NONE; 4];
    let mut distances = [f32::INFINITY; 4];
    for (n, b) in boxes.iter().enumerate() {
        let (mut near, mut far) = ([0f32; 3], [0f32; 3]);
        for a in 0..3 {
            let low = (b[a] - ray.origin[a]) * inverse[a];
            let high = (b[3 + a] - ray.origin[a]) * inverse[a];
            near[a] = low.min(high);
            far[a] = low.max(high);
        }
        let enter = near[0].max(near[1]).max(near[2]);
        let leave = far[0].min(far[1]).min(far[2]);
        if !b[0].is_nan() && leave >= enter.max(0.0) && enter < ray.extent {
            ids[n] = children[n];
            distances[n] = enter;
        }
    }
    if sort {
        for (a, b) in [(0, 1), (2, 3), (0, 2), (1, 3), (1, 2)] {
            if distances[b] < distances[a] {
                distances.swap(a, b);
                ids.swap(a, b);
            }
        }
    }
    ids
}

pub const MISS: [f32; 4] = [f32::INFINITY, 1.0, 0.0, 0.0];

pub fn triangle(p: &[[f32; 3]; 3], ray: &Ray) -> [f32; 4] {
    let d = ray.direction;
    let size = d.map(f32::abs);
    let kz = if size[0] >= size[1] {
        if size[0] >= size[2] { 0 } else { 2 }
    } else if size[1] >= size[2] {
        1
    } else {
        2
    };
    let (mut kx, mut ky) = ((kz + 1) % 3, (kz + 2) % 3);
    if d[kz] < 0.0 {
        std::mem::swap(&mut kx, &mut ky);
    }
    let sz = ray.inverse[kz];
    let (sx, sy) = (d[kx] * sz, d[ky] * sz);
    let sheared = |v: &[f32; 3]| {
        let r = [
            v[0] - ray.origin[0],
            v[1] - ray.origin[1],
            v[2] - ray.origin[2],
        ];
        (r[kx] - sx * r[kz], r[ky] - sy * r[kz], sz * r[kz])
    };
    let (ax, ay, az) = sheared(&p[0]);
    let (bx, by, bz) = sheared(&p[1]);
    let (cx, cy, cz) = sheared(&p[2]);
    let u = cx * by - cy * bx;
    let v = ax * cy - ay * cx;
    let w = bx * ay - by * ax;
    let behind = u < 0.0 || v < 0.0 || w < 0.0;
    let before = u > 0.0 || v > 0.0 || w > 0.0;
    let det = u + (v + w);
    let t = (u * az + v * bz) + w * cz;
    let ahead = det > 0.0 && t >= 0.0 || det < 0.0 && t <= 0.0;
    if !(behind && before) && ahead {
        [t, det, v, w]
    } else {
        MISS
    }
}

pub fn half_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let (exponent, mantissa) = (u32::from(h >> 10 & 0x1f), u32::from(h & 0x3ff));
    let bits = match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            let shift = mantissa.leading_zeros() - 21;
            sign | (113 - shift) << 23 | (mantissa << shift & 0x3ff) << 13
        }
        0x1f => sign | 0xff << 23 | mantissa << 13,
        e => sign | (e + 112) << 23 | mantissa << 13,
    };
    f32::from_bits(bits)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn f32_to_half(x: f32, up: bool) -> u16 {
        let mut best: Option<u16> = None;
        for h in 0..=0xffffu16 {
            let f = half_to_f32(h);
            if f.is_nan() || if up { f < x } else { f > x } {
                continue;
            }
            best = match best {
                Some(b) if (half_to_f32(b) - x).abs() <= (f - x).abs() => Some(b),
                _ => Some(h),
            };
        }
        best.expect("a half")
    }

    pub(crate) fn box32(children: [u32; 4], boxes: [[f32; 6]; 4]) -> [u32; 28] {
        let mut w = [0u32; 28];
        w[..4].copy_from_slice(&children);
        for (b, bx) in boxes.iter().enumerate() {
            for (k, x) in bx.iter().enumerate() {
                w[4 + 6 * b + k] = x.to_bits();
            }
        }
        w
    }

    pub(crate) fn box16(children: [u32; 4], boxes: [[f32; 6]; 4]) -> [u32; 28] {
        let mut w = [0u32; 28];
        w[..4].copy_from_slice(&children);
        for (b, bx) in boxes.iter().enumerate() {
            for (k, x) in bx.iter().enumerate() {
                let half = 6 * b + k;
                let h = if x.is_nan() {
                    0x7e00
                } else {
                    f32_to_half(*x, k >= 3)
                };
                w[4 + half / 2] |= u32::from(h) << (16 * (half % 2));
            }
        }
        w
    }

    pub(crate) fn triangles(vertices: &[[f32; 3]], id: u32) -> [u32; 28] {
        let mut w = [0u32; 28];
        for (n, v) in vertices.iter().enumerate() {
            for (c, x) in v.iter().enumerate() {
                w[3 * n + c] = x.to_bits();
            }
        }
        w[15] = id;
        w
    }

    const UNIT: [f32; 6] = [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0];

    fn moved(b: [f32; 6], by: [f32; 3]) -> [f32; 6] {
        std::array::from_fn(|k| b[k] + by[k % 3])
    }

    const OFF: [f32; 6] = [f32::NAN; 6];

    #[test]
    fn node_pointers_name_64_byte_nodes_past_the_descriptors_base() {
        let structure = 0x12_3456_7800u64;
        let pointer = (structure >> 8 << 5) + (0x40 >> 3 | 5);
        assert_eq!(kind(pointer), 5);
        assert_eq!(node_address(0, BOX_SORT, pointer), structure + 0x40);
        let word0 = (structure >> 8) as u32;
        let word1 = (structure >> 40) as u32 | BOX_SORT;
        assert_eq!(
            node_address(word0, word1, 0x1000 >> 3 | 4),
            structure + 0x1000
        );
        assert_eq!(node_address(0, 0x8000_0001, 8), (1 << 40) + 64);
    }

    #[test]
    fn halves_convert_exactly() {
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x7c00), f32::INFINITY);
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
        assert_eq!(half_to_f32(0x03ff), 1023.0 * 2f32.powi(-24));
        assert_eq!(half_to_f32(0x7bff), 65504.0);
        assert!(half_to_f32(0x7e00).is_nan());
        assert_eq!(half_to_f32(0x8000).to_bits(), 0x8000_0000);
        assert!(half_to_f32(f32_to_half(0.1, true)) >= 0.1);
        assert!(half_to_f32(f32_to_half(0.1, false)) <= 0.1);
    }

    #[test]
    fn boxes_are_returned_nearest_first_and_misses_last() {
        let children = [0x105, 0x20c, 0x304, 0x400];
        let node = box32(
            children,
            [
                moved(UNIT, [30.0, 0.0, 0.0]),
                moved(UNIT, [10.0, 0.0, 0.0]),
                moved(UNIT, [10.0, 5.0, 0.0]),
                moved(UNIT, [20.0, 0.0, 0.0]),
            ],
        );
        let ray = Ray::new([0.0; 3], [1.0, 0.0, 0.0], 1000.0);
        assert_eq!(intersect(&node, 5, &ray, true), [0x20c, 0x400, 0x105, NONE]);
        assert_eq!(
            intersect(&node, 5, &ray, false),
            [0x105, 0x20c, NONE, 0x400]
        );
        let short = Ray {
            extent: 19.5,
            ..ray
        };
        assert_eq!(
            intersect(&node, 5, &short, true),
            [0x20c, 0x400, NONE, NONE]
        );
        let exact = Ray {
            extent: 19.0,
            ..ray
        };
        assert_eq!(intersect(&node, 5, &exact, true), [0x20c, NONE, NONE, NONE]);
        let back = Ray::new([0.0; 3], [-1.0, 0.0, 0.0], 1000.0);
        assert_eq!(intersect(&node, 5, &back, true), [NONE; 4]);
    }

    #[test]
    fn a_ray_starting_inside_a_box_enters_it_at_its_origin() {
        let node = box32(
            [8, 16, NONE, NONE],
            [UNIT, moved(UNIT, [3.0, 0.0, 0.0]), OFF, OFF],
        );
        let ray = Ray::new([0.0; 3], [1.0, 0.0, 0.0], 100.0);
        assert_eq!(intersect(&node, 5, &ray, true), [8, 16, NONE, NONE]);
        let tiny = Ray {
            extent: 1e-6,
            ..ray
        };
        assert_eq!(intersect(&node, 5, &tiny, true), [8, NONE, NONE, NONE]);
    }

    #[test]
    fn axis_aligned_rays_and_origins_on_a_face_make_no_nan() {
        let node = box32([8, 16, 24, 32], [UNIT, OFF, OFF, OFF]);
        let ray = Ray::new([-5.0, -1.0, 0.0], [1.0, 0.0, 0.0], 100.0);
        assert!(ray.inverse[1].is_infinite());
        assert_eq!(intersect(&node, 5, &ray, true), [8, NONE, NONE, NONE]);
        let top = Ray::new([-5.0, 1.0, 0.0], [1.0, 0.0, 0.0], 100.0);
        assert_eq!(intersect(&node, 5, &top, true), [NONE; 4]);
        let outside = Ray::new([-5.0, 1.5, 0.0], [1.0, 0.0, 0.0], 100.0);
        assert_eq!(intersect(&node, 5, &outside, true), [NONE; 4]);
        let minus = Ray::new([-5.0, 0.5, 0.0], [1.0, -0.0, 0.0], 100.0);
        assert_eq!(minus.inverse[1], f32::NEG_INFINITY);
        assert_eq!(intersect(&node, 5, &minus, true), [8, NONE, NONE, NONE]);
    }

    #[test]
    fn a_box_switched_off_and_a_missing_child_return_none() {
        let node = box32(
            [8, NONE, 24, 32],
            [UNIT, UNIT, [f32::NAN, -1.0, -1.0, 1.0, 1.0, 1.0], OFF],
        );
        let ray = Ray::new([-5.0, 0.0, 0.0], [1.0, 0.0, 0.0], 100.0);
        assert_eq!(intersect(&node, 5, &ray, true), [8, NONE, NONE, NONE]);
    }

    #[test]
    fn equal_distances_keep_child_order() {
        let node = box32([8, 16, 24, 32], [UNIT; 4]);
        let ray = Ray::new([-5.0, 0.0, 0.0], [1.0, 0.0, 0.0], 100.0);
        assert_eq!(intersect(&node, 5, &ray, true), [8, 16, 24, 32]);
    }

    #[test]
    fn half_float_boxes_test_like_their_widened_values() {
        let boxes = [
            moved(UNIT, [30.0, 0.1, 0.0]),
            moved(UNIT, [10.0, 0.0, 0.3]),
            moved(UNIT, [10.0, 5.0, 0.0]),
            moved(UNIT, [20.7, 0.0, 0.0]),
        ];
        let children = [0x105, 0x20c, 0x304, 0x400];
        let node = box16(children, boxes);
        let wide = boxes16(&node);
        for (w, b) in wide.iter().zip(&boxes) {
            for k in 0..3 {
                assert!(w[k] <= b[k] && w[3 + k] >= b[3 + k], "rounded outward");
            }
        }
        let ray = Ray::new([0.0; 3], [1.0, 0.0, 0.0], 1000.0);
        assert_eq!(intersect(&node, 4, &ray, true), [0x20c, 0x400, 0x105, NONE]);
        assert_eq!(
            intersect(&node, 4, &ray, true),
            intersect(&box32(children, wide), 5, &ray, true)
        );
    }

    fn hit(r: [u32; 4]) -> Option<(f32, f32, f32, bool)> {
        let [t, det, i, j] = r.map(f32::from_bits);
        (t != f32::INFINITY).then(|| (t / det, i / det, j / det, det > 0.0))
    }

    const TRIANGLE: [[f32; 3]; 3] = [[0.0, 2.0, 5.0], [2.0, -2.0, 5.0], [-2.0, -2.0, 5.0]];

    #[test]
    fn a_triangle_returns_distance_and_weights_over_one_denominator() {
        let node = triangles(&TRIANGLE, 9);
        let ray = Ray::new([0.0; 3], [0.0, 0.0, 1.0], 100.0);
        let (t, i, j, front) = hit(intersect(&node, 0, &ray, true)).expect("a hit");
        assert_eq!(t, 5.0);
        assert_eq!((i, j), (0.25, 0.25));
        assert!(front);
        let ray = Ray::new([1.75, -1.75, 0.0], [0.0, 0.0, 1.0], 100.0);
        let (_, i, j, _) = hit(intersect(&node, 0, &ray, true)).expect("a hit");
        assert!(i > 0.85 && j < 0.1, "{i} {j}");
        let ray = Ray::new([0.0, 0.0, 10.0], [0.0, 0.0, -1.0], 100.0);
        let (t, i, j, front) = hit(intersect(&node, 0, &ray, true)).expect("a hit");
        assert_eq!((t, i, j, front), (5.0, 0.25, 0.25, false));
        let ray = Ray::new([0.0; 3], [0.0, 0.0, 4.0], 100.0);
        assert_eq!(hit(intersect(&node, 0, &ray, true)).unwrap().0, 1.25);
    }

    #[test]
    fn a_triangle_is_missed_beside_behind_and_edge_on() {
        let node = triangles(&TRIANGLE, 9);
        let miss = MISS.map(f32::to_bits);
        let ray = Ray::new([3.0, 0.0, 0.0], [0.0, 0.0, 1.0], 100.0);
        assert_eq!(intersect(&node, 0, &ray, true), miss);
        let ray = Ray::new([0.0, 0.0, 6.0], [0.0, 0.0, 1.0], 100.0);
        assert_eq!(intersect(&node, 0, &ray, true), miss);
        let ray = Ray::new([-5.0, 0.0, 5.0], [1.0, 0.0, 0.0], 100.0);
        assert_eq!(intersect(&node, 0, &ray, true), miss);
        let flat = triangles(&[[0.0, 0.0, 5.0], [1.0, 1.0, 5.0], [2.0, 2.0, 5.0]], 9);
        let ray = Ray::new([1.0, 1.0, 0.0], [0.0, 0.0, 1.0], 100.0);
        assert_eq!(intersect(&flat, 0, &ray, true), miss);
        let nan = triangles(&[[f32::NAN, 2.0, 5.0], TRIANGLE[1], TRIANGLE[2]], 9);
        let ray = Ray::new([0.0; 3], [0.0, 0.0, 1.0], 100.0);
        assert_eq!(intersect(&nan, 0, &ray, true), miss);
        let still = Ray::new([0.0; 3], [0.0; 3], 100.0);
        assert_eq!(intersect(&node, 0, &still, true), miss);
        let short = Ray::new([0.0; 3], [0.0, 0.0, 1.0], 1.0);
        assert!(hit(intersect(&node, 0, &short, true)).is_some());
    }

    #[test]
    fn a_ray_through_a_shared_edge_misses_neither_triangle() {
        let quad = [
            [-1.0, -1.0, 4.0],
            [-1.0, 1.0, 4.0],
            [1.0, 1.0, 4.0],
            [1.0, -1.0, 4.0],
        ];
        let first = triangles(&[quad[0], quad[1], quad[2]], 9);
        let second = triangles(&[quad[0], quad[2], quad[3]], 9);
        for n in -40..=40 {
            let x = n as f32 / 64.0;
            for (ox, oy) in [(x, x), (x, x + 1.0 / 1024.0), (x + 1.0 / 1024.0, x)] {
                for d in [[0.0, 0.0, 1.0], [0.125, -0.0625, 1.0], [0.25, 0.5, 2.0]] {
                    let s = 4.0 / d[2];
                    let ray = Ray::new([ox - d[0] * s, oy - d[1] * s, 0.0], d, 100.0);
                    let a = hit(intersect(&first, 0, &ray, true));
                    let b = hit(intersect(&second, 0, &ray, true));
                    assert!(
                        a.is_some() || b.is_some(),
                        "a hole at {ox} {oy} along {d:?}"
                    );
                    for h in [a, b].into_iter().flatten() {
                        assert!((h.0 - s).abs() < 1e-5 && h.3, "{h:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_id_dword_says_which_vertices_the_weights_belong_to() {
        let [a, b, c] = TRIANGLE;
        let ray = Ray::new([0.5, -0.25, 0.0], [0.0, 0.0, 1.0], 100.0);
        let plain = intersect(&triangles(&[a, b, c], 9), 0, &ray, true);
        for (i, j) in [(1u32, 2u32), (2, 0), (0, 1), (2, 1), (0, 2), (1, 0)] {
            let mut stored = [[0f32; 3]; 3];
            stored[(3 - i - j) as usize] = a;
            stored[i as usize] = b;
            stored[j as usize] = c;
            let node = triangles(&stored, i | j << 2);
            assert_eq!(intersect(&node, 0, &ray, true), plain, "I {i} J {j}");
        }
        let node = triangles(&[a, b, c], 9 | 0x80);
        assert_eq!(intersect(&node, 0, &ray, true), plain);
    }

    #[test]
    fn triangles_one_to_three_take_their_vertices_and_their_id_byte() {
        let [a, b, c] = TRIANGLE;
        let far = [50.0, 50.0, 50.0];
        let ray = Ray::new([0.5, -0.25, 0.0], [0.0, 0.0, 1.0], 100.0);
        let plain = intersect(&triangles(&[a, b, c], 9), 0, &ray, true);
        for k in 1..4u32 {
            let mut vertices = [far; 5];
            let at = TRIANGLE_VERTICES[k as usize];
            vertices[at[0] as usize] = a;
            vertices[at[1] as usize] = b;
            vertices[at[2] as usize] = c;
            let node = triangles(&vertices, 9 << (8 * k));
            assert_eq!(intersect(&node, k, &ray, true), plain, "triangle {k}");
        }
        let d = [2.0, 2.0, 5.0];
        let node = triangles(&[a, b, c, d], 9 | (1 | 2 << 2) << 8);
        let got = triangle_vertices(&node, 1);
        assert_eq!(got, [b, d, c]);
    }

    #[test]
    fn instances_and_procedural_nodes_return_a_triangle_miss() {
        let node = triangles(&TRIANGLE, 9);
        let ray = Ray::new([0.0; 3], [0.0, 0.0, 1.0], 100.0);
        for k in [6, 7] {
            assert_eq!(intersect(&node, k, &ray, true), MISS.map(f32::to_bits));
        }
    }

    #[test]
    fn the_largest_axis_and_its_sign_do_not_change_the_answer() {
        let turn = |v: [f32; 3], axis: usize, flip: bool| {
            let mut r = v;
            for _ in 0..axis {
                r = [r[2], r[0], r[1]];
            }
            if flip {
                let largest = (2 + axis) % 3;
                r[largest] = -r[largest];
                r[(largest + 1) % 3] = -r[(largest + 1) % 3];
            }
            r
        };
        let ray = Ray::new([0.25, -0.5, 0.0], [0.0625, 0.125, 1.0], 100.0);
        let base = hit(triangle(&TRIANGLE, &ray).map(f32::to_bits)).expect("a hit");
        for axis in 0..3 {
            for flip in [false, true] {
                let p = TRIANGLE.map(|v| turn(v, axis, flip));
                let r = Ray::new(
                    turn(ray.origin, axis, flip),
                    turn(ray.direction, axis, flip),
                    100.0,
                );
                let h = hit(triangle(&p, &r).map(f32::to_bits)).expect("a hit");
                assert!((h.0 - base.0).abs() < 1e-5, "{axis} {flip}: {h:?} {base:?}");
                assert!((h.1 - base.1).abs() < 1e-5 && (h.2 - base.2).abs() < 1e-5);
                assert_eq!(h.3, base.3, "facing, axis {axis} flip {flip}");
            }
        }
    }
}
