use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{
    EXCEPTION_ACCESS_VIOLATION, EXCEPTION_BREAKPOINT, EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_PRIV_INSTRUCTION, EXCEPTION_STACK_OVERFLOW, HMODULE,
};

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, EXCEPTION_POINTERS, M128A, RtlLookupFunctionEntry,
    RtlVirtualUnwind, SetUnhandledExceptionFilter, UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits;

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
pub(crate) const MAX_FRAMES: usize = 40;

#[cfg(feature = "diagnostics")]
static SAVE: std::sync::OnceLock<Vec<(usize, usize, std::path::PathBuf)>> =
    std::sync::OnceLock::new();

#[cfg(feature = "diagnostics")]
pub fn save_on_crash(ranges: Vec<(usize, usize, std::path::PathBuf)>) {
    let _ = SAVE.set(ranges);
}

static LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn log_crashes_to(file: std::path::PathBuf) {
    if LOG.set(file).is_ok() {
        let print = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            print(info);
            append_to_log(&format!(
                "Oathkeep: panic on thread {:?}: {info}\n{}",
                std::thread::current().name().unwrap_or("?"),
                title_output()
            ));
        }));
    }
}

pub fn log_crash(text: &str) {
    let text = format!(
        "{text}
{}",
        title_output()
    );
    eprint!("{text}");
    append_to_log(&text);
}

fn append_to_log(text: &str) {
    use std::io::Write;
    let Some(file) = LOG.get() else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file);
    if let Ok(mut f) = opened {
        let _ = write!(
            f,
            "unix time {now}, process {}\n{text}\n",
            std::process::id()
        );
    }
}

static SAID: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const SAID_LINES: usize = 8;

pub fn note_title_output(line: &str) {
    if line.is_empty() {
        return;
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() == SAID_LINES {
        said.pop_front();
    }
    said.push_back(line.to_owned());
}

fn title_output() -> String {
    let Ok(said) = SAID.try_lock() else {
        return String::new();
    };
    said.iter()
        .map(|l| format!("  title said: {l}\n"))
        .collect()
}

pub fn report_crashes() {
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        unsafe { report_unhandled(info) };
        EXCEPTION_CONTINUE_SEARCH
    }
    // SAFETY: registers handlers that only read the exception they are
    // given; the title's own filter, once it sets one, takes the place of
    // ours and calls `report_unhandled` first.
    unsafe {
        AddVectoredExceptionHandler(0, Some(handler));
        SetUnhandledExceptionFilter(Some(filter));
    }
}

const FATAL: [i32; 6] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO,
    EXCEPTION_STACK_OVERFLOW,
    EXCEPTION_BREAKPOINT,
];

unsafe fn emulate_amd_only(info: *mut EXCEPTION_POINTERS) -> bool {
    // SAFETY: the system passes valid exception pointers; the faulting
    // instruction's bytes are readable, as the CPU just fetched them.
    unsafe {
        let record = &*(*info).ExceptionRecord;
        let context = &mut *(*info).ContextRecord;
        if record.ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION {
            return false;
        }
        let code = context.Rip as *const u8;
        match code.cast::<[u8; 3]>().read_unaligned() {
            [0x0f, 0x01, 0xfc] => ((context.Rax & !63) as *mut u8).write_bytes(0, 64),
            [0x0f, 0x01, 0xfa] => {}
            [0x0f, 0x01, 0xfb] => std::thread::yield_now(),
            _ => {
                let room = 0x1000 - (context.Rip as usize & 0xfff);
                let mut bytes = [0u8; 32];
                let n = room.min(bytes.len());
                std::ptr::copy_nonoverlapping(code, bytes.as_mut_ptr(), n);
                if let Some(pass) = clwb_loop(&bytes[..n]) {
                    let limit = *gpr(context, pass.limit);
                    let base = gpr(context, pass.base);
                    *base = if *base <= limit {
                        base.wrapping_add(((limit - *base) / 64 + 1) * 64)
                    } else {
                        base.wrapping_add(64)
                    };
                    let flags = cmp_flags(*base, limit);
                    context.EFlags = context.EFlags & !CMP_FLAGS | flags;
                    context.Rip += pass.len as u64;
                    return true;
                }
                if n >= 15
                    && let Some(len) = clwb_len(bytes[..15].try_into().unwrap_or([0; 15]))
                {
                    context.Rip += len as u64;
                    return true;
                }
                let Some(len) = sse4a(code, &mut context.Anonymous.FltSave.XmmRegisters) else {
                    return false;
                };
                context.Rip += len as u64;
                return true;
            }
        }
        context.Rip += 3;
    }
    true
}

