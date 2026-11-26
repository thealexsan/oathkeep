mod emulate;

use std::ptr::null_mut;

use super::amd_only::*;
use super::unwind::*;
use super::*;
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, EXCEPTION_RECORD, RtlCaptureContext, RtlCaptureStackBackTrace,
    RtlLookupFunctionEntry,
};

/// What [`capture_frames`] saw: our [`unwind`], then the system's walk.
type Captured = (Vec<u64>, Vec<usize>);

/// A frame we expect [`unwind`] to report: the helper it is in, and the
/// helper's name for the message.
type Expected = (&'static str, fn() -> Captured);

/// The runtime function entry start covering `at`: the image base the
/// lookup writes, plus the entry's `BeginAddress`. Asking the system for
/// both sides of the comparison means a thunk or an incremental-link jump
/// is not mistaken for a different function.
fn entry_start_at(at: u64) -> Option<u64> {
    let mut image = 0;
    // SAFETY: looks up the function table entry covering `at`; nothing
    // keeps a reference to the entry past this call.
    let entry = unsafe { RtlLookupFunctionEntry(at, &mut image, null_mut()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: a non-null entry belongs to `image`, which the lookup wrote,
    // and `BeginAddress` is an offset into that image.
    Some(image + u64::from(unsafe { (*entry).BeginAddress }))
}

#[test]
fn unwinds_like_the_system_walk() {
    // Asking the system's own walk whether the stack looks right is not
    // reliable: on Windows 10 x64 debug builds `RtlCaptureStackBackTrace`
    // returns no frames about half the time. The frames above us are
    // known instead (see `level_a`), so check those first.
    let (frames, walked) = level_a();
    // Frame 0 is in `capture_frames`, which took the context; then its
    // callers in call order, each named by its runtime function entry.
    let helpers: [Expected; 4] = [
        ("capture_frames", capture_frames),
        ("level_c", level_c),
        ("level_b", level_b),
        ("level_a", level_a),
    ];
    assert!(frames.len() >= helpers.len(), "{frames:?}");
    for (i, (name, helper)) in helpers.iter().enumerate() {
        let want = entry_start_at(*helper as usize as u64)
            .unwrap_or_else(|| panic!("no unwind data for {name}"));
        let got = entry_start_at(frames[i])
            .unwrap_or_else(|| panic!("no unwind data for frame {i} at {:#x}", frames[i]));
        assert_eq!(got, want, "frame {i} should be {name}: {frames:?}");
    }
    // A walk that repeats an address is not a walk.
    let mut distinct = frames[..helpers.len()].to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), helpers.len(), "repeated: {frames:?}");
    // Where the system's walk does return frames, it must still agree with
    // ours: frame 0 differs (both are in `capture_frames`, at different
    // call sites), and the callers above are the same.
    if walked.len() >= helpers.len() {
        assert!(frames.len() >= walked.len(), "{frames:?} vs {walked:?}");
        let ours: Vec<usize> = frames[1..walked.len()]
            .iter()
            .map(|&f| f as usize)
            .collect();
        assert_eq!(ours.as_slice(), &walked[1..]);
    }
}

/// The frames of this call, both ways: our [`unwind`], and the system's
/// walk, which Windows 10 can come up empty on.
#[inline(never)]
fn capture_frames() -> Captured {
    // SAFETY: a zeroed CONTEXT is valid and both calls write buffers of
    // ours; `RtlCaptureContext` wants the context 16-byte aligned too.
    unsafe {
        let mut context = Aligned16(std::mem::zeroed());
        RtlCaptureContext(&mut context.0);
        let mut walked = [null_mut(); 8];
        let n = RtlCaptureStackBackTrace(0, 8, walked.as_mut_ptr(), null_mut());
        let frames = unwind(context.0);
        let walked = walked[..usize::from(n)]
            .iter()
            .map(|&f| f as usize)
            .collect();
        (frames, walked)
    }
}

/// `level_a` → `level_b` → `level_c` → [`capture_frames`]: three known
/// callers above the capture. `black_box` after each call keeps it from
/// becoming a tail call, which would erase the caller's frame.
#[inline(never)]
fn level_a() -> Captured {
    let captured = level_b();
    std::hint::black_box(&captured);
    captured
}

/// See [`level_a`].
#[inline(never)]
fn level_b() -> Captured {
    let captured = level_c();
    std::hint::black_box(&captured);
    captured
}

/// See [`level_a`].
#[inline(never)]
fn level_c() -> Captured {
    let captured = capture_frames();
    std::hint::black_box(&captured);
    captured
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
    // This test's own code: an image page, named by its module.
    let code = memory_at(memory_at_names_what_an_access_hit as *const () as usize);
    let named = code.starts_with("read-execute, in oathkeep_loader-");
    assert!(named, "{code}");
}

/// On an Intel CPU, the real instructions go through the vectored
/// handler. On an AMD CPU they run natively, and the tests below test
/// the emulation.
#[test]
fn clwb_loops() {
    // clwb [rcx]; add rcx, 0x40; cmp rcx, rax; jbe top
    let a = clwb_loop(&[
        0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
    ])
    .expect("rcx loop");
    assert_eq!((a.base, a.limit, a.len), (1, 0, 13));
    // clwb [r9]; add r9, 0x40; cmp r9, rax; jbe top
    let b = clwb_loop(&[
        0x66, 0x41, 0x0f, 0xae, 0x31, 0x49, 0x83, 0xc1, 0x40, 0x49, 0x39, 0xc1, 0x76, 0xf2,
    ])
    .expect("r9 loop");
    assert_eq!((b.base, b.limit, b.len), (9, 0, 14));
    // A different stride, a jump elsewhere, or a cut-off loop is not one.
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
    // The flags the last cmp leaves: base just past limit.
    assert_eq!(cmp_flags(0x1040, 0x1000) & 0x41, 0); // CF and ZF clear: the loop ends
    assert_eq!(cmp_flags(0x1000, 0x1000) & 0x40, 0x40);
}

#[test]
fn clwb_loop_wrapping_ranges_do_not_overflow_and_fall_back_to_single_clwb() {
    assert_eq!(clwb_loop_end(0x1000, 0x1080), Some(0x10c0));
    assert_eq!(clwb_loop_end(0x10c0, 0x1080), Some(0x1100));
    assert_eq!(clwb_loop_end(0, u64::MAX - 10), None);
    assert_eq!(clwb_loop_end(u64::MAX - 16, 0x100), None);
    assert_eq!(clwb_loop_end(u64::MAX - 63, u64::MAX), None);

    // clwb [rcx]; add rcx, 0x40; cmp rcx, rax; jbe top (13 bytes, clwb is 4 bytes).
    let mut bytes = [0xcc_u8; 16];
    bytes[..13].copy_from_slice(&[
        0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
    ]);
    let run = |base: u64, limit: u64| -> (u64, u64) {
        let start = bytes.as_ptr() as u64;
        // SAFETY: zeroed record/context are valid; `emulate_amd_only` only
        // reads `bytes` and updates `context`.
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = EXCEPTION_ILLEGAL_INSTRUCTION;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = start;
            context.Rcx = base;
            context.Rax = limit;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            assert!(emulate_amd_only(&mut pointers));
            (context.Rip - start, context.Rcx)
        }
    };
    // Normal range completes the whole 13-byte loop in one exception.
    assert_eq!(run(0x1000, 0x1080), (13, 0x10c0));
    // Overflowing/wrapping ranges must not panic or skip the loop: emulate
    // just the 4-byte `clwb` instruction and let the CPU run add/cmp/jbe.
    assert_eq!(run(0, u64::MAX - 10), (4, 0));
    assert_eq!(run(u64::MAX - 16, 0x100), (4, u64::MAX - 16));
}

