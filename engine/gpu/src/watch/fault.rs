use super::BLOCK;
use super::guards::{PROTECTED, guards};
use super::mapping::came_back;
use super::protect::open;

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
pub(crate) fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        let _fault = crate::timeline::span("a fault looked at");
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const READ: usize = 0;
        const WRITE: usize = 1;
        // A fault of our own copies (`crate::safecopy`): whatever it is, if
        // it is not a block we guard, the copy fails and goes on from there.
        let ours = crate::safecopy::failing_exit(r.ExceptionAddress as usize);
        // SAFETY: as above.
        let context = unsafe { (*info).ContextRecord };
        let fail = move |exit: usize| {
            // SAFETY: the context the system passes, to go on from.
            unsafe { (*context).Rip = exit as u64 };
            EXCEPTION_CONTINUE_EXECUTION
        };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return match ours {
                Some(exit) => fail(exit),
                None if r.ExceptionCode == EXCEPTION_ACCESS_VIOLATION as NTSTATUS
                    && r.NumberParameters >= 2
                    && r.ExceptionInformation[0] == READ
                    && came_back(r.ExceptionInformation[1], false) =>
                {
                    EXCEPTION_CONTINUE_EXECUTION
                }
                None => EXCEPTION_CONTINUE_SEARCH,
            };
        }
        let at = r.ExceptionInformation[1];
        let block = at / BLOCK;
        if ours.is_none()
            && PROTECTED.any(block, block + 1)
            // SAFETY: the context the system passes, to go on from.
            && crate::titlewrite::store(unsafe { &mut *context }, at)
        {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        // Another thread may have opened the block since this one faulted
        // (then nothing waits for the lock), and a copy may have guarded it
        // again since that (then it is ours again, not read-only for some
        // other reason).
        for _ in 0..3 {
            if !PROTECTED.any(block, block + 1) && writable(at) {
                #[cfg(feature = "diagnostics")]
                crate::faultbook::note(crate::faultbook::What::Open, block, 0, 0);
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            #[cfg(feature = "diagnostics")]
            let began = std::time::Instant::now();
            // (Let go again before the next look: the runtime wants the
            // guards to end a change.)
            let mut g = guards();
            #[cfg(feature = "diagnostics")]
            let waited = began.elapsed();
            let opened = open(&mut g, at, at + 1);
            if opened != 0 {
                g.opened(block);
                drop(g);
                #[cfg(feature = "diagnostics")]
                {
                    crate::copystat::add(crate::copystat::C::GuardFaults, 1);
                    let ns = waited.as_nanos().min(u128::from(u32::MAX)) as usize;
                    let held = (began.elapsed() - waited).as_nanos() as u64;
                    crate::faultbook::note(crate::faultbook::What::Fault, block, opened << 32 | ns, held);
                    crate::faultbook::note(crate::faultbook::What::Site, r.ExceptionAddress as usize, block, 0);
                }
                return EXCEPTION_CONTINUE_EXECUTION;
            }
        }
        if writable(at) {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        match ours {
            Some(exit) => fail(exit),
            None if came_back(at, true) => EXCEPTION_CONTINUE_EXECUTION,
            None => EXCEPTION_CONTINUE_SEARCH,
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
pub(super) fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}
