//! Whole-wave sections (decision 0003): code between `s_or_saveexec -1`
//! (or `s_nand_saveexec 0`) and the restore of the saved mask runs in every lane of the console's
//! wave, the inactive ones included, which is how the compiler expands
//! HLSL wave reductions and prefix sums (an identity put into the inactive
//! lanes with `v_cndmask`, then DPP, `v_permlanex16` and `v_readlane`
//! steps). A host lane that is elsewhere cannot run that code, so each
//! section is simulated on the console's lanes with symbolic values, and a
//! value it leaves is lowered by what it turned out to be: the reduction or
//! prefix of one operation over the lanes' (masked) inputs, which DXIL's
//! wave operations compute over the host's active lanes. No instruction
//! sequence is matched; any step the simulation does not model leaves the
//! values it writes unknown.

use std::collections::HashMap;

use crate::gfx10::{Dpp, Fields, NULL, Operand};
use crate::ir::{EXEC, Inst, Program, Reg, Value};

/// An associative operation and its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    AddU,
    AddF,
    /// A sum of 16-bit floats in the low halves (`v_add_f16`), each addition rounded to 16
    /// bits: not associative, so only the balanced tree over the lanes in their order is
    /// followed (see [`buddies`]), which is how the compiler sums a wave.
    AddH,
    MinU,
    MaxU,
    MinI,
    MaxI,
    MinF,
    MaxF,
    Or,
    And,
    Xor,
}

impl Op {
    pub(super) fn of(name: &str) -> Option<Op> {
        Some(match name {
            // the low dword of a 64-bit sum (`v_add_co_ci_u32` carries into
            // the high one) is the 32-bit sum of the low dwords
            "v_add_nc_u32" | "v_add_co_u32" => Op::AddU,
            "v_add_f32" => Op::AddF,
            "v_add_f16" => Op::AddH,
            "v_min_u32" => Op::MinU,
            "v_max_u32" => Op::MaxU,
            "v_min_i32" => Op::MinI,
            "v_max_i32" => Op::MaxI,
            "v_min_f32" => Op::MinF,
            "v_max_f32" => Op::MaxF,
            "v_or_b32" => Op::Or,
            "v_and_b32" => Op::And,
            "v_xor_b32" => Op::Xor,
            _ => return None,
        })
    }

    /// Whether `k` leaves the other operand unchanged.
    pub(super) fn identity(self, k: u32) -> bool {
        match self {
            Op::AddU | Op::AddH | Op::Or | Op::Xor | Op::MaxU => k == 0,
            Op::AddF => k == 0 || k == 0x8000_0000,
            Op::MinU | Op::And => k == u32::MAX,
            Op::MinI => k == 0x7FFF_FFFF,
            Op::MaxI => k == 0x8000_0000,
            Op::MinF => k == 0x7F80_0000,
            Op::MaxF => k == 0xFF80_0000,
        }
    }

    /// Counting a lane twice changes the result.
    pub(super) fn counts(self) -> bool {
        matches!(self, Op::AddU | Op::AddF | Op::AddH | Op::Xor)
    }
}

/// Whether the lane sets `a` and `b` are the two halves of one aligned run of lanes: lanes
/// 2k and 2k+1, then two such pairs side by side, and so on up to the two halves of the
/// wave. Sums joined only that way form the balanced tree over the lanes in their order,
/// whichever of the two a lane adds to the other (the addition is commutative).
pub(super) fn buddies(a: u64, b: u64) -> bool {
    let n = a.count_ones();
    if n != b.count_ones() || !n.is_power_of_two() || n == 64 {
        return false;
    }
    let run = |s: u64| {
        let first = s.trailing_zeros();
        (first.is_multiple_of(n) && s >> first == (1u64 << n) - 1).then_some(first)
    };
    match (run(a), run(b)) {
        (Some(x), Some(y)) => x.min(y) % (2 * n) == 0 && x.abs_diff(y) == n,
        _ => false,
    }
}

