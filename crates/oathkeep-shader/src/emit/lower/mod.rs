//! IR to DXIL, one machine instruction at a time (decision 0003).
//!
//! Every register dword is an `i32` (SCC an `i1`); float operations bitcast
//! their operands. Lane masks exist only as the per-lane booleans of
//! structurisation; branches and guards are built from them. A value the
//! emitter cannot express is `Missing` with a reason, and only fails the
//! program when something with an effect (an export, a branch) needs it.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::UserData;
use super::bitcode::{Attr, Bin, Cast, Rmw, Tag, Term, V, pred};
use super::container::SigElement;
use super::shader::{Shader, flags};
use crate::ShaderError;
use crate::gfx10::{Dpp, Fields, NULL, Operand};
use crate::io::{Barycentric, Export, Input, Interp, Io, Target};
use crate::ir::{EXEC, Exit, Inst, Program, Reg, Value};
use crate::native::{HwStage, NativeProgram, ResourceType, RootType, StageIo, Usage, UsedResource};
use crate::resource::{Address, CODE, MEMORY, Resources, Role, Site, Source};
use crate::structure::{Atom, Bool, LaneExit, Structured};


/// The ray-tracing node test (`image_bvh_intersect_ray`), a child module so
/// it shares the emitter's state.
pub(super) mod intersect;

mod buffer;
mod compare;
mod control_flow;
mod descriptor_data;
mod descriptors;
mod dispatch;
mod driver;
mod export;
mod flat_memory;
mod format_load;
mod formats;
mod heap_index;
mod host_config;
mod image;
mod init;
mod inputs;
mod interp;
mod lanes;
mod lds;
mod lds_meetings;
mod mask_analysis;
mod math;
mod operands;
mod packed16;
mod reg_write;
mod scalar;
mod signature_pack;
mod smem;
#[cfg(test)]
mod tests;
mod thread_model;
mod title_memory;
mod valu;
mod valu16;
mod valu64;
mod valu_int;
mod wave_exchange;
mod wave_local;
mod wave_sections;

use formats::{Numeric, buffer_format, half_shift, inline_f16};
pub use host_config::{host_settings, set_host_max_wave, set_host_rdna};
pub(super) use host_config::{
    FIRST_HALF, host_max_wave, host_rdna, reads_mask_as_data, with_first_half_pairs,
    with_local_waves, with_local_waves_any,
};
#[cfg(test)]
pub(super) use host_config::{TEST_F16_NEAREST, TEST_HOST_WAVE, TEST_RDNA};
use host_config::{
    FIRST_HALF_PAIRS, LOCAL_ANY, LOCAL_WAVES, f16_nearest_on, lane_constant_masks, own_bits_on,
};
use interp::interp_mode;
pub(super) use lds_meetings::multi_wave_wide_safe;
use lds_meetings::lds_meetings;
pub(super) use mask_analysis::{
    DIVERGENT_MASK_EXCHANGE, LANE_MASK, MASK_ELSEWHERE, branches_around, needs_every_lane,
    take_needs,
};
use mask_analysis::{MaskData, all_lane_blocks, lane_uniform, need};
use signature_pack::{half_input_partners, half_outputs, input_packs, packed_half};
use thread_model::meeting_mode;
pub(super) use wave_local::wave_local;

type Why = &'static str;

/// How an interpolation evaluates an input channel.
#[derive(Debug, Clone, Copy)]
enum Eval {
    /// Loaded with this DXIL interpolation mode (1: constant, P0).
    Mode(u8),
    /// From the vertex values of a constant element: P0 when `p0`, plus
    /// i (P1 - P0), plus j (P2 - P0), weights as floats.
    Weights {
        p0: bool,
        i: Option<V>,
        j: Option<V>,
    },
}

#[derive(Debug, Clone, Copy)]
enum Val {
    I32(V),
    Bit(V),
    Missing(Why),
}

pub(crate) struct Lowered {
    pub dxil: Vec<u8>,
    /// Bit n: immediate sampler n is a comparison sampler.
    pub cmp_samplers: u64,
    pub root_cbvs: Vec<u8>,
    pub root_srvs: Vec<u8>,
    pub root_uavs: Vec<u8>,
    pub user_data: Vec<UserData>,
    pub samplers: Vec<[u32; 4]>,
    /// The wave size the shader requires of the host, if any.
    pub wave_size: Option<u32>,
    /// Heap handles made with a non-uniform index.
    #[cfg(test)]
    pub nonuniform_handles: u32,
    /// Whether the program uses wave operations.
    #[cfg(test)]
    pub wave_ops: bool,
}

