use super::super::{Bin, Lower, V, pred};
use super::{NodeAt, Ray};
use crate::bvh;

impl<'a> Lower<'a> {
    /// The triangle test ([`bvh::triangle`] of [`bvh::triangle_vertices`]):
    /// four result dwords, made in the current block.
    pub(super) fn bvh_triangle(&mut self, at: NodeAt, kind: V, ray: &Ray) -> [V; 4] {
        let c = self.cur;
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        // The triangle's byte of the dword at byte 60: which of its
        // vertices barycentric I and J weigh.
        let id = self.bvh_node_words(at, None, 60, 1)[0];
        let (k2, k3, k12, k15) = (k(self, 2), k(self, 3), k(self, 12), k(self, 15));
        let shift = self.s.m.bin(c, Bin::Shl, kind, k3);
        let id = self.s.m.bin(c, Bin::LShr, id, shift);
        let i = self.s.m.bin(c, Bin::And, id, k3);
        let j = self.s.m.bin(c, Bin::LShr, id, k2);
        let j = self.s.m.bin(c, Bin::And, j, k3);
        let first = self.s.m.bin(c, Bin::Sub, k3, i);
        let first = self.s.m.bin(c, Bin::Sub, first, j);
        let first = self.s.m.bin(c, Bin::And, first, k3);
        // The node's vertices the triangle is made of, four bits each.
        let (k0, k1) = (k(self, 0), k(self, 1));
        let maps: [V; 4] = std::array::from_fn(|n| k(self, bvh::vertex_map(n as u32)));
        let (is0, is1, is2) = (
            self.s.m.cmp(c, pred::EQ, kind, k0),
            self.s.m.cmp(c, pred::EQ, kind, k1),
            self.s.m.cmp(c, pred::EQ, kind, k2),
        );
        let map = self.s.m.select(c, is2, maps[2], maps[3]);
        let map = self.s.m.select(c, is1, maps[1], map);
        let map = self.s.m.select(c, is0, maps[0], map);
        let mut p = Vec::with_capacity(3);
        for n in [first, i, j] {
            let nibble = self.s.m.bin(c, Bin::Shl, n, k2);
            let vertex = self.s.m.bin(c, Bin::LShr, map, nibble);
            let vertex = self.s.m.bin(c, Bin::And, vertex, k15);
            let byte = self.s.m.bin(c, Bin::Mul, vertex, k12);
            let words = self.bvh_node_words(at, Some(byte), 0, 3);
            // Relative to the ray's origin.
            let r: Vec<V> = (0..3)
                .map(|a| {
                    let f = self.as_float(words[a]);
                    self.s.m.bin(c, Bin::Sub, f, ray.origin[a])
                })
                .collect();
            p.push([r[0], r[1], r[2]]);
        }
        // The ray's largest axis z, and x and y after it in an order that
        // keeps the winding.
        let fzero = self.s.m.f32(0);
        let size = ray.direction.map(|d| self.unary(6, d));
        let xy = self.s.m.cmp(c, pred::OGE, size[0], size[1]);
        let xz = self.s.m.cmp(c, pred::OGE, size[0], size[2]);
        let yz = self.s.m.cmp(c, pred::OGE, size[1], size[2]);
        let z0 = self.s.m.bin(c, Bin::And, xy, xz);
        let on = self.s.m.bool(true);
        let not_xy = self.s.m.bin(c, Bin::Xor, xy, on);
        let z1 = self.s.m.bin(c, Bin::And, not_xy, yz);
        // Component (kz + n) % 3 of `q`.
        let pick = |l: &mut Self, q: [V; 3], n: usize| {
            let rest = l.s.m.select(c, z1, q[(1 + n) % 3], q[(2 + n) % 3]);
            l.s.m.select(c, z0, q[n % 3], rest)
        };
        let dz = pick(self, ray.direction, 0);
        let backwards = self.s.m.cmp(c, pred::OLT, dz, fzero);
        // (x, y, z) of `q`: the two after the largest axis, swapped for a
        // ray that runs backwards along it.
        let axes = |l: &mut Self, q: [V; 3]| {
            let (x, y) = (pick(l, q, 1), pick(l, q, 2));
            [
                l.s.m.select(c, backwards, y, x),
                l.s.m.select(c, backwards, x, y),
                pick(l, q, 0),
            ]
        };
        let d = axes(self, ray.direction);
        let sz = pick(self, ray.inverse, 0);
        let sx = self.s.m.bin(c, Bin::Mul, d[0], sz);
        let sy = self.s.m.bin(c, Bin::Mul, d[1], sz);
        let mut sheared = Vec::with_capacity(3);
        for r in p {
            let r = axes(self, r);
            let x = self.s.m.bin(c, Bin::Mul, sx, r[2]);
            let x = self.s.m.bin(c, Bin::Sub, r[0], x);
            let y = self.s.m.bin(c, Bin::Mul, sy, r[2]);
            let y = self.s.m.bin(c, Bin::Sub, r[1], y);
            let z = self.s.m.bin(c, Bin::Mul, sz, r[2]);
            sheared.push((x, y, z));
        }
        let [(ax, ay, az), (bx, by, bz), (cx, cy, cz)] = sheared[..] else {
            unreachable!()
        };
        // p * q - r * s.
        let cross = |l: &mut Self, p: V, q: V, r: V, s: V| {
            let a = l.s.m.bin(c, Bin::Mul, p, q);
            let b = l.s.m.bin(c, Bin::Mul, r, s);
            l.s.m.bin(c, Bin::Sub, a, b)
        };
        let u = cross(self, cx, by, cy, bx);
        let v = cross(self, ax, cy, ay, cx);
        let w = cross(self, bx, ay, by, ax);
        let any = |l: &mut Self, p: u8| {
            let a = l.s.m.cmp(c, p, u, fzero);
            let b = l.s.m.cmp(c, p, v, fzero);
            let d = l.s.m.cmp(c, p, w, fzero);
            let a = l.s.m.bin(c, Bin::Or, a, b);
            l.s.m.bin(c, Bin::Or, a, d)
        };
        let behind = any(self, pred::OLT);
        let before = any(self, pred::OGT);
        let outside = self.s.m.bin(c, Bin::And, behind, before);
        let inside = self.s.m.bin(c, Bin::Xor, outside, on);
        let det = self.s.m.bin(c, Bin::Add, v, w);
        let det = self.s.m.bin(c, Bin::Add, u, det);
        let t = self.s.m.bin(c, Bin::Mul, u, az);
        let t2 = self.s.m.bin(c, Bin::Mul, v, bz);
        let t = self.s.m.bin(c, Bin::Add, t, t2);
        let t2 = self.s.m.bin(c, Bin::Mul, w, cz);
        let t = self.s.m.bin(c, Bin::Add, t, t2);
        // The distance is not negative: the numerator has the
        // denominator's sign (or is 0). Ordered compares: a denominator
        // of 0 or a NaN is a miss.
        let front = self.s.m.cmp(c, pred::OGT, det, fzero);
        let front_ahead = self.s.m.cmp(c, pred::OGE, t, fzero);
        let front = self.s.m.bin(c, Bin::And, front, front_ahead);
        let back = self.s.m.cmp(c, pred::OLT, det, fzero);
        let back_ahead = self.s.m.cmp(c, pred::OLE, t, fzero);
        let back = self.s.m.bin(c, Bin::And, back, back_ahead);
        let ahead = self.s.m.bin(c, Bin::Or, front, back);
        let hit = self.s.m.bin(c, Bin::And, inside, ahead);
        let miss = bvh::MISS.map(|f| self.s.m.f32(f.to_bits()));
        let out = [t, det, v, w];
        std::array::from_fn(|n| {
            let f = self.s.m.select(c, hit, out[n], miss[n]);
            self.as_int(f)
        })
    }
}