/// A lane's value inside a section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sym {
    Unknown,
    K(u32),
    /// The lane's own value of a VGPR value from before the section.
    Own(Value),
    /// A uniform value from before the section.
    Uni(Value),
    /// `op` over `lanes` of `mask ? src : ident` (`op` none: one lane).
    Red {
        op: Option<Op>,
        src: Value,
        mask: Value,
        ident: u32,
        lanes: u64,
    },
    /// Low (`!high`) or high (`high`) 32 bits of the 64-bit float sum over
    /// `lanes` of `mask ? f64(lo, hi) : 0.0`.
    Red64F {
        lo: Value,
        hi: Value,
        mask: Value,
        lanes: u64,
        high: bool,
    },
}

/// What a value a section leaves holds in every lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    K(u32),
    Value(Value),
    /// The lane's own `mask ? src : ident`.
    Masked(Value, Value, u32),
    /// `op` over every lane.
    Active(Op, Value, Value, u32),
    /// Low (`!high`) or high (`high`) 32 bits of the 64-bit float sum over every lane.
    ActiveF64(Value, Value, Value, bool),
    /// `op` over the lanes below (`inclusive`: and including) each lane.
    Prefix(Op, Value, Value, u32, bool),
}

/// The section starting at instruction `start` of a block: its restore
/// index and the shapes of the values it writes.
pub(crate) struct Section {
    pub end: usize,
    pub shapes: HashMap<Value, Shape>,
    /// What a `v_readlane_b32` of one lane takes, later in the block, from a
    /// VGPR the section leaves (by the scalar it writes): the last lanes of
    /// a reduction hold it before the first do.
    pub reads: HashMap<Value, Shape>,
}

/// Every whole-wave section of `block`, by start index.
pub(crate) fn sections(p: &Program, block: usize) -> HashMap<usize, Section> {
    let insts = &p.blocks[block].insts;
    let wave: u32 = if p.wave64 { 64 } else { 32 };
    let mut out = HashMap::new();
    let mut i = 0;
    while i < insts.len() {
        let inst = &insts[i];
        // EXEC becomes every lane: `s_or_saveexec -1` or `s_nand_saveexec 0`.
        let all = match inst.name() {
            "s_or_saveexec_b64" | "s_or_saveexec_b32" => Some(Operand::Const(193)),
            "s_nand_saveexec_b64" | "s_nand_saveexec_b32" => Some(Operand::Const(128)),
            _ => None,
        };
        let start = all.is_some_and(
            |k| matches!(&inst.fields, Fields::Sop { src, .. } if src.first() == Some(&k)),
        );
        let saved = match &inst.fields {
            Fields::Sop {
                dst: Some(Operand::Scalar { code, .. }),
                ..
            } if start => inst.write(Reg::S(*code)),
            _ => None,
        };
        let Some(saved) = saved else {
            i += 1;
            continue;
        };
        let end = (i + 1..insts.len()).find(|&j| {
            insts[j].write(Reg::S(EXEC)).is_some()
                && insts[j].read(Reg::S(saved_code(inst))) == Some(saved)
        });
        let Some(end) = end else {
            break;
        };
        let (vec, uni) = simulate(&insts[i + 1..end], wave, saved);
        let mut reads = HashMap::new();
        for later in insts[end + 1..].iter().filter(|i| i.name() == "v_readlane_b32") {
            if let Fields::Valu(v) = &later.fields
                && let (Some(Operand::Scalar { code, .. }), [Operand::Vector { reg, .. }, lane]) =
                    (v.dst, &v.src[..])
                && let Some(left) = later.read(Reg::V(*reg)).and_then(|x| vec.get(&x))
                && let Some(w) = later.write(Reg::S(code))
                && let Some(l) = lane_number(lane)
                && let Some(&sym) = left.get(l as usize)
                // (a lane's own value stays a lane read)
                && !matches!(sym, Sym::Own(_))
                && let Some(shape) = single(sym, wave)
            {
                reads.insert(w, shape);
            }
        }
        let shapes = shapes(vec, uni, wave);
        out.insert(i, Section { end, shapes, reads });
        i = end + 1;
    }
    out
}

