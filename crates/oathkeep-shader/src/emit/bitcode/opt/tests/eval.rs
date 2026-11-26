use super::super::super::types::{Bin, Cast, Const, Module, Op, Term, V, pred};
use super::super::*;

pub(super) fn run(m: &Module, inputs: &[u32]) -> Vec<(String, Vec<u64>)> {
    let mut vals: Vec<Option<u64>> = vec![None; m.insts.len()];
    let mut memory = [0u64; 16];
    let mut trace = Vec::new();
    let eval = |vals: &[Option<u64>], v: V| -> u64 {
        match v {
            V::Inst(i) => vals[i as usize].expect("a value defined before its use"),
            V::Const(c) => {
                match (&m.consts[c as usize].1, m.int_bits(m.consts[c as usize].0)) {
                    (Const::Int(x), Some(bits)) => *x as u64 & mask(bits),
                    (Const::Float(bits), _) => u64::from(*bits),
                    _ => 0,
                }
            }
            _ => panic!("a global or function as a value"),
        }
    };
    let (mut b, mut from) = (0usize, usize::MAX);
    for _ in 0..4000 {
        let blk = &m.blocks[b];
        // Joins take their values together, before any is assigned.
        let joined: Vec<(u32, u64)> = blk
            .phis
            .iter()
            .map(|&p| {
                let Op::Phi(incoming) = &m.insts[p as usize].op else {
                    panic!("a join")
                };
                let v = incoming
                    .iter()
                    .find(|x| x.1 == from)
                    .expect("a value for the edge taken");
                (p, eval(&vals, v.0))
            })
            .collect();
        for (p, x) in joined {
            vals[p as usize] = Some(x);
        }
        for &i in &blk.body {
            let inst = &m.insts[i as usize];
            let bits = m.int_bits(inst.ty);
            let float = |x: u64| f32::from_bits(x as u32);
            let r = match &inst.op {
                Op::Bin(op, x, y) => {
                    let (x, y) = (eval(&vals, *x), eval(&vals, *y));
                    match bits {
                        Some(bits) => {
                            let r = match op {
                                Bin::Add => x.wrapping_add(y),
                                Bin::Sub => x.wrapping_sub(y),
                                Bin::Mul => x.wrapping_mul(y),
                                Bin::And => x & y,
                                Bin::Or => x | y,
                                Bin::Xor => x ^ y,
                                Bin::Shl => x << y,
                                Bin::LShr => x >> y,
                                Bin::AShr => (signed(bits, x) >> y) as u64,
                                _ => panic!("no divisions here"),
                            };
                            r & mask(bits)
                        }
                        None => {
                            let (x, y) = (float(x), float(y));
                            let r = match op {
                                Bin::Add => x + y,
                                Bin::Sub => x - y,
                                _ => x * y,
                            };
                            u64::from(r.to_bits())
                        }
                    }
                }
                Op::Cmp(p, x, y) => {
                    let wide = m.int_bits(m.type_of(*x));
                    let (x, y) = (eval(&vals, *x), eval(&vals, *y));
                    let holds = match wide {
                        Some(bits) => {
                            let (sx, sy) = (signed(bits, x), signed(bits, y));
                            match *p {
                                pred::EQ => x == y,
                                pred::NE => x != y,
                                pred::IUGT => x > y,
                                pred::IUGE => x >= y,
                                pred::IULT => x < y,
                                pred::IULE => x <= y,
                                pred::SGT => sx > sy,
                                pred::SGE => sx >= sy,
                                pred::SLT => sx < sy,
                                _ => sx <= sy,
                            }
                        }
                        None => {
                            let (x, y) = (float(x), float(y));
                            if x.is_nan() || y.is_nan() {
                                p & 8 != 0
                            } else {
                                (p & 1 != 0 && x == y)
                                    || (p & 2 != 0 && x > y)
                                    || (p & 4 != 0 && x < y)
                            }
                        }
                    };
                    u64::from(holds)
                }
                Op::Select(c, t, f) => {
                    if eval(&vals, *c) != 0 {
                        eval(&vals, *t)
                    } else {
                        eval(&vals, *f)
                    }
                }
                Op::Cast(op, x) => {
                    let from_bits = m.int_bits(m.type_of(*x));
                    let x = eval(&vals, *x);
                    match (op, from_bits, bits) {
                        (Cast::Bitcast, _, _) => x,
                        (Cast::ZExt, _, _) => x,
                        (Cast::SExt, Some(f), Some(t)) => signed(f, x) as u64 & mask(t),
                        (Cast::Trunc, _, Some(t)) => x & mask(t),
                        _ => panic!("a cast the generator does not make"),
                    }
                }
                Op::Call(f, args) => {
                    let a: Vec<u64> = args.iter().map(|&v| eval(&vals, v)).collect();
                    let name = m.funcs[*f as usize].name.as_str();
                    match name {
                        "dx.op.loadInput.i32" => {
                            u64::from(inputs[(a[0] ^ a[1]) as usize % inputs.len()])
                        }
                        "dx.op.binary.i32" => a[0].min(a[1]),
                        "dx.op.cbufferLoadLegacy.i32" => u64::from(
                            inputs[a[0] as usize % inputs.len()].rotate_left(a[1] as u32 & 31),
                        ),
                        "dx.op.bufferLoad.i32" => memory[(a[0] ^ a[1]) as usize % 16],
                        "dx.op.waveReadLaneFirst.i32" => a[0] ^ a[1],
                        _ => {
                            memory[a[0] as usize % 16] = a[1];
                            trace.push((name.to_owned(), a));
                            0
                        }
                    }
                }
                _ => panic!("an instruction the generator does not make"),
            };
            vals[i as usize] = Some(r);
        }
        from = b;
        b = match blk.term.expect("a terminator") {
            Term::Ret => {
                trace.push(("ret".into(), Vec::new()));
                return trace;
            }
            Term::Br(t) => t,
            Term::CondBr(c, t, f) => {
                if eval(&vals, c) != 0 {
                    t
                } else {
                    f
                }
            }
        };
    }
    trace.push(("still running".into(), Vec::new()));
    trace
}
