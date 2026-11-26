use super::super::{Attr, Bin, Lower, Term, V, pred};
use super::{NodeAt, Ray};
use crate::bvh;

impl<'a> Lower<'a> {
    /// The box test ([`bvh::boxes`]): four result dwords. Starts in the
    /// current block and leaves `self.cur` at the block it ends in.
    pub(super) fn bvh_boxes(&mut self, at: NodeAt, kind: V, ray: &Ray, sort: bool) -> [V; 4] {
        let head = self.cur;
        let children = self.bvh_node_words(at, None, 0, 4);
        let k5 = self.s.m.i32(5);
        let wide = self.s.m.cmp(head, pred::EQ, kind, k5);
        let (wide_block, half_block) = (self.s.m.new_block(), self.s.m.new_block());
        let join = self.s.m.new_block();
        self.s
            .m
            .terminate(head, Term::CondBr(wide, wide_block, half_block));
        // 32-bit floats: 24 dwords from byte 16.
        self.cur = wide_block;
        let k16 = self.s.m.i32(16);
        let mut words = self.bvh_node_words(at, None, 16, 12);
        let second = self.bvh_second_half(at);
        words.extend(self.bvh_node_words(second, None, 0, 12));
        let floats: Vec<V> = words.iter().map(|&w| self.as_float(w)).collect();
        // (The look for the second half goes on in another block.)
        let wide_block = self.cur;
        self.s.m.terminate(wide_block, Term::Br(join));
        // 16-bit floats: 24 halves in 12 dwords from byte 16, the low half
        // of a dword first.
        self.cur = half_block;
        let words = self.bvh_node_words(at, None, 16, 12);
        let f32t = self.s.t.f32;
        let mut halves = Vec::with_capacity(24);
        for n in 0..24 {
            let mut h = words[n / 2];
            if n % 2 == 1 {
                h = self.s.m.bin(half_block, Bin::LShr, h, k16);
            }
            // The conversion reads the low 16 bits.
            halves.push(self.s.op(
                half_block,
                "legacyF16ToF32",
                Attr::ReadNone,
                f32t,
                131,
                &[h],
            ));
        }
        self.s.m.terminate(half_block, Term::Br(join));
        self.cur = join;
        // A page that is not there reads zeros, which are a child and a box:
        // no child of it is hit. (The last two boxes of a 128-byte node are in
        // its second half.)
        let there = match (at, second) {
            (NodeAt::Memory { mapped: first, .. }, NodeAt::Memory { mapped: last, .. }) => {
                let i1t = self.s.m.type_of(first);
                let mapped = self.s.m.phi(join, i1t);
                self.s
                    .m
                    .set_phi(mapped, vec![(last, wide_block), (first, half_block)]);
                let both = self.s.m.bin(join, Bin::And, first, mapped);
                Some([first, first, both, both])
            }
            _ => None,
        };
        let mut coords = Vec::with_capacity(24);
        for n in 0..24 {
            let phi = self.s.m.phi(join, f32t);
            self.s
                .m
                .set_phi(phi, vec![(floats[n], wide_block), (halves[n], half_block)]);
            coords.push(phi);
        }
        // The inverse direction held finite ([`bvh::boxes`]).
        let (most, least) = (
            self.s.m.f32(f32::MAX.to_bits()),
            self.s.m.f32((-f32::MAX).to_bits()),
        );
        let inverse = ray.inverse.map(|x| {
            let x = self.binary(35, x, least);
            self.binary(36, x, most)
        });
        let c = join;
        let (none, infinity, fzero) = (
            self.s.m.i32(bvh::NONE),
            self.s.m.f32(f32::INFINITY.to_bits()),
            self.s.m.f32(0),
        );
        let mut ids = [none; 4];
        let mut distances = [infinity; 4];
        for n in 0..4 {
            let bx = &coords[6 * n..6 * n + 6];
            let (mut near, mut far) = ([fzero; 3], [fzero; 3]);
            for a in 0..3 {
                let low = self.s.m.bin(c, Bin::Sub, bx[a], ray.origin[a]);
                let low = self.s.m.bin(c, Bin::Mul, low, inverse[a]);
                let high = self.s.m.bin(c, Bin::Sub, bx[3 + a], ray.origin[a]);
                let high = self.s.m.bin(c, Bin::Mul, high, inverse[a]);
                near[a] = self.binary(36, low, high);
                far[a] = self.binary(35, low, high);
            }
            let enter = self.binary(35, near[0], near[1]);
            let enter = self.binary(35, enter, near[2]);
            let leave = self.binary(36, far[0], far[1]);
            let leave = self.binary(36, leave, far[2]);
            let from = self.binary(35, enter, fzero);
            // Ordered compares: a NaN anywhere is a miss.
            let through = self.s.m.cmp(c, pred::OGE, leave, from);
            let before = self.s.m.cmp(c, pred::OLT, enter, ray.extent);
            let off = self.is_nan_f32(bx[0]);
            let on = self.s.m.bool(true);
            let on = self.s.m.bin(c, Bin::Xor, off, on);
            let hit = self.s.m.bin(c, Bin::And, through, before);
            let hit = self.s.m.bin(c, Bin::And, hit, on);
            let child = self.s.m.cmp(c, pred::NE, children[n], none);
            let hit = self.s.m.bin(c, Bin::And, hit, child);
            let hit = match there {
                Some(there) => self.s.m.bin(c, Bin::And, hit, there[n]),
                None => hit,
            };
            ids[n] = self.s.m.select(c, hit, children[n], none);
            distances[n] = self.s.m.select(c, hit, enter, infinity);
        }
        if sort {
            for (a, b) in [(0, 1), (2, 3), (0, 2), (1, 3), (1, 2)] {
                let swap = self.s.m.cmp(c, pred::OLT, distances[b], distances[a]);
                let (da, db) = (distances[a], distances[b]);
                distances[a] = self.s.m.select(c, swap, db, da);
                distances[b] = self.s.m.select(c, swap, da, db);
                let (ia, ib) = (ids[a], ids[b]);
                ids[a] = self.s.m.select(c, swap, ib, ia);
                ids[b] = self.s.m.select(c, swap, ia, ib);
            }
        }
        ids
    }
}
