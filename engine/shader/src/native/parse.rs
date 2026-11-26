use crate::ShaderError::{self, Malformed};
use crate::dxbc::{u32_at, u64_at};

use super::format::*;
use super::{
    ComputeIo, ExportPatch, HwStage, MeshIo, NativeProgram, PsInput, ResourceType, RootParam,
    RootType, StageIo, Usage, UsedResource, VsInput, VsOutput,
};

/// The native programs in a `DXIL` part's data, or in an `ACCC` part's
/// ([`fspx_program`]); empty for the PC layout.
pub fn native_programs(part: &[u8]) -> Result<Vec<NativeProgram>, ShaderError> {
    if part.starts_with(b"FSPX") {
        return Ok(vec![fspx_program(part)?]);
    }
    if part.get(XDAT_OFFSET..XDAT_OFFSET + 4) != Some(b"XDAT") {
        return Ok(Vec::new());
    }
    let count = u32_at(part, XDAT_OFFSET + 4)? as usize;
    if count > MAX_XDAT_ENTRIES {
        return Err(Malformed("implausible XDAT entry count"));
    }
    let mut out = Vec::new();
    for i in 0..count {
        let at = XDAT_OFFSET + 12 + 12 * i;
        if u32_at(part, at)? != KIND_NATIVE {
            continue;
        }
        let offset = u32_at(part, at + 4)?.into();
        let size = u32_at(part, at + 8)?.into();
        let blob = bytes(part, offset, size).ok_or(Malformed("XDAT entry out of bounds"))?;
        out.extend(programs(&unpack(blob)?)?);
    }
    Ok(out)
}

/// The compute program of an `ACCC` part, which a container carries in place
/// of `DXIL` when another compiler made the native code (the AMD
/// FidelityFX upscaler's shaders). Layout as observed (offsets from "FSPX"):
///
/// ```text
/// +0x00 "FSPX" u32 version (1), u32 entry count, u32 size; entries of 16 B
///       from +0x10: u32 kind, u32 offset, u32 size, u32 0
/// kind 0  pipeline registers, 0x1c B: +0x0c u16 x3 thread-group size,
///         +0x12 u16 flags (bit 4 wave32), +0x14 COMPUTE_PGM_RSRC1,
///         +0x18 COMPUTE_PGM_RSRC2
/// kind 1  pipeline metadata (a hash); kind 6 the GFX10 code
/// kind 7  program metadata: u32 0x403, u32 size - 0x14, u32 code dwords,
///         u64 0, u8 0x40, u8 first user-data dword, u8 count of 0x41
///         records, u8 0; then 32-byte records from +0x18, each a u32 tag:
///   0x41  a resource the code reads: u8 resource type (as FSSM's used
///         entries), u8 root type, u16 0, u32 table offset (dwords), u32
///         register, u32 view dimension (1 buffer, 4 2D, 0 root view),
///         u8 descriptor dwords, u8 user-data dword, u8 root slot, u8 0,
///         u32 -1, u32 0
///   0x42  a view declaration: u32 0, u8 1, u8 class (0 SRV, 1 UAV),
///         u8 resource kind (DXIL's), u8 0, u32 register
///   0x43  the shader hash (16 B); the last record
/// ```
pub fn fspx_program(part: &[u8]) -> Result<NativeProgram, ShaderError> {
    if u32_at(part, 4)? != FSPX_VERSION {
        return Err(Malformed("unknown FSPX version"));
    }
    let count = u32_at(part, 8)? as usize;
    if count > MAX_XDAT_ENTRIES {
        return Err(Malformed("implausible FSPX entry count"));
    }
    let entry = |kind: u32| -> Result<&[u8], ShaderError> {
        for i in 0..count {
            let at = FSPX_HEADER + 16 * i;
            if u32_at(part, at)? == kind {
                let (offset, size) = (u32_at(part, at + 4)?, u32_at(part, at + 8)?);
                return bytes(part, offset.into(), size.into())
                    .ok_or(Malformed("FSPX entry out of bounds"));
            }
        }
        Err(Malformed("FSPX entry missing"))
    };
    let packet = entry(FSPX_PACKET)?;
    let code = entry(FSPX_CODE)?;
    let meta = entry(FSPX_METADATA)?;
    if u32_at(meta, 8)? as usize * 4 != code.len() {
        return Err(Malformed("FSPX code size disagrees"));
    }
    let wave64 = u16_at(packet, PACKET_FLAGS)? & PACKET_WAVE32 == 0;
    // VGPRs in granules of 4 (wave64) or 8 (wave32).
    let granules = (u32_at(packet, PACKET_RSRC1)? & 0x3F) + 1;
    let vgprs = (granules * if wave64 { 4 } else { 8 }).min(256) as u16;
    let threads = |i: usize| u16_at(packet, PACKET_THREADS + 2 * i).map(u32::from);
    let mut layout: Vec<RootParam> = Vec::new();
    let mut used = Vec::new();
    for e in meta
        .get(FSPX_RECORDS..)
        .unwrap_or(&[])
        .as_chunks::<FSPX_RECORD>()
        .0
    {
        match e[0] {
            0x41 => {}
            0x42 => continue,
            0x43 => break,
            _ => return Err(Malformed("unknown FSPX metadata record")),
        }
        let root = RootType::from_raw(e[5].into());
        let u = UsedResource {
            kind: match e[4] {
                0 => ResourceType::Srv,
                1 => ResourceType::Sampler,
                2 => ResourceType::Cbv,
                3 => ResourceType::Uav,
                n => ResourceType::Other(n),
            },
            root,
            table_offset_dwords: le32(e, 8),
            lower_bound: le32(e, 12),
            stride: 0,
            dwords: e[20],
            user_data: e[21],
            slot: e[22],
        };
        let sgprs = match root {
            RootType::Table => 1,
            RootType::Descriptor => 2,
            // The code holds its words; no user data.
            RootType::ImmediateSampler => 0,
            _ => return Err(Malformed("an FSPX resource of an unknown root type")),
        };
        if sgprs > 0 && !layout.iter().any(|p| p.slot == u.slot) {
            layout.push(RootParam {
                root,
                table_offset_dwords: 0,
                sgprs,
                base_register: u.lower_bound,
                user_data: u.user_data,
                slot: u.slot,
            });
        }
        used.push(u);
    }
    Ok(NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs,
        layout,
        used,
        io: StageIo::Cs(ComputeIo {
            rsrc2: u32_at(packet, PACKET_RSRC2)?,
            threads: [threads(0)?, threads(1)?, threads(2)?],
        }),
        code: code.to_vec(),
    })
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, ShaderError> {
    data.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(Malformed("FSPX data out of bounds"))
}

