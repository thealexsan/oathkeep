//! Control-flow structurisation (decision 0003): the program as one lane
//! runs it, without EXEC.
//!
//! Lane masks (EXEC, compare and carry results, the SGPRs they are saved
//! in) become per-lane booleans ([`Bools`]). A branch on a wave-wide test of
//! a mask (`s_cbranch_execz`, `vccz`, SCC of a mask operation) becomes a
//! branch on the lane's own bit: compiled code only skips work that lanes
//! outside the mask would not change, or tests a value that is the same in
//! every lane, so either way each lane can decide alone. A path-sensitive
//! pass then proves, for each vector instruction, that EXEC holds in every
//! lane that reaches it; what it cannot prove stays as the instruction's
//! guard, a per-lane `if` around it.
//!
//! The control-flow graph keeps its shape and must be reducible (DXIL's
//! only requirement). Helper lanes of a pixel shader count as live: on PC
//! their side effects are dropped too, and what they compute is only read
//! through derivatives.

use std::collections::{HashMap, HashSet};

use crate::ShaderError;
use crate::ir::{Program, Value};


mod bools;
mod lanes;
mod masks;
mod step;
#[cfg(test)]
mod tests;

pub use bools::{Atom, Bool, Bools, Meets};
use bools::Map;

/// How a lane leaves a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneExit {
    Jump(usize),
    /// To `taken` in lanes where `when` holds. A condition over
    /// [`Atom::Uniform`]s only is a uniform branch.
    Branch {
        when: Bool,
        taken: usize,
        not_taken: usize,
    },
    End,
    Indirect,
}

/// A [`Program`] as one lane runs it: same blocks, per-lane branches.
#[derive(Debug, Clone)]
pub struct Structured {
    pub bools: Bools,
    /// Per block.
    pub exits: Vec<LaneExit>,
    /// Per block and instruction: the condition, beyond reaching it, under
    /// which a lane runs a vector instruction; `None` when reaching it is
    /// enough (lanes outside EXEC then never get there, so a VGPR write
    /// needs no old value). Always `None` for scalar instructions.
    pub guards: Vec<Vec<Option<Bool>>>,
    /// Per block: what holds in every lane that enters it (over forward
    /// edges; a loop header forgets what its loop redefines).
    pub reach: Vec<Bool>,
    /// Per block: the lane's EXEC bit at the end of the block (intersected
    /// with `reach`), i.e. whether the lane is active when leaving the block.
    pub exit_exec: Vec<Bool>,
    /// The lane's bit of each lane-mask value, by its low dword's value.
    pub masks: HashMap<Value, Bool>,
    /// Exact Wave64 pairs for checking masks at their uses.
    paired_masks: Map<Key, Bool>,
    /// Wave64 masks: low dword's value by the high dword's.
    pub highs: HashMap<Value, Value>,
    /// Masks (low dwords) set only in lanes whose EXEC bit is set where
    /// they are written: every such lane reaches the writing block, so a
    /// ballot there gives the mask as data. No split low dword, as in
    /// `masks`.
    pub active: HashSet<Value>,
    /// Incoming lane boolean per predecessor edge (in block predecessor order)
    /// for each [`Atom::Phi`] id.
    pub phi_masks: HashMap<Value, Vec<Bool>>,
    /// Blocks whose closing branch the whole wave takes together ([`structure_wide`]): its
    /// `when` is still the lane's own bit; the wave goes by all of them.
    pub wide: HashSet<usize>,
    /// Per block closed by a branch on a lane mask: whether the console takes it when some
    /// lane's `when` holds (`true`) or when every lane's does (`false`).
    pub wave_any: Vec<Option<bool>>,
    /// SCC values a mask operation sets when some lane's bit of its result is set, with that
    /// lane bit: as data such an SCC is "some lane of the wave has it".
    pub scc_some: HashMap<Value, Bool>,
}

impl Structured {
    pub(crate) fn mask_bits(&self, low: Value, high: Option<Value>) -> Option<Bool> {
        match high {
            Some(high) => self.paired_masks.get(&(low, high)),
            None => self.masks.get(&low),
        }
        .copied()
    }
}

/// Builds the per-lane form of `program`.
pub fn structure(program: &Program) -> Result<Structured, ShaderError> {
    structure_wide(program, &HashSet::new())
}

