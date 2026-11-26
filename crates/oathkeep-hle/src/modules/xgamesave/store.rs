use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{MANIFEST, READABLE};

/// `name` as a file name: letters, digits, `-` and `_` stay, every other
/// byte becomes `%XX`. The result never contains a dot.
pub(super) fn file_name(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The name behind a file name from [`file_name`]; `None` for other files.
pub(super) fn decode(file: &str) -> Option<String> {
    if file.contains('.') {
        return None;
    }
    let mut out = Vec::new();
    let mut bytes = file.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

/// The container's folder: its name made safe, cut short, then `~` and a
/// hash of the exact name (64-bit FNV-1a, stable across builds).
pub(super) fn container_dir(root: &Path, name: &str) -> PathBuf {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let mut readable = file_name(name);
    readable.truncate(READABLE); // ASCII only
    root.join(format!("{readable}~{hash:016x}"))
}

/// Documented: the path part (up to the last `/`) letters, digits, `_` and
/// `/`; the file part letters, digits, `_`, `.` and `-`, not empty, not
/// ending in `.`, no `..`; 256 characters at most.
pub(super) fn valid_container(name: &str) -> bool {
    let (path, file) = name.rsplit_once('/').unwrap_or(("", name));
    name.len() <= 256
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
        && !file.is_empty()
        && !file.ends_with('.')
        && !file.contains("..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A committed version of a container.
pub(super) struct Version {
    pub(super) dir: PathBuf,
    /// The highest version number any entry in the container's folder
    /// carries, readable or not; the next commit goes above it.
    pub(super) highest: u64,
    pub(super) name: String,
    pub(super) display: String,
    /// Blob names; blob `i` is the file `i`.
    pub(super) blobs: Vec<String>,
}

impl Version {
    pub(super) fn blob(&self, i: usize) -> PathBuf {
        self.dir.join(i.to_string())
    }

    pub(super) fn sizes(&self) -> Vec<(String, u64)> {
        let size = |i| std::fs::metadata(self.blob(i)).map_or(0, |m| m.len());
        let blobs = self.blobs.iter().enumerate();
        blobs.map(|(i, n)| (n.clone(), size(i))).collect()
    }
}

/// The manifest: length-prefixed strings (u32 LE): container name, display
/// name, then each blob name.
fn manifest(name: &str, display: &str, blobs: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in [name, display].into_iter().chain(blobs.iter().copied()) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    out
}

fn parse_manifest(bytes: &[u8]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (len, tail) = rest.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*len) as usize;
        out.push(String::from_utf8(tail.get(..len)?.to_vec()).ok()?);
        rest = &tail[len..];
    }
    (out.len() >= 2).then_some(out)
}

/// The number of a version folder exactly as [`commit`] names it (`v1`,
/// `v2`, ...); `None` for anything else, such as `v01`, `v+1` or `v0`.
fn version_number(file: &str) -> Option<u64> {
    let n = file.strip_prefix('v')?.parse::<u64>().ok()?;
    (n > 0 && format!("v{n}") == file).then_some(n)
}

/// The names in version folder `dir`: container, display, blobs; `None` if
/// its manifest is missing or does not parse.
fn load(dir: &Path) -> std::io::Result<Option<(String, String, Vec<String>)>> {
    let bytes = match std::fs::read(dir.join(MANIFEST)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some(mut names) = parse_manifest(&bytes) else {
        return Ok(None);
    };
    let blobs = names.split_off(2);
    let [name, display] = <[String; 2]>::try_from(names).expect("two names, checked by the parser");
    Ok(Some((name, display, blobs)))
}

/// The container's newest readable committed version, if any. Removes what
/// a crash left: unfinished versions, and versions older than that one.
/// An unreadable newer folder (a stray, or a broken manifest) is neither
/// taken nor removed, and never costs the version below it.
pub(super) fn current(dir: &Path) -> std::io::Result<Option<Version>> {
    let mut committed = Vec::new();
    let mut highest = 0;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if file.ends_with(".new") {
            std::fs::remove_dir_all(entry.path())?;
        } else if let Some(n) = version_number(&file) {
            highest = highest.max(n);
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                committed.push(n);
            }
        }
    }
    committed.sort_unstable_by(|a, b| b.cmp(a));
    if committed.is_empty() {
        return Ok(None);
    }
    for (at, &number) in committed.iter().enumerate() {
        let version = dir.join(format!("v{number}"));
        let Some((name, display, blobs)) = load(&version)? else {
            continue;
        };
        for old in &committed[at + 1..] {
            std::fs::remove_dir_all(dir.join(format!("v{old}")))?;
        }
        return Ok(Some(Version {
            dir: version,
            highest,
            name,
            display,
            blobs,
        }));
    }
    // Versions exist but none reads: keep them all, and refuse.
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "no save version has a readable manifest",
    ))
}

