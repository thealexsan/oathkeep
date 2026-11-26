use super::super::super::types::{Attr, Bin, Cast, Module, Op, Term, V, pred};
use super::super::*;

use super::{Diamond, Kit, body, diamond, kit};

#[test]
fn unread_results_without_effect_are_dropped() {
    let Kit {
        mut m,
        b,
        pure,
        reads,
        writes,
    } = kit();
    let (one, two) = (m.i32(1), m.i32(2));
    let used = m.call(b, reads, vec![one, two]);
    let dead = m.call(b, reads, vec![two, one]);
    let chain = m.bin(b, Bin::Add, dead, one);
    let _unread = m.call(b, pure, vec![chain, two]);
    // A store's result is unread and stays, with what it reads.
    let kept = m.bin(b, Bin::Add, used, two);
    m.call(b, writes, vec![kept, one]);
    m.terminate(b, Term::Ret);
    m.optimise(DEAD);
    assert_eq!(body(&m, b), 3, "the load, the add and the store");
    m.write();
}

#[test]
fn branch_conditions_and_phi_cycles() {
    let Kit {
        mut m,
        b,
        reads,
        writes,
        ..
    } = kit();
    let i32t = m.int(32);
    let (zero, one) = (m.i32(0), m.i32(1));
    let (head, exit) = (m.new_block(), m.new_block());
    m.terminate(b, Term::Br(head));
    // A counter that decides the branch, and a register that only
    // feeds itself round the loop.
    let count = m.phi(head, i32t);
    let idle = m.phi(head, i32t);
    let next = m.bin(head, Bin::Add, count, one);
    let idle_next = m.call(head, reads, vec![idle, one]);
    let more = m.cmp(head, pred::IULT, next, one);
    m.terminate(head, Term::CondBr(more, head, exit));
    m.set_phi(count, vec![(zero, b), (next, head)]);
    m.set_phi(idle, vec![(zero, b), (idle_next, head)]);
    m.call(exit, writes, vec![zero, one]);
    m.terminate(exit, Term::Ret);
    m.optimise(DEAD);
    assert_eq!(m.blocks[head].phis.len(), 1, "the counter");
    assert_eq!(body(&m, head), 2, "the step and the test");
    assert_eq!(body(&m, exit), 1);
    m.write();
}

#[test]
fn wave_operations_have_no_effect_of_their_own() {
    let mut k = kit();
    let i32t = k.m.int(32);
    let fty = k.m.func_ty(i32t, vec![i32t, i32t]);
    let first = k.m.function(
        "dx.op.waveReadLaneFirst.i32",
        fty,
        Some(Attr::NoUnwind),
        false,
    );
    let barrier =
        k.m.function("dx.op.barrier", fty, Some(Attr::NoDuplicate), false);
    let one = k.m.i32(1);
    k.m.call(k.b, first, vec![one, one]);
    k.m.call(k.b, barrier, vec![one, one]);
    k.m.terminate(k.b, Term::Ret);
    k.m.optimise(DEAD);
    assert_eq!(body(&k.m, k.b), 1, "the barrier");
    // The declarations nothing calls any more are gone, and the call
    // that stayed names its function by the new number.
    let names: Vec<&str> = k.m.funcs.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["main", "dx.op.barrier"]);
    let Op::Call(f, _) = k.m.insts[k.m.blocks[k.b].body[0] as usize].op else {
        panic!("a call");
    };
    assert_eq!(k.m.funcs[f as usize].name, "dx.op.barrier");
    k.m.write();
}

