//! The stores of x86-64 code that [`super::store`] does in the title's place:
//! moves from a register or a constant, arithmetic of those with memory
//! (locked or not), exchanges, and the 4, 8, 16 and 32 byte stores of the
//! vector registers. Everything else is `None`.

use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

mod exec;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Alu {
    Add,
    Or,
    And,
    Sub,
    Xor,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Op {
    /// These bytes (the low `size` of them).
    Put([u8; 32]),
    /// With a value; `keeps` carry (`inc`, `dec`).
    Alu { alu: Alu, with: u64, keeps: bool },
    /// With a register, which gets what was there.
    Xchg(Register),
    Xadd(Register),
    Cmpxchg(Register),
}

/// A general register, or (`high`) the second byte of one of the first four.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Register {
    pub(super) number: usize,
    pub(super) high: bool,
}

pub(super) struct Store {
    /// Where it stores, and how many bytes.
    pub(super) at: usize,
    pub(super) size: usize,
    /// The instruction's length.
    pub(super) len: usize,
    pub(super) op: Op,
    pub(super) locked: bool,
}

pub(super) fn general(c: &CONTEXT, number: usize) -> u64 {
    match number {
        0 => c.Rax,
        1 => c.Rcx,
        2 => c.Rdx,
        3 => c.Rbx,
        4 => c.Rsp,
        5 => c.Rbp,
        6 => c.Rsi,
        7 => c.Rdi,
        8 => c.R8,
        9 => c.R9,
        10 => c.R10,
        11 => c.R11,
        12 => c.R12,
        13 => c.R13,
        14 => c.R14,
        _ => c.R15,
    }
}

pub(super) fn general_mut(c: &mut CONTEXT, number: usize) -> &mut u64 {
    match number {
        0 => &mut c.Rax,
        1 => &mut c.Rcx,
        2 => &mut c.Rdx,
        3 => &mut c.Rbx,
        4 => &mut c.Rsp,
        5 => &mut c.Rbp,
        6 => &mut c.Rsi,
        7 => &mut c.Rdi,
        8 => &mut c.R8,
        9 => &mut c.R9,
        10 => &mut c.R10,
        11 => &mut c.R11,
        12 => &mut c.R12,
        13 => &mut c.R13,
        14 => &mut c.R14,
        _ => &mut c.R15,
    }
}

impl Register {
    /// Operand `number` of `size` bytes, with or without a REX prefix.
    pub(super) fn of(number: usize, size: usize, rex: bool) -> Register {
        if size == 1 && !rex && (4..8).contains(&number) {
            Register { number: number - 4, high: true }
        } else {
            Register { number, high: false }
        }
    }

    pub(super) fn get(self, c: &CONTEXT) -> u64 {
        general(c, self.number) >> if self.high { 8 } else { 0 }
    }

    /// As an instruction writes it: 32 bits clear the rest, fewer leave it.
    pub(super) fn put(self, c: &mut CONTEXT, value: u64, size: usize) {
        let register = general_mut(c, self.number);
        let shift = if self.high { 8 } else { 0 };
        *register = match size {
            8 => value,
            4 => value & 0xffff_ffff,
            _ => *register & !(mask(size) << shift) | (value & mask(size)) << shift,
        };
    }
}

pub(super) fn mask(size: usize) -> u64 {
    if size >= 8 { u64::MAX } else { (1 << (size * 8)) - 1 }
}

/// The upper half of vector register `reg`, where the context has it; nothing
/// in it if the thread used none.
fn upper(c: &CONTEXT, reg: usize) -> Option<[u8; 16]> {
    use windows_sys::Win32::System::Diagnostics::Debug::{GetXStateFeaturesMask, LocateXStateFeature};
    const AVX: u32 = 2;
    let mut mask = 0;
    // SAFETY: a context as the system passes it to a handler (the calls
    // change nothing of it).
    unsafe {
        if GetXStateFeaturesMask(c, &mut mask) == 0 {
            return None;
        }
        if mask >> AVX & 1 == 0 {
            return Some([0; 16]);
        }
        let mut len = 0;
        let at = LocateXStateFeature(c, AVX, &mut len) as *const [u8; 16];
        (!at.is_null() && len as usize >= 16 * 16).then(|| at.add(reg).read_unaligned())
    }
}

