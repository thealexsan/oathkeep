use super::*;
pub(super) use super::rest::{look, run};

mod order;

use super::*;

#[test]
fn notes_are_cut_to_whole_words() {
    // Bytes 5..18 of a copy: the words at 8 and 12.
    let (was, new): (Vec<u8>, Vec<u8>) = ((5..18).collect(), (105..118).collect());
    note(u64::MAX, 5, &was, &new);
    let noted = NOTED.lock().unwrap_or_else(|e| e.into_inner()).remove(&u64::MAX).expect("noted");
    let [t] = &noted[..] else { panic!("one piece") };
    assert_eq!((t.at, &t.was[..], &t.new[..]), (8, &was[3..11], &new[3..11]));
}

/// The tests below want processes of their own with the relay on: each
/// `OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 OATHKEEP_UPCOPY_CHECK=1 cargo
/// test -p oathkeep-gpu --lib upcheck -- --ignored --test-threads=1
/// <name>`.
const LEN: usize = 4 << 20;

/// Title memory of our own (write-watched, or a view of a section, which
/// is guarded), and the card copy of the title buffer it is.
pub(super) fn title(resource: usize, view: bool) -> (&'static mut [u8], usize, std::sync::Arc<Shadow>) {
    let all = title_of(resource, view, LEN);
    assert!(all.2.carded(), "the copy is in the card's memory");
    all
}

pub(super) fn title_of(resource: usize, view: bool, len: usize) -> (&'static mut [u8], usize, std::sync::Arc<Shadow>) {
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_RESERVE, MapViewOfFile, PAGE_READWRITE, VirtualAlloc,
    };
    const WRITE_WATCH: u32 = 0x20_0000;
    // SAFETY: memory of our own, never freed.
    let base = unsafe {
        if view {
            let section = CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, len as u32, std::ptr::null());
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value.cast::<u8>()
        } else {
            VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT | WRITE_WATCH, PAGE_READWRITE).cast::<u8>()
        }
    };
    assert!(!base.is_null());
    crate::memory::shadow::register(resource, base as u64, len as u64);
    let shadow = crate::memory::shadow::find(base as u64, 64).expect("a copy");
    assert!(super::super::relay_exact() && super::super::relay_copy(), "set OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1");
    // SAFETY: `len` bytes just committed.
    (unsafe { std::slice::from_raw_parts_mut(base, len) }, base as usize, shadow)
}

/// Work for the card that takes some milliseconds.
pub(super) struct Slow(ID3D12Resource, ID3D12Resource);

impl Slow {
    pub(super) fn new(host: &Host) -> Slow {
        Slow(host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap())
    }

    pub(super) fn record(&self, list: &HostList, times: usize) {
        // SAFETY: an open list; live buffers.
        unsafe {
            for _ in 0..times {
                list.list.CopyBufferRegion(&self.1, 0, &self.0, 0, 64 << 20);
                list.list.ResourceBarrier(&[
                    transition(&self.1, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON),
                ]);
            }
        }
    }
}

