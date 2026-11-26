use super::encode::{Writer, for_each_operand, signed, successors};
use super::types::*;

#[test]
fn vbr_and_blocks() {
    let mut w = Writer::default();
    w.vbr(5, 6);
    w.vbr(100, 6); // two chunks: 100 = 0b1100100 -> 0b100100 | 32, then 3
    assert_eq!(w.cur, 5 | (0b10_0100 << 6) | (3 << 12));
    let mut w = Writer::default();
    w.enter(8, 3);
    w.record(1, &[1]);
    w.exit();
    let bytes = w.finish();
    // header word, length word, one record word, end word.
    assert_eq!(bytes.len() % 4, 0);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
    assert_eq!(signed(-3), 7);
    assert_eq!(signed(2), 4);
}

/// A structured shader's shape: `stages` if/else diamonds in a row, each
/// merge with one phi per register (as the lowering makes them), then a
/// loop, and a tail that reads only the first `read` registers.
fn synthetic(regs: usize, stages: usize, read: usize) -> Module {
    let mut m = Module::default();
    let i32t = m.int(32);
    let void = m.void();
    let fty = m.func_ty(i32t, vec![i32t, i32t]);
    let f = m.function("dx.op.synthetic", fty, Some(Attr::ReadNone), false);
    let main_ty = m.func_ty(void, vec![]);
    m.function("main", main_ty, None, true);
    let mut b = m.new_block();
    let mut cur: Vec<V> = (0..regs).map(|r| m.i32(r as u32)).collect();
    for s in 0..stages {
        let c = m.cmp(b, pred::EQ, cur[0], cur[s % regs]);
        let (t, e, j) = (m.new_block(), m.new_block(), m.new_block());
        m.terminate(b, Term::CondBr(c, t, e));
        let x = m.call(t, f, vec![cur[s % regs], cur[(s + 1) % regs]]);
        m.terminate(t, Term::Br(j));
        m.terminate(e, Term::Br(j));
        cur = (0..regs)
            .map(|r| {
                let p = m.phi(j, i32t);
                let tv = if r == s % regs { x } else { cur[r] };
                m.set_phi(p, vec![(tv, t), (cur[r], e)]);
                p
            })
            .collect();
        b = j;
    }
    // A loop: header phis fed back from an empty latch (which is folded).
    let (head, body, latch, exit) =
        (m.new_block(), m.new_block(), m.new_block(), m.new_block());
    m.terminate(b, Term::Br(head));
    let phis: Vec<V> = (0..regs).map(|_| m.phi(head, i32t)).collect();
    let one = m.i32(1);
    let stepped: Vec<V> = phis
        .iter()
        .map(|&p| m.bin(body, Bin::Add, p, one))
        .collect();
    let c = m.cmp(body, pred::IULT, stepped[0], cur[0]);
    m.terminate(head, Term::Br(body));
    m.terminate(body, Term::CondBr(c, latch, exit));
    m.terminate(latch, Term::Br(head));
    for r in 0..regs {
        m.set_phi(phis[r], vec![(cur[r], b), (stepped[r], latch)]);
    }
    let mut acc = stepped[0];
    for &v in &stepped[1..read.max(1)] {
        acc = m.bin(exit, Bin::Add, acc, v);
    }
    let _ = m.call(exit, f, vec![acc, cur[0]]);
    m.terminate(exit, Term::Ret);
    m
}

