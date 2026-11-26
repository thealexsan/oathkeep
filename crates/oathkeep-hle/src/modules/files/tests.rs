use super::*;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

#[test]
fn kernel_reads_reach_the_buffer() {
    use std::os::windows::io::AsRawHandle;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.bin");
    std::fs::write(&path, b"0123456789").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let (mut out, mut n) = ([0u8; 10], 0);
    let handle = file.as_raw_handle().cast();
    assert_ne!(
        ReadFile(handle, out.as_mut_ptr(), 10, &mut n, std::ptr::null_mut()),
        0
    );
    assert_eq!((&out, n), (b"0123456789", 10));
}

#[test]
fn fread_reads_beside_in_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.bin");
    // Over two chunks, and not a whole number of 3-byte items.
    let bytes: Vec<u8> = (0..(5 << 19) + 1).map(|i| (i * 7 + i / 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let fclose = host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32);
    let open = || {
        let f = _wfopen(w(&path.to_string_lossy()).as_ptr(), w("rb").as_ptr());
        assert!(!f.is_null());
        f
    };

    // Asked for far more than the file holds: the whole file, in items.
    let f = open();
    let mut out = vec![0xaau8; bytes.len() + 64];
    let items = fread(out.as_mut_ptr(), 3, out.len() / 3, f);
    assert_eq!(items, bytes.len() / 3);
    assert_eq!(
        &out[..bytes.len()],
        &bytes[..],
        "the partial last item's bytes too"
    );
    assert!(
        out[bytes.len()..].iter().all(|&b| b == 0xaa),
        "nothing past what was read"
    );
    fclose(f);

    // Exact reads continue where the last one stopped.
    let f = open();
    let mut a = vec![0u8; 1000];
    let mut b = vec![0u8; (1 << 20) + 5];
    assert_eq!(fread(a.as_mut_ptr(), 1000, 1, f), 1);
    assert_eq!(fread(b.as_mut_ptr(), 1, b.len(), f), b.len());
    assert_eq!(a, bytes[..1000]);
    assert_eq!(b, bytes[1000..1000 + b.len()]);
    fclose(f);
}

fn text(v: &[u16]) -> String {
    String::from_utf16_lossy(&v[..v.len() - 1])
}

#[test]
fn rewrites_only_paths_on_mounted_drives() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let letter = mount(dir.path()).unwrap();
    assert_eq!(
        mount(dir.path()),
        Some(letter),
        "mounting twice reuses the letter"
    );
    let tr = |s: String| translate(&w(&s)[..s.encode_utf16().count()]).map(|v| text(&v));
    assert_eq!(
        tr(format!("{letter}:/a/b.txt")),
        Some(format!("{root}/a/b.txt"))
    );
    let lower = letter.to_ascii_lowercase();
    assert_eq!(tr(format!("{lower}:\\a")), Some(format!("{root}\\a")));
    assert_eq!(tr(format!("{letter}:")), Some(format!("{root}\\")));
    assert_eq!(
        tr(format!(r"\\?\{letter}:/a/b")),
        Some(format!(r"\\?\{root}\a\b"))
    );
    assert_eq!(tr(r"C:\Windows".into()), None);
    assert_eq!(tr("relative/path".into()), None);
    assert_eq!(tr(String::new()), None);

    // Fixed letters, as the console's package drive; a later mount of
    // the same letter replaces the earlier one.
    let package = tempfile::tempdir().unwrap();
    mount_at('g', dir.path());
    mount_at('G', package.path());
    assert_eq!(
        tr(r"G:\appdata0.rpf".into()),
        Some(format!(r"{}\appdata0.rpf", package.path().display()))
    );
    assert_ne!(mount(dir.path()), Some('G'), "free letters skip fixed ones");

    // Long ones take the `\\?\` form, cleaned up as the host would.
    let long = "d".repeat(250);
    assert_eq!(
        tr(format!(r"G:/a/./b/../{long}//f")),
        Some(format!(r"\\?\{}\a\{long}\f", package.path().display()))
    );
    assert_eq!(
        verbatim(&w(r"\\server\share\")[..15], &w("/x/../../y")[..10]),
        w(r"\\?\UNC\server\share\y")[..22].to_vec()
    );
}

/// A title run by its file name alone has an empty folder: the drive
/// over it is the current directory, not the current drive's root.
#[test]
fn an_empty_root_is_the_current_directory() {
    let letter = mount(Path::new("")).unwrap();
    let path = format!(r"{letter}:\a.rpf");
    let host = translate(&w(&path)[..path.len()]).map(|v| text(&v));
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(host, Some(cwd.join("a.rpf").display().to_string()));
}

#[test]
fn long_title_paths_work_on_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let letter = mount(dir.path()).unwrap();
    let deep = format!("{letter}:/{}/{}", "a".repeat(200), "b".repeat(100));
    let null = std::ptr::null_mut();
    let first = w(&deep[..203]);
    assert_ne!(CreateDirectoryW(first.as_ptr(), null), 0);
    assert_ne!(CreateDirectoryW(w(&deep).as_ptr(), null), 0);
    let file = format!("{deep}/../{}/f.txt", "b".repeat(100));
    let f = _wfopen(w(&file).as_ptr(), w("wb").as_ptr());
    assert!(!f.is_null(), "{file}");
    host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32)(f);
    assert_ne!(DeleteFileW(w(&format!("{deep}/f.txt")).as_ptr()), 0);
    assert_ne!(RemoveDirectoryW(w(&deep).as_ptr()), 0);
}

