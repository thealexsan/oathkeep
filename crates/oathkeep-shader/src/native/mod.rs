//! Native GFX10 programs that the console compiler stores in a `DXIL` part.
//!
//! An `XDAT` index after the program header lists blobs stored around the
//! bitcode; one or more of them hold native programs, zlib-packed (a vertex
//! shader can carry two: compiled for use with and without a geometry
//! shader). Layout as observed in shipped titles (index offsets are relative
//! to the part data):
//!
//! ```text
//! +0x18 "XDAT"  u32 entry count, u32 (0x24 seen), then per entry:
//!               u32 kind, u32 offset, u32 size
//! kind 0x0d     "XBCH" u32 version (3), u64 packed size, u64 unpacked size,
//!               zlib stream
//! unpacked      "FSSM" u32 version (0x101), u64 payload size, payload
//! payload +0x00 u64 graphics slot count (6)
//!         +0x08 u64 record offsets: LS HS ES GS VS PS (0x08..0x30),
//!               mesh 0x48, compute 0x60; unused ones are 0xDEADBEEF_DEADBEEF
//! record  +0x00 u64 code offset, +0x08 u64 code size (payload-relative),
//!         +0x14 u32 flags: bit 5 wave32 (else wave64)
//!         +0x28 u64 root layout table, +0x30 u64 used-resource table
//!               (payload-relative), +0x38 u32 and +0x3c u32 their counts
//!         +0x50 u32 bits 0-9 VGPRs used, bits 20-27 SGPRs used
//! layout  20 B: u32 root type, u32 table offset (dwords), u32 user SGPRs,
//!               u32 base register, u8 first user-data dword, u8 root slot,
//!               u16 0
//! used    20 B: u8 resource type, u8 root type, u16 0, u32 table offset
//!               (dwords), u32 lower bound, u32 stride, u8 descriptor
//!               dwords, u8 first user-data dword, u8 root slot, u8 0
//! ```
//!
//! Stage data follows the record; offsets from its end (E = record + 0x54):
//!
//! ```text
//! PS  E+0x24  32 inputs of 8 B: u8 usage, u8 signature row, u8 channel
//!             mask, u8 attribute (v_interp `attr`), u8 default, u8 second
//!             row (of a packed pair), u8 parameter slot, u8 flags (bit 0
//!             flat, 1 linear, 3 fp16, 4 fp16 packed); E+0x124 u32 count
//!     E+0x134 u32 SPI_PS_INPUT_ADDR, E+0x138 u32 DB_SHADER_CONTROL
//!     E+0x148 u32 export patch size (dwords), E+0x14c 8 patches, one per
//!             MRT: u32 code offset (dwords, -1 none), then the export code
//!             for each color format in 10 slots of 10 dwords (ZERO, 32_R,
//!             32_GR, 32_AR, FP16, UNORM16, SNORM16, UINT16, SINT16, 32_ABGR)
//! VS  E+0x24  32 inputs of 5 B: u8 usage, u8 row, u8 channel mask, u8 VGPR
//!             of channel 0 (channel k in VGPR + k), u8 highest channel;
//!             E+0xc4 u32 count
//!     E+0xc8  32 outputs of 6 B: u8 usage, u8 row, u8 parameter (`exp
//!             param<n>`), u8 row again, u8 slot, u8 flags (bit 0 fp16,
//!             bit 1 two fp16 values per dword); E+0x188 u32 count
//! CS  E+0x24  u32 COMPUTE_PGM_RSRC2, E+0x28 u32 x3 thread-group size
//! MS  E+0xc8  outputs as VS, E+0x188 count; the param byte's bit 7 marks a
//!             per-primitive output (`PSG1` row); the 3 bytes more are the
//!             second row of a packed pair, a slot, flags (bit 0 fp16, bit 1
//!             two fp16 elements packed)
//!     E+0x18c u32 SPI_VS_OUT_CONFIG, SPI_SHADER_POS_FORMAT,
//!             PA_CL_VS_OUT_CNTL, VGT_STRMOUT_CONFIG, SPI_SHADER_PGM_RSRC2_GS,
//!             VGT_GS_OUT_PRIM_TYPE, VGT_GS_INSTANCE_CNT, SPI_SHADER_IDX_FORMAT
//!     E+0x448 u16 thread-group size in lanes, u16 maximum vertices,
//!             u16 maximum primitives, u8 user SGPRs, u8 output topology
//!             (2 triangles); code follows at E+0x45c
//! record +0x4c u32 LDS bytes (`lds_byte_size`; mesh programs read it here)
//! ```
//!
//! Rows are registers of the container's signature parts (`ISG1`, `OSG1`).
//! Merged stages keep their stage data in another layout (not read).
//!
//! Other kinds seen: 0x07 program header + bitcode (or a stub when stripped),
//! 0x0a `xbop` (compiler options), 0x0e a table not decoded yet, 0x10 and
//! 0x12 16-byte blobs (0x10 is the shader hash).
//! Unknown versions are rejected so a format change shows up as an error.


