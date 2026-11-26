//! A DXIL vertex, pixel, compute or mesh shader under construction: the module,
//! the `dx.op` declarations it calls, its signatures and bound resources,
//! and the metadata and container that wrap it up (SM 6.6, validator 1.6).

use super::bitcode::{Module, Ty, V};
use super::container::SigElement;

/// Where the shader reads its user data: `b0` in this space.
pub const USER_DATA_SPACE: u32 = 0;
/// Where immediate samplers are bound: `s<n>` in this space.
pub const SAMPLER_SPACE: u32 = 0;
/// Where root CBVs and SRVs are bound: `b<root parameter index>` and
/// `t<root parameter index>` in this space.
pub const ROOT_CBV_SPACE: u32 = 1;

/// The root SRV `t<SLOT_STRIDES>` in [`ROOT_CBV_SPACE`] of a shader that
/// reads buffers through descriptors it picks from the view heap at an
/// index it computes (SM 6.6 dynamic resources): a raw buffer of
/// [`SLOT_STRIDES_COUNT`] dwords the host keeps, dword `i` the bytes per
/// element of the buffer view at heap slot `i` (0: none or not a buffer).
/// The console's descriptor holds that stride and its hardware multiplies
/// the element index by it; the program's code does not say it, and the
/// used-resource list only names what tables hold. A shader that has this
/// root SRV also tells the host that the buffer views of the whole heap
/// must be ready when it runs, not only those its tables name.
pub const SLOT_STRIDES: u8 = 255;
/// The same table of strides and format kinds as [`SLOT_STRIDES`], bound for
/// shaders that only inspect views from descriptor tables (`RootType::Table`).
/// Kept distinct from [`SLOT_STRIDES`] so the host binds the table without
/// running the full bindless heap buffer sweep.
pub const SLOT_TABLE_STRIDES: u8 = 252;
/// The bits of a [`SLOT_STRIDES`] dword that hold the stride.
pub const VIEW_STRIDE_BITS: u32 = (1 << 30) - 1;
/// In a [`SLOT_STRIDES`] dword: a typed buffer view of an integer format
/// (`UINT` or `SINT`), whose 16-bit typed accesses (`_d16`) pass bits.
/// (After Oathkeep 4243a96a.)
pub const VIEW_INTEGER: u32 = 1 << 31;
/// With [`VIEW_INTEGER`]: a signed (`SINT`) format.
pub const VIEW_SIGNED: u32 = 1 << 30;
/// Dwords of the [`SLOT_STRIDES`] buffer: heap indices are clamped to it.
pub const SLOT_STRIDES_COUNT: u32 = 1 << 21;