#[test]
fn title_file_calls_land_in_the_host_folder() {
    let dir = tempfile::tempdir().unwrap();
    let letter = mount(dir.path()).unwrap();
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const CREATE_ALWAYS: u32 = 2;
    let close = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32);
    let fclose = host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32);

    let path = w(&format!(r"{letter}:\created.bin"));
    let null = std::ptr::null_mut();
    let h = CreateFileW(
        path.as_ptr(),
        GENERIC_WRITE,
        0,
        null,
        CREATE_ALWAYS,
        0,
        null,
    );
    assert_ne!(h as isize, -1);
    close(h);
    assert!(dir.path().join("created.bin").is_file());

    let path = format!("{letter}:/opened.txt\0");
    let f = fopen(path.as_ptr().cast(), c"wb".as_ptr());
    assert!(!f.is_null());
    fclose(f);
    assert!(dir.path().join("opened.txt").is_file());

    let path = format!("{letter}:/stream.txt\0");
    const OUT: i32 = 2;
    let f = fiopen(path.as_ptr().cast(), OUT, 0x40);
    assert!(!f.is_null());
    fclose(f);
    assert!(dir.path().join("stream.txt").is_file());

    assert_eq!(remove(path.as_ptr().cast()), 0);
    assert!(!dir.path().join("stream.txt").exists());
    let f = fiopen_wide(w(&format!("{letter}:/wide.txt")).as_ptr(), OUT, 0x40);
    assert!(!f.is_null());
    fclose(f);
    assert!(dir.path().join("wide.txt").is_file());

    const OPEN_EXISTING: u32 = 3;
    let path = w(&format!(r"{letter}:\created.bin"));
    let h = CreateFile2(path.as_ptr(), GENERIC_WRITE, 0, OPEN_EXISTING, null);
    assert_ne!(h as isize, -1);
    close(h);
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
    assert_ne!(GetFileAttributesW(path.as_ptr()), INVALID_FILE_ATTRIBUTES);

    const O_WRONLY: i32 = 1;
    const O_CREAT: i32 = 0x100;
    const S_IWRITE: i32 = 0x80;
    let path = format!("{letter}:/low.bin\0");
    let fd = _open(path.as_ptr().cast(), O_WRONLY | O_CREAT, S_IWRITE);
    assert!(fd >= 0);
    host!("ucrtbase.dll", "_close": extern "C" fn(i32) -> i32)(fd);
    assert!(dir.path().join("low.bin").is_file());

    // Other paths pass through untouched.
    let host_path = format!("{}\0", dir.path().join("plain.txt").display());
    let f = fopen(host_path.as_ptr().cast(), c"wb".as_ptr());
    assert!(!f.is_null());
    fclose(f);
    assert!(dir.path().join("plain.txt").is_file());
}

/// A narrow path is rewritten once, as its wide twin is. The inner
/// drive's folder lies on the outer virtual drive, as the title folder
/// does when it sits on a host `G:` drive and is mounted as `G:`. The
/// outer letter is no host drive, so the host finds nothing there; a
/// narrow call that reaches the outer folder rewrote the path twice.
#[test]
fn narrow_paths_are_rewritten_once() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("inner");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("present.txt"), b"").unwrap();
    let outer = mount(dir.path()).unwrap();
    let inner = mount(Path::new(&format!(r"{outer}:\inner"))).unwrap();
    let path = |name: &str| format!("{inner}:/{name}\0");

    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
    let wide = GetFileAttributesW(w(&format!("{inner}:/present.txt")).as_ptr());
    assert_eq!(wide, INVALID_FILE_ATTRIBUTES);
    let narrow = GetFileAttributesA(path("present.txt").as_ptr().cast());
    assert_eq!(narrow, wide, "GetFileAttributesA agrees with its wide twin");

    const GENERIC_WRITE: u32 = 0x4000_0000;
    const CREATE_ALWAYS: u32 = 2;
    let null = std::ptr::null_mut();
    let h = CreateFileA(
        path("a.bin").as_ptr().cast(),
        GENERIC_WRITE,
        0,
        null,
        CREATE_ALWAYS,
        0,
        null,
    );
    assert_eq!(h as isize, -1);
    assert!(fopen(path("b.txt").as_ptr().cast(), c"wb".as_ptr()).is_null());
    let mut f: File = std::ptr::null_mut();
    let error = fopen_s(&mut f, path("c.txt").as_ptr().cast(), c"wb".as_ptr());
    assert_ne!(error, 0);
    const OUT: i32 = 2;
    assert!(fiopen(path("d.txt").as_ptr().cast(), OUT, 0x40).is_null());

    // Nothing was made behind a path rewritten twice.
    let names: Vec<_> = std::fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["present.txt"]);
}
