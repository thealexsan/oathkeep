mod eval;
mod fuzz;
mod passes;

use super::super::types::{Attr, Module, Term, V, pred};

pub(super) struct Kit {
    pub(super) m: Module,
    b: usize,
    pure: V,
    reads: V,
    writes: V,
}

pub(super) fn kit() -> Kit {
    let mut m = Module::default();
    let i32t = m.int(32);
    let void = m.void();
    let fty = m.func_ty(i32t, vec![i32t, i32t]);
    m.ptr(fty);
    let pure = m.function("dx.op.binary.i32", fty, Some(Attr::ReadNone), false);
    let reads = m.function("dx.op.bufferLoad.i32", fty, Some(Attr::ReadOnly), false);
    let writes = m.function("dx.op.bufferStore.i32", fty, Some(Attr::NoUnwind), false);
    let main_ty = m.func_ty(void, vec![]);
    m.ptr(main_ty);
    m.function("main", main_ty, None, true);
    let b = m.new_block();
    Kit {
        m,
        b,
        pure,
        reads,
        writes,
    }
}

pub(super) fn body(m: &Module, b: usize) -> usize {
    m.blocks[b].body.len()
}

pub(super) struct Diamond {
    k: Kit,
    x: V,
    y: V,
    pub(super) c: V,
    then: usize,
    other: usize,
    join: usize,
}

pub(super) fn diamond() -> Diamond {
    let mut k = kit();
    let (one, two) = (k.m.i32(1), k.m.i32(2));
    let x = k.m.call(k.b, k.reads, vec![one, one]);
    let y = k.m.call(k.b, k.reads, vec![two, two]);
    let c = k.m.cmp(k.b, pred::IULT, x, y);
    let (then, other, join) = (k.m.new_block(), k.m.new_block(), k.m.new_block());
    k.m.terminate(k.b, Term::CondBr(c, then, other));
    k.m.terminate(then, Term::Br(join));
    k.m.terminate(other, Term::Br(join));
    k.m.terminate(join, Term::Ret);
    Diamond {
        k,
        x,
        y,
        c,
        then,
        other,
        join,
    }
}

