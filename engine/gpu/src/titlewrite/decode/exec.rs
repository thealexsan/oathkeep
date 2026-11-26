//! Execution and comparison of decoded x86-64 store instructions.

use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering::SeqCst};
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

use super::{Alu, Op, Register, Store, mask};

pub(super) const CF: u32 = 1;
pub(super) const PF: u32 = 4;
const AF: u32 = 0x10;
pub(super) const ZF: u32 = 0x40;
pub(super) const SF: u32 = 0x80;
pub(super) const OF: u32 = 0x800;

/// The result of an operation on `size` bytes and the flags it leaves.
fn arithmetic(alu: Alu, a: u64, b: u64, size: usize, keeps: bool, flags: u32) -> (u64, u32) {
    let m = mask(size);
    let (a, b) = (a & m, b & m);
    let top = |v: u64| v >> (size * 8 - 1) & 1 != 0;
    let r = match alu {
        Alu::Add => a.wrapping_add(b),
        Alu::Or => a | b,
        Alu::And => a & b,
        Alu::Sub => a.wrapping_sub(b),
        Alu::Xor => a ^ b,
    } & m;
    let mut out = flags & !(CF | PF | AF | ZF | SF | OF);
    let mut flag = |set: bool, flag: u32| out |= if set { flag } else { 0 };
    flag(r == 0, ZF);
    flag(top(r), SF);
    flag((r as u8).count_ones().is_multiple_of(2), PF);
    match alu {
        Alu::Add => {
            flag(r < a, CF);
            flag(top((a ^ r) & (b ^ r)), OF);
            flag((a ^ b ^ r) & 16 != 0, AF);
        }
        Alu::Sub => {
            flag(a < b, CF);
            flag(top((a ^ b) & (a ^ r)), OF);
            flag((a ^ b ^ r) & 16 != 0, AF);
        }
        _ => {}
    }
    if keeps {
        out = out & !CF | flags & CF;
    }
    (r, out)
}

/// A value of `size` bytes in memory, changed by `f` as one if `locked`: what
/// was there (`None`: not aligned).
///
/// # Safety
/// `size` bytes can be read and written at `at`.
unsafe fn change(at: usize, size: usize, locked: bool, f: impl Fn(u64) -> u64) -> Option<u64> {
    if !locked {
        let mut bytes = [0u8; 8];
        // SAFETY (both): by the caller.
        unsafe { std::ptr::copy_nonoverlapping(at as *const u8, bytes.as_mut_ptr(), size) };
        let was = u64::from_le_bytes(bytes);
        unsafe { std::ptr::copy_nonoverlapping(f(was).to_le_bytes().as_ptr(), at as *mut u8, size) };
        return Some(was);
    }
    if !at.is_multiple_of(size) {
        return None;
    }
    // SAFETY (all): by the caller, and aligned.
    Some(unsafe {
        match size {
            1 => u64::from(AtomicU8::from_ptr(at as *mut _).fetch_update(SeqCst, SeqCst, |v| Some(f(v.into()) as u8)).ok()?),
            2 => u64::from(AtomicU16::from_ptr(at as *mut _).fetch_update(SeqCst, SeqCst, |v| Some(f(v.into()) as u16)).ok()?),
            4 => u64::from(AtomicU32::from_ptr(at as *mut _).fetch_update(SeqCst, SeqCst, |v| Some(f(v.into()) as u32)).ok()?),
            _ => AtomicU64::from_ptr(at as *mut _).fetch_update(SeqCst, SeqCst, |v| Some(f(v))).ok()?,
        }
    })
}