pub(super) fn unpack(blob: &[u8]) -> Result<Vec<u8>, ShaderError> {
    if blob.get(0..4) != Some(b"XBCH") {
        return Err(Malformed("native blob lacks XBCH magic"));
    }
    if u32_at(blob, 4)? != XBCH_VERSION {
        return Err(Malformed("unknown XBCH version"));
    }
    let packed = u64_at(blob, 8)?;
    let unpacked = u64_at(blob, 16)?;
    if unpacked > MAX_UNPACKED {
        return Err(Malformed("implausible XBCH unpacked size"));
    }
    let stream =
        bytes(blob, XBCH_HEADER as u64, packed).ok_or(Malformed("XBCH stream out of bounds"))?;
    let out = inflate_zlib_exact(stream, unpacked as usize)
        .ok_or(Malformed("XBCH stream does not inflate to its stated size"))?;
    Ok(out)
}

/// Inflates a zlib stream incrementally up to `expected` bytes, rejecting
/// truncated, corrupt, under-filled, or oversized streams without eagerly
/// allocating the full declared size up front.
fn inflate_zlib_exact(source: &[u8], expected: usize) -> Option<Vec<u8>> {
    let mut state = zlib_rs::Inflate::new(true, 15);
    let mut out = Vec::new();
    let (mut in_pos, mut out_pos) = (0usize, 0usize);
    loop {
        if out_pos == expected {
            let mut extra = [0u8; 1];
            let status = state
                .decompress(
                    &source[in_pos..],
                    &mut extra,
                    zlib_rs::InflateFlush::NoFlush,
                )
                .ok()?;
            return (status == zlib_rs::Status::StreamEnd
                && state.total_out() as usize == expected)
                .then_some(out);
        }
        if out_pos == out.len() {
            let next_len = if out.is_empty() {
                expected.min(source.len().saturating_mul(4).max(4096))
            } else {
                out.len().saturating_mul(2).min(expected)
            };
            out.resize(next_len, 0);
        }
        let status = state
            .decompress(
                &source[in_pos..],
                &mut out[out_pos..],
                zlib_rs::InflateFlush::NoFlush,
            )
            .ok()?;
        let next_in = state.total_in() as usize;
        let next_out = state.total_out() as usize;
        match status {
            zlib_rs::Status::StreamEnd => {
                out.truncate(next_out);
                return (next_out == expected).then_some(out);
            }
            zlib_rs::Status::Ok if next_in != in_pos || next_out != out_pos => {
                in_pos = next_in;
                out_pos = next_out;
            }
            zlib_rs::Status::Ok | zlib_rs::Status::BufError => return None,
        }
    }
}

