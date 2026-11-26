/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true);
    there == of
}

/// Whether the two questions above and below are put to `VirtualQuery`, as
/// they were (`OATHKEEP_COMMITTED_QUERY=1`).
fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_COMMITTED_QUERY").is_some_and(|v| v == "1"))
}

/// Of the kernel's pages that hold `[at, at + len)` (`len > 0`): how many
/// can be read, and how many there are; with `all`, it stops at the first
/// that cannot.
fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for (i, entry) in info[..n].iter().enumerate() {
            // Bit 0 of the answer: the page is in the working set.
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            if valid || crate::backend::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

fn committed_by_query(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there.
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    static BY_VIEWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *BY_VIEWS.get_or_init(|| std::env::var_os("OATHKEEP_MAPPED_BY_VIEWS").is_none_or(|v| v != "0"))
        && let Some(there) = crate::memory::inplace::shown_bytes(start, len)
    {
        return there;
    }
    if !by_query() {
        static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *BY_PAGE.get_or_init(|| std::env::var_os("OATHKEEP_MAPPED_BY_PAGE").is_some_and(|v| v == "1")) {
            if !committed(at, 1) && !committed((end - 1) as *const u8, 1) {
                return 0;
            }
            let (there, _) = readable(start, len, false);
            return (there as u64 * 4096).min(len as u64);
        }
        use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: plain win32 handle query and zeroed structs.
        let proc_handle = unsafe { GetCurrentProcess() };
        let mut edge_info: [PSAPI_WORKING_SET_EX_INFORMATION; 2] = unsafe { std::mem::zeroed() };
        edge_info[0].VirtualAddress = (start & !0xfff) as *mut _;
        edge_info[1].VirtualAddress = ((end - 1) & !0xfff) as *mut _;
        let edge_n = if edge_info[0].VirtualAddress == edge_info[1].VirtualAddress { 1 } else { 2 };
        // SAFETY: valid buffer of `edge_n` entries.
        let edge_asked = unsafe {
            K32QueryWorkingSetEx(
                proc_handle,
                edge_info.as_mut_ptr().cast(),
                (edge_n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        let mut any_edge = false;
        for (idx, entry) in edge_info[..edge_n].iter().enumerate() {
            // SAFETY: plain bitfield read.
            let valid = edge_asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let addr = if idx == 0 { start } else { end - 1 };
            let mut byte = [0u8];
            if valid || crate::backend::read_into(addr as *const u8, &mut byte) {
                any_edge = true;
                break;
            }
        }
        if !any_edge {
            return 0;
        }
        const BLOCK: usize = 64 << 10;
        let mut there = 0u64;
        let mut block = start & !(BLOCK - 1);
        // SAFETY: plain zeroed structs.
        let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
        let mut spans = [(0usize, 0u64); 256];
        while block < end {
            let mut n = 0usize;
            while block < end && n < info.len() {
                let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
                info[n].VirtualAddress = (lo & !0xfff) as *mut _;
                spans[n] = (lo, (hi - lo) as u64);
                n += 1;
                block = block.saturating_add(BLOCK);
            }
            // SAFETY: valid buffer of `n` entries.
            let asked = unsafe {
                K32QueryWorkingSetEx(
                    proc_handle,
                    info.as_mut_ptr().cast(),
                    (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
                )
            } != 0;
            for i in 0..n {
                // SAFETY: plain bitfield read.
                let valid = asked && unsafe { info[i].VirtualAttributes.Flags } & 1 != 0;
                let mut byte = [0u8];
                if valid || crate::backend::read_into(spans[i].0 as *const u8, &mut byte) {
                    there += spans[i].1;
                }
            }
        }
        return there;
    }
    if !committed(at, 1) && !committed((end - 1) as *const u8, 1) {
        return 0;
    }
    let (mut p, mut there) = (start, 0u64);
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(end);
        if n == 0 || next <= p {
            break;
        }
        if info.State == MEM_COMMIT {
            there += (next - p) as u64;
        }
        p = next;
    }
    there
}