/// [`structure`], with the branches that close the blocks of `wide` kept as the console has
/// them: one decision for the whole wave (`s_cbranch_execz` and `vccz`: taken when no lane's
/// bit is set; `execnz` and `vccnz`: when some lane's is; SCC of a mask operation likewise,
/// [`Structured::wave_any`]), which every lane follows. A lane
/// then learns nothing about its own bit from the way it went, so past such a branch EXEC
/// stays a guard on every vector instruction, as it is on the console. For a console wave
/// run as two host waves, which exchange that decision and can then meet (a group barrier)
/// inside the branch.
pub fn structure_wide(program: &Program, wide: &HashSet<usize>) -> Result<Structured, ShaderError> {
    let mut l = Lanes::new(program)?;
    l.wide = wide.clone();
    loop {
        let (execs, exit_exec) = l.masks_in_order();
        let cond = l.paths()?;
        let (wrong, new_derived) = l.check_joins(&cond);
        if l.b.overflow {
            return Err(TOO_LARGE);
        }
        if wrong.is_empty() && new_derived.is_empty() {
            return l.finish(&execs, &exit_exec, &cond);
        }
        l.unresolved.extend(wrong);
        l.derived_phis.extend(new_derived);
        l.reset();
    }
}

/// A lane mask by the values of its low and high dword (both the low one in
/// wave32).
pub(super) type Key = (Value, Value);

pub(super) struct Lanes<'a> {
    pub(super) p: &'a Program,
    pub(super) order: Vec<usize>,
    /// Reverse-postorder index per block.
    pub(super) index: Vec<usize>,
    /// Per block, in predecessor order: (block, over its taken edge).
    incoming: Vec<Vec<(usize, bool)>>,
    /// Block and instruction index writing each value.
    defs: Map<Value, (usize, usize)>,
    /// Block and index of each phi value.
    phis: Map<Value, (usize, usize)>,
    /// Mask joins found wrong when resolved to a constant: atoms instead.
    unresolved: HashSet<Value>,
    /// Phi ids whose joins depend on EXEC or VALU results (including along
    /// back edges), carried across resets so their joins stay in `masks`.
    derived_phis: HashSet<Value>,
    b: Bools,
    /// Masks derived from EXEC and VALU results.
    masks: Map<Key, Bool>,
    /// Other values read as masks: constants, uniform selects, opaque ones.
    other: Map<Key, Bool>,
    /// Pairs read as masks that `mask` does not see: carry-ins, and pairs
    /// joins take over back edges.
    seen: Vec<Key>,
    /// For branches: SCC of a mask operation ("some lane is set") as the
    /// lane's own bit.
    scc_lane: Map<Value, Bool>,
    /// Joins resolved to a constant, to check once all edges are known.
    joins: Vec<Join>,
    /// [`Atom::Phi`] joins created in the current pass, resolved in
    /// `check_joins` and exported in `finish`.
    phi_joins: Vec<PhiJoin>,
    phi_join_ids: HashSet<Value>,
    /// Per block: what a lane knows on entry, over forward edges only.
    first: Vec<Bool>,
    /// Per block ending in a branch: when a lane takes it.
    when: Vec<Option<Bool>>,
    /// See [`Structured::active`].
    active: HashSet<Value>,
    /// See [`Structured::wide`].
    wide: HashSet<usize>,
    /// SCC values of [`Self::scc_lane`] that are set when EVERY lane's bit is (`s_cmp_eq` of
    /// two masks: "no lane differs"); the others are set when some lane's is.
    scc_every: HashSet<Value>,
    /// See [`Structured::wave_any`].
    wave_any: Vec<Option<bool>>,
}

/// A mask join taken as `value` in every lane that reaches `block`: each
/// incoming mask held (or failed) on its edge.
pub(super) struct Join {
    pub(super) block: usize,
    /// The phi value naming the join.
    id: Value,
    value: bool,
    derived: bool,
    args: Vec<Key>,
}

/// A mask join represented as [`Atom::Phi`], whose incoming edge masks are
/// resolved once every block (including loop back edges) has been visited.
pub(super) struct PhiJoin {
    pub(super) block: usize,
    id: Value,
    derived: bool,
    args: Vec<Key>,
}

pub(super) struct SopMasks {
    dst: Option<(Key, Bool)>,
    exec: Option<(Key, Bool)>,
    /// Some operand derives from EXEC or a VALU result.
    derived: bool,
    /// For branches: SCC ("result is not zero") per lane.
    scc: Option<Bool>,
}

pub(super) const MAX_ROUNDS: usize = 256;

pub(super) const TOO_LARGE: ShaderError = ShaderError::Isa {
    offset: 0,
    reason: "lane conditions too large",
};

/// `ir::build` splits irreducible loops; checked again because everything
/// here assumes a reducible graph.
pub(super) fn check_reducible(p: &Program, order: &[usize], index: &[usize]) -> Result<(), ShaderError> {
    match crate::ir::irreducible_edge(&p.blocks, order, index) {
        Some((from, _)) => Err(ShaderError::Unsupported {
            offset: p.blocks[from].offset,
            what: "irreducible control flow",
        }),
        None => Ok(()),
    }
}

