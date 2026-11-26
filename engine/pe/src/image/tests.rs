use super::{ImageInfo, MACHINE_AMD64, PeError, dir, layout};

/// The test binary itself is a real MSVC-linked PE32+ image.
fn own_image() -> Vec<u8> {
    std::fs::read(std::env::current_exe().unwrap()).unwrap()
}

#[test]
fn parses_own_test_binary() {
    let info = ImageInfo::parse(&own_image()).unwrap();
    assert_eq!(info.machine, MACHINE_AMD64);
    assert!(!info.is_dll());
    assert!(info.entry_rva != 0);
    assert!(info.has_exception_table, "x64 images always carry .pdata");
    assert!(info.sections.iter().any(|s| s.name.starts_with(".text")));
    let kernel32 = info
        .imports
        .iter()
        .find(|m| m.dll.eq_ignore_ascii_case("kernel32.dll"))
        .expect("Rust std imports kernel32");
    assert!(kernel32.entries.iter().all(|e| e.iat_slot_rva != 0));
    // Rust std uses static TLS, so a TLS directory must be present (its
    // callback array may legitimately be empty).
    assert!(info.tls.is_some());
}

#[test]
fn rva_to_file_offset_maps_entry_point() {
    let data = own_image();
    let info = ImageInfo::parse(&data).unwrap();
    let off = info.rva_to_file_offset(info.entry_rva).unwrap() as usize;
    assert!(off < data.len());
}

#[test]
fn rva_to_file_offset_rejects_offsets_past_4_gib() {
    let mut info = ImageInfo::parse(&own_image()).unwrap();
    let rva = info.sections[0].rva + 0x20;
    assert!(info.rva_to_file_offset(rva).is_some());
    // Used to overflow (a panic in debug builds, a wrong offset in
    // release ones).
    info.sections[0].file_offset = u32::MAX - 0x10;
    assert_eq!(info.rva_to_file_offset(rva), None);
}

#[test]
fn rejects_garbage_without_panicking() {
    assert!(ImageInfo::parse(b"").is_err());
    assert!(ImageInfo::parse(b"MZ not really").is_err());
    let mut truncated = own_image();
    truncated.truncate(0x200);
    assert!(ImageInfo::parse(&truncated).is_err());
}