/// The operands of each store left in block `b`.
fn stores(m: &Module, b: usize) -> Vec<Vec<V>> {
    m.blocks[b]
        .body
        .iter()
        .filter_map(|&i| match &m.insts[i as usize].op {
            Op::Call(g, args) if m.funcs[*g as usize].name.contains("Store") => {
                Some(args.clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn constants_and_identities_fold() {
    let Kit {
        mut m,
        b,
        reads,
        writes,
        ..
    } = kit();
    let (i32t, f32t) = (m.int(32), m.float());
    let (zero, one, all) = (m.i32(0), m.i32(1), m.i32(u32::MAX));
    let x = m.call(b, reads, vec![one, one]);
    // x + 0, -1 & x, x >> 0, x * 1, x | x: all x.
    let a = m.bin(b, Bin::Add, x, zero);
    let a = m.bin(b, Bin::And, all, a);
    let a = m.bin(b, Bin::LShr, a, zero);
    let a = m.bin(b, Bin::Mul, a, one);
    let a = m.bin(b, Bin::Or, a, a);
    // There and back.
    let f = m.cast(b, Cast::Bitcast, a, f32t);
    let a = m.cast(b, Cast::Bitcast, f, i32t);
    // Constants: (262168 >> 16) & 127 = 4, 7 & 31 = 7, -8 >>s 1 = -4.
    let k = m.i32(262168);
    let sixteen = m.i32(16);
    let k = m.bin(b, Bin::LShr, k, sixteen);
    let k127 = m.i32(127);
    let k = m.bin(b, Bin::And, k, k127);
    let minus8 = m.i32(-8i32 as u32);
    let s = m.bin(b, Bin::AShr, minus8, one);
    // A shift by the width is left alone.
    let k32 = m.i32(32);
    let wide = m.bin(b, Bin::Shl, one, k32);
    // A select between a value and itself, and on a constant.
    let c = m.cmp(b, pred::IULT, zero, one);
    let sel = m.select(b, c, a, wide);
    let same = m.select(b, c, sel, sel);
    m.call(b, writes, vec![same, k]);
    m.call(b, writes, vec![s, wide]);
    m.terminate(b, Term::Ret);
    m.optimise(FOLD | DEAD);
    assert_eq!(body(&m, b), 4, "the read, the wide shift and two stores");
    let four = m.i32(4);
    let minus4 = m.i32(-4i32 as u32);
    let stores = stores(&m, b);
    assert_eq!(stores, [vec![x, four], vec![minus4, wide]]);
    m.write();
}

#[test]
fn float_arithmetic_is_not_folded() {
    let Kit {
        mut m, b, writes, ..
    } = kit();
    let f32t = m.float();
    let fty = m.func_ty(f32t, vec![f32t, f32t]);
    let input = m.function("dx.op.loadInput.f32", fty, Some(Attr::ReadNone), false);
    let (zero, one, two) = (m.f32(0), m.f32(1.0f32.to_bits()), m.f32(2.0f32.to_bits()));
    let x = m.call(b, input, vec![zero, zero]);
    let y = m.call(b, input, vec![one, one]);
    let a = m.bin(b, Bin::Add, x, zero);
    let p = m.bin(b, Bin::Mul, a, one);
    // The same sum the other way round is another instruction.
    let xy = m.bin(b, Bin::Add, x, y);
    let yx = m.bin(b, Bin::Add, y, x);
    // Constants compare here: 1 < 2, and nothing equals a NaN.
    let nan = m.f32(f32::NAN.to_bits());
    let lt = m.cmp(b, pred::OLT, one, two);
    let eq = m.cmp(b, pred::OEQ, nan, nan);
    let une = m.cmp(b, pred::UNE, nan, one);
    let sel = m.select(b, lt, xy, yx);
    let sel = m.select(b, eq, p, sel);
    let sel = m.select(b, une, sel, p);
    m.call(b, writes, vec![sel, p]);
    m.terminate(b, Term::Ret);
    m.optimise(ALL);
    // Two inputs, x + 0.0, * 1.0, x + y and the store: y + x and the
    // selects are gone.
    assert_eq!(body(&m, b), 6);
    assert_eq!(stores(&m, b), [[xy, p]]);
    m.write();
}

/// entry: `c = x < y`, branch to `then` or `other`, which join.
#[test]
fn a_value_is_reused_below_its_block_and_not_beside_it() {
    let Diamond {
        k:
            Kit {
                mut m,
                b,
                pure,
                writes,
                ..
            },
        x,
        y,
        then,
        other,
        join,
        ..
    } = diamond();
    // The entry's value again in a branch; a branch's value again in
    // the other branch and in the join (neither is on every path).
    let top = m.call(b, pure, vec![x, y]);
    m.call(b, writes, vec![top, top]);
    let again = m.call(then, pure, vec![x, y]);
    let side = m.bin(then, Bin::Add, y, x);
    m.call(then, writes, vec![again, side]);
    let beside = m.bin(other, Bin::Add, x, y);
    m.call(other, writes, vec![beside, beside]);
    let after = m.bin(join, Bin::Add, x, y);
    m.call(join, writes, vec![after, after]);
    m.optimise(REUSE);
    assert_eq!(stores(&m, then), [[top, side]]);
    assert_eq!(body(&m, then), 2);
    assert_eq!(body(&m, other), 2);
    assert_eq!(body(&m, join), 2);
    m.write();
}

#[test]
fn a_branch_taken_says_what_its_condition_is() {
    let Diamond {
        k:
            Kit {
                mut m,
                b,
                reads,
                writes,
                ..
            },
        x,
        y,
        c,
        then,
        other,
        join,
    } = diamond();
    let three = m.i32(3);
    let d = m.call(b, reads, vec![three, three]);
    let d = m.cmp(b, pred::NE, d, three);
    let both = m.bin(b, Bin::And, c, d);
    // Behind the branch: the condition again (a guard), its opposite
    // comparison, and a guarded write.
    let guard = m.bin(then, Bin::And, c, d);
    let old = m.select(then, guard, x, y);
    let not = m.cmp(then, pred::IUGE, x, y);
    let new = m.select(then, not, y, old);
    m.call(then, writes, vec![new, new]);
    let kept = m.select(other, c, x, y);
    m.call(other, writes, vec![kept, kept]);
    // After the join nothing is known.
    let open = m.select(join, c, x, y);
    m.call(join, writes, vec![open, open]);
    m.optimise(FOLD | FACTS);
    // then: select(d, x, y) and the store; other: y is stored.
    assert_eq!(body(&m, then), 2);
    let V::Inst(sel) = stores(&m, then)[0][0] else {
        panic!()
    };
    assert!(matches!(m.insts[sel as usize].op, Op::Select(g, a, z) if (g, a, z) == (d, x, y)));
    assert_eq!(stores(&m, other), [[y, y]]);
    assert_eq!(body(&m, join), 2);
    let _ = both;
    m.write();

    // Both terms of an `and` that held hold behind it.
    let Diamond {
        k:
            Kit {
                mut m,
                b,
                reads,
                writes,
                ..
            },
        x,
        y,
        c,
        ..
    } = diamond();
    let three = m.i32(3);
    let d = m.call(b, reads, vec![three, three]);
    let d = m.cmp(b, pred::NE, d, three);
    let both = m.bin(b, Bin::And, c, d);
    let (inner, out) = (m.new_block(), m.new_block());
    m.terminate(b, Term::CondBr(both, inner, out));
    let a = m.select(inner, c, x, y);
    let z = m.select(inner, d, a, y);
    m.call(inner, writes, vec![z, z]);
    m.terminate(inner, Term::Br(out));
    // Where the `and` did not hold either term may.
    let w = m.select(out, d, x, y);
    m.call(out, writes, vec![w, w]);
    m.terminate(out, Term::Ret);
    m.optimise(FOLD | FACTS);
    assert_eq!(stores(&m, inner), [[x, x]]);
    assert_eq!(body(&m, out), 2);
    m.write();
}

#[test]
fn a_branch_on_a_constant_goes_one_way() {
    let Diamond {
        k: Kit {
            mut m, b, writes, ..
        },
        x,
        y,
        c,
        then,
        other,
        join,
    } = diamond();
    let i32t = m.int(32);
    // Behind the branch the same test again: its other side is never
    // run, and the join behind it has one value.
    let (deep, never, meet) = (m.new_block(), m.new_block(), m.new_block());
    m.terminate(then, Term::CondBr(c, deep, never));
    m.terminate(deep, Term::Br(meet));
    m.call(never, writes, vec![y, y]);
    m.terminate(never, Term::Br(meet));
    let p = m.phi(meet, i32t);
    m.set_phi(p, vec![(x, deep), (y, never)]);
    m.call(meet, writes, vec![p, p]);
    m.terminate(meet, Term::Br(join));
    let q = m.phi(join, i32t);
    m.set_phi(q, vec![(p, meet), (y, other)]);
    m.call(join, writes, vec![q, q]);
    m.optimise(ALL);
    assert!(matches!(m.blocks[then].term, Some(Term::Br(t)) if t == deep));
    assert_eq!(stores(&m, meet), [[x, x]]);
    assert!(m.blocks[meet].phis.is_empty());
    assert_eq!(body(&m, never), 0, "emptied: nothing reaches it");
    let Op::Phi(incoming) = &m.insts[m.blocks[join].phis[0] as usize].op else {
        panic!()
    };
    assert_eq!(incoming, &vec![(x, meet), (y, other)]);
    let _ = b;
    m.write();
}

#[test]
fn a_read_is_reused_within_its_block_until_a_store() {
    let Diamond {
        k:
            Kit {
                mut m,
                b,
                reads,
                writes,
                ..
            },
        x,
        y,
        then,
        ..
    } = diamond();
    let i32t = m.int(32);
    // The entry reads (1, 1) for x: again in the entry it is x; in the
    // next block it is read again.
    let one = m.i32(1);
    let again = m.call(b, reads, vec![one, one]);
    m.call(b, writes, vec![again, y]);
    let after = m.call(b, reads, vec![one, one]);
    let twice = m.call(b, reads, vec![one, one]);
    let sum = m.bin(b, Bin::Add, after, twice);
    m.call(b, writes, vec![sum, sum]);
    let below = m.call(then, reads, vec![one, one]);
    m.call(then, writes, vec![below, below]);
    // A constant buffer cannot change: one load for the function.
    let cfty = m.func_ty(i32t, vec![i32t, i32t]);
    let cb = m.function(
        "dx.op.cbufferLoadLegacy.i32",
        cfty,
        Some(Attr::ReadOnly),
        false,
    );
    let top = m.call(b, cb, vec![one, one]);
    m.call(b, writes, vec![top, top]);
    let under = m.call(then, cb, vec![one, one]);
    m.call(then, writes, vec![under, under]);
    m.optimise(REUSE | LOADS);
    let stores = stores(&m, b);
    assert_eq!(stores[0], [x, y], "the second read is the first");
    let V::Inst(sum) = stores[1][0] else { panic!() };
    assert!(
        matches!(m.insts[sum as usize].op, Op::Bin(Bin::Add, a, z) if a == after && z == after),
        "read again after the store, once"
    );
    assert_eq!(stores[2], [top, top]);
    assert_eq!(
        super::passes::stores(&m, then),
        [vec![below, below], vec![top, top]]
    );
    m.write();
}

#[test]
fn a_join_of_one_value_and_two_equal_joins() {
    let Diamond {
        k: Kit { mut m, writes, .. },
        x,
        y,
        then,
        other,
        join,
        ..
    } = diamond();
    let i32t = m.int(32);
    let one = m.phi(join, i32t);
    m.set_phi(one, vec![(x, then), (x, other)]);
    let p = m.phi(join, i32t);
    m.set_phi(p, vec![(x, then), (y, other)]);
    let q = m.phi(join, i32t);
    m.set_phi(q, vec![(y, other), (x, then)]);
    m.call(join, writes, vec![one, p]);
    let sum = m.bin(join, Bin::Add, p, q);
    m.call(join, writes, vec![sum, q]);
    m.optimise(FOLD | REUSE);
    assert_eq!(
        m.blocks[join].phis,
        [match p {
            V::Inst(i) => i,
            _ => unreachable!(),
        }]
    );
    let stores = self::stores(&m, join);
    assert_eq!(stores[0], [x, p]);
    assert_eq!(stores[1][1], p);
    m.write();
}
