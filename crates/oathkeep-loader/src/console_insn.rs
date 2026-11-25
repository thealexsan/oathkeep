#[path = "console_insn_decode.rs"]
mod decode;

use decode::ConsoleOnly;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

const CONTINUE_EXECUTION: i32 = -1;

fn remembers() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_CLZERO_ASK").is_none_or(|v| v != "1"))
}

const ANSWERS: u32 = 4096;

thread_local! {
    static WRITABLE: Cell<([(usize, usize); 4], u32)> = const { Cell::new(([(0, 0); 4], 0)) };
    static CODE: Cell<[(usize, usize); 8]> = const { Cell::new([(0, 0); 8]) };
}

static EMULATED: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicU64 = AtomicU64::new(0);
static ZEROED: AtomicU64 = AtomicU64::new(0);
static DEMAND_RESERVED_END: std::sync::OnceLock<fn(usize) -> Option<usize>> =
    std::sync::OnceLock::new();

pub fn set_demand_reserved_end(hook: fn(usize) -> Option<usize>) {
    let _ = DEMAND_RESERVED_END.set(hook);
}

fn readable(va: usize, bytes: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    found
        && m.State == MEM_COMMIT
        && m.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0
        && m.BaseAddress as usize + m.RegionSize >= va + bytes
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Writable {
    Committed(usize),
    ZeroReserved(usize),
}

fn writable(va: usize, bytes: usize) -> Option<bool> {
    match writable_to(va, va + bytes)? {
        Writable::Committed(to) if to == va + bytes => Some(true),
        Writable::ZeroReserved(to) if to == va + bytes => Some(false),
        _ => None,
    }
}

fn writable_to(va: usize, end: usize) -> Option<Writable> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION,
        PAGE_EXECUTE_READWRITE, PAGE_READWRITE, VirtualQuery,
    };
    let remember = remembers();
    if remember {
        let (regions, answers) = WRITABLE.with(Cell::get);
        if let Some(&(_, to)) = regions.iter().find(|&&(from, to)| from <= va && va < to) {
            let next = if answers >= ANSWERS {
                ([(0, 0); 4], 0)
            } else {
                (regions, answers + 1)
            };
            WRITABLE.with(|w| w.set(next));
            return Some(Writable::Committed(to.min(end)));
        }
    }
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    if !found {
        return None;
    }
    let (from, to) = (
        m.BaseAddress as usize,
        m.BaseAddress as usize + m.RegionSize,
    );
    if m.State == MEM_RESERVE {
        if m.Type == MEM_MAPPED
            && m.AllocationProtect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) != 0
        {
            return Some(Writable::ZeroReserved(to.min(end)));
        }
        if m.Type == MEM_PRIVATE
            && let Some(hook) = DEMAND_RESERVED_END.get()
            && let Some(pool_end) = hook(va)
        {
            return Some(Writable::ZeroReserved(to.min(pool_end).min(end)));
        }
    }
    if m.State != MEM_COMMIT || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 {
        return None;
    }
    if remember {
        WRITABLE.with(|w| {
            let (mut regions, answers) = w.get();
            regions.rotate_right(1);
            regions[0] = (from, to);
            w.set((regions, answers));
        });
    }
    Some(Writable::Committed(to.min(end)))
}

