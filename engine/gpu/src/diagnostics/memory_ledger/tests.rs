use std::sync::atomic::Ordering;

use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use super::check::{Names, decode};
use super::watch::texture_address;
use super::*;

/// The two descriptors of the card that were read (a texture of 64x64 and
/// one of 2048x2048, RX 7900 XTX) and a raw buffer's.
#[test]
fn descriptors_are_read_as_the_card_has_them() {
    let mut d = [0u8; 32];
    d[..16].copy_from_slice(&[0, 0xb4, 0, 2, 0, 0, 0xa0, 0xc2, 0x0f, 0xc0, 0x0f, 0, 0xac, 0x0f, 0x60, 0x91]);
    assert_eq!(decode(&d), Names::Texture { base: 0x2_00b4_0000, meta: None, kind: 9, width: 64, height: 64 });
    d[..16].copy_from_slice(&[0, 0xb4, 2, 2, 0, 0, 0xa0, 0xc2, 0xff, 0xc1, 0xff, 1, 0xac, 0x0f, 0xb0, 0x91]);
    assert_eq!(decode(&d), Names::Texture { base: 0x2_02b4_0000, meta: None, kind: 9, width: 2048, height: 2048 });
    d = [0; 32];
    assert_eq!(decode(&d), Names::Nothing);
    d[..8].copy_from_slice(&0x1_2345_6780u64.to_le_bytes());
    d[8..12].copy_from_slice(&400u32.to_le_bytes());
    assert_eq!(decode(&d), Names::Buffer { base: 0x1_2345_6780, stride: 0, bytes: 400 });
    d[6] = 24; // a stride
    assert_eq!(decode(&d), Names::Buffer { base: 0x1_2345_6780, stride: 24, bytes: 9600 });
}

/// Who has an address, and who had it, newest first.
/// A texture of the pool and its block stand in the book while they
/// live, and not after.
#[test]
fn a_pooled_texture_and_its_block_are_in_the_book() {
    let Some(host) = crate::backend::host() else { return };
    FORCED.store(true, Ordering::Relaxed);
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 128,
        Height: 256,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT(83),
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    // SAFETY: a valid description.
    let texture = unsafe { crate::memory::pool::texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) };
    if let Some(texture) = texture {
        let at = texture_address(&texture, &desc);
        println!("address {at:x?}");
        if let Some(at) = at {
            let names = || with(|b| b.holders(at).iter().map(|e| e.what.clone()).collect::<Vec<_>>());
            let found = names();
            println!("held by {found:?}");
            assert!(found.iter().any(|n| n.contains("a texture in a block")), "{found:?}");
            assert!(found.iter().any(|n| n.contains("a block of textures")), "{found:?}");
            drop(texture);
            assert!(!names().iter().any(|n| n.contains("a texture in a block")));
        }
    }
    FORCED.store(false, Ordering::Relaxed);
}

#[test]
fn the_book_knows_what_is_gone() {
    let mut b = Book::default();
    let block = b.add(1, 0x1000_0000, 64 << 20, "a block".into(), 5);
    let one = b.add(2, 0x1000_0000, 0x8000, "one".into(), 6);
    let two = b.add(3, 0x1000_8000, 0x8000, "two".into(), 7);
    let names = |b: &Book, at| b.holders(at).iter().map(|e| e.what.clone()).collect::<Vec<_>>();
    assert_eq!(names(&b, 0x1000_0100), ["one", "a block"]);
    assert_eq!(names(&b, 0x1000_8100), ["two", "a block"]);
    assert!(b.alive(0x1000_8100) && !b.alive(0x1000_0000 + (64 << 20)) && !b.alive(0xfff_ffff));
    assert!(names(&b, 0x1000_0000 + (64 << 20)).is_empty());
    b.remove(one, 2, 10);
    b.remove(block, 1, 12);
    assert!(names(&b, 0x1000_0100).is_empty());
    let gone: Vec<(String, u64)> = b.gone_at(0x1000_0100, 9).iter().map(|(e, at)| (e.what.clone(), *at)).collect();
    assert_eq!(gone, [("a block".to_owned(), 12), ("one".to_owned(), 10)]);
    assert_eq!(b.neighbours(0x1000_0100)[1].map(|e| e.what.as_str()), Some("two"));
    b.remove(two, 3, 13);
    assert!(b.neighbours(u64::MAX - 1)[0].is_none() && b.by_object.is_empty());
}
