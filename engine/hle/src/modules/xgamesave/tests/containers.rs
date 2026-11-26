use std::ffi::{c_char, c_void};
use std::path::Path;

use super::super::abi::{
    close_container, close_provider, close_update, create_container, delete_async, delete_result,
    enumerate_containers, quota_async, quota_result, read_async, read_result,
};
use super::super::ops::{BLOB, Container, ContainerInfo, Provider, This, Update, give, read};
use super::super::store::{
    container_dir, containers, current_named, decode, file_name, valid_container,
};
use super::super::{
    BLOB_NOT_FOUND, BUFFER_TOO_SMALL, INVALID_CONTAINER_NAME, MANIFEST, NO_ACCESS, OBJECT, QUOTA,
    saves_lock,
};
use super::{block, old_state, state, update};
use crate::modules::xasync::{self, AsyncBlock};
use crate::modules::xgameruntime::{HRESULT, S_OK, method, text};
use crate::modules::xuser;

#[test]
fn names_are_exact() {
    for name in ["Save_1", "slot/2", "a.b c%", "\u{e9}t\u{e9}"] {
        let file = file_name(name);
        assert!(!file.contains(['.', '/', ' ']), "{file}");
        assert_eq!(decode(&file).as_deref(), Some(name));
    }
    assert_eq!(decode("x.new"), None);
    for good in ["Profile/Slot_1", "a/b/Save-2.sav", "x", &"a".repeat(256)] {
        assert!(valid_container(good), "{good}");
    }
    for bad in ["a b", "", "a/", "a.", "a..b", "a.b/c", "a-b/c", &"a".repeat(257)] {
        assert!(!valid_container(bad), "{bad}");
    }

    // Containers and blobs differing only in case, reserved device
    // names, characters the host forbids, long names: all distinct.
    let dir = tempfile::tempdir().unwrap();
    let p = Provider { root: dir.path().into() };
    let long = "n".repeat(300);
    let blobs = ["A", "a", "CON", "nul.txt", "x/y:z*?\"<>|", long.as_str(), "end. "];
    for container in ["Save", "save", "SAVE/CON.x"] {
        let c = p.container(container);
        let u = Update {
            name: c.name.clone(),
            dir: c.dir.clone(),
            display: None,
            changes: blobs
                .iter()
                .map(|b| (b.to_string(), Some(format!("{container}:{b}").into_bytes())))
                .collect(),
        };
        assert_eq!(u.submit(), S_OK, "{container}");
    }
    let _saves = saves_lock();
    let names: Vec<String> = containers(dir.path()).into_iter().map(|c| c.0).collect();
    drop(_saves);
    assert_eq!(names, ["SAVE/CON.x", "Save", "save"]);
    for container in ["Save", "save", "SAVE/CON.x"] {
        let read = read(&p.container(container), None).unwrap();
        let mut expected: Vec<_> = blobs
            .iter()
            .map(|b| (b.to_string(), format!("{container}:{b}").into_bytes()))
            .collect();
        expected.sort();
        assert_eq!(read, expected, "{container}");
    }
}

#[test]
fn the_old_layout_moves_over() {
    let saves = tempfile::tempdir().unwrap();
    let root = saves.path().join(format!("{:016X}", xuser::XUID)).join("scid");
    let old = root.join("Player");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("Save%20Game%20Data"), b"data").unwrap();
    std::fs::write(old.join("Save%20Game%20Header"), b"header").unwrap();
    std::fs::write(old.join("Save%20Game%20Data.new"), b"half").unwrap();
    std::fs::write(old.join(".display"), b"Chapter 1").unwrap();
    let p = Provider::open(saves.path(), Some("scid")).unwrap();
    assert!(!old.exists());
    let c = p.container("Player");
    let blobs = read(&c, None).unwrap();
    assert_eq!(
        blobs,
        [
            ("Save Game Data".into(), b"data".to_vec()),
            ("Save Game Header".into(), b"header".to_vec())
        ]
    );
    let v = current_named(&c.dir, "Player").unwrap().unwrap();
    assert_eq!((v.dir.file_name(), v.display.as_str()), (Some("v1".as_ref()), "Chapter 1"));
}

/// An empty configuration id names none. As a folder name it would be
/// the folder every configuration lives in, whose folders then look
/// like containers of the old layout and are converted away.
#[test]
fn an_empty_configuration_id_is_the_default_one() {
    let saves = tempfile::tempdir().unwrap();
    let scid = Provider::open(saves.path(), Some("scid")).unwrap();
    assert_eq!(update(&scid.root, "one", &[("a", Some("kept"))]), S_OK);
    let kept = Some(("one".into(), vec![("a".into(), "kept".into())]));
    assert_eq!(state(&scid.root), kept);

    let empty = Provider::open(saves.path(), Some("")).unwrap();
    assert!(scid.root.is_dir(), "another configuration's saves are gone");
    assert_eq!(state(&scid.root), kept);
    let default = Provider::open(saves.path(), None).unwrap();
    assert_eq!(empty.root, default.root);

    // Named configurations stay apart, from it and from each other.
    assert_ne!(empty.root, scid.root);
    assert_eq!(state(&empty.root), None);
    let other = Provider::open(saves.path(), Some("other")).unwrap();
    assert_eq!(state(&other.root), None);
    assert_eq!(state(&scid.root), kept);
}

