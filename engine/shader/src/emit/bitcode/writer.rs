use super::encode::{chars, for_each_operand, signed, successors, Writer, CONSTANTS_BLOCK, FUNCTION_BLOCK, METADATA_BLOCK, MODULE_BLOCK, PARAMATTR_BLOCK, PARAMATTR_GROUP_BLOCK, TYPE_BLOCK, VALUE_SYMTAB_BLOCK};
use super::types::*;

impl Module {
    /// The module as bitcode.
    /// Which instructions are written: every body instruction, the branch
    /// conditions, and whatever they read, transitively (a phi nothing reads
    /// is dropped). A worklist visits each instruction once, so a long chain
    /// of merges costs one pass, not one pass per merge.
    pub(super) fn live(&self) -> Vec<bool> {
        let mut live = vec![false; self.insts.len()];
        let mut work: Vec<u32> = Vec::new();
        let mark = |live: &mut [bool], work: &mut Vec<u32>, v: V| {
            if let V::Inst(i) = v
                && !live[i as usize]
            {
                live[i as usize] = true;
                work.push(i);
            }
        };
        for b in &self.blocks {
            for &i in &b.body {
                mark(&mut live, &mut work, V::Inst(i));
            }
            if let Some(Term::CondBr(c, ..)) = b.term {
                mark(&mut live, &mut work, c);
            }
        }
        while let Some(i) = work.pop() {
            for_each_operand(&self.insts[i as usize].op, |v| {
                mark(&mut live, &mut work, v)
            });
        }
        live
    }