/// Title memory by address (`resource::MEMORY`): console code reads and
/// writes through raw 40-bit addresses it takes from root constants,
/// descriptors and other memory (the ray-tracing structure builder and the
/// programs that walk its structures). The host keeps copies of the title
/// memory shaders use; a shader with such an access finds the copy by the
/// address:
///
/// - the root SRV `t<SLOT_MEMORY>` in [`ROOT_CBV_SPACE`] is a directory of
///   [`MEMORY_PAGES`] entries of two dwords, one per 64 KiB of title
///   address space: the number of the host copy holding that page (0: none
///   yet; with [`MEMORY_READ_ONLY`]: stores there are dropped) and the page's
///   byte offset in that copy;
/// - copy `n` is the raw buffer view at view-heap slot `base + n`, `base`
///   the user-data dword of kind [`USER_MEMORY_BASE`]; slot `base` itself
///   reads zeros and drops writes;
/// - the root UAV `u<SLOT_FAULTS>` takes the page numbers of accesses that
///   found no copy ([`MEMORY_FAULTS`] dwords, dword 0 a sink), which the
///   host reads afterwards to make those copies for the frames to come.
pub const SLOT_MEMORY: u8 = 254;
/// See [`SLOT_MEMORY`].
pub const SLOT_FAULTS: u8 = 253;
/// Entries of the [`SLOT_MEMORY`] directory: title addresses below 128 GiB.
pub const MEMORY_PAGES: u32 = 1 << 21;
/// In a directory entry's first dword: shaders do not store there by address
/// (a title buffer the structure builder has not been given to write).
pub const MEMORY_READ_ONLY: u32 = 1 << 31;
/// Dwords of the [`SLOT_FAULTS`] buffer.
pub const MEMORY_FAULTS: u32 = 4096;
/// `RootType::Other` code of a user-data dword the host computes: the view
/// heap slot of the memory copies' views ([`SLOT_MEMORY`]).
pub const USER_MEMORY_BASE: u32 = 0x100;
/// `RootType::Other` code of a user-data dword the host computes: the low
/// 32 bits of the title address of the buffer view at dword `dword` of the
/// descriptor table of root parameter `slot`; [`USER_VIEW_ADDRESS`] + 1:
/// its bits from 32 up. (Console code reads the address out of a buffer
/// descriptor and uses it as a pointer.)
pub const USER_VIEW_ADDRESS: u32 = 0x101;
/// `RootType::Other` code of a user-data dword the host computes: the
/// bytes (0 to 15) by which the raw view the host binds for the root view
/// of root parameter `slot` begins before the title's address. A raw view
/// begins on 16 bytes of its host buffer; the console's root views begin
/// anywhere (the structure builder's counters sit at addresses ending in
/// 4 and c). The shader adds the dword to every byte offset through that
/// view. Asked for every root UAV.
pub const USER_VIEW_BIAS: u32 = 0x103;
/// `RootType::Other` code of a user-data dword the host computes: the low
/// 32 bits of the title address in the root view of root parameter `slot`;
/// [`USER_ROOT_ADDRESS`] + 1: its bits from 32 up. Programs with a spilled
/// user-data table (the ray-tracing structure builder's) reach what their
/// root views name as title memory by address ([`SLOT_MEMORY`]): a root view
/// has no size, and the host's copy of what it names may end in the middle of
/// it (a structure of megabytes crosses the 4 MiB windows).
pub const USER_ROOT_ADDRESS: u32 = 0x104;
/// `RootType::Other` code of a user-data dword the host sets at a dispatch
/// counted in threads (the console's `DispatchX`): how many of the threads
/// numbered along X run (0: all of every group). The metadata's `dword` is
/// the group's width. Asked for by the structure builder's programs
/// ([`crate::spill::is_spilled`]).
pub const USER_DISPATCH_THREADS_X: u32 = 0x106;

/// `ShaderFlags` bits (metadata) and their `SFI0` feature bits.
pub(crate) mod flags {
    pub const RAW_BUFFERS: u64 = 1 << 4;
    pub const DOUBLES: u64 = 1 << 2;
    pub const STENCIL_REF: u64 = 1 << 11;
    pub const TYPED_UAV_LOADS: u64 = 1 << 13;
    /// Unordered views in a vertex, geometry or mesh program.
    pub const UAVS_EVERY_STAGE: u64 = 1 << 16;
    pub const ROVS: u64 = 1 << 18;
    pub const WAVE_OPS: u64 = 1 << 19;
    pub const BARYCENTRICS: u64 = 1 << 22;
    pub const INT64: u64 = 1 << 20;
    pub const ATOMIC_TYPED64: u64 = 1 << 27;
    pub const ATOMIC_HEAP64: u64 = 1 << 32;
    pub const RESOURCE_HEAP: u64 = 1 << 30;
    pub const SAMPLER_HEAP: u64 = 1 << 31;
}

pub(crate) struct Types {
    pub void: Ty,
    pub i1: Ty,
    pub i32: Ty,
    pub f32: Ty,
    pub handle: Ty,
    pub props: Ty,
    pub bind: Ty,
    pub cbuf_ret: Ty,
    pub res_ret: Ty,
}

/// What a mesh shader declares beyond its thread-group size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mesh {
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// 1 lines, 2 triangles.
    pub topology: u8,
}

/// What a geometry shader declares: triangles in, triangle strips out on
/// stream 0, one instance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub max_vertices: u32,
}

