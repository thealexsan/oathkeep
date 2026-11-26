//! One-to-one SSA form of a native program (decision 0003, step 1).
//!
//! Every machine instruction becomes one [`Inst`], except scheduling-only
//! ones (`s_nop`, `s_waitcnt*`, `s_inst_prefetch`...) and redundant conditional
//! branches.
//! Each register dword is a variable: a read names the [`Value`] it sees, a
//! write defines a new one, and [`Phi`]s merge values where control flow
//! joins. Nothing is raised: an instruction keeps its opcode and decoded
//! fields, so its exact semantics stay with it.
//!
//! Implicit operands are explicit: SCC, M0, and EXEC, which every vector
//! instruction reads. A write to a VGPR also reads the register's old value,
//! because lanes disabled in EXEC keep it.
//!
//! M0-relative VGPR access (`v_movrels/d/sd_b32`, `v_swaprel_b32`) names its
//! operand's whole possible range: from the encoded register up to the
//! program's VGPR count, all read (and all written, for a destination).


use crate::gfx10::{Fields, Instruction};


mod build;
mod cfg;
mod effects;
mod transform;
mod unspill;
#[cfg(test)]
mod tests;

pub use build::build;
pub(crate) use cfg::{idoms, irreducible_edge, reverse_postorder};
pub(crate) use effects::operand_reads;
pub(crate) use transform::{thread_edges, without_early_outs, without_skips};

/// One register dword. Scalar codes as in [`Operand::Scalar`]: 0-105 SGPRs,
/// 106/107 VCC, 108-123 trap temporaries, 124 M0, 126/127 EXEC. Codes from
/// [`PARKED`] up are scalars the compiler parked in VGPR lanes (`unspill`):
/// no machine instruction names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reg {
    S(u8),
    Scc,
    V(u8),
}

pub const VCC: u8 = 106;
pub const M0: u8 = 124;
pub const EXEC: u8 = 126;
/// The first scalar code past the machine's (0-127): see [`Reg`].
pub const PARKED: u8 = 128;

/// An SSA value: the content of one register dword (SCC: one bit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Value(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub wave64: bool,
    /// VGPRs v0 up to this exist; bounds M0-relative access.
    pub vgprs: u16,
    /// In code order, then copies made to give every loop one entry; block 0
    /// is the entry.
    pub blocks: Vec<Block>,
    /// Registers read before any write, with their value on entry: what the
    /// hardware or a called fetch routine set up (user SGPRs, system values,
    /// vertex inputs).
    pub inputs: Vec<(Reg, Value)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Byte offset of the block's first machine instruction (shared by a
    /// block and its copies).
    pub offset: usize,
    pub preds: Vec<usize>,
    pub phis: Vec<Phi>,
    pub insts: Vec<Inst>,
    pub exit: Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Jump(usize),
    /// `s_cbranch_*`, the block's last instruction.
    Branch {
        taken: usize,
        not_taken: usize,
    },
    /// `s_endpgm` and its variants.
    End,
    /// `s_setpc_b64` to an address held in registers, e.g. the handover to
    /// the next half of a merged hardware stage.
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
    /// One per predecessor, in [`Block::preds`] order.
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inst {
    pub machine: Instruction,
    pub fields: Fields,
    /// Every register dword read, with the value it holds.
    pub reads: Vec<(Reg, Value)>,
    /// Every register dword written, with the value it receives.
    pub writes: Vec<(Reg, Value)>,
}

impl Inst {
    pub fn name(&self) -> &'static str {
        self.machine.name().unwrap_or("")
    }

    /// The value `reg` holds when this instruction reads it.
    pub fn read(&self, reg: Reg) -> Option<Value> {
        self.reads.iter().find(|r| r.0 == reg).map(|r| r.1)
    }

    /// The value this instruction writes to `reg`.
    pub fn write(&self, reg: Reg) -> Option<Value> {
        self.writes.iter().find(|w| w.0 == reg).map(|w| w.1)
    }
}

