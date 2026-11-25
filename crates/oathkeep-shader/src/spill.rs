use crate::ShaderError;
use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{self, Program, Reg, Value};
use crate::native::{HwStage, NativeProgram, StageIo};

const FIRST: u32 = 16;

pub fn build(native: &NativeProgram) -> Result<Program, ShaderError> {
    let mut p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    user_data(&mut p, native);
    Ok(p)
}

fn spilled(native: &NativeProgram) -> Option<Vec<(u32, u32)>> {
    let StageIo::Cs(cs) = &native.io else {
        return None;
    };
    if native.stage != HwStage::Cs {
        return None;
    }
    let registers = u32::from(cs.user_sgprs());
    let spilled: Vec<(u32, u32)> = native
        .layout
        .iter()
        .filter(|e| e.sgprs > 0 && u32::from(e.user_data) >= registers)
        .map(|e| (u32::from(e.user_data), e.sgprs))
        .collect();
    (spilled.iter().map(|e| e.0).min() == Some(FIRST) && registers >= 2).then_some(spilled)
}

pub fn is_spilled(native: &NativeProgram) -> bool {
    spilled(native).is_some()
}

pub fn user_data(p: &mut Program, native: &NativeProgram) -> usize {
    let Some(spilled) = spilled(native) else {
        return 0;
    };
    let entry = |reg: Reg, p: &Program| p.inputs.iter().find(|i| i.0 == reg).map(|i| i.1);
    let (Some(low), Some(high)) = (entry(Reg::S(0), p), entry(Reg::S(1), p)) else {
        return 0;
    };

    let mut lows = vec![low];
    let mut highs: Vec<Value> = Vec::new();
    let mut raw_highs = vec![high];
    let mut forming: Vec<(usize, usize)> = Vec::new();
    loop {
        let mut changed = false;
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, inst) in block.insts.iter().enumerate() {
                let Fields::Sop { dst, src, .. } = &inst.fields else {
                    continue;
                };
                let Some(Operand::Scalar { code: d, .. }) = *dst else {
                    continue;
                };
                let Some(out) = inst.write(Reg::S(d)) else {
                    continue;
                };
                let read = |op: &Operand| match *op {
                    Operand::Scalar { code, .. } if code != NULL => inst.read(Reg::S(code)),
                    _ => None,
                };
                let found = match (inst.name(), &src[..]) {
                    ("s_mov_b32", [s]) => match read(s) {
                        Some(v) if lows.contains(&v) => Some(&mut lows),
                        Some(v) if highs.contains(&v) => Some(&mut highs),
                        Some(v) if raw_highs.contains(&v) => Some(&mut raw_highs),
                        _ => None,
                    },
                    ("s_and_b32", [x, y]) => {
                        let masked = |a: &Operand, m: &Operand| {
                            matches!(*m, Operand::Literal(0xff))
                                && read(a).is_some_and(|v| raw_highs.contains(&v))
                        };
                        (masked(x, y) || masked(y, x)).then_some(&mut highs)
                    }
                    _ => None,
                };
                if let Some(set) = found
                    && !set.contains(&out)
                {
                    set.push(out);
                    forming.push((b, i));
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    if highs.is_empty() {
        return 0;
    }

    let mut read_somewhere = std::collections::HashSet::new();
    for block in &p.blocks {
        read_somewhere.extend(block.phis.iter().flat_map(|phi| phi.args.iter().copied()));
        read_somewhere.extend(block.insts.iter().flat_map(|i| i.reads.iter().map(|r| r.1)));
    }
    let mut next = p
        .inputs
        .iter()
        .map(|i| i.1)
        .chain(p.blocks.iter().flat_map(|b| {
            b.phis
                .iter()
                .map(|phi| phi.value)
                .chain(b.insts.iter().flat_map(|i| i.writes.iter().map(|w| w.1)))
        }))
        .map(|v| v.0 + 1)
        .max()
        .unwrap_or(0);

    let mut replaced: std::collections::HashMap<Value, Value> = Default::default();
    let mut gone: Vec<(usize, usize)> = Vec::new();
    let mut inputs = p.inputs.clone();
    for (b, block) in p.blocks.iter().enumerate() {
        'insts: for (i, inst) in block.insts.iter().enumerate() {
            let Fields::Smem {
                data: Some(Operand::Scalar { code: first, .. }),
                base: Operand::Scalar { code: base, .. },
                offset,
                soffset,
                ..
            } = inst.fields
            else {
                continue;
            };
            if !inst.name().starts_with("s_load_dword")
                || !matches!(
                    soffset,
                    Operand::Scalar { code: NULL, .. } | Operand::Const(128)
                )
                || offset < 0
                || offset % 4 != 0
                || !inst.read(Reg::S(base)).is_some_and(|v| lows.contains(&v))
                || !inst
                    .read(Reg::S(base + 1))
                    .is_some_and(|v| highs.contains(&v))
            {
                continue;
            }
            let mut dwords = Vec::new();
            for &(reg, v) in &inst.writes {
                let Reg::S(code) = reg else {
                    continue 'insts;
                };
                let Some(k) = code.checked_sub(first) else {
                    continue 'insts;
                };
                let index = FIRST + offset as u32 / 4 + u32::from(k);
                let declared = spilled.iter().any(|&(at, n)| index >= at && index < at + n);
                if !declared {
                    if read_somewhere.contains(&v) {
                        continue 'insts;
                    }
                    continue;
                }
                let Ok(code) = u8::try_from(index) else {
                    continue 'insts;
                };
                if code > 105 {
                    continue 'insts;
                }
                dwords.push((Reg::S(code), v));
            }
            for (reg, v) in dwords {
                let value = match inputs.iter().find(|e| e.0 == reg) {
                    Some(e) => e.1,
                    None => {
                        let value = Value(next);
                        next += 1;
                        inputs.push((reg, value));
                        value
                    }
                };
                replaced.insert(v, value);
            }
            gone.push((b, i));
        }
    }
    if gone.is_empty() {
        return 0;
    }
    p.inputs = inputs;
    let renamed = |v: &mut Value| {
        if let Some(&to) = replaced.get(v) {
            *v = to;
        }
    };
    for block in &mut p.blocks {
        for phi in &mut block.phis {
            phi.args.iter_mut().for_each(renamed);
        }
        for inst in &mut block.insts {
            inst.reads.iter_mut().for_each(|r| renamed(&mut r.1));
        }
    }
    let mut drop = gone.clone();
    loop {
        let read_somewhere = {
            let mut used = std::collections::HashSet::new();
            for (b, block) in p.blocks.iter().enumerate() {
                used.extend(block.phis.iter().flat_map(|phi| phi.args.iter().copied()));
                for (i, inst) in block.insts.iter().enumerate() {
                    if !drop.contains(&(b, i)) {
                        used.extend(inst.reads.iter().map(|r| r.1));
                    }
                }
            }
            used
        };
        let before = drop.len();
        for &site in &forming {
            let inst = &p.blocks[site.0].insts[site.1];
            if !drop.contains(&site) && inst.writes.iter().all(|w| !read_somewhere.contains(&w.1)) {
                drop.push(site);
            }
        }
        if drop.len() == before {
            break;
        }
    }
    for (b, block) in p.blocks.iter_mut().enumerate() {
        let mut i = 0;
        block.insts.retain(|_| {
            i += 1;
            !drop.contains(&(b, i - 1))
        });
    }
    let mut read = std::collections::HashSet::new();
    for block in &p.blocks {
        read.extend(block.phis.iter().flat_map(|phi| phi.args.iter().copied()));
        read.extend(block.insts.iter().flat_map(|i| i.reads.iter().map(|r| r.1)));
    }
    p.inputs
        .retain(|&(reg, v)| !matches!(reg, Reg::S(0 | 1)) || read.contains(&v));
    gone.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{ComputeIo, RootParam, RootType};

    fn param(root: RootType, user_data: u8, sgprs: u32, slot: u8) -> RootParam {
        RootParam {
            root,
            table_offset_dwords: 0,
            sgprs,
            base_register: 0,
            user_data,
            slot,
        }
    }

    fn native(words: &[u32], registers: u32, layout: Vec<RootParam>) -> NativeProgram {
        NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 256,
            layout,
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: registers << 1 | 1 << 7,
                threads: [1, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        }
    }

    const TABLE: [u32; 8] = [
        0x8703_FF01,
        0x0000_00FF,
        0xBE82_0300,
        0xF400_0141,
        0xFA00_0010,
        0xF404_0001,
        0xFA00_0000,
        0xBF81_0000,
    ];

    #[test]
    fn loads_of_the_spilled_table_become_entry_registers() {
        let n = native(
            &TABLE,
            4,
            vec![
                param(RootType::Descriptor, 16, 2, 0),
                param(RootType::Descriptor, 18, 2, 1),
                param(RootType::Constants, 20, 16, 2),
            ],
        );
        let mut p = ir::build(&n.code, false, 256).unwrap();
        assert_eq!(user_data(&mut p, &n), 2);
        let names: Vec<&str> = p
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .map(|i| i.name())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with("s_load_")), "{names:?}");
        let mut regs: Vec<Reg> = p.inputs.iter().map(|i| i.0).collect();
        regs.sort();
        assert_eq!(regs, [Reg::S(16), Reg::S(17), Reg::S(20)]);
    }

    #[test]
    fn parameters_in_registers_are_left_alone() {
        let n = native(&TABLE, 6, vec![param(RootType::Descriptor, 0, 2, 0)]);
        let mut p = ir::build(&n.code, false, 256).unwrap();
        let before = p.clone();
        assert_eq!(user_data(&mut p, &n), 0);
        assert_eq!(p, before);
    }

    #[test]
    fn a_used_dword_no_parameter_declares_keeps_its_load() {
        let mut words = TABLE.to_vec();
        words.insert(7, 0xBE86_0305);
        let n = native(&words, 4, vec![param(RootType::Descriptor, 16, 2, 0)]);
        let mut p = ir::build(&n.code, false, 256).unwrap();
        assert_eq!(user_data(&mut p, &n), 1);
        let loads = p
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|i| i.name().starts_with("s_load_"))
            .count();
        assert_eq!(loads, 1);
        assert!(p.inputs.iter().any(|i| i.0 == Reg::S(0)));
    }
}
