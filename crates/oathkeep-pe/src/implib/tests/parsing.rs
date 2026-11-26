use super::*;

#[test]
fn parses_named_code_imports() {
    // Deliberately out of order, to show that the result is sorted.
    let lib = archive(vec![
        (
            "version.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME,
                0,
                "GetFileVersionInfoSizeW",
                "version.dll",
                None,
            ),
        ),
        (
            "kernel32.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME,
                7,
                "ExitProcess",
                "kernel32.dll",
                None,
            ),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.other_members, 0);
    assert_eq!(
        parsed.imports,
        [
            LibImport {
                dll: "kernel32.dll".into(),
                // `object` does not expose the hint of a short-format
                // import, so it is always 0 here.
                export: ImportSymbol::Name {
                    hint: 0,
                    name: "ExitProcess".into()
                },
                kind: LibImportKind::Code,
            },
            LibImport {
                dll: "version.dll".into(),
                export: ImportSymbol::Name {
                    hint: 0,
                    name: "GetFileVersionInfoSizeW".into()
                },
                kind: LibImportKind::Code,
            },
        ]
    );
}

#[test]
fn parses_imports_by_ordinal() {
    let lib = archive(vec![(
        "d3d12.dll/",
        import_object(
            import_type::CODE,
            name_type::ORDINAL,
            42,
            "Alpha",
            "d3d12.dll",
            None,
        ),
    )]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.other_members, 0);
    assert_eq!(parsed.imports.len(), 1);
    assert_eq!(parsed.imports[0].dll, "d3d12.dll");
    assert_eq!(parsed.imports[0].export, ImportSymbol::Ordinal(42));
    assert_eq!(parsed.imports[0].kind, LibImportKind::Code);
}

#[test]
fn maps_the_import_kinds() {
    let lib = archive(vec![
        (
            "a.dll/",
            import_object(
                import_type::DATA,
                name_type::NAME,
                0,
                "tls_index",
                "a.dll",
                None,
            ),
        ),
        (
            "b.dll/",
            import_object(
                import_type::CONST,
                name_type::NAME,
                0,
                "limit",
                "b.dll",
                None,
            ),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    let kinds: Vec<_> = parsed.imports.iter().map(|i| i.kind).collect();
    assert_eq!(parsed.other_members, 0);
    assert_eq!(parsed.imports.len(), 2);
    assert!(kinds.contains(&LibImportKind::Data));
    assert!(kinds.contains(&LibImportKind::Const));
    assert!(!kinds.contains(&LibImportKind::Code));
}

#[test]
fn a_name_exported_under_another_name_uses_the_export() {
    let lib = archive(vec![(
        "a.dll/",
        import_object(
            import_type::CODE,
            name_type::NAME_EXPORTAS,
            0,
            "__imp_local",
            "a.dll",
            Some("Remote"),
        ),
    )]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.other_members, 0);
    assert_eq!(
        parsed.imports[0].export,
        ImportSymbol::Name {
            hint: 0,
            name: "Remote".into()
        }
    );
}

#[test]
fn prefixed_and_decorated_symbols_are_cleaned_up() {
    // `NAME_NO_PREFIX` drops a leading `?`, `@` or `_`; `NAME_UNDECORATE`
    // also stops at the first `@`. Both are what a linker writes for C
    // symbols, so the map holds the plain export name.
    let lib = archive(vec![
        (
            "a.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME_NO_PREFIX,
                0,
                "_Remap",
                "a.dll",
                None,
            ),
        ),
        (
            "b.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME_UNDECORATE,
                0,
                "?Thing@@YAHXZ",
                "b.dll",
                None,
            ),
        ),
        (
            "c.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME_NO_PREFIX,
                0,
                "plain",
                "c.dll",
                None,
            ),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    let names: Vec<_> = parsed
        .imports
        .iter()
        .map(|i| i.export.to_string())
        .collect();
    assert_eq!(parsed.other_members, 0);
    assert_eq!(names, ["Remap", "Thing", "plain"]);
}

#[test]
fn the_same_import_twice_is_listed_once() {
    let exit_process = || {
        import_object(
            import_type::CODE,
            name_type::NAME,
            0,
            "ExitProcess",
            "kernel32.dll",
            None,
        )
    };
    let lib = archive(vec![
        ("kernel32.dll/", exit_process()),
        ("kernel32.dll/", exit_process()),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.imports.len(), 1);
    assert_eq!(parsed.other_members, 0);
}

#[test]
fn the_same_symbol_of_a_different_kind_is_kept() {
    // A data import (`__imp_`) and a code import of one name differ.
    let lib = archive(vec![
        (
            "a.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME,
                0,
                "table",
                "a.dll",
                None,
            ),
        ),
        (
            "a.dll/",
            import_object(
                import_type::DATA,
                name_type::NAME,
                0,
                "table",
                "a.dll",
                None,
            ),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    assert_eq!(parsed.imports.len(), 2);
    assert_ne!(parsed.imports[0].kind, parsed.imports[1].kind);
}

#[test]
fn sorted_ordinal_and_named_exports_order_by_dll_then_name() {
    let lib = archive(vec![
        (
            "b.dll/",
            import_object(
                import_type::CODE,
                name_type::ORDINAL,
                1,
                "second",
                "b.dll",
                None,
            ),
        ),
        (
            "a.dll/",
            import_object(import_type::CODE, name_type::NAME, 0, "zeta", "a.dll", None),
        ),
        (
            "a.dll/",
            import_object(
                import_type::CODE,
                name_type::NAME,
                0,
                "alpha",
                "a.dll",
                None,
            ),
        ),
        (
            "a.dll/",
            import_object(
                import_type::CODE,
                name_type::ORDINAL,
                9,
                "first",
                "a.dll",
                None,
            ),
        ),
    ]);
    let parsed = ImportLibrary::parse(&lib).unwrap();
    let pairs: Vec<_> = parsed
        .imports
        .iter()
        .map(|i| (i.dll.as_str(), i.export.clone()))
        .collect();
    assert_eq!(
        pairs,
        [
            (
                "a.dll",
                ImportSymbol::Name {
                    hint: 0,
                    name: "alpha".into()
                }
            ),
            (
                "a.dll",
                ImportSymbol::Name {
                    hint: 0,
                    name: "zeta".into()
                }
            ),
            ("a.dll", ImportSymbol::Ordinal(9)),
            ("b.dll", ImportSymbol::Ordinal(1)),
        ]
    );
}