/// The null slot every host view heap keeps at its end (0: no limit). Shaders translated while
/// it is set read that slot for any view index at or past it. A read past a heap's end is
/// undefined: a host that caps its heap (NVIDIA at 1 000 000 views; a retail game's heap is
/// larger) hung its GPU (GPU-based validation: "descriptor heap index out of bounds"), and AMD
/// RDNA3 took a page fault and removed the device.
pub(super) static VIEW_LIMIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// See [`VIEW_LIMIT`]: `null_slot` is the last slot of the host's view heap, kept null.
pub fn set_view_heap_limit(null_slot: u32) {
    VIEW_LIMIT.store(null_slot, std::sync::atomic::Ordering::Relaxed);
}

/// See [`VIEW_LIMIT`]: the null slot shaders translated now clamp view indices to (0: none).
pub fn view_heap_limit() -> u32 {
    VIEW_LIMIT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where the host's raw twins of its buffer views start in its view heaps (0: there are none).
///
/// A buffer a shader picks from the heap by an index it computes is read as raw bytes whatever
/// view the title has there, and the index comes from data: a record of a mesh long gone, a list
/// entry nothing wrote. On AMD a raw load through a view that is not raw (typed, structured, a
/// constant buffer's) has no end: it reads whatever lies there, up to 4 GiB on, and loses the
/// device where nothing does; and a texture's descriptor read as a buffer's is a wild address.
/// So the host keeps, for slot `i`, a raw view of the same bytes at this place + `i` (a constant
/// buffer view of a constant buffer in a title slot, a null raw view of everything else), and
/// shaders translated while this is set read picked buffers there, and tables' buffers that they
/// read as raw bytes.
/// A power of two above every slot index.
static RAW_VIEWS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
thread_local! {
    /// For a test's thread alone.
    pub(crate) static RAW_VIEWS_HERE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// See [`RAW_VIEWS`].
pub fn set_raw_views(first: u32) {
    debug_assert!(first == 0 || first.is_power_of_two());
    RAW_VIEWS.store(first, std::sync::atomic::Ordering::Relaxed);
}

/// See [`RAW_VIEWS`]: where shaders translated now read the buffers they pick (0: in place).
pub fn raw_views() -> u32 {
    #[cfg(test)]
    if let Some(first) = RAW_VIEWS_HERE.get() {
        return first;
    }
    RAW_VIEWS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) struct Shader {
    pub m: Module,
    pub t: Types,
    pub pixel: bool,
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    /// A mesh shader's per-primitive outputs.
    pub prims: Vec<SigElement>,
    /// A mesh shader (`threads` is its thread-group size).
    pub mesh: Option<Mesh>,
    /// A geometry shader (the rectangle stage, `rect.rs`); `pixel` is false.
    pub geometry: Option<Geometry>,
    /// Dwords of user data the shader reads (0: no user-data buffer).
    pub user_data: u32,
    /// Immediate samplers, bound as `s0..` in [`SAMPLER_SPACE`].
    pub samplers: u32,
    /// Bit n: immediate sampler n is a comparison sampler.
    pub cmp_samplers: u64,
    /// Root CBVs read, by root parameter index, with their handles.
    pub root_cbvs: Vec<(u8, V)>,
    /// Root SRVs (raw buffers) read, likewise.
    pub root_srvs: Vec<(u8, V)>,
    /// Root UAVs (raw buffers) read or written, likewise.
    pub root_uavs: Vec<(u8, V)>,
    pub flags: u64,
    /// Heap handles made with a non-uniform index.
    pub nonuniform_handles: u32,
    /// A compute shader with this thread-group size.
    pub threads: Option<[u32; 3]>,
    /// The wave size a compute shader requires (`WaveSize`).
    pub wave_size: Option<u32>,
    /// Thread-group shared memory, in dwords.
    pub lds_dwords: u32,
    pub(super) lds: Option<V>,
    pub(super) main: V,
    pub entry: usize,
}


mod finish;
mod handles;