#[test]
fn clwb_lengths() {
    assert_eq!(clwb_len(&[0x66, 0x41, 0x0f, 0xae, 0x31]), Some(5)); // clwb [r9]
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x30]), Some(4)); // clwb [rax]
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x70, 0x40]), Some(5)); // clwb [rax+0x40]
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0xb0, 0, 1, 0, 0]), Some(8)); // [rax+disp32]
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x34, 0x24]), Some(5)); // clwb [rsp]
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x35, 0, 0, 0, 0]), Some(8)); // [rip+disp32]
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x38]), None); // clflushopt, /7
    assert_eq!(clwb_len(&[0x0f, 0xae, 0x30]), None); // xsaveopt, no 66
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0xf0]), None); // register form
}

#[test]
fn a_clwb_at_the_end_of_a_page_is_decoded() {
    // Exact-length slices, as at the end of a page.
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x30]), Some(4));
    assert_eq!(clwb_len(&[0x66, 0x41, 0x0f, 0xae, 0x31]), Some(5));
    // Cut short by the slice: returns None without panicking.
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0xb0, 0, 1]), None); // disp32, 6 of 8 bytes
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x70]), None); // disp8, 4 of 5 bytes
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x34]), None); // SIB missing
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x34, 0x25, 0, 0]), None); // SIB disp32 cut short
    assert_eq!(clwb_len(&[0x66, 0x0f, 0xae]), None); // ModRM missing
    assert_eq!(clwb_len(&[0x66]), None);
    assert_eq!(clwb_len(&[]), None);

    // Emulation at the last 4 and 5 bytes of a page with an unreadable page after it.
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
        VirtualFree, VirtualProtect,
    };
    // SAFETY: two pages of ours, the second unreadable so reading past the first faults.
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
        for insn in [
            &[0x66, 0x0f, 0xae, 0x30][..],
            &[0x66, 0x41, 0x0f, 0xae, 0x31][..],
        ] {
            let end = page.add(0x1000 - insn.len());
            end.copy_from_nonoverlapping(insn.as_ptr(), insn.len());
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = EXCEPTION_ILLEGAL_INSTRUCTION;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = end as u64;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            assert!(emulate_amd_only(&mut pointers));
            assert_eq!(context.Rip, end as u64 + insn.len() as u64);
        }
        VirtualFree(page.cast(), 0, MEM_RELEASE);
    }
}