    pub fn write(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(b"BC\xC0\xDE");
        w.enter(MODULE_BLOCK, 3);
        w.record(1, &[1]); // VERSION: relative value ids

        // Attribute groups 1-4 and one list per group.
        w.enter(PARAMATTR_GROUP_BLOCK, 3);
        for (id, kinds) in [
            (1, &[18u64, 20][..]),
            (2, &[18]),
            (3, &[18, 21]),
            (4, &[12, 18]),
        ] {
            let mut r = vec![id, 0xFFFF_FFFF];
            for &k in kinds {
                r.extend([0, k]);
            }
            w.record(3, &r);
        }
        w.exit();
        w.enter(PARAMATTR_BLOCK, 3);
        for id in 1..=4 {
            w.record(2, &[id]);
        }
        w.exit();

        w.enter(TYPE_BLOCK, 4);
        w.record(1, &[self.types.len() as u64]);
        for t in &self.types {
            match t {
                TypeDef::Void => w.record(2, &[]),
                TypeDef::Float => w.record(3, &[]),
                TypeDef::Double => w.record(4, &[]),
                TypeDef::Int(b) => w.record(7, &[u64::from(*b)]),
                TypeDef::Ptr(p, space) => w.record(8, &[u64::from(p.0), u64::from(*space)]),
                TypeDef::Array(n, e) => w.record(11, &[u64::from(*n), u64::from(e.0)]),
                TypeDef::Struct(name, fields) => {
                    let mut r = vec![0];
                    r.extend(fields.iter().map(|f| u64::from(f.0)));
                    match name {
                        Some(n) => {
                            w.record(19, &chars(n));
                            w.record(20, &r);
                        }
                        None => w.record(18, &r),
                    }
                }
                TypeDef::Func(ret, params) => {
                    let mut r = vec![0, u64::from(ret.0)];
                    r.extend(params.iter().map(|p| u64::from(p.0)));
                    w.record(21, &r);
                }
            }
        }
        w.exit();

        w.record(2, &chars("dxil-ms-dx"));
        w.record(
            3,
            &chars(
                "e-m:e-p:32:32-i1:32-i8:32-i16:32-i32:32-i64:64-f16:32-f32:32-f64:64-n8:16:32:64",
            ),
        );
        let first_const = (self.globals.len() + self.funcs.len()) as u64;
        for g in &self.globals {
            // [type, address space << 2 | explicit type | constant, init id
            // + 1, linkage (external 0, internal 3), log2 alignment + 1,
            // section]
            let (constant, init, linkage) = match g.init {
                Some(i) => (1, first_const + u64::from(i) + 1, 3),
                None => (0, 0, 0),
            };
            w.record(
                7,
                &[
                    u64::from(g.ty.0),
                    u64::from(g.space) << 2 | 2 | constant,
                    init,
                    linkage,
                    3,
                    0,
                ],
            );
        }
        for f in &self.funcs {
            // [type, cc, isproto, linkage, paramattr, align, section,
            // visibility, gc, unnamed_addr, prologue, dllstorage, comdat, prefix]
            let attr = f.attr.map_or(0, |a| a as u64);
            w.record(
                8,
                &[
                    u64::from(f.ty.0),
                    0,
                    u64::from(!f.body),
                    0,
                    attr,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ],
            );
        }

        // Value ids: globals, functions, constants, instructions.
        let globals = self.globals.len() as u32;
        let funcs = globals + self.funcs.len() as u32;
        let const_id = |i: u32| u64::from(funcs + i);
        w.enter(CONSTANTS_BLOCK, 4);
        let mut current = None;
        for (ty, c) in &self.consts {
            if current != Some(*ty) {
                w.record(1, &[u64::from(ty.0)]);
                current = Some(*ty);
            }
            match c {
                Const::Undef => w.record(3, &[]),
                Const::Int(v) => w.record(4, &[signed(*v)]),
                Const::Float(bits) => w.record(6, &[u64::from(*bits)]),
                Const::Aggregate(ids) => {
                    let r: Vec<u64> = ids.iter().map(|&i| const_id(i)).collect();
                    w.record(7, &r);
                }
            }
        }
        w.exit();

        // Phis nothing reads are left out (one register per phi makes many).
        let live = self.live();

        // Blocks are laid out in reverse postorder, so every block follows
        // its forward predecessors (drivers take a branch to an earlier
        // block for a loop's back edge).
        let succs = |b: usize| successors(self.blocks[b].term);
        let mut seen = vec![false; self.blocks.len()];
        let mut post = Vec::new();
        let mut stack = vec![(0usize, 0usize)];
        seen[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            top.1 += 1;
            match succs(b).get(k) {
                Some(&s) if !seen[s] => {
                    seen[s] = true;
                    stack.push((s, 0));
                }
                Some(_) => {}
                None => {
                    post.push(b);
                    stack.pop();
                }
            }
        }
        post.reverse();
        let mut pos = vec![u64::MAX; self.blocks.len()];
        for (n, &b) in post.iter().enumerate() {
            pos[b] = n as u64;
        }
        // An empty latch (only a branch back to its loop header) is folded
        // into its one predecessor, so the exiting block is the latch as in
        // DXC's loops; the AMD driver hangs on divergent loops otherwise.
        let mut preds = vec![Vec::new(); self.blocks.len()];
        for &b in &post {
            for &s in succs(b).iter() {
                preds[s].push(b);
            }
        }
        let mut fold = vec![None; self.blocks.len()];
        for &b in &post {
            let blk = &self.blocks[b];
            if let (Some(Term::Br(h)), true, [p]) = (
                blk.term,
                blk.phis.iter().all(|&i| !live[i as usize]) && blk.body.is_empty(),
                &preds[b][..],
            ) && pos[h] < pos[b]
                && !succs(*p).contains(&h)
            {
                fold[b] = Some((*p, h));
            }
        }
        let layout: Vec<usize> = post.into_iter().filter(|&b| fold[b].is_none()).collect();
        let mut pos = vec![u64::MAX; self.blocks.len()];
        for (n, &b) in layout.iter().enumerate() {
            pos[b] = n as u64;
        }
        // Branch targets and phi blocks through folded latches.
        let target = |t: usize| fold[t].map_or(pos[t], |(_, h)| pos[h]);
        let from = |b: usize| fold[b].map_or(pos[b], |(p, _)| pos[p]);

        // Instruction value ids, in layout order.
        let base = funcs + self.consts.len() as u32;
        let mut ids = vec![u32::MAX; self.insts.len()];
        let mut next = base;
        for b in layout.iter().map(|&b| &self.blocks[b]) {
            for &i in b.phis.iter().filter(|&&i| live[i as usize]).chain(&b.body) {
                if !self.is_void(self.insts[i as usize].ty) {
                    ids[i as usize] = next;
                    next += 1;
                }
            }
        }
        let abs = |v: V| -> u32 {
            match v {
                V::Global(i) => i,
                V::Func(i) => globals + i,
                V::Const(i) => funcs + i,
                V::Inst(i) => {
                    let id = ids[i as usize];
                    assert_ne!(id, u32::MAX, "use of a void value");
                    id
                }
            }
        };

        w.enter(METADATA_BLOCK, 3);
        for m in &self.mds {
            match m {
                MdDef::Str(s) => w.record(1, &chars(s)),
                MdDef::Value(ty, v) => w.record(2, &[u64::from(ty.0), u64::from(abs(*v))]),
                MdDef::Node(ops) => {
                    let r: Vec<u64> = ops
                        .iter()
                        .map(|o| o.map_or(0, |m| u64::from(m.0) + 1))
                        .collect();
                    w.record(3, &r);
                }
            }
        }
        for (name, nodes) in &self.named {
            w.record(4, &chars(name));
            let r: Vec<u64> = nodes.iter().map(|m| u64::from(m.0)).collect();
            w.record(10, &r);
        }
        w.exit();
        // The metadata kinds an LLVM 3.7 module declares, as DXC writes them.
        w.enter(METADATA_BLOCK, 3);
        for (id, kind) in [
            "dbg",
            "tbaa",
            "prof",
            "fpmath",
            "range",
            "tbaa.struct",
            "invariant.load",
            "alias.scope",
            "noalias",
            "nontemporal",
            "llvm.mem.parallel_loop_access",
            "nonnull",
            "dereferenceable",
            "dereferenceable_or_null",
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = vec![id as u64];
            r.extend(chars(kind));
            w.record(6, &r);
        }
        w.exit();

        w.enter(VALUE_SYMTAB_BLOCK, 4);
        let names = self.globals.iter().map(|g| &g.name);
        for (i, name) in names.chain(self.funcs.iter().map(|f| &f.name)).enumerate() {
            let mut r = vec![i as u64];
            r.extend(chars(name));
            w.record(1, &r);
        }
        w.exit();

        w.enter(FUNCTION_BLOCK, 4);
        w.record(1, &[layout.len() as u64]);
        let mut inst_id = base;
        // The operands of variable-length records, reused across them.
        let mut r: Vec<u64> = Vec::new();
        for b in layout.iter().map(|&b| &self.blocks[b]) {
            for &i in b.phis.iter().filter(|&&i| live[i as usize]).chain(&b.body) {
                let inst = &self.insts[i as usize];
                let rel = |v: V| {
                    let a = abs(v);
                    assert!(a < inst_id, "forward reference outside a phi");
                    u64::from(inst_id - a)
                };
                match &inst.op {
                    Op::Bin(op, a, b) => w.record(2, &[rel(*a), rel(*b), *op as u64]),
                    Op::Cast(op, v) => w.record(3, &[rel(*v), u64::from(inst.ty.0), *op as u64]),
                    Op::Cmp(p, a, b) => w.record(28, &[rel(*a), rel(*b), u64::from(*p)]),
                    Op::Select(c, t, f) => w.record(29, &[rel(*t), rel(*f), rel(*c)]),
                    Op::Extract(a, idx) => w.record(26, &[rel(*a), u64::from(*idx)]),
                    Op::Call(f, args) => {
                        r.clear();
                        r.extend([0, 1 << 15, u64::from(self.funcs[*f as usize].ty.0)]);
                        r.push(rel(V::Func(*f)));
                        r.extend(args.iter().map(|&a| rel(a)));
                        w.record(34, &r);
                    }
                    Op::Phi(incoming) => {
                        r.clear();
                        r.push(u64::from(inst.ty.0));
                        for &(v, b) in incoming {
                            r.push(signed(i64::from(inst_id) - i64::from(abs(v))));
                            r.push(from(b));
                        }
                        w.record(16, &r);
                    }
                    Op::Gep(ty, p, idx) => {
                        r.clear();
                        r.extend([0, u64::from(ty.0), rel(*p)]);
                        r.extend(idx.iter().map(|&i| rel(i)));
                        w.record(43, &r);
                    }
                    // Alignment 4 (log2 + 1), not volatile.
                    Op::Load(p) => w.record(20, &[rel(*p), u64::from(inst.ty.0), 3, 0]),
                    Op::Store(p, v) => w.record(44, &[rel(*p), rel(*v), 3, 0]),
                    // Sequentially consistent, across threads.
                    Op::AtomicRmw(op, p, v) => {
                        w.record(38, &[rel(*p), rel(*v), *op as u64, 0, 6, 1])
                    }
                    Op::CmpXchg(p, c, n) => {
                        w.record(46, &[rel(*p), rel(*c), rel(*n), 0, 6, 1, 6, 0]);
                    }
                }
                if !self.is_void(inst.ty) {
                    inst_id += 1;
                }
            }
            match b.term.expect("unterminated block") {
                Term::Ret => w.record(10, &[]),
                Term::Br(t) => w.record(11, &[target(t)]),
                Term::CondBr(c, t, f) => {
                    let a = abs(c);
                    assert!(a < inst_id);
                    w.record(11, &[target(t), target(f), u64::from(inst_id - a)]);
                }
            }
        }
        w.exit();
        w.exit();
        w.finish()
    }
}
