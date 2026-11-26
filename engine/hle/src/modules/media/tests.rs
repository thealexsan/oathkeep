use super::super::files;
use super::url::{local, percent_decode};
use super::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn text(v: &[u16]) -> String {
    String::from_utf16_lossy(v.strip_suffix(&[0]).unwrap_or(v))
}

#[test]
fn percent_decode_requires_two_hex_digits() {
    assert_eq!(percent_decode("%20"), " ");
    assert_eq!(percent_decode("%+1"), "%+1");
    assert_eq!(percent_decode("%2"), "%2");
    assert_eq!(percent_decode("%%20"), "% ");
    assert_eq!(percent_decode("a%2Gb"), "a%2Gb");
}

#[test]
fn only_local_files_reach_the_host() {
    // Network schemes and shares are refused, naming scheme and host.
    let refused = |url: &str| local(&wide(url)).unwrap_err();
    assert_eq!(
        refused("https://cdn.example.com/a.mp4"),
        ("https".into(), "cdn.example.com".into())
    );
    assert_eq!(refused("RTSP://10.0.0.1:554/x").0, "rtsp");
    assert_eq!(refused("mms://h/x").0, "mms");
    assert_eq!(
        refused(r"\\server\share\a.mp4"),
        ("unc".into(), "server".into())
    );
    assert_eq!(
        refused("file://server/share/a.mp4"),
        ("file".into(), "server".into())
    );

    // A path on a virtual drive goes to its host folder, also as a
    // `file:` URL; other local files pass as they are.
    // Checked by opening the files: `files` may spell the host root
    // differently (absolute, `\\?\` for long paths) from the temp dir.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("movies")).unwrap();
    std::fs::write(dir.path().join("movies").join("intro.mp4"), b"1").unwrap();
    std::fs::write(dir.path().join("movies").join("intro a.mp4"), b"2").unwrap();
    let letter = files::mount(dir.path()).expect("a free drive letter");
    let host = text(&local(&wide(&format!(r"{letter}:\movies\intro.mp4"))).unwrap());
    assert_eq!(
        std::fs::read(&host).ok().as_deref(),
        Some(&b"1"[..]),
        "{host}"
    );
    let from_url =
        text(&local(&wide(&format!("file:///{letter}:/movies/intro%20a.mp4"))).unwrap());
    assert_eq!(
        std::fs::read(&from_url).ok().as_deref(),
        Some(&b"2"[..]),
        "{from_url}"
    );
    assert_eq!(
        text(&local(&wide("file:///C:/Windows/a.wav")).unwrap()),
        r"C:\Windows\a.wav"
    );
    assert_eq!(text(&local(&wide("clip.mp4")).unwrap()), "clip.mp4");
}

/// Other spellings of `\\server\share\a.mp4`, as paths and as `file:`
/// URLs without a host.
const SHARE_PATHS: [&str; 3] = [
    r"/\server\share\a.mp4",
    r"\/server/share/a.mp4",
    r"\??\UNC\server\share\a.mp4",
];
const SHARE_URLS: [&str; 5] = [
    "file:////server/share/a.mp4",
    "file://///server/share/a.mp4",
    r"file:\\server\share\a.mp4",
    "file:/%5Cserver/share/a.mp4",
    "file:///%5C%5Cserver/share/a.mp4",
];

#[test]
fn shares_are_refused_however_they_are_spelled() {
    let asked = |url: &str| local(&wide(url)).map(|v| text(&v));
    for path in SHARE_PATHS {
        assert_eq!(asked(path), Err(("unc".into(), "server".into())), "{path}");
    }
    for url in SHARE_URLS {
        assert_eq!(asked(url), Err(("file".into(), "server".into())), "{url}");
    }
    // Local files that look alike still pass: one leading separator, a
    // drive after the slashes, a drive after the object prefix, and the
    // path of a URL that names this machine.
    for local_file in [
        r"\movies\a.mp4",
        "file:///movies/a.mp4",
        r"\??\C:\Windows\a.wav",
        "file://localhost//server/share/a.mp4",
    ] {
        assert_eq!(asked(local_file), Ok(local_file.into()), "{local_file}");
    }
    assert_eq!(
        asked("file:////C:/Windows/a.wav"),
        Ok(r"C:\Windows\a.wav".into())
    );
}

/// What the host makes of those spellings: the share, for each.
#[cfg(windows)]
#[test]
fn the_host_reads_those_spellings_as_shares() {
    for path in &SHARE_PATHS[..2] {
        assert_eq!(
            std::path::absolute(path).unwrap().to_str(),
            Some(r"\\server\share\a.mp4"),
            "{path}"
        );
    }
    // The object prefix reaches the host's file calls as it is.
    let windows = std::env::var("SystemRoot").unwrap();
    assert!(std::fs::metadata(format!(r"\??\{windows}")).is_ok());

    let from_url = files::host!("shlwapi.dll", "PathCreateFromUrlW":
        extern "system" fn(*const u16, *mut u16, *mut u32, u32) -> HRESULT);
    let host_path = |url: &str| {
        let url: Vec<u16> = url.encode_utf16().chain([0]).collect();
        let (mut path, mut n) = ([0u16; 260], 260);
        assert_eq!(from_url(url.as_ptr(), path.as_mut_ptr(), &mut n, 0), S_OK);
        String::from_utf16_lossy(&path[..n as usize])
    };
    for url in SHARE_URLS {
        let path = host_path(url);
        assert!(
            path.starts_with(r"\\") && path.ends_with(r"server\share\a.mp4"),
            "{url}: {path}"
        );
    }
    // And of the ones that pass: local paths.
    assert_eq!(
        host_path("file://localhost//server/share/a.mp4"),
        r"\server\share\a.mp4"
    );
    assert_eq!(host_path("file:////C:/Windows/a.wav"), r"C:\Windows\a.wav");
    assert_eq!(host_path("file:///movies/a.mp4"), r"\movies\a.mp4");
}

#[test]
fn interface_ids_are_the_resolvers() {
    assert_eq!(&IID_IUNKNOWN[8..], &[0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
    assert_eq!(&IID_RESOLVER[..4], &[0x2D, 0xA3, 0xE5, 0xFB]);
}
