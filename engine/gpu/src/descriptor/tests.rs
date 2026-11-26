use super::view::{CONSTANT, CONSTANT_ADDRESS_BITS, KIND_SHIFT, VIEWS, constant_words, inline, view_at};
use super::*;
use crate::object::{S_OK, slot};

/// What newer titles pass; tests use it throughout, so that reading the
/// placement stays inside the value.
#[repr(C)]
struct Extended(HeapDesc, Placement);

fn create(desc: HeapDesc, placement: Placement, out: &mut This) -> HRESULT {
    let extended = Extended(desc, placement);
    create_heap(std::ptr::null(), &extended.0, &HEAP_IIDS[0], out)
}

#[cfg(windows)]
#[test]
fn raw_heap_sync_stops_before_dynamic_host_slots() {
    let start = 0x1000usize;
    let step = 32;
    let capped = 5;
    assert_eq!(
        super::sync::changed_slots(
            start + 3 * step + 4,
            start + 4 * step + 8,
            start,
            step as u64,
            capped
        ),
        3..5
    );
    assert_eq!(
        super::sync::changed_slots(
            start + 6 * step,
            start + 9 * step,
            start,
            step as u64,
            capped
        ),
        5..5
    );
}

#[test]
fn heaps_hold_count_descriptors_from_their_start() {
    let null = std::ptr::null();
    let desc = HeapDesc {
        kind: 0,
        count: 1000,
        flags: 1,
        nodes: 0,
    };
    let none = Placement {
        address: 0,
        size: 0,
    };
    let mut h = null;
    assert_eq!(create(desc, none, &mut h), S_OK);
    let cpu = slot::<extern "system" fn(This) -> u64>(h, 0x48)(h);
    assert_eq!(cpu % 64, 0);
    assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), cpu);
    assert_eq!(increment_size(null, 0), 32);
    // SAFETY: the heap holds 1000 descriptors of 32 bytes.
    unsafe { (cpu as *mut u8).add(999 * 32 + 31).write(1) };
    assert_eq!(increment_size(null, 9), 0);
    let bad = HeapDesc { kind: 9, ..desc };
    assert_eq!(create(bad, none, &mut h), E_INVALIDARG);
}

/// A descriptor copied into a placed heap as bytes is mirrored before
/// the next submission, even though no descriptor method saw the copy.
#[cfg(windows)]
#[test]
fn records_copied_as_bytes_are_made_at_the_next_sync() {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    let memory = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            crate::watch::PAGE,
            MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
            PAGE_READWRITE,
        )
    };
    assert!(!memory.is_null());
    let desc = HeapDesc {
        kind: SAMPLERS,
        count: 64,
        flags: SHADER_VISIBLE,
        nodes: 1,
    };
    let placement = Placement {
        address: memory as u64,
        size: 64 * 16,
    };
    let mut h: This = std::ptr::null();
    assert_eq!(create(desc, placement, &mut h), S_OK);
    if let Some(host) = heap(h).host() {
        let mut elsewhere = [0u64; 2];
        let source = elsewhere.as_mut_ptr() as u64;
        let sampler = [21, 3, 3, 3, 0, 1, 0, 0, 0, 0, 0, 0x7f7f_ffff, 0];
        create_sampler(std::ptr::null(), &sampler, source);
        // SAFETY: the heap slot and source both hold 16 descriptor bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source as *const u8,
                memory.cast::<u8>().add(5 * 16),
                16,
            )
        };
        assert!(!host.made(5));
        sync_heaps();
        assert!(host.made(5));
    }
    slot::<extern "system" fn(This) -> u32>(h, 0x10)(h);
    // SAFETY: this allocation belongs to the test.
    unsafe { VirtualFree(memory, 0, MEM_RELEASE) };
}

#[test]
fn views_survive_copies() {
    let null = std::ptr::null();
    let mut heap = vec![0u64; 4 * 8]; // four 32-byte slots
    let at = |i: usize| heap.as_ptr() as u64 + 32 * i as u64;
    let (s0, s1, s2, s3) = (at(0), at(1), at(2), at(3));
    create_cbv(null, &[0x1000, 1, 256, 0], s0);
    assert_eq!(view(s0), Some(View::Constant(0x1_0000_1000, 256)));
    create_srv(null, null, std::ptr::null(), s1);
    assert_eq!(view(s1), Some(View::Shader(0, None)));
    // The same view twice is one entry.
    create_srv(null, null, std::ptr::null(), s2);
    // SAFETY: both slots are in `heap`.
    assert_eq!(unsafe { *(s1 as *const [u32; 3]) }, unsafe {
        *(s2 as *const [u32; 3])
    });
    // Two ranges of one into one range of two.
    let (dst, dst_n, src, src_n) = ([s2], [2u32], [s0, s1], [1u32, 1]);
    copy(
        null,
        1,
        dst.as_ptr(),
        dst_n.as_ptr(),
        2,
        src.as_ptr(),
        src_n.as_ptr(),
        0,
    );
    assert_eq!(view(s2), view(s0));
    assert_eq!(view(s3), view(s1));
    assert_eq!(view(at(0) + 8), None);
    heap[0] = 0;
}

