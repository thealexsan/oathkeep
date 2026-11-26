use super::super::{title_memory_reading, title_memory_remapping};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// A change of mapping waits for a read in place that is under way, and
/// a read in place waits for a change of mapping.
#[test]
fn reads_in_place_and_remappings_wait_for_each_other() {
    // A reader holds; the change does not begin until it lets go.
    let changed = Arc::new(AtomicBool::new(false));
    let reading = title_memory_reading();
    let (began, wait) = mpsc::channel();
    let change = {
        let changed = changed.clone();
        std::thread::spawn(move || {
            began.send(()).unwrap();
            let _changing = title_memory_remapping();
            changed.store(true, Ordering::SeqCst);
        })
    };
    wait.recv().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!changed.load(Ordering::SeqCst), "the mapping changed under a read in place");
    drop(reading);
    change.join().unwrap();
    assert!(changed.load(Ordering::SeqCst));
    // A change holds; the read does not begin until it is done.
    let read = Arc::new(AtomicBool::new(false));
    let changing = title_memory_remapping();
    let (began, wait) = mpsc::channel();
    let reader = {
        let read = read.clone();
        std::thread::spawn(move || {
            began.send(()).unwrap();
            let _reading = title_memory_reading();
            read.store(true, Ordering::SeqCst);
        })
    };
    wait.recv().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!read.load(Ordering::SeqCst), "a read in place began during a change of mapping");
    drop(changing);
    reader.join().unwrap();
    assert!(read.load(Ordering::SeqCst));
}
