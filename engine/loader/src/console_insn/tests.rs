use super::patch::{clzero_stub, jump, patch, with_patch};

use super::*;

#[test]
fn wait_instructions_are_replaced_inside_sixteen_aligned_bytes() {
    let word = u128::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 0x0f, 0x01, 0xfb, 11, 12, 13, 14, 15, 16]);
    let new = with_patch(word, 0x1007, &[0xf3, 0x90, 0x90]).unwrap();
    assert_eq!(new.to_le_bytes(), [1, 2, 3, 4, 5, 6, 7, 0xf3, 0x90, 0x90, 11, 12, 13, 14, 15, 16]);
    assert!(with_patch(word, 0x100d, &[0; 3]).is_some());
    assert!(with_patch(word, 0x100e, &[0; 3]).is_none(), "crosses into the next sixteen");
}

/// `monitorx; mwaitx; ret` in a page of its own: each is replaced at its first fault,
/// and the page is protected as before.
#[test]
fn a_wait_faults_once_and_the_page_keeps_its_protection() {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        VirtualProtect, VirtualQuery,
    };
    // SAFETY: memory of our own, read, written and run within its page.
    unsafe {
        let page = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
        assert!(!page.is_null());
        let code = [0x0f, 0x01, 0xfa, 0x0f, 0x01, 0xfb, 0xc3];
        // The first over the end of its aligned eight bytes, as the title's is; and one
        // over the end of its sixteen (at 46), which is left alone.
        std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(6), code.len());
        std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(46), code.len());
        let mut old = 0;
        assert_ne!(VirtualProtect(page.cast(), 4096, PAGE_EXECUTE_READ, &mut old), 0);
        let at = page as usize;
        let mut context: CONTEXT = std::mem::zeroed();
        for rip in [at + 6, at + 9, at + 46] {
            context.Rip = rip as u64;
            assert_eq!(emulate(&mut context), CONTINUE_EXECUTION);
            assert_eq!(context.Rip, rip as u64 + 3);
        }
        if patches_waits() {
            let now = std::slice::from_raw_parts(page.add(6), 7);
            assert_eq!(now, [0x0f, 0x1f, 0x00, 0xf3, 0x90, 0x90, 0xc3]);
            // They run now.
            let run: extern "system" fn() = std::mem::transmute(page.add(6));
            run();
            // A second thread that faulted at the same place meanwhile.
            assert!(patch(at + 6, [0x0f, 0x01, 0xfa], [0x0f, 0x1f, 0x00]));
            assert!(!patch(at + 46, [0x0f, 0x01, 0xfa], [0x0f, 0x1f, 0x00]), "crosses its sixteen bytes");
        }
        assert_eq!(std::slice::from_raw_parts(page.add(46), 3), [0x0f, 0x01, 0xfa]);
        let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        VirtualQuery(page.cast(), &mut m, size_of_val(&m));
        assert_eq!(m.Protect, PAGE_EXECUTE_READ);
        VirtualFree(page.cast(), 0, MEM_RELEASE);
    }
}

/// `clzero` loops run as plain stores: the lines they name are zero and no other byte,
/// and the pointer, the scratch register and the flags come out as from the loop.
#[test]
fn a_clzero_loop_becomes_plain_stores() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};
    // Against rdx (rm rax, reg rdx) and against rcx, the scratch register.
    for (cmp, by) in [([0x48, 0x39, 0xd0], 2), ([0x48, 0x39, 0xc8], 1), ([0x4c, 0x39, 0xc0], 8)] {
        // SAFETY: memory of our own, written and run inside it.
        unsafe {
            let page = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize;
            assert_ne!(page, 0);
            // A function of (start, end): the start in rax, the end in its register, the
            // loop at 16, and what rax became.
            let mut head = vec![0x48, 0x89, 0xc8]; // mov rax, rcx
            match by {
                1 => head.extend([0x48, 0x89, 0xd1]),  // mov rcx, rdx
                8 => head.extend([0x49, 0x89, 0xd0]),  // mov r8, rdx
                _ => {}
            }
            head.resize(16, 0x90);
            let mut code = head;
            code.extend([0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40]);
            code.extend(cmp);
            code.extend([0x72, 0xf4]);
            code.push(0xc3);
            std::ptr::copy_nonoverlapping(code.as_ptr(), page as *mut u8, code.len());
            let mut context: CONTEXT = std::mem::zeroed();
            let mut block = vec![0xaau8; 64 * 8 + 64];
            let base = block.as_mut_ptr() as usize;
            let first = base.next_multiple_of(64);
            // The fault itself: emulated whole, and replaced.
            context.Rip = page as u64 + 16;
            context.Rax = first as u64;
            context.Rdx = first as u64 + 128;
            context.Rcx = context.Rdx;
            context.R8 = context.Rdx;
            assert_eq!(emulate(&mut context), CONTINUE_EXECUTION);
            assert_eq!((context.Rip, context.Rax), (page as u64 + 28, first as u64 + 128));
            if patches_clzero() {
                assert_eq!(*((page + 16) as *const u8), 0xe9, "a jump");
                block.fill(0xaa);
                let run: extern "system" fn(usize, usize) -> usize = std::mem::transmute(page);
                // From inside a line to inside the third: whole lines, one past the end.
                assert_eq!(run(first + 8, first + 130), first + 8 + 128);
                let at = first - base;
                assert!(block[at..at + 128].iter().all(|&b| b == 0));
                assert!(block[..at].iter().chain(&block[at + 128..]).all(|&b| b == 0xaa));
                // An end at the start: one line.
                block.fill(0xaa);
                assert_eq!(run(first, first), first + 64);
                assert!(block[at..at + 64].iter().all(|&b| b == 0) && block[at + 64] == 0xaa);
            }
            VirtualFree(page as *mut _, 0, MEM_RELEASE);
        }
    }
    assert!(clzero_stub(0x1000, 0x2000, [0x48, 0x39, 0xc0]).is_none(), "against rax itself");
    assert!(jump(0x1000, 0x9000_0000_0000).is_none(), "out of reach");
}