fn clwb_len(code: [u8; 15]) -> Option<usize> {
    let at = if code[1] & 0xf0 == 0x40 { 2 } else { 1 };
    if code[0] != 0x66 || code[at] != 0x0f || code[at + 1] != 0xae {
        return None;
    }
    let modrm = code[at + 2];
    let (mode, rm) = (modrm >> 6, modrm & 7);
    if mode == 3 || (modrm >> 3) & 7 != 6 {
        return None;
    }
    let mut len = at + 3;
    let mut base = rm;
    if rm == 4 {
        base = code[len] & 7;
        len += 1;
    }
    len += match mode {
        0 if rm == 5 || (rm == 4 && base == 5) => 4,
        1 => 1,
        2 => 4,
        _ => 0,
    };
    Some(len)
}

struct ClwbLoop {
    base: u8,
    limit: u8,
    len: usize,
}

fn clwb_loop(code: &[u8]) -> Option<ClwbLoop> {
    let (rex, at) = match code.get(1)? {
        0x40 | 0x41 => (code[1], 2),
        _ => (0, 1),
    };
    if *code.first()? != 0x66 || code.get(at..at + 2)? != [0x0f, 0xae] {
        return None;
    }
    let modrm = *code.get(at + 2)?;
    let rm = modrm & 7;
    if modrm >> 6 != 0 || (modrm >> 3) & 7 != 6 || rm == 4 || rm == 5 {
        return None;
    }
    let base = rm | (rex & 1) << 3;
    let add = at + 3;
    if *code.get(add..add + 4)? != [0x48 | base >> 3, 0x83, 0xc0 | (base & 7), 0x40] {
        return None;
    }
    let cmp = add + 4;
    let &[cmp_rex @ 0x48..=0x4f, 0x39, cmp_modrm] = code.get(cmp..cmp + 3)? else {
        return None;
    };
    if cmp_modrm >> 6 != 3 || (cmp_modrm & 7 | (cmp_rex & 1) << 3) != base {
        return None;
    }
    let limit = (cmp_modrm >> 3) & 7 | (cmp_rex & 4) << 1;
    let jump = cmp + 3;
    let len = jump + 2;
    if *code.get(jump)? != 0x76 || i64::from(*code.get(jump + 1)? as i8) != -(len as i64) {
        return None;
    }
    (limit != base).then_some(ClwbLoop { base, limit, len })
}

fn gpr(context: &mut CONTEXT, n: u8) -> &mut u64 {
    match n & 15 {
        0 => &mut context.Rax,
        1 => &mut context.Rcx,
        2 => &mut context.Rdx,
        3 => &mut context.Rbx,
        4 => &mut context.Rsp,
        5 => &mut context.Rbp,
        6 => &mut context.Rsi,
        7 => &mut context.Rdi,
        8 => &mut context.R8,
        9 => &mut context.R9,
        10 => &mut context.R10,
        11 => &mut context.R11,
        12 => &mut context.R12,
        13 => &mut context.R13,
        14 => &mut context.R14,
        _ => &mut context.R15,
    }
}

fn cmp_flags(a: u64, b: u64) -> u32 {
    let r = a.wrapping_sub(b);
    u32::from(a < b)
        | u32::from((r as u8).count_ones().is_multiple_of(2)) << 2
        | (((a ^ b ^ r) >> 4) as u32 & 1) << 4
        | u32::from(r == 0) << 6
        | ((r >> 63) as u32) << 7
        | ((((a ^ b) & (a ^ r)) >> 63) as u32) << 11
}
const CMP_FLAGS: u32 = 0x8d5;

