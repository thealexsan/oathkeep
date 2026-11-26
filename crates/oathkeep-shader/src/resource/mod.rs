//! Resource tracking (decision 0003): where each descriptor a program uses
//! comes from, in terms of the root signature.
//!
//! Root parameters arrive in user SGPRs ([`RootParam`]): a descriptor table
//! as the 32-bit heap address of its range (the code sets the high half to
//! 0), a root descriptor as a 64-bit buffer address, root constants as
//! themselves. A root signature with dynamic resources (SM 6.6) adds the
//! 32-bit base addresses of both heaps; a descriptor is then loaded from
//! one of them at an index the code computes (from a root constant, say). The code loads table descriptors from heap memory with
//! `s_load_dword*`, and builds a buffer descriptor around a root
//! descriptor's address itself. Each descriptor operand and each scalar
//! memory address is traced through the SSA values back to those
//! parameters, by data flow: copies, phis that agree, and additions.

use crate::ShaderError;
use crate::gfx10::{Fields, Operand};
use crate::ir::{Program, Reg, Value};
use crate::native::{HwStage, RootParam};
use crate::structure::{Bool, Meets, Structured};

/// An instruction: block and index in [`crate::ir::Block::insts`].

mod addressing;
mod copy_tracking;
mod lane;
mod tracer;
mod types;
mod waterfall;
#[cfg(test)]
pub(crate) mod tests;

pub(crate) use copy_tracking::inline;
pub use types::*;
use types::RAW;
pub(crate) use copy_tracking::Moved;

/// Traces every descriptor and scalar address of `p`, which runs as
/// `stage`, to the root parameters in `layout`. Fails on the first one it
/// cannot trace. `lanes`, `p` as one lane runs it, tells which paths the
/// lanes that access a per-lane descriptor took and which writes they ran;
/// without it, per-lane descriptors chosen by control flow are refused and
/// per-lane writes are taken to run in every lane.
pub fn track(
    p: &Program,
    layout: &[RootParam],
    stage: HwStage,
    lanes: Option<&Structured>,
) -> Result<Resources, ShaderError> {
    let t = Tracer::new(p, layout, stage, lanes)?;
    let mut out = Resources {
        params: p
            .inputs
            .iter()
            .filter_map(|&(reg, v)| {
                let (param, dword) = t.param_of(reg)?;
                Some((v, param, dword))
            })
            .collect(),
        ..Resources::default()
    };
    // Group accesses in the same loop so a shared mask can select several
    // descriptors together.
    let mut first_lane_at = std::collections::BTreeMap::<usize, (Vec<usize>, Vec<Value>)>::new();
    // The console compiler's waterfall reads the first lane's table offset
    // and loads that entry: accesses, offset reads and loads by block.
    let mut offset_at =
        std::collections::BTreeMap::<usize, (Vec<usize>, Vec<Value>, Vec<Site>)>::new();
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            let site = (b, i);
            t.at.set(Some(site));
            if let Some(value) = t.restored(inst, Some(b)) {
                out.restored_lanes.insert(site, value);
            }
            let fail = |what| ShaderError::Unsupported {
                offset: inst.machine.offset,
                what,
            };
            let mut first_lane = Vec::new();
            let mut offset = Vec::new();
            let mut descriptor = |op: &Operand, role| -> Result<(), ShaderError> {
                let source = t.descriptor(inst, op, site).map_err(fail)?;
                // A descriptor the code keeps per lane ([`Tracer::lane_copies`])
                // is the same in every lane: its loop is lowered as it stands
                // and needs no proof.
                if !t.kept_per_lane(inst, op) {
                    first_lane.extend(t.first_lane_words(inst, op).unwrap_or_default());
                }
                if let Source::Loaded { load, .. } = source
                    && out.scalar.iter().any(|s| s.0 == load)
                    && let Some(v) = t.first_lane_offset(load)
                {
                    offset.push((v, load));
                }
                out.descriptors.push(Descriptor { site, role, source });
                Ok(())
            };
            match &inst.fields {
                Fields::Image { rsrc, .. } if inst.name().starts_with("image_bvh") => {
                    // The ray and node test names no texture. A descriptor
                    // whose flag dwords the code sets as numbers and whose
                    // type is a ray-tracing structure is a range of title
                    // memory (the title: all of it, or from one structure's
                    // address on), and the node's address is among the
                    // operands; any other descriptor must be a resource of
                    // the host.
                    let word = |k: u8| match *rsrc {
                        Operand::Scalar { code, dwords } if k < dwords => {
                            inst.read(Reg::S(code + k)).map(|v| t.origin(v))
                        }
                        _ => None,
                    };
                    match (word(1), word(3)) {
                        (Some(Term::Const(word1)), Some(Term::Const(word3)))
                            if word3 >> 28 == crate::bvh::DESCRIPTOR_TYPE =>
                        {
                            out.bvh.push((site, BvhNodes::Memory { word1, word3 }));
                        }
                        _ => {
                            descriptor(rsrc, Role::Resource)?;
                            out.bvh.push((site, BvhNodes::Resource));
                        }
                    }
                }
                Fields::Image { rsrc, sampler, .. } => {
                    descriptor(rsrc, Role::Resource)?;
                    if let Some(s) = sampler {
                        descriptor(s, Role::Sampler)?;
                    }
                }
                // Cache invalidations name a buffer's address space but do
                // not read its descriptor; the host has no cache to drop.
                Fields::Buffer { rsrc, .. } if !inst.name().ends_with("_inv") => {
                    descriptor(rsrc, Role::Resource)?
                }
                Fields::Buffer { .. } => {}
                Fields::Smem { base, .. } if inst.name().starts_with("s_buffer_") => {
                    descriptor(base, Role::Resource)?;
                }
                Fields::Smem {
                    base,
                    offset,
                    soffset,
                    ..
                } if inst.name().contains("_load_") || inst.name().contains("_store_") => {
                    let address = t
                        .smem_address(inst, base, *offset, soffset)
                        .or_else(|why| t.by_address(why))
                        .map_err(fail)?;
                    out.scalar.push((site, address));
                }
                Fields::Flat {
                    addr,
                    saddr,
                    offset,
                    ..
                } if inst.name().starts_with("global_") => {
                    let address = t
                        .flat_address(inst, addr.as_ref(), saddr.as_ref(), *offset)
                        .or_else(|why| t.by_address(why))
                        .map_err(fail)?;
                    out.scalar.push((site, address));
                }
                _ => {}
            }
            if !first_lane.is_empty() {
                let (accesses, reads) = first_lane_at.entry(b).or_default();
                accesses.push(i);
                reads.extend(first_lane);
            }
            if !offset.is_empty() {
                let (accesses, reads, loads) = offset_at.entry(b).or_default();
                accesses.push(i);
                for (v, load) in offset {
                    reads.push(v);
                    loads.push(load);
                }
            }
        }
    }
    // What follows traces no access of one block.
    t.at.set(None);
    // The proof needs the per-lane form. Without it nothing is emitted, and
    // its failure is the one to report, so the reads stay unproven here.
    if t.lanes.is_none() {
        return Ok(out);
    }
    for (block, (accesses, mut reads)) in first_lane_at {
        reads.sort_unstable();
        reads.dedup();
        let proof = t
            .waterfall(block, &accesses, &reads)
            .map_err(|what| ShaderError::Unsupported {
                offset: t.inst((block, accesses[0])).machine.offset,
                what,
            })?;
        out.waterfall.compares.extend(proof.compares);
        out.waterfall.reads.extend(proof.reads);
    }
    // Unproven, an offset read stays the first lane's (one uniform entry),
    // which is what the hardware reads outside a waterfall loop.
    for (block, (accesses, mut reads, loads)) in offset_at {
        reads.sort_unstable();
        reads.dedup();
        if let Ok(proof) = t.waterfall(block, &accesses, &reads) {
            out.waterfall.compares.extend(proof.compares);
            out.waterfall.reads.extend(proof.reads);
            out.waterfall.loads.extend(loads);
        }
    }
    out.lane_copied = t.lane_copied.take();
    out.lane_copied.sort_unstable();
    out.lane_copied.dedup();
    out.root_indexed_views = out
        .descriptors
        .iter()
        .filter_map(|d| t.root_indexed_view(d, &out.scalar))
        .collect();
    Ok(out)
}

