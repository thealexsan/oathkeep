use super::*;


    /// A view of a host copy that begins at a later level shows the same
    /// levels of it.
    #[test]
    fn views_count_from_the_first_level_of_the_host_copy() {
        let view = |first, levels, clamp| {
            let mut desc = srv_desc(&[98, 4, 5768, 0, first, levels, 0, 0, 0, 0]);
            desc.Anonymous.Texture2D.ResourceMinLODClamp = clamp;
            from_base(&mut desc, 3);
            // SAFETY: the member its dimension says.
            let t = unsafe { desc.Anonymous.Texture2D };
            (t.MostDetailedMip, t.MipLevels, t.ResourceMinLODClamp)
        };
        assert_eq!(view(3, 4, 0.0), (0, 4, 0.0));
        assert_eq!(view(5, u32::MAX, 5.5), (2, u32::MAX, 2.5));
        // (Two levels the copy has not.)
        assert_eq!(view(1, 4, 0.0), (0, 2, 0.0));
    }

    /// 2D textures and arrays have a neutral stand-in, others a null view.
    #[test]
    fn a_texture_not_there_yet_has_a_neutral_stand_in() {
        let Some(host) = host() else { return };
        // SAFETY: plain heap creation and queries.
        unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .unwrap();
            let at = heap.GetCPUDescriptorHandleForHeapStart();
            let view = |dimension| View::Shader(0, Some([98, dimension, 5768, 0, 0, 1, 0, 1, 0, 0]));
            assert!(neutral_for(&view(4), at).is_some());
            assert!(neutral_for(&view(5), at).is_some());
            assert!(neutral_for(&view(9), at).is_none());
            assert!(neutral_for(&view(8), at).is_none());
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }

    /// Memory past the levels a slot's view shows is not its texture's.
    #[test]
    fn a_view_of_smaller_levels_is_not_told_of_memory_past_them() {
        let mut spans = Spans::default();
        spans.note(1, 0x10000, 0x50000, true);
        spans.shows(1, 3, 0x12000);
        spans.note(2, 0x10000, 0x50000, true);
        spans.shows(2, 0, 0x50000);
        assert_eq!(spans.slots_holding(0x11000, 0x11800), vec![1, 2]);
        assert_eq!(spans.slots_holding(0x20000, 0x21000), vec![2]);
        assert_eq!(spans.slots_in(0x20000, 0x21000), vec![2]);
        spans.forget(2);
        assert!(spans.slots_in(0x20000, 0x21000).is_empty());
        assert_eq!(spans.slots_in(0x0, 0x10001), vec![1]);
    }

    /// A texture filled with less than was mapped is looked at again, but
    /// not for ever: memory that is shown and cannot be read ends it, and a
    /// filling that got everything starts the count anew.
    #[test]
    fn a_short_filling_is_looked_at_again_a_few_times() {
        // An address no other test names.
        let start = 0x7fee_1234_0000u64;
        let pending = || SHORT.lock().unwrap().0.remove(&start);
        for _ in 0..SHORT_IN_A_ROW {
            super::read_short(start);
            assert!(pending());
        }
        super::read_short(start);
        assert!(!pending());
        super::read_all(start);
        super::read_short(start);
        assert!(pending());
        super::read_all(start);
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// What the texture code asks of the spans (levels on demand): where
    /// the next texture starts inside a span, the lowest level a view of a
    /// texture shows, and a texture's slots.
    #[test]
    fn spans_say_the_next_start_and_the_lowest_level_viewed() {
        let mut spans = Spans::default();
        // A 512x512 texture of 0x56000 bytes viewed from levels 3 and 1,
        // and the next one 0x6000 after it (run 188).
        spans.note(1, 0x1_0000, 0x1_0000 + 0x5_6000, true);
        spans.shows(1, 3, u64::MAX);
        spans.note(2, 0x1_0000, 0x1_0000 + 0x5_6000, true);
        spans.shows(2, 1, u64::MAX);
        spans.note(3, 0x1_6000, 0x1_6000 + 0x5_6000, true);
        assert_eq!(spans.next_start(0x1_0000, 0x1_0000 + 0x5_6000), Some(0x1_6000));
        assert_eq!(spans.next_start(0x1_6000, 0x1_6000 + 0x5_6000), None);
        assert_eq!(spans.first_viewed(0x1_0000), Some(1));
        assert_eq!(spans.first_viewed(0x1_6000), None);
        assert_eq!(spans.slots_at(0x1_0000), vec![1, 2]);
        // The view from level 1 goes: the texture is viewed from level 3.
        spans.forget(2);
        assert_eq!(spans.first_viewed(0x1_0000), Some(3));
    }

    /// Views of one placed texture are told from views of others by the
    /// address and the resource description, not by what they show.
    #[test]
    fn views_of_one_placed_texture_are_told_by_address_and_description() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], first_mip: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[4], desc[5], desc[10], desc[11]) = (98, 4, first_mip, 8 - first_mip, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let low = view(texture, 3, 0x1000_0000);
        // more mips of the same texture
        assert!(same_placed_texture(&low, &view(texture, 0, 0x1000_0000)));
        // another address, another description, a buffer, an unordered view
        assert!(!same_placed_texture(&low, &view(texture, 3, 0x2000_0000)));
        let mut wider = texture;
        wider[4] = 1024;
        assert!(!same_placed_texture(&low, &view(wider, 3, 0x1000_0000)));
        let mut buffer = texture;
        buffer[0] = 1;
        assert!(!same_placed_texture(&view(buffer, 3, 0x1000_0000), &view(buffer, 3, 0x1000_0000)));
        let View::Placed(_, resource, desc) = low.clone() else { unreachable!() };
        assert!(!same_placed_texture(&low, &View::Placed(true, resource, desc)));
        assert!(!same_placed_texture(&low, &View::Shader(0, None)));
    }

    #[test]
    fn ranges_that_touch_are_one() {
        let mut ranges = vec![(40, 50), (30, 40), (35, 40), (10, 20), (45, 60), (60, 61), (1, 1)];
        join(&mut ranges);
        assert_eq!(ranges, [(1, 1), (10, 20), (30, 61)]);
        join(&mut ranges);
        assert_eq!(ranges.len(), 3);
    }

    /// Typed buffer views of integer formats are marked in the table of
    /// strides; float, normalized, raw and structured ones are not.
    #[test]
    fn integer_typed_buffer_views_are_marked_in_the_table_of_strides() {
        use oathkeep_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
        // Format, dimension (buffer), first element, count, stride, counter, flags.
        let uav = |format, stride, flags| View::Unordered(0, 0, Some([format, 1, 0, 0, 64, stride, 0, 0, flags, 0]));
        assert_eq!(view_kind(&uav(12, 0, 0)), VIEW_INTEGER, "R16G16B16A16_UINT");
        assert_eq!(view_kind(&uav(59, 0, 0)), VIEW_INTEGER | VIEW_SIGNED, "R16_SINT");
        assert_eq!(view_kind(&uav(10, 0, 0)), 0, "R16G16B16A16_FLOAT");
        assert_eq!(view_kind(&uav(58, 0, 0)), 0, "R16_SNORM");
        assert_eq!(view_kind(&uav(39, 0, 1)), 0, "raw");
        assert_eq!(view_kind(&uav(0, 16, 0)), 0, "structured");
        assert_eq!(view_stride(&uav(12, 0, 0)), Some(8));
    }

    /// What may stand for a texture that is not there yet: one of its kind in
    /// another place or size; another one only if asked; nothing a shader
    /// cannot read in its place.
    #[test]
    fn a_slot_shows_on_what_can_stand_for_its_new_texture() {
        let texture = [3, 0, 0, 0, 512, 0, 256, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], format: u32, dimension: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[10], desc[11]) = (format, dimension, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let old = view(texture, 98, 4, 0x1000_0000);
        let mut twice = texture;
        (twice[4], twice[6], twice[7]) = (1024, 512, 0x9_0001);
        let mut square = texture;
        square[6] = 512;
        let mut buffer = texture;
        buffer[0] = 1;
        assert_eq!(kin(&old, &view(texture, 98, 4, 0x1000_0000)), Kin::Same);
        assert_eq!(kin(&old, &view(texture, 98, 4, 0x2000_0000)), Kin::Moved);
        assert_eq!(kin(&old, &view(twice, 98, 4, 0x2000_0000)), Kin::Sized);
        assert_eq!(kin(&old, &view(square, 98, 4, 0x2000_0000)), Kin::Other);
        assert_eq!(kin(&old, &view(twice, 83, 4, 0x2000_0000)), Kin::Other);
        assert_eq!(kin(&old, &view(texture, 98, 9, 0x2000_0000)), Kin::None);
        assert_eq!(kin(&old, &view(buffer, 98, 1, 0x2000_0000)), Kin::None);
        for (kin, kept) in [(Kin::Same, [true; 3]), (Kin::Moved, [false, true, true]), (Kin::Sized, [false, true, true]), (Kin::Other, [false, false, true]), (Kin::None, [false; 3])] {
            assert_eq!([0, 1, 2].map(|keep| kin.kept(keep)), kept, "{kin:?}");
        }
    }

    /// A constant buffer view larger than the host takes is cut to what it
    /// takes: the device, which such a view removes, stays.
    #[test]
    fn an_oversized_constant_buffer_view_leaves_the_device_alive() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        // SAFETY: a plain device query.
        let alive = || unsafe { host.device.GetDeviceRemovedReason() }.is_ok();
        // Made first: an allocation that fails on a removed device must not
        // end the test early.
        let (buffer, _) = host.upload_buffer(1 << 20).expect("an upload buffer");
        let memory = vec![0u8; 2 << 20];
        let at = (memory.as_ptr() as u64).next_multiple_of(64 << 10);
        for size in [969_472u32, 896_256] {
            assert!(make(&View::Constant(at, size), heap.cpu(0)).is_some());
            assert!(alive(), "a view of {size} bytes");
        }
        // SAFETY: a live upload buffer.
        heap.constant_at(1, &buffer, unsafe { buffer.GetGPUVirtualAddress() }, 969_472);
        assert!(alive(), "a view of the list's own memory");
        assert_eq!(constant_bytes(480), 512);
        assert_eq!(constant_bytes(64 << 10), 64 << 10);
        assert_eq!(constant_bytes(u32::MAX), 64 << 10);
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn a_structured_view_made_raw_passes_the_host_check() {
        // 100 elements of 20 bytes from element 8, in a copy that starts 32
        // bytes before the view's memory: bytes 192..2192.
        let (first, count) = raw_window(32, 8, 100, 20).unwrap();
        assert_eq!((first, count), (48, 500));
        assert!(buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 4096));
        // the format a raw view must name; the structured one's is refused
        assert!(!buffer_view_ok(DXGI_FORMAT_UNKNOWN, first, count, 0, true, 4096));
        // past the buffer, not on 16 bytes, no elements
        assert!(!buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 2000));
        assert!(raw_window(4, 0, 100, 20).is_none());
        assert!(!buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, 0, 0, 0, true, 4096));
        // a structured view the host takes as it is
        assert!(buffer_view_ok(DXGI_FORMAT_UNKNOWN, 1, 10, 12, false, 4096));
    }

    /// The raw twin of a view shows the same bytes: typed and structured ones
    /// that begin on 16 bytes, whole dwords, none past the buffer.
    #[test]
    fn raw_twins_show_the_views_bytes() {
        let (u16s, u32s, none) = (DXGI_FORMAT(57), DXGI_FORMAT(42), DXGI_FORMAT_UNKNOWN);
        assert_eq!(twin_window(u32s, 4, 10, 0, false, 4096), Some((4, 10, 0)));
        assert_eq!(twin_window(u16s, 8, 7, 0, false, 4096), Some((4, 4, 0)));
        // the last dword would reach past the buffer
        assert_eq!(twin_window(u16s, 8, 7, 0, false, 30), Some((4, 3, 0)));
        assert_eq!(twin_window(none, 2, 3, 24, false, 4096), Some((12, 18, 0)));
        assert_eq!(twin_window(DXGI_FORMAT_R32_TYPELESS, 8, 5, 0, true, 4096), Some((8, 5, 0)));
        // where no raw view begins: from the 16 bytes below
        assert_eq!(twin_window(none, 1, 3, 24, false, 4096), Some((4, 20, 8)));
        assert_eq!(twin_window(u32s, 1, 10, 0, false, 4096), Some((0, 11, 4)));
        assert_eq!(twin_window(u32s, 1, 10, 0, false, 4), None);
    }

    /// What a slot is made as is made beside the heap and copied in whole: the
    /// slot's descriptor is the one the view has, and each thread makes its
    /// own.
    #[test]
    fn views_are_made_beside_the_heap_and_copied_in() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let (Some(a), Some(b)) = (staged(false, 0), staged(false, 1)) else {
            return;
        };
        assert_ne!(a.ptr, b.ptr);
        assert_ne!(Some(a.ptr), std::thread::spawn(|| staged(false, 0).map(|h| h.ptr)).join().unwrap());
        let (buffer, _) = host.shared_buffer(1 << 16).unwrap();
        let index = heap.dynamic(1).unwrap();
        let bytes = |at: D3D12_CPU_DESCRIPTOR_HANDLE| {
            let mut bytes = [0u8; 32];
            crate::host::read_into(at.ptr as *const u8, &mut bytes).then_some(bytes)
        };
        heap.raw_at(index, &buffer, 64, 1024);
        let (slot, beside) = (bytes(heap.cpu(index)), bytes(staged(false, 0).unwrap()));
        assert_eq!(slot, beside, "copied as it was made");
        heap.null_raw_at(index);
        assert_ne!(bytes(heap.cpu(index)), slot, "another view since");
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
