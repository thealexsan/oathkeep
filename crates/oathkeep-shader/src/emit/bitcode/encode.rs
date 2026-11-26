use super::types::*;

pub(super) const MODULE_BLOCK: u64 = 8;
pub(super) const PARAMATTR_BLOCK: u64 = 9;
pub(super) const PARAMATTR_GROUP_BLOCK: u64 = 10;
pub(super) const CONSTANTS_BLOCK: u64 = 11;
pub(super) const FUNCTION_BLOCK: u64 = 12;
pub(super) const VALUE_SYMTAB_BLOCK: u64 = 14;
pub(super) const METADATA_BLOCK: u64 = 15;
pub(super) const TYPE_BLOCK: u64 = 17;

/// Calls `f` with every value `op` reads, in operand order.
pub(in crate::emit::bitcode) fn for_each_operand(op: &Op, mut f: impl FnMut(V)) {
    match op {
        Op::Bin(_, a, b) | Op::Cmp(_, a, b) | Op::Store(a, b) | Op::AtomicRmw(_, a, b) => {
            f(*a);
            f(*b);
        }
        Op::Cast(_, v) | Op::Extract(v, _) | Op::Load(v) => f(*v),
        Op::Select(a, b, c) | Op::CmpXchg(a, b, c) => {
            f(*a);
            f(*b);
            f(*c);
        }
        Op::Call(_, args) => args.iter().copied().for_each(f),
        Op::Phi(inc) => inc.iter().for_each(|x| f(x.0)),
        Op::Gep(_, p, idx) => {
            f(*p);
            idx.iter().copied().for_each(f);
        }
    }
}

/// A block's successors (at most two), without allocating.
pub(super) struct Succs {
    to: [usize; 2],
    len: usize,
}

impl std::ops::Deref for Succs {
    type Target = [usize];
    fn deref(&self) -> &[usize] {
        &self.to[..self.len]
    }
}

pub(in crate::emit::bitcode) fn successors(term: Option<Term>) -> Succs {
    match term {
        Some(Term::Br(t)) => Succs { to: [t, 0], len: 1 },
        Some(Term::CondBr(_, t, f)) => Succs { to: [t, f], len: 2 },
        _ => Succs { to: [0, 0], len: 0 },
    }
}

pub(super) fn chars(s: &str) -> Vec<u64> {
    s.bytes().map(u64::from).collect()
}

/// LLVM's signed VBR operand: magnitude shifted left, sign in bit 0.
pub(super) fn signed(v: i64) -> u64 {
    if v >= 0 {
        (v as u64) << 1
    } else {
        (v.unsigned_abs() << 1) | 1
    }
}

/// The bitstream: little-endian 32-bit words filled from bit 0.
#[derive(Default)]
pub(super) struct Writer {
    words: Vec<u32>,
    pub(super) cur: u64,
    bits: u32,
    /// Per open block: abbreviation width, index of its length word.
    stack: Vec<(u32, usize)>,
}

impl Writer {
    pub(super) fn width(&self) -> u32 {
        self.stack.last().map_or(2, |b| b.0)
    }

    pub(super) fn emit(&mut self, v: u64, width: u32) {
        debug_assert!(width == 64 || v >> width == 0);
        self.cur |= v << self.bits;
        self.bits += width;
        while self.bits >= 32 {
            self.words.push(self.cur as u32);
            self.cur >>= 32;
            self.bits -= 32;
        }
    }

    pub(super) fn vbr(&mut self, mut v: u64, width: u32) {
        let hi = 1u64 << (width - 1);
        while v >= hi {
            self.emit(v & (hi - 1) | hi, width);
            v >>= width - 1;
        }
        self.emit(v, width);
    }

    pub(super) fn align(&mut self) {
        if self.bits > 0 {
            self.words.push(self.cur as u32);
            self.cur = 0;
            self.bits = 0;
        }
    }

    pub(super) fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.emit(u64::from(x), 8);
        }
    }

    pub(super) fn enter(&mut self, id: u64, width: u32) {
        self.emit(1, self.width()); // ENTER_SUBBLOCK
        self.vbr(id, 8);
        self.vbr(u64::from(width), 4);
        self.align();
        self.words.push(0);
        self.stack.push((width, self.words.len()));
    }

    pub(super) fn exit(&mut self) {
        self.emit(0, self.width()); // END_BLOCK
        self.align();
        let (_, start) = self.stack.pop().expect("open block");
        self.words[start - 1] = (self.words.len() - start) as u32;
    }

    /// An unabbreviated record.
    pub(super) fn record(&mut self, code: u64, ops: &[u64]) {
        self.emit(3, self.width());
        self.vbr(code, 6);
        self.vbr(ops.len() as u64, 6);
        for &o in ops {
            self.vbr(o, 6);
        }
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        self.align();
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

