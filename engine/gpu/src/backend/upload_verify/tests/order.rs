use super::*;

/// What a draw of `list` reads at `at`: comes to `out`.
fn read_out(list: &HostList, shadow: &std::sync::Arc<Shadow>, at: usize, n: usize, out: (&ID3D12Resource, u64)) {
    shadow.refresh(list, shadow.span().0 + at as u64, n as u64);
    // SAFETY: an open list; live buffers.
    unsafe {
        list.list.CopyBufferRegion(out.0, out.1, &shadow.buffer, at as u64, n as u64);
        list.list
            .ResourceBarrier(&[transition(&shadow.buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
    }
}

/// Copies, shader writes and reads in random order, within lists and
/// across submissions in the air, the title's CPU writing sources in
/// between: every read sees what stands before it, and in the end title
/// memory and the copies hold the last of everything.
fn in_order(resource: usize, sizes: [usize; 3]) {
    let Some(host) = host() else {
        return;
    };
    let buffers: Vec<_> = sizes.iter().enumerate().map(|(i, &n)| title_of(resource + i, i == 0, n)).collect();
    let slow = Slow::new(host);
    let (out, out_data) = host.shared_buffer(1 << 20).unwrap();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64 ^ resource as u64;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as usize % n
    };
    // Of each buffer: the first 4 KiB are the title's own (sources), the
    // next 12 are written by copies and shaders.
    const OWN: usize = 4096;
    const ALL: usize = 16384;
    // (Of the third only the first 4 KiB: shaders never write it.)
    let mut model = [vec![0u8; ALL], vec![0u8; ALL], vec![0u8; ALL]];
    let mut titles: Vec<&'static mut [u8]> = Vec::new();
    let mut made = Vec::new();
    for (title, start, shadow) in buffers {
        titles.push(title);
        made.push((start, shadow));
    }
    let mut lists = Vec::new();
    for (start, shadow) in &made {
        let all = host.new_list().unwrap();
        shadow.refresh(&all, *start as u64, ALL as u64);
        all.close();
        host.execute_own(&all);
        lists.push(all);
    }
    settle_now(WHY_SYNC);
    for round in 0..150 {
        // (offset in `out`, what was expected, where from)
        let mut expected: Vec<(usize, Vec<u8>, String)> = Vec::new();
        let mut told = String::new();
        for submission in 0..1 + next(5) {
            for _ in 0..next(6) {
                let (b, at) = (next(3), next(OWN / 64) * 64);
                let value = next(255) as u8 + 1;
                // (A copy with one side is what submissions in the air
                // read: the title's CPU waits for them, as it does.)
                if !made[b].1.carded() {
                    settle_now(WHY_SYNC);
                }
                titles[b][at..at + 64].fill(value);
                model[b][at..at + 64].fill(value);
            }
            let list = host.new_list().unwrap();
            if next(3) == 0 {
                slow.record(&list, 4 + next(12));
            }
            for op in 0..1 + next(10) {
                let word = |next: &mut dyn FnMut(usize) -> usize| OWN + next((ALL - OWN - 256) / 4) * 4;
                match next(5) {
                    0 | 1 => {
                        let (d, s) = (next(2), next(3));
                        let n = 4 * (1 + next(64));
                        let to = word(&mut next);
                        let from = if s == 2 || next(2) == 0 { next((OWN - 256) / 4) * 4 } else { word(&mut next) };
                        if d == s && from < to + n && to < from + n {
                            continue;
                        }
                        assert!(list.place_copy(made[d].0 + to, made[s].0 + from, n), "done in the list");
                        let bytes = model[s][from..from + n].to_vec();
                        model[d][to..to + n].copy_from_slice(&bytes);
                        told += &format!("\n  s{submission} op{op}: copy {d}+{to:#x} <- {s}+{from:#x} {n:#x}");
                    }
                    2 => {
                        let (b, at, n) = (next(2), word(&mut next), 4 * (1 + next(32)));
                        let value = next(255) as u8 + 1;
                        let (start, shadow) = &made[b];
                        shadow.refresh(&list, (*start + at) as u64, n as u64);
                        shadow.gpu_writes(&list, (*start + at) as u64, n as u64);
                        write(host, &list, shadow, at, &vec![value; n]);
                        model[b][at..at + n].fill(value);
                        told += &format!("\n  s{submission} op{op}: shader {b}+{at:#x} {n:#x} = {value:#x}");
                    }
                    _ => {
                        let (b, n) = (next(2), 256);
                        let at = if next(4) == 0 { next((OWN - 256) / 4) * 4 } else { word(&mut next) };
                        let place = expected.len() * 256;
                        read_out(&list, &made[b].1, at, n, (&out, place as u64));
                        expected.push((place, model[b][at..at + n].to_vec(), format!("s{submission} op{op}: read {b}+{at:#x}")));
                    }
                }
            }
            list.close();
            host.execute_own(&list);
            lists.push(list);
        }
        settle_now(WHY_SYNC);
        for (at, bytes, what) in expected {
            // SAFETY: inside the mapping; the copies into it have run.
            let got = unsafe { std::slice::from_raw_parts(out_data.add(at), bytes.len()) };
            if let Some(k) = (0..bytes.len()).find(|&k| got[k] != bytes[k]) {
                panic!("round {round}, {what}: at +{k:#x} {:#x}, not {:#x}{told}", got[k], bytes[k]);
            }
        }
        for (b, title) in titles.iter().enumerate() {
            if let Some(k) = (0..ALL).find(|&k| title[k] != model[b][k]) {
                panic!("round {round}: title memory {b}+{k:#x} is {:#x}, not {:#x}{told}", title[k], model[b][k]);
            }
        }
        // Brought up to date by the next, the copies hold it too.
        let list = host.new_list().unwrap();
        for (b, (_, shadow)) in made.iter().enumerate() {
            read_out(&list, shadow, 0, ALL, (&out, (b * ALL) as u64));
        }
        list.close();
        host.execute_own(&list);
        lists.push(list);
        settle_now(WHY_SYNC);
        for (b, model) in model.iter().enumerate() {
            // SAFETY: as above.
            let got = unsafe { std::slice::from_raw_parts(out_data.add(b * ALL), ALL) };
            if let Some(k) = (0..ALL).find(|&k| got[k] != model[k]) {
                panic!("round {round}: the copy {b}+{k:#x} is {:#x}, not {:#x}{told}", got[k], model[k]);
            }
        }
        if on() {
            assert_eq!(look(host), Some(0), "round {round}");
        }
    }
    eprintln!(
        "copies at their place, bytes, out of host copies, into textures: {:?}",
        super::inline_copies::STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect::<Vec<_>>()
    );
}

/// Two buffers with two sides.
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn copies_stand_where_the_list_has_them_on_the_card() {
    in_order(0x9100, [LEN, LEN, LEN]);
}

/// One in the card's memory, one in the PC's (one side without
/// `OATHKEEP_CARD_ALL`).
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn copies_stand_where_the_list_has_them_mixed() {
    in_order(0x9200, [LEN, 256 << 10, 256 << 10]);
}

#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn copies_stand_where_the_list_has_them_in_pc_memory() {
    in_order(0x9300, [256 << 10, 256 << 10, LEN]);
}

/// As the title records it: copy, draw, copy, draw in one list; a source
/// a shader writes first; a counter copied to zero and counted up.
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn copy_draw_copy_draw() {
    let Some(host) = host() else {
        return;
    };
    let (title, start, shadow) = title_of(0x9400, true, LEN);
    let (out, out_data) = host.shared_buffer(1 << 16).unwrap();
    let all = host.new_list().unwrap();
    shadow.refresh(&all, start as u64, LEN as u64);
    all.close();
    host.execute_own(&all);
    settle_now(WHY_SYNC);
    title[0..64].fill(1);
    title[64..128].fill(2);
    title[128..192].fill(0);
    let list = host.new_list().unwrap();
    const D: usize = 0x10000;
    const S: usize = 0x20000;
    assert!(list.place_copy(start + D, start, 64));
    read_out(&list, &shadow, D, 64, (&out, 0));
    assert!(list.place_copy(start + D, start + 64, 64));
    read_out(&list, &shadow, D, 64, (&out, 64));
    // A shader writes S, and it is copied on.
    shadow.gpu_writes(&list, (start + S) as u64, 64);
    write(host, &list, &shadow, S, &[7; 64]);
    assert!(list.place_copy(start + D + 64, start + S, 64));
    read_out(&list, &shadow, D + 64, 64, (&out, 128));
    // Zero, counted up, taken away.
    assert!(list.place_copy(start + D + 128, start + 128, 64));
    shadow.gpu_writes(&list, (start + D + 128) as u64, 64);
    write(host, &list, &shadow, D + 128, &[5; 64]);
    assert!(list.place_copy(start + D + 192, start + D + 128, 64));
    read_out(&list, &shadow, D + 192, 64, (&out, 192));
    // What a copy into a texture takes of S here.
    let between = list.place_source(start + S, 64).expect("shaders write it");
    shadow.gpu_writes(&list, (start + S) as u64, 64);
    write(host, &list, &shadow, S, &[9; 64]);
    // SAFETY: an open list; live buffers.
    unsafe { list.list.CopyBufferRegion(&out, 256, &between, 0, 64) };
    assert!(list.place_source(start + (1 << 20), 64).is_some(), "the buffer is written");
    // And zero for the next frame.
    assert!(list.place_copy(start + D + 128, start + 128, 64));
    list.close();
    host.execute_own(&list);
    settle_now(WHY_SYNC);
    // SAFETY: the mapping; the copies into it have run.
    let read = unsafe { std::slice::from_raw_parts(out_data, 320) };
    assert_eq!(read[256], 7, "what a texture takes");
    assert_eq!([read[0], read[64], read[128], read[192]], [1, 2, 7, 5], "what the draws read");
    assert_eq!(
        [title[D], title[D + 64], title[D + 128], title[D + 192], title[S]],
        [2, 7, 0, 5, 9],
        "title memory"
    );
    let holds = card(host, &shadow);
    assert_eq!([holds[D], holds[D + 64], holds[D + 128], holds[D + 192], holds[S]], [2, 7, 0, 5, 9], "the card");
    assert_eq!(look(host), Some(0));
    // With the debug layer.
    if let Some(said) = host.log_messages() {
        eprintln!("{said}");
    }
}

/// Declared writes beside the title's own, in the same 64 KiB pieces and
/// dword by dword, flights landing while later ones are in the air: in
/// the end title memory and the card hold what each wrote last. (A byte
/// is written by flights or by the title, not both at a time.)
#[test]
#[ignore = "wants OATHKEEP_RELAY=2 OATHKEEP_RELAY_COPY=1 and a process of its own"]
fn declared_writes_beside_the_titles_own_stay() {
    let Some(host) = host() else {
        return;
    };
    for view in [false, true] {
        beside(host, 0x5555 + usize::from(view), view);
    }
}

fn beside(host: &Host, resource: usize, view: bool) {
    let (title, start, shadow) = title(resource, view);
    let slow = Slow::new(host);
    // A small generator: the runs are the same each time.
    let mut seed = 0x1234_5678_9abc_def1u64;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as usize % n
    };
    // What memory should hold: pieces 0..8 are used, words of them by
    // turns the title's (even) and the shaders' (odd).
    const USED: usize = 8 << 16;
    let mut model = vec![0u8; USED];
    let mut lists = Vec::new();
    for round in 0..800usize {
        // The title writes some of its words, flights in the air or not.
        for _ in 0..next(24) {
            let at = next(USED / 8) * 8;
            let value = ((round as u32) << 8 | next(256) as u32).to_le_bytes();
            title[at..at + 4].copy_from_slice(&value);
            model[at..at + 4].copy_from_slice(&value);
        }
        // A submission: reads all or a part, writes a declared part.
        let list = host.new_list().unwrap();
        let (from, to) = match next(4) {
            0 => (0, LEN),
            1 => {
                let a = next(8) << 16;
                (a, a + (1 << 16))
            }
            _ => {
                let a = next(USED - 4096) & !255;
                (a, a + 256 * (1 + next(16)))
            }
        };
        shadow.refresh(&list, (start + from) as u64, (to - from) as u64);
        if next(5) > 0 {
            shadow.gpu_writes(&list, (start + from) as u64, (to - from) as u64);
            if next(3) == 0 {
                slow.record(&list, 1 + next(3));
            }
            for _ in 0..1 + next(6) {
                let words = (to.min(USED) - from) / 8;
                let at = from + next(words) * 8 + 4;
                // Now and then something in between first.
                if next(4) == 0 {
                    write(host, &list, &shadow, at, &[0xEE; 4]);
                    slow.record(&list, 1);
                }
                let value = if next(3) == 0 { [7, 7, 7, 7] } else { (0x8000_0000 | (round as u32) << 8 | next(256) as u32).to_le_bytes() };
                write(host, &list, &shadow, at, &value);
                model[at..at + 4].copy_from_slice(&value);
            }
        }
        list.close();
        host.execute_own(&list);
        lists.push(list);
        if round % 40 == 39 {
            // Everything refreshed, landed and looked at.
            let all = host.new_list().unwrap();
            shadow.refresh(&all, start as u64, LEN as u64);
            all.close();
            host.execute_own(&all);
            settle_now(WHY_SYNC);
            let all = host.new_list().unwrap();
            shadow.refresh(&all, start as u64, LEN as u64);
            all.close();
            host.execute_own(&all);
            settle_now(WHY_SYNC);
            lists.push(all);
            assert_eq!(look(host), Some(0), "round {round}: the card's side is the CPU's side");
            let holds = card(host, &shadow);
            let wrong = |held: &[u8]| (0..USED / 4).filter(|w| held[w * 4..w * 4 + 4] != model[w * 4..w * 4 + 4]).collect::<Vec<_>>();
            let (on_card, in_title) = (wrong(&holds), wrong(title));
            assert!(
                on_card.is_empty() && in_title.is_empty(),
                "round {round}: words {on_card:x?} on the card and {in_title:x?} in title memory are not what was written last"
            );
            assert_eq!(host.log_messages(), None);
        }
    }
}
