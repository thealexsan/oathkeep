use super::*;

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;

fn desc(width: u64, height: u32, mips: u16, format: u32, layout: u32) -> ResourceDesc {
    ResourceDesc {
        dimension: 3,
        alignment: 0,
        width,
        height,
        depth_or_array: 1,
        mips,
        format,
        samples: 1,
        quality: 0,
        layout,
        flags: 0,
    }
}

/// Method `index` of a computer, as the title calls it.
fn slot<T: Copy>(c: This, index: usize) -> T {
    // SAFETY: the computer is live; T is the slot's function type.
    unsafe { std::mem::transmute_copy(&(*c).vtable.0[index]) }
}

fn create(d: &ResourceDesc) -> This {
    let mut c = std::ptr::null_mut();
    assert_eq!(XGCreateTextureComputer(d, &mut c), S_OK);
    c
}

/// Element offsets: linear rows of 1 KiB (128 elements), 0 outside.
#[test]
fn element_offsets_follow_the_layout() {
    type Offset = extern "system" fn(This, u32, u32, u64, u32, u32, u32, *mut u32) -> u64;
    let c = create(&desc(128, 4, 1, 11, 0x100));
    let offset: Offset = slot(c, 6);
    let mut bit = 7;
    assert_eq!(offset(c, 0, 0, 3, 1, 0, 0, &mut bit), (128 + 3) * 8);
    assert_eq!(bit, 0);
    assert_eq!(offset(c, 0, 0, 128, 0, 0, 0, std::ptr::null_mut()), 0);
    slot::<extern "system" fn(This) -> u32>(c, 1)(c);
}

#[test]
fn suggestions_follow_the_use() {
    let mut out = 0;
    let mut d = desc(256, 256, 1, 98, 0);
    assert_eq!(XGSuggestSwizzleMode(&d, 0, &mut out), S_OK);
    assert_eq!(out, 0x105);
    d.flags = ALLOW_RENDER_TARGET;
    XGSuggestSwizzleMode(&d, 0, &mut out);
    assert_eq!(out, 0x11b);
    d.flags = ALLOW_DEPTH_STENCIL;
    XGSuggestSwizzleMode(&d, 0, &mut out);
    assert_eq!(out, 0x118);
    // A 1D texture has no tiled layout, whatever its use.
    d.dimension = TEXTURE_1D;
    XGSuggestSwizzleMode(&d, 0, &mut out);
    assert_eq!(out, 0x100);
}

/// The title's calls: layout, then a copy in and out of each mip.
#[test]
fn layout_and_copies_through_the_object() {
    // 256x256 BC7, full chain, 64 KiB display mode.
    let c = create(&desc(256, 256, 0, 98, 0x10a));
    // SAFETY: plain data, filled by the call.
    let mut l: Box<LayoutOut> = Box::new(unsafe { std::mem::zeroed() });
    let get: extern "system" fn(This, *mut LayoutOut) -> HRESULT = slot(c, 2);
    assert_eq!(get(c, &mut *l), S_OK);
    assert_eq!(
        (l.size, l.alignment, l.mips, l.planes),
        (131072, 65536, 9, 1)
    );
    let p = &l.plane[0];
    assert_eq!((p.standard_mips, p.tail_mips, p.element_bytes), (1, 8, 16));
    assert_eq!((p.mips[0].offset, p.mips[0].pitch_bytes), (65536, 1024));
    assert_eq!((p.mips[1].offset, p.mips[1].in_tail), (32768, 1));

    type Into = extern "system" fn(This, *mut u8, u32, u32, *const u8, u32, u32) -> HRESULT;
    type From = extern "system" fn(This, *mut u8, u32, u32, u32, *const u8, u32, u32) -> HRESULT;
    let (into, from): (Into, From) = (slot(c, 8), slot(c, 9));
    let mut tiled = vec![0u8; l.size as usize];
    for mip in 0..9u32 {
        let m = &p.mips[mip as usize];
        let row = m.elements[0] * 16;
        let rows: Vec<u8> = (0..row * m.elements[1]).map(|i| (i ^ mip) as u8).collect();
        assert_eq!(
            into(c, tiled.as_mut_ptr(), 0, mip, rows.as_ptr(), row, 0),
            S_OK
        );
        let mut back = vec![0u8; rows.len()];
        assert_eq!(
            from(c, back.as_mut_ptr(), 0, mip, 0, tiled.as_ptr(), row, 0),
            S_OK
        );
        assert_eq!(back, rows, "mip {mip}");
    }
    // 0x50: one tile for the tail, one for mip 0.
    type Tiling = extern "system" fn(
        This,
        *mut u32,
        *mut PackedMips,
        *mut [u32; 3],
        *mut u32,
        u32,
        *mut SubresourceTiling,
    ) -> HRESULT;
    let tiling: Tiling = slot(c, 10);
    let (mut tiles, mut shape, mut n) = (0, [0; 3], 4);
    let mut packed = PackedMips {
        standard: 0,
        packed: 0,
        tiles: 0,
        start: 0,
    };
    let mut subs: Vec<SubresourceTiling> = (0..4)
        .map(|_| SubresourceTiling {
            width: 0,
            height: 0,
            depth: 0,
            start: 0,
        })
        .collect();
    assert_eq!(
        tiling(
            c,
            &mut tiles,
            &mut packed,
            &mut shape,
            &mut n,
            0,
            subs.as_mut_ptr()
        ),
        S_OK
    );
    assert_eq!(
        (tiles, packed.standard, packed.packed, packed.tiles),
        (2, 1, 8, 1)
    );
    assert_eq!((shape, n, subs[0].start), ([256, 256, 1], 1, 1));
    let release: extern "system" fn(This) -> u32 = slot(c, 1);
    assert_eq!(release(c), 0);

    // 4 KiB modes have no 64 KiB tiling.
    let null = std::ptr::null_mut::<u8>();
    let c = create(&desc(256, 256, 0, 98, 0x105));
    assert_eq!(
        tiling(
            c,
            &mut tiles,
            null.cast(),
            null.cast(),
            null.cast(),
            0,
            null.cast()
        ),
        E_FAIL
    );
    assert_eq!(release(c), 0);
}

#[test]
fn bad_descriptions_fail() {
    let mut c = std::ptr::null_mut();
    let bad = desc(256, 256, 1, 0, 0x10a);
    assert_eq!(XGCreateTextureComputer(&bad, &mut c), E_INVALIDARG);
    let bad = desc(256, 256, 1, 28, 0x103);
    assert_eq!(XGCreateTextureComputer(&bad, &mut c), E_INVALIDARG);
    assert!(module().exports.len() == 2);
}