unsafe fn sse4a(code: *const u8, xmm: &mut [M128A; 16]) -> Option<usize> {
    // SAFETY: the caller's code bytes are readable up to the instruction's end.
    let b = |i: usize| unsafe { code.add(i).read() };
    let prefix = b(0);
    let (rex, at) = if b(1) & 0xf0 == 0x40 {
        (b(1), 2)
    } else {
        (0, 1)
    };
    if !matches!(prefix, 0x66 | 0xf2) || b(at) != 0x0f || !matches!(b(at + 1), 0x78 | 0x79) {
        return None;
    }
    let modrm = b(at + 2);
    if modrm >> 6 != 3 {
        return None;
    }
    let dst = usize::from((modrm >> 3) & 7 | (rex & 4) << 1);
    let src = usize::from(modrm & 7 | (rex & 1) << 3);
    let immediate = b(at + 1) == 0x78;
    let (fields, len) = if immediate {
        ((b(at + 3), b(at + 4)), at + 5)
    } else {
        let f = if prefix == 0x66 {
            xmm[src].Low
        } else {
            xmm[src].High as u64
        };
        ((f as u8, (f >> 8) as u8), at + 3)
    };
    let (n, at_bit) = (u32::from(fields.0 & 63), u32::from(fields.1 & 63));
    let bits = if n == 0 { 64 } else { n };
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    if prefix == 0x66 {
        if immediate && (modrm >> 3) & 7 != 0 {
            return None;
        }
        let target = if immediate { src } else { dst };
        xmm[target].Low = (xmm[target].Low >> at_bit) & mask;
    } else {
        let value = (xmm[src].Low & mask) << at_bit;
        xmm[dst].Low = xmm[dst].Low & !(mask << at_bit) | value;
    }
    Some(len)
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes valid exception pointers.
    if unsafe { (*(*info).ExceptionRecord).ExceptionCode } == EXCEPTION_ILLEGAL_INSTRUCTION {
        // SAFETY: the faulting thread's context, for the exception being dispatched.
        let r = unsafe { crate::console_insn::emulate(&mut *(*info).ContextRecord) };
        if r != 0 {
            return r;
        }
    }
    // SAFETY: the system passes valid exception pointers.
    if unsafe { emulate_amd_only(info) } {
        return -1;
    }
    // SAFETY: as above.
    if FATAL.contains(&unsafe { (*(*info).ExceptionRecord).ExceptionCode }) {
        // SAFETY: as above.
        unsafe { report("first-chance exception", info, false) };
    }
    EXCEPTION_CONTINUE_SEARCH
}

pub unsafe fn report_unhandled(info: *const EXCEPTION_POINTERS) {
    // SAFETY: forwarded from the caller.
    unsafe { report("unhandled exception", info, true) };
}

unsafe fn instruction_bytes(at: usize) -> String {
    let n = (0x1000 - (at & 0xfff)).min(15);
    // SAFETY: `n` bytes from `at` stay inside its page, which the caller
    // says is readable.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, n) };
    let hex: Vec<_> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex.join(" ")
}

const CPP_EXCEPTION: i32 = 0xE06D_7363_u32 as i32;
const CPP_MAGIC: usize = 0x1993_0520;

unsafe fn thrown(p: &[usize]) -> String {
    // SAFETY: the throw's ThrowInfo, its RVAs relative to `p[3]`: catchable
    // type array at +12 (count, then type RVAs), each type's descriptor at
    // +4, whose decorated name starts at +16; the object is at `p[1]`.
    unsafe {
        let rva = |at: usize| (at as *const u32).read_unaligned() as usize;
        let types = p[3] + rva(p[2] + 12);
        let name = |i: usize| {
            let descriptor = p[3] + rva(p[3] + rva(types + 4 + 4 * i) + 4);
            std::ffi::CStr::from_ptr((descriptor + 16) as *const std::ffi::c_char).to_string_lossy()
        };
        let first = name(0);
        let value = match &*first {
            ".H" | ".I" | ".J" | ".K" if p[1] != 0 => {
                format!(" = {:#010x}", (p[1] as *const u32).read_unaligned())
            }
            _ if (0..rva(types)).any(|i| name(i) == ".?AVexception@std@@") => {
                let message = (p[1] as *const *const std::ffi::c_char).add(1).read();
                if message.is_null() {
                    String::new()
                } else {
                    format!(": {}", std::ffi::CStr::from_ptr(message).to_string_lossy())
                }
            }
            _ => String::new(),
        };
        format!("{first}{value}")
    }
}

