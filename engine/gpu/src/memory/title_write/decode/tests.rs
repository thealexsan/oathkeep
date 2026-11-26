use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

use super::exec::{CF, OF, PF, SF, ZF};
use super::{Register, general_mut, store};

fn context() -> CONTEXT {
    // SAFETY: plain data.
    unsafe { std::mem::zeroed() }
}

/// The instruction's address, size and length, with every register its
/// number times 0x10000.
fn decoded(code: &[u8]) -> Option<(usize, usize, usize)> {
    let mut c = context();
    for n in 0..16 {
        *general_mut(&mut c, n) = n as u64 * 0x10000;
    }
    c.Rip = 0x7000_0000;
    let mut padded = code.to_vec();
    padded.resize(15, 0xcc);
    store(&padded, &c).map(|s| (s.at, s.size, s.len))
}

#[test]
fn stores_are_told_by_their_bytes() {
    // mov [rcx+rax+48h], rbx
    assert_eq!(decoded(&[0x48, 0x89, 0x5c, 0x01, 0x48]), Some((0x10048, 8, 5)));
    // mov [rbx], rdx
    assert_eq!(decoded(&[0x48, 0x89, 0x13]), Some((0x30000, 8, 3)));
    // or byte [r9+9eh], 1
    assert_eq!(decoded(&[0x41, 0x80, 0x89, 0x9e, 0, 0, 0, 1]), Some((0x9009e, 1, 8)));
    // lock dec dword [rcx+10h]
    assert_eq!(decoded(&[0xf0, 0xff, 0x49, 0x10]), Some((0x10010, 4, 4)));
    // lock xadd [r10+r12*8-1], eax
    assert_eq!(decoded(&[0xf0, 0x43, 0x0f, 0xc1, 0x44, 0xe2, 0xff]), Some((0xa0000 + 0xc0000 * 8 - 1, 4, 7)));
    // mov word [rsp], 1234h
    assert_eq!(decoded(&[0x66, 0xc7, 0x04, 0x24, 0x34, 0x12]), Some((0x40000, 2, 6)));
    // mov [rip+10h], al; mov byte [rip+10h], 5 (counted from its end)
    assert_eq!(decoded(&[0x88, 0x05, 0x10, 0, 0, 0]), Some((0x7000_0016, 1, 6)));
    assert_eq!(decoded(&[0xc6, 0x05, 0x10, 0, 0, 0, 5]), Some((0x7000_0017, 1, 7)));
    // mov [12345678h], eax (no base)
    assert_eq!(decoded(&[0x89, 0x04, 0x25, 0x78, 0x56, 0x34, 0x12]), Some((0x1234_5678, 4, 7)));
    // vmovaps [rsi+10h], xmm4; vmovaps [r12], xmm3; movups [rax], xmm9
    assert_eq!(decoded(&[0xc5, 0xf8, 0x29, 0x66, 0x10]), Some((0x60010, 16, 5)));
    assert_eq!(decoded(&[0xc4, 0xc1, 0x78, 0x29, 0x1c, 0x24]), Some((0xc0000, 16, 6)));
    assert_eq!(decoded(&[0x44, 0x0f, 0x11, 0x08]), Some((0, 16, 4)));
    // vmovss [rax], xmm0; movlps, movhps, vmovq, movd
    assert_eq!(decoded(&[0xc5, 0xfa, 0x11, 0x00]), Some((0, 4, 4)));
    assert_eq!(decoded(&[0x0f, 0x13, 0x00]), Some((0, 8, 3)));
    assert_eq!(decoded(&[0xc5, 0xf9, 0xd6, 0x00]), Some((0, 8, 4)));
    assert_eq!(decoded(&[0x66, 0x0f, 0x7e, 0x00]), Some((0, 4, 4)));
    // Not stores of these: a load, a register's, a string instruction, a
    // thread's own memory, a compare, adc.
    assert_eq!(decoded(&[0x48, 0x8b, 0x13]), None);
    assert_eq!(decoded(&[0x48, 0x89, 0xd3]), None);
    assert_eq!(decoded(&[0xf3, 0xa4]), None);
    assert_eq!(decoded(&[0x65, 0x48, 0x89, 0x13]), None);
    assert_eq!(decoded(&[0x83, 0x38, 1]), None);
    assert_eq!(decoded(&[0x11, 0x00]), None);
    // blend by a mask (vmaskmov is of another map), vmovss with a third
    // register.
    assert_eq!(decoded(&[0xc4, 0xe2, 0x79, 0x2e, 0x00]), None);
}

