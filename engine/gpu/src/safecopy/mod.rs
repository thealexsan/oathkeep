//! Copies between our memory and the title's that fail instead of faulting.
//!
//! Title memory may be unmapped, remapped or made read-only by the title's
//! other threads while we copy it, so copies went through the kernel
//! (`ReadProcessMemory`, `NtWriteVirtualMemory`), which reports a missing
//! page instead of faulting: some 35,000 calls a frame, each of which locks
//! the pages it copies, 39 ms of kernel time on the submitting thread alone.
//!
//! [`copy`] is one `rep movsb`. A fault in it is taken by the handler of
//! [`crate::watch`] (`on_write`, the first in the process), which opens a
//! block we guard ourselves and lets the copy go on, and otherwise sends it
//! to its failing exit. `OATHKEEP_SAFE_COPY=0`: through the kernel, as before.

use std::sync::OnceLock;

std::arch::global_asm!(
    ".globl victory_safe_copy",
    ".globl victory_safe_copy_at",
    ".globl victory_safe_copy_after",
    ".globl victory_safe_copy_failed",
    // rcx destination, rdx source, r8 bytes; eax: whether all were copied.
    // A leaf that leaves the stack alone (the two registers it must give
    // back are kept in volatile ones), so a stack walk through it works.
    "victory_safe_copy:",
    "mov r10, rsi",
    "mov r11, rdi",
    "mov rdi, rcx",
    "mov rsi, rdx",
    "mov rcx, r8",
    "victory_safe_copy_at:",
    "rep movsb",
    "victory_safe_copy_after:",
    "mov eax, 1",
    "mov rsi, r10",
    "mov rdi, r11",
    "ret",
    "victory_safe_copy_failed:",
    "xor eax, eax",
    "mov rsi, r10",
    "mov rdi, r11",
    "ret",
    ".globl victory_safe_put",
    ".globl victory_safe_put_at",
    ".globl victory_safe_put_after",
    ".globl victory_safe_put_failed",
    // rcx destination, rdx what was there, r8 what is to be there, r9 blocks
    // of eight dwords: stores the dwords in which the two differ, and no
    // other byte. eax: whether all could be stored.
    "victory_safe_put:",
    "test r9, r9",
    "jz 4f",
    "vpcmpeqd ymm3, ymm3, ymm3",
    "2:",
    "vmovdqu ymm1, ymmword ptr [r8]",
    "vpcmpeqd ymm2, ymm1, ymmword ptr [rdx]",
    "vmovmskps eax, ymm2",
    "cmp eax, 255",
    "je 3f",
    "vpxor ymm2, ymm2, ymm3",
    "victory_safe_put_at:",
    "vpmaskmovd ymmword ptr [rcx], ymm2, ymm1",
    "victory_safe_put_after:",
    "nop",
    "3:",
    "add rcx, 32",
    "add rdx, 32",
    "add r8, 32",
    "dec r9",
    "jnz 2b",
    "vzeroupper",
    "4:",
    "mov eax, 1",
    "ret",
    "victory_safe_put_failed:",
    "vzeroupper",
    "xor eax, eax",
    "ret",
    ".globl victory_safe_merge",
    ".globl victory_safe_merge_at",
    ".globl victory_safe_merge_after",
    ".globl victory_safe_merge_failed",
    // rcx title memory, rdx what the card holds, r8 what it held when last
    // looked at, r9 blocks of 32 bytes, fifth where the two counts go. Where
    // a byte of the second differs from the third, it goes into title memory
    // (as whole dwords, and only dwords that then differ from what is there)
    // and into the third; each block of title memory is read just before it
    // is written. rax: the blocks left when a page of title memory could not
    // be read or written (0: all done). The counts: blocks in which the
    // second and third differed, dwords stored in title memory.
    "victory_safe_merge:",
    "xor r10d, r10d",
    "xor r11d, r11d",
    "test r9, r9",
    "jz 6f",
    "vpcmpeqd ymm5, ymm5, ymm5",
    "2:",
    "cmp r9, 4",
    "jb 8f",
    "vmovdqu ymm0, ymmword ptr [rdx]",
    "vpxor ymm0, ymm0, ymmword ptr [r8]",
    "vmovdqu ymm1, ymmword ptr [rdx + 32]",
    "vpxor ymm1, ymm1, ymmword ptr [r8 + 32]",
    "vpor ymm1, ymm0, ymm1",
    "vmovdqu ymm2, ymmword ptr [rdx + 64]",
    "vpxor ymm2, ymm2, ymmword ptr [r8 + 64]",
    "vmovdqu ymm3, ymmword ptr [rdx + 96]",
    "vpxor ymm3, ymm3, ymmword ptr [r8 + 96]",
    "vpor ymm2, ymm2, ymm3",
    "vpor ymm0, ymm1, ymm2",
    "vptest ymm0, ymm0",
    "jnz 9f",
    "add rcx, 128",
    "add rdx, 128",
    "add r8, 128",
    "sub r9, 4",
    "jnz 2b",
    "jmp 6f",
    "9:",
    "vptest ymm1, ymm1",
    "jnz 8f",
    "add rcx, 64",
    "add rdx, 64",
    "add r8, 64",
    "sub r9, 2",
    "8:",
    "vmovdqu ymm0, ymmword ptr [rdx]",
    "vpcmpeqb ymm1, ymm0, ymmword ptr [r8]",
    "vpmovmskb eax, ymm1",
    "cmp eax, -1",
    "je 5f",
    "victory_safe_merge_at:",
    "vmovdqu ymm2, ymmword ptr [rcx]",
    "vpblendvb ymm3, ymm0, ymm2, ymm1",
    "vpcmpeqd ymm4, ymm3, ymm2",
    "vmovmskps eax, ymm4",
    "xor eax, 255",
    "jz 4f",
    "cmp eax, 255",
    "je 3f",
    "vpxor ymm4, ymm4, ymm5",
    "vpmaskmovd ymmword ptr [rcx], ymm4, ymm3",
    "jmp 7f",
    "3:",
    "vmovdqu ymmword ptr [rcx], ymm3",
    "victory_safe_merge_after:",
    "7:",
    "popcnt eax, eax",
    "add r11, rax",
    "4:",
    "inc r10",
    "vmovdqu ymmword ptr [r8], ymm0",
    "5:",
    "add rcx, 32",
    "add rdx, 32",
    "add r8, 32",
    "dec r9",
    "jnz 2b",
    "6:",
    "victory_safe_merge_failed:",
    "vzeroupper",
    "mov rax, qword ptr [rsp + 40]",
    "mov qword ptr [rax], r10",
    "mov qword ptr [rax + 8], r11",
    "mov rax, r9",
    "ret",
);