unsafe fn report(what: &str, info: *const EXCEPTION_POINTERS, to_file: bool) {
    // SAFETY: the system passes valid exception pointers.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
    let code = record.ExceptionCode;
    let at = record.ExceptionAddress as usize;
    let p = record.ExceptionInformation;
    let detail = if code == EXCEPTION_ACCESS_VIOLATION {
        let kind = match p[0] {
            0 => "read",
            1 => "write",
            _ => "execute",
        };
        format!(" ({kind} {:#x}: {})", p[1], memory_at(p[1]))
    } else if code == CPP_EXCEPTION && record.NumberParameters >= 4 && p[0] == CPP_MAGIC {
        // SAFETY: a live MSVC throw.
        format!(" (C++ {} thrown)", unsafe { thrown(&p) })
    } else if code == EXCEPTION_ILLEGAL_INSTRUCTION || code == EXCEPTION_PRIV_INSTRUCTION {
        // SAFETY: the CPU refused the instruction at `at`, so it fetched it.
        format!(" (code {})", unsafe { instruction_bytes(at) })
    } else {
        String::new()
    };
    let mut text = String::new();
    let mut line = |l: String| {
        eprintln!("{l}");
        text.push_str(&l);
        text.push('\n');
    };
    line(format!(
        "Oathkeep: {what} {:#010x} at {}{detail}",
        code as u32,
        locate(at)
    ));
    line(format!(
        "  rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x} rsi {:#x} rdi {:#x} rsp {:#x} rbp {:#x}",
        context.Rax,
        context.Rbx,
        context.Rcx,
        context.Rdx,
        context.Rsi,
        context.Rdi,
        context.Rsp,
        context.Rbp
    ));
    line(format!(
        "  r8 {:#x} r9 {:#x} r10 {:#x} r11 {:#x} r12 {:#x} r13 {:#x} r14 {:#x} r15 {:#x}",
        context.R8,
        context.R9,
        context.R10,
        context.R11,
        context.R12,
        context.R13,
        context.R14,
        context.R15
    ));
    for (i, pc) in unwind(*context).into_iter().enumerate() {
        line(format!("  #{i:<2} {}", locate(pc as usize)));
    }
    if to_file {
        let said = title_output();
        eprint!("{said}");
        text.push_str(&said);
        append_to_log(&text);
    }
    #[cfg(feature = "diagnostics")]
    for (address, len, file) in SAVE.get().into_iter().flatten() {
        // SAFETY: the caller of `save_on_crash` named readable memory.
        let bytes = unsafe { std::slice::from_raw_parts(*address as *const u8, *len) };
        let saved = std::fs::write(file, bytes);
        eprintln!(
            "  saved {address:#x}+{len:#x} to {} ({saved:?})",
            file.display()
        );
    }
}

fn unwind(context: CONTEXT) -> Vec<u64> {
    let (mut low, mut high) = (0, 0);
    // SAFETY: writes the current thread's stack bounds.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let mut frames = Vec::with_capacity(MAX_FRAMES);
    unwind_into(context, low, high, &mut frames);
    frames
}

#[repr(C, align(16))]
pub(crate) struct Aligned16(pub(crate) CONTEXT);

pub(crate) fn unwind_into(context: CONTEXT, low: usize, high: usize, frames: &mut Vec<u64>) {
    let mut aligned = Aligned16(context);
    let context = &mut aligned.0;
    frames.push(context.Rip);
    while frames.len() < MAX_FRAMES {
        let rsp = context.Rsp as usize;
        if rsp < low || rsp + 8 > high {
            break;
        }
        let mut image = 0;
        // SAFETY: looks up the function table entry covering `Rip`, if any.
        let entry = unsafe { RtlLookupFunctionEntry(context.Rip, &mut image, null_mut()) };
        if entry.is_null() {
            // SAFETY: `rsp` is inside this thread's stack.
            context.Rip = unsafe { (rsp as *const u64).read() };
            context.Rsp += 8;
        } else {
            let (mut data, mut frame) = (null_mut(), 0);
            // SAFETY: `entry` belongs to `image`; the context is our copy.
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image,
                    context.Rip,
                    entry,
                    context,
                    &mut data,
                    &mut frame,
                    null_mut(),
                )
            };
        }
        if context.Rip == 0 {
            break;
        }
        frames.push(context.Rip);
    }
}

