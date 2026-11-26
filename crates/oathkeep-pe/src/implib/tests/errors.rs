use super::*;

#[test]
fn an_empty_archive_has_no_imports() {
    let parsed = ImportLibrary::parse(b"!<arch>\n").unwrap();
    assert!(parsed.imports.is_empty());
    assert_eq!(parsed.other_members, 0);
}

#[test]
fn linker_and_names_members_are_skipped_not_counted() {
    // What a COFF archive looks like: two linker members, an optional
    // names table, then the import objects.
    let lib = archive(vec![
        ("/", vec![0; 8]),
        ("/", vec![0; 8]),
        ("//", b"a_long_member_name/\n".to_vec()),
        (
            "kernel32.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME,
                0,
                "ExitProcess",
                "kernel32.dll",
                None,
            ),
        ),
        (
            "user32.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME,
                0,
                "MessageBoxW",
                "user32.dll",
                None,
            ),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.imports.len(), 2);
    assert_eq!(parsed.other_members, 0);
}

#[test]
fn members_that_are_not_import_objects_are_counted() {
    let lib = archive(vec![
        // A regular COFF object starts with a machine, not a zero `Sig1`.
        ("object.obj/", {
            let mut object = Vec::new();
            object.extend_from_slice(&MACHINE_AMD64.to_le_bytes());
            object.extend_from_slice(&1u16.to_le_bytes()); // NumberOfSections
            object.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
            object.resize(60, 0);
            object
        }),
        // A long-format descriptor: no short-format `Sig2`.
        ("long.lib/", {
            let mut long = import_object(
                import_type::CODE,
                name_type::NAME,
                0,
                "Old",
                "old.dll",
                None,
            );
            long[2..4].copy_from_slice(&0u16.to_le_bytes());
            long
        }),
        // An import header whose string table is cut short.
        ("short.lib/", {
            let mut short = import_object(
                import_type::CODE,
                name_type::NAME,
                0,
                "Cut",
                "cut.dll",
                None,
            );
            short.truncate(24);
            short
        }),
        (
            "a.dll/",
            import_object(import_type::CODE, name_type::NAME, 0, "Kept", "a.dll", None),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.other_members, 3);
    assert_eq!(parsed.imports.len(), 1);
    assert_eq!(parsed.imports[0].dll, "a.dll");
}

#[test]
fn odd_sized_members_are_padded_aligned() {
    // 20-byte header plus 9 bytes of strings: the archive pads one byte,
    // and the next member must still be found.
    let odd = import_object(import_type::CODE, name_type::NAME, 0, "f", "ab", None);
    assert_eq!(odd.len() % 2, 1);
    let lib = archive(vec![
        ("a.dll/", odd),
        (
            "b.dll/",
            import_object(import_type::CODE, name_type::NAME, 0, "Next", "b.dll", None),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.imports.len(), 2);
    assert_eq!(parsed.other_members, 0);
}

#[test]
fn names_that_are_not_utf8_are_reported_lossily() {
    // The symbol name "Café" is stored as UTF-8; replacing its lead byte
    // leaves a stray continuation byte, so the member holds a name that
    // is not valid UTF-8.
    let mut lib = archive(vec![(
        "x.dll/",
        import_object(
            import_type::CODE,
            name_type::NAME,
            0,
            "Caf\u{e9}",
            "x.dll",
            None,
        ),
    )]);
    let at = lib
        .windows(2)
        .position(|w| w == [0xC3, 0xA9])
        .expect("the name is in the member");
    lib[at] = 0xFF;
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(
        parsed.imports[0].export,
        // One replacement character for the invalid byte, one for the
        // continuation byte that is then unexpected.
        ImportSymbol::Name {
            hint: 0,
            name: "Caf\u{fffd}\u{fffd}".into()
        }
    );
}

#[test]
fn rejects_input_that_is_not_an_archive() {
    let cases: [&[u8]; 3] = [
        b"",
        b"MZ a PE image, not a library",
        b"!<arch>", // truncated magic
    ];
    for data in cases {
        assert!(
            matches!(ImportLibrary::parse(data), Err(PeError::Malformed(_))),
            "{data:?} must be rejected"
        );
    }
}

#[test]
fn rejects_a_truncated_member_header() {
    let mut data = b"!<arch>\n".to_vec();
    data.extend_from_slice(&[b'x'; 20]);
    assert!(matches!(
        ImportLibrary::parse(&data),
        Err(PeError::Malformed(_))
    ));
}

#[test]
fn rejects_a_member_whose_data_is_cut_off() {
    // The header promises 0x100 bytes, the archive holds 4.
    let mut data = b"!<arch>\n".to_vec();
    let mut member = archive_member("a.dll/", &[0u8; 0x100]);
    member.truncate(60 + 4);
    data.extend_from_slice(&member);
    assert!(matches!(
        ImportLibrary::parse(&data),
        Err(PeError::Malformed(_))
    ));
}