#[test]
fn saves_persist_across_providers() {
    let this: This = &OBJECT;
    let saves = tempfile::tempdir().unwrap();
    let provider = give(Provider::open(saves.path(), Some("scid")).unwrap());

    // Write two blobs through an update.
    let mut container = std::ptr::null_mut();
    type Create = extern "system" fn(
        This,
        *const Provider,
        *const c_char,
        *mut *mut Container,
    ) -> HRESULT;
    let create = method::<Create>(&OBJECT, 0x80);
    assert_eq!(
        create(this, provider, c"bad name".as_ptr(), &mut container),
        INVALID_CONTAINER_NAME
    );
    assert_eq!(
        create(this, provider, c"Slot_1".as_ptr(), &mut container),
        S_OK
    );
    let mut update = std::ptr::null_mut();
    type Update_ =
        extern "system" fn(This, *const Container, *const c_char, *mut *mut Update) -> HRESULT;
    assert_eq!(
        method::<Update_>(&OBJECT, 0xb8)(this, container, c"Chapter 1".as_ptr(), &mut update),
        S_OK
    );
    type Write =
        extern "system" fn(This, *mut Update, *const c_char, *const u8, usize) -> HRESULT;
    let write = method::<Write>(&OBJECT, 0xc8);
    assert_eq!(
        write(this, update, c"world".as_ptr(), b"hello".as_ptr(), 5),
        S_OK
    );
    assert_eq!(
        write(this, update, c"player.dat".as_ptr(), b"xy".as_ptr(), 2),
        S_OK
    );
    assert_eq!(
        write(this, update, c"gone".as_ptr(), b"z".as_ptr(), 1),
        S_OK
    );
    type Delete = extern "system" fn(This, *mut Update, *const c_char) -> HRESULT;
    assert_eq!(method::<Delete>(&OBJECT, 0xd0)(this, update, c"gone".as_ptr()), S_OK);
    type Submit = extern "system" fn(This, *const Update, *mut AsyncBlock) -> HRESULT;
    let mut b = block();
    assert_eq!(method::<Submit>(&OBJECT, 0xe0)(this, update, &mut b), S_OK);
    assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
    close_update(this, update);
    close_container(this, container);
    close_provider(this, provider);

    // A new provider sees the container and reads both blobs back.
    let provider = give(Provider::open(saves.path(), Some("scid")).unwrap());
    extern "system" fn seen(info: *const ContainerInfo, context: *mut c_void) -> bool {
        // SAFETY: valid during the callback; the context is the test's vector.
        let (info, out) = unsafe {
            (
                &*info,
                &mut *context.cast::<Vec<(String, String, u32, u64)>>(),
            )
        };
        out.push((
            text(info.name).unwrap(),
            text(info.display_name).unwrap(),
            info.blob_count,
            info.total_size,
        ));
        true
    }
    let mut found: Vec<(String, String, u32, u64)> = Vec::new();
    let context = (&raw mut found).cast();
    assert_eq!(
        enumerate_containers(this, provider, context, Some(seen)),
        S_OK
    );
    assert_eq!(found, [("Slot_1".into(), "Chapter 1".into(), 2, 7)]);

    let mut container = std::ptr::null_mut();
    assert_eq!(
        create_container(this, provider, c"Slot_1".as_ptr(), &mut container),
        S_OK
    );
    let mut b = block();
    let wanted = [c"world".as_ptr(), c"player.dat".as_ptr()];
    assert_eq!(
        read_async(this, container, wanted.as_ptr(), 2, &mut b),
        S_OK
    );
    assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
    let mut size = 0;
    assert_eq!(xasync::XAsyncGetResultSize(&mut b, &mut size), S_OK);
    assert_eq!(
        size,
        2 * BLOB + "world\0hello".len() + "player.dat\0xy".len()
    );
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let mut count = 0;
    assert_eq!(
        read_result(this, &mut b, 8, buffer.as_mut_ptr().cast(), &mut count),
        BUFFER_TOO_SMALL
    );
    assert_eq!(
        read_result(this, &mut b, size, buffer.as_mut_ptr().cast(), &mut count),
        S_OK
    );
    assert_eq!(count, 2);
    let blob = |i: usize| {
        // SAFETY: the layout the call wrote.
        unsafe {
            let entry = buffer.as_ptr().cast::<u8>().add(i * BLOB);
            let name = text(entry.cast::<*const c_char>().read_unaligned()).unwrap();
            let len = entry.add(8).cast::<u32>().read_unaligned() as usize;
            let data = std::slice::from_raw_parts(
                entry.add(16).cast::<*const u8>().read_unaligned(),
                len,
            );
            (name, data.to_vec())
        }
    };
    assert_eq!(blob(0), ("world".into(), b"hello".to_vec()));
    assert_eq!(blob(1), ("player.dat".into(), b"xy".to_vec()));

    let missing = [c"nothing".as_ptr()];
    let mut one = 1;
    let mut small = [0u8; 8];
    let read_now = method::<
        extern "system" fn(
            This,
            *const Container,
            *const *const c_char,
            *mut u32,
            usize,
            *mut u8,
        ) -> HRESULT,
    >(&OBJECT, 0xa0);
    assert_eq!(
        read_now(
            this,
            container,
            missing.as_ptr(),
            &mut one,
            8,
            small.as_mut_ptr()
        ),
        BLOB_NOT_FOUND
    );
    assert_eq!(
        read_now(
            this,
            container,
            wanted.as_ptr(),
            &mut 2,
            8,
            small.as_mut_ptr()
        ),
        BUFFER_TOO_SMALL
    );

    // Quota counts the blobs; deleting the container frees it.
    let mut b = block();
    assert_eq!(quota_async(this, provider, &mut b), S_OK);
    let mut left = 0i64;
    assert_eq!(quota_result(this, &mut b, &mut left), S_OK);
    assert!(left as u64 <= QUOTA - 7);
    let mut b = block();
    assert_eq!(
        delete_async(this, provider, c"Slot_1".as_ptr(), &mut b),
        S_OK
    );
    assert_eq!(delete_result(this, &mut b), S_OK);
    found.clear();
    let context = (&raw mut found).cast();
    assert_eq!(
        enumerate_containers(this, provider, context, Some(seen)),
        S_OK
    );
    assert!(found.is_empty());
    close_container(this, container);
    close_provider(this, provider);
}

