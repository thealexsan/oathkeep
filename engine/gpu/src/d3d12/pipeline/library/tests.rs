use super::*;

#[test]
fn names_change_with_what_goes_into_the_pipeline() {
    let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
    assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
    assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
    assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
    assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
    assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
    // Parts do not run into each other.
    assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
}

#[test]
fn a_rejected_blob_gives_an_empty_library() {
    let Some(device) = crate::backend::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
        return; // no host GPU here
    };
    let library = open(&device, Blob::Held(b"not a pipeline library".repeat(16))).expect("an empty library");
    // SAFETY: a plain query on a live library.
    assert!(unsafe { library.library.GetSerializedSize() } > 0);
    assert!(matches!(library._blob, Blob::None));
}

/// A mapped file reads as the file's bytes, a file written through a
/// mapping holds what was written, and the library's file can be
/// replaced while it is mapped (or the new one waits as the next).
#[test]
fn the_library_file_is_mapped_and_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(LIBRARY_FILE);
    assert!(matches!(Blob::of(dir.path()), Blob::None), "no file yet");
    std::fs::write(&path, b"").unwrap();
    assert!(matches!(Blob::of(dir.path()), Blob::None), "an empty file");
    let old: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    std::fs::write(&path, &old).unwrap();
    let blob = Blob::of(dir.path());
    assert!(matches!(blob, Blob::Mapped(_)) || !mapped());
    assert_eq!(blob.bytes(), &old[..]);
    // A save while the old file is mapped.
    let tmp = path.with_extension("tmp");
    let new: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
    {
        let mut file = Mapped::create(&tmp, new.len()).expect("a new file, mapped");
        file.bytes_mut().copy_from_slice(&new);
    }
    assert_eq!(std::fs::read(&tmp).unwrap(), new);
    replace_library(dir.path(), &tmp).unwrap();
    assert!(!tmp.exists());
    // The mapping still reads the old library.
    assert_eq!(blob.bytes(), &old[..]);
    drop(blob);
    // The next start reads the new one, wherever the save left it.
    assert_eq!(Blob::of(dir.path()).bytes(), &new[..]);
    assert!(!dir.path().join(NEXT_FILE).exists());
}

/// The bytes at `at` of the pattern the test below writes.
fn pattern(at: usize, out: &mut [u8]) {
    for (i, word) in out.chunks_exact_mut(4).enumerate() {
        let v = ((at / 4 + i) as u32).wrapping_mul(2_654_435_761).wrapping_add(1);
        word.copy_from_slice(&v.to_le_bytes());
    }
}

/// A file of `len` bytes (whole MiB) of [`pattern`], written past the
/// system's cache: none of its pages is in memory afterwards. False
/// where the folder's file system does not take such writes.
fn write_uncached(path: &std::path::Path, len: usize) -> bool {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH
    const UNCACHED: u32 = 0x2000_0000 | 0x8000_0000;
    const CHUNK: usize = 1 << 20;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(UNCACHED)
        .open(path)
    else {
        return false;
    };
    // Such writes want a buffer on a sector boundary.
    let mut store = vec![0u8; CHUNK + 4096];
    let start = store.as_ptr().align_offset(4096);
    for at in (0..len).step_by(CHUNK) {
        let chunk = &mut store[start..start + CHUNK];
        pattern(at, chunk);
        if file.write_all(chunk).is_err() {
            return false;
        }
    }
    true
}

/// The device loss of runs 72, 73 and 82: the library's file is
/// replaced while it is mapped, and pages of the mapping that were not
/// in memory then are read afterwards (in the runs: pages the trim let
/// go and Windows gave to something else). With the file's handle kept
/// open they still read as the old file. (With it closed, as before,
/// all but the first few read as zeros on the PC this was found on.)
#[test]
fn a_replaced_library_file_still_reads_from_the_disk() {
    const LEN: usize = 8 << 20;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(LIBRARY_FILE);
    if !write_uncached(&path, LEN) {
        return; // no uncached writes here: nothing to show
    }
    let mapped = Mapped::open_holding(&path, true).expect("the file, mapped");
    // A save: the new library, renamed over the mapped one.
    let tmp = path.with_extension("tmp");
    {
        let mut file = Mapped::create(&tmp, LEN).expect("a new file, mapped");
        file.bytes_mut().fill(0xEE);
    }
    replace_library(dir.path(), &tmp).unwrap();
    // Every page of the old mapping, read for the first time now.
    let mut expected = vec![0u8; 4096];
    for (page, bytes) in mapped.bytes().chunks(4096).enumerate() {
        pattern(page * 4096, &mut expected);
        assert!(bytes == &expected[..], "page {page} of the replaced library no longer reads as the old file");
    }
}

#[test]
fn concurrent_write_atomically_never_fails_or_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("host-pipelines.bin");
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let target = target.clone();
            std::thread::spawn(move || {
                let payload = vec![b'a' + t as u8; 4096];
                for _ in 0..30 {
                    write_atomically(&target, &payload).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let final_bytes = std::fs::read(&target).unwrap();
    assert_eq!(final_bytes.len(), 4096);
    assert!(final_bytes.iter().all(|&b| b == final_bytes[0]));
}