pub fn locate(address: usize) -> String {
    module_of(address).unwrap_or_else(|| format!("{address:#x}"))
}

fn module_of(address: usize) -> Option<String> {
    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: looks up the module containing `address`; no reference taken.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    };
    if found == 0 {
        return None;
    }
    let mut path = [0u16; 260];
    // SAFETY: `module` is a loaded module; the buffer length is passed.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) };
    let path = String::from_utf16_lossy(&path[..len as usize]);
    let name = path.rsplit('\\').next().unwrap_or(&path);
    Some(format!("{name}+{:#x}", address - module as usize))
}

fn memory_at(address: usize) -> String {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_IMAGE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE,
        PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD,
        PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
    };
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: reads the address space's bookkeeping, not `address` itself.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    if n == 0 {
        return "outside the address space".into();
    }
    let protect = match info.State {
        MEM_FREE => return "nothing mapped there".into(),
        MEM_RESERVE => return "reserved, never committed".into(),
        _ if info.Protect & PAGE_GUARD != 0 => return "a guard page".into(),
        _ => match info.Protect & 0xff {
            PAGE_NOACCESS => "no access".into(),
            PAGE_READONLY => "read-only".into(),
            PAGE_READWRITE => "read-write".into(),
            PAGE_WRITECOPY => "copy-on-write".into(),
            PAGE_EXECUTE => "execute-only".into(),
            PAGE_EXECUTE_READ => "read-execute".into(),
            PAGE_EXECUTE_READWRITE => "read-write-execute".into(),
            PAGE_EXECUTE_WRITECOPY => "copy-on-write, executable".into(),
            _ => format!("protection {:#x}", info.Protect),
        },
    };
    match module_of(address) {
        Some(module) if info.Type == MEM_IMAGE => format!("{protect}, in {module}"),
        _ => protect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_RECORD, RtlCaptureContext, RtlCaptureStackBackTrace,
    };

    #[test]
    #[inline(never)]
    fn unwinds_like_the_system_walk() {
        // SAFETY: zeroed CONTEXT is valid; both calls write our buffers.
        let (frames, walked) = unsafe {
            let mut context = Aligned16(std::mem::zeroed());
            RtlCaptureContext(&mut context.0);
            let mut walked = [null_mut(); 8];
            let n = RtlCaptureStackBackTrace(0, 8, walked.as_mut_ptr(), null_mut());
            (unwind(context.0), walked[..usize::from(n)].to_vec())
        };
        assert!(walked.len() >= 4, "{walked:?}");
        let ours: Vec<_> = frames[1..walked.len()]
            .iter()
            .map(|&f| f as usize)
            .collect();
        let system: Vec<_> = walked[1..].iter().map(|&f| f as usize).collect();
        assert_eq!(ours, system);
    }

    #[test]
    fn memory_at_names_what_an_access_hit() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY,
            PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // SAFETY: reserves four pages of ours and commits three of them;
        // nothing reads or writes them.
        let base = unsafe {
            let base = VirtualAlloc(std::ptr::null(), 0x4000, MEM_RESERVE, PAGE_NOACCESS);
            assert!(!base.is_null());
            let page = |i: usize, protect| {
                let at = base.cast::<u8>().add(i * 0x1000).cast();
                assert!(!VirtualAlloc(at, 0x1000, MEM_COMMIT, protect).is_null());
            };
            page(1, PAGE_READONLY);
            page(2, PAGE_READWRITE | PAGE_GUARD);
            page(3, PAGE_NOACCESS);
            base as usize
        };
        assert_eq!(memory_at(base + 8), "reserved, never committed");
        assert_eq!(memory_at(base + 0x1008), "read-only");
        assert_eq!(memory_at(base + 0x2008), "a guard page");
        assert_eq!(memory_at(base + 0x3008), "no access");
        // SAFETY: releases the reservation made above.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
        assert_eq!(memory_at(base + 8), "nothing mapped there");
        assert_eq!(memory_at(0), "nothing mapped there");
        assert_eq!(memory_at(usize::MAX), "outside the address space");
        let code = memory_at(memory_at_names_what_an_access_hit as *const () as usize);
        let named = code.starts_with("read-execute, in oathkeep_loader-");
        assert!(named, "{code}");
    }

    #[test]
    fn clwb_loops() {
        let a = clwb_loop(&[
            0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
        ])
        .expect("rcx loop");
        assert_eq!((a.base, a.limit, a.len), (1, 0, 13));
        let b = clwb_loop(&[
            0x66, 0x41, 0x0f, 0xae, 0x31, 0x49, 0x83, 0xc1, 0x40, 0x49, 0x39, 0xc1, 0x76, 0xf2,
        ])
        .expect("r9 loop");
        assert_eq!((b.base, b.limit, b.len), (9, 0, 14));
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x20, 0x48, 0x39, 0xc1, 0x76, 0xf3
            ])
            .is_none()
        );
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf0
            ])
            .is_none()
        );
        assert!(clwb_loop(&[0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40]).is_none());
        assert_eq!(cmp_flags(0x1040, 0x1000) & 0x41, 0);
        assert_eq!(cmp_flags(0x1000, 0x1000) & 0x40, 0x40);
    }

    #[test]
    fn clwb_lengths() {
        let pad = |b: &[u8]| {
            let mut c = [0u8; 15];
            c[..b.len()].copy_from_slice(b);
            c
        };
        assert_eq!(clwb_len(pad(&[0x66, 0x41, 0x0f, 0xae, 0x31])), Some(5));
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x30])), Some(4));
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x70, 0x40])), Some(5));
        assert_eq!(
            clwb_len(pad(&[0x66, 0x0f, 0xae, 0xb0, 0, 1, 0, 0])),
            Some(8)
        );
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x34, 0x24])), Some(5));
        assert_eq!(
            clwb_len(pad(&[0x66, 0x0f, 0xae, 0x35, 0, 0, 0, 0])),
            Some(8)
        );
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0x38])), None);
        assert_eq!(clwb_len(pad(&[0x0f, 0xae, 0x30])), None);
        assert_eq!(clwb_len(pad(&[0x66, 0x0f, 0xae, 0xf0])), None);
    }

    #[test]
    fn clzero_zeroes_its_line_on_any_cpu() {
        report_crashes();
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5);
        // SAFETY: clzero writes only the 64-byte line holding `at`, ours.
        unsafe { std::arch::asm!(".byte 0x0f, 0x01, 0xfc", in("rax") at) };
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
        let out: u64;
        // SAFETY: touches only the named registers.
        unsafe {
            std::arch::asm!(
                "movq xmm0, {a}", "movq xmm1, {b}",
                ".byte 0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10",
                "movq {a}, xmm0",
                ".byte 0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14",
                "movq {b}, xmm0",
                a = inout(reg) 0x1111_2222_3333_4444u64 => _, b = inout(reg) 0xabcdu64 => out,
                out("xmm0") _, out("xmm1") _)
        };
        assert_eq!(out, 0xbc);
        // SAFETY: monitorx/mwaitx are wait hints that write no memory.
        unsafe {
            std::arch::asm!(".byte 0x0f, 0x01, 0xfa", ".byte 0x0f, 0x01, 0xfb",
                in("rax") at, in("ecx") 0, in("edx") 0)
        };
    }

    fn reg(low: u64, high: u64) -> M128A {
        M128A {
            Low: low,
            High: high as i64,
        }
    }

    fn emulate_as(exception: i32, code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        let mut bytes = [0xcc_u8; 16];
        bytes[..code.len()].copy_from_slice(code);
        let start = bytes.as_ptr() as u64;
        // SAFETY: a zeroed record and context are valid; the emulation
        // reads only `bytes` and writes only `context` (and memory at
        // `rax` for clzero, which the caller owns).
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = exception;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = start;
            context.Rax = rax;
            context.Anonymous.FltSave.XmmRegisters = *xmm;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            let emulated = emulate_amd_only(&mut pointers);
            *xmm = context.Anonymous.FltSave.XmmRegisters;
            emulated.then(|| context.Rip - start)
        }
    }

    fn emulate(code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        emulate_as(EXCEPTION_ILLEGAL_INSTRUCTION, code, rax, xmm)
    }

    fn low(xmm: &[M128A; 16], i: usize) -> u64 {
        xmm[i].Low
    }

    #[test]
    fn emulated_clzero_zeroes_its_line() {
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5) as u64;
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfc], at, &mut xmm), Some(3));
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn emulated_monitorx_and_mwaitx_are_skipped() {
        let mut xmm = [reg(1, 2); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfa], 0, &mut xmm), Some(3));
        assert_eq!(emulate(&[0x0f, 0x01, 0xfb], 0, &mut xmm), Some(3));
        assert!(xmm.iter().all(|r| r.Low == 1 && r.High == 2));
    }

    #[test]
    fn emulated_insertq_and_extrq_with_immediates() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1111_2222_3333_4444, 0x5555);
        xmm[1] = reg(0xabcd, 0x6666);
        let insertq = [0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 1), xmm[1].High), (0xabcd, 0x6666));
        let extrq = [0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_insertq_and_extrq_with_fields_in_a_register() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1111_2222_3333_4444, 0);
        xmm[1] = reg(0xabcd, 0x10_10);
        assert_eq!(emulate(&[0xf2, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        xmm[1] = reg(0x14_08, 0);
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_sse4a_reaches_xmm8_to_xmm15() {
        let mut xmm = [reg(0, 0); 16];
        xmm[8] = reg(0x1111_2222_3333_4444, 0);
        xmm[9] = reg(0xabcd, 0);
        let insertq = [0xf2, 0x45, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 8), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 0), low(&xmm, 1)), (0, 0));
        let extrq = [0x66, 0x41, 0x0f, 0x78, 0xc1, 0x08, 0x04];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 9), 0xbc);
        assert_eq!(low(&xmm, 1), 0);
    }

    #[test]
    fn emulated_sse4a_length_0_is_64_bits() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1122_3344_5566_7788, 0);
        assert_eq!(
            emulate(&[0x66, 0x0f, 0x78, 0xc0, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0x1122_3344_5566_7788);
        xmm[1] = reg(0xdead_beef_cafe_f00d, 0);
        assert_eq!(
            emulate(&[0xf2, 0x0f, 0x78, 0xc1, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0xdead_beef_cafe_f00d);
    }

    #[test]
    fn other_instructions_and_exceptions_are_not_emulated() {
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x0b], 0, &mut xmm), None);
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0x00], 0, &mut xmm), None);
        let bad_extrq = [0x66, 0x0f, 0x78, 0xc8, 0x08, 0x14];
        assert_eq!(emulate(&bad_extrq, 0, &mut xmm), None);
        let clzero = [0x0f, 0x01, 0xfc];
        assert_eq!(
            emulate_as(EXCEPTION_ACCESS_VIOLATION, &clzero, 0, &mut xmm),
            None
        );
    }

    #[test]
    fn instruction_bytes_stop_at_the_end_of_their_page() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        };
        let mut code = [0x90_u8; 16];
        code[..6].copy_from_slice(&[0x0f, 0x01, 0xfd, 0x48, 0x89, 0xc1]);
        // SAFETY: two fresh pages of ours, the second made unreadable, so a
        // read past the end of the first faults.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            let mut old = 0;
            let guard = page.add(0x1000).cast();
            assert_ne!(VirtualProtect(guard, 0x1000, PAGE_NOACCESS, &mut old), 0);
            page.copy_from_nonoverlapping(code.as_ptr(), code.len());
            let whole = "0f 01 fd 48 89 c1 90 90 90 90 90 90 90 90 90";
            assert_eq!(instruction_bytes(page as usize), whole);
            let end = page.add(0x1000 - 3);
            end.copy_from_nonoverlapping(code.as_ptr(), 3);
            assert_eq!(instruction_bytes(end as usize), "0f 01 fd");
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }
}
