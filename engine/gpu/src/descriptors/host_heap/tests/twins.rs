use super::*;

    #[test]
    fn a_raw_view_is_moved_only_where_its_descriptor_says_where_it_is() {
        let (Some(host), Some(at)) = (host(), staged(false, 1)) else {
            return; // no host GPU here
        };
        let (buffer, _) = host.shared_buffer(1 << 16).unwrap();
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 8,
                    NumElements: 100,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: this thread's slot, a live buffer.
        let start = unsafe {
            host.device.CreateShaderResourceView(&buffer, Some(&desc), at);
            buffer.GetGPUVirtualAddress() + 32
        };
        let words = || {
            let mut bytes = [0u8; 32];
            assert!(crate::backend::read_into(at.ptr as *const u8, &mut bytes));
            bytes
        };
        let before = words();
        assert!(!move_raw(at, start + 16, 400, 8), "another address");
        assert!(!move_raw(at, start, 404, 8), "another size");
        assert!(!move_raw(at, start, 400, 400), "nothing left");
        assert_eq!(words(), before);
        if move_raw(at, start, 400, 8) {
            let moved = words();
            assert_eq!(moved[..8], (start + 8).to_le_bytes());
            assert_eq!(moved[8..12], 392u32.to_le_bytes());
            assert_eq!(moved[12..], before[12..]);
        } else {
            assert_eq!(words(), before);
        }
    }

    /// Views that begin where no host view can (or where a structured view
    /// cannot promote to raw): raw and structured ones off 16 bytes, typed ones
    /// off their elements (or off 16 bytes when non-AMD raw twins require it).
    #[test]
    fn views_off_their_elements_are_told() {
        // the colour table bakes' parameter block: raw, 8 bytes into its window
        assert!(off_elements(8, 0, true, 4));
        assert!(!off_elements(0, 0, true, 4));
        assert!(!off_elements(48, 0, true, 4));
        // Structured views: only 16-byte-aligned starts promote cleanly to raw
        // (`raw_window`); stride-aligned non-16 offsets (456, 8, 12) must take
        // an exact aligned copy so NVIDIA raw reads never return zeros.
        assert!(off_elements(456, 456, false, 0));
        assert!(off_elements(8, 8, false, 0));
        assert!(off_elements(12, 12, false, 0));
        assert!(!off_elements(32, 456, false, 0));
        assert!(off_elements(100, 456, false, 0));
        // A typed view of 12-byte elements on 16 bytes and not on an element.
        assert!(off_elements(23_702_896, 0, false, 12));
        assert!(!off_elements(48, 0, false, 12));
        // 2-byte aligned offset=6 for 4-byte elements.
        assert!(off_elements(6, 0, false, 4));
        assert!(!off_elements(12, 0, false, 4));
        assert!(off_elements(12, 0, false, 8));
        // Non-AMD raw twin mode also aligns typed views that start off 16 bytes.
        assert!(off_elements_for(12, 0, false, 4, true));
        assert!(!off_elements_for(16, 0, false, 4, true));
    }

    #[test]
    fn unaligned_cbv_uses_aligned_slab_and_matches_buffer_copy() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let memory = vec![0x5au8; 2 << 16];
        let page = (memory.as_ptr() as u64).next_multiple_of(4096);
        let unaligned = page + 0x90;
        let view = View::Constant(unaligned, 320);
        let kept = make(&view, heap.cpu(0)).expect("unaligned CBV must not become null");
        let made_buf = kept.expect("CBV keeps backing buffer");
        let (copy, addr, len) = buffer_copy(&view).expect("buffer_copy must resolve CBV");
        assert_eq!((addr, len), (unaligned, 320));
        assert!(copy.can_bind_cbv(unaligned, 320));
        assert_eq!(copy.gpu(unaligned) % 256, 0);
        use windows::core::Interface;
        assert_eq!(made_buf.as_raw(), copy.buffer.as_raw());
        // Zero-length unaligned CBV returns None cleanly (null CBV).
        assert!(make(&View::Constant(unaligned, 0), heap.cpu(1)).is_none());
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    #[test]
    fn two_byte_aligned_and_structured_non16_views_use_exact_copies() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let memory = vec![0u64; 1 << 13];
        let start = (memory.as_ptr() as u64).next_multiple_of(4096);
        // SAFETY: plain CPU-side heap of one view.
        let cpu: ID3D12DescriptorHeap = unsafe {
            host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                ..Default::default()
            })
        }
        .unwrap();
        // SAFETY: live heap.
        let at = unsafe { cpu.GetCPUDescriptorHandleForHeapStart() };
        // Typed R32_UINT (format 42, 4-byte element) placed at offset +6 (2-byte aligned).
        let typed_at_6 = {
            let resource: [u32; 14] = [1, 0, 0, 0, 64, 0, 1, 1 | 1 << 16, 0, 1, 0, 1, 0, 0];
            let mut desc = [0u32; 16];
            desc[..10].copy_from_slice(&[42, 1, 5768, 0, 0, 0, 16, 0, 0, 0]);
            let addr = start + 6;
            (desc[10], desc[11]) = (addr as u32, (addr >> 32) as u32);
            View::Placed(false, resource, desc)
        };
        let made = make(&typed_at_6, at).expect("offset=6 element=4 view must succeed").expect("buffer");
        let (copy, addr, bytes) = buffer_copy(&typed_at_6).expect("buffer_copy");
        assert_eq!((copy.span(), addr, bytes), ((start + 6, 64), start + 6, 64));
        // SAFETY: live buffer.
        assert_eq!(unsafe { made.GetDesc() }.Width, 256);
        use windows::core::Interface;
        assert_eq!(made.as_raw(), copy.buffer.as_raw());

        // Structured 8-byte elements at offset +8 (stride-aligned, non-16-byte aligned).
        let struct_at_8 = {
            let resource: [u32; 14] = [1, 0, 0, 0, 64, 0, 1, 1 | 1 << 16, 0, 1, 0, 1, 0, 0];
            let mut desc = [0u32; 16];
            desc[..10].copy_from_slice(&[0, 1, 5768, 0, 0, 0, 8, 8, 0, 0]);
            let addr = start + 8;
            (desc[10], desc[11]) = (addr as u32, (addr >> 32) as u32);
            View::Placed(false, resource, desc)
        };
        let made_s = make(&struct_at_8, at).expect("structured offset=8 view must succeed").expect("buffer");
        let (copy_s, addr_s, bytes_s) = buffer_copy(&struct_at_8).expect("buffer_copy");
        assert_eq!((copy_s.span(), addr_s, bytes_s), ((start + 8, 64), start + 8, 64));
        assert_eq!(made_s.as_raw(), copy_s.buffer.as_raw());
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    #[test]
    fn make_listed_skips_raw_twins_when_heap_has_no_raw_half() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        if heap.raw != 0 {
            return; // adapter supports raw_twins; this test checks raw == 0 path
        }
        let memory = vec![0u64; 1 << 11];
        let addr = (memory.as_ptr() as u64).next_multiple_of(4096) + 4;
        let resource: [u32; 14] = [1, 0, 0, 0, 64, 0, 1, 1 | 1 << 16, 0, 1, 0, 1, 0, 0];
        let mut desc = [0u32; 16];
        // R32_UINT at +4 (element-aligned, non-16-byte aligned).
        desc[..10].copy_from_slice(&[42, 1, 5768, 0, 0, 0, 16, 0, 0, 0]);
        (desc[10], desc[11]) = (addr as u32, (addr >> 32) as u32);
        let view = View::Placed(false, resource, desc);
        let mut slot_mem = [0u32; 8];
        crate::descriptors::record(slot_mem.as_mut_ptr() as u64, view);
        let rec = [slot_mem[0], slot_mem[1], slot_mem[2]];
        let dyn_slot = heap.dynamic(1).unwrap();
        let before = TWINS[2].load(std::sync::atomic::Ordering::Relaxed);
        assert!(heap.make_listed(0x9999_8888, rec, dyn_slot).is_some());
        let after = TWINS[2].load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(before, after, "no null raw twin work when heap.raw == 0");
    }

    #[test]
    fn capped_view_heap_dynamic_budget_and_telemetry() {
        // NVIDIA 1,000,000 cap (null = 999,999) with 1,245,184 title slots:
        // keeps CAPPED_DYNAMIC (262,144) so dynamic table refusals stay at 0.
        assert_eq!(dynamic_slots_for(1_245_184, 999_999, None), CAPPED_DYNAMIC);
        assert_eq!(999_999u32.saturating_sub(CAPPED_DYNAMIC), 737_855);
        // A title heap that fits alongside DYNAMIC on a 1,000,000 cap keeps DYNAMIC.
        assert_eq!(dynamic_slots_for(500_000, 999_999, None), DYNAMIC);
        // Explicit OATHKEEP_DYNAMIC_SLOTS override works on capped heaps and clamps safely.
        assert_eq!(dynamic_slots_for(1_245_184, 999_999, Some(1 << 17)), 1 << 17);
        assert_eq!(dynamic_slots_for(1_245_184, 999_999, Some(1024)), 1 << 16);
        assert_eq!(dynamic_slots_for(1_245_184, 999_999, Some(900_000)), 999_999 / 2);
        // AMD uncapped (null = 2^21 - 1) leaves room for up to 4x DYNAMIC.
        assert_eq!(
            dynamic_slots_for(1_245_184, (1 << 21) - 1, None),
            ((1 << 21) - 1) - 1_245_184
        );
        // Unclamped (null = 0).
        assert_eq!(dynamic_slots_for(1024, 0, None), DYNAMIC);
        assert_eq!(dynamic_slots_for(1024, 0, Some(1 << 19)), 1 << 19);

        // Verify past-cap mirror telemetry records below-null vs past-null drops safely,
        // and `capped_raw_pages` alone surfaces `capped_telemetry` when `title_count > count`.
        let Some(mut heap) = HostHeap::new(false, 4) else {
            return;
        };
        let mut fake = [0u32; 8];
        let handle = fake.as_mut_ptr() as u64;
        assert!(heap.mirror(heap.count, handle, false).is_none());
        assert!(heap.mirror(heap.count + heap.dynamic + 5, handle, true).is_none());
        use std::sync::atomic::Ordering::Relaxed;
        assert_eq!(heap.capped_mirror[0].load(Relaxed), 1);
        assert_eq!(heap.capped_mirror[1].load(Relaxed), 1);
        assert_eq!(heap.capped_mirror[2].load(Relaxed), 1);
        assert_eq!(heap.highest_capped.load(Relaxed), heap.count + heap.dynamic + 5);
        heap.title_count = heap.count + 16;
        let first_snap = heap.capped_telemetry().expect("mirror counts trigger telemetry");
        assert_eq!(first_snap.6, [1, 1, 1]);
        assert!(heap.capped_telemetry().is_none(), "empty telemetry suppressed");
        heap.note_capped_raw_page();
        let page_snap = heap.capped_telemetry().expect("raw page past cap triggers telemetry");
        assert_eq!(page_snap.6, [0, 0, 0]);
        assert_eq!(page_snap.7, 1);
    }

    /// Such a view is made on a copy of its own bytes, and that copy is the one
    /// brought up to date for it; another's is its buffer's.
    #[test]
    fn views_off_their_elements_get_a_copy_of_their_bytes() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let memory = vec![0u64; 1 << 13];
        let start = (memory.as_ptr() as u64).next_multiple_of(4096);
        // SAFETY: a plain CPU-side heap of one view.
        let cpu: ID3D12DescriptorHeap = unsafe {
            host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                ..Default::default()
            })
        }
        .unwrap();
        // SAFETY: a live heap.
        let at = unsafe { cpu.GetCPUDescriptorHandleForHeapStart() };
        // Memory placed at `address`, seen as the title passes it: raw, 32
        // dwords, or four 456-byte elements.
        let view = |address: u64, raw: bool| {
            let resource: [u32; 14] = [1, 0, 0, 0, 4 * 456, 0, 1, 1 | 1 << 16, 0, 1, 0, 1, 0, 0];
            let mut desc = [0u32; 16];
            let words = if raw {
                [39, 1, 5768, 0, 0, 0, 32, 0, 1, 0]
            } else {
                [0, 1, 5768, 0, 0, 0, 4, 456, 0, 0]
            };
            desc[..10].copy_from_slice(&words);
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(false, resource, desc)
        };
        let width = |v: &View| {
            let made = make(v, at).expect("a view").expect("a buffer");
            // SAFETY: a live buffer.
            unsafe { made.GetDesc() }.Width
        };
        for (off, raw, len) in [(8, true, 128u64), (8, false, 4 * 456)] {
            let v = view(start + off, raw);
            assert_eq!(width(&v), len.next_multiple_of(256));
            let (copy, address, bytes) = buffer_copy(&v).unwrap();
            assert_eq!((copy.span(), address, bytes), ((start + off, len), start + off, len));
        }
        for (off, raw) in [(16, true), (32, false)] {
            let v = view(start + off, raw);
            let (copy, ..) = buffer_copy(&v).unwrap();
            assert_ne!(copy.span().0, start + off, "its buffer's copy");
            // SAFETY: a live buffer.
            assert_eq!(width(&v), unsafe { copy.buffer.GetDesc() }.Width);
        }
    }