fn programs(fssm: &[u8]) -> Result<Vec<NativeProgram>, ShaderError> {
    if fssm.get(0..4) != Some(b"FSSM") {
        return Err(Malformed("native blob lacks FSSM magic"));
    }
    if u32_at(fssm, 4)? != FSSM_VERSION {
        return Err(Malformed("unknown FSSM version"));
    }
    let payload = &fssm[FSSM_HEADER.min(fssm.len())..];
    if u64_at(fssm, 8)? != payload.len() as u64 {
        return Err(Malformed("FSSM payload size disagrees"));
    }
    if u64_at(payload, 0)? != GRAPHICS_SLOT_COUNT {
        return Err(Malformed("unexpected FSSM slot count"));
    }
    let mut out = Vec::new();
    for (stage, at) in SLOTS {
        let start = u64_at(payload, at)?;
        if start == ABSENT {
            continue;
        }
        let record =
            bytes(payload, start, RECORD_SIZE).ok_or(Malformed("FSSM record out of bounds"))?;
        let code = bytes(payload, u64_at(record, 0)?, u64_at(record, 8)?)
            .ok_or(Malformed("native code out of bounds"))?;
        let vgprs = (u32_at(record, RECORD_REGS)? & 0x3FF) as u16;
        if vgprs > 256 {
            return Err(Malformed("implausible VGPR count"));
        }
        let table = |at: usize, count_at: usize| -> Result<&[u8], ShaderError> {
            let len = ENTRY_SIZE * u64::from(u32_at(record, count_at)?);
            if len == 0 {
                return Ok(&[]);
            }
            bytes(payload, u64_at(record, at)?, len)
                .ok_or(Malformed("FSSM root table out of bounds"))
        };
        let layout = table(RECORD_LAYOUT, RECORD_COUNTS)?
            .as_chunks::<{ ENTRY_SIZE as usize }>()
            .0
            .iter()
            .map(|e| RootParam {
                root: RootType::from_raw(le32(e, 0)),
                table_offset_dwords: le32(e, 4),
                sgprs: le32(e, 8),
                base_register: le32(e, 12),
                user_data: e[16],
                slot: e[17],
            })
            .collect();
        let used = table(RECORD_USED, RECORD_COUNTS + 4)?
            .as_chunks::<{ ENTRY_SIZE as usize }>()
            .0
            .iter()
            .map(|e| UsedResource {
                kind: match e[0] {
                    0 => ResourceType::Srv,
                    1 => ResourceType::Sampler,
                    2 => ResourceType::Cbv,
                    3 => ResourceType::Uav,
                    n => ResourceType::Other(n),
                },
                root: RootType::from_raw(e[1].into()),
                table_offset_dwords: le32(e, 4),
                lower_bound: le32(e, 8),
                stride: le32(e, 12),
                dwords: e[16],
                user_data: e[17],
                slot: e[18],
            })
            .collect();
        // The record is in bounds, so its end is too.
        let stage_data = &payload[(start + RECORD_SIZE) as usize..];
        let io = match stage {
            HwStage::Ps => ps_io(stage_data, code.len())?,
            HwStage::Vs => vs_io(stage_data)?,
            HwStage::Cs => StageIo::Cs(ComputeIo {
                rsrc2: u32_at(stage_data, CS_RSRC2)?,
                threads: [
                    u32_at(stage_data, CS_THREADS)?,
                    u32_at(stage_data, CS_THREADS + 4)?,
                    u32_at(stage_data, CS_THREADS + 8)?,
                ],
            }),
            HwStage::Ms => ms_io(stage_data, u32_at(record, RECORD_LDS)?)?,
            _ => StageIo::None,
        };
        out.push(NativeProgram {
            stage,
            wave64: u32_at(record, RECORD_FLAGS)? & FLAG_WAVE32 == 0,
            vgprs,
            layout,
            used,
            io,
            code: code.to_vec(),
        });
    }
    Ok(out)
}