pub(crate) unsafe fn emulate(ctx: &mut CONTEXT) -> i32 {
    let rip = ctx.Rip as usize;
    let known = if remembers() {
        CODE.with(Cell::get)
    } else {
        [(0, 0); 8]
    };
    let n = match known.iter().find(|&&(at, n)| at == rip && n > 0) {
        Some(&(_, n)) => n,
        None => {
            if !readable(rip, 1) {
                return 0;
            }
            let n = (1..=16).rev().find(|&n| readable(rip, n)).unwrap_or(0);
            if remembers() {
                let mut known = known;
                known.rotate_right(1);
                known[0] = (rip, n);
                CODE.with(|c| c.set(known));
            }
            n
        }
    };
    // SAFETY: `n` bytes at `rip` are readable.
    let code = unsafe { std::slice::from_raw_parts(rip as *const u8, n) };
    let Some((what, len)) = decode::decode(code) else {
        return 0;
    };
    static SEEN: std::sync::Mutex<Vec<std::mem::Discriminant<ConsoleOnly>>> =
        std::sync::Mutex::new(Vec::new());
    if let Ok(mut seen) = SEEN.lock()
        && !seen.contains(&std::mem::discriminant(&what))
    {
        seen.push(std::mem::discriminant(&what));
        if what != ConsoleOnly::Ud2 {
            tracing::warn!(instruction = ?what, at = format_args!("{rip:#x}"), "a console CPU instruction this CPU lacks: emulated (logged once)");
        }
    }
    match what {
        ConsoleOnly::Ud2 => return 0,
        ConsoleOnly::Clwb => {
            use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
            let mut old = 0u32;
            // SAFETY: the instruction's own bytes, made writable for the patch and restored.
            unsafe {
                if VirtualProtect(rip as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) != 0 {
                    std::slice::from_raw_parts_mut(rip as *mut u8, len).fill(0x90);
                    VirtualProtect(rip as *const _, len, old, &mut old);
                }
            }
        }
        ConsoleOnly::Monitorx => {}
        ConsoleOnly::Mwaitx => std::hint::spin_loop(),
        ConsoleOnly::Clzero => {
            let a32 = code[..len - 3].contains(&0x67);
            let line = (if a32 { ctx.Rax & 0xFFFF_FFFF } else { ctx.Rax } as usize) & !63;
            let Some(needs_write) = writable(line, 64) else {
                return 0;
            };
            if needs_write {
                // SAFETY: the 64-byte line is committed and writable.
                unsafe { std::ptr::write_bytes(line as *mut u8, 0, 64) };
            }
        }
        ConsoleOnly::ClzeroLoop { end_reg } => {
            let regs: [u64; 16] = [
                ctx.Rax, ctx.Rcx, ctx.Rdx, ctx.Rbx, ctx.Rsp, ctx.Rbp, ctx.Rsi, ctx.Rdi, ctx.R8,
                ctx.R9, ctx.R10, ctx.R11, ctx.R12, ctx.R13, ctx.R14, ctx.R15,
            ];
            let end = regs[end_reg as usize & 15] as usize;
            let start = ctx.Rax as usize;
            if end > start && end - start < 2 << 30 {
                let mut at = start;
                while at < end {
                    match writable_to(at, end) {
                        Some(Writable::Committed(to)) => {
                            // SAFETY: committed and writable from `at` to `to`.
                            unsafe { std::ptr::write_bytes(at as *mut u8, 0, to - at) };
                            at = to;
                        }
                        Some(Writable::ZeroReserved(to)) => {
                            at = to;
                        }
                        None => break,
                    }
                }
                if at == start {
                    return 0;
                }
                ZEROED.fetch_add((at - start) as u64, Relaxed);
                if at < end {
                    ctx.Rax = at as u64;
                    return CONTINUE_EXECUTION;
                }
            }
            ctx.Rax = end as u64;
        }
        ConsoleOnly::Rdpru => {
            let mut v = 0i64;
            // SAFETY: a valid out pointer.
            unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut v) };
            let v = if ctx.Rcx as u32 <= 1 { v as u64 } else { 0 };
            ctx.Rax = v & 0xFFFF_FFFF;
            ctx.Rdx = v >> 32;
        }
        ConsoleOnly::Rdpid(r) => {
            // SAFETY: no preconditions.
            let cpu = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() }
                as u64;
            let regs: [&mut u64; 16] = [
                &mut ctx.Rax,
                &mut ctx.Rcx,
                &mut ctx.Rdx,
                &mut ctx.Rbx,
                &mut ctx.Rsp,
                &mut ctx.Rbp,
                &mut ctx.Rsi,
                &mut ctx.Rdi,
                &mut ctx.R8,
                &mut ctx.R9,
                &mut ctx.R10,
                &mut ctx.R11,
                &mut ctx.R12,
                &mut ctx.R13,
                &mut ctx.R14,
                &mut ctx.R15,
            ];
            *regs[r as usize & 15] = cpu;
        }
        ConsoleOnly::Extrq { dst, src, len, idx } => {
            // SAFETY: the context's XMM registers (CONTEXT_FLOATING_POINT is captured for exceptions).
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let (len, idx) = match src {
                Some(s) => {
                    let c = x[s as usize & 15].Low;
                    (c as u8, (c >> 8) as u8)
                }
                None => (len, idx),
            };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::extrq(d.Low, len, idx);
            d.High = 0;
        }
        ConsoleOnly::Insertq {
            dst,
            src,
            reg,
            len,
            idx,
        } => {
            // SAFETY: as above.
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let s = x[src as usize & 15];
            let (len, idx) = if reg {
                (s.High as u8, (s.High >> 8) as u8)
            } else {
                (len, idx)
            };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::insertq(d.Low, s.Low, len, idx);
            d.High = 0;
        }
    }
    ctx.Rip += len as u64;
    let emulated = EMULATED.fetch_add(1, Relaxed) + 1;
    if emulated >= 1 << 16 && emulated.is_power_of_two() {
        tracing::info!(
            emulated,
            asked = ASKED.load(Relaxed),
            zeroed_mb = ZEROED.load(Relaxed) >> 20,
            "console CPU instructions emulated so far, the times Windows was asked about memory for them, and what clzero loops zeroed"
        );
    }
    CONTINUE_EXECUTION
}
