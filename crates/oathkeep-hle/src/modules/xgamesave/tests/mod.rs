use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::ops::{Provider, Update, delete, no_room, read};
use super::store::{container_dir, current_named};
use super::{OUT_OF_LOCAL_STORAGE, QUOTA, QUOTA_EXCEEDED};
use crate::modules::xasync::AsyncBlock;
use crate::modules::xgameruntime::{HRESULT, S_OK};

mod containers;

fn block() -> AsyncBlock {
    AsyncBlock {
        queue: std::ptr::null(),
        context: std::ptr::null_mut(),
        callback: None,
        internal: [0; 4],
    }
}

static STEPS: AtomicU32 = AtomicU32::new(0);

/// In a child process of the crash test, stops for good at the step
/// `OATHKEEP_SAVE_STOP` names and says so.
pub(in crate::modules::xgamesave) fn crash_point() {
    let Ok(stop) = std::env::var("OATHKEEP_SAVE_STOP") else {
        return;
    };
    if stop.parse() == Ok(STEPS.fetch_add(1, Ordering::SeqCst) + 1) {
        println!("STOPPED");
        let _ = std::io::stdout().flush();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
}

/// The per-user quota and the host disk fail with their own errors.
#[test]
fn a_full_disk_is_not_the_quota() {
    let mb = 1 << 20;
    assert_eq!(no_room(2 * mb, 0, 0, mb), Some(OUT_OF_LOCAL_STORAGE));
    assert_eq!(no_room(2 * mb, 0, QUOTA - mb, 100 * mb), Some(QUOTA_EXCEEDED));
    // Replacing a blob of the same size fits a full quota.
    assert_eq!(no_room(mb, mb, QUOTA, 100 * mb), None);
    assert_eq!(no_room(mb, 0, 0, mb), None);
}

fn update(root: &Path, display: &str, changes: &[(&str, Option<&str>)]) -> HRESULT {
    Update {
        name: "Slot".into(),
        dir: container_dir(root, "Slot"),
        display: Some(display.into()),
        changes: changes
            .iter()
            .map(|(n, d)| (n.to_string(), d.map(|d| d.as_bytes().to_vec())))
            .collect(),
    }
    .submit()
}

/// The container `Slot`: display name and blobs.
fn state(root: &Path) -> Option<(String, Vec<(String, String)>)> {
    let c = Provider { root: root.into() }.container("Slot");
    let display = current_named(&c.dir, "Slot").unwrap()?.display;
    let blobs = read(&c, None).unwrap();
    let text = |(n, d): (String, Vec<u8>)| (n, String::from_utf8(d).unwrap());
    Some((display, blobs.into_iter().map(text).collect()))
}

fn old_state() -> Option<(String, Vec<(String, String)>)> {
    Some(("one".into(), vec![("a".into(), "old a".into()), ("b".into(), "old b".into())]))
}

/// Run by the crash test in a child process; does nothing otherwise.
#[test]
fn crash_child() {
    let (Ok(root), Ok(op)) = (std::env::var("OATHKEEP_SAVE_CHILD"), std::env::var("OATHKEEP_SAVE_OP")) else {
        return;
    };
    let root = PathBuf::from(root);
    let hr = if op == "update" {
        update(&root, "two", &[("a", Some("new a")), ("b", None), ("c", Some("new c"))])
    } else {
        delete(&root, "Slot")
    };
    assert_eq!(hr, S_OK);
    println!("FINISHED");
}

/// Kills a process in the middle of an update (and of a deletion) at
/// every step: what is left is the whole old container or the whole
/// new one.
#[test]
fn a_kill_at_any_step_leaves_old_or_new() {
    let new = Some(("two".into(), vec![("a".into(), "new a".into()), ("c".into(), "new c".into())]));
    for (op, after) in [("update", new), ("delete", None)] {
        let mut seen = (0, 0);
        for stop in 1.. {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            assert_eq!(update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]), S_OK);
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "modules::xgamesave::tests::crash_child", "--nocapture"])
                .args(["--test-threads=1"])
                .env("OATHKEEP_SAVE_CHILD", root)
                .env("OATHKEEP_SAVE_OP", op)
                .env("OATHKEEP_SAVE_STOP", stop.to_string())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let out = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut finished = false;
            for line in out.lines() {
                let line = line.unwrap();
                if line.contains("STOPPED") {
                    break;
                }
                finished |= line.contains("FINISHED");
            }
            let _ = child.kill();
            child.wait().unwrap();
            let left = state(root);
            if left == old_state() {
                seen.0 += 1;
            } else {
                assert_eq!(left, after, "{op}, killed at step {stop}");
                seen.1 += 1;
            }
            if finished {
                break;
            }
        }
        assert!(seen.0 > 0 && seen.1 > 1, "{op}: {seen:?}");
    }
}

/// A kept blob that cannot be hard-linked (NTFS allows 1024 links to a
/// file) is copied into the new version with its exact bytes.
#[cfg(windows)]
#[test]
fn a_kept_blob_past_the_link_limit_is_copied() {
    let dir = tempfile::tempdir().unwrap();
    let links = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(
        update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]),
        S_OK
    );
    const NTFS: &str = "the temp folder must be on NTFS, which allows 1024 links to a file";
    let old_b = container_dir(root, "Slot").join("v1").join("1");
    for i in 0..1023 {
        std::fs::hard_link(&old_b, links.path().join(i.to_string())).expect(NTFS);
    }
    let full = std::fs::hard_link(&old_b, links.path().join("full")).expect_err(NTFS);
    assert_eq!(full.kind(), std::io::ErrorKind::TooManyLinks, "{NTFS}");

    assert_eq!(update(root, "two", &[("a", Some("new a"))]), S_OK);

    let new = (
        "two".into(),
        vec![("a".into(), "new a".into()), ("b".into(), "old b".into())],
    );
    assert_eq!(state(root), Some(new));
    assert!(!old_b.exists());
    assert_eq!(std::fs::read(links.path().join("0")).unwrap(), b"old b");
}