pub(super) fn saved_code(inst: &Inst) -> u8 {
    match &inst.fields {
        Fields::Sop {
            dst: Some(Operand::Scalar { code, .. }),
            ..
        } => *code,
        _ => NULL,
    }
}

/// The lane `l` reads with DPP, whether it reads 0 instead, and whether it
/// writes; `None` for patterns not modelled.
pub(super) fn dpp_lane(d: Dpp, l: u32) -> Option<(u32, bool, bool)> {
    match d {
        Dpp::Lanes { sel, .. } => Some(((l & !7) | (sel >> (3 * (l & 7)) & 7), false, true)),
        Dpp::Row {
            ctrl,
            row_mask,
            bank_mask,
            bound_ctrl,
            ..
        } => {
            let (row, pos, n) = (l & !15, l & 15, u32::from(ctrl & 15));
            let enabled = row_mask >> (l >> 4 & 3) & 1 != 0 && bank_mask >> (l >> 2 & 3) & 1 != 0;
            let (src, inside) = match ctrl {
                0..=0xFF => ((l & !3) | (u32::from(ctrl) >> (2 * (l & 3)) & 3), true),
                0x101..=0x10F => (l + n, pos + n < 16),
                0x111..=0x11F => (l.wrapping_sub(n), pos >= n),
                0x121..=0x12F => (row | (pos.wrapping_sub(n) & 15), true),
                0x140 => (l ^ 15, true),
                0x141 => (l ^ 7, true),
                0x150..=0x15F => (row | n, true),
                0x160..=0x16F => (l ^ n, true),
                _ => return None,
            };
            match (inside, bound_ctrl) {
                (true, _) => Some((src, false, enabled)),
                (false, true) => Some((l, true, enabled)),
                (false, false) => Some((l, false, false)),
            }
        }
    }
}

pub(super) fn combine(op: Op, a: Sym, b: Sym) -> Sym {
    match (a, b) {
        (Sym::K(k), x) | (x, Sym::K(k)) if op.identity(k) => x,
        (
            Sym::Red {
                op: oa,
                src,
                mask,
                ident,
                lanes: la,
            },
            Sym::Red {
                op: ob,
                src: sb,
                mask: mb,
                ident: ib,
                lanes: lb,
            },
        ) if (src, mask, ident) == (sb, mb, ib)
            && op.identity(ident)
            && oa.is_none_or(|o| o == op)
            && ob.is_none_or(|o| o == op)
            && (!op.counts() || la & lb == 0)
            && (op != Op::AddH || buddies(la, lb)) =>
        {
            Sym::Red {
                op: Some(op),
                src,
                mask,
                ident,
                lanes: la | lb,
            }
        }
        _ => Sym::Unknown,
    }
}

/// The lane a `v_readlane_b32` names.
pub(super) fn lane_number(op: &Operand) -> Option<u32> {
    match *op {
        Operand::Const(c) => crate::resource::inline(c),
        Operand::Literal(x) => Some(x),
        _ => None,
    }
}

/// The lanes' values of the VGPRs the section writes, and its scalars.
pub(super) type Left = (HashMap<Value, Vec<Sym>>, HashMap<Value, Sym>);

mod simulate;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::WAVE_OR_V7_TO_S7;
use simulate::simulate;

pub(super) fn constant(inst: &Inst) -> Option<u64> {
    let Fields::Sop { src, .. } = &inst.fields else {
        return None;
    };
    let wide = inst.name() == "s_mov_b64";
    match *src.first()? {
        Operand::Scalar { code: NULL, .. } => Some(0),
        Operand::Const(c @ 128..=208) => {
            let x = crate::resource::inline(c)?;
            Some(if wide {
                i64::from(x as i32) as u64
            } else {
                u64::from(x)
            })
        }
        Operand::Const(c) if !wide => crate::resource::inline(c).map(u64::from),
        Operand::Literal(x) => Some(u64::from(x)),
        _ => None,
    }
}