/// Constant buffer views are named whole in their records: any address
/// a program can have, any size below 16 KiB, multiples of 256 bytes
/// up to 4 MiB; what does not fit says so.
#[test]
fn constant_views_fit_their_records() {
    let top = (1u64 << CONSTANT_ADDRESS_BITS) - 1;
    for address in [0u64, 0x100, 0x2602_0000, 0xd_6b60_0000, 0x7ffd_a34f_0000, top] {
        for size in [0u32, 1, 255, 256, 4000, (1 << 14) - 1, 1 << 14, 65536, (1 << 22) - 256] {
            let [i, generation] = constant_words(address, size).expect("fits");
            assert_eq!(generation >> KIND_SHIFT, CONSTANT);
            assert_eq!(view_at(i, generation), Some(View::Constant(address, size)), "{address:#x} {size}");
        }
    }
    assert_eq!(constant_words(top + 1, 256), None, "an address past 47 bits");
    assert_eq!(constant_words(0x1000, (1 << 14) + 1), None, "16 KiB or more, no multiple of 256");
    assert_eq!(constant_words(0x1000, 1 << 22), None, "4 MiB");
    // Different views never share their words.
    assert_ne!(constant_words(0x1000, 256), constant_words(0x1000, 512));
    assert_ne!(constant_words(0x1000, 256), constant_words(0x1100, 256));
    assert_ne!(constant_words(0x1000, 1 << 14), constant_words(0x1000, 64));
}

/// The leak of run 65: a title that writes constant buffer views and
/// views of placed memory at ever new addresses adds nothing to the
/// table; one shape stands for all the placed views alike.
#[test]
fn views_at_new_addresses_add_no_entries() {
    if !inline() {
        return;
    }
    let null = std::ptr::null();
    let slots = [0u64; 4];
    let at = slots.as_ptr() as u64;
    // A resource description and a view no other test uses.
    let resource: [u32; 14] = std::array::from_fn(|k| 0x5ee0_0000 + k as u32);
    let mut desc: [u32; 16] = std::array::from_fn(|k| 0x7a70_0000 + k as u32);
    let mine = |v: &View| match v {
        View::Constant(address, _) => (0x6_0000_0000..0x6_1000_0000).contains(address),
        View::Placed(_, r, _) => *r == resource,
        _ => false,
    };
    let entries = || VIEWS.read().unwrap().list.iter().filter(|(v, _)| mine(v)).count();
    let shapes = || VIEWS.read().unwrap().shapes.iter().filter(|s| s.1 == resource).count();
    for n in 0..20_000u64 {
        let address = 0x6_0000_0000 + n * 0x100;
        create_cbv(null, &[address as u32, (address >> 32) as u32, 512, 0], at);
        assert_eq!(view(at), Some(View::Constant(address, 512)));
        let placed = 0xd_4940_0000 + n * 0x1_0000;
        (desc[10], desc[11]) = (placed as u32, (placed >> 32) as u32);
        create_placed_srv(null, &resource, &desc, at);
        assert_eq!(view(at), Some(View::Placed(false, resource, desc)));
    }
    assert_eq!(entries(), 0, "no table entries");
    assert_eq!(shapes(), 1, "one shape for 20,000 placed views");
    // The same shape, unordered: another shape.
    create_placed_uav(null, &resource, &desc, at);
    assert_eq!(view(at), Some(View::Placed(true, resource, desc)));
    assert_eq!(shapes(), 2);
    // Placed memory past 1 TiB (not title memory) is a table entry.
    (desc[10], desc[11]) = (0, 0x100);
    create_placed_srv(null, &resource, &desc, at);
    assert_eq!(view(at), Some(View::Placed(false, resource, desc)));
    assert_eq!(entries(), 1);
    // A record's words are the same for the same view (the host knows
    // a slot's view by them), and differ with the address.
    // SAFETY: both handles are slots of 32 bytes above.
    let words = |handle: u64| unsafe { *(handle as *const [u32; 3]) };
    let other = [0u64; 4];
    create_cbv(null, &[0x40, 6, 512, 0], at);
    create_cbv(null, &[0x40, 6, 512, 0], other.as_ptr() as u64);
    assert_eq!(words(at), words(other.as_ptr() as u64));
    create_cbv(null, &[0x140, 6, 512, 0], other.as_ptr() as u64);
    assert_ne!(words(at), words(other.as_ptr() as u64));
}

/// A released resource's views go stale; the same view of a new
/// resource at the same address is a new record.
#[test]
fn views_die_with_their_resource() {
    let null = std::ptr::null();
    let slots = [0u64; 2 * 4];
    let (a, b) = (slots.as_ptr() as u64, slots.as_ptr() as u64 + 32);
    let resource = 0x7777_0000usize as This;
    create_srv(null, resource, std::ptr::null(), a);
    assert_eq!(view(a), Some(View::Shader(0x7777_0000, None)));
    forget(0x7777_0000);
    assert_eq!(view(a), None, "stale after release");
    create_srv(null, resource, std::ptr::null(), b);
    assert_eq!(view(b), Some(View::Shader(0x7777_0000, None)));
    // SAFETY: both slots are in `slots`.
    let (old, new) = unsafe { (*(a as *const [u32; 3]), *(b as *const [u32; 3])) };
    assert_ne!(old, new, "a new generation");
}

/// The title places its shader-visible heap in its own memory
/// and checks that the heap starts there.
#[test]
fn placed_heaps_start_at_the_title_memory() {
    let mut memory = vec![0u8; 64 * 32];
    let address = memory.as_mut_ptr() as u64;
    let desc = HeapDesc {
        kind: 0,
        count: 64,
        flags: 1,
        nodes: 1,
    };
    let mut h = std::ptr::null();
    assert_eq!(
        create(
            desc,
            Placement {
                address,
                size: 64 * 32
            },
            &mut h
        ),
        S_OK
    );
    assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
    assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), address);
    // A size that does not match the count is not a placement.
    assert_eq!(create(desc, Placement { address, size: 7 }, &mut h), S_OK);
    assert_ne!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
}