/// What the processor makes of `code` with rax as the address, rcx and
/// rdx as operands; against [`Store::run`].
fn against_the_processor(code: &[u8], seed: u64) {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc};
    static PLACE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: fresh memory for code.
    let place = *PLACE.get_or_init(|| unsafe {
        VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize
    });
    let spread = |n: u64| {
        let v = (seed + n).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ seed << 7;
        // Small, edge and large ones.
        match v >> 61 {
            0 => 0,
            1 => v & 1,
            2 => u64::MAX,
            3 => 0x8000_0000_0000_0000u64 | 0x8000 | 0x80 | 0x8000_0000,
            _ => v,
        }
    };
    // f(address, operand, compared, flags, out):
    // mov r11, rcx ; the address (its instruction takes [r11])
    // mov rax, r8 ; mov r10, r9 ; push r10 ; popfq ; <code> ; pushfq ; pop r10
    // mov rcx, [rsp+28h] ; mov [rcx], r10 ; mov [rcx+8], rdx ; mov [rcx+16], rax ; ret
    let mut f = vec![0x49, 0x89, 0xcb, 0x4c, 0x89, 0xc0, 0x41, 0x51, 0x9d];
    f.extend(code);
    f.extend([0x9c, 0x41, 0x5a, 0x48, 0x8b, 0x4c, 0x24, 0x28]);
    f.extend([0x4c, 0x89, 0x11, 0x48, 0x89, 0x51, 0x08, 0x48, 0x89, 0x41, 0x10, 0xc3]);
    type F = unsafe extern "system" fn(*mut u64, u64, u64, u64, *mut [u64; 3]);
    let (there, operand, compared) = (spread(1), spread(2), if seed % 3 == 0 { spread(1) } else { spread(3) });
    let flags = 0x202 | (spread(4) & 0x8d5) as u32;
    let (mut memory, mut out) = (there, [0; 3]);
    // SAFETY: the code above, which keeps to the calling convention.
    unsafe {
        std::ptr::copy_nonoverlapping(f.as_ptr(), place as *mut u8, f.len());
        let f: F = std::mem::transmute(place);
        f(&mut memory, operand, compared, u64::from(flags), &mut out);
    }
    let mut c = context();
    let mut ours = there;
    (c.R11, c.Rdx, c.Rax, c.EFlags) = (&raw mut ours as u64, operand, compared, flags);
    let mut padded = code.to_vec();
    padded.resize(15, 0xcc);
    let store = store(&padded, &c).unwrap_or_else(|| panic!("{code:x?}"));
    assert_eq!((store.at, store.len), (&raw mut ours as usize, code.len()), "{code:x?}");
    // SAFETY: our word.
    assert!(unsafe { store.run(&mut c, &raw mut ours as usize) });
    assert_eq!(ours, memory, "{code:x?} of {there:x} with {operand:x}");
    assert_eq!((c.Rdx, c.Rax), (out[1], out[2]), "{code:x?} of {there:x} with {operand:x}, {compared:x}");
    // (`and`, `or` and `xor` leave AF as they like.)
    const SEEN: u32 = CF | PF | ZF | SF | OF;
    assert_eq!(c.EFlags & SEEN, out[0] as u32 & SEEN, "{code:x?} of {there:x} with {operand:x}");
}