unsafe extern "system" {
    fn victory_safe_copy(dst: *mut u8, src: *const u8, len: usize) -> u32;
    fn victory_safe_copy_at();
    fn victory_safe_copy_after();
    fn victory_safe_copy_failed();
    fn victory_safe_put(dst: *mut u8, was: *const u8, now: *const u8, blocks: usize) -> u32;
    fn victory_safe_put_at();
    fn victory_safe_put_after();
    fn victory_safe_put_failed();
    fn victory_safe_merge(title: *mut u8, now: *const u8, held: *mut u8, blocks: usize, counts: *mut [u64; 2]) -> usize;
    fn victory_safe_merge_at();
    fn victory_safe_merge_after();
    fn victory_safe_merge_failed();
}

/// What [`merge`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Merged {
    /// Blocks of 32 bytes in which the card's bytes were new.
    pub(crate) new: u64,
    /// Dwords stored in title memory.
    pub(crate) dwords: u64,
    /// Whether title memory could be read and written wherever it had to be.
    pub(crate) there: bool,
}

/// What shaders wrote goes into title memory: every byte of `now` that
/// differs from the one in `held` goes to `title` and into `held`, every
/// other byte of both stays as it is. Title memory is read 32 bytes at a
/// time, each just before what changes of them is stored, and only the
/// dwords that change are stored (at ends that are no dword's, bytes): the
/// title's own threads may be writing those beside them.
///
/// Where title memory cannot be read or written, the rest of `held` takes
/// `now` if `take` says so (the caller has the card's bytes nowhere else),
/// else it stays.
///
/// # Safety
/// `now` and `held` are `len` bytes of ours, not overlapping, that nothing
/// else writes (but for `now` the card: it is read once); `title` is as
/// [`copy`]'s. The stretch lies in one page of 4 KiB of title memory. [`on`]
/// must have been asked.
pub(crate) unsafe fn merge(title: *mut u8, now: *const u8, held: *mut u8, len: usize, take: bool) -> Merged {
    let mut out = Merged { there: true, ..Merged::default() };
    let mut at = 0;
    while at < len {
        let address = title.addr() + at;
        let left = len - at;
        if !address.is_multiple_of(4) || left < 4 {
            // A dword only partly in the stretch: its bytes one by one.
            // SAFETY: inside the stretch.
            unsafe {
                let byte = now.add(at).read_volatile();
                let mut there = 0u8;
                if byte != held.add(at).read() {
                    out.new += 1;
                    let mut ok = copy(&mut there, title.add(at), 1);
                    if ok && there != byte {
                        ok = copy(title.add(at), &byte, 1);
                        out.dwords += u64::from(ok);
                    }
                    out.there &= ok;
                    if ok || take {
                        held.add(at).write(byte);
                    }
                }
            }
            at += 1;
            continue;
        }
        let into = address % 32;
        let mut counts = [0u64; 2];
        if into != 0 || left < 32 {
            // Part of a block: in a block of its own, equal beside the part.
            let n = (32 - into).min(left) & !3;
            let (mut new, mut old) = ([0u8; 32], [0u8; 32]);
            // SAFETY: inside the stretch; the block of title memory around
            // it lies in the same page, and nothing of it outside the part
            // is stored (the two arrays are equal there).
            unsafe {
                std::ptr::copy_nonoverlapping(now.add(at), new.as_mut_ptr().add(into), n);
                std::ptr::copy_nonoverlapping(held.add(at), old.as_mut_ptr().add(into), n);
                let stuck = victory_safe_merge(title.wrapping_add(at).wrapping_sub(into), new.as_ptr(), old.as_mut_ptr(), 1, &mut counts);
                out.there &= stuck == 0;
                let from = if stuck != 0 && take { new.as_ptr() } else { old.as_ptr() };
                std::ptr::copy_nonoverlapping(from.add(into), held.add(at), n);
            }
            at += n;
        } else {
            let blocks = left / 32;
            // SAFETY: whole blocks of the stretch.
            let stuck = unsafe { victory_safe_merge(title.add(at), now.add(at), held.add(at), blocks, &mut counts) };
            if stuck != 0 {
                out.there = false;
                if take {
                    let from = at + (blocks - stuck) * 32;
                    // SAFETY: the blocks that were not done.
                    unsafe { std::ptr::copy_nonoverlapping(now.add(from), held.add(from), stuck * 32) };
                }
            }
            at += blocks * 32;
        }
        out.new += counts[0];
        out.dwords += counts[1];
    }
    out
}

