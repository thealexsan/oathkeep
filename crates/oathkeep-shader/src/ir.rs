use crate::ShaderError;
use crate::gfx10::{self, Fields, Flow, Instruction, Operand};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reg {
    S(u8),
    Scc,
    V(u8),
}

pub const VCC: u8 = 106;
pub const M0: u8 = 124;
pub const EXEC: u8 = 126;
pub const PARKED: u8 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Value(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub wave64: bool,
    pub vgprs: u16,
    pub blocks: Vec<Block>,
    pub inputs: Vec<(Reg, Value)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub offset: usize,
    pub preds: Vec<usize>,
    pub phis: Vec<Phi>,
    pub insts: Vec<Inst>,
    pub exit: Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Jump(usize),
    Branch { taken: usize, not_taken: usize },
    End,
    Indirect,
}

impl Exit {
    pub fn successors(&self) -> Vec<usize> {
        match *self {
            Exit::Jump(b) => vec![b],
            Exit::Branch { taken, not_taken } => vec![taken, not_taken],
            Exit::End | Exit::Indirect => vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phi {
    pub reg: Reg,
    pub value: Value,
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inst {
    pub machine: Instruction,
    pub fields: Fields,
    pub reads: Vec<(Reg, Value)>,
    pub writes: Vec<(Reg, Value)>,
}

impl Inst {
    pub fn name(&self) -> &'static str {
        self.machine.name().unwrap_or("")
    }

    pub fn read(&self, reg: Reg) -> Option<Value> {
        self.reads.iter().find(|r| r.0 == reg).map(|r| r.1)
    }

    pub fn write(&self, reg: Reg) -> Option<Value> {
        self.writes.iter().find(|w| w.0 == reg).map(|w| w.1)
    }
}

pub fn build(code: &[u8], wave64: bool, vgprs: u16) -> Result<Program, ShaderError> {
    let mut list = gfx10::decode(code)?;
    let lanes = 1 + u8::from(wave64);
    let isa = |offset, reason| ShaderError::Isa { offset, reason };

    let mut flows = Vec::with_capacity(list.len());
    let mut leaders = vec![0];
    for (i, inst) in list.iter().enumerate() {
        let flow = gfx10::control(inst, code.len(), |at| {
            list[..list.partition_point(|i| i.offset < at)].last()
        })?;
        if let Flow::Jump(t) | Flow::Branch(t) = flow {
            leaders.push(t);
        }
        if flow != Flow::Next
            && let Some(next) = list.get(i + 1)
        {
            leaders.push(next.offset);
        }
        flows.push(flow);
    }
    leaders.sort_unstable();
    leaders.dedup();
    let parked = unspill(&mut list, &leaders, wave64, lanes, vgprs);
    let block_at = |offset: usize| {
        leaders
            .binary_search(&offset)
            .map_err(|_| isa(offset, "jump into a block"))
    };

    let mut blocks: Vec<Block> = leaders
        .iter()
        .map(|&offset| Block {
            offset,
            preds: Vec::new(),
            phis: Vec::new(),
            insts: Vec::new(),
            exit: Exit::End,
        })
        .collect();
    let next_offsets: Vec<Option<usize>> = (0..list.len())
        .map(|i| list.get(i + 1).map(|n| n.offset))
        .collect();
    for (n, ((machine, flow), next)) in list.into_iter().zip(flows).zip(next_offsets).enumerate() {
        let b = leaders.partition_point(|&l| l <= machine.offset) - 1;
        let name = machine.name().ok_or(ShaderError::Unsupported {
            offset: machine.offset,
            what: "unknown opcode",
        })?;
        let mut fields = machine.fields(wave64);
        if let Ok(k) = parked.binary_search_by_key(&n, |m| m.0)
            && let Fields::Sop {
                dst: Some(Operand::Scalar { code: to, .. }),
                src,
                ..
            } = &mut fields
            && let Some(Operand::Scalar { code: from, .. }) = src.first_mut()
        {
            (*to, *from) = (parked[k].1, parked[k].2);
        }
        let fallthrough = || block_at(next.ok_or(isa(machine.offset, "falls off the code"))?);
        let ends = flow != Flow::Next || next.is_none_or(|n| leaders.binary_search(&n).is_ok());
        if ends {
            blocks[b].exit = match flow {
                Flow::Next => Exit::Jump(fallthrough()?),
                Flow::Jump(t) => Exit::Jump(block_at(t)?),
                Flow::Branch(t) => Exit::Branch {
                    taken: block_at(t)?,
                    not_taken: fallthrough()?,
                },
                Flow::Stop if name == "s_setpc_b64" => Exit::Indirect,
                Flow::Stop => Exit::End,
            };
        }
        if let Some((mut reads, writes)) = effects(&machine, &fields, name, lanes, vgprs)? {
            for w in writes.iter().filter(|w| matches!(w, Reg::V(_))) {
                if !reads.contains(w) {
                    reads.push(*w);
                }
            }
            let unset = |regs: Vec<Reg>| regs.into_iter().map(|r| (r, NONE)).collect();
            blocks[b].insts.push(Inst {
                machine,
                fields,
                reads: unset(reads),
                writes: unset(writes),
            });
        }
    }
    link(&mut blocks);
    if !blocks[0].preds.is_empty() {
        return Err(ShaderError::Unsupported {
            offset: 0,
            what: "a branch to the program entry",
        });
    }
    split_irreducible(&mut blocks)?;

    Ok(rename(blocks, wave64, vgprs))
}

fn unspill(
    list: &mut [gfx10::Instruction],
    leaders: &[usize],
    wave64: bool,
    lanes: u8,
    vgprs: u16,
) -> Vec<(usize, u8, u8)> {
    fn parked(inst: &gfx10::Instruction, fields: &Fields) -> Option<(bool, u8, u8, u32)> {
        let Fields::Valu(v) = fields else { return None };
        if v.abs != 0
            || v.neg != 0
            || v.opsel != 0
            || v.clamp
            || v.omod != 0
            || v.sdwa.is_some()
            || v.dpp.is_some()
        {
            return None;
        }
        let lane = |op: &Operand| match *op {
            Operand::Const(c @ 128..=191) => Some(u32::from(c) - 128),
            _ => None,
        };
        match (inst.name()?, v.dst?, v.src.as_slice()) {
            ("v_writelane_b32", Operand::Vector { reg, .. }, [Operand::Scalar { code, .. }, k])
                if *code < 105 =>
            {
                Some((true, reg, *code, lane(k)?))
            }
            ("v_readlane_b32", Operand::Scalar { code, .. }, [Operand::Vector { reg, .. }, k])
                if code < 105 =>
            {
                Some((false, *reg, code, lane(k)?))
            }
            _ => None,
        }
    }
    let mut stores = [false; 256];
    let mut other = [false; 256];
    let mut used = [false; 128];
    let mut parks: Vec<Option<(bool, u8, u8, u32)>> = Vec::with_capacity(list.len());
    let mut written: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    let mut read: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    for inst in list.iter() {
        let Some(name) = inst.name() else {
            return Vec::new();
        };
        let fields = inst.fields(wave64);
        let park = parked(inst, &fields);
        let Ok(effects) = effects(inst, &fields, name, lanes, vgprs) else {
            return Vec::new();
        };
        let mut codes = Vec::new();
        let mut from = Vec::new();
        if let Some((reads, writes)) = effects {
            for &reg in &writes {
                if let Reg::S(c) = reg {
                    codes.push(c);
                }
            }
            for &reg in &reads {
                if let Reg::S(c) = reg {
                    from.push(c);
                }
            }
            let relative = name == "v_movrels_b32";
            let n_reads = reads.len();
            for (k, reg) in reads.into_iter().chain(writes).enumerate() {
                if relative && k < n_reads && matches!(reg, Reg::V(_)) {
                    continue;
                }
                match reg {
                    Reg::S(c) => used[usize::from(c)] = true,
                    Reg::V(r) if park.is_some_and(|p| p.1 == r) => stores[usize::from(r)] = true,
                    Reg::V(r) => other[usize::from(r)] = true,
                    Reg::Scc => {}
                }
            }
        }
        parks.push(park);
        written.push(codes);
        read.push(from);
    }
    for (i, park) in parks.iter_mut().enumerate() {
        if park.is_some_and(|p| other[usize::from(p.1)]) {
            *park = None;
        }
        let _ = i;
    }
    if parks.iter().all(Option::is_none) {
        return Vec::new();
    }
    let leader = |i: usize| leaders.binary_search(&list[i].offset).is_ok();
    let mut slots: Vec<(u8, u32)> = parks.iter().flatten().map(|p| (p.1, p.3)).collect();
    slots.sort_unstable();
    slots.dedup();
    let partner = |i: usize| -> Option<usize> {
        let (save, reg, code, lane) = parks[i]?;
        if save {
            let mut j = i + 1;
            while j < list.len() && !leader(j) {
                if parks[j] == Some((true, reg, code + 1, lane + 1)) {
                    return Some(j);
                }
                if parks[j].is_some_and(|p| p.1 == reg && (p.3 == lane || p.3 == lane + 1))
                    || written[j].contains(&code)
                    || written[j].contains(&(code + 1))
                {
                    return None;
                }
                j += 1;
            }
            None
        } else {
            let mut j = i + 1;
            while j < list.len() && !leader(j) {
                if parks[j] == Some((false, reg, code + 1, lane + 1)) {
                    return Some(j);
                }
                if parks[j].is_some_and(|p| p.0 && p.1 == reg && (p.3 == lane || p.3 == lane + 1))
                    || written[j].contains(&code)
                    || written[j].contains(&(code + 1))
                    || read[j].contains(&(code + 1))
                {
                    return None;
                }
                j += 1;
            }
            None
        }
    };
    let mut pairs: Vec<(u8, u32)> = Vec::new();
    for &(reg, lane) in &slots {
        if !wave64
            || pairs.iter().any(|&(r, l)| r == reg && l + 1 == lane)
            || !slots.contains(&(reg, lane + 1))
        {
            continue;
        }
        let lower: Vec<usize> = (0..list.len())
            .filter(|&i| parks[i].is_some_and(|p| (p.1, p.3) == (reg, lane)))
            .collect();
        let upper = parks
            .iter()
            .flatten()
            .filter(|p| (p.1, p.3) == (reg, lane + 1))
            .count();
        let partners: Vec<Option<usize>> = lower.iter().map(|&i| partner(i)).collect();
        let mut distinct: Vec<usize> = partners.iter().flatten().copied().collect();
        distinct.sort_unstable();
        distinct.dedup();
        if partners.iter().all(Option::is_some)
            && distinct.len() == lower.len()
            && upper == lower.len()
        {
            pairs.push((reg, lane));
        }
    }
    let past = usize::from(PARKED);
    let mut free = [false; 256];
    for c in (0..105).chain(108..=123) {
        free[c] = !used[c];
    }
    free[past..255].fill(true);
    let mut codes: Vec<((u8, u32), u8)> = Vec::new();
    for &(reg, lane) in &pairs {
        let at = (108..123)
            .chain((0..104).rev())
            .chain(past..254)
            .find(|&c| free[c] && free[c + 1]);
        let Some(c) = at else { return Vec::new() };
        (free[c], free[c + 1]) = (false, false);
        codes.push(((reg, lane), c as u8));
        codes.push(((reg, lane + 1), c as u8 + 1));
    }
    for &slot in &slots {
        if codes.iter().any(|c| c.0 == slot) {
            continue;
        }
        let Some(c) = (108..=123)
            .chain((0..105).rev())
            .chain(past..255)
            .find(|&c| free[c])
        else {
            return Vec::new();
        };
        free[c] = false;
        codes.push((slot, c as u8));
    }
    let code_of = |reg: u8, lane: u32| codes.iter().find(|c| c.0 == (reg, lane)).map(|c| c.1);
    let put = |list: &mut [gfx10::Instruction], i: usize, encoding, opcode: u16, word: u32| {
        list[i] = gfx10::Instruction {
            offset: list[i].offset,
            encoding,
            opcode,
            words: vec![word],
        };
    };
    let field = |code: u8| if code < PARKED { u32::from(code) } else { 0 };
    let mov = |wide: bool, dst: u8, src: u8| {
        0xBE80_0000 | field(dst) << 16 | (3 + u32::from(wide)) << 8 | field(src)
    };
    const NOP: u32 = 0xBF80_0000;
    let mut edits: Vec<(usize, Option<(bool, u8, u8)>)> = Vec::new();
    for i in 0..list.len() {
        let Some((save, reg, code, lane)) = parks[i] else {
            continue;
        };
        let Some(slot) = code_of(reg, lane) else {
            return Vec::new();
        };
        if pairs.contains(&(reg, lane)) {
            let Some(j) = partner(i) else {
                return Vec::new();
            };
            let (at, gone) = if save { (j, i) } else { (i, j) };
            let (dst, src) = if save { (slot, code) } else { (code, slot) };
            edits.push((at, Some((true, dst, src))));
            edits.push((gone, None));
        } else if lane > 0 && pairs.contains(&(reg, lane - 1)) {
        } else {
            let (dst, src) = if save { (slot, code) } else { (code, slot) };
            edits.push((i, Some((false, dst, src))));
        }
    }
    let mut moves = Vec::new();
    for (i, edit) in edits {
        match edit {
            Some((wide, dst, src)) => {
                put(
                    list,
                    i,
                    gfx10::Encoding::Sop1,
                    3 + u16::from(wide),
                    mov(wide, dst, src),
                );
                moves.push((i, dst, src));
            }
            None => put(list, i, gfx10::Encoding::Sopp, 0, NOP),
        }
    }
    moves.sort_unstable();
    moves
}

fn rename(mut blocks: Vec<Block>, wave64: bool, vgprs: u16) -> Program {
    let order = reverse_postorder(&blocks);
    let (live_in, live_out) = liveness(&blocks, &order);
    let mut values = 0u32;
    let mut new_value = || {
        values += 1;
        Value(values - 1)
    };
    let mut inputs = Vec::new();
    let mut outs: Vec<Vec<(Reg, Value)>> = vec![Vec::new(); blocks.len()];
    let mut current = [NONE; REGS];
    for &b in &order {
        current.fill(NONE);
        let block = &mut blocks[b];
        for reg in members(&live_in[b]) {
            current[index(reg)] = match block.preds[..] {
                [] => {
                    let v = new_value();
                    inputs.push((reg, v));
                    v
                }
                [p] => lookup(&outs[p], reg),
                _ => {
                    let value = new_value();
                    block.phis.push(Phi {
                        reg,
                        value,
                        args: Vec::new(),
                    });
                    value
                }
            };
        }
        for inst in &mut block.insts {
            for (reg, v) in &mut inst.reads {
                *v = current[index(*reg)];
                debug_assert_ne!(*v, NONE, "{reg:?} read at {:#x}", inst.machine.offset);
            }
            for (reg, v) in &mut inst.writes {
                *v = new_value();
                current[index(*reg)] = *v;
            }
        }
        outs[b] = members(&live_out[b])
            .map(|reg| (reg, current[index(reg)]))
            .collect();
    }
    for block in &mut blocks {
        for phi in &mut block.phis {
            phi.args = block
                .preds
                .iter()
                .map(|&p| lookup(&outs[p], phi.reg))
                .collect();
            debug_assert!(!phi.args.contains(&NONE), "{phi:?}");
        }
    }
    remove_trivial_phis(&mut blocks, values);
    Program {
        wave64,
        vgprs,
        blocks,
        inputs,
    }
}

pub(crate) fn thread_edges(
    p: &Program,
    edges: &[(usize, usize, usize)],
) -> Result<Program, ShaderError> {
    let mut blocks = p.blocks.clone();
    for block in &mut blocks {
        block.phis.clear();
    }
    for &(pred, join, dest) in edges {
        let mut copy = blocks[join].clone();
        copy.exit = Exit::Jump(dest);
        let target = blocks.len();
        match &mut blocks[pred].exit {
            Exit::Jump(t) => *t = target,
            Exit::Branch { taken, not_taken } => {
                if *taken == join {
                    *taken = target;
                }
                if *not_taken == join {
                    *not_taken = target;
                }
            }
            _ => unreachable!("threading an incoming edge"),
        }
        blocks.push(copy);
    }
    link(&mut blocks);
    split_irreducible(&mut blocks)?;
    Ok(rename(blocks, p.wave64, p.vgprs))
}

pub(crate) fn without_early_outs(p: &Program) -> Option<Program> {
    let ends = |b: usize| {
        p.blocks[b].exit == Exit::End
            && p.blocks[b]
                .insts
                .iter()
                .all(|i| matches!(i.fields, crate::gfx10::Fields::Sopp { .. }))
    };
    let mut blocks = p.blocks.clone();
    let mut any = false;
    for block in &mut blocks {
        let Exit::Branch { taken, not_taken } = block.exit else {
            continue;
        };
        if ends(taken)
            && block
                .insts
                .last()
                .is_some_and(|i| i.name() == "s_cbranch_execz")
        {
            block.insts.pop();
            block.exit = Exit::Jump(not_taken);
            any = true;
        }
    }
    if !any {
        return None;
    }
    for block in &mut blocks {
        block.phis.clear();
    }
    link(&mut blocks);
    split_irreducible(&mut blocks).ok()?;
    Some(rename(blocks, p.wave64, p.vgprs))
}

pub(crate) fn without_skips(p: &Program, deep: bool) -> Option<Program> {
    let n = p.blocks.len();
    let ends = |b: usize| {
        p.blocks[b].exit == Exit::End
            && p.blocks[b]
                .insts
                .iter()
                .all(|i| matches!(i.fields, crate::gfx10::Fields::Sopp { .. }))
    };
    let mut copy: std::collections::HashMap<Value, Value> = Default::default();
    let mut live: std::collections::HashSet<Value> = Default::default();
    let idle = |inst: &Inst, live: &std::collections::HashSet<Value>| {
        let name = inst.name();
        matches!(inst.fields, crate::gfx10::Fields::Sop { .. })
            && !(name.starts_with("s_setpc")
                || name.starts_with("s_swappc")
                || name.starts_with("s_call")
                || name.starts_with("s_rfe")
                || name.starts_with("s_movrel")
                || name.starts_with("s_subvector"))
            && inst.writes.iter().all(|w| !live.contains(&w.1))
    };
    for block in &p.blocks {
        for inst in &block.insts {
            if matches!(inst.name(), "s_mov_b32" | "s_mov_b64")
                && inst.reads.len() == inst.writes.len()
                && inst
                    .reads
                    .iter()
                    .chain(&inst.writes)
                    .all(|r| matches!(r.0, Reg::S(_)))
            {
                copy.extend(inst.writes.iter().zip(&inst.reads).map(|(w, r)| (w.1, r.1)));
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for block in &p.blocks {
            for inst in &block.insts {
                if !idle(inst, &live) {
                    for r in &inst.reads {
                        changed |= live.insert(r.1);
                    }
                }
            }
            for phi in &block.phis {
                if live.contains(&phi.value) {
                    for &a in &phi.args {
                        changed |= live.insert(a);
                    }
                }
            }
        }
    }
    let root = |mut v: Value| {
        while deep && let Some(&c) = copy.get(&v) {
            v = c;
        }
        v
    };
    let skipped = |b: usize, target: usize, first: usize| -> Option<Vec<bool>> {
        let mut inside = vec![false; n];
        let mut stack = vec![first];
        while let Some(x) = stack.pop() {
            if x == target || inside[x] {
                continue;
            }
            if x == b {
                return None;
            }
            inside[x] = true;
            match p.blocks[x].exit {
                Exit::End | Exit::Indirect => return None,
                exit => stack.extend(exit.successors()),
            }
        }
        for x in (0..n).filter(|&x| inside[x]) {
            if p.blocks[x].preds.iter().any(|&q| q != b && !inside[q]) {
                return None;
            }
        }
        let join = &p.blocks[target];
        for phi in &join.phis {
            let scalar = match phi.reg {
                Reg::S(c) => c != 126 && c != 127,
                Reg::Scc => true,
                Reg::V(_) => false,
            };
            if !scalar || deep && !live.contains(&phi.value) {
                continue;
            }
            let before: Vec<Value> = join
                .preds
                .iter()
                .zip(&phi.args)
                .filter(|(q, _)| **q == b)
                .map(|(_, a)| root(*a))
                .collect();
            let within = join
                .preds
                .iter()
                .zip(&phi.args)
                .filter(|(q, _)| inside[**q])
                .map(|(_, a)| root(*a));
            if within.into_iter().any(|a| !before.contains(&a)) {
                return None;
            }
        }
        if !deep && join.preds.iter().any(|&q| q != b && !inside[q]) {
            return None;
        }
        Some(inside)
    };
    let mut blocks = p.blocks.clone();
    let mut any = false;
    for b in 0..n {
        let Exit::Branch { taken, not_taken } = p.blocks[b].exit else {
            continue;
        };
        if !p.blocks[b]
            .insts
            .last()
            .is_some_and(|i| i.name() == "s_cbranch_execz")
        {
            continue;
        }
        if ends(taken) || skipped(b, taken, not_taken).is_some() {
            blocks[b].insts.pop();
            blocks[b].exit = Exit::Jump(not_taken);
            any = true;
        }
    }
    if !any {
        return None;
    }
    for block in &mut blocks {
        block.phis.clear();
    }
    link(&mut blocks);
    split_irreducible(&mut blocks).ok()?;
    Some(rename(blocks, p.wave64, p.vgprs))
}

const NONE: Value = Value(u32::MAX);
const REGS: usize = 256 + 1 + 256;
type RegSet = [u64; REGS.div_ceil(64)];

fn index(reg: Reg) -> usize {
    match reg {
        Reg::S(c) => usize::from(c),
        Reg::Scc => 256,
        Reg::V(r) => 257 + usize::from(r),
    }
}

fn reg_at(i: usize) -> Reg {
    match i {
        0..256 => Reg::S(i as u8),
        256 => Reg::Scc,
        _ => Reg::V((i - 257) as u8),
    }
}

fn insert(set: &mut RegSet, reg: Reg) {
    let i = index(reg);
    set[i / 64] |= 1 << (i % 64);
}

fn contains(set: &RegSet, reg: Reg) -> bool {
    let i = index(reg);
    set[i / 64] >> (i % 64) & 1 != 0
}

fn members(set: &RegSet) -> impl Iterator<Item = Reg> + '_ {
    (0..REGS)
        .filter(|&i| set[i / 64] >> (i % 64) & 1 != 0)
        .map(reg_at)
}

fn lookup(outs: &[(Reg, Value)], reg: Reg) -> Value {
    outs.binary_search_by_key(&reg, |&(r, _)| r)
        .map_or(NONE, |i| outs[i].1)
}

pub(crate) fn reverse_postorder(blocks: &[Block]) -> Vec<usize> {
    let mut seen = vec![false; blocks.len()];
    let mut post = Vec::with_capacity(blocks.len());
    let mut stack = vec![(0, 0)];
    seen[0] = true;
    while let Some(top) = stack.last_mut() {
        let (b, i) = *top;
        top.1 += 1;
        match blocks[b].exit.successors().get(i) {
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
    post
}

fn link(blocks: &mut [Block]) {
    for block in blocks.iter_mut() {
        block.preds.clear();
    }
    for b in 0..blocks.len() {
        for s in blocks[b].exit.successors() {
            blocks[s].preds.push(b);
        }
    }
}

pub(crate) fn irreducible_edge(
    blocks: &[Block],
    order: &[usize],
    index: &[usize],
) -> Option<(usize, usize)> {
    let &entry = order.first()?;
    let idom = idoms(blocks, order, index);
    for &b in order {
        for s in blocks[b].exit.successors() {
            if index[s] > index[b] {
                continue;
            }
            let mut d = b;
            while d != s && d != entry {
                d = idom[d];
            }
            if d != s {
                return Some((b, s));
            }
        }
    }
    None
}

pub(crate) fn idoms(blocks: &[Block], order: &[usize], index: &[usize]) -> Vec<usize> {
    let mut idom = vec![usize::MAX; blocks.len()];
    let Some(&entry) = order.first() else {
        return idom;
    };
    idom[entry] = entry;
    let intersect = |idom: &[usize], mut a: usize, mut b: usize| {
        while a != b {
            while index[a] > index[b] {
                a = idom[a];
            }
            while index[b] > index[a] {
                b = idom[b];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order[1..] {
            let mut new = usize::MAX;
            for &q in blocks[b].preds.iter().filter(|&&q| idom[q] != usize::MAX) {
                new = if new == usize::MAX {
                    q
                } else {
                    intersect(&idom, q, new)
                };
            }
            if idom[b] != new {
                idom[b] = new;
                changed = true;
            }
        }
    }
    idom
}

fn split_irreducible(blocks: &mut Vec<Block>) -> Result<(), ShaderError> {
    let limit = 4 * blocks.len() + 64;
    loop {
        let order = reverse_postorder(blocks);
        let mut index = vec![usize::MAX; blocks.len()];
        for (i, &b) in order.iter().enumerate() {
            index[b] = i;
        }
        let Some((from, _)) = irreducible_edge(blocks, &order, &index) else {
            return Ok(());
        };
        let found = multi_entry_loop(blocks, &vec![true; blocks.len()]);
        let Some((region, mut entries)) = found.filter(|_| blocks.len() < limit) else {
            return Err(ShaderError::Unsupported {
                offset: blocks[from].offset,
                what: "irreducible control flow",
            });
        };
        entries.sort_by_key(|&e| index[e]);
        let (header, entry) = (entries[0], entries[1]);

        let mut copy = vec![usize::MAX; blocks.len()];
        let mut copied = Vec::new();
        let mut stack = vec![entry];
        while let Some(b) = stack.pop() {
            if copy[b] != usize::MAX {
                continue;
            }
            copy[b] = blocks.len() + copied.len();
            copied.push(b);
            for s in blocks[b].exit.successors() {
                if region[s] && s != header {
                    stack.push(s);
                }
            }
        }
        let retarget = |exit: &mut Exit, to: &dyn Fn(usize) -> usize| match exit {
            Exit::Jump(s) => *s = to(*s),
            Exit::Branch { taken, not_taken } => {
                *taken = to(*taken);
                *not_taken = to(*not_taken);
            }
            Exit::End | Exit::Indirect => {}
        };
        for &b in &copied {
            let mut block = blocks[b].clone();
            retarget(&mut block.exit, &|s| {
                if copy[s] == usize::MAX { s } else { copy[s] }
            });
            blocks.push(block);
        }
        let outside: Vec<usize> = blocks[entry]
            .preds
            .iter()
            .copied()
            .filter(|&p| !region[p])
            .collect();
        for p in outside {
            retarget(&mut blocks[p].exit, &|s| {
                if s == entry { copy[entry] } else { s }
            });
        }
        link(blocks);
    }
}

fn multi_entry_loop(blocks: &[Block], inside: &[bool]) -> Option<(Vec<bool>, Vec<usize>)> {
    let reach = |from: usize, forward: bool| {
        let mut seen = vec![false; blocks.len()];
        let mut stack = vec![from];
        while let Some(b) = stack.pop() {
            let next = if forward {
                blocks[b].exit.successors()
            } else {
                blocks[b].preds.clone()
            };
            for n in next {
                if inside[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        seen
    };
    let mut done = vec![false; blocks.len()];
    for v in (0..blocks.len()).filter(|&v| inside[v]) {
        if done[v] {
            continue;
        }
        let (ahead, behind) = (reach(v, true), reach(v, false));
        let region: Vec<bool> = ahead.iter().zip(&behind).map(|(a, b)| a & b).collect();
        if !region[v] {
            continue;
        }
        let members = (0..blocks.len()).filter(|&b| region[b]);
        for b in members.clone() {
            done[b] = true;
        }
        let entries: Vec<usize> = members
            .filter(|&b| blocks[b].preds.iter().any(|&p| !region[p]))
            .collect();
        match entries[..] {
            [] => {}
            [header] => {
                let mut nested = region;
                nested[header] = false;
                if let Some(found) = multi_entry_loop(blocks, &nested) {
                    return Some(found);
                }
            }
            _ => return Some((region, entries)),
        }
    }
    None
}

fn liveness(blocks: &[Block], order: &[usize]) -> (Vec<RegSet>, Vec<RegSet>) {
    let empty = [0; REGS.div_ceil(64)];
    let mut uses = vec![empty; blocks.len()];
    let mut defs = vec![empty; blocks.len()];
    for (b, block) in blocks.iter().enumerate() {
        for inst in &block.insts {
            for &(reg, _) in &inst.reads {
                if !contains(&defs[b], reg) {
                    insert(&mut uses[b], reg);
                }
            }
            for &(reg, _) in &inst.writes {
                insert(&mut defs[b], reg);
            }
        }
    }
    let mut live_in = vec![empty; blocks.len()];
    let mut live_out = vec![empty; blocks.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().rev() {
            let mut out = empty;
            for s in blocks[b].exit.successors() {
                for (o, i) in out.iter_mut().zip(&live_in[s]) {
                    *o |= i;
                }
            }
            let mut inn = empty;
            for w in 0..inn.len() {
                inn[w] = uses[b][w] | out[w] & !defs[b][w];
            }
            if inn != live_in[b] || out != live_out[b] {
                (live_in[b], live_out[b]) = (inn, out);
                changed = true;
            }
        }
    }
    (live_in, live_out)
}

fn remove_trivial_phis(blocks: &mut [Block], values: u32) {
    let mut replaced: Vec<Value> = (0..values).map(Value).collect();
    let find = |replaced: &[Value], mut v: Value| {
        while replaced[v.0 as usize] != v {
            v = replaced[v.0 as usize];
        }
        v
    };
    let mut changed = true;
    while changed {
        changed = false;
        for block in blocks.iter_mut() {
            block.phis.retain(|phi| {
                let mut only = None;
                for &a in &phi.args {
                    let a = find(&replaced, a);
                    if a == phi.value || Some(a) == only {
                        continue;
                    }
                    if only.is_some() {
                        return true;
                    }
                    only = Some(a);
                }
                let Some(v) = only else { return true };
                replaced[phi.value.0 as usize] = v;
                changed = true;
                false
            });
        }
    }
    for block in blocks {
        for phi in &mut block.phis {
            for a in &mut phi.args {
                *a = find(&replaced, *a);
            }
        }
        for inst in &mut block.insts {
            for (_, v) in &mut inst.reads {
                *v = find(&replaced, *v);
            }
        }
    }
}

const DROPPED: &[&str] = &[
    "s_nop",
    "s_waitcnt",
    "s_waitcnt_vscnt",
    "s_waitcnt_vmcnt",
    "s_waitcnt_expcnt",
    "s_waitcnt_lgkmcnt",
    "s_waitcnt_depctr",
    "s_wait_idle",
    "s_inst_prefetch",
    "s_clause",
    "s_setprio",
    "s_sleep",
    "s_ttracedata",
    "s_ttracedata_imm",
    "s_incperflevel",
    "s_decperflevel",
    "v_nop",
    "v_pipeflush",
    "v_clrexcp",
];

type Effects = (Vec<Reg>, Vec<Reg>);

pub(crate) fn operand_reads(p: &Program, inst: &Inst) -> Vec<Reg> {
    let lanes = 1 + u8::from(p.wave64);
    effects(&inst.machine, &inst.fields, inst.name(), lanes, p.vgprs)
        .ok()
        .flatten()
        .map_or_else(Vec::new, |e| e.0)
}

fn effects(
    inst: &Instruction,
    fields: &Fields,
    name: &'static str,
    lanes: u8,
    vgprs: u16,
) -> Result<Option<Effects>, ShaderError> {
    if DROPPED.contains(&name) {
        return Ok(None);
    }
    let unsupported = || ShaderError::Unsupported {
        offset: inst.offset,
        what: name,
    };
    let mask = |code: u8| (0..lanes).map(move |i| Reg::S(code + i));
    let (mut r, mut w) = (Vec::new(), Vec::new());
    let regs = |op: &Operand, out: &mut Vec<Reg>| {
        add_regs(op, lanes, out).map_err(|_| ShaderError::Isa {
            offset: inst.offset,
            reason: "register range out of bounds",
        })
    };
    let vector = !matches!(
        fields,
        Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. }
    );
    match fields {
        Fields::Sop { dst, src, .. } => {
            if name.starts_with("s_movrel")
                || name.starts_with("s_subvector_loop")
                || matches!(name, "s_call_b64" | "s_rfe_b64")
            {
                return Err(unsupported());
            }
            for s in src {
                regs(s, &mut r)?;
            }
            if let Some(d) = dst {
                regs(d, &mut w)?;
                if name.starts_with("s_bitset")
                    || name.starts_with("s_cmov")
                    || matches!(name, "s_addk_i32" | "s_mulk_i32")
                {
                    regs(d, &mut r)?;
                }
            }
            if matches!(name, "s_addc_u32" | "s_subb_u32" | "s_cmovk_i32")
                || name.starts_with("s_cselect_")
                || name.starts_with("s_cmov_")
            {
                r.push(Reg::Scc);
            }
            if name.contains("saveexec") || name.contains("wrexec") {
                r.extend(mask(EXEC));
                w.extend(mask(EXEC));
            }
            if writes_scc(inst.encoding, name) {
                w.push(Reg::Scc);
            }
        }
        Fields::Sopp { .. } => match name {
            "s_cbranch_scc0" | "s_cbranch_scc1" => r.push(Reg::Scc),
            "s_cbranch_vccz" | "s_cbranch_vccnz" => r.extend(mask(VCC)),
            "s_cbranch_execz" | "s_cbranch_execnz" => r.extend(mask(EXEC)),
            "s_sendmsg" | "s_sendmsghalt" => r.push(Reg::S(M0)),
            "s_endpgm"
            | "s_endpgm_saved"
            | "s_endpgm_ordered_ps_done"
            | "s_code_end"
            | "s_branch"
            | "s_barrier"
            | "s_icache_inv"
            | "s_wakeup"
            | "s_round_mode"
            | "s_denorm_mode" => {}
            _ => return Err(unsupported()),
        },
        Fields::Smem {
            data,
            base,
            soffset,
            ..
        } => match name {
            "s_memtime" | "s_memrealtime" | "s_get_waveid_in_workgroup" => {
                regs(data.as_ref().ok_or_else(unsupported)?, &mut w)?;
            }
            "s_gl1_inv" | "s_dcache_inv" | "s_dcache_wb" => {}
            _ if name.contains("_load_") || name.contains("_store_") => {
                regs(base, &mut r)?;
                regs(soffset, &mut r)?;
                let data = data.as_ref().ok_or_else(unsupported)?;
                regs(
                    data,
                    if name.contains("_load_") {
                        &mut w
                    } else {
                        &mut r
                    },
                )?;
            }
            _ => return Err(unsupported()),
        },
        Fields::Valu(v) => {
            let (src_indexed, dst_indexed) = match name {
                "v_movrels_b32" => (true, false),
                "v_movreld_b32" => (false, true),
                "v_movrelsd_b32" | "v_movrelsd_2_b32" | "v_swaprel_b32" => (true, true),
                _ => (false, false),
            };
            let indexed = |op: &Operand, out: &mut Vec<Reg>| match *op {
                Operand::Vector { reg, .. } if u16::from(reg) < vgprs => {
                    out.extend((u16::from(reg)..vgprs).map(|r| Reg::V(r as u8)));
                    Ok(())
                }
                _ => Err(ShaderError::Isa {
                    offset: inst.offset,
                    reason: "relative VGPR base outside the program's VGPRs",
                }),
            };
            for (i, s) in v.src.iter().enumerate() {
                if i == 0 && src_indexed {
                    indexed(s, &mut r)?;
                } else {
                    regs(s, &mut r)?;
                }
            }
            if name.starts_with("v_cmpx") {
                w.extend(mask(EXEC));
            } else if let Some(d) = &v.dst {
                if dst_indexed {
                    indexed(d, &mut w)?;
                } else {
                    regs(d, &mut w)?;
                }
            }
            if let Some(c) = &v.carry {
                regs(c, &mut w)?;
            }
            match name {
                "v_swap_b32" => {
                    regs(&v.src[0], &mut w)?;
                    if let Some(d) = &v.dst {
                        regs(d, &mut r)?;
                    }
                }
                "v_swaprel_b32" => {
                    indexed(&v.src[0], &mut w)?;
                    if let Some(d) = &v.dst {
                        indexed(d, &mut r)?;
                    }
                }
                "v_div_fmas_f32" | "v_div_fmas_f64" => r.extend(mask(VCC)),
                _ => {}
            }
            if name.starts_with("v_interp_") || src_indexed || dst_indexed {
                r.push(Reg::S(M0));
            }
        }
        Fields::Vintrp { dst, src, .. } => {
            regs(src, &mut r)?;
            r.push(Reg::S(M0));
            regs(dst, &mut w)?;
        }
        Fields::Ds {
            dst,
            addr,
            data,
            gds,
            ..
        } => {
            for o in addr.iter().chain(data) {
                regs(o, &mut r)?;
            }
            if let Some(d) = dst {
                regs(d, &mut w)?;
            }
            if *gds
                || name.starts_with("ds_gws")
                || name.contains("addtid")
                || matches!(name, "ds_ordered_count" | "ds_append" | "ds_consume")
            {
                r.push(Reg::S(M0));
            }
        }
        Fields::Buffer {
            data,
            addr,
            rsrc,
            soffset,
            offen,
            idxen,
            lds,
            cache,
            ..
        } => {
            if *offen || *idxen {
                regs(addr, &mut r)?;
            }
            regs(rsrc, &mut r)?;
            regs(soffset, &mut r)?;
            if *lds {
                r.push(Reg::S(M0));
            }
            if let Some(d) = data {
                if name.contains("_load_") {
                    if !lds {
                        regs(d, &mut w)?;
                    }
                } else if name.contains("_store_") {
                    regs(d, &mut r)?;
                } else if name.contains("_atomic_") {
                    regs(d, &mut r)?;
                    if cache.glc {
                        regs(&atomic_result(d, name), &mut w)?;
                    }
                } else {
                    return Err(unsupported());
                }
            }
        }
        Fields::Image {
            data,
            addr,
            rsrc,
            sampler,
            cache,
            ..
        } => {
            for a in addr.iter().chain([rsrc]).chain(sampler) {
                regs(a, &mut r)?;
            }
            if name.contains("_store") {
                regs(data, &mut r)?;
            } else if name.contains("_atomic_") {
                regs(data, &mut r)?;
                if cache.glc {
                    regs(&atomic_result(data, name), &mut w)?;
                }
            } else {
                regs(data, &mut w)?;
            }
        }
        Fields::Flat {
            dst,
            addr,
            data,
            saddr,
            ..
        } => {
            for o in [addr, data, saddr].into_iter().flatten() {
                regs(o, &mut r)?;
            }
            if let Some(d) = dst {
                regs(d, &mut w)?;
            }
        }
        Fields::Exp { src, .. } => {
            for s in src.iter().flatten() {
                regs(s, &mut r)?;
            }
        }
    }
    if vector {
        r.extend(mask(EXEC));
    }
    let mut seen = Vec::with_capacity(r.len());
    r.retain(|reg| {
        let new = !seen.contains(reg);
        seen.push(*reg);
        new
    });
    Ok(Some((r, w)))
}

fn writes_scc(encoding: gfx10::Encoding, name: &str) -> bool {
    use gfx10::Encoding::*;
    match encoding {
        Sopc => true,
        Sopk => name.starts_with("s_cmpk_") || name == "s_addk_i32",
        _ => {
            let stem = name.split('_').nth(1).unwrap_or("");
            name.contains("saveexec")
                || name.contains("wrexec")
                || matches!(
                    stem,
                    "add"
                        | "sub"
                        | "addc"
                        | "subb"
                        | "min"
                        | "max"
                        | "and"
                        | "or"
                        | "xor"
                        | "andn2"
                        | "orn2"
                        | "nand"
                        | "nor"
                        | "xnor"
                        | "lshl"
                        | "lshr"
                        | "ashr"
                        | "bfe"
                        | "absdiff"
                        | "lshl1"
                        | "lshl2"
                        | "lshl3"
                        | "lshl4"
                        | "not"
                        | "wqm"
                        | "bcnt0"
                        | "bcnt1"
                        | "quadmask"
                        | "abs"
                )
        }
    }
}

fn atomic_result(data: &Operand, name: &str) -> Operand {
    match *data {
        Operand::Vector { reg, dwords } if name.contains("cmpswap") => Operand::Vector {
            reg,
            dwords: dwords / 2,
        },
        op => op,
    }
}

fn add_regs(op: &Operand, lanes: u8, out: &mut Vec<Reg>) -> Result<(), ()> {
    match *op {
        Operand::Scalar { code, .. } if code == gfx10::NULL => {}
        Operand::Scalar { code, dwords } => {
            let end = u16::from(code) + u16::from(dwords);
            let limit = if code < PARKED { 128 } else { 256 };
            if end > limit {
                return Err(());
            }
            out.extend((u16::from(code)..end).map(|c| Reg::S(c as u8)));
        }
        Operand::Vector { reg, dwords } => {
            if u16::from(reg) + u16::from(dwords) > 256 {
                return Err(());
            }
            out.extend((0..dwords).map(|i| Reg::V(reg + i)));
        }
        Operand::Const(251) => out.extend((0..lanes).map(|i| Reg::S(VCC + i))),
        Operand::Const(252) => out.extend((0..lanes).map(|i| Reg::S(EXEC + i))),
        Operand::Const(253) => out.push(Reg::Scc),
        Operand::Const(254) => out.push(Reg::S(M0)),
        Operand::Const(_) | Operand::Literal(_) | Operand::Attr { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn build_words(words: &[u32], wave64: bool) -> Program {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        build(&code, wave64, 256).unwrap()
    }

    fn input(p: &Program, reg: Reg) -> Value {
        p.inputs.iter().find(|i| i.0 == reg).expect("input").1
    }

    fn skip(body: &[u32], after: &[u32]) -> Program {
        let mut w = vec![
            0xBE94_0306,
            0x7D88_0000,
            0xBE8C_246A,
            0xBF88_0000 | body.len() as u32,
        ];
        w.extend_from_slice(body);
        w.push(0xBEFE_040C);
        w.extend_from_slice(after);
        w.push(S_ENDPGM);
        build_words(&w, true)
    }

    fn skips(p: &Program) -> usize {
        p.blocks
            .iter()
            .filter(|b| {
                b.insts
                    .last()
                    .is_some_and(|i| i.name() == "s_cbranch_execz")
            })
            .count()
    }

    #[test]
    fn a_skip_is_left_out_when_the_scalars_it_changes_come_back_or_are_not_read() {
        const SET: u32 = 0xBE86_0385;
        const USE: u32 = 0x7E02_0206;
        const BACK: u32 = 0xBE86_0314;
        const READ: u32 = 0x7E04_0206;
        let kept = skip(&[SET, USE], &[READ]);
        assert_eq!(skips(&kept), 1);
        assert!(without_skips(&kept, false).is_none());
        assert!(without_skips(&kept, true).is_none());
        let back = skip(&[SET, USE, BACK], &[READ]);
        assert!(without_skips(&back, false).is_none());
        assert_eq!(skips(&without_skips(&back, true).expect("left out")), 0);
        let unread = skip(&[SET, USE], &[]);
        assert_eq!(skips(&without_skips(&unread, false).expect("left out")), 0);
        assert_eq!(skips(&without_skips(&unread, true).expect("left out")), 0);
        let idle = skip(&[SET, USE], &[0xBEFE_0406, 0xBEFE_040C, 0x7E04_0200]);
        assert!(without_skips(&idle, false).is_none());
        assert_eq!(skips(&without_skips(&idle, true).expect("left out")), 0);
    }

    #[test]
    fn joins_get_phis_only_where_values_differ() {
        let code = [
            0xBF06_0100,
            0xBF85_0001,
            0xBE82_0380,
            0x7E00_0202,
            0x7E02_0200,
            S_ENDPGM,
        ];
        for wave64 in [false, true] {
            let p = build_words(&code, wave64);
            let [b0, b1, b2] = &p.blocks[..] else {
                panic!("{:?}", p.blocks)
            };
            assert_eq!(
                b0.exit,
                Exit::Branch {
                    taken: 2,
                    not_taken: 1
                }
            );
            assert_eq!(b1.exit, Exit::Jump(2));
            assert_eq!(b2.preds, [0, 1]);
            assert_eq!(b0.insts[1].read(Reg::Scc), b0.insts[0].write(Reg::Scc));

            let [phi] = &b2.phis[..] else {
                panic!("{:?}", b2.phis)
            };
            assert_eq!(phi.reg, Reg::S(2));
            assert_eq!(
                phi.args,
                [input(&p, Reg::S(2)), b1.insts[0].write(Reg::S(2)).unwrap()]
            );
            let [mov0, mov1, end] = &b2.insts[..] else {
                panic!()
            };
            assert_eq!(mov0.read(Reg::S(2)), Some(phi.value));
            assert_eq!(mov1.read(Reg::S(0)), Some(input(&p, Reg::S(0))));
            assert_eq!(mov1.read(Reg::V(1)), Some(input(&p, Reg::V(1))));
            assert_eq!(mov1.read(Reg::S(EXEC)), Some(input(&p, Reg::S(EXEC))));
            assert_eq!(mov1.read(Reg::S(EXEC + 1)).is_some(), wave64);
            assert_eq!((end.name(), b2.exit), ("s_endpgm", Exit::End));
        }
    }

    #[test]
    fn in_place_scalar_ops_read_their_destination() {
        let p = build_words(&[0xBE8D_1D94, 0xB78E_0010, 0xBE8F_0380, S_ENDPGM], false);
        let [bitset, addk, mov, _] = &p.blocks[0].insts[..] else {
            panic!("{:?}", p.blocks[0].insts)
        };
        assert_eq!(bitset.read(Reg::S(13)), Some(input(&p, Reg::S(13))));
        assert_eq!(addk.read(Reg::S(14)), Some(input(&p, Reg::S(14))));
        assert_eq!(mov.read(Reg::S(15)), None);
    }

    #[test]
    fn loops_carry_values_through_phis() {
        let p = build_words(
            &[0xBE80_0380, 0x8000_8100, 0xBF07_8400, 0xBF85_FFFD, S_ENDPGM],
            true,
        );
        assert!(p.inputs.is_empty(), "{:?}", p.inputs);
        let [b0, b1, b2] = &p.blocks[..] else {
            panic!()
        };
        assert_eq!(b1.preds, [0, 1]);
        assert_eq!(
            b1.exit,
            Exit::Branch {
                taken: 1,
                not_taken: 2
            }
        );
        let [phi] = &b1.phis[..] else {
            panic!("{:?}", b1.phis)
        };
        let add = &b1.insts[0];
        assert_eq!(phi.reg, Reg::S(0));
        assert_eq!(
            phi.args,
            [
                b0.insts[0].write(Reg::S(0)).unwrap(),
                add.write(Reg::S(0)).unwrap()
            ]
        );
        assert_eq!(add.read(Reg::S(0)), Some(phi.value));
        assert_eq!(b1.insts[1].read(Reg::S(0)), add.write(Reg::S(0)));
        assert_eq!(b2.insts[0].name(), "s_endpgm");
    }

    #[test]
    fn drops_scheduling_and_follows_far_jumps() {
        let p = build_words(
            &[
                0xBF80_0000,
                0xBE88_1F00,
                0x8008_FF08,
                20,
                0x8209_8009,
                0xBE80_2008,
                0xFFFF_FFFF,
                S_ENDPGM,
            ],
            false,
        );
        let names: Vec<_> = p.blocks[0].insts.iter().map(Inst::name).collect();
        assert_eq!(
            names,
            ["s_getpc_b64", "s_add_u32", "s_addc_u32", "s_setpc_b64"]
        );
        assert_eq!(p.blocks[0].exit, Exit::Jump(1));
        assert_eq!(p.blocks[1].offset, 28);
        let i = &p.blocks[0].insts;
        assert_eq!(i[2].read(Reg::Scc), i[1].write(Reg::Scc));
    }

    #[test]
    fn reports_what_it_cannot_express() {
        let code: Vec<u8> = [0xBE80_2E01_u32, S_ENDPGM]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        assert_eq!(
            build(&code, false, 256),
            Err(ShaderError::Unsupported {
                offset: 0,
                what: "s_movrels_b32"
            })
        );
    }

    #[test]
    fn relative_vgpr_access_spans_up_to_the_vgpr_count() {
        let code: Vec<u8> = [0x7E04_8501_u32, 0x7E00_8704, S_ENDPGM]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let p = build(&code, false, 6).unwrap();
        let [movreld, movrels, _] = &p.blocks[0].insts[..] else {
            panic!()
        };
        let vgprs = |regs: &[(Reg, Value)]| -> Vec<u8> {
            regs.iter()
                .filter_map(|r| match r.0 {
                    Reg::V(v) => Some(v),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(vgprs(&movreld.writes), [2, 3, 4, 5]);
        assert_eq!(vgprs(&movreld.reads), [1, 2, 3, 4, 5]);
        assert!(movreld.read(Reg::S(M0)).is_some());
        assert_eq!(vgprs(&movrels.writes), [0]);
        assert_eq!(vgprs(&movrels.reads), [4, 5, 0]);
        assert_eq!(movrels.read(Reg::V(4)), movreld.write(Reg::V(4)));
        assert!(build(&code, false, 4).is_err(), "v4 is past 4 VGPRs");
    }
}
