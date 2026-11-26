//! GFX10 machine code: instruction boundaries, encodings and opcodes.
//!
//! Instructions are found by following control flow from offset 0, not by a
//! linear sweep: compilers place data (constant tables) in the code region
//! after the end of the program. [`Instruction::fields`] decodes the operand
//! fields and [`Instruction::text`] prints them in AMD's SP3 syntax.
//!
//! Encodings by the top bits of the first dword:
//!
//! ```text
//! 0xxxxxxx  VOP2; 0111111 VOP1; 0111110 VOPC
//! 10        SOP2; 1011 SOPK; 101111101 SOP1; 101111110 SOPC; 101111111 SOPP
//! 110010 VINTRP  110011 VOP3P  110101 VOP3  110110 DS  110111 FLAT/GLOBAL/SCRATCH
//! 111000 MUBUF   111010 MTBUF  111100 MIMG  111101 SMEM  111110 EXP
//! ```

mod fields;
mod opcodes;
mod sp3;

pub use fields::{Cache, Dpp, Fields, NULL, Operand, Sdwa, Valu};

use std::collections::BTreeMap;

use crate::ShaderError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Sop2,
    Sopk,
    Sop1,
    Sopc,
    Sopp,
    Smem,
    Vop2,
    Vop1,
    Vopc,
    Vop3,
    Vop3p,
    Vintrp,
    Ds,
    Mubuf,
    Mtbuf,
    Mimg,
    Exp,
    Flat,
    Global,
    Scratch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// Byte offset in the program.
    pub offset: usize,
    pub encoding: Encoding,
    pub opcode: u16,
    /// Every dword of the instruction, including literals and extra dwords.
    pub words: Vec<u32>,
    /// [`name`] of `encoding` and `opcode`, resolved once at decode time.
    /// The translator asks about every instruction eight times; the table is
    /// a binary search.
    name: Option<&'static str>,
}

impl Instruction {
    /// An instruction of these words, its name resolved.
    pub fn new(offset: usize, encoding: Encoding, opcode: u16, words: Vec<u32>) -> Self {
        Self { offset, encoding, opcode, words, name: name(encoding, opcode) }
    }

    /// AMD's name for the opcode, if it is in our table.
    pub fn name(&self) -> Option<&'static str> {
        self.name
    }
}

/// Decodes every instruction reachable from offset 0, sorted by offset.
pub fn decode(code: &[u8]) -> Result<Vec<Instruction>, ShaderError> {
    if !code.len().is_multiple_of(4) {
        return Err(isa(code.len(), "code size is not a multiple of 4"));
    }
    let mut found = BTreeMap::new();
    let mut pending = vec![0usize];
    while let Some(mut offset) = pending.pop() {
        while !found.contains_key(&offset) {
            let inst = instruction(code, offset)?;
            let next = offset + 4 * inst.words.len();
            let flow = control(&inst, code.len(), |at| {
                found.range(..at).next_back().map(|(_, i)| i)
            })?;
            found.insert(offset, inst);
            match flow {
                Flow::Next => offset = next,
                Flow::Stop => break,
                Flow::Jump(target) => offset = target,
                Flow::Branch(target) => {
                    pending.push(target);
                    offset = next;
                }
            }
        }
    }
    let list: Vec<Instruction> = found.into_values().collect();
    if let Some(w) = list
        .windows(2)
        .find(|w| w[0].offset + 4 * w[0].words.len() > w[1].offset)
    {
        return Err(isa(w[1].offset, "a branch lands inside an instruction"));
    }
    Ok(list)
}

/// Where execution goes after an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Next,
    /// End of the program, or an indirect jump that is not a far jump.
    Stop,
    Jump(usize),
    /// Conditional: to the target or the next instruction.
    Branch(usize),
}

/// Control flow after `inst`, resolving far jumps. `before(at)` returns the
/// decoded instruction with the highest offset below `at`, if any.
pub(crate) fn control<'a>(
    inst: &Instruction,
    len: usize,
    before: impl Fn(usize) -> Option<&'a Instruction>,
) -> Result<Flow, ShaderError> {
    Ok(match flow(inst, inst.offset + 4 * inst.words.len(), len)? {
        Flow::Stop if inst.name() == Some("s_setpc_b64") => {
            far_jump(before, inst, len)?.map_or(Flow::Stop, Flow::Jump)
        }
        flow => flow,
    })
}

