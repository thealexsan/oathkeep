use super::super::super::types::{Attr, Bin, Cast, Module, Term, V, pred};
use super::super::*;

use super::eval::run;
use super::{Kit, kit};

struct Gen {
    k: Kit,
    rng: u64,
    regs: Vec<V>,
    conds: Vec<V>,
    cur: usize,
    input: V,
    table: V,
    wave: V,
    /// The operands of the block's last read (to read the place again,
    /// and to store to it).
    last: Option<(V, V)>,
}

impl Gen {
    fn next(&mut self, n: u64) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) % n
    }

    fn reg(&mut self) -> V {
        let n = self.next(self.regs.len() as u64) as usize;
        self.regs[n]
    }

    /// A register or a small constant.
    fn value(&mut self) -> V {
        match self.next(4) {
            0 => {
                let k = [0u32, 1, 2, 31, u32::MAX, 0x8000_0000, 7][self.next(7) as usize];
                self.k.m.i32(k)
            }
            _ => self.reg(),
        }
    }

    fn cond(&mut self) -> V {
        let c = self.cur;
        let made = match self.next(6) {
            0 if !self.conds.is_empty() => {
                let n = self.next(self.conds.len() as u64) as usize;
                return self.conds[n];
            }
            1 if self.conds.len() > 1 => {
                let (a, b) = (
                    self.next(self.conds.len() as u64),
                    self.next(self.conds.len() as u64),
                );
                let op = [Bin::And, Bin::Or, Bin::Xor][self.next(3) as usize];
                self.k
                    .m
                    .bin(c, op, self.conds[a as usize], self.conds[b as usize])
            }
            2 if !self.conds.is_empty() => {
                let n = self.next(self.conds.len() as u64) as usize;
                let t = self.k.m.bool(true);
                self.k.m.bin(c, Bin::Xor, self.conds[n], t)
            }
            _ => {
                let p = pred::EQ + self.next(10) as u8;
                let (x, y) = (self.reg(), self.value());
                self.k.m.cmp(c, p, x, y)
            }
        };
        self.conds.push(made);
        made
    }

    fn statement(&mut self) {
        let c = self.cur;
        let d = self.next(self.regs.len() as u64) as usize;
        let i32t = self.k.m.int(32);
        let f32t = self.k.m.float();
        let r = match self.next(14) {
            0..=3 => {
                let op = [Bin::Add, Bin::Sub, Bin::Mul, Bin::And, Bin::Or, Bin::Xor]
                    [self.next(6) as usize];
                let (x, y) = (self.value(), self.value());
                self.k.m.bin(c, op, x, y)
            }
            4 => {
                let op = [Bin::Shl, Bin::LShr, Bin::AShr][self.next(3) as usize];
                let x = self.value();
                let by = self.next(32) as u32;
                let by = self.k.m.i32(by);
                self.k.m.bin(c, op, x, by)
            }
            5 | 6 => {
                // A guarded write: the old value where the guard fails.
                let g = self.cond();
                let x = self.value();
                self.k.m.select(self.cur, g, x, self.regs[d])
            }
            7 => {
                let (x, y) = (self.value(), self.value());
                let f = [self.k.pure, self.input, self.table, self.wave][self.next(4) as usize];
                self.k.m.call(c, f, vec![x, y])
            }
            8 | 9 => {
                let again = self.next(2) == 0;
                let (x, y) = match self.last {
                    Some(last) if again => last,
                    _ => (self.value(), self.value()),
                };
                self.last = Some((x, y));
                self.k.m.call(c, self.k.reads, vec![x, y])
            }
            10 => {
                // A store, half the time to the place read last.
                let (mut x, y) = (self.value(), self.value());
                let there = self.next(2) == 0;
                if let Some((a, b)) = self.last
                    && there
                {
                    x = self.k.m.bin(c, Bin::Xor, a, b);
                }
                self.k.m.call(c, self.k.writes, vec![x, y]);
                return;
            }
            11 => {
                // Float arithmetic, read back as bits.
                let (x, y) = (self.value(), self.value());
                let (x, y) = (
                    self.k.m.cast(c, Cast::Bitcast, x, f32t),
                    self.k.m.cast(c, Cast::Bitcast, y, f32t),
                );
                let op = [Bin::Add, Bin::Sub, Bin::Mul][self.next(3) as usize];
                let f = self.k.m.bin(c, op, x, y);
                if self.next(2) == 0 {
                    let p = self.next(16) as u8;
                    let t = self.k.m.cmp(c, p, f, y);
                    self.conds.push(t);
                }
                self.k.m.cast(c, Cast::Bitcast, f, i32t)
            }
            12 => {
                let g = self.cond();
                self.k.m.cast(self.cur, Cast::ZExt, g, i32t)
            }
            _ => {
                let x = self.value();
                let f = self.k.m.cast(c, Cast::Bitcast, x, f32t);
                self.k.m.cast(c, Cast::Bitcast, f, i32t)
            }
        };
        self.regs[d] = r;
    }

    fn code(&mut self, depth: u32) {
        for _ in 0..1 + self.next(6) {
            match self.next(8) {
                0 | 1 if depth > 0 => self.branch(depth - 1),
                2 if depth > 0 => self.repeat(depth - 1),
                _ => self.statement(),
            }
        }
    }

    /// if / else, every register joined.
    fn branch(&mut self, depth: u32) {
        let c = self.cond();
        let i32t = self.k.m.int(32);
        let (then, other, join) = (
            self.k.m.new_block(),
            self.k.m.new_block(),
            self.k.m.new_block(),
        );
        self.k.m.terminate(self.cur, Term::CondBr(c, then, other));
        let (before, known) = (self.regs.clone(), self.conds.len());
        self.cur = then;
        self.last = None;
        self.code(depth);
        let (then_regs, then_end) = (std::mem::replace(&mut self.regs, before), self.cur);
        self.k.m.terminate(then_end, Term::Br(join));
        self.conds.truncate(known);
        self.cur = other;
        self.last = None;
        if self.next(3) != 0 {
            self.code(depth);
        }
        let other_end = self.cur;
        self.k.m.terminate(other_end, Term::Br(join));
        self.conds.truncate(known);
        for (r, &t) in then_regs.iter().enumerate() {
            let p = self.k.m.phi(join, i32t);
            self.k
                .m
                .set_phi(p, vec![(t, then_end), (self.regs[r], other_end)]);
            self.regs[r] = p;
        }
        self.cur = join;
        self.last = None;
    }

    /// A loop that runs at most a few times, every register joined at
    /// its head.
    fn repeat(&mut self, depth: u32) {
        let i32t = self.k.m.int(32);
        let (head, exit) = (self.k.m.new_block(), self.k.m.new_block());
        let before = self.cur;
        self.k.m.terminate(before, Term::Br(head));
        let entering = self.regs.clone();
        let joins: Vec<V> = (0..self.regs.len())
            .map(|_| self.k.m.phi(head, i32t))
            .collect();
        let count = self.k.m.phi(head, i32t);
        self.regs = joins.clone();
        let known = self.conds.len();
        self.cur = head;
        self.last = None;
        self.code(depth);
        let one = self.k.m.i32(1);
        let next = self.k.m.bin(self.cur, Bin::Add, count, one);
        let limit = 1 + self.next(3) as u32;
        let limit = self.k.m.i32(limit);
        let mut more = self.k.m.cmp(self.cur, pred::IULT, next, limit);
        if self.next(2) == 0 {
            let c = self.cond();
            more = self.k.m.bin(self.cur, Bin::And, more, c);
        }
        let latch = self.cur;
        self.k.m.terminate(latch, Term::CondBr(more, head, exit));
        for (r, &p) in joins.iter().enumerate() {
            self.k
                .m
                .set_phi(p, vec![(entering[r], before), (self.regs[r], latch)]);
        }
        let zero = self.k.m.i32(0);
        self.k.m.set_phi(count, vec![(zero, before), (next, latch)]);
        self.conds.truncate(known);
        self.cur = exit;
        self.last = None;
    }
}