/// The entries of `size` bytes from `at`, with their count at `count_at`.
fn semantics(data: &[u8], at: usize, size: usize, count_at: usize) -> Result<&[u8], ShaderError> {
    let count = u32_at(data, count_at)?;
    if count > MAX_SEMANTICS {
        return Err(Malformed("implausible semantic count"));
    }
    data.get(at..at + size * count as usize)
        .ok_or(Malformed("stage data out of bounds"))
}

fn ps_io(data: &[u8], code_len: usize) -> Result<StageIo, ShaderError> {
    let inputs = semantics(data, PS_INPUTS, 8, PS_INPUT_COUNT)?
        .as_chunks::<8>()
        .0
        .iter()
        .map(|e| PsInput {
            usage: Usage::from_raw(e[0]),
            row: e[1],
            mask: e[2] & 0xf,
            attr: e[3],
            flat: e[7] & 1 != 0,
            fp16: e[7] & 8 != 0,
            pack2: e[7] & 0x10 != 0,
            row2: e[5],
        })
        .collect();
    let size = u32_at(data, PS_PATCH_SIZE)? as usize;
    if size > PATCH_SLOT_DWORDS {
        return Err(Malformed("implausible export patch size"));
    }
    let mut patches = Vec::new();
    for mrt in 0..MRTS {
        let at = PS_PATCHES + mrt * PATCH_BYTES;
        let offset = u32_at(data, at)?;
        if offset == u32::MAX {
            continue;
        }
        if (offset as usize + size) * 4 > code_len {
            return Err(Malformed("export patch outside the code"));
        }
        let full = at + 4 + PATCH_FULL * PATCH_SLOT_DWORDS * 4;
        let code = (0..size)
            .map(|i| u32_at(data, full + 4 * i))
            .collect::<Result<_, _>>()?;
        patches.push(ExportPatch {
            mrt: mrt as u8,
            offset,
            code,
        });
    }
    Ok(StageIo::Ps {
        inputs,
        input_addr: u32_at(data, PS_INPUT_ADDR)?,
        db_shader_control: u32_at(data, PS_DB_SHADER_CONTROL)?,
        patches,
    })
}

fn vs_outputs(data: &[u8]) -> Result<Vec<VsOutput>, ShaderError> {
    Ok(semantics(data, VS_OUTPUTS, 6, VS_OUTPUT_COUNT)?
        .as_chunks::<6>()
        .0
        .iter()
        .map(|e| VsOutput {
            usage: Usage::from_raw(e[0]),
            row: e[1],
            param: e[2],
            fp16: e[5] & 1 != 0,
            pack2: e[5] & 2 != 0,
        })
        .collect())
}

fn vs_io(data: &[u8]) -> Result<StageIo, ShaderError> {
    let inputs = semantics(data, VS_INPUTS, 5, VS_INPUT_COUNT)?
        .as_chunks::<5>()
        .0
        .iter()
        .map(|e| VsInput {
            usage: Usage::from_raw(e[0]),
            row: e[1],
            mask: e[2] & 0xf,
            vgpr: e[3],
        })
        .collect();
    let outputs = vs_outputs(data)?;
    Ok(StageIo::Vs { inputs, outputs })
}

fn ms_io(data: &[u8], lds_bytes: u32) -> Result<StageIo, ShaderError> {
    let outputs = vs_outputs(data)?;
    let info = data
        .get(MS_INFO..MS_INFO + 8)
        .ok_or(Malformed("stage data out of bounds"))?;
    let half = |at: usize| u32::from(u16::from_le_bytes([info[at], info[at + 1]]));
    Ok(StageIo::Ms(MeshIo {
        outputs,
        clip_cntl: u32_at(data, MS_CLIP_CNTL)?,
        threads: half(0),
        max_vertices: half(2),
        max_primitives: half(4),
        topology: info[7],
        lds_bytes,
    }))
}

/// For entries already sized by `as_chunks`.
fn le32(entry: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(entry[at..at + 4].try_into().unwrap())
}

/// `data[offset..offset + len]` with untrusted 64-bit bounds.
fn bytes(data: &[u8], offset: u64, len: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    data.get(start..start.checked_add(usize::try_from(len).ok()?)?)
}

