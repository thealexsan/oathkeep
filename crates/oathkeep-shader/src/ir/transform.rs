
use crate::ShaderError;

use super::cfg::{NONE, REGS, index, link, liveness, lookup, members, remove_trivial_phis, reverse_postorder, split_irreducible};
use super::{Block, Exit, Inst, Phi, Program, Reg, Value};

pub(super) fn rename(mut blocks: Vec<Block>, wave64: bool, vgprs: u16) -> Program {
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
                // A single predecessor comes earlier in reverse postorder.
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

/// Duplicate a join for each proven (predecessor, join, destination) edge.
/// The instructions still execute on that path and define fresh SSA values.
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

/// The program without its early-outs: an `s_cbranch_execz` to a block that
/// only ends the program (`if (!inBounds) return;`, the outermost branch of
/// most compute programs) is left out, so the lanes that would have left
/// walk the body with EXEC clear. Vector instructions and every effect are
/// masked by EXEC already, as on the console, where the branch is taken
/// only when no lane of the wave is left; scalar code has no effects. The
/// body is then code every lane runs, which a console wave run as two host
/// waves needs for the barriers of its exchanges and LDS accesses.
/// `None` if the program has no such branch.
pub(crate) fn without_early_outs(p: &Program) -> Option<Program> {
    let ends = |b: usize| {
        p.blocks[b].exit == Exit::End
            && p.blocks[b].insts.iter().all(|i| matches!(i.fields, crate::gfx10::Fields::Sopp { .. }))
    };
    let mut blocks = p.blocks.clone();
    let mut any = false;
    for block in &mut blocks {
        let Exit::Branch { taken, not_taken } = block.exit else {
            continue;
        };
        if ends(taken) && block.insts.last().is_some_and(|i| i.name() == "s_cbranch_execz") {
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

/// As [`without_early_outs`], and also without the branches that only skip a
/// region when no lane is left for it: an `s_cbranch_execz` whose target is
/// where the skipped code ends up anyway (`if (c) { ... }` with nothing
/// else leaving the region). Every lane then walks the region with EXEC
/// clear for those that would have skipped it, and LDS accesses and
/// exchanges inside it are in code every lane runs.
/// The console takes such a branch only when no lane of the wave is left,
/// and then its scalar code does not run: a branch is kept if the code
/// after it could see that (a scalar register or SCC that the region
/// changes and the join merges with the value from before; EXEC aside,
/// which is empty either way). `None` if nothing changes.
///
/// `deep` looks closer at what the code after the branch can see. A scalar the region moves
/// away and back (the compiler parks scalars in VGPR lanes around a branch and takes them
/// back before the join, `unspill`) is the value it copies; a join nothing reads shows no
/// one the difference (a restore of EXEC that the next one overwrites reads nothing); and
/// the join may have other ways in (an inner branch that ends together with an outer one).
/// That leaves out most of the skips of the title's large lighting programs, whose bodies
/// are then code every lane runs.
pub(crate) fn without_skips(p: &Program, deep: bool) -> Option<Program> {
    let n = p.blocks.len();
    let ends = |b: usize| {
        p.blocks[b].exit == Exit::End
            && p.blocks[b].insts.iter().all(|i| matches!(i.fields, crate::gfx10::Fields::Sopp { .. }))
    };
    // Scalars moved from register to register (a scalar the compiler parked in a VGPR lane
    // comes back by such a move, `unspill`) are the value they copy.
    let mut copy: std::collections::HashMap<Value, Value> = Default::default();
    // Values something reads, directly or through joins. A scalar operation none of whose
    // results is read reads nothing either (the compiler restores EXEC from a saved mask
    // and overwrites it at once where two branches end together).
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
                && inst.reads.iter().chain(&inst.writes).all(|r| matches!(r.0, Reg::S(_)))
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
    // The region a skip of `b` to `target` leaves out, if it is a pure skip.
    let skipped = |b: usize, target: usize, first: usize| -> Option<Vec<bool>> {
        let mut inside = vec![false; n];
        let mut stack = vec![first];
        while let Some(x) = stack.pop() {
            if x == target || inside[x] {
                continue;
            }
            if x == b {
                return None; // a loop back to the branch
            }
            inside[x] = true;
            match p.blocks[x].exit {
                Exit::End | Exit::Indirect => return None,
                exit => stack.extend(exit.successors()),
            }
        }
        // entered only through the branch
        for x in (0..n).filter(|&x| inside[x]) {
            if p.blocks[x].preds.iter().any(|&q| q != b && !inside[q]) {
                return None;
            }
        }
        // the join sees no scalar the region changed
        let join = &p.blocks[target];
        for phi in &join.phis {
            let scalar = match phi.reg {
                Reg::S(c) => c != 126 && c != 127,
                Reg::Scc => true,
                Reg::V(_) => false,
            };
            // (a join nothing reads shows no one the difference)
            if !scalar || deep && !live.contains(&phi.value) {
                continue;
            }
            let before: Vec<Value> =
                join.preds.iter().zip(&phi.args).filter(|(q, _)| **q == b).map(|(_, a)| root(*a)).collect();
            let within = join.preds.iter().zip(&phi.args).filter(|(q, _)| inside[**q]).map(|(_, a)| root(*a));
            if within.into_iter().any(|a| !before.contains(&a)) {
                return None;
            }
        }
        // Other ways into the join than the branch and the region (an inner branch that ends
        // together with an outer one) change nothing in that: the lanes that would have
        // skipped arrive from the region instead, with the same scalars.
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
        if !p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execz") {
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