/// Whether the `len` bytes at `a` and `b` differ.
///
/// # Safety
/// Both are `len` bytes that can be read; the card may be writing them.
pub(crate) unsafe fn differ(a: *const u8, b: *const u8, len: usize) -> bool {
    use std::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_or_si256, _mm256_setzero_si256, _mm256_testz_si256, _mm256_xor_si256};
    /// # Safety
    /// [`differ`]'s; the processor has AVX2 (as the code above assumes).
    #[target_feature(enable = "avx2")]
    unsafe fn blocks(a: *const u8, b: *const u8, n: usize) -> bool {
        let mut at = 0;
        while at < n {
            let mut any = _mm256_setzero_si256();
            for k in at..n.min(at + 4) {
                // SAFETY: block `k`, inside both.
                let (x, y) = unsafe {
                    (_mm256_loadu_si256(a.add(k * 32).cast::<__m256i>()), _mm256_loadu_si256(b.add(k * 32).cast::<__m256i>()))
                };
                any = _mm256_or_si256(any, _mm256_xor_si256(x, y));
            }
            if _mm256_testz_si256(any, any) == 0 {
                return true;
            }
            at += 4;
        }
        false
    }
    // SAFETY: the caller's.
    if unsafe { blocks(a, b, len / 32) } {
        return true;
    }
    // SAFETY: the bytes behind the blocks.
    (len / 32 * 32..len).any(|i| unsafe { a.add(i).read_volatile() != b.add(i).read_volatile() })
}

