use super::*;
use super::super::report::instruction_bytes;
use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, M128A};

fn reg(low: u64, high: u64) -> M128A {
    M128A {
        Low: low,
        High: high as i64,
    }
}

/// Gives `code` to the emulation as if it faulted with `exception` at
/// its first byte, with `rax` and `xmm` as the saved registers. The
/// emulation does not execute `code`, so this works on any CPU. Returns
/// the number of bytes that the emulation skipped, or `None` if it did
/// not emulate the instruction. `xmm` gets the saved registers after.
fn emulate_as(exception: i32, code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
    // `int3` after the instruction, so that a decoder that reads too
    // far sees no valid bytes.
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
    let at = line.0.as_mut_ptr().wrapping_add(5) as u64; // inside the first line
    let mut xmm = [reg(0, 0); 16];
    assert_eq!(emulate(&[0x0f, 0x01, 0xfc], at, &mut xmm), Some(3));
    assert!(line.0[..64].iter().all(|&b| b == 0));
    assert!(line.0[64..].iter().all(|&b| b == 0xff));
}

/// Gives `code` to the emulation as if it faulted at `code[at]` with
/// `context` as the saved state, on any CPU; returns whether it
/// emulated and leaves the state after in `context`.
fn emulate_in(code: &[u8], at: usize, context: &mut CONTEXT) -> bool {
    // At a fixed place in a page, so the loops' neighbours are always
    // read (they are not near a page's edges).
    #[repr(align(4096))]
    struct Page([u8; 4096]);
    let mut page = Box::new(Page([0xcc; 4096]));
    let bytes = &mut page.0[0x800..];
    bytes[..code.len()].copy_from_slice(code);
    context.Rip = bytes.as_ptr() as u64 + at as u64;
    // SAFETY: a zeroed record is valid; the emulation reads only
    // `bytes` and writes `context` and the lines the caller owns.
    unsafe {
        let mut record: EXCEPTION_RECORD = std::mem::zeroed();
        record.ExceptionCode = EXCEPTION_ILLEGAL_INSTRUCTION;
        let mut pointers = EXCEPTION_POINTERS {
            ExceptionRecord: &mut record,
            ContextRecord: context,
        };
        let emulated = emulate_amd_only(&mut pointers);
        context.Rip -= bytes.as_ptr() as u64;
        emulated
    }
}

#[test]
fn emulated_clzero_loops_end_like_their_last_pass() {
    #[repr(align(64))]
    struct Lines([u8; 192]);
    let zeroed = |l: &Lines| [l.0[0] == 0, l.0[64] == 0, l.0[128] == 0];
    // clzero; add rax, 0x40; cmp rax, r10; jb
    let mut mem = Lines([0xff; 192]);
    let start = mem.0.as_mut_ptr() as u64;
    // SAFETY: an all-zero context is valid.
    let mut context: CONTEXT = unsafe { std::mem::zeroed() };
    context.Rax = start;
    context.R10 = start + 100;
    context.EFlags = 0x202 | CMP_FLAGS;
    let code = [0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x4c, 0x39, 0xd0, 0x72, 0xf4];
    assert!(emulate_in(&code, 0, &mut context));
    assert_eq!((context.Rip, context.Rax), (12, start + 128));
    assert_eq!(context.EFlags, 0x202 | native_compare(start + 128, start + 100));
    assert_eq!(zeroed(&mem), [true, true, false]);
    // mov rax, rdx; add rdx, 0x40; clzero; cmp rdx, r8; jb (faults at clzero,
    // after the first pass's mov and add)
    let mut mem = Lines([0xff; 192]);
    let start = mem.0.as_mut_ptr() as u64;
    // SAFETY: an all-zero context is valid.
    let mut context: CONTEXT = unsafe { std::mem::zeroed() };
    context.Rax = start + 64;
    context.Rdx = start + 128;
    context.R8 = start + 192;
    let code = [
        0x48, 0x89, 0xd0, 0x48, 0x83, 0xc2, 0x40, 0x0f, 0x01, 0xfc, 0x4c, 0x39, 0xc2, 0x72,
        0xf1,
    ];
    assert!(emulate_in(&code, 7, &mut context));
    assert_eq!((context.Rip, context.Rax, context.Rdx), (15, start + 128, start + 192));
    assert_eq!(context.EFlags, native_compare(start + 192, start + 192));
    assert_eq!(zeroed(&mem), [false, true, true]);
    // cmp rax, rax is not the loop (the limit would move with rax): one
    // line, flags untouched.
    let mut mem = Lines([0xff; 192]);
    // SAFETY: an all-zero context is valid.
    let mut context: CONTEXT = unsafe { std::mem::zeroed() };
    context.Rax = mem.0.as_mut_ptr() as u64;
    context.EFlags = 0x202;
    let code = [0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xc0, 0x72, 0xf4];
    assert!(emulate_in(&code, 0, &mut context));
    assert_eq!((context.Rip, context.EFlags), (3, 0x202));
    assert_eq!(zeroed(&mem), [true, false, false]);
    // Any other neighbours: one line, then the next instruction.
    let mut mem = Lines([0xff; 192]);
    // SAFETY: an all-zero context is valid.
    let mut context: CONTEXT = unsafe { std::mem::zeroed() };
    context.Rax = mem.0.as_mut_ptr() as u64 + 70;
    context.R10 = !0;
    let code = [0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x20, 0x4c, 0x39, 0xd0, 0x72, 0xf4];
    assert!(emulate_in(&code, 0, &mut context));
    assert_eq!(context.Rip, 3);
    assert_eq!(zeroed(&mem), [false, true, false]);
}