mod format;
mod parse;
#[cfg(test)]
mod tests;

pub use parse::{fspx_program, native_programs};

/// Hardware shader stage a native program runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HwStage {
    Ls,
    Hs,
    Es,
    Gs,
    Vs,
    Ps,
    /// Mesh shader.
    Ms,
    Cs,
}

impl HwStage {
    /// The SGPR holding user-data dword `index`: merged stages (and mesh
    /// shaders, which run as one) have 8 system SGPRs first.
    pub fn user_sgpr(self, index: u8) -> u8 {
        match self {
            HwStage::Ls | HwStage::Hs | HwStage::Es | HwStage::Gs | HwStage::Ms => index + 8,
            HwStage::Vs | HwStage::Ps | HwStage::Cs => index,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProgram {
    pub stage: HwStage,
    /// Runs as 64 lanes per wave (else 32); sizes lane masks.
    pub wave64: bool,
    /// VGPRs the program uses (v0 up to this), the bound of indexed access.
    pub vgprs: u16,
    /// Where the root signature's parameters arrive (user SGPRs).
    pub layout: Vec<RootParam>,
    /// The descriptors the program reads, by root parameter.
    pub used: Vec<UsedResource>,
    pub io: StageIo,
    /// GFX10 machine code, as stored.
    pub code: Vec<u8>,
}

impl NativeProgram {
    /// The code with each render target's export patched for its full
    /// 32-bit format, so exports carry the shader's values unconverted (as
    /// the console does at pipeline creation for that format).
    pub fn patched_code(&self) -> Vec<u8> {
        let mut code = self.code.clone();
        if let StageIo::Ps { patches, .. } = &self.io {
            for p in patches {
                let at = p.offset as usize * 4;
                for (i, word) in p.code.iter().enumerate() {
                    code[at + 4 * i..at + 4 * i + 4].copy_from_slice(&word.to_le_bytes());
                }
            }
        }
        code
    }
}

/// What a semantic is, as the stage data names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Usage {
    Generic,
    ClipDistance,
    Other(u8),
}

impl Usage {
    fn from_raw(raw: u8) -> Self {
        match raw {
            6 => Usage::Generic,
            7 => Usage::ClipDistance,
            n => Usage::Other(n),
        }
    }
}

/// A pixel shader input: what `v_interp` of attribute `attr` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsInput {
    pub usage: Usage,
    /// Register in the input signature.
    pub row: u8,
    pub mask: u8,
    pub attr: u8,
    pub flat: bool,
    pub fp16: bool,
    /// Two 16-bit values per dword ([`VsOutput::pack2`]); `row2` is the
    /// second element's register.
    pub pack2: bool,
    pub row2: u8,
}

/// A vertex attribute the fetch shader loads: channel k into VGPR `vgpr + k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsInput {
    pub usage: Usage,
    pub row: u8,
    pub mask: u8,
    pub vgpr: u8,
}

/// What `exp param<param>` of a vertex shader writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsOutput {
    pub usage: Usage,
    /// Register in the output signature.
    pub row: u8,
    pub param: u8,
    /// 16-bit values, one per dword (low half) unless `pack2`: then two per
    /// dword, channel c of this row in dword c / 2 (odd channels in the
    /// high half), then a second 16-bit row from the next whole dword (the
    /// next 16-bit row of the signature no entry names).
    pub fp16: bool,
    pub pack2: bool,
}

/// The export code of one render target for its full 32-bit format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPatch {
    pub mrt: u8,
    /// Where it goes in the code, in dwords.
    pub offset: u32,
    pub code: Vec<u32>,
}

/// Inputs and outputs, for the stages whose stage data is read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StageIo {
    #[default]
    None,
    Vs {
        inputs: Vec<VsInput>,
        outputs: Vec<VsOutput>,
    },
    Ps {
        inputs: Vec<PsInput>,
        /// `SPI_PS_INPUT_ADDR`: which system values arrive in VGPRs.
        input_addr: u32,
        /// `DB_SHADER_CONTROL`: depth export, kill.
        db_shader_control: u32,
        patches: Vec<ExportPatch>,
    },
    Cs(ComputeIo),
    Ms(MeshIo),
}

