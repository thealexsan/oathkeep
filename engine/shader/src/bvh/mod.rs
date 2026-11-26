//! The console GPU's ray-tracing node test, `image_bvh_intersect_ray` and
//! `image_bvh64_intersect_ray`, on the CPU: the node formats, and what the
//! instruction returns for one ray and one node. The emitter lowers the
//! instruction to the same arithmetic in the same order
//! (`emit/intersect.rs`), so its code can be compared against this dword
//! for dword.
//!
//! The instruction tests one ray against one 64-byte node of a bounding
//! volume hierarchy and returns four dwords. Traversal (the stack, instance
//! transforms, culling, the hit distance `numerator / denominator`) is the
//! program's own code.
//!
//! # Node pointers
//!
//! A node pointer's low three bits are the node's kind, the rest its byte
//! offset in 8-byte units: the node is at `(pointer >> 3) << 6` past the
//! base address of the descriptor (40 bits in dword 0 and the low byte of
//! dword 1, in 256-byte units). The 32-bit form takes a 32-bit pointer;
//! the 64-bit form a 64-bit one, with which a program uses one descriptor
//! with base 0 for every structure and adds `address >> 3` itself.
//!
//! | kind | node |
//! |---|---|
//! | 0-3 | triangle 0-3 of a triangle node |
//! | 4 | box node, 16-bit floats (64 bytes) |
//! | 5 | box node, 32-bit floats (128 bytes) |
//! | 6 | instance: not read by the instruction |
//! | 7 | procedural primitive: not read by the instruction |
//!
//! # Box nodes
//!
//! Four child pointers at bytes 0-15 (`0xffff_ffff`: none), then four
//! boxes, each minimum x y z, maximum x y z: 16-bit floats from byte 16
//! (12 bytes a box), or 32-bit floats from byte 16 (24 bytes a box; bytes
//! 112-127 are not read). The result is the pointers of the children whose
//! box the ray enters before its extent, nearest entry first when the
//! descriptor enables box sorting (bit 63), and `0xffff_ffff` for the rest.
//!
//! # Triangle nodes
//!
//! Five vertices of three 32-bit floats at bytes 0-59 and a dword at byte
//! 60 with one byte per triangle: bits 1:0 name the triangle's vertex that
//! barycentric I weighs, bits 3:2 the one J weighs (the builder stores an
//! input triangle's second vertex at the former, its third at the latter
//! and its first at the remaining one, so I and J are the weights of the
//! input triangle's second and third vertices). Triangle `k` of the node
//! uses the node's vertices [`TRIANGLE_VERTICES`]`[k]`. A program that
//! keeps fewer triangles in a node uses the bytes of the unused vertices
//! for its own data.
//!
//! With the descriptor's triangle return mode set (bit 120) the result is
//! the hit distance's numerator, its denominator, and the numerators of I
//! and J over the same denominator, as floats. The denominator is positive
//! for a triangle whose vertices run clockwise seen from the ray's origin
//! in a left-handed coordinate system. A miss returns +infinity, 1.0, 0, 0.
//! The ray's extent is not tested for triangles.
//!
//! # Where this comes from
//!
//! The node formats, the descriptor bits and the tests are those of the
//! public software emulation of this instruction in the open-source Vulkan
//! driver for this GPU family (box test and sorting network as there; the
//! triangle test is the watertight one it uses, Woop, Benthin and Wald,
//! JCGT 2013), and agree with what the title's own structure
//! builder writes and its trace programs read. Not known from either: how
//! the console rounds, which of two boxes entered at the same distance it
//! returns first, and what it returns for a ray exactly through an edge
//! (here: a hit in both triangles of the edge) or for kinds 6 and 7 (here:
//! a triangle miss; the title's programs do not read it).

/// The result dword for a box the ray misses, or a child that is not there.
pub const NONE: u32 = 0xffff_ffff;

/// The node's vertices that make triangle 0-3, by the triangle's own
/// vertex numbers 0-2.
pub const TRIANGLE_VERTICES: [[u32; 3]; 4] = [[0, 1, 2], [1, 3, 2], [2, 3, 4], [2, 4, 0]];

/// Descriptor dword 1: boxes are returned nearest first.
pub const BOX_SORT: u32 = 1 << 31;
/// Descriptor dword 3: triangles return the numerators of I and J (clear:
/// a triangle id and a hit status, which nothing here produces).
pub const RETURN_BARYCENTRICS: u32 = 1 << 24;
/// Descriptor dword 3, bits 31:28: the descriptor's type, a ray-tracing
/// structure.
pub const DESCRIPTOR_TYPE: u32 = 8;

/// The ray as the instruction takes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    pub extent: f32,
    pub origin: [f32; 3],
    pub direction: [f32; 3],
    /// What the program computed as `1 / direction`.
    pub inverse: [f32; 3],
}

impl Ray {
    /// A ray with the inverse direction a program computes by division.
    pub fn new(origin: [f32; 3], direction: [f32; 3], extent: f32) -> Self {
        Self {
            extent,
            origin,
            direction,
            inverse: direction.map(|d| 1.0 / d),
        }
    }
}

/// The kind of the node a pointer names.
pub fn kind(pointer: u64) -> u32 {
    (pointer & 7) as u32
}

