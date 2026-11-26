mod decompress;
mod io;
mod queue;

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use oathkeep_formats::bcpack;

use crate::modules::xgameruntime::{Guid, HRESULT, Object, S_OK, method};

use super::execute::use_bcpack_tables;
use super::factory::{DStorageGetFactory, QueueDesc};
use super::scheduler::{Command, Lanes, Read};
use super::trace::Note;
use super::{FACTORY, Out, Request};

pub(super) fn call<F: Copy>(object: *mut c_void, slot: usize) -> F {
    // SAFETY: every object here starts with the runtime header.
    method(unsafe { &*object.cast::<Object>() }, slot)
}

pub(super) fn factory() -> *mut c_void {
    let mut factory = std::ptr::null_mut();
    assert_eq!(DStorageGetFactory(&FACTORY, &mut factory), S_OK);
    factory
}

pub(super) fn open(path: &std::path::Path) -> *mut c_void {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let open: extern "system" fn(*mut c_void, *const u16, *const Guid, Out) -> HRESULT =
        call(factory(), 0x20);
    let mut file = std::ptr::null_mut();
    assert_eq!(
        open(factory(), wide.as_ptr(), std::ptr::null(), &mut file),
        S_OK
    );
    file
}

pub(super) fn status_array(capacity: u32) -> *mut c_void {
    let create: extern "system" fn(
        *mut c_void,
        u32,
        *const c_char,
        *const Guid,
        Out,
    ) -> HRESULT = call(factory(), 0x28);
    let mut array = std::ptr::null_mut();
    let name = c"test".as_ptr();
    assert_eq!(
        create(factory(), capacity, name, std::ptr::null(), &mut array),
        S_OK
    );
    array
}

pub(super) fn new_queue(source_type: u64) -> *mut c_void {
    let create: extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, Out) -> HRESULT =
        call(factory(), 0x50);
    let desc = QueueDesc {
        source_type,
        capacity: 0x80,
        priority: 0,
        name: c"test".as_ptr(),
        unknown: 1,
    };
    let mut queue = std::ptr::null_mut();
    assert_eq!(create(factory(), &desc, std::ptr::null(), &mut queue), S_OK);
    queue
}

pub(super) fn request(
    options: u64,
    destination: *mut c_void,
    destination_size: usize,
    source: *const c_void,
    offset: u64,
    source_size: usize,
) -> Request {
    Request {
        options,
        destination,
        page_offset: 0,
        destination_size: destination_size as u32,
        source,
        offset,
        source_size: source_size as u32,
        intermediate_size: 0,
        tag: 0,
        name: std::ptr::null(),
    }
}

pub(super) fn enqueue(queue: *mut c_void, request: &Request) {
    let f: extern "system" fn(*mut c_void, *const Request) = call(queue, 0x18);
    f(queue, request);
}

pub(super) fn enqueue_status(queue: *mut c_void, array: *mut c_void, index: u32) {
    let f: extern "system" fn(*mut c_void, *mut c_void, u32) = call(queue, 0x20);
    f(queue, array, index);
}

pub(super) fn void(object: *mut c_void, slot: usize) {
    let f: extern "system" fn(*mut c_void) = call(object, slot);
    f(object);
}

pub(super) fn result(array: *mut c_void, index: u32) -> HRESULT {
    let f: extern "system" fn(*mut c_void, u32) -> HRESULT = call(array, 0x20);
    f(array, index)
}

/// Polls as the title does, then returns the entry's result.
pub(super) fn wait(array: *mut c_void, index: u32) -> HRESULT {
    let done: extern "system" fn(*mut c_void, u32) -> bool = call(array, 0x18);
    let start = Instant::now();
    while !done(array, index) {
        assert!(start.elapsed() < Duration::from_secs(10), "never completed");
        std::thread::sleep(Duration::from_millis(1));
    }
    result(array, index)
}

pub(super) fn file_with(bytes: &[u8]) -> (tempfile::TempDir, *mut c_void) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.bin");
    std::fs::write(&path, bytes).unwrap();
    let file = open(&path);
    (dir, file)
}

pub(super) fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

pub(super) fn compress_zlib(plain: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; zlib_rs::compress_bound(plain.len())];
    let (packed, rc) =
        zlib_rs::compress_slice(&mut buf, plain, zlib_rs::DeflateConfig::new(6));
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    packed.to_vec()
}

/// Lanes without workers: what `start` picks, and when statuses land.
pub(super) fn lanes(staging: u64) -> Lanes {
    Lanes {
        lanes: HashMap::new(),
        next_id: 0,
        next_seq: 0,
        staged: 0,
        staging,
        starved: false,
        clock: [0; 3],
        now: 0,
        lane_now: [0; 3],
        done: Default::default(),
        refused: 0,
        bcpack: Default::default(),
        since: None,
    }
}

pub(super) fn read(lanes: &mut Lanes, id: u64, options: u64, size: u32) {
    let mut r = request(options, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0);
    (r.destination_size, r.source_size) = (size, size);
    let lane = lanes.lanes.get_mut(&id).unwrap();
    let note = Note::default();
    lane.unsubmitted.push(Command::Read(Read { request: r, note }));
}

/// BCPack tables we made (uniform models; BC4 index pairs: low 3 bits,
/// next 3 bits), in a folder kept for the test process.
pub(super) fn with_test_tables() {
    static DIR: OnceLock<()> = OnceLock::new();
    DIR.get_or_init(|| {
        let mut t = Vec::new();
        for m in 0..128 {
            let g = m / 16;
            for b in 0..9 {
                let f: u16 = if b < g + 2 { 1 << (15 - g) } else { 0 };
                t.extend_from_slice(&f.to_le_bytes());
            }
        }
        t.extend([0; 256 + 64 + 256]);
        t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4));
        t.resize(bcpack::TABLES_SIZE, 0);
        let dir = tempfile::tempdir().unwrap().keep();
        std::fs::write(dir.join("tables.bin"), t).unwrap();
        use_bcpack_tables(dir);
    });
}

/// Two BC4 blocks from the format's test encoder: endpoints 10, -3 with
/// indices p -> p & 7, then the same block plus 1 everywhere.
pub(super) const BC4_STREAM: [u8; 88] = [
    0, 0, 0, 0, 0, 0, 0, 0, 5, 5, 5, 5, 4, 4, 4, 4, 1, 1, 1, 1, 0x10, 0x10, 0x10, 0x10, 2, 1,
    0, 0, 0, 0, 0, 0, 0, 0, 0x14, 5, 0x20, 0x68, 0xb0, 0xf8, 3, 3, 2, 2, 0x24, 0x24, 0x24,
    0x24, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 4, 4, 0x10, 0x10, 0x10, 0x10, 0, 0, 0, 0,
    0x20, 0x68, 0xb0, 0xf8, 0x24, 0x24, 0x24, 0x24, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// What `BC4_STREAM` unpacks to.
pub(super) fn bc4_blocks() -> [u8; 16] {
    let code = [0u64, 2, 3, 4, 5, 6, 7, 1];
    let block = |e0: u64, e1: u64, add: usize| {
        let bits = (0..16).fold(0, |a, p| a | code[(p + add) & 7] << (16 + 3 * p));
        (e0 | e1 << 8 | bits).to_le_bytes()
    };
    let mut out = [0; 16];
    out[..8].copy_from_slice(&block(138, 125, 0));
    out[8..].copy_from_slice(&block(139, 126, 1));
    out
}
