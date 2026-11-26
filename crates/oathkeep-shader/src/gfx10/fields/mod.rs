//! Operand fields of GFX10 instructions, by encoding.
//!
//! Layouts follow AMD's RDNA ISA reference. Register ranges carry their size
//! in dwords, which depends on the opcode (read from its name) and, for lane
//! masks (VCC, EXEC, compare results), on the wave size.

/// A source or destination of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// Scalar registers by operand code: 0-105 `s0`-`s105`, 106/107
    /// `vcc_lo`/`vcc_hi`, 108-123 `ttmp0`-`ttmp15`, 124 `m0`, 125 `null`,
    /// 126/127 `exec_lo`/`exec_hi`. The decoder gives no code above 127; the
    /// IR names scalars parked in VGPR lanes by the codes from 128 up
    /// (`crate::ir::PARKED`).
    Scalar {
        code: u8,
        dwords: u8,
    },
    Vector {
        reg: u8,
        dwords: u8,
    },
    /// Inline constant or read-only special source, by operand code
    /// (128-192: 0-64, 193-208: -1 to -16, 240-248: floats, 251-254: vccz,
    /// execz, scc, lds_direct).
    Const(u8),
    /// The 32-bit literal that follows the instruction.
    Literal(u32),
    /// Interpolation attribute and channel (0-3 = x-w); `high` selects the
    /// upper f16 half.
    Attr {
        index: u8,
        chan: u8,
        high: bool,
    },
}

/// Operand code of `null` (no register; reads as 0).
pub const NULL: u8 = 125;
pub(super) const VCC: u8 = 106;
pub(super) const EXEC: u8 = 126;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cache {
    pub glc: bool,
    pub slc: bool,
    pub dlc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fields {
    /// SOP1, SOP2, SOPC, SOPK; `imm` is SOPK's 16-bit immediate.
    Sop {
        dst: Option<Operand>,
        src: Vec<Operand>,
        imm: Option<u16>,
    },
    /// The 16-bit immediate: branch offset in dwords, wait counts, message...
    Sopp {
        imm: u16,
    },
    Smem {
        data: Option<Operand>,
        base: Operand,
        /// Signed byte offset, added to `soffset`.
        offset: i32,
        soffset: Operand,
        cache: Cache,
    },
    Valu(Valu),
    Vintrp {
        dst: Operand,
        /// The i/j VGPR, or for `v_interp_mov_f32` the parameter (0 `p10`,
        /// 1 `p20`, 2 `p0`) as [`Operand::Const`].
        src: Operand,
        attr: Operand,
    },
    Ds {
        dst: Option<Operand>,
        addr: Option<Operand>,
        data: Vec<Operand>,
        offset0: u8,
        offset1: u8,
        gds: bool,
    },
    /// MUBUF, and MTBUF with `format` set.
    Buffer {
        /// Loaded, stored, or the atomic operand (and result with `glc`).
        data: Option<Operand>,
        addr: Operand,
        rsrc: Operand,
        soffset: Operand,
        offset: u16,
        offen: bool,
        idxen: bool,
        format: Option<u8>,
        lds: bool,
        tfe: bool,
        cache: Cache,
    },
    Image {
        data: Operand,
        /// One range, or one VGPR per address with the NSA form.
        addr: Vec<Operand>,
        rsrc: Operand,
        sampler: Option<Operand>,
        dmask: u8,
        dim: u8,
        unorm: bool,
        r128: bool,
        a16: bool,
        d16: bool,
        tfe: bool,
        lwe: bool,
        cache: Cache,
    },
    /// FLAT, GLOBAL and SCRATCH.
    Flat {
        dst: Option<Operand>,
        addr: Option<Operand>,
        data: Option<Operand>,
        saddr: Option<Operand>,
        offset: i16,
        lds: bool,
        cache: Cache,
    },
    Exp {
        target: u8,
        /// Per channel; `None` when disabled.
        src: [Option<Operand>; 4],
        compr: bool,
        done: bool,
        vm: bool,
    },
}

/// VOP1, VOP2, VOPC, VOP3 and VOP3P, with their SDWA and DPP forms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Valu {
    /// A VGPR range, an SGPR (`v_readlane_b32`...) or a lane mask (compares).
    pub dst: Option<Operand>,
    /// Carry-out or the second result of VOP3B (`v_add_co_u32`, `v_div_scale_f32`...).
    pub carry: Option<Operand>,
    /// In operand order, including the implicit VCC of VOP2 `v_cndmask_b32`
    /// and carry-in, and the literal of `v_madmk`/`v_madak` forms.
    pub src: Vec<Operand>,
    /// Bit i applies to `src[i]`. For VOP3P, `neg` is the low half's.
    pub neg: u8,
    pub abs: u8,
    /// VOP3P: negate the high half (`v_fma_mix*`: absolute value).
    pub neg_hi: u8,
    /// VOP3: bit 3 is the destination. VOP3P with `opsel_hi`: the halves used.
    pub opsel: u8,
    pub opsel_hi: u8,
    pub clamp: bool,
    /// Output modifier: 1 ×2, 2 ×4, 3 ÷2.
    pub omod: u8,
    pub sdwa: Option<Sdwa>,
    pub dpp: Option<Dpp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sdwa {
    /// Selects: 0-3 byte, 4-5 word, 6 dword. Compares have no `dst_sel`.
    pub dst_sel: u8,
    /// 0 pad with zeros, 1 sign-extend, 2 preserve.
    pub dst_unused: u8,
    pub src_sel: [u8; 2],
    /// Bit i sign-extends `src[i]`.
    pub sext: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dpp {
    /// DPP16: `ctrl` is the 9-bit permutation (quad_perm, row shifts...).
    Row {
        ctrl: u16,
        row_mask: u8,
        bank_mask: u8,
        bound_ctrl: bool,
        fi: bool,
    },
    /// DPP8: eight 3-bit lane selects.
    Lanes { sel: u32, fi: bool },
}


mod decode;
mod dwords;

pub(super) use dwords::ds_pair;