fn flow(inst: &Instruction, next: usize, len: usize) -> Result<Flow, ShaderError> {
    let target = || {
        let delta = 4 * i64::from(inst.words[0] as u16 as i16);
        usize::try_from(next as i64 + delta)
            .ok()
            .filter(|&t| t < len)
            .ok_or(isa(inst.offset, "branch target outside the code"))
    };
    Ok(match (inst.encoding, inst.name().unwrap_or("")) {
        (
            Encoding::Sopp,
            "s_endpgm" | "s_endpgm_saved" | "s_endpgm_ordered_ps_done" | "s_code_end",
        ) => Flow::Stop,
        (Encoding::Sopp, "s_branch") => Flow::Jump(target()?),
        (Encoding::Sopp, name) if name.starts_with("s_cbranch_") => Flow::Branch(target()?),
        (Encoding::Sopk, "s_call_b64") => Flow::Branch(target()?),
        // Indirect jump; `decode` resolves the compiler's far-jump form.
        (Encoding::Sop1, "s_setpc_b64") => Flow::Stop,
        _ => Flow::Next,
    })
}

/// Resolves the far jump compilers emit beyond the reach of a branch offset:
/// `s_getpc_b64 s[n:n+1]`, `s_add_u32|s_sub_u32 sn, sn, literal`, a carry
/// into s(n+1), `s_setpc_b64 s[n:n+1]`, looking back over the straight-line
/// run just decoded. Any other `s_setpc_b64` (e.g. a handover to the next
/// merged stage through registers) gives `None`.
fn far_jump<'a>(
    before: impl Fn(usize) -> Option<&'a Instruction>,
    setpc: &Instruction,
    len: usize,
) -> Result<Option<usize>, ShaderError> {
    let n = setpc.words[0] & 0xFF;
    let mut delta = None;
    let mut at = setpc.offset;
    for _ in 0..4 {
        let Some(prev) = before(at) else {
            break;
        };
        if prev.offset + 4 * prev.words.len() != at {
            break;
        }
        let w = prev.words[0];
        let (sdst, ssrc0, ssrc1) = ((w >> 16) & 0x7F, w & 0xFF, (w >> 8) & 0xFF);
        match (prev.encoding, prev.name()) {
            (Encoding::Sop2, Some(op @ ("s_add_u32" | "s_sub_u32")))
                if sdst == n && ssrc0 == n && ssrc1 == 0xFF && delta.is_none() =>
            {
                // The low dword of a signed 64-bit offset: a backward jump
                // adds a negative one (its high dword is -1).
                let literal = i64::from(prev.words[1] as i32);
                delta = Some(if op == "s_add_u32" { literal } else { -literal });
            }
            (Encoding::Sop1, Some("s_getpc_b64")) if sdst == n => {
                let Some(delta) = delta else { break };
                // s_getpc_b64 yields the address of the instruction after it.
                return usize::try_from(prev.offset as i64 + 4 + delta)
                    .ok()
                    .filter(|&t| t < len)
                    .map(Some)
                    .ok_or(isa(setpc.offset, "far jump target outside the code"));
            }
            _ => {}
        }
        at = prev.offset;
    }
    Ok(None)
}