#[test]
fn stores_do_what_the_processor_does() {
    let mut codes: Vec<Vec<u8>> = Vec::new();
    // op [r11], rdx in all sizes; op [r11], constant; inc, dec; xchg, xadd, cmpxchg.
    for op in [0x00, 0x08, 0x20, 0x28, 0x30, 0x88, 0x86] {
        codes.push(vec![0x41, op, 0x13]);
        codes.push(vec![0x66, 0x41, op | 1, 0x13]);
        codes.push(vec![0x41, op | 1, 0x13]);
        codes.push(vec![0x49, op | 1, 0x13]);
        if op < 0x80 {
            codes.push(vec![0xf0, 0x41, op | 1, 0x13]);
            codes.push(vec![0xf0, 0x49, op | 1, 0x13]);
        }
    }
    for kind in [0u8, 1, 4, 5, 6] {
        let modrm = 0x03 | kind << 3;
        for constant in [1, 0x7f, 0x80, 0xff] {
            codes.push(vec![0x41, 0x80, modrm, constant]);
            codes.push(vec![0x41, 0x83, modrm, constant]);
            codes.push(vec![0x49, 0x83, modrm, constant]);
            codes.push(vec![0x66, 0x41, 0x83, modrm, constant]);
            codes.push(vec![0xf0, 0x41, 0x83, modrm, constant]);
            codes.push(vec![0x49, 0x81, modrm, 3, 0, 0, constant]);
            codes.push(vec![0x66, 0x41, 0x81, modrm, 3, constant]);
        }
    }
    for modrm in [0x03, 0x0b] {
        codes.push(vec![0x41, 0xfe, modrm]);
        codes.push(vec![0x41, 0xff, modrm]);
        codes.push(vec![0x49, 0xff, modrm]);
        codes.push(vec![0xf0, 0x41, 0xff, modrm]);
        codes.push(vec![0x66, 0x41, 0xff, modrm]);
    }
    for op in [0xb0, 0xc0] {
        codes.push(vec![0x41, 0x0f, op, 0x13]);
        codes.push(vec![0xf0, 0x41, 0x0f, op, 0x13]);
        codes.push(vec![0xf0, 0x66, 0x41, 0x0f, op | 1, 0x13]);
        codes.push(vec![0xf0, 0x41, 0x0f, op | 1, 0x13]);
        codes.push(vec![0xf0, 0x49, 0x0f, op | 1, 0x13]);
    }
    codes.push(vec![0x41, 0xc6, 0x03, 0x99]);
    codes.push(vec![0x66, 0x41, 0xc7, 0x03, 0x99, 0x88]);
    codes.push(vec![0x41, 0xc7, 0x03, 1, 2, 3, 0x84]);
    codes.push(vec![0x49, 0xc7, 0x03, 1, 2, 3, 0x84]);
    for code in &codes {
        for seed in 0..200 {
            against_the_processor(code, seed);
        }
    }
}

/// The second byte of a register, and 32 bits that clear the rest.
#[test]
fn registers_are_read_and_written_as_by_the_processor() {
    let mut c = context();
    c.Rax = 0x1122_3344_5566_7788;
    assert_eq!(Register::of(4, 1, false).get(&c) & 0xff, 0x77, "ah");
    assert_eq!(Register::of(4, 1, true), Register { number: 4, high: false }, "spl");
    Register { number: 0, high: true }.put(&mut c, 0xff, 1);
    assert_eq!(c.Rax, 0x1122_3344_5566_ff88);
    Register { number: 0, high: false }.put(&mut c, 0xabcd, 2);
    assert_eq!(c.Rax, 0x1122_3344_5566_abcd);
    Register { number: 0, high: false }.put(&mut c, u64::MAX, 4);
    assert_eq!(c.Rax, 0xffff_ffff);
}