/// The shape of a scalar.
fn single(s: Sym, wave: u32) -> Option<Shape> {
    let all = u64::MAX >> (64 - wave);
    match s {
        Sym::K(k) => Some(Shape::K(k)),
        Sym::Own(x) | Sym::Uni(x) => Some(Shape::Value(x)),
        Sym::Red {
            op: Some(op),
            src,
            mask,
            ident,
            lanes,
        } if lanes == all => Some(Shape::Active(op, src, mask, ident)),
        Sym::Red64F {
            lo,
            hi,
            mask,
            lanes,
            high,
        } if lanes == all => Some(Shape::ActiveF64(lo, hi, mask, high)),
        _ => None,
    }
}

/// The shape of each value, where every lane agrees on one.
fn shapes(
    vec: HashMap<Value, Vec<Sym>>,
    uni: HashMap<Value, Sym>,
    wave: u32,
) -> HashMap<Value, Shape> {
    let all = u64::MAX >> (64 - wave);
    let mut out = HashMap::new();
    for (v, s) in uni {
        if let Some(sh) = single(s, wave) {
            out.insert(v, sh);
        }
    }
    for (v, lanes) in vec {
        if let Some(sh) = per_lane(&lanes, all) {
            out.insert(v, sh);
        }
    }
    out
}

fn per_lane(lanes: &[Sym], all: u64) -> Option<Shape> {
    let first = lanes[0];
    let own = |l: usize, s: Sym| match s {
        Sym::Red {
            op: None,
            src,
            mask,
            ident,
            lanes,
        } if lanes == 1 << l => Some((src, mask, ident)),
        _ => None,
    };
    if let Some(m) = own(0, first)
        && lanes.iter().enumerate().all(|(l, &s)| own(l, s) == Some(m))
    {
        return Some(Shape::Masked(m.0, m.1, m.2));
    }
    if lanes.iter().all(|&s| s == first) {
        return match first {
            Sym::K(k) => Some(Shape::K(k)),
            Sym::Uni(x) | Sym::Own(x) => Some(Shape::Value(x)),
            Sym::Red {
                op: Some(op),
                src,
                mask,
                ident,
                lanes,
            } if lanes == all => Some(Shape::Active(op, src, mask, ident)),
            Sym::Red64F {
                lo,
                hi,
                mask,
                lanes,
                high,
            } if lanes == all => Some(Shape::ActiveF64(lo, hi, mask, high)),
            _ => None,
        };
    }
    // Prefixes: lane l holds lanes 0..l (exclusive) or 0..=l (inclusive).
    let mut meta: Option<(Value, Value, u32, bool)> = None;
    let mut op_found: Option<Op> = None;
    for (l, &s) in lanes.iter().enumerate() {
        let below = (1u64 << l) - 1;
        let (op, src, mask, ident, set) = match s {
            Sym::Red {
                op,
                src,
                mask,
                ident,
                lanes,
            } => (op, src, mask, ident, lanes),
            // Lane 0 of an exclusive prefix holds the identity.
            Sym::K(_) if l == 0 => continue,
            _ => return None,
        };
        let inclusive = set == below | (1u64 << l);
        if !inclusive && set != below {
            return None;
        }
        if l == 0 && !inclusive {
            return None;
        }
        if op.is_none() && set.count_ones() != 1 {
            return None;
        }
        let lane_meta = (src, mask, ident, inclusive);
        match meta {
            None => meta = Some(lane_meta),
            Some(prev) if prev == lane_meta => {}
            _ => return None,
        }
        if let Some(o) = op {
            match op_found {
                None => op_found = Some(o),
                Some(prev) if prev == o => {}
                _ => return None,
            }
        }
    }
    let (src, mask, ident, inclusive) = meta?;
    let op = op_found?;
    if !op.identity(ident) {
        return None;
    }
    match lanes[0] {
        Sym::K(k) if !inclusive && k == ident => {}
        Sym::Red { .. } if inclusive => {}
        _ => return None,
    }
    Some(Shape::Prefix(op, src, mask, ident, inclusive))
}