/// The byte address of the node `pointer` names, for a descriptor whose
/// first two dwords are `word0` and `word1`.
pub fn node_address(word0: u32, word1: u32, pointer: u64) -> u64 {
    let base = (u64::from(word1 & 0xff) << 32 | u64::from(word0)) << 8;
    base.wrapping_add((pointer >> 3) << 6)
}

/// What the instruction returns for the node at `words` (its first 28
/// dwords; a 64-byte node's dwords 16-27 are not read) named by a pointer
/// of this `kind`, with box sorting as `sort` says and the triangle return
/// mode set.
pub fn intersect(words: &[u32; 28], kind: u32, ray: &Ray, sort: bool) -> [u32; 4] {
    match kind & 7 {
        4 => boxes(&children(words), &boxes16(words), ray, sort),
        5 => boxes(&children(words), &boxes32(words), ray, sort),
        k @ 0..=3 => triangle(&triangle_vertices(words, k), ray).map(f32::to_bits),
        _ => MISS.map(f32::to_bits),
    }
}

/// A box node's four child pointers.
pub fn children(words: &[u32; 28]) -> [u32; 4] {
    [words[0], words[1], words[2], words[3]]
}

/// The four boxes of a 16-bit box node: minimum x y z, maximum x y z.
pub fn boxes16(words: &[u32; 28]) -> [[f32; 6]; 4] {
    std::array::from_fn(|b| {
        std::array::from_fn(|k| {
            let half = 6 * b + k;
            half_to_f32((words[4 + half / 2] >> (16 * (half % 2))) as u16)
        })
    })
}

/// The four boxes of a 32-bit box node.
pub fn boxes32(words: &[u32; 28]) -> [[f32; 6]; 4] {
    std::array::from_fn(|b| std::array::from_fn(|k| f32::from_bits(words[4 + 6 * b + k])))
}

/// [`TRIANGLE_VERTICES`] of triangle `k` in four bits each, vertex 0 lowest
/// (as the public descriptions of the format give it: 0x210, 0x231, 0x432,
/// 0x042).
pub fn vertex_map(k: u32) -> u32 {
    let v = TRIANGLE_VERTICES[(k & 3) as usize];
    v[0] | v[1] << 4 | v[2] << 8
}

/// The vertices of triangle `k` (0-3) of a triangle node, the first being
/// the one neither barycentric weighs, then I's, then J's.
pub fn triangle_vertices(words: &[u32; 28], k: u32) -> [[f32; 3]; 3] {
    let id = words[15] >> (8 * k);
    let (i, j) = (id & 3, id >> 2 & 3);
    // An id naming one vertex twice, or a vertex 3, is malformed; it reads
    // what this arithmetic lands on (a vertex of the node), as the emitted
    // code does.
    let first = 3u32.wrapping_sub(i).wrapping_sub(j) & 3;
    [first, i, j].map(|n| {
        let vertex = (vertex_map(k) >> (4 * n) & 0xf) as usize;
        std::array::from_fn(|c| f32::from_bits(words[3 * vertex + c]))
    })
}

/// The box test: child pointers of the boxes the ray enters, `NONE` for
/// the others; sorted by entry distance when `sort`, in child order
/// otherwise.
pub fn boxes(children: &[u32; 4], boxes: &[[f32; 6]; 4], ray: &Ray, sort: bool) -> [u32; 4] {
    // An infinite inverse (a direction component of 0) would turn an
    // origin exactly on a box's face into a NaN. (Maximum then minimum, as
    // the emitted code: a NaN becomes the lower bound, where `clamp` would
    // keep it.)
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
        // A box whose minimum x is NaN is switched off. A child that is not
        // there may have inverted infinite bounds, which every ray enters at
        // minus infinity: it would be first of the sorted children and end
        // the walk before them (DexNom/Oathkeep bc3cd4b0).
        if children[n] != NONE && !b[0].is_nan() && leave >= enter.max(0.0) && enter < ray.extent {
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

/// What a triangle the ray misses returns.
pub const MISS: [f32; 4] = [f32::INFINITY, 1.0, 0.0, 0.0];

/// The triangle test: the hit distance's numerator, the denominator, and
/// the numerators of the weights of vertices 1 and 2; [`MISS`] otherwise.
/// Watertight: the vertices are sheared so the ray runs along its largest
/// axis, and the signs of the three edge functions decide, so a ray
/// between two triangles that share an edge cannot miss both. Any NaN
/// misses, and so does a triangle seen edge-on.
pub fn triangle(p: &[[f32; 3]; 3], ray: &Ray) -> [f32; 4] {
    let d = ray.direction;
    let size = d.map(f32::abs);
    // The largest axis, and the other two in an order that keeps the
    // triangle's winding.
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

/// A 16-bit float as a 32-bit one (exact).
pub fn half_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let (exponent, mantissa) = (u32::from(h >> 10 & 0x1f), u32::from(h & 0x3ff));
    let bits = match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            // Subnormal: normalise.
            let shift = mantissa.leading_zeros() - 21;
            sign | (113 - shift) << 23 | (mantissa << shift & 0x3ff) << 13
        }
        0x1f => sign | 0xff << 23 | mantissa << 13,
        e => sign | (e + 112) << 23 | mantissa << 13,
    };
    f32::from_bits(bits)
}


#[cfg(test)]
pub(crate) mod tests;