pub(crate) struct Parts<'a> {
    pub native: &'a NativeProgram,
    pub p: &'a Program,
    pub st: &'a Structured,
    pub io: &'a Io,
    pub res: &'a Resources,
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    pub prims: Vec<SigElement>,
    /// Mesh: what the group id arrives multiplied by.
    pub mesh_stride: u32,
}

enum Stop {
    /// A phi whose incoming value has no DXIL form was read: redo with it missing.
    Retry(Value, Why),
    Fail(ShaderError),
}

pub(crate) fn lower(parts: &Parts) -> Result<Lowered, ShaderError> {
    lower_counted(parts, counted_in_threads(parts))
}

/// Whether the title dispatches the program by a count of threads whose last
/// group may be partial: the structure builder's programs of one dimension.
/// Their lanes past the count must not run (they count the lanes of a wave
/// and store). Not where a group barrier would wait for lanes that left, nor
/// where two host waves of 32 stand for a console wave (they exchange, which
/// takes all lanes).
fn counted_in_threads(parts: &Parts) -> bool {
    let crate::native::StageIo::Cs(cs) = &parts.native.io else {
        return false;
    };
    cs.threads[0] > 1
        && cs.threads[1..] == [1, 1]
        && crate::spill::is_spilled(parts.native)
        && (!parts.native.wave64 || host_max_wave() >= 64)
        && !parts.p.blocks.iter().flat_map(|b| &b.insts).any(|i| i.name() == "s_barrier")
}

/// [`lower`]; `counted`: the lanes past a dispatch's count of threads leave
/// at once (`shader.rs` `USER_DISPATCH_THREADS_X`; after DexNom/Oathkeep
/// bc3cd4b0, which does so for two of the programs by their digests).
pub(super) fn lower_counted(parts: &Parts, counted: bool) -> Result<Lowered, ShaderError> {
    let mut bad = HashMap::new();
    loop {
        match Lower::new(parts, &bad).run(counted) {
            Ok(l) => return Ok(l),
            Err(Stop::Retry(v, why)) => {
                if bad.insert(v, why).is_some() {
                    return Err(ShaderError::Unsupported {
                        offset: 0,
                        what: "a phi that does not settle",
                    });
                }
            }
            Err(Stop::Fail(e)) => return Err(e),
        }
    }
}

const TWO_PI: f32 = std::f32::consts::TAU;
/// `s_sendmsg` id of a mesh or NGG stage's output allocation request.
const GS_ALLOC_REQ: u16 = 9;
/// `s_sendmsg` id that ends a pixel wave's ordered section.
const ORDERED_PS_DONE: u16 = 7;
/// Hardware register (of `s_setreg_b32`) a pixel wave names its packer in
/// before it waits for the waves it overlaps.
const POPS_PACKER: u16 = 25;
/// The operand `src_pops_exiting_wave_id`.
const POPS_EXITING: u8 = 239;

/// The instructions inside a pixel program's ordered section
/// (primitive-ordered pixel shading, which the console compiler makes of
/// rasterizer ordered views): from the write of `POPS_PACKER`, along every
/// path, to the message that ends the section or the program's end. The
/// console waits there until the waves holding earlier primitives' samples
/// of the same pixels have left their sections; a host's rasterizer ordered
/// views wait by themselves, at their first access. Empty for a program
/// without such a section.
fn ordered_section(p: &Program) -> HashSet<Site> {
    let starts = |i: &Inst| {
        i.name() == "s_setreg_b32"
            && matches!(i.fields, Fields::Sop { imm: Some(imm), .. } if imm & 0x3F == POPS_PACKER)
    };
    let ends = |i: &Inst| {
        i.name() == "s_sendmsg"
            && matches!(i.fields, Fields::Sopp { imm } if imm & 0xF == ORDERED_PS_DONE)
    };
    let mut section = HashSet::new();
    let mut entered = vec![false; p.blocks.len()];
    let mut work: Vec<Site> = p
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(b, block)| {
            (0..block.insts.len())
                .filter(|&i| starts(&block.insts[i]))
                .map(move |i| (b, i))
        })
        .collect();
    while let Some((b, from)) = work.pop() {
        let block = &p.blocks[b];
        let mut done = false;
        for i in from..block.insts.len() {
            if ends(&block.insts[i]) {
                done = true;
                break;
            }
            section.insert((b, i));
        }
        if done {
            continue;
        }
        for (t, next) in p.blocks.iter().enumerate() {
            if next.preds.contains(&b) && !std::mem::replace(&mut entered[t], true) {
                work.push((t, 0));
            }
        }
    }
    section
}

