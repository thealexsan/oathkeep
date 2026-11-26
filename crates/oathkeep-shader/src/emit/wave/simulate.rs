use std::collections::HashMap;

use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Inst, Reg, Value};

use super::{Left, Op, Sym, constant, combine, dpp_lane, lane_number};


pub(super) fn simulate(insts: &[Inst], wave: u32, saved: Value) -> Left {
    let all = if wave == 64 {
        u64::MAX
    } else {
        u64::from(u32::MAX)
    };
    let mut exec = all;
    let mut vec: HashMap<Value, Vec<Sym>> = HashMap::new();
    let mut uni: HashMap<Value, Sym> = HashMap::new();
    // Masks the section derives from the saved EXEC (`s_and` with it).
    let mut masked = vec![saved];
    for inst in insts {
        let name = inst.name();
        if inst.write(Reg::S(EXEC)).is_some() {
            // Constant lane sets (prefix sums); anything else ends what
            // the simulation can follow.
            let lo = name
                .starts_with("s_mov_b")
                .then(|| constant(inst))
                .flatten();
            let Some(k) = lo else {
                return (vec, uni);
            };
            exec = (if name == "s_mov_b64" {
                k
            } else {
                (exec & !0xFFFF_FFFF) | (k & 0xFFFF_FFFF)
            }) & all;
            continue;
        }
        if inst.write(Reg::S(EXEC + 1)).is_some() {
            let Some(k) = name
                .starts_with("s_mov_b")
                .then(|| constant(inst))
                .flatten()
            else {
                return (vec, uni);
            };
            exec = ((exec & 0xFFFF_FFFF) | ((k & 0xFFFF_FFFF) << 32)) & all;
            continue;
        }
        if let Fields::Sop { dst, src, .. } = &inst.fields {
            if name.starts_with("s_and_b")
                && src.iter().any(|o| matches!(*o, Operand::Scalar { code, .. } if inst.read(Reg::S(code)).is_some_and(|v| masked.contains(&v))))
                && let Some(&(_, w)) = inst.writes.iter().find(|w| matches!(w.0, Reg::S(_)))
            {
                masked.push(w);
            }
            if name == "s_mov_b32"
                && let (Some(Operand::Scalar { code: dc, .. }), Some(s0)) = (dst, src.first())
                && *dc != NULL
                && let Some(w) = inst.write(Reg::S(*dc))
            {
                let sym = match *s0 {
                    Operand::Scalar { code, .. } if code != NULL => inst
                        .read(Reg::S(code))
                        .map_or(Sym::Unknown, |x| uni.get(&x).copied().unwrap_or(Sym::Uni(x))),
                    Operand::Const(c) => crate::resource::inline(c).map_or(Sym::Unknown, Sym::K),
                    Operand::Literal(x) => Sym::K(x),
                    _ => Sym::Unknown,
                };
                uni.insert(w, sym);
            }
            for &(_, w) in &inst.writes {
                uni.entry(w).or_insert(Sym::Unknown);
            }
            continue;
        }
        let Fields::Valu(v) = &inst.fields else {
            for &(_, w) in &inst.writes {
                vec.insert(w, vec![Sym::Unknown; wave as usize]);
            }
            continue;
        };
        let lanes = |op: &Operand,
                     vec: &HashMap<Value, Vec<Sym>>,
                     uni: &HashMap<Value, Sym>|
         -> Vec<Sym> {
            let one = |s: Sym| vec![s; wave as usize];
            match *op {
                Operand::Vector { reg, .. } => match inst.read(Reg::V(reg)) {
                    Some(x) => vec
                        .get(&x)
                        .cloned()
                        .unwrap_or_else(|| (0..wave).map(|_| Sym::Own(x)).collect()),
                    None => one(Sym::Unknown),
                },
                Operand::Scalar { code, .. } if code != NULL => match inst.read(Reg::S(code)) {
                    Some(x) => one(uni.get(&x).copied().unwrap_or(Sym::Uni(x))),
                    None => one(Sym::Unknown),
                },
                Operand::Const(c) => one(crate::resource::inline(c).map_or(Sym::Unknown, Sym::K)),
                Operand::Literal(x) => one(Sym::K(x)),
                _ => one(Sym::Unknown),
            }
        };
        let modifiers = v.abs != 0 || v.neg != 0 || v.clamp || v.omod != 0 || v.sdwa.is_some();
        if name == "v_readlane_b32" {
            let (Some(Operand::Scalar { code, .. }), Some(src), Some(lane)) =
                (v.dst, v.src.first(), v.src.get(1))
            else {
                continue;
            };
            let l = lane_number(lane);
            let x = lanes(src, &vec, &uni);
            if let Some(w) = inst.write(Reg::S(code)) {
                let s = l
                    .and_then(|l| x.get(l as usize).copied())
                    .unwrap_or(Sym::Unknown);
                uni.insert(w, s);
            }
            continue;
        }
        if name == "v_add_f64"
            && !modifiers
            && v.dpp.is_none()
            && let Some(Operand::Vector { reg: dreg, .. }) = v.dst
            && let (Some(dst_lo), Some(dst_hi)) =
                (inst.write(Reg::V(dreg)), inst.write(Reg::V(dreg + 1)))
        {
            let pair_lanes = |op: Option<&Operand>| -> Option<(Vec<Sym>, Vec<Sym>)> {
                match *op? {
                    Operand::Vector { reg, .. } => Some((
                        lanes(&Operand::Vector { reg, dwords: 1 }, &vec, &uni),
                        lanes(
                            &Operand::Vector {
                                reg: reg + 1,
                                dwords: 1,
                            },
                            &vec,
                            &uni,
                        ),
                    )),
                    Operand::Scalar { code, .. } if code != NULL => Some((
                        lanes(&Operand::Scalar { code, dwords: 1 }, &vec, &uni),
                        lanes(
                            &Operand::Scalar {
                                code: code + 1,
                                dwords: 1,
                            },
                            &vec,
                            &uni,
                        ),
                    )),
                    _ => None,
                }
            };
            let old_lo = lanes(
                &Operand::Vector {
                    reg: dreg,
                    dwords: 1,
                },
                &vec,
                &uni,
            );
            let old_hi = lanes(
                &Operand::Vector {
                    reg: dreg + 1,
                    dwords: 1,
                },
                &vec,
                &uni,
            );
            let f64_term = |lo_s: Sym, hi_s: Sym| -> Option<(Value, Value, Value, u64)> {
                match (lo_s, hi_s) {
                    (
                        Sym::Red {
                            op: None,
                            src: lo,
                            mask: ml,
                            ident: 0,
                            lanes: ll,
                        },
                        Sym::Red {
                            op: None,
                            src: hi,
                            mask: mh,
                            ident: 0,
                            lanes: lh,
                        },
                    ) if ml == mh && ll == lh => Some((lo, hi, ml, ll)),
                    (
                        Sym::Red64F {
                            lo: lo1,
                            hi: hi1,
                            mask: m1,
                            lanes: l1,
                            high: false,
                        },
                        Sym::Red64F {
                            lo: lo2,
                            hi: hi2,
                            mask: m2,
                            lanes: l2,
                            high: true,
                        },
                    ) if (lo1, hi1, m1, l1) == (lo2, hi2, m2, l2) => Some((lo1, hi1, m1, l1)),
                    _ => None,
                }
            };
            let mut out_lo = vec![Sym::Unknown; wave as usize];
            let mut out_hi = vec![Sym::Unknown; wave as usize];
            if let (Some((alo, ahi)), Some((blo, bhi))) =
                (pair_lanes(v.src.first()), pair_lanes(v.src.get(1)))
            {
                for l in 0..wave as usize {
                    let (rl, rh) = match (f64_term(alo[l], ahi[l]), f64_term(blo[l], bhi[l])) {
                        (Some((lo1, hi1, m1, la)), Some((lo2, hi2, m2, lb)))
                            if (lo1, hi1, m1) == (lo2, hi2, m2) && la & lb == 0 =>
                        {
                            (
                                Sym::Red64F {
                                    lo: lo1,
                                    hi: hi1,
                                    mask: m1,
                                    lanes: la | lb,
                                    high: false,
                                },
                                Sym::Red64F {
                                    lo: lo1,
                                    hi: hi1,
                                    mask: m1,
                                    lanes: la | lb,
                                    high: true,
                                },
                            )
                        }
                        _ => (Sym::Unknown, Sym::Unknown),
                    };
                    out_lo[l] = if exec >> l & 1 != 0 { rl } else { old_lo[l] };
                    out_hi[l] = if exec >> l & 1 != 0 { rh } else { old_hi[l] };
                }
            }
            vec.insert(dst_lo, out_lo);
            vec.insert(dst_hi, out_hi);
            continue;
        }
        let Some(Operand::Vector { reg: dreg, .. }) = v.dst else {
            for &(_, w) in &inst.writes {
                uni.insert(w, Sym::Unknown);
            }
            continue;
        };
        let Some(dst) = inst.write(Reg::V(dreg)) else {
            continue;
        };
        let old = lanes(
            &Operand::Vector {
                reg: dreg,
                dwords: 1,
            },
            &vec,
            &uni,
        );
        // src0 as each lane fetches it (DPP), with the lanes that write.
        let src0 = v.src.first().map(|o| lanes(o, &vec, &uni));
        let mut writes = exec;
        let fetched: Option<Vec<Sym>> = match (v.dpp, &src0) {
            (Some(d), Some(s)) => {
                if exec != all {
                    vec.insert(dst, vec![Sym::Unknown; wave as usize]);
                    continue;
                }
                let mut f = Vec::with_capacity(wave as usize);
                let mut ok = true;
                for l in 0..wave {
                    match dpp_lane(d, l) {
                        Some((src, zero, on)) if src < wave => {
                            f.push(if zero { Sym::K(0) } else { s[src as usize] });
                            if !on {
                                writes &= !(1u64 << l);
                            }
                        }
                        _ => ok = false,
                    }
                }
                ok.then_some(f)
            }
            (None, Some(s)) => Some(s.clone()),
            _ => None,
        };
        let src = |k: usize| v.src.get(k).map(|o| lanes(o, &vec, &uni));
        let result: Vec<Sym> = match (name, fetched) {
            _ if modifiers => vec![Sym::Unknown; wave as usize],
            ("v_mov_b32", Some(a)) => a,
            ("v_bfrev_b32", Some(a)) => a
                .into_iter()
                .map(|s| match s {
                    Sym::K(k) => Sym::K(k.reverse_bits()),
                    _ => Sym::Unknown,
                })
                .collect(),
            ("v_cndmask_b32", Some(a)) => {
                let mask = match v.src.get(2) {
                    Some(&Operand::Scalar { code, .. }) => inst.read(Reg::S(code)),
                    _ => None,
                };
                let b = src(1).unwrap_or_default();
                (0..wave as usize)
                    .map(|l| match (a[l], b.get(l).copied(), mask) {
                        (Sym::K(ident), Some(Sym::Own(x)), Some(m)) if masked.contains(&m) => {
                            Sym::Red {
                                op: None,
                                src: x,
                                mask: m,
                                ident,
                                lanes: 1 << l,
                            }
                        }
                        _ => Sym::Unknown,
                    })
                    .collect()
            }
            ("v_permlanex16_b32", Some(a)) => {
                // Lanes of each row read the other row of their half; lane
                // i's pick is nibble i of {src2, src1}.
                let sel = |k: usize| match v.src.get(k) {
                    Some(&Operand::Const(c)) => crate::resource::inline(c),
                    Some(&Operand::Literal(x)) => Some(x),
                    _ => None,
                };
                match (sel(1), sel(2)) {
                    (Some(s1), Some(s2)) => {
                        let s = u64::from(s1) | u64::from(s2) << 32;
                        (0..wave)
                            .map(|l| {
                                let other = (l & !31) | ((l & 16) ^ 16);
                                let pick = (s >> (4 * (l & 15)) & 15) as u32;
                                a[(other | pick) as usize]
                            })
                            .collect()
                    }
                    _ => vec![Sym::Unknown; wave as usize],
                }
            }
            ("v_sub_nc_u32", Some(a)) => {
                // A prefix sum less the lane's own term: the exclusive one.
                let b = src(1).unwrap_or_default();
                (0..wave as usize)
                    .map(|l| match (a[l], b.get(l).copied()) {
                        (
                            Sym::Red {
                                op: None | Some(Op::AddU),
                                src,
                                mask,
                                ident: 0,
                                lanes,
                            },
                            Some(Sym::Red {
                                op: None | Some(Op::AddU),
                                src: s2,
                                mask: m2,
                                ident: 0,
                                lanes: l2,
                            }),
                        ) if (src, mask) == (s2, m2) && lanes & l2 == l2 => {
                            let rest = lanes & !l2;
                            if rest == 0 {
                                Sym::K(0)
                            } else {
                                Sym::Red {
                                    op: Some(Op::AddU),
                                    src,
                                    mask,
                                    ident: 0,
                                    lanes: rest,
                                }
                            }
                        }
                        _ => Sym::Unknown,
                    })
                    .collect()
            }
            (n, Some(a)) => match (Op::of(n), src(1)) {
                (Some(op), Some(b)) => (0..wave as usize)
                    .map(|l| combine(op, a[l], b[l]))
                    .collect(),
                _ => vec![Sym::Unknown; wave as usize],
            },
            _ => vec![Sym::Unknown; wave as usize],
        };
        let merged = (0..wave as usize)
            .map(|l| {
                if writes >> l & 1 != 0 {
                    result[l]
                } else {
                    old[l]
                }
            })
            .collect();
        vec.insert(dst, merged);
    }
    (vec, uni)
}