/// The version of container `name` in `dir`; none for a missing container
/// or another name that hashes alike.
pub(super) fn current_named(dir: &Path, name: &str) -> std::io::Result<Option<Version>> {
    match current(dir) {
        Ok(v) => Ok(v.filter(|v| v.name == name)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Where a crash test stops the process (test builds only).
pub(super) fn step() {
    #[cfg(test)]
    super::tests::crash_point();
}

/// A blob of a new version: new data, or the previous version's file.
pub(super) enum Source<'a> {
    Data(&'a [u8]),
    Kept(PathBuf),
}

/// Writes version `number` of the container in `dir` and commits it.
pub(super) fn commit(
    dir: &Path,
    number: u64,
    name: &str,
    display: &str,
    blobs: &[(&str, Source<'_>)],
) -> std::io::Result<()> {
    let staged = dir.join(format!("v{number}.new"));
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    std::fs::create_dir_all(&staged)?;
    let write = |path: PathBuf, data: &[u8]| -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(data)?;
        file.sync_all()
    };
    for (i, (_, source)) in blobs.iter().enumerate() {
        let path = staged.join(i.to_string());
        match source {
            Source::Data(data) => write(path, data)?,
            // Files are never changed in place, so sharing one is safe.
            Source::Kept(from) => {
                std::fs::hard_link(from, &path).or_else(|_| write(path, &std::fs::read(from)?))?
            }
        }
        step();
    }
    let names: Vec<&str> = blobs.iter().map(|(n, _)| *n).collect();
    write(staged.join(MANIFEST), &manifest(name, display, &names))?;
    step();
    std::fs::rename(&staged, dir.join(format!("v{number}")))?;
    step();
    Ok(())
}

/// The containers under a provider's root, by exact name; containers in
/// the old one-file-per-blob layout are converted first.
pub(super) fn containers(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        let file = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() {
            continue;
        }
        if file.ends_with(".gone") {
            let _ = std::fs::remove_dir_all(&path);
        } else if file.contains('~') {
            if let Ok(Some(v)) = current(&path) {
                out.push((v.name, path));
            }
        } else if let Some(name) = decode(&file)
            && let Some(dir) = migrate(root, &name, &path)
        {
            out.push((name, dir));
        }
    }
    out.sort();
    out
}

/// Moves a container of the old layout (blob files named by encoded name,
/// a `.display` file) into a committed version, then removes it.
fn migrate(root: &Path, name: &str, old: &Path) -> Option<PathBuf> {
    let dir = container_dir(root, name);
    let done = current_named(&dir, name).ok()?.is_some(); // a crash after the commit
    if !done {
        let display = std::fs::read_to_string(old.join(".display")).unwrap_or_default();
        let mut blobs: Vec<(String, Vec<u8>)> = std::fs::read_dir(old)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let blob = decode(e.file_name().to_str()?)?;
                Some((blob, std::fs::read(e.path()).ok()?))
            })
            .collect();
        blobs.sort();
        let sources: Vec<_> = blobs
            .iter()
            .map(|(n, d)| (n.as_str(), Source::Data(d)))
            .collect();
        commit(&dir, 1, name, &display, &sources).ok()?;
    }
    std::fs::remove_dir_all(old).ok()?;
    tracing::info!(container = name, "game save moved to the versioned layout");
    Some(dir)
}

pub(super) fn used_bytes(root: &Path) -> u64 {
    containers(root)
        .iter()
        .filter_map(|(name, dir)| current_named(dir, name).ok().flatten())
        .map(|v| v.sizes().iter().map(|(_, size)| size).sum::<u64>())
        .sum()
}
