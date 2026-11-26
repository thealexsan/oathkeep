use windows_sys::Win32::Foundation::ERROR_COMMITMENT_LIMIT;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualFree,
};
use windows_sys::Win32::System::ProcessStatus::PERFORMANCE_INFORMATION;

use super::super::MIB;
use super::super::alloc::{
    ALIGNMENT_SHIFT, ALLOC_32BIT, BasicInformation, GRAPHICS, PAGES_2M, XMemAlloc, XMemFree,
    XMemVirtualAlloc,
};
use super::super::stats::{WorkingSet, XMemGetWorkingSetStatistics, XMemTransferMemory, commit_limit_message};
use super::query;

#[test]
fn alloc_honours_alignment_and_frees() {
    for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
        let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
        let p = XMemAlloc(100, attributes);
        assert!(!p.is_null());
        assert_eq!(p as usize % align, 0, "field {field:#x}");
        // SAFETY: 100 bytes were allocated.
        unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
        XMemFree(p, attributes);
    }
    let p = XMemAlloc(4096, ALLOC_32BIT);
    assert!(!p.is_null() && (p as usize) < 1 << 32);
    XMemFree(p, ALLOC_32BIT);
}

#[test]
fn virtual_alloc_meets_console_constraints() {
    const GPU_READ_WRITE: u32 = 0x2000;
    let p = XMemVirtualAlloc(
        std::ptr::null_mut(),
        4 * MIB,
        PAGES_2M | MEM_RESERVE | MEM_COMMIT,
        GRAPHICS | 7 << 12, // 40-bit addresses
        PAGE_READWRITE | GPU_READ_WRITE,
    );
    assert!(!p.is_null());
    assert_eq!(p as usize % (2 * MIB), 0);
    assert!((p as usize) < 1 << 40);
    let info = query(p);
    assert_eq!(info.host.State, MEM_COMMIT);
    assert_eq!(info.host.Protect, PAGE_READWRITE);
    assert_eq!(info.flags & GRAPHICS, GRAPHICS);
    // Write-watched, for the GPU side's copies.
    let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
    // SAFETY: room for `count` addresses.
    let watched = unsafe {
        windows_sys::Win32::System::Memory::GetWriteWatch(
            0,
            p,
            4 * MIB,
            found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    };
    assert_eq!(watched, 0);
    // SAFETY: our own reservation.
    assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    assert_eq!(query(p).flags, 0);
    // No address field: above the 32-bit range and below 2^40, where
    // the console keeps it.
    let p = XMemVirtualAlloc(
        std::ptr::null_mut(),
        16 << 30,
        MEM_RESERVE,
        GRAPHICS,
        PAGE_NOACCESS,
    );
    assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
    assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
    // SAFETY: our own reservation.
    assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
}

#[test]
fn transfers_shrink_the_title_budget() {
    let mut before = WorkingSet::default();
    assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
    assert!(before.title_used > 0);
    assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
    assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
    let standard = oathkeep_gpu::console::current().title_memory().1;
    assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
    let mut after = WorkingSet::default();
    XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
    assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
}

#[test]
fn statistics_have_the_console_layout() {
    assert_eq!(size_of::<WorkingSet>(), 0x50);
    assert_eq!(size_of::<BasicInformation>(), 0x40);
    assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
}

#[test]
fn commit_limit_report_includes_request_and_system_commit() {
    let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
    let info = PERFORMANCE_INFORMATION {
        CommitTotal: 512,
        CommitLimit: 768,
        PageSize: 4096,
        ..Default::default()
    };
    let report = commit_limit_message(4 * MIB, "physical pages", &error, Some(&info));
    assert!(report.contains("4194304 bytes of physical pages"));
    assert!(report.contains("2 MiB of 3 MiB in use"));
    let fallback = commit_limit_message(4 * MIB, "virtual memory", &error, None);
    assert!(fallback.contains("4194304 bytes of virtual memory"));
    for report in [&report, &fallback] {
        assert!(report.contains("os error 1455"));
        assert!(report.contains("Windows paging file size"));
    }
    assert!(!fallback.contains("System commit:"));
}
