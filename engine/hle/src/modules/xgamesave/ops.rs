use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};

use super::super::xgameruntime::{HRESULT, Object, S_OK, text};
use super::super::{storage, xuser};
use super::store::{
    Source, commit, container_dir, containers, current, current_named, file_name, step, used_bytes,
};
use super::{
    BLOB_NOT_FOUND, MANIFEST, MAX_UPDATE, NO_ACCESS, OUT_OF_LOCAL_STORAGE, QUOTA, QUOTA_EXCEEDED,
    UPDATE_TOO_BIG, saves_lock,
};

pub(super) struct Provider {
    pub(super) root: PathBuf,
}
pub(super) struct Container {
    pub(super) name: String,
    pub(super) dir: PathBuf,
}
pub(super) struct Update {
    pub(super) name: String,
    pub(super) dir: PathBuf,
    pub(super) display: Option<String>,
    /// Blob writes (`Some`) and deletions (`None`), in order.
    pub(super) changes: Vec<(String, Option<Vec<u8>>)>,
}

/// A handle we gave out.
pub(super) fn get<'a, T>(handle: *const T) -> Option<&'a T> {
    // SAFETY: handles are boxes we created and the title has not closed.
    unsafe { handle.as_ref() }
}
pub(super) fn give<T>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}
pub(super) fn take<T>(handle: *mut T) {
    if !handle.is_null() {
        // SAFETY: as in `get`; closing ends the handle.
        drop(unsafe { Box::from_raw(handle) });
    }
}

impl Provider {
    /// The provider for the only user and `config` (the service
    /// configuration id), under `saves`. An empty id names no
    /// configuration, like none: as a folder name it would be the user's
    /// folder itself, where the other configurations' folders are.
    pub(super) fn open(saves: &Path, config: Option<&str>) -> Result<Provider, HRESULT> {
        let config = config.filter(|c| !c.is_empty()).unwrap_or("default");
        let root = saves
            .join(format!("{:016X}", xuser::XUID))
            .join(file_name(config));
        std::fs::create_dir_all(&root).map_err(|_| NO_ACCESS)?;
        let _saves = saves_lock();
        containers(&root); // converts old containers now
        Ok(Provider { root })
    }

    pub(super) fn remaining(&self) -> u64 {
        let _saves = saves_lock();
        remaining(&self.root)
    }

    pub(super) fn container(&self, name: &str) -> Container {
        Container {
            name: name.to_owned(),
            dir: container_dir(&self.root, name),
        }
    }
}

/// Quota left under a provider's root, or less if the host disk is fuller.
fn remaining(root: &Path) -> u64 {
    QUOTA
        .saturating_sub(used_bytes(root))
        .min(storage::disk_free(root))
}

/// Why an update writing `written` bytes, `replaced` of them over blobs it
/// replaces, does not fit: past the per-user quota (`used` bytes kept), or
/// past the host disk's `free` bytes, which is not the user's quota (the
/// new version is written before the old one goes, so nothing it replaces
/// counts there).
pub(super) fn no_room(written: u64, replaced: u64, used: u64, free: u64) -> Option<HRESULT> {
    if written > QUOTA.saturating_sub(used) + replaced {
        Some(QUOTA_EXCEEDED)
    } else if written > free {
        Some(OUT_OF_LOCAL_STORAGE)
    } else {
        None
    }
}

fn io_error(dir: &Path, e: &std::io::Error) -> HRESULT {
    tracing::warn!(dir = %dir.display(), %e, "game save");
    if e.kind() == std::io::ErrorKind::StorageFull {
        OUT_OF_LOCAL_STORAGE
    } else {
        NO_ACCESS
    }
}

impl Update {
    /// Applies the update as one new version of the container.
    pub(super) fn submit(&self) -> HRESULT {
        let _saves = saves_lock();
        let current = match current(&self.dir) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return io_error(&self.dir, &e),
        };
        if current.as_ref().is_some_and(|v| v.name != self.name) {
            // Another container's name hashes alike: never mix the two.
            return NO_ACCESS;
        }
        // The blobs after the update: name → new data or the kept file.
        let mut blobs: Vec<(&str, Source<'_>)> = current
            .iter()
            .flat_map(|v| {
                let kept = v.blobs.iter().enumerate();
                kept.map(|(i, n)| (n.as_str(), Source::Kept(v.blob(i))))
            })
            .collect();
        for (name, data) in &self.changes {
            blobs.retain(|(n, _)| n != name);
            if let Some(data) = data {
                blobs.push((name, Source::Data(data)));
            }
        }
        blobs.sort_by(|a, b| a.0.cmp(b.0));
        let written: usize = blobs
            .iter()
            .map(|(_, s)| match s {
                Source::Data(d) => d.len(),
                Source::Kept(_) => 0,
            })
            .sum();
        if written > MAX_UPDATE {
            return UPDATE_TOO_BIG;
        }
        let replaced: u64 = current.as_ref().map_or(0, |v| {
            let sizes = v.sizes();
            let changed = sizes.iter().filter(|(n, _)| self.changes.iter().any(|(c, _)| c == n));
            changed.map(|(_, size)| size).sum()
        });
        let root = self.dir.parent().unwrap_or(&self.dir);
        let free = storage::disk_free(root);
        if let Some(hr) = no_room(written as u64, replaced, used_bytes(root), free) {
            return hr;
        }
        let display = self.display.as_deref();
        let display = display.or(current.as_ref().map(|v| v.display.as_str()));
        let number = current.as_ref().map_or(1, |v| v.highest + 1);
        if let Err(e) = commit(&self.dir, number, &self.name, display.unwrap_or(""), &blobs) {
            return io_error(&self.dir, &e);
        }
        // The old version is garbage now; a crash here leaves it for later.
        if let Some(old) = &current {
            let _ = std::fs::remove_dir_all(&old.dir);
            step();
        }
        S_OK
    }
}