fn random_program(seed: u64) -> Module {
    let mut k = kit();
    let i32t = k.m.int(32);
    let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
    let input =
        k.m.function("dx.op.loadInput.i32", fty, Some(Attr::ReadNone), false);
    let table = k.m.function(
        "dx.op.cbufferLoadLegacy.i32",
        fty,
        Some(Attr::ReadOnly),
        false,
    );
    let wave = k.m.function(
        "dx.op.waveReadLaneFirst.i32",
        fty,
        Some(Attr::NoUnwind),
        false,
    );
    let b = k.b;
    let mut g = Gen {
        k,
        rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        regs: Vec::new(),
        conds: Vec::new(),
        cur: b,
        input,
        table,
        wave,
        last: None,
    };
    for r in 0..5 {
        let (x, y) = (g.k.m.i32(r), g.k.m.i32(r * 3));
        let v = g.k.m.call(b, input, vec![x, y]);
        g.regs.push(v);
    }
    g.code(3);
    g.code(2);
    // Every register is read at the end.
    for r in 0..g.regs.len() {
        let at = g.k.m.i32(r as u32);
        g.k.m.call(g.cur, g.k.writes, vec![at, g.regs[r]]);
    }
    g.k.m.terminate(g.cur, Term::Ret);
    g.k.m
}

#[test]
fn every_pass_leaves_what_random_programs_store() {
    let mut removed = 0;
    for seed in 1..=400u64 {
        let plain = random_program(seed);
        let inputs: Vec<Vec<u32>> = (0..6u32)
            .map(|n| {
                (0..7u32)
                    .map(|i| match n {
                        0 => 0,
                        1 => i,
                        2 => u32::MAX - i,
                        _ => {
                            (seed as u32 ^ n)
                                .wrapping_mul(0x9E37_79B9)
                                .rotate_left(i * 5)
                                ^ (i << 28)
                        }
                    })
                    .collect()
            })
            .collect();
        let expected: Vec<_> = inputs.iter().map(|i| run(&plain, i)).collect();
        let written: usize = plain
            .blocks
            .iter()
            .map(|b| b.body.len() + b.phis.len())
            .sum();
        for passes in [
            DEAD,
            FOLD,
            REUSE,
            FACTS,
            LOADS,
            FOLD | FACTS,
            FOLD | REUSE | DEAD,
            ALL,
        ] {
            let mut m = random_program(seed);
            m.optimise(passes);
            for (i, want) in inputs.iter().zip(&expected) {
                assert_eq!(&run(&m, i), want, "seed {seed}, passes {passes}");
            }
            if passes == ALL {
                let left: usize = m.blocks.iter().map(|b| b.body.len() + b.phis.len()).sum();
                removed += written - left;
            }
            m.write();
        }
    }
    // The programs are of the wasteful kind: the passes find work.
    assert!(removed > 4000, "{removed} instructions removed");
}