impl Store {
    /// Whether it leaves the memory as it is, and what it leaves in the flags
    /// then; `None` for those of which that is not asked (locked ones,
    /// exchanges).
    ///
    /// # Safety
    /// [`Store::size`] bytes can be read at `at`.
    pub(in crate::titlewrite) unsafe fn same(&self, c: &CONTEXT) -> Option<(bool, u32)> {
        if self.locked {
            return None;
        }
        let mut there = [0u8; 32];
        // SAFETY: by the caller (32 bytes at the most).
        unsafe { std::ptr::copy_nonoverlapping(self.at as *const u8, there.as_mut_ptr(), self.size) };
        match self.op {
            Op::Put(bytes) => Some((there[..self.size] == bytes[..self.size], c.EFlags)),
            Op::Alu { alu, with, keeps } => {
                let was = u64::from_le_bytes(there[..8].try_into().expect("eight bytes")) & mask(self.size);
                let (value, flags) = arithmetic(alu, was, with, self.size, keeps, c.EFlags);
                Some((value == was, flags))
            }
            _ => None,
        }
    }

    /// Whether the loader may make it look before it writes
    /// (`oathkeep_loader::write_sites`, which looks itself): an `or` of a byte
    /// with a constant, a move of 4 bytes or more; of the five bytes a jump
    /// needs.
    pub(in crate::titlewrite) fn can_look(&self, code: &[u8]) -> bool {
        let at = usize::from(code[0] & 0xf0 == 0x40);
        self.len >= 5
            && match self.op {
                Op::Alu { alu: Alu::Or, .. } => code[at] == 0x80,
                Op::Put(_) => matches!(code[0], 0xc4 | 0xc5) && self.size >= 16 || matches!(code[at], 0x89 | 0xc7),
                _ => false,
            }
    }

    /// Does it, with `to` for the address, and gives the registers what the
    /// instruction leaves in them (but the instruction pointer). `false`:
    /// nothing was done.
    ///
    /// # Safety
    /// [`Store::size`] bytes can be read and written at `to`.
    pub(in crate::titlewrite) unsafe fn run(&self, c: &mut CONTEXT, to: usize) -> bool {
        let (size, locked, m) = (self.size, self.locked, mask(self.size));
        // SAFETY (all `change`s): by the caller.
        match self.op {
            Op::Put(bytes) => {
                // As one, for those the processor stores as one.
                // SAFETY (all): by the caller, aligned where it must be.
                unsafe {
                    let low = u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]);
                    match size {
                        1 => (to as *mut u8).write_volatile(bytes[0]),
                        2 if to.is_multiple_of(2) => (to as *mut u16).write_volatile(low as u16),
                        4 if to.is_multiple_of(4) => (to as *mut u32).write_volatile(low as u32),
                        8 if to.is_multiple_of(8) => (to as *mut u64).write_volatile(low),
                        _ => std::ptr::copy_nonoverlapping(bytes.as_ptr(), to as *mut u8, size),
                    }
                }
            }
            Op::Alu { alu, with, keeps } => {
                let flags = c.EFlags;
                let Some(was) = (unsafe { change(to, size, locked, |v| arithmetic(alu, v, with, size, keeps, flags).0) }) else {
                    return false;
                };
                c.EFlags = arithmetic(alu, was, with, size, keeps, flags).1;
            }
            Op::Xchg(register) => {
                let value = register.get(c) & m;
                let Some(was) = (unsafe { change(to, size, true, |_| value) }) else {
                    return false;
                };
                register.put(c, was & m, size);
            }
            Op::Xadd(register) => {
                let value = register.get(c);
                let Some(was) = (unsafe { change(to, size, locked, |v| v.wrapping_add(value) & m) }) else {
                    return false;
                };
                c.EFlags = arithmetic(Alu::Add, was, value, size, false, c.EFlags).1;
                register.put(c, was & m, size);
            }
            Op::Cmpxchg(register) => {
                let (value, expected) = (register.get(c) & m, c.Rax & m);
                let Some(was) = (unsafe { change(to, size, locked, |v| if v & m == expected { value } else { v }) }) else {
                    return false;
                };
                let was = was & m;
                c.EFlags = arithmetic(Alu::Sub, expected, was, size, false, c.EFlags).1;
                if was != expected {
                    Register { number: 0, high: false }.put(c, was, size);
                }
            }
        }
        true
    }
}
