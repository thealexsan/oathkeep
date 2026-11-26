mod command_tests {
    /// Commands are summed by pipeline and first word; those without a
    /// pipeline by their first two words.
    #[test]
    fn commands_are_named_by_their_pipeline() {
        use super::super::timing::command_key;
        assert_eq!(
            command_key("dispatch 182x1x1 [compute 24f989b98dcb03f0] | 24f989b98dcb03f0: u4=0xa6b080 [heap 341380: x]"),
            "dispatch [compute 24f989b98dcb03f0]"
        );
        assert_eq!(
            command_key("indirect kind 10 max 1 at 0xd9358fce0 [mesh 0750cb2d975c7563, pixel 085d374c5a795549] | t"),
            "indirect [mesh 0750cb2d975c7563, pixel 085d374c5a795549]"
        );
        assert_eq!(command_key("texture upload 0x271b61c28b0 (2048x2048x1 f99 mips 10) sub 0"), "texture upload");
        assert_eq!(command_key("alias copy of 0x26f66aca780 into 0x26fbdc93890"), "alias copy");
    }
}

mod own_tests {
    use super::super::alloc::{BIG_AT, BIG_BYTES, big_blocks, block_class, owned};
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn blocks_count_in_their_size_class() {
        assert_eq!(block_class(0), 0);
        assert_eq!(block_class(4095), 0);
        assert_eq!(block_class(4096), 1);
        assert_eq!(block_class(65535), 1);
        assert_eq!(block_class(65536), 2);
        assert_eq!(block_class((1 << 20) - 1), 2);
        assert_eq!(block_class(1 << 20), 3);
    }

    /// A block of 32 MiB or more is listed by its size while it lives.
    #[test]
    fn big_blocks_are_listed_while_they_live() {
        let size = (48usize << 20) + 4096;
        let listed = |at: usize| {
            BIG_AT
                .iter()
                .zip(&BIG_BYTES)
                .any(|(place, bytes)| place.load(Relaxed) == at && bytes.load(Relaxed) == size as u64)
        };
        let block = vec![0u8; size];
        let at = block.as_ptr() as usize;
        assert!(listed(at));
        assert!(big_blocks().contains("48.00"));
        drop(block);
        assert!(!listed(at));
    }

    /// A big block shows in the counters while it lives (other tests
    /// allocate small blocks meanwhile: only the big class is looked at,
    /// and only for at least this block).
    #[test]
    fn a_block_is_counted_while_it_lives() {
        let block = vec![1u8; 8 << 20];
        assert!(owned(3) >= 8 << 20);
        assert!(owned(7) >= 1);
        drop(block);
        assert!(owned(7) < u64::MAX / 2, "no count below zero");
    }
}
