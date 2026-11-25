use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use windows_sys::Win32::System::Diagnostics::Debug::{
    DebugBreak, FlushInstructionCache, IsDebuggerPresent,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnimplementedPolicy {
    #[default]
    Trap,
    ReturnZero,
}

#[derive(Debug)]
pub struct ImportSite {
    pub dll: String,
    pub symbol: String,
    pub policy: UnimplementedPolicy,
    pub calls: AtomicU64,
}

static UNIMPLEMENTED_CALLS: AtomicU64 = AtomicU64::new(0);

pub fn unimplemented_call_count() -> u64 {
    UNIMPLEMENTED_CALLS.load(Ordering::Relaxed)
}

extern "system" fn unimplemented_import(site: &'static ImportSite) -> u64 {
    UNIMPLEMENTED_CALLS.fetch_add(1, Ordering::Relaxed);
    let previous = site.calls.fetch_add(1, Ordering::Relaxed);
    match site.policy {
        UnimplementedPolicy::ReturnZero => {
            if previous == 0 {
                tracing::warn!(dll = %site.dll, symbol = %site.symbol, "unimplemented import called, returning 0");
            }
            0
        }
        UnimplementedPolicy::Trap => {
            tracing::error!(dll = %site.dll, symbol = %site.symbol, "unimplemented import called");
            let (dll, symbol) = (&site.dll, &site.symbol);
            let text = format!("Oathkeep: unimplemented import {dll}!{symbol} called");
            crate::log_crash(&text);
            // SAFETY: plain Win32 calls without preconditions.
            unsafe {
                if IsDebuggerPresent() != 0 {
                    DebugBreak();
                }
            }
            std::process::abort();
        }
    }
}

const THUNK_SIZE: usize = 32;

pub(crate) struct ThunkBlock {
    base: *mut u8,
    len: usize,
    used: usize,
}

impl ThunkBlock {
    pub(crate) fn new(count: usize) -> Result<Self> {
        let len = (count.max(1) * THUNK_SIZE).next_multiple_of(4096);
        // SAFETY: fresh anonymous allocation; no existing memory is touched.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as *mut u8;
        if base.is_null() {
            bail!(
                "VirtualAlloc for import stubs failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(ThunkBlock { base, len, used: 0 })
    }

    pub(crate) fn emit(&mut self, site: &'static ImportSite) -> Result<usize> {
        if self.used + THUNK_SIZE > self.len {
            bail!("import stub block is full");
        }
        let mut code = [0xCC_u8; THUNK_SIZE];
        code[0..2].copy_from_slice(&[0x48, 0xB9]);
        code[2..10].copy_from_slice(&(site as *const ImportSite as u64).to_le_bytes());
        code[10..12].copy_from_slice(&[0x48, 0xB8]);
        let handler = unimplemented_import as extern "system" fn(&'static ImportSite) -> u64;
        code[12..20].copy_from_slice(&(handler as *const () as u64).to_le_bytes());
        code[20..22].copy_from_slice(&[0xFF, 0xE0]);
        // SAFETY: `used + THUNK_SIZE <= len`, and the block is still writable.
        let at = unsafe {
            let at = self.base.add(self.used);
            std::ptr::copy_nonoverlapping(code.as_ptr(), at, THUNK_SIZE);
            at
        };
        self.used += THUNK_SIZE;
        Ok(at as usize)
    }

    pub(crate) fn seal(self) -> Result<()> {
        let mut old = 0;
        // SAFETY: `base..base+len` is the allocation made in `new`.
        let ok = unsafe { VirtualProtect(self.base as _, self.len, PAGE_EXECUTE_READ, &mut old) };
        if ok == 0 {
            bail!(
                "VirtualProtect for import stubs failed: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: flushing our own process's instruction cache for our own block.
        unsafe { FlushInstructionCache(GetCurrentProcess(), self.base as _, self.len) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trap_child() {
        let Some(log) = std::env::var_os("OATHKEEP_TRAP_LOG") else {
            return;
        };
        crate::log_crashes_to(log.into());
        let site = Box::leak(Box::new(ImportSite {
            dll: "probe.dll".into(),
            symbol: "Missing".into(),
            policy: UnimplementedPolicy::Trap,
            calls: AtomicU64::new(0),
        }));
        unimplemented_import(site);
    }

    #[test]
    fn a_trapped_import_is_in_the_crash_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crashes.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "thunks::tests::trap_child", "--nocapture"])
            .env("OATHKEEP_TRAP_LOG", &log)
            .output()
            .unwrap();
        assert!(!out.status.success(), "the trap ends the process");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let named = text.contains("Oathkeep: unimplemented import probe.dll!Missing called");
        assert!(named, "crash log: {text:?}");
    }
}