/// A dword after copy propagation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Term {
    Const(u32),
    Value(Value),
}

/// A 32-bit sum: optional root parameter (as its first dword), constant,
/// optional computed term.
pub(super) struct Sum {
    base: Option<Value>,
    offset: u32,
    dynamic: Option<Value>,
}

pub(super) struct Tracer<'a> {
    pub(super) p: &'a Program,
    layout: &'a [RootParam],
    stage: HwStage,
    /// Defining instruction per value (phis and inputs have none).
    defs: Vec<Option<Site>>,
    /// Block and index of each phi value.
    phis: Vec<Option<(usize, usize)>>,
    /// Register per input value.
    inputs: Vec<Option<Reg>>,
    origin: Vec<Term>,
    /// See [`Resources::lane_copied`].
    lane_copied: std::cell::RefCell<Vec<Site>>,
    lanes: Option<&'a Structured>,
    /// While set, [`Self::sum`] keeps the first-lane read of an index as
    /// the computed term instead of what the lanes hold.
    read_first_lane: std::cell::Cell<bool>,
    /// What the conditions asked of `lanes` so far came down to: the proofs
    /// of one program ask about the same blocks and masks again and again.
    meets: std::cell::RefCell<Meets>,
    /// Reverse-postorder index per block.
    rpo: Vec<usize>,
    /// The instruction site of the access being traced ([`Self::live`]).
    at: std::cell::Cell<Option<Site>>,
}

/// What lanes are known to satisfy: each [`Bool`] where its flag is set,
/// its negation where not.
pub(super) type Known = Vec<(Bool, bool)>;

pub(super) const MAX_ROUNDS: usize = 64;
pub(super) const MAX_JOINS: usize = 8;
/// VGPR values followed for one dword of a descriptor kept per lane.
pub(super) const MAX_COPIES: usize = 64;