fn instruction(code: &[u8], offset: usize) -> Result<Instruction, ShaderError> {
    let word = |i: usize| {
        code.get(offset + 4 * i..offset + 4 * i + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
            .ok_or(isa(offset, "instruction runs past the end of the code"))
    };
    let w = word(0)?;
    let (encoding, opcode, len) = if w >> 31 == 0 {
        let src0 = w & 0x1FF;
        // Literal constant, SDWA, DPP16, DPP8, DPP8 with fetch-inactive.
        let extra = usize::from(matches!(src0, 0xFF | 0xF9 | 0xFA | 0xE9 | 0xEA));
        match w >> 25 {
            0x3F => (Encoding::Vop1, (w >> 9) & 0xFF, 1 + extra),
            0x3E => (Encoding::Vopc, (w >> 17) & 0xFF, 1 + extra),
            op => {
                let mk_ak = name(Encoding::Vop2, op as u16).is_some_and(|n| {
                    matches!(
                        n,
                        "v_madmk_f32"
                            | "v_madak_f32"
                            | "v_fmamk_f32"
                            | "v_fmaak_f32"
                            | "v_madmk_f16"
                            | "v_madak_f16"
                            | "v_fmamk_f16"
                            | "v_fmaak_f16"
                    )
                });
                (Encoding::Vop2, op, 1 + extra.max(usize::from(mk_ak)))
            }
        }
    } else if w >> 30 == 0b10 {
        let literal = |srcs: &[u32]| 1 + usize::from(srcs.contains(&0xFF));
        match w >> 23 {
            0x17D => (Encoding::Sop1, (w >> 8) & 0xFF, literal(&[w & 0xFF])),
            0x17E => (
                Encoding::Sopc,
                (w >> 16) & 0x7F,
                literal(&[w & 0xFF, (w >> 8) & 0xFF]),
            ),
            0x17F => (Encoding::Sopp, (w >> 16) & 0x7F, 1),
            _ if w >> 28 == 0b1011 => {
                let op = (w >> 23) & 0x1F;
                let imm32 = name(Encoding::Sopk, op as u16) == Some("s_setreg_imm32_b32");
                (Encoding::Sopk, op, 1 + usize::from(imm32))
            }
            _ => (
                Encoding::Sop2,
                (w >> 23) & 0x7F,
                literal(&[w & 0xFF, (w >> 8) & 0xFF]),
            ),
        }
    } else {
        let vop3 =
            |w1: u32| 2 + usize::from([w1, w1 >> 9, w1 >> 18].iter().any(|s| s & 0x1FF == 0xFF));
        match w >> 26 {
            0x32 => (Encoding::Vintrp, (w >> 16) & 0x3, 1),
            0x33 => (Encoding::Vop3p, (w >> 16) & 0x7F, vop3(word(1)?)),
            0x35 => (Encoding::Vop3, (w >> 16) & 0x3FF, vop3(word(1)?)),
            0x36 => (Encoding::Ds, (w >> 18) & 0xFF, 2),
            0x37 => {
                let encoding = match (w >> 14) & 3 {
                    0 => Encoding::Flat,
                    1 => Encoding::Scratch,
                    2 => Encoding::Global,
                    _ => return Err(isa(offset, "reserved FLAT segment")),
                };
                (encoding, (w >> 18) & 0x7F, 2)
            }
            0x38 => (Encoding::Mubuf, (w >> 18) & 0x7F | (w >> 25 & 1) << 7, 2),
            0x3A => (
                Encoding::Mtbuf,
                (w >> 16) & 0x7 | (word(1)? >> 21 & 1) << 3,
                2,
            ),
            // Bits 2:1 count extra dwords of non-sequential address registers.
            0x3C => (
                Encoding::Mimg,
                (w >> 18) & 0x7F | (w & 1) << 7,
                2 + (w as usize >> 1 & 3),
            ),
            0x3D => (Encoding::Smem, (w >> 18) & 0xFF, 2),
            0x3E => (Encoding::Exp, 0, 2),
            _ => return Err(isa(offset, "unknown encoding")),
        }
    };
    let opcode = opcode as u16;
    Ok(Instruction {
        offset,
        encoding,
        opcode,
        words: (0..len).map(word).collect::<Result<_, _>>()?,
        name: name(encoding, opcode),
    })
}

fn name(encoding: Encoding, opcode: u16) -> Option<&'static str> {
    use opcodes as t;
    let (table, op) = match encoding {
        Encoding::Sop2 => (t::SOP2, opcode),
        Encoding::Sopk => (t::SOPK, opcode),
        Encoding::Sop1 => (t::SOP1, opcode),
        Encoding::Sopc => (t::SOPC, opcode),
        Encoding::Sopp => (t::SOPP, opcode),
        Encoding::Smem => (t::SMEM, opcode),
        Encoding::Vop2 => (t::VOP2, opcode),
        Encoding::Vop1 => (t::VOP1, opcode),
        Encoding::Vopc => (t::VOPC, opcode),
        // VOP3 also encodes the VOPC, VOP2 and VOP1 opcodes at fixed bases.
        Encoding::Vop3 => match opcode {
            0x000..0x100 => (t::VOPC, opcode),
            0x100..0x140 => (t::VOP2, opcode - 0x100),
            0x180..0x200 => (t::VOP1, opcode - 0x180),
            _ => (t::VOP3, opcode),
        },
        Encoding::Vop3p => (t::VOP3P, opcode),
        Encoding::Vintrp => (t::VINTRP, opcode),
        Encoding::Ds => (t::DS, opcode),
        Encoding::Mubuf => (t::MUBUF, opcode),
        Encoding::Mtbuf => (t::MTBUF, opcode),
        Encoding::Mimg => (t::MIMG, opcode),
        Encoding::Exp => return Some("exp"),
        Encoding::Flat => (t::FLAT, opcode),
        Encoding::Global => (t::GLOBAL, opcode),
        Encoding::Scratch => (t::SCRATCH, opcode),
    };
    table
        .binary_search_by_key(&op, |&(o, _)| o)
        .ok()
        .map(|i| table[i].1)
}

fn isa(offset: usize, reason: &'static str) -> ShaderError {
    ShaderError::Isa { offset, reason }
}


#[cfg(test)]
mod tests;
