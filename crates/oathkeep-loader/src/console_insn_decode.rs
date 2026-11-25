#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleOnly {
    Clwb,
    Clzero,
    ClzeroLoop {
        end_reg: u8,
    },
    Rdpru,
    Monitorx,
    Mwaitx,
    Rdpid(u8),
    Extrq {
        dst: u8,
        src: Option<u8>,
        len: u8,
        idx: u8,
    },
    Insertq {
        dst: u8,
        src: u8,
        reg: bool,
        len: u8,
        idx: u8,
    },
    Ud2,
}

fn field_len(len: u8) -> u32 {
    match len & 63 {
        0 => 64,
        n => n as u32,
    }
}

pub fn extrq(v: u64, len: u8, idx: u8) -> u64 {
    let (len, idx) = (field_len(len), (idx & 63) as u32);
    let field = v.checked_shr(idx).unwrap_or(0);
    if len >= 64 {
        field
    } else {
        field & ((1u64 << len) - 1)
    }
}

pub fn insertq(dst: u64, src: u64, len: u8, idx: u8) -> u64 {
    let (len, idx) = (field_len(len), (idx & 63) as u32);
    let ones = if len >= 64 {
        u64::MAX
    } else {
        (1u64 << len) - 1
    };
    let mask = ones.checked_shl(idx).unwrap_or(0);
    (dst & !mask) | ((src & ones).checked_shl(idx).unwrap_or(0) & mask)
}

fn modrm_len(b: &[u8]) -> Option<usize> {
    let modrm = *b.first()?;
    let (md, rm) = (modrm >> 6, modrm & 7);
    if md == 3 {
        return Some(1);
    }
    let mut len = 1;
    let mut base = rm;
    if rm == 4 {
        base = *b.get(1)? & 7;
        len += 1;
    }
    len += match md {
        0 if base == 5 => 4,
        0 => 0,
        1 => 1,
        _ => 4,
    };
    (b.len() >= len).then_some(len)
}