struct Lower<'a> {
    s: Shader,
    x: &'a Parts<'a>,
    bad: &'a HashMap<Value, Why>,
    lane_fail: HashMap<Value, Why>,
    vals: HashMap<Value, Val>,
    floats: HashMap<Value, V>,
    /// DXIL block per IR block (unreachable: none).
    block: Vec<Option<usize>>,
    cur: usize,
    bools: HashMap<(usize, Bool), V>,
    lane_bits: HashMap<Value, V>,
    /// IR phi location by value.
    phi_at: HashMap<Value, (usize, usize)>,
    /// Descriptor dwords with no host value (addresses, formats, all of a constant
    /// buffer's): no program reads them as numbers ([`Self::dead`]).
    stale: HashSet<Value>,
    /// See [`ordered_section`] (pixel programs).
    ordered: HashSet<Site>,
    /// Raw UAV buffers (see [`Lower::entry`]) the program loads from past its
    /// wave's cache (GLC): other groups write what it waits for. On the host
    /// they are globally coherent; a load in a loop that only waits would
    /// be made once otherwise.
    coherent: HashSet<(usize, u32)>,
    /// In the instruction being lowered: the host copy its root view's bytes
    /// are in ([`Lower::root_memory`]).
    root_copy: Option<V>,
    /// Whether the page [`Lower::memory`] last looked for has a copy.
    page_there: Option<V>,
    mask_phis: HashMap<Value, V>,
    pending_masks: Vec<(V, usize, usize)>,
    data_phis: Vec<(V, usize, usize)>,
    read_phis: HashSet<Value>,
    params: HashMap<Value, (usize, u32)>,
    user_data: Option<V>,
    user_reads: BTreeMap<u32, UserData>,
    samplers: Vec<[u32; 4]>,
    descriptors: HashMap<(Site, bool), &'a Source>,
    scalar: HashMap<Site, Address>,
    exports: HashMap<Site, &'a Export>,
    interps: HashMap<Site, &'a Interp>,
    handles: HashMap<(usize, Site, u8, bool, (u32, u32)), V>,
    /// Element strides read from the host's table by heap slot
    /// ([`super::shader::SLOT_STRIDES`]), by block and descriptor load.
    strides: HashMap<(usize, Site, u8), V>,
    /// Blocks splitting the edge from one IR block to another.
    edges: HashMap<(usize, usize), usize>,
    /// Per IR block: a `done` export is behind every lane entering it.
    exported: Vec<bool>,
    /// Reverse-postorder index per IR block.
    rpo: Vec<usize>,
    /// DXIL block each IR block ends in (guarded stores split blocks).
    tail: Vec<Option<usize>>,
    /// The program writes UAVs: barriers fence them too.
    writes_uavs: bool,
    /// src0 of the current instruction as DPP fetched it.
    src0: Option<V>,
    /// Compute code that reads other lanes: thread ids follow the host
    /// lanes, so lane n is console lane n.
    console_lanes: bool,
    /// Mesh: the thread index in the group (made in the entry block).
    flat: Option<V>,
    /// Mesh: IR blocks whose `s_barrier` is left out.
    no_barrier: HashSet<usize>,
    /// IR blocks whose `s_barrier` not every lane is sure to reach (a lane
    /// branch lies between the entry and the block, or the program has a
    /// loop a lane can be cut out of): the barrier there keeps its fences
    /// and does not wait for the group ([`super::group_sync`]).
    fence_only: HashSet<usize>,
    /// A group barrier before every LDS access (a console wave run as two host waves).
    split_lds: bool,
    /// With `split_lds`, LDS accesses in lane branches: per IR block whether every lane runs
    /// it, and whether the host waves meet at its start and before its exit ([`lds_meetings`]).
    lds_meet: Option<(Vec<bool>, Vec<(bool, bool)>)>,
    /// A console wave64 program run as two host waves of 32 that exchange through LDS: the first
    /// of two exchange dwords, whether the group has no upper half (32 threads or fewer), and per
    /// IR block whether every lane runs it (exchanges need both halves there).
    exchange: Option<(u32, bool, Vec<bool>)>,
    /// Multi-wave (`group > 64`) widened execution: widened and guarded scalar
    /// branches synchronize across every host wave in the thread group via LDS
    /// so every wave reaches barriers in lockstep.
    group_wide: bool,
    /// With `exchange`: nothing is exchanged, each host wave acts as a wave
    /// of its own ([`with_local_waves`]).
    local: bool,
    /// The instruction being lowered may read a lane mask as a number under
    /// `local` (a bit count, a first set bit, `v_mbcnt`).
    local_ok: bool,
    /// With `local`: every mask read and lane read is taken per host wave
    /// ([`with_local_waves_any`]).
    local_any: bool,
    /// With `exchange`: VGPR values every lane holds alike ([`lane_uniform`]); a first-lane
    /// read of one needs nothing from the other host wave.
    uniform: HashSet<Value>,
    /// With `exchange`: the wave ORs that only steer a loop over each lane's own bits, which
    /// a host wave takes over its own lanes wherever they stand ([`super::own_bits`]).
    own_bits: super::own_bits::OwnBits,
    /// The console lane (0..63) of this thread, made in the entry block.
    clane: Option<V>,
    /// The first exchange dword of this thread's console wave ([`Self::exchange_pair`]).
    pair: Option<V>,
    /// A console wave64 program in host waves of 32: the LDS dword counting the console threads
    /// handed to host waves (`wave_base`), and the base made in the entry block.
    halves: Option<u32>,
    wave_base: Option<V>,
    /// Vertex and mesh outputs holding 16-bit values: row, whether two per
    /// dword, and the row packed after it.
    half_outputs: HashMap<u8, (bool, Option<u8>)>,
    /// How lane masks become data.
    mask_data: MaskData,
    /// The instruction being lowered may read lane masks as data.
    mask_ok: bool,
    /// Ballots of masks read as data, by low dword: (low, high).
    ballots: HashMap<Value, (V, V)>,
    /// Masks whose ballot is being built: a mask whose lane bit needs its
    /// own value as a number (an opaque mask keyed by a dword it shares
    /// with another) would otherwise recurse until the stack overflows.
    balloting: HashSet<Value>,
    /// Block writing each value.
    def_block: HashMap<Value, usize>,
    /// IR block being lowered.
    cur_ir: usize,
    /// Whole-wave sections by block and start index.
    sections: HashMap<usize, HashMap<usize, super::wave::Section>>,
    /// Values written in a section that code outside it reads.
    section_out: HashSet<Value>,
    /// The program's constant table (the code from the first byte read
    /// through `s_getpc_b64` on): global, first byte, dwords.
    code_table: Option<(V, u32, u32)>,
    /// Loop headers' iteration counters (see [`LOOP_LIMIT`]): the phi by
    /// IR block, and its incoming (value, DXIL block) pairs.
    counters: HashMap<usize, (V, Vec<(V, usize)>)>,
    /// The block a loop stopped by [`LOOP_LIMIT`] leaves the program from.
    abort: Option<usize>,
}

/// Iterations after which a loop is left (per entry into it), unless
/// [`super::loop_limit`] says otherwise. The console runs a loop as long as
/// its data says; data the host has not mirrored yet (a null view reads
/// zeros) can make that four billion iterations, which hangs the host GPU
/// until the driver resets it. A lane that reaches the limit leaves the
/// program as killed (a pixel discards).
// shortcut: a genuine loop of more iterations than this is cut short too.
pub(super) const LOOP_LIMIT: u32 = 1 << 20;


fn unsupported(offset: usize, what: Why) -> Stop {
    Stop::Fail(ShaderError::Unsupported { offset, what })
}