/// Only `v<N>` as `commit` writes it is a version: `v01` and the like
/// are neither read, counted nor removed (#870).
#[test]
fn a_noncanonical_folder_does_not_remove_a_committed_save() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]), S_OK);
    let slot = container_dir(root, "Slot");
    for stray in ["v01", "v001", "v+1", "v0", "v02"] {
        std::fs::create_dir(slot.join(stray)).unwrap();
    }
    assert_eq!(state(root), old_state());
    assert!(slot.join("v1").is_dir());
    for stray in ["v01", "v001", "v+1", "v0", "v02"] {
        assert!(slot.join(stray).exists(), "{stray} was removed");
    }
}

/// A newer version folder that does not read (an empty folder, a file,
/// a broken manifest) neither hides the committed save, nor removes it,
/// nor blocks the next update, which goes above it.
#[test]
fn an_unreadable_newer_version_does_not_cost_the_save() {
    type Make = fn(&Path);
    let strays: [(&str, Make); 3] = [
        ("empty folder", |p| std::fs::create_dir(p).unwrap()),
        ("file", |p| std::fs::write(p, b"not a version").unwrap()),
        ("broken manifest", |p| {
            std::fs::create_dir(p).unwrap();
            std::fs::write(p.join(MANIFEST), [9, 0, 0, 0, b'x']).unwrap();
        }),
    ];
    for (what, make) in strays {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]), S_OK);
        let slot = container_dir(root, "Slot");
        make(&slot.join("v2"));

        assert_eq!(state(root), old_state(), "{what}: read");
        assert!(slot.join("v1").is_dir(), "{what}: v1 was removed");

        assert_eq!(update(root, "two", &[("a", Some("new a"))]), S_OK, "{what}: update");
        let blobs = vec![("a".into(), "new a".into()), ("b".into(), "old b".into())];
        let after = Some(("two".into(), blobs));
        assert_eq!(state(root), after, "{what}: after the update");
        assert!(slot.join("v3").is_dir(), "{what}: the update did not go above v2");
        // Read again: the new version stays the container.
        assert_eq!(state(root), after, "{what}: second read");
    }
}

/// With no readable version at all, reads and updates fail and nothing
/// is removed.
#[test]
fn no_readable_version_is_refused_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(update(root, "one", &[("a", Some("old a"))]), S_OK);
    let slot = container_dir(root, "Slot");
    std::fs::write(slot.join("v1").join(MANIFEST), [1, 2, 3]).unwrap();
    let c = Provider { root: root.into() }.container("Slot");
    assert_eq!(read(&c, None).err(), Some(NO_ACCESS));
    assert_eq!(update(root, "two", &[("a", Some("new a"))]), NO_ACCESS);
    assert!(slot.join("v1").join("0").is_file());
    assert!(!slot.join("v2").exists());
}
