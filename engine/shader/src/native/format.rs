use super::HwStage;

pub(super) const XDAT_OFFSET: usize = 0x18;
pub(super) const MAX_XDAT_ENTRIES: usize = 64;
pub(super) const KIND_NATIVE: u32 = 0x0d;
pub(super) const XBCH_VERSION: u32 = 3;
pub(super) const XBCH_HEADER: usize = 0x18;
pub(super) const FSSM_VERSION: u32 = 0x101;
pub(super) const FSSM_HEADER: usize = 0x10;
pub(super) const GRAPHICS_SLOT_COUNT: u64 = 6;
/// Where each stage's record offset sits in the FSSM payload.
pub(super) const SLOTS: [(HwStage, usize); 8] = [
    (HwStage::Ls, 0x08),
    (HwStage::Hs, 0x10),
    (HwStage::Es, 0x18),
    (HwStage::Gs, 0x20),
    (HwStage::Vs, 0x28),
    (HwStage::Ps, 0x30),
    (HwStage::Ms, 0x48),
    (HwStage::Cs, 0x60),
];
pub(super) const ABSENT: u64 = 0xDEAD_BEEF_DEAD_BEEF;
pub(super) const RECORD_SIZE: u64 = 0x54;
pub(super) const RECORD_FLAGS: usize = 0x14;
pub(super) const FLAG_WAVE32: u32 = 1 << 5;
pub(super) const RECORD_REGS: usize = 0x50;
pub(super) const RECORD_LAYOUT: usize = 0x28;
pub(super) const RECORD_USED: usize = 0x30;
pub(super) const RECORD_COUNTS: usize = 0x38;
pub(super) const ENTRY_SIZE: u64 = 20;
pub(super) const MAX_SEMANTICS: u32 = 32;
pub(super) const PS_INPUTS: usize = 0x24;
pub(super) const PS_INPUT_COUNT: usize = 0x124;
pub(super) const PS_INPUT_ADDR: usize = 0x134;
pub(super) const PS_DB_SHADER_CONTROL: usize = 0x138;
pub(super) const PS_PATCH_SIZE: usize = 0x148;
pub(super) const PS_PATCHES: usize = 0x14c;
pub(super) const PATCH_SLOT_DWORDS: usize = 10;
/// The variant that exports every channel as a raw 32-bit value.
pub(super) const PATCH_FULL: usize = 9;
pub(super) const PATCH_BYTES: usize = 4 + 10 * PATCH_SLOT_DWORDS * 4;
pub(super) const MRTS: usize = 8;
pub(super) const VS_INPUTS: usize = 0x24;
pub(super) const VS_INPUT_COUNT: usize = 0xc4;
pub(super) const VS_OUTPUTS: usize = 0xc8;
pub(super) const VS_OUTPUT_COUNT: usize = 0x188;
pub(super) const CS_RSRC2: usize = 0x24;
pub(super) const CS_THREADS: usize = 0x28;
pub(super) const RECORD_LDS: usize = 0x4c;
pub(super) const MS_CLIP_CNTL: usize = 0x194;
pub(super) const MS_INFO: usize = 0x448;
pub(super) const FSPX_VERSION: u32 = 1;
pub(super) const FSPX_HEADER: usize = 0x10;
pub(super) const FSPX_PACKET: u32 = 0;
pub(super) const FSPX_CODE: u32 = 6;
pub(super) const FSPX_METADATA: u32 = 7;
pub(super) const PACKET_THREADS: usize = 0x0c;
pub(super) const PACKET_FLAGS: usize = 0x12;
pub(super) const PACKET_WAVE32: u16 = 1 << 4;
pub(super) const PACKET_RSRC1: usize = 0x14;
pub(super) const PACKET_RSRC2: usize = 0x18;
pub(super) const FSPX_RECORDS: usize = 0x18;
pub(super) const FSPX_RECORD: usize = 32;
/// Unpacked blobs are a few KiB; refuse to allocate for absurd sizes.
pub(super) const MAX_UNPACKED: u64 = 16 << 20;