/// A shader's stand-in: `bytes` go to offset `at` of the copy.
pub(super) fn write(host: &Host, list: &HostList, shadow: &Shadow, at: usize, bytes: &[u8]) {
    let (upload, data) = host.upload_buffer(bytes.len() as u64).unwrap();
    // SAFETY: the mapping; an open list and live buffers.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        list.list.CopyBufferRegion(&shadow.buffer, at as u64, &upload, 0, bytes.len() as u64);
        list.list
            .ResourceBarrier(&[transition(&shadow.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
    }
    list.keep(&upload);
}

/// What the card's side holds, nothing being in the air.
pub(super) fn card(host: &Host, shadow: &Shadow) -> Vec<u8> {
    let (held, data) = host.shared_buffer(LEN as u64).unwrap();
    // SAFETY: an open list; live buffers.
    run(host, |list| unsafe { list.CopyBufferRegion(&held, 0, &shadow.buffer, 0, LEN as u64) }).unwrap();
    // SAFETY: the mapping; the copy has run.
    unsafe { std::slice::from_raw_parts(data, LEN) }.to_vec()
}

/// (e) of the list in `Host::relay_card_back`'s fetch: a flight's result
/// is fetched while the next flight is at the same bytes, which ends with
/// what it found. Without `OATHKEEP_FETCH_CHECK` title memory keeps what
/// was in between, and the next refresh of the 64 KiB sends it up.
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn a_result_caught_in_between_is_put_right() {
    let Some(host) = host() else {
        return;
    };
    let (title, start, shadow) = title(0x3333, false);
    let slow = Slow::new(host);
    let (mut wrong, mut undone) = (0, 0);
    let mut lists = Vec::new();
    const ROUNDS: usize = 12;
    for round in 0..ROUNDS {
        let result: Vec<u8> = (0..256).map(|i| (round * 17 + i + 1) as u8).collect();
        let between = vec![0u8; 256];
        let a = host.new_list().unwrap();
        shadow.refresh(&a, start as u64, LEN as u64);
        shadow.gpu_writes(&a, start as u64, LEN as u64);
        write(host, &a, &shadow, 1024, &result);
        a.close();
        let b = host.new_list().unwrap();
        shadow.gpu_writes(&b, start as u64, LEN as u64);
        write(host, &b, &shadow, 1024, &between);
        slow.record(&b, 60);
        write(host, &b, &shadow, 1024, &result);
        b.close();
        host.execute_own(&a);
        host.execute_own(&b);
        settle_now(WHY_SYNC);
        wrong += usize::from(title[1024..1280] != result[..]);
        // The title writes beside it: the refresh takes the 64 KiB.
        title[4096] = round as u8 + 1;
        let c = host.new_list().unwrap();
        shadow.refresh(&c, start as u64, LEN as u64);
        c.close();
        host.execute_own(&c);
        settle_now(WHY_SYNC);
        let holds = card(host, &shadow);
        assert_eq!(holds[4096], round as u8 + 1, "the title's write is on the card");
        undone += usize::from(holds[1024..1280] != result[..]);
        lists.extend([a, b, c]);
    }
    eprintln!(
        "{wrong} of {ROUNDS} results wrong in title memory, {undone} undone on the card; pieces checked, suspect, put right: {:?}",
        super::torn_read_check::STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect::<Vec<_>>()
    );
    assert_eq!(look(host), Some(0));
    assert_eq!((wrong, undone), (0, 0), "without OATHKEEP_FETCH_CHECK=1 most are wrong");
}

/// What nobody declared: a shader writes a piece through a view that was
/// not known. The title writes another byte of the piece: it goes up
/// whole, with title memory's older bytes, and the check says so.
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 OATHKEEP_UPCOPY_CHECK=1 and a process of its own"]
fn the_check_finds_an_undeclared_write_undone() {
    let Some(host) = host() else {
        return;
    };
    assert!(on(), "set OATHKEEP_UPCOPY_CHECK=1");
    let (title, start, shadow) = title(0x4444, false);
    let a = host.new_list().unwrap();
    shadow.refresh(&a, start as u64, LEN as u64);
    shadow.gpu_writes(&a, start as u64, 1 << 16);
    write(host, &a, &shadow, (5 << 16) + 512, &[0x77; 64]);
    a.close();
    host.execute_own(&a);
    settle_now(WHY_SYNC);
    assert_eq!(card(host, &shadow)[(5 << 16) + 512], 0x77);
    assert!(title[(5 << 16) + 512] == 0, "it never comes back");
    assert_eq!(look(host), Some(16), "the look at rest finds it");
    title[(5 << 16) + 9000] = 1;
    let b = host.new_list().unwrap();
    shadow.refresh(&b, start as u64, LEN as u64);
    b.close();
    host.execute_own(&b);
    settle_now(WHY_SYNC);
    let undone = card(host, &shadow)[(5 << 16) + 512] == 0;
    let stale = STALE.lock().unwrap_or_else(|e| e.into_inner());
    let found = stale.get(&shadow.span()).map_or(0, |s| s.words);
    eprintln!("undone on the card: {undone}; the check found {found} words");
    assert_eq!((undone, found), (true, 16));
}

/// A shader that writes through views it picks: its buffer is found when
/// it comes to it in turn, and what it writes from then on is landed.
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 OATHKEEP_PICKED_WRITES=1 OATHKEEP_PICKED_MB=1 and a process of its own"]
fn writes_through_picked_views_come_back() {
    let Some(host) = host() else {
        return;
    };
    assert!(crate::diagnostics::picked_writes::on(), "set OATHKEEP_PICKED_WRITES=1");
    let (title, start, shadow) = title(0x6666, true);
    crate::diagnostics::picked_writes::shown(vec![(shadow.clone(), start as u64, (start + LEN) as u64)], true);
    let at = (37 << 16) + 512;
    let mut lists = Vec::new();
    let mut back = Vec::new();
    for round in 0..12u8 {
        // Turns go by frames.
        crate::d3d12::frame::test_advance();
        title[(37 << 16) + 9000] = round + 1;
        let list = host.new_list().unwrap();
        shadow.refresh(&list, start as u64, LEN as u64);
        crate::diagnostics::picked_writes::runs_in(&list);
        write(host, &list, &shadow, at, &[0x40 + round; 64]);
        list.close();
        host.execute_own(&list);
        settle_now(WHY_SYNC);
        back.push(title[at..at + 64] == [0x40 + round; 64]);
        // The title's own write beside it is on the card, and the
        // shader's stays, once its buffer is known.
        let next = host.new_list().unwrap();
        shadow.refresh(&next, start as u64, LEN as u64);
        next.close();
        host.execute_own(&next);
        settle_now(WHY_SYNC);
        if *back.last().unwrap() {
            let holds = card(host, &shadow);
            assert_eq!((holds[(37 << 16) + 9000], holds[at]), (round + 1, 0x40 + round));
            assert_eq!(look(host), Some(0));
        }
        lists.extend([list, next]);
    }
    eprintln!("came back, by round: {back:?}");
    let first = back.iter().position(|b| *b).expect("it is found");
    assert!(first <= 6 && back[first..].iter().all(|b| *b), "{back:?}");
}

/// The title copies another value to the same bytes in each of three
/// submissions, and each has a draw (a copy here) that reads them. The
/// card is busy meanwhile, so all three are prepared before the first
/// runs: each must read its own.
fn each_reads_its_own(len: usize, resource: usize, carded: bool) {
    let Some(host) = host() else {
        return;
    };
    let (title, start, shadow) = title_of(resource, true, len);
    assert_eq!(shadow.carded(), carded, "where the copy is");
    let slow = Slow::new(host);
    let (out, out_data) = host.shared_buffer(1 << 16).unwrap();
    let mut lists = Vec::new();
    const AT: usize = 8192;
    // All there once.
    let all = host.new_list().unwrap();
    shadow.refresh(&all, start as u64, len as u64);
    all.close();
    host.execute_own(&all);
    settle_now(WHY_SYNC);
    lists.push(all);
    let mut wrong = Vec::new();
    for round in 0..6u8 {
        for v in 0..3u8 {
            title[65_536 + usize::from(v) * 256..][..256].fill(round * 16 + v + 1);
        }
        let busy = host.new_list().unwrap();
        slow.record(&busy, 40);
        busy.close();
        host.execute_own(&busy);
        lists.push(busy);
        for v in 0..3usize {
            let list = host.new_list().unwrap();
            list.defer_copy((start + AT) as *mut u8, (start + 65_536 + v * 256) as *const u8, 256);
            shadow.refresh(&list, (start + AT) as u64, 256);
            // SAFETY: an open list; live buffers.
            unsafe {
                list.list.CopyBufferRegion(&out, (v * 256) as u64, &shadow.buffer, AT as u64, 256);
            }
            list.close();
            host.execute_own(&list);
            lists.push(list);
        }
        settle_now(WHY_SYNC);
        // SAFETY: a mapping of 64 KiB; the copies into it have run.
        let read = unsafe { std::slice::from_raw_parts(out_data, 768) };
        for v in 0..3u8 {
            let got = read[usize::from(v) * 256];
            if got != round * 16 + v + 1 {
                wrong.push((round, v, got - round * 16));
            }
        }
    }
    eprintln!("read another submission's value (round, submission, whose + 1): {wrong:?}");
    let stale = STALE.lock().unwrap_or_else(|e| e.into_inner());
    let found = stale.get(&shadow.span()).map_or(0, |s| s.words);
    eprintln!("the check at sends found {found} words");
    assert!(wrong.is_empty());
    assert_eq!(found, 0);
    // With the debug layer (`OATHKEEP_D3D12_DEBUG=1`, diagnostics).
    assert_eq!(host.log_messages(), None);
}

#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn each_submission_reads_its_own_copy_on_the_card() {
    each_reads_its_own(LEN, 0x7777, true);
}

#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn each_submission_reads_its_own_copy_in_pc_memory() {
    each_reads_its_own(256 << 10, 0x8888, std::env::var_os("OATHKEEP_CARD_ALL").is_some_and(|v| v == "1"));
}