/// Puts `now` at `dst` where it differs from `was`, whole dwords (as `dst`
/// is aligned) and not a byte beside them: other threads may be writing
/// those. At the ends, single bytes of a dword only partly in the stretch.
/// False if a page could not be written; then some may have been.
///
/// # Safety
/// As [`copy`]'s.
pub(crate) unsafe fn put_changed(dst: *mut u8, was: &[u8], now: &[u8]) -> bool {
    assert_eq!(was.len(), now.len());
    let n = now.len();
    let head = dst.addr().wrapping_neg() & 3;
    let head = head.min(n);
    let blocks = (n - head) / 32;
    let tail = head + blocks * 32;
    let mut all = true;
    if blocks > 0 {
        // SAFETY: the two slices hold the blocks; the destination's faults
        // leave through the failing exit.
        all &= unsafe { victory_safe_put(dst.add(head), was[head..].as_ptr(), now[head..].as_ptr(), blocks) != 0 };
    }
    let single = |from: usize, to: usize| {
        let mut all = true;
        let mut i = from;
        while i < to {
            // A whole aligned dword, or the bytes there are of one.
            let len = (4 - (dst.addr() + i) % 4).min(to - i);
            if was[i..i + len] != now[i..i + len] {
                // SAFETY: inside the stretch.
                all &= unsafe { copy(dst.add(i), now[i..].as_ptr(), len) };
            }
            i += len;
        }
        all
    };
    all &= single(0, head);
    all &= single(tail, n);
    all
}

/// Whether copies are made here and not by the kernel.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("OATHKEEP_SAFE_COPY").is_none_or(|v| v != "0");
        if on {
            crate::watch::install();
        }
        on
    })
}

/// Copies `len` bytes. False if a page of the source cannot be read or one
/// of the destination cannot be written; then some of the bytes may have
/// been copied.
///
/// # Safety
/// Of the two ranges, what is not title memory must be valid, and no Rust
/// reference may cover the destination. [`on`] must have been asked.
pub(crate) unsafe fn copy(dst: *mut u8, src: *const u8, len: usize) -> bool {
    // SAFETY: a fault on either side leaves through the failing exit
    // ([`failing_exit`]); the rest is the caller's.
    unsafe { victory_safe_copy(dst, src, len) != 0 }
}

/// For the fault handler: where a fault at instruction `at` is to go on, if
/// it is [`copy`]'s.
pub(crate) fn failing_exit(at: usize) -> Option<usize> {
    let (from, to) = (victory_safe_copy_at as *const () as usize, victory_safe_copy_after as *const () as usize);
    if (from..to).contains(&at) {
        return Some(victory_safe_copy_failed as *const () as usize);
    }
    let (from, to) = (victory_safe_put_at as *const () as usize, victory_safe_put_after as *const () as usize);
    if (from..to).contains(&at) {
        return Some(victory_safe_put_failed as *const () as usize);
    }
    let (from, to) = (victory_safe_merge_at as *const () as usize, victory_safe_merge_after as *const () as usize);
    (from..to).contains(&at).then_some(victory_safe_merge_failed as *const () as usize)
}

#[cfg(test)]
mod tests;
