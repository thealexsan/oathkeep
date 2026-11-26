mod costs;

use super::with_title;

/// Title memory whose tail is reserved and not committed reads through
/// the copy as its bytes and then zeros, without a fault; so does
/// memory a page of which is taken away between two reads, and the
/// zeros are zeros whatever the read buffer held before.
#[test]
fn an_uncommitted_tail_reads_as_zeros() {
    use windows_sys::Win32::System::Memory::*;
    const HEAD: usize = 0xb_0000;
    const SIZE: usize = 0x20_0000;
    // SAFETY: a private reservation of our own; only committed pages
    // are written in place, and nothing else reads them in place.
    unsafe {
        // The read buffer holds something else first.
        let other = vec![0x77u8; SIZE];
        with_title(other.as_ptr(), SIZE, |bytes| assert!(bytes.iter().all(|&b| b == 0x77)));
        let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
        assert!(!base.is_null());
        assert_eq!(VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE), base.cast());
        base.write_bytes(0x5a, HEAD);
        with_title(base, SIZE, |bytes| {
            assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
            assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
        });
        // The title takes a page in the middle away: zeros from then
        // on, the rest as before.
        assert_ne!(VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT), 0);
        with_title(base, SIZE, |bytes| {
            assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
            assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
            assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
            assert!(bytes[HEAD..].iter().all(|&b| b == 0));
        });
        assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
    }
}

/// The texture both device-loss runs named last.
#[test]
fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
    use windows_sys::Win32::System::Memory::*;
    for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
        // The description as the log printed it, word for word.
        let words: [u32; 14] = [3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0];
        // SAFETY: fourteen words are a resource description (56 bytes,
        // plain numbers).
        let desc: crate::d3d12::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
        let layout = crate::d3d12::resource::texture_layout(&desc)
            .expect("a console layout")
            .planes[0]
            .layout
            .clone();
        let size = layout.size as usize;
        let (row, pitch) = (16 * 8, 256usize);
        assert!(size >= row * height as usize, "the layout holds every texel");
        // SAFETY: our own reservation; the committed part is written
        // and read in place, by this thread alone.
        unsafe {
            let pages = size.div_ceil(4096) + 1;
            let base = VirtualAlloc(std::ptr::null(), (pages + 1) * 4096, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE), base.cast());
            for i in 0..pages * 4096 {
                base.add(i).write((i * 7 + i / 251) as u8);
            }
            let memory = base.add(within);
            let in_place = std::slice::from_raw_parts(memory, size);
            with_title(memory, size, |copied| assert_eq!(copied, in_place));
            let staged = (height as usize - 1) * pitch + row;
            assert_eq!(staged, if height == 4 { 896 } else { 1920 });
            let detile = |tiled: &[u8]| {
                let mut linear = vec![0xeeu8; pitch * height as usize];
                layout
                    .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                    .expect("detiled");
                linear
            };
            let direct = detile(in_place);
            let through = with_title(memory, size, |copied| detile(copied));
            assert_eq!(direct, through);
            for y in 0..height as usize {
                assert!(direct[y * pitch..][..row].iter().any(|&b| b != 0xee), "row {y} written");
                assert!(direct[y * pitch + row..(y + 1) * pitch].iter().all(|&b| b == 0xee), "nothing past row {y}");
            }
            let mut exact = vec![0u8; staged];
            layout
                .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                .expect("the staging buffer's bytes are enough");
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }
}

#[test]
fn lru_eviction_orders_unmapped_then_cold_picked_and_protects_active_draws() {
    use super::lru::LruTracker;
    use crate::memory::residency::VramLevel;

    let mut lru = LruTracker::default();
    lru.note_upload(1, 0x1000_0000, 4 << 20, 1_000, false);
    lru.note_upload(2, 0x2000_0000, 4 << 20, 3_000, false);
    lru.note_upload(3, 0x3000_0000, 8 << 20, 500, true);
    lru.note_upload(4, 0x4000_0000, 2 << 20, 14_500, false);
    lru.note_upload(5, 0x5000_0000, 4 << 20, 100, false);
    lru.note_upload(6, 0x6000_0000, 4 << 20, 100, false);

    let now = 15_000;
    let victims = lru.select_victims(
        now,
        6 << 20,
        VramLevel::High,
        |res| res == 5,
        |res| res == 6,
        |start| start != 0x4000_0000,
    );
    assert_eq!(victims.iter().map(|v| v.1).collect::<Vec<_>>(), vec![4, 1]);
    assert!(victims[0].3);
    assert!(!victims[1].3);

    let crit = lru.select_victims(
        30_000,
        20 << 20,
        VramLevel::Critical,
        |res| res == 5,
        |res| res == 6,
        |start| start != 0x4000_0000,
    );
    assert_eq!(crit.iter().map(|v| v.1).collect::<Vec<_>>(), vec![4, 1, 2, 3]);
}

#[test]
fn fence_retire_queue_defers_drops_until_gpu_fence_and_lag_complete() {
    use super::lru::FenceRetireQueue;
    let mut q = FenceRetireQueue::new();
    q.retire(10, "tex_a");
    q.retire(12, "tex_b");
    q.retire(20, "tex_c");

    assert_eq!(q.drain_completed(15, 4), vec!["tex_a"]);
    assert_eq!(q.len(), 2);
    assert!(q.drain_completed(15, 4).is_empty());
    assert_eq!(q.drain_completed(16, 4), vec!["tex_b"]);
    assert_eq!(q.len(), 1);
    assert_eq!(q.drain_completed(24, 4), vec!["tex_c"]);
    assert_eq!(q.len(), 0);
}

#[test]
fn clamp_speculative_mips_preserves_mip_tail_under_vram_pressure() {
    use super::staging::clamp_speculative_mips;
    use crate::memory::residency::VramLevel;

    let all_9 = (1u32 << 9) - 1;
    assert_eq!(clamp_speculative_mips(all_9, 6, 9, false, VramLevel::Critical), all_9);
    assert_eq!(clamp_speculative_mips(all_9, 6, 9, true, VramLevel::Normal), all_9);
    assert_eq!(clamp_speculative_mips(all_9, 6, 9, true, VramLevel::High), all_9 & !1);
    assert_eq!(clamp_speculative_mips(all_9, 6, 9, true, VramLevel::Critical), 0b1_1110_0000);
    assert_eq!(clamp_speculative_mips(1, 6, 9, true, VramLevel::Critical), 1);
}

#[test]
fn fingerprint_detects_mid_mip_mutations_within_48k_budget() {
    use super::aliased::{fingerprint, fingerprint_windows};

    let windows = fingerprint_windows(8 << 20);
    let total: usize = windows.iter().map(|&(_, len)| len).sum();
    assert!(total <= 48 << 10);
    assert_eq!(windows.len(), 9);
    assert_eq!(windows.first().copied(), Some((0, 16 << 10)));
    assert_eq!(windows.last().copied(), Some(((8 << 20) - (4 << 10), 4 << 10)));

    let mut buf = vec![0x33u8; 256 << 10];
    let before = fingerprint(buf.as_ptr() as u64, buf.len() as u64);
    assert_ne!(before, 0);
    let probe_at = fingerprint_windows(buf.len())[2].0 + 17;
    buf[probe_at] ^= 0xff;
    let after = fingerprint(buf.as_ptr() as u64, buf.len() as u64);
    assert_ne!(before, after);
}