#[test]
fn emulated_rdpru_reads_known_counters_and_keeps_flags() {
    let cases = [(0, true), (1, true), (2, false), (0x1_0000_0000, true)];
    let starts = [0x202, 0x202 | CMP_FLAGS];
    for ((ecx, known), flags) in cases.into_iter().flat_map(|c| starts.map(|f| (c, f))) {
        // SAFETY: an all-zero context is valid.
        let mut context: CONTEXT = unsafe { std::mem::zeroed() };
        context.Rcx = ecx;
        context.Rax = !0;
        context.Rdx = !0;
        context.EFlags = flags;
        assert!(emulate_in(&[0x0f, 0x01, 0xfd], 0, &mut context));
        assert_eq!(context.Rip, 3);
        assert_eq!(context.EFlags, flags, "ecx {ecx:#x}");
        let count = context.Rdx << 32 | context.Rax;
        assert!(context.Rax >> 32 == 0 && context.Rdx >> 32 == 0);
        assert_eq!(count != 0, known, "ecx {ecx:#x}");
    }
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
    // insertq xmm0, xmm1, length 16, index 16
    let insertq = [0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10];
    assert_eq!(emulate(&insertq, 0, &mut xmm), Some(6));
    assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
    assert_eq!((low(&xmm, 1), xmm[1].High), (0xabcd, 0x6666));
    // extrq xmm0, length 8, index 20
    let extrq = [0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14];
    assert_eq!(emulate(&extrq, 0, &mut xmm), Some(6));
    assert_eq!(low(&xmm, 0), 0xbc);
}

#[test]
fn emulated_insertq_and_extrq_with_fields_in_a_register() {
    let mut xmm = [reg(0, 0); 16];
    // insertq xmm0, xmm1: length and index in xmm1's high half.
    xmm[0] = reg(0x1111_2222_3333_4444, 0);
    xmm[1] = reg(0xabcd, 0x10_10);
    assert_eq!(emulate(&[0xf2, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
    assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
    // extrq xmm0, xmm1: length and index in xmm1's low half.
    xmm[1] = reg(0x14_08, 0);
    assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
    assert_eq!(low(&xmm, 0), 0xbc);
}

#[test]
fn emulated_sse4a_reaches_xmm8_to_xmm15() {
    let mut xmm = [reg(0, 0); 16];
    xmm[8] = reg(0x1111_2222_3333_4444, 0);
    xmm[9] = reg(0xabcd, 0);
    // insertq xmm8, xmm9, length 16, index 16 (REX.R and REX.B)
    let insertq = [0xf2, 0x45, 0x0f, 0x78, 0xc1, 0x10, 0x10];
    assert_eq!(emulate(&insertq, 0, &mut xmm), Some(7));
    assert_eq!(low(&xmm, 8), 0x1111_2222_abcd_4444);
    assert_eq!((low(&xmm, 0), low(&xmm, 1)), (0, 0));
    // extrq xmm9, length 8, index 4 (REX.B)
    let extrq = [0x66, 0x41, 0x0f, 0x78, 0xc1, 0x08, 0x04];
    assert_eq!(emulate(&extrq, 0, &mut xmm), Some(7));
    assert_eq!(low(&xmm, 9), 0xbc);
    assert_eq!(low(&xmm, 1), 0);
}

#[test]
fn emulated_sse4a_length_0_is_64_bits() {
    let mut xmm = [reg(0, 0); 16];
    xmm[0] = reg(0x1122_3344_5566_7788, 0);
    // extrq xmm0, length 0 (64), index 0. A non-zero index with
    // length 64 goes past bit 63, and AMD leaves that result undefined.
    assert_eq!(
        emulate(&[0x66, 0x0f, 0x78, 0xc0, 0x00, 0x00], 0, &mut xmm),
        Some(6)
    );
    assert_eq!(low(&xmm, 0), 0x1122_3344_5566_7788);
    // insertq xmm0, xmm1, length 0 (64), index 0
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
    assert_eq!(emulate(&[0x0f, 0x0b], 0, &mut xmm), None); // ud2
    // extrq with a memory operand, which does not exist
    assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0x00], 0, &mut xmm), None);
    // extrq with immediates needs ModRM.reg 0
    let bad_extrq = [0x66, 0x0f, 0x78, 0xc8, 0x08, 0x14];
    assert_eq!(emulate(&bad_extrq, 0, &mut xmm), None);
    // a clzero that did not cause an illegal instruction
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
    // rdpru, then the next instruction (mov rcx, rax) and padding.
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
        // The page's last three bytes: rdpru, and nothing read after it.
        let end = page.add(0x1000 - 3);
        end.copy_from_nonoverlapping(code.as_ptr(), 3);
        assert_eq!(instruction_bytes(end as usize), "0f 01 fd");
        VirtualFree(page.cast(), 0, MEM_RELEASE);
    }
}