/// The store at the start of `code`, which the registers are those of.
pub(super) fn store(code: &[u8], c: &CONTEXT) -> Option<Store> {
    let mut at = 0;
    let mut next = || {
        let byte = code.get(at).copied();
        at += 1;
        byte
    };
    let (mut locked, mut narrow, mut repeat, mut long) = (false, false, 0u8, false);
    let mut byte = next()?;
    loop {
        match byte {
            0xf0 => locked = true,
            0x66 => narrow = true,
            0xf2 | 0xf3 => repeat = byte,
            // (Of data and stack segments nothing is used; branch hints.)
            0x2e | 0x36 | 0x3e | 0x26 => {}
            _ => break,
        }
        byte = next()?;
    }
    let mut rex = 0u8;
    if byte & 0xf0 == 0x40 {
        rex = byte;
        byte = next()?;
    }
    // The vector prefixes, with their own forms of the others.
    let vector = match byte {
        0xc5 if rex == 0 => {
            let b = next()?;
            rex = if b & 0x80 == 0 { 0x44 } else { 0x40 };
            Some(b)
        }
        0xc4 if rex == 0 => {
            let (b, d) = (next()?, next()?);
            if b & 0x1f != 1 {
                return None;
            }
            rex = 0x40 | (!b >> 5 & 7) | (d >> 4 & 8);
            Some(d)
        }
        _ => None,
    };
    if let Some(b) = vector {
        if narrow || repeat != 0 || locked {
            return None;
        }
        long = b & 4 != 0;
        // (A store has no third register.)
        if b >> 3 & 15 != 15 {
            return None;
        }
        (narrow, repeat) = match b & 3 {
            0 => (false, 0),
            1 => (true, 0),
            2 => (false, 0xf3),
            _ => (false, 0xf2),
        };
        byte = next()?;
    }
    let two = vector.is_some() || byte == 0x0f;
    if two && vector.is_none() {
        byte = next()?;
    }
    let wide = rex & 8 != 0;
    let modrm = next()?;
    let (mode, reg, rm) = (modrm >> 6, (modrm >> 3 & 7) as usize | (rex as usize & 4) << 1, modrm & 7);
    if mode == 3 {
        return None;
    }
    // The address, all but what hangs on the instruction's length.
    let mut address = 0u64;
    let mut on_rip = false;
    let mut base = Some(rm as usize | (rex as usize & 1) << 3);
    if rm == 4 {
        let sib = next()?;
        let index = (sib >> 3 & 7) as usize | (rex as usize & 2) << 2;
        if index != 4 {
            address = general(c, index) << (sib >> 6);
        }
        base = Some((sib & 7) as usize | (rex as usize & 1) << 3);
        if sib & 7 == 5 && mode == 0 {
            base = None;
        }
    } else if rm == 5 && mode == 0 {
        base = None;
        on_rip = true;
    }
    if let Some(base) = base {
        address = address.wrapping_add(general(c, base));
    }
    let displacement = match mode {
        1 => i64::from(next()? as i8),
        2 => i64::from(i32::from_le_bytes([next()?, next()?, next()?, next()?])),
        _ if base.is_none() => i64::from(i32::from_le_bytes([next()?, next()?, next()?, next()?])),
        _ => 0,
    };
    address = address.wrapping_add(displacement as u64);

    let words = if wide {
        8
    } else if narrow {
        2
    } else {
        4
    };
    let put = |value: u64| {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        Op::Put(bytes)
    };
    let of = |size: usize| Register::of(reg, size, rex != 0);
    let vector_bytes = || {
        // SAFETY: the registers, whichever way they are named.
        let x = unsafe { &c.Anonymous.FltSave }.XmmRegisters[reg];
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&x.Low.to_le_bytes());
        bytes[8..16].copy_from_slice(&x.High.to_le_bytes());
        bytes
    };
    let alu_of = |kind: u8| match kind {
        0 => Some(Alu::Add),
        1 => Some(Alu::Or),
        4 => Some(Alu::And),
        5 => Some(Alu::Sub),
        6 => Some(Alu::Xor),
        _ => None,
    };
    let (op, size) = if long {
        if !matches!((byte, narrow, repeat), (0x11 | 0x29, _, 0) | (0x7f, true, 0) | (0x7f, false, 0xf3) | (0xe7, true, 0)) {
            return None;
        }
        let mut bytes = vector_bytes();
        bytes[16..].copy_from_slice(&upper(c, reg)?);
        (Op::Put(bytes), 32)
    } else if two {
        match (byte, narrow, repeat) {
            (0x11, _, 0) | (0x29, _, 0) | (0x7f, true, 0) | (0x7f, false, 0xf3) | (0xe7, true, 0) => (Op::Put(vector_bytes()), 16),
            (0x11, false, 0xf3) => (Op::Put(vector_bytes()), 4),
            (0x11, false, 0xf2) | (0x13, _, 0) | (0xd6, true, 0) => (Op::Put(vector_bytes()), 8),
            (0x17, _, 0) => {
                let mut bytes = vector_bytes();
                bytes.copy_within(8..16, 0);
                (Op::Put(bytes), 8)
            }
            (0x7e, true, 0) => (Op::Put(vector_bytes()), if wide { 8 } else { 4 }),
            _ if vector.is_some() || repeat != 0 => return None,
            (0xc3, false, _) => (put(of(words).get(c)), words),
            (0xb0 | 0xb1, _, _) => {
                let size = if byte & 1 == 0 { 1 } else { words };
                (Op::Cmpxchg(of(size)), size)
            }
            (0xc0 | 0xc1, _, _) => {
                let size = if byte & 1 == 0 { 1 } else { words };
                (Op::Xadd(of(size)), size)
            }
            _ => return None,
        }
    } else {
        if repeat != 0 {
            return None;
        }
        let size = if byte & 1 == 0 { 1 } else { words };
        let mut constant = |bytes: usize| -> Option<u64> {
            Some(match bytes {
                1 => next()? as i8 as i64 as u64,
                2 => i64::from(i16::from_le_bytes([next()?, next()?])) as u64,
                _ => i64::from(i32::from_le_bytes([next()?, next()?, next()?, next()?])) as u64,
            })
        };
        match byte {
            0x88 | 0x89 => (put(of(size).get(c)), size),
            0xc6 | 0xc7 if reg & 7 == 0 => (put(constant(size)?), size),
            0x00 | 0x01 | 0x08 | 0x09 | 0x20 | 0x21 | 0x28 | 0x29 | 0x30 | 0x31 => {
                (Op::Alu { alu: alu_of(byte >> 3)?, with: of(size).get(c), keeps: false }, size)
            }
            0x80 | 0x81 | 0x83 => {
                let alu = alu_of(reg as u8 & 7)?;
                (Op::Alu { alu, with: constant(if byte == 0x81 { size } else { 1 })?, keeps: false }, size)
            }
            0x86 | 0x87 => {
                locked = true;
                (Op::Xchg(of(size)), size)
            }
            0xfe | 0xff if reg & 7 < 2 => {
                (Op::Alu { alu: if reg & 7 == 0 { Alu::Add } else { Alu::Sub }, with: 1, keeps: true }, size)
            }
            _ => return None,
        }
    };
    if locked && matches!(op, Op::Put(_)) {
        return None;
    }
    // What is read of memory decides nothing but what is stored: no
    // register but the operand is written.
    if on_rip {
        address = address.wrapping_add(c.Rip).wrapping_add(at as u64);
    }
    Some(Store { at: address as usize, size, len: at, op, locked })
}