#[test]
fn rejects_iat_outside_image() {
    let mut data = own_image();
    let dd = layout::data_directory_offset(&data, dir::IMPORT).unwrap();
    let import_rva = layout::read_u32(&data, dd).unwrap();
    let desc = ImageInfo::parse(&data)
        .unwrap()
        .rva_to_file_offset(import_rva)
        .unwrap() as usize;
    // First descriptor's FirstThunk, at the 4 GiB limit: used to overflow.
    data[desc + 16..desc + 20].copy_from_slice(&0xFFFF_FFF8u32.to_le_bytes());
    assert!(matches!(
        ImageInfo::parse(&data),
        Err(PeError::Malformed(_))
    ));
}
/// A minimal PE32+ image: one section at RVA 0x1000 holding `body`, with
/// the export directory (RVA 0x1000, 0x80 bytes) pointing at it.
fn synthetic_image(body: &[u8]) -> Vec<u8> {
    let mut d = vec![0u8; 0x200];
    d[0..2].copy_from_slice(b"MZ");
    d[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    d[0x40..0x44].copy_from_slice(b"PE\0\0");
    d[0x44..0x46].copy_from_slice(&MACHINE_AMD64.to_le_bytes());
    d[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // sections
    d[0x54..0x56].copy_from_slice(&240u16.to_le_bytes()); // optional header size
    d[0x56..0x58].copy_from_slice(&0x2022u16.to_le_bytes()); // DLL, large address aware
    let opt = 0x58;
    d[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
    d[opt + 24..opt + 32].copy_from_slice(&0x1_8000_0000u64.to_le_bytes());
    d[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
    d[opt + 36..opt + 40].copy_from_slice(&0x200u32.to_le_bytes());
    d[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
    d[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
    d[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
    d[opt + 112..opt + 116].copy_from_slice(&0x1000u32.to_le_bytes());
    d[opt + 116..opt + 120].copy_from_slice(&0x80u32.to_le_bytes());
    let sec = opt + 240;
    d[sec..sec + 6].copy_from_slice(b".edata");
    d[sec + 8..sec + 12].copy_from_slice(&0x200u32.to_le_bytes()); // virtual size
    d[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes());
    d[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes()); // raw size
    d[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes()); // raw offset
    d.extend_from_slice(body);
    d.resize(0x400, 0);
    d
}

#[test]
fn rejects_section_extending_beyond_file() {
    // Create a synthetic PE image.
    let mut data = synthetic_image(&vec![0u8; 0x200]);

    // Remove the export directory to isolate section validation.
    let opt = 0x58;
    data[opt + 112..opt + 120].fill(0);

    // Verify the baseline PE is accepted.
    assert!(
        ImageInfo::parse(&data).is_ok(),
        "baseline synthetic image must be valid"
    );

    // The section header starts after the 240-byte optional header.
    let sec = opt + 240;

    // Declare 4096 bytes of raw section data.
    // The actual file contains only 1024 bytes.
    data[sec + 16..sec + 20].copy_from_slice(&0x1000u32.to_le_bytes());

    // Investigate whether the parser rejects this malformed image.
    assert!(
        ImageInfo::parse(&data).is_err(),
        "section extends beyond physical file bounds"
    );
}

#[test]
fn accepts_section_ending_at_file_boundary() {
    let mut data = synthetic_image(&vec![0u8; 0x200]);

    // Disable the export directory to isolate section validation.
    let opt = 0x58;
    data[opt + 112..opt + 120].fill(0);

    // Raw section: offset 0x200 + size 0x200 = file length 0x400.
    let info = ImageInfo::parse(&data);

    assert!(
        info.is_ok(),
        "section ending exactly at EOF should be accepted"
    );
}

#[test]
fn accepts_section_with_zero_raw_size() {
    let mut data = synthetic_image(&vec![0u8; 0x200]);

    // Disable the export directory.
    let opt = 0x58;
    data[opt + 112..opt + 120].fill(0);

    // Section header begins after the optional header.
    let sec = opt + 240;

    // A section may occupy virtual memory without having raw data.
    data[sec + 16..sec + 20].copy_from_slice(&0u32.to_le_bytes());

    let info = ImageInfo::parse(&data);

    assert!(
        info.is_ok(),
        "section with zero raw size should be accepted"
    );
}

#[test]
fn unused_export_slots_are_not_exports() {
    // Ordinal base 1; three address slots: "alpha", an unused hole (0),
    // and "gamma". The Windows loader treats address 0 as "not exported".
    let mut body = vec![0u8; 0x100];
    let put32 =
        |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put32(&mut body, 12, 0x1040); // name of the DLL
    put32(&mut body, 16, 1); // ordinal base
    put32(&mut body, 20, 3); // number of functions
    put32(&mut body, 24, 2); // number of names
    put32(&mut body, 28, 0x1028); // address table
    put32(&mut body, 32, 0x1034); // name pointer table
    put32(&mut body, 36, 0x103C); // name ordinal table
    put32(&mut body, 0x28, 0x1200);
    put32(&mut body, 0x2C, 0);
    put32(&mut body, 0x30, 0x1204);
    put32(&mut body, 0x34, 0x1044);
    put32(&mut body, 0x38, 0x104A);
    body[0x3C..0x3E].copy_from_slice(&0u16.to_le_bytes());
    body[0x3E..0x40].copy_from_slice(&2u16.to_le_bytes());
    body[0x40..0x44].copy_from_slice(b"t.dl");
    body[0x44..0x4A].copy_from_slice(b"alpha\0");
    body[0x4A..0x50].copy_from_slice(b"gamma\0");

    let info = ImageInfo::parse(&synthetic_image(&body)).unwrap();
    let exports: Vec<_> = info
        .exports
        .iter()
        .map(|e| (e.name.as_deref(), e.ordinal, e.rva))
        .collect();
    assert_eq!(
        exports,
        [
            (Some("alpha"), 1, Some(0x1200)),
            (Some("gamma"), 3, Some(0x1204))
        ]
    );
}

#[test]
fn every_name_of_a_shared_ordinal_survives() {
    // Two names for address slot 0, one for slot 1. The PE-specific
    // export table keeps only one name per slot, so "alpha" would be
    // lost; the generic iterator keeps both.
    let mut body = vec![0u8; 0x100];
    let put32 =
        |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put32(&mut body, 12, 0x1040); // name of the DLL
    put32(&mut body, 16, 1); // ordinal base
    put32(&mut body, 20, 2); // number of functions
    put32(&mut body, 24, 3); // number of names
    put32(&mut body, 28, 0x1028); // address table
    put32(&mut body, 32, 0x1034); // name pointer table
    put32(&mut body, 36, 0x1044); // name ordinal table
    put32(&mut body, 0x28, 0x1200);
    put32(&mut body, 0x2C, 0x1204);
    put32(&mut body, 0x34, 0x1050);
    put32(&mut body, 0x38, 0x1056);
    put32(&mut body, 0x3C, 0x105C);
    body[0x40..0x46].copy_from_slice(b"t.dll\0");
    body[0x44..0x46].copy_from_slice(&0u16.to_le_bytes());
    body[0x46..0x48].copy_from_slice(&0u16.to_le_bytes());
    body[0x48..0x4A].copy_from_slice(&1u16.to_le_bytes());
    body[0x50..0x56].copy_from_slice(b"alpha\0");
    body[0x56..0x5B].copy_from_slice(b"beta\0");
    body[0x5C..0x62].copy_from_slice(b"gamma\0");

    let info = ImageInfo::parse(&synthetic_image(&body)).unwrap();
    let exports: Vec<_> = info
        .exports
        .iter()
        .map(|e| (e.name.as_deref(), e.ordinal, e.rva))
        .collect();
    assert_eq!(
        exports,
        [
            (Some("alpha"), 1, Some(0x1200)),
            (Some("beta"), 1, Some(0x1200)),
            (Some("gamma"), 2, Some(0x1204)),
        ]
    );
}
