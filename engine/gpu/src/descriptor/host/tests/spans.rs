use super::*;

    #[test]
    fn a_set_of_slots_holds_what_was_put_in() {
        let set = Slots::new(200);
        for slot in [199, 0, 64, 63, 130, 64] {
            set.put(slot);
        }
        assert_eq!(set.all(|_| true), [0, 63, 64, 130, 199]);
        assert_eq!(set.all(|slot| slot % 2 == 0), [0, 64, 130]);
        assert!(set.remove(64) && !set.remove(64) && !set.remove(5));
        assert_eq!(set.take(), [0, 63, 130, 199]);
        assert!(set.take().is_empty());
    }

    #[test]
    fn slots_all_mod_matches_modulo_filter_and_contains() {
        let set = Slots::new(300);
        for slot in [0, 1, 15, 16, 17, 31, 32, 63, 64, 80, 144, 255, 256, 299] {
            set.put(slot);
            assert!(set.contains(slot));
        }
        assert!(!set.contains(2));
        assert!(!set.contains(500));
        for parts in [1u32, 3, 16, 64] {
            for part in 0..parts {
                assert_eq!(
                    set.all_mod(part, parts),
                    set.all(|slot| slot % parts == part),
                    "mismatch for part={part}, parts={parts}"
                );
            }
        }
    }

    /// A slot a draw read through a table leaves the queue; the title's
    /// next write of it waits for its draw, and is due after the grace
    /// frames if none reads it.
    #[test]
    fn slots_read_through_tables_leave_the_texture_queue() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let frame = 100;
        let slots = |at: u64| -> Vec<u32> { heap.texture_slots_due(at, 8).0.into_iter().map(|d| d.0).collect() };
        let texture = View::Placed(false, [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 0x105, 0, 0], [0; 16]);
        assert!(is_texture_view(&texture));
        heap.queue_texture(2, true, false, frame);
        heap.queue_texture(3, true, false, frame);
        assert_eq!(slots(frame), [3, 2]);
        heap.tabled_in(3, true, frame);
        assert_eq!(slots(frame), [2]);
        // Made in place by the draw that read it: still out.
        heap.queue_texture(3, true, true, frame);
        assert_eq!(slots(frame), [2]);
        // Written again in the next frame: due once the grace frames have
        // passed without a draw that reads it.
        heap.queue_texture(3, true, false, frame + 1);
        assert_eq!(slots(frame + TABLED_GRACE), [2]);
        assert_eq!(slots(frame + 1 + TABLED_GRACE), [3, 2]);
        // The draw comes after all: out again.
        heap.tabled_in(3, true, frame + 1);
        assert_eq!(slots(u64::MAX), [2]);
        // A slot of a table no draw read stays queued; its next write waits.
        heap.tabled_in(2, false, frame + 1);
        assert_eq!(slots(frame + 1), [2]);
        heap.queue_texture(2, true, false, frame + 2);
        assert_eq!(slots(frame + 2), []);
        // Long after the last table, a write is due at once.
        heap.queue_texture(2, true, false, frame + 3 + TABLED_FRAMES);
        assert_eq!(slots(frame + 3 + TABLED_FRAMES), [2]);
    }

    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    #[test]
    fn dynamic_search_can_find_a_run_across_the_cursor() {
        let Some(heap) = HostHeap::new(false, 4) else {
            eprintln!("no host descriptor heap; skipped");
            return;
        };
        let first = heap.dynamic(1).unwrap();
        let left = heap.dynamic(2).unwrap();
        let right = heap.dynamic(2).unwrap();
        let rest = heap.dynamic(heap.dynamic - 5).unwrap();
        assert_eq!((first, left, right, rest), (4, 5, 7, 9));

        // Reallocating the left half puts the cursor back in the middle.
        heap.release(left, 2);
        assert_eq!(heap.dynamic(2), Some(left));
        heap.release(left, 2);
        heap.release(right, 2);
        assert_eq!(heap.dynamic(4), Some(left));
        assert_eq!(heap.dynamic(1), None);

        // A free slot at each physical end does not form a two-slot range.
        heap.release(first, 1);
        let last = first + heap.dynamic - 1;
        heap.release(last, 1);
        assert_eq!(heap.dynamic(2), None);
    }

    #[test]
    fn dynamic_slots_populate_and_clear_stride_table() {
        use oathkeep_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let Some((_, _, data)) = heap.strides else {
            panic!("view heap should allocate strides table");
        };
        let read_stride = |slot: u32| -> u32 {
            // SAFETY: inside `SLOT_STRIDES_COUNT` mapping of `heap.strides`.
            unsafe { *(data as *const u32).add(slot as usize) }
        };
        let memory = vec![0u64; 1 << 11];
        let addr = (memory.as_ptr() as u64).next_multiple_of(4096);
        let placed_typed = |uav: bool, dxgi_fmt: u32, elems: u32| {
            let resource: [u32; 14] = [1, 0, 0, 0, 4096, 0, 1, 1 | 1 << 16, 0, 1, 0, 1, 0, 0];
            let mut desc = [0u32; 16];
            // [format, dim=1 (buffer), mapping/pad, ..., num_elements @6, stride @7, flags @8]
            let words = if uav {
                [dxgi_fmt, 1, 0, 0, elems, 0, 0, 0, 0, 0]
            } else {
                [dxgi_fmt, 1, 5768, 0, 0, 0, elems, 0, 0, 0]
            };
            desc[..10].copy_from_slice(&words);
            (desc[10], desc[11]) = (addr as u32, (addr >> 32) as u32);
            View::Placed(uav, resource, desc)
        };

        let s0 = heap.dynamic(4).unwrap();
        // R16G16B16A16_UINT (12): 8 bytes + VIEW_INTEGER
        let u16x4 = placed_typed(true, 12, 64);
        heap.make_at(s0, &u16x4);
        assert_eq!(read_stride(s0), 8 | VIEW_INTEGER);

        // R16_SINT (59): 2 bytes + VIEW_INTEGER | VIEW_SIGNED
        let s16 = placed_typed(false, 59, 64);
        heap.make_at(s0 + 1, &s16);
        assert_eq!(read_stride(s0 + 1), 2 | VIEW_INTEGER | VIEW_SIGNED);

        // R16G16B16A16_FLOAT (10): 8 bytes, no integer bits
        let f16x4 = placed_typed(true, 10, 64);
        heap.make_at(s0 + 2, &f16x4);
        assert_eq!(read_stride(s0 + 2), 8);

        // Clearing / nulling / raw views overwrite stale bits cleanly.
        heap.make_null(s0);
        assert_eq!(read_stride(s0), 0);
        heap.null_raw_at(s0 + 1);
        assert_eq!(read_stride(s0 + 1), 0);
        heap.null_raw_uav_at(s0 + 2);
        assert_eq!(read_stride(s0 + 2), 0);

        // `make_listed` (both cold creation and cached copy hit) must populate strides.
        let mut slot_mem = [0u32; 8];
        crate::descriptor::record(slot_mem.as_mut_ptr() as u64, u16x4);
        let rec = [slot_mem[0], slot_mem[1], slot_mem[2]];
        assert!(heap.make_listed(0x1234, rec, s0).is_some());
        assert_eq!(read_stride(s0), 8 | VIEW_INTEGER);
        heap.make_null(s0 + 3);
        assert_eq!(read_stride(s0 + 3), 0);
        assert!(heap.make_listed(0x1234, rec, s0 + 3).is_some());
        assert_eq!(read_stride(s0 + 3), 8 | VIEW_INTEGER);
    }