#[test]
fn clzero_zeroes_its_line_on_any_cpu() {
    report_crashes();
    #[repr(align(64))]
    struct Line([u8; 128]);
    let mut line = Line([0xff; 128]);
    let at = line.0.as_mut_ptr().wrapping_add(5); // inside the first line
    // SAFETY: clzero writes only the 64-byte line holding `at`, ours.
    unsafe { std::arch::asm!(".byte 0x0f, 0x01, 0xfc", in("rax") at) };
    assert!(line.0[..64].iter().all(|&b| b == 0));
    assert!(line.0[64..].iter().all(|&b| b == 0xff));
    // insertq xmm0, xmm1, len 16, at 16; then extrq xmm0, len 8, at 20.
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
    assert_eq!(out, 0xbc); // 0x1111_2222_abcd_4444 >> 20, 8 bits
    // SAFETY: monitorx/mwaitx are wait hints that write no memory.
    unsafe {
        std::arch::asm!(".byte 0x0f, 0x01, 0xfa", ".byte 0x0f, 0x01, 0xfb",
            in("rax") at, in("ecx") 0, in("edx") 0)
    };
    // rdpru of MPERF: a count in edx:eax, their high halves clear.
    let (low, high): (u64, u64);
    // SAFETY: touches only the named registers.
    unsafe {
        std::arch::asm!(".byte 0x0f, 0x01, 0xfd",
            out("rax") low, inout("rcx") 0u64 => _, out("rdx") high)
    };
    assert_ne!(high << 32 | low, 0);
    assert!(high >> 32 == 0 && low >> 32 == 0);
}

/// The flags a native `cmp a, b` leaves.
fn native_compare(a: u64, b: u64) -> u32 {
    let flags: u64;
    // SAFETY: compares two registers and reads the flags.
    unsafe {
        std::arch::asm!("cmp {a}, {b}", "pushfq", "pop {f}",
            a = in(reg) a, b = in(reg) b, f = out(reg) flags)
    };
    flags as u32 & CMP_FLAGS
}

#[test]
fn compare_sets_flags_like_the_cpu() {
    let values = [0, 1, 0x40, 0x7f, 0x80, 0xff, 1 << 63, (1 << 63) - 1, !0, !0 - 0x40];
    for a in values {
        for b in values {
            assert_eq!(cmp_flags(a, b), native_compare(a, b), "cmp {a:#x}, {b:#x}");
        }
    }
}

#[test]
fn clzero_loops_end_like_their_last_pass() {
    report_crashes();
    #[repr(align(64))]
    struct Lines([u8; 192]);
    let zeroed = |l: &Lines| [l.0[0] == 0, l.0[64] == 0, l.0[128] == 0];
    // clzero; add rax, 0x40; cmp rax, r10; jb
    let mut mem = Lines([0xff; 192]);
    let start = mem.0.as_mut_ptr() as u64;
    let (end, flags): (u64, u64);
    // SAFETY: zeroes the first two lines of `mem`.
    unsafe {
        std::arch::asm!(
            ".byte 0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x4c, 0x39, 0xd0, 0x72, 0xf4",
            "pushfq", "pop {f}",
            inout("rax") start => end, in("r10") start + 100, f = out(reg) flags)
    };
    assert_eq!(end, start + 128);
    assert_eq!(flags as u32 & CMP_FLAGS, native_compare(end, start + 100));
    assert_eq!(zeroed(&mem), [true, true, false]);
    // mov rax, rdx; add rdx, 0x40; clzero; cmp rdx, r8; jb
    let mut mem = Lines([0xff; 192]);
    let start = mem.0.as_mut_ptr() as u64;
    let (last, next, flags): (u64, u64, u64);
    // SAFETY: zeroes the last two lines of `mem`.
    unsafe {
        std::arch::asm!(
            ".byte 0x48, 0x89, 0xd0, 0x48, 0x83, 0xc2, 0x40, 0x0f, 0x01, 0xfc",
            ".byte 0x4c, 0x39, 0xc2, 0x72, 0xf1",
            "pushfq", "pop {f}",
            out("rax") last, inout("rdx") start + 64 => next, in("r8") start + 192,
            f = out(reg) flags)
    };
    assert_eq!((last, next), (start + 128, start + 192));
    assert_eq!(flags as u32 & CMP_FLAGS, native_compare(next, start + 192));
    assert_eq!(zeroed(&mem), [false, true, true]);
}
