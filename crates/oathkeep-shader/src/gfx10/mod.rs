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
    pub offset: usize,
    pub encoding: Encoding,
    pub opcode: u16,
    pub words: Vec<u32>,
}

impl Instruction {
    pub fn name(&self) -> Option<&'static str> {
        name(self.encoding, self.opcode)
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Next,
    Stop,
    Jump(usize),
    Branch(usize),
}

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
        (Encoding::Sop1, "s_setpc_b64") => Flow::Stop,
        _ => Flow::Next,
    })
}

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
                let literal = i64::from(prev.words[1] as i32);
                delta = Some(if op == "s_add_u32" { literal } else { -literal });
            }
            (Encoding::Sop1, Some("s_getpc_b64")) if sdst == n => {
                let Some(delta) = delta else { break };
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
    Ok(Instruction {
        offset,
        encoding,
        opcode: opcode as u16,
        words: (0..len).map(word).collect::<Result<_, _>>()?,
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
mod tests {
    use super::*;

    const S_ENDPGM: u32 = 0xBF81_0000;
    const fn sopp(op: u32, simm16: i16) -> u32 {
        0xBF80_0000 | op << 16 | simm16 as u16 as u32
    }

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn summary(code: &[u32]) -> Vec<(usize, usize, &'static str)> {
        decode(&bytes(code))
            .unwrap()
            .iter()
            .map(|i| (i.offset, i.words.len(), i.name().unwrap_or("?")))
            .collect()
    }

    #[test]
    fn tables_are_sorted() {
        for t in [
            opcodes::SOP2,
            opcodes::SOPK,
            opcodes::SOP1,
            opcodes::SOPC,
            opcodes::SOPP,
            opcodes::SMEM,
            opcodes::VOP2,
            opcodes::VOP1,
            opcodes::VOPC,
            opcodes::VOP3,
            opcodes::VOP3P,
            opcodes::VINTRP,
            opcodes::DS,
            opcodes::MUBUF,
            opcodes::MTBUF,
            opcodes::MIMG,
            opcodes::FLAT,
            opcodes::GLOBAL,
            opcodes::SCRATCH,
        ] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        }
    }

    #[test]
    fn lengths_and_names() {
        let code = [
            0xBE80_03FF,
            0x3F80_0000,
            0x0600_0000,
            0xD54B_0000,
            0x03FE_0000,
            0x4000_0000,
            0xF080_0F0A,
            0,
            0,
            0xF800_080F,
            0,
            S_ENDPGM,
        ];
        assert_eq!(
            summary(&code),
            [
                (0, 2, "s_mov_b32"),
                (8, 1, "v_add_f32"),
                (12, 3, "v_fma_f32"),
                (24, 3, "image_sample"),
                (36, 2, "exp"),
                (44, 1, "s_endpgm"),
            ]
        );
    }

    #[test]
    fn follows_branches_and_skips_data() {
        let code = [
            sopp(0x04, 1),
            sopp(0x02, 1),
            S_ENDPGM,
            S_ENDPGM,
            S_ENDPGM,
            0xFFFF_FFFF,
        ];
        let found: Vec<_> = summary(&code).iter().map(|i| i.0).collect();
        assert_eq!(found, [0, 4, 8, 12]);
    }

    #[test]
    fn follows_far_jumps() {
        let code = [
            0xBE88_1F00,
            0x8008_FF08,
            20,
            0x8209_8009,
            0xBE80_2008,
            0xFFFF_FFFF,
            S_ENDPGM,
        ];
        let found: Vec<_> = summary(&code).iter().map(|i| (i.0, i.2)).collect();
        assert_eq!(
            found,
            [
                (0, "s_getpc_b64"),
                (4, "s_add_u32"),
                (12, "s_addc_u32"),
                (16, "s_setpc_b64"),
                (24, "s_endpgm"),
            ]
        );
        assert_eq!(summary(&[0xBE80_2018, S_ENDPGM]).len(), 1);
    }

    #[test]
    fn follows_far_jumps_backward() {
        let code = [
            sopp(0x02, 1),
            S_ENDPGM,
            0xBE88_1F00,
            0x8008_FF08,
            0xFFFF_FFF8,
            0x8209_C109,
            0xBE80_2008,
        ];
        let found: Vec<_> = summary(&code).iter().map(|i| (i.0, i.2)).collect();
        assert_eq!(
            found,
            [
                (0, "s_branch"),
                (4, "s_endpgm"),
                (8, "s_getpc_b64"),
                (12, "s_add_u32"),
                (20, "s_addc_u32"),
                (24, "s_setpc_b64"),
            ]
        );
    }

    #[test]
    fn rejects_bad_code() {
        assert!(decode(&[0; 3]).is_err(), "size not a multiple of 4");
        assert!(
            decode(&bytes(&[sopp(0x02, 100)])).is_err(),
            "branch out of range"
        );
        assert!(
            decode(&bytes(&[0xBE80_03FF])).is_err(),
            "literal past the end"
        );
        assert!(
            decode(&bytes(&[0xE400_0000, 0])).is_err(),
            "unknown encoding"
        );
        assert!(decode(&bytes(&[sopp(0x04, 1), 0xBE80_03FF, S_ENDPGM, S_ENDPGM])).is_err());
    }
}