/// Deletes a container: renamed away (one step), then removed.
pub(super) fn delete(root: &Path, name: &str) -> HRESULT {
    let _saves = saves_lock();
    let dir = container_dir(root, name);
    match current_named(&dir, name) {
        Ok(Some(_)) => {}
        Ok(None) => return S_OK,
        Err(e) => return io_error(&dir, &e),
    }
    let gone = dir.with_extension("gone");
    let _ = std::fs::remove_dir_all(&gone);
    step();
    if let Err(e) = std::fs::rename(&dir, &gone) {
        return io_error(&dir, &e);
    }
    step();
    let _ = std::fs::remove_dir_all(&gone);
    S_OK
}

/// Reads the named blobs (all of them for `None`) of a container.
pub(super) fn read(
    c: &Container,
    names: Option<Vec<String>>,
) -> Result<Vec<(String, Vec<u8>)>, HRESULT> {
    let _saves = saves_lock();
    let version = current_named(&c.dir, &c.name)
        .map_err(|e| io_error(&c.dir, &e))?
        .ok_or(BLOB_NOT_FOUND)?;
    let names = names.unwrap_or_else(|| version.blobs.clone());
    names
        .into_iter()
        .map(|name| {
            let i = version.blobs.iter().position(|b| *b == name).ok_or(BLOB_NOT_FOUND)?;
            let data = std::fs::read(version.blob(i)).map_err(|e| io_error(&c.dir, &e))?;
            Ok((name, data))
        })
        .collect()
}

/// One blob as the public API lays it out: name, size, data.
pub(super) const BLOB: usize = 24;

/// Blobs laid out as the title reads them: the blob array, then each name
/// (NUL-terminated) and its data, with pointers `base` plus offset.
pub(super) fn layout(blobs: &[(String, Vec<u8>)], base: usize) -> Vec<u8> {
    let mut out = vec![0u8; blobs.len() * BLOB];
    for (i, (name, data)) in blobs.iter().enumerate() {
        let name_at = base + out.len();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        let data_at = base + out.len();
        out.extend_from_slice(data);
        let entry = &mut out[i * BLOB..(i + 1) * BLOB];
        entry[..8].copy_from_slice(&name_at.to_le_bytes());
        entry[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        entry[16..].copy_from_slice(&data_at.to_le_bytes());
    }
    out
}

/// The names a title passes (`count` strings, or all blobs if null).
pub(super) fn names(names: *const *const c_char, count: u32) -> Option<Vec<String>> {
    if names.is_null() {
        return None;
    }
    // SAFETY: the title passes `count` strings.
    let names = unsafe { std::slice::from_raw_parts(names, count as usize) };
    Some(names.iter().map(|&n| text(n).unwrap_or_default()).collect())
}

pub(super) type This = *const Object;
pub(super) type ContainerCallback = extern "system" fn(*const ContainerInfo, *mut c_void) -> bool;
pub(super) type BlobCallback = extern "system" fn(*const BlobInfo, *mut c_void) -> bool;

#[repr(C)]
pub struct ContainerInfo {
    pub name: *const c_char,
    pub display_name: *const c_char,
    pub blob_count: u32,
    pub total_size: u64,
    pub last_modified: i64,
    pub needs_sync: bool,
}

#[repr(C)]
pub struct BlobInfo {
    pub name: *const c_char,
    pub size: u32,
}

/// Calls `callback` with the information of each container that passes
/// `keep` until it returns false; never under our lock (it may call us).
pub(super) fn report(
    root: &Path,
    keep: impl Fn(&str) -> bool,
    context: *mut c_void,
    callback: ContainerCallback,
) {
    let infos: Vec<(CString, CString, u32, u64, i64)> = {
        let _saves = saves_lock();
        containers(root)
            .into_iter()
            .filter(|(name, _)| keep(name))
            .filter_map(|(name, dir)| current_named(&dir, &name).ok().flatten())
            .map(|v| {
                let modified = std::fs::metadata(v.dir.join(MANIFEST))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                let size = v.sizes().iter().map(|(_, s)| s).sum();
                let count = v.blobs.len() as u32;
                (cstring(&v.name), cstring(&v.display), count, size, modified)
            })
            .collect()
    };
    for (name, display, blob_count, total_size, last_modified) in &infos {
        let info = ContainerInfo {
            name: name.as_ptr(),
            display_name: display.as_ptr(),
            blob_count: *blob_count,
            total_size: *total_size,
            last_modified: *last_modified,
            needs_sync: false,
        };
        if !callback(&info, context) {
            break;
        }
    }
}

pub(super) fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}
