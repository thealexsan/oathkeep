
use crate::ir::Value;

use super::copy_tracking::Moved;

pub type Site = (usize, usize);

/// A byte address: a root parameter's, plus a constant and at most one
/// value the program computes (an index times a stride, say).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    /// Index into the layout, or [`CODE`].
    pub param: usize,
    /// Added modulo 2^32 (with carry into the high half of a root
    /// descriptor's address).
    pub offset: u32,
    pub dynamic: Option<Value>,
}

/// [`Address::param`] of the program's own code: a constant table the
/// compiler appends after the code and reads through `s_getpc_b64`; the
/// offset is then the byte offset in the code.
pub const CODE: usize = usize::MAX;

/// [`Address::param`] of title memory at an address no root parameter
/// gives: one the code computes from root constants, reads out of a
/// descriptor or loads from other memory (compute programs only: the
/// ray-tracing structure builder and the programs that walk its
/// structures). Offset and computed term are not used: lowering takes the
/// 64-bit address from the instruction's operands.
pub const MEMORY: usize = usize::MAX - 1;

/// The reasons an address traces to no root parameter for which the
/// address is then taken as it stands ([`MEMORY`]).
pub(super) const RAW: [&str; 5] = [
    "an address not from a root parameter",
    "an address from a root parameter that is no address",
    "an address in an SGPR that is no root parameter",
    "an address loaded from memory",
    "an address from a phi",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Dwords `first..` of what the scalar load at `load` reads (see
    /// [`Resources::scalar`] for its address).
    Loaded { load: Site, first: u8 },
    /// A buffer descriptor built around an address: `bits` OR-ed into
    /// dword 1 (stride, swizzle), dwords 2 and 3 as given.
    Built {
        address: Address,
        bits: u32,
        words: [u32; 2],
    },
    /// Fixed in the code (an immediate sampler, say).
    Constant(Vec<u32>),
    /// Chosen by control flow: one source per predecessor of `block`, in
    /// [`crate::ir::Block::preds`] order.
    Phi { block: usize, args: Vec<Source> },
    /// Chosen by SCC (`scc`, the value an `s_cselect` or `s_cmov` of each
    /// dword reads): the source when it is set, then when it is clear.
    Select { scc: Value, args: Vec<Source> },
    /// Dwords `first..` of what the vector load at `load` (`global_load_*`)
    /// reads, each lane at its own address (see [`Resources::scalar`]); the
    /// dwords past that load come from loads of the bytes that follow. A
    /// descriptor that differs between lanes.
    Fetched { load: Site, first: u8 },
    /// Picked per lane by `v_cndmask_b32` reading the lane mask whose low
    /// dword is `mask`: the source where the lane's bit is set, then where it
    /// is clear.
    Lanes { mask: Value, args: Vec<Source> },
    /// Per-lane dwords chosen by control flow (VGPRs joined where lanes
    /// that took different paths meet): one source per predecessor of
    /// `block`, in [`crate::ir::Block::preds`] order, `None` on an edge no
    /// lane that accesses the descriptor takes. A descriptor that differs
    /// between lanes.
    LanePhi {
        block: usize,
        args: Vec<Option<Source>>,
    },
}

impl Source {
    /// The sources this one picks from (itself unless a phi).
    pub fn leaves(&self) -> Vec<&Source> {
        match self {
            Source::Phi { args, .. } | Source::Select { args, .. } | Source::Lanes { args, .. } => {
                args.iter().flat_map(Source::leaves).collect()
            }
            Source::LanePhi { args, .. } => {
                args.iter().flatten().flat_map(Source::leaves).collect()
            }
            s => vec![s],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Image, buffer or `s_buffer_load` resource.
    Resource,
    Sampler,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub site: Site,
    pub role: Role,
    pub source: Source,
}

/// A view-heap descriptor whose byte address can be evaluated when binding
/// the draw, using a table base and a root constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootIndexedView {
    pub table_slot: u8,
    pub constant_slot: u8,
    pub constant_dword: u32,
    pub byte_offset: u32,
    pub shift: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    /// Scalar values saved to a constant VGPR lane and restored within one
    /// block, even if that VGPR is also used in ordinary vector operations.
    pub(crate) restored_lanes: std::collections::HashMap<Site, Moved>,
    /// Every scalar memory access by address (`s_load_*`, `s_store_*`), and
    /// every global memory access (`global_*`, the dynamic term per lane).
    pub scalar: Vec<(Site, Address)>,
    pub descriptors: Vec<Descriptor>,
    /// Direct descriptor sources with a draw-evaluable index.
    pub root_indexed_views: Vec<RootIndexedView>,
    /// User SGPRs holding root parameters: value on entry, layout index,
    /// dword within the parameter.
    pub params: Vec<(Value, usize, u32)>,
    /// Scalar loads of one descriptor the code keeps per lane in VGPRs
    /// ([`Tracer::lane_copies`]): they read one address, so their dwords
    /// stand in for each other.
    pub lane_copied: Vec<Site>,
    pub waterfall: Waterfall,
    /// Every `image_bvh_intersect_ray` and `image_bvh64_intersect_ray`: where
    /// its nodes are.
    pub bvh: Vec<(Site, BvhNodes)>,
}

/// Where a ray-tracing node test ([`crate::bvh`]) reads its nodes. Its
/// descriptor operand names a range of memory, not a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BvhNodes {
    /// A buffer of the host: the descriptor is loaded from a table (it is
    /// among [`Resources::descriptors`]), and node pointers count from the
    /// buffer's start. Its flags are not known to the shader: boxes are
    /// sorted and triangles return barycentrics, as every descriptor
    /// the title builds asks.
    Resource,
    /// Title memory by address: the code makes the descriptor from numbers
    /// (the title: the whole address space, or one structure's address in
    /// dword 0). Dwords 1 and 3, which hold its flags.
    Memory { word1: u32, word3: u32 },
}

/// Waterfall loops over descriptors that differ between lanes: each pass
/// reads the first active lane's descriptor with `v_readfirstlane_b32`,
/// compares it with every lane's own (`v_cmp_eq`), and runs the access with
/// the lanes that match. Their accesses take each lane's own descriptor
/// ([`Source::Fetched`], [`Source::Lanes`]), so the first lane's dwords are
/// each lane's own too: the compares hold in every lane and one pass serves
/// all lanes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Waterfall {
    /// The compares, true in every lane.
    pub compares: Vec<Site>,
    /// The `v_readfirstlane_b32` results they compare, each the lane's own
    /// source dword.
    pub reads: Vec<Value>,
    /// Descriptor loads at a table offset among `reads` (the console
    /// compiler's form): each lane loads its own entry.
    pub loads: Vec<Site>,
}