pub fn decode(code: &[u8]) -> Option<(ConsoleOnly, usize)> {
    let mut i = 0;
    let (mut p66, mut pf3, mut rep, mut rex) = (false, false, 0u8, 0u8);
    while let Some(&b) = code.get(i) {
        match b {
            0x66 => p66 = true,
            0xF2 => rep = 0xF2,
            0xF3 => {
                pf3 = true;
                rep = 0xF3;
            }
            0x2E | 0x3E | 0x26 | 0x36 | 0x64 | 0x65 | 0x67 => {}
            0x40..=0x4F => {
                rex = b;
                i += 1;
                continue;
            }
            _ => break,
        }
        rex = 0;
        i += 1;
    }
    if *code.get(i)? != 0x0F {
        return None;
    }
    match (*code.get(i + 1)?, code.get(i + 2).copied()) {
        (0x01, Some(0xFC)) => {
            if code.len() >= i + 12
                && !code[..i].contains(&0x67)
                && code[i + 3..i + 7] == [0x48, 0x83, 0xC0, 0x40]
                && (code[i + 7] == 0x48 || code[i + 7] == 0x4C)
                && code[i + 8] == 0x39
                && code[i + 10..i + 12] == [0x72, 0xF4]
            {
                let rex_r = code[i + 7] == 0x4C;
                let modrm = code[i + 9];
                let reg_idx = ((modrm >> 3) & 7) | if rex_r { 8 } else { 0 };
                Some((ConsoleOnly::ClzeroLoop { end_reg: reg_idx }, i + 12))
            } else {
                Some((ConsoleOnly::Clzero, i + 3))
            }
        }
        (0x01, Some(0xFD)) => Some((ConsoleOnly::Rdpru, i + 3)),
        (0x01, Some(0xFA)) => Some((ConsoleOnly::Monitorx, i + 3)),
        (0x01, Some(0xFB)) => Some((ConsoleOnly::Mwaitx, i + 3)),
        (0x0B, _) => Some((ConsoleOnly::Ud2, i + 2)),
        (0x78 | 0x79, Some(m)) if (rep == 0xF2 || (rep == 0 && p66)) && m >> 6 == 3 => {
            let reg = ((m >> 3) & 7) | (rex & 4) << 1;
            let rm = (m & 7) | (rex & 1) << 3;
            let imm = |k: usize| code.get(i + 3 + k).copied();
            match (code[i + 1], rep == 0xF2) {
                (0x78, false) if (m >> 3) & 7 == 0 => Some((
                    ConsoleOnly::Extrq {
                        dst: rm,
                        src: None,
                        len: imm(0)?,
                        idx: imm(1)?,
                    },
                    i + 5,
                )),
                (0x79, false) => Some((
                    ConsoleOnly::Extrq {
                        dst: reg,
                        src: Some(rm),
                        len: 0,
                        idx: 0,
                    },
                    i + 3,
                )),
                (0x78, true) => Some((
                    ConsoleOnly::Insertq {
                        dst: reg,
                        src: rm,
                        reg: false,
                        len: imm(0)?,
                        idx: imm(1)?,
                    },
                    i + 5,
                )),
                (0x79, true) => Some((
                    ConsoleOnly::Insertq {
                        dst: reg,
                        src: rm,
                        reg: true,
                        len: 0,
                        idx: 0,
                    },
                    i + 3,
                )),
                _ => None,
            }
        }
        (0xAE, Some(m)) if p66 && (m >> 3) & 7 == 6 && m >> 6 != 3 => {
            Some((ConsoleOnly::Clwb, i + 2 + modrm_len(&code[i + 2..])?))
        }
        (0xC7, Some(m)) if pf3 && (m >> 3) & 7 == 7 && m >> 6 == 3 => {
            Some((ConsoleOnly::Rdpid((m & 7) | (rex & 1) << 3), i + 3))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse4a_bit_fields() {
        assert_eq!(extrq(0x1234_5678_9ABC_DEF0, 8, 16), 0xBC);
        assert_eq!(extrq(0x1234_5678_9ABC_DEF0, 0, 0), 0x1234_5678_9ABC_DEF0);
        assert_eq!(extrq(0x8000_0000_0000_0000, 1, 63), 1);
        assert_eq!(
            insertq(0xFFFF_FFFF_FFFF_FFFF, 0xAB, 8, 8),
            0xFFFF_FFFF_FFFF_ABFF
        );
        assert_eq!(insertq(0, 0x1FF, 4, 60), 0xF000_0000_0000_0000);
        assert_eq!(
            insertq(0x1111, 0xDEAD_BEEF_0000_0000, 0, 0),
            0xDEAD_BEEF_0000_0000
        );
        for (v, len, idx) in [
            (0x0123_4567_89AB_CDEFu64, 12u8, 20u8),
            (u64::MAX, 63, 1),
            (0x55, 3, 0),
        ] {
            assert_eq!(extrq(insertq(0, v, len, idx), len, idx), extrq(v, len, 0));
        }
    }

    #[test]
    fn decodes_the_console_only_instructions() {
        assert_eq!(decode(&[0x0F, 0x01, 0xFC]), Some((ConsoleOnly::Clzero, 3)));
        assert_eq!(
            decode(&[
                0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x4C, 0x39, 0xD0, 0x72, 0xF4
            ]),
            Some((ConsoleOnly::ClzeroLoop { end_reg: 10 }, 12))
        );
        assert_eq!(
            decode(&[
                0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x48, 0x39, 0xD0, 0x72, 0xF4
            ]),
            Some((ConsoleOnly::ClzeroLoop { end_reg: 2 }, 12))
        );
        assert_eq!(
            decode(&[
                0x67, 0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x48, 0x39, 0xD0, 0x72, 0xF4
            ]),
            Some((ConsoleOnly::Clzero, 4))
        );
        assert_eq!(decode(&[0x0F, 0x01, 0xFD]), Some((ConsoleOnly::Rdpru, 3)));
        assert_eq!(
            decode(&[0x0F, 0x01, 0xFA]),
            Some((ConsoleOnly::Monitorx, 3))
        );
        assert_eq!(decode(&[0x0F, 0x01, 0xFB]), Some((ConsoleOnly::Mwaitx, 3)));
        assert_eq!(decode(&[0x0F, 0x0B]), Some((ConsoleOnly::Ud2, 2)));
        assert_eq!(
            decode(&[0x66, 0x0F, 0xAE, 0x30]),
            Some((ConsoleOnly::Clwb, 4))
        );
        assert_eq!(
            decode(&[0x66, 0x0F, 0xAE, 0x71, 0x40]),
            Some((ConsoleOnly::Clwb, 5))
        );
        assert_eq!(
            decode(&[0x66, 0x0F, 0xAE, 0x74, 0x24, 0x08]),
            Some((ConsoleOnly::Clwb, 6))
        );
        assert_eq!(
            decode(&[0x66, 0x0F, 0xAE, 0x35, 1, 2, 3, 4]),
            Some((ConsoleOnly::Clwb, 8))
        );
        assert_eq!(
            decode(&[0x66, 0x41, 0x0F, 0xAE, 0x30]),
            Some((ConsoleOnly::Clwb, 5))
        );
        assert_eq!(
            decode(&[0xF3, 0x0F, 0xC7, 0xF8]),
            Some((ConsoleOnly::Rdpid(0), 4))
        );
        assert_eq!(
            decode(&[0xF3, 0x41, 0x0F, 0xC7, 0xF9]),
            Some((ConsoleOnly::Rdpid(9), 5))
        );
        assert_eq!(
            decode(&[0xF2, 0x0F, 0x78, 0xC1, 0x08, 0x10]),
            Some((
                ConsoleOnly::Insertq {
                    dst: 0,
                    src: 1,
                    reg: false,
                    len: 8,
                    idx: 16
                },
                6
            ))
        );
        assert_eq!(
            decode(&[0xF2, 0x45, 0x0F, 0x79, 0xC1]),
            Some((
                ConsoleOnly::Insertq {
                    dst: 8,
                    src: 9,
                    reg: true,
                    len: 0,
                    idx: 0
                },
                5
            ))
        );
        assert_eq!(
            decode(&[0x66, 0x0F, 0x78, 0xC2, 0x04, 0x08]),
            Some((
                ConsoleOnly::Extrq {
                    dst: 2,
                    src: None,
                    len: 4,
                    idx: 8
                },
                6
            ))
        );
        assert_eq!(
            decode(&[0x66, 0x0F, 0x79, 0xCA]),
            Some((
                ConsoleOnly::Extrq {
                    dst: 1,
                    src: Some(2),
                    len: 0,
                    idx: 0
                },
                4
            ))
        );
        assert_eq!(
            decode(&[0xF2, 0x0F, 0x78, 0xC1, 0x08]),
            None,
            "an immediate missing"
        );
        assert_eq!(
            decode(&[0x0F, 0x78, 0xC1, 0x08, 0x10]),
            None,
            "VMREAD without a prefix is not SSE4a"
        );
        let insertq = Some((
            ConsoleOnly::Insertq {
                dst: 0,
                src: 1,
                reg: false,
                len: 16,
                idx: 16,
            },
            7,
        ));
        for code in [
            [0x66, 0xF2, 0x0F, 0x78, 0xC1, 0x10, 0x10],
            [0xF2, 0x66, 0x0F, 0x78, 0xC1, 0x10, 0x10],
            [0xF3, 0xF2, 0x0F, 0x78, 0xC1, 0x10, 0x10],
            [0x67, 0xF2, 0x0F, 0x78, 0xC1, 0x10, 0x10],
            [0x41, 0xF2, 0x0F, 0x78, 0xC1, 0x10, 0x10],
        ] {
            assert_eq!(decode(&code), insertq, "{code:02x?}");
        }
        for code in [
            [0xF2, 0xF3, 0x0F, 0x78, 0xC1, 0x10, 0x10],
            [0xF3, 0x66, 0x0F, 0x78, 0xC1, 0x10, 0x10],
            [0x66, 0xF3, 0x0F, 0x78, 0xC0, 0x10, 0x10],
        ] {
            assert_eq!(decode(&code), None, "#UD on the CPU: {code:02x?}");
        }
        assert_eq!(
            decode(&[0x67, 0x0F, 0x01, 0xFC]),
            Some((ConsoleOnly::Clzero, 4))
        );
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x38]), None);
        assert_eq!(decode(&[0x0F, 0xAE, 0x30]), None);
        assert_eq!(decode(&[0x48, 0x8B, 0x01]), None);
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x74, 0x24]), None);
        assert_eq!(decode(&[0x0F]), None);
    }
}