/// A mesh program's outputs and limits. It runs as one hardware NGG stage:
/// lane n of the group (`threads` lanes) exports vertex n (`exp pos`,
/// `exp param`) and primitive n (`exp prim`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshIo {
    /// `exp param<n>` targets; `param` bit 7 set: a per-primitive output,
    /// `row` then a row of the primitive signature (`PSG1`).
    pub outputs: Vec<VsOutput>,
    /// `PA_CL_VS_OUT_CNTL`: which clip and cull distances `pos1+` carry.
    pub clip_cntl: u32,
    /// Thread-group size in lanes (the HLSL group rounded up to waves).
    pub threads: u32,
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// 1 lines, 2 triangles.
    pub topology: u8,
    pub lds_bytes: u32,
}

/// A compute program's dispatch settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputeIo {
    /// `COMPUTE_PGM_RSRC2`: user SGPRs, which system SGPRs and VGPRs
    /// follow them, LDS size.
    pub rsrc2: u32,
    /// Thread-group size (`numthreads`).
    pub threads: [u32; 3],
}

impl ComputeIo {
    /// User SGPRs before the system ones.
    pub fn user_sgprs(&self) -> u8 {
        (self.rsrc2 >> 1 & 0x1F) as u8
    }

    /// The SGPR of each enabled thread-group id component, in order after
    /// the user SGPRs, then the thread-group info SGPR if enabled.
    pub fn system_sgprs(&self) -> ([Option<u8>; 3], Option<u8>) {
        let mut next = self.user_sgprs();
        let mut take = |bit: u32| {
            (self.rsrc2 >> bit & 1 != 0).then(|| {
                next += 1;
                next - 1
            })
        };
        let ids = [take(7), take(8), take(9)];
        (ids, take(10))
    }

    /// SGPRs the hardware initializes: the user SGPRs, the enabled system
    /// ones, then the scratch wave offset when scratch is enabled (bit 0).
    /// The ones above hold whatever the previous wave left.
    pub fn initialized_sgprs(&self) -> u8 {
        let (ids, info) = self.system_sgprs();
        let end = ids.into_iter().chain([info]).flatten().map(|s| s + 1).max();
        end.unwrap_or(0).max(self.user_sgprs()) + (self.rsrc2 & 1) as u8
    }

    /// Thread id components in v0, v1, v2 (1 to 3).
    pub fn thread_id_vgprs(&self) -> u8 {
        (self.rsrc2 >> 11 & 3) as u8 + 1
    }

    /// LDS bytes per thread group (512-byte granules).
    pub fn lds_bytes(&self) -> u32 {
        (self.rsrc2 >> 15 & 0x1FF) * 512
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RootType {
    Constants,
    /// A 64-bit buffer address.
    Descriptor,
    /// The 32-bit heap address of a descriptor table range.
    Table,
    /// A sampler the root signature fixes.
    ImmediateSampler,
    /// The 32-bit address of the CBV/SRV/UAV heap (SM 6.6 dynamic resources).
    ResourceHeap,
    /// The 32-bit address of the sampler heap.
    SamplerHeap,
    Other(u32),
}

impl RootType {
    fn from_raw(raw: u32) -> Self {
        match raw {
            0 => RootType::Constants,
            1 => RootType::Descriptor,
            3 => RootType::Table,
            4 => RootType::ImmediateSampler,
            5 => RootType::ResourceHeap,
            6 => RootType::SamplerHeap,
            n => RootType::Other(n),
        }
    }
}

/// One root parameter (or table range) and the user SGPRs holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootParam {
    pub root: RootType,
    /// Of the range within its table.
    pub table_offset_dwords: u32,
    /// User SGPRs from `user_data` on.
    pub sgprs: u32,
    /// Shader register of the first descriptor (`b`/`t`/`u`/`s` number).
    pub base_register: u32,
    /// First user-data dword ([`HwStage::user_sgpr`]).
    pub user_data: u8,
    /// Index of the parameter in the root signature.
    pub slot: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceType {
    Srv,
    Sampler,
    Cbv,
    Uav,
    Other(u8),
}

/// A descriptor range the program reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsedResource {
    pub kind: ResourceType,
    pub root: RootType,
    pub table_offset_dwords: u32,
    /// Shader register of the first descriptor read.
    pub lower_bound: u32,
    pub stride: u32,
    /// Size of the descriptor (4 or 8).
    pub dwords: u8,
    pub user_data: u8,
    pub slot: u8,
}