/// FNV-1a, to pin a written module's exact bytes.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Liveness as `write` computed it before the worklist: whole passes over
/// the instructions until one changes nothing.
fn live_by_passes(m: &Module) -> Vec<bool> {
    let mut live = vec![false; m.insts.len()];
    for b in &m.blocks {
        for &i in &b.body {
            live[i as usize] = true;
        }
        if let Some(Term::CondBr(V::Inst(c), ..)) = b.term {
            live[c as usize] = true;
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for (i, inst) in m.insts.iter().enumerate() {
            if live[i] {
                for_each_operand(&inst.op, |v| {
                    if let V::Inst(j) = v
                        && !live[j as usize]
                    {
                        live[j as usize] = true;
                        changed = true;
                    }
                });
            }
        }
    }
    live
}

#[test]
fn worklist_liveness_matches_passes() {
    for (regs, stages, read) in [(1, 0, 1), (1, 1, 1), (4, 3, 1), (8, 17, 3), (16, 40, 16)] {
        let m = synthetic(regs, stages, read);
        let live = m.live();
        assert_eq!(live, live_by_passes(&m), "{regs} regs x {stages} stages");
    }
}

#[test]
fn unread_phis_are_dropped() {
    let mut m = Module::default();
    let i32t = m.int(32);
    let (entry, join) = (m.new_block(), m.new_block());
    let (a, b) = (m.i32(1), m.i32(2));
    m.terminate(entry, Term::Br(join));
    let read = m.phi(join, i32t);
    let unread = m.phi(join, i32t);
    // A cycle of phis that only feed each other stays dead.
    let cyc_a = m.phi(join, i32t);
    let cyc_b = m.phi(join, i32t);
    m.set_phi(read, vec![(a, entry)]);
    m.set_phi(unread, vec![(b, entry)]);
    m.set_phi(cyc_a, vec![(cyc_b, entry)]);
    m.set_phi(cyc_b, vec![(cyc_a, entry)]);
    let sum = m.bin(join, Bin::Add, read, b);
    m.terminate(join, Term::Ret);
    let ids = |v: V| match v {
        V::Inst(i) => i as usize,
        _ => unreachable!(),
    };
    let live = m.live();
    assert!(live[ids(read)] && live[ids(sum)]);
    assert!(!live[ids(unread)] && !live[ids(cyc_a)] && !live[ids(cyc_b)]);
    assert_eq!(live, live_by_passes(&m));
}

#[test]
fn empty_latch_with_dead_phis_is_folded() {
    // Two identical loops except one has an unread phi in its empty latch;
    // both must fold the latch and emit identical bitcode.
    let build = |with_dead_phi: bool| {
        let mut m = Module::default();
        let i32t = m.int(32);
        let void = m.void();
        let fty = m.func_ty(void, vec![i32t]);
        let f = m.function("sink", fty, Some(Attr::NoUnwind), false);
        let main_ty = m.func_ty(void, vec![]);
        m.function("main", main_ty, None, true);
        let (entry, head, body, latch, exit) = (
            m.new_block(),
            m.new_block(),
            m.new_block(),
            m.new_block(),
            m.new_block(),
        );
        let (z, one, ten) = (m.i32(0), m.i32(1), m.i32(10));
        m.terminate(entry, Term::Br(head));
        let i = m.phi(head, i32t);
        m.terminate(head, Term::Br(body));
        let next = m.bin(body, Bin::Add, i, one);
        let cond = m.cmp(body, pred::IULT, next, ten);
        m.terminate(body, Term::CondBr(cond, latch, exit));
        if with_dead_phi {
            let dead = m.phi(latch, i32t);
            m.set_phi(dead, vec![(next, body)]);
        }
        m.terminate(latch, Term::Br(head));
        m.set_phi(i, vec![(z, entry), (next, latch)]);
        let _ = m.call(exit, f, vec![next]);
        m.terminate(exit, Term::Ret);
        m.write()
    };
    assert_eq!(build(false), build(true));
}

#[test]
fn successors_of_each_terminator() {
    assert!(successors(None).is_empty());
    assert!(successors(Some(Term::Ret)).is_empty());
    assert_eq!(&*successors(Some(Term::Br(7))), &[7]);
    assert_eq!(&*successors(Some(Term::CondBr(V::Const(0), 3, 5))), &[3, 5]);
}

/// The written bytes are pinned to what the writer produced before the
/// worklist and the reused record buffers.
#[test]
fn written_bytes_unchanged() {
    for ((regs, stages, read), len, hash) in [
        ((64, 64, 64), 38_916, 0x9586_730c_f953_4a45),
        ((128, 256, 32), 295_952, 0x89e5_de2d_64f0_8785),
    ] {
        let bytes = synthetic(regs, stages, read).write();
        assert_eq!((bytes.len(), fnv(&bytes)), (len, hash), "{regs}x{stages}");
    }
}

#[test]
#[ignore = "benchmark: cargo test --release -p oathkeep-shader bitcode -- --ignored --nocapture"]
fn bench_write() {
    for (regs, stages, read) in [(64, 64, 64), (128, 256, 32), (256, 512, 256)] {
        let m = synthetic(regs, stages, read);
        let bytes = m.write();
        let runs = 5;
        let t = std::time::Instant::now();
        for _ in 0..runs {
            std::hint::black_box(m.write());
        }
        let per = t.elapsed() / runs;
        println!(
            "write {regs} regs x {stages} stages ({} insts): {per:?}, {} bytes, fnv {:#018x}",
            m.insts.len(),
            bytes.len(),
            fnv(&bytes)
        );
    }
}
