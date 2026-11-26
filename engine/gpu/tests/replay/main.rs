//! The frame recorder and replayer on the PC's own adapter (skipped without
//! one): a small program with clears, indirect draws of textures picked from
//! a heap, compute, buffers the CPU changes between submissions, descriptors
//! replaced between them, and textures that share memory. It runs without
//! the recorder, then with it, then one frame of it is replayed; every
//! picture and buffer must be the same.
//!
//! `OATHKEEP_TEST_D3D_DEBUG=1` runs it under the debug layer.
#![cfg(all(windows, feature = "diagnostics"))]

use oathkeep_gpu::replay::play::{self, Buffers, Options, Player};
use oathkeep_gpu::replay::record::{self, At, Settings};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

mod d3d;
mod scene_frame;
mod scene_init;
mod scene_ops;
mod shaders;

use d3d::{rows, table_of};
use scene_frame::Outcome;
use scene_init::Scene;

fn check(player: &mut Player, options: &Options, expected: &Outcome, what: &str) -> play::Report {
    let report = player.run(options).unwrap();
    println!("{}", report.text(what));
    assert_eq!(report.frame_ms.len(), options.loops as usize);
    assert!(report.median_ms() > 0.0);
    let (db, same) = report.picture.unwrap();
    assert!(
        same && db.is_infinite(),
        "{what}: the picture differs ({db} dB)"
    );
    let mut get = |name: &str| {
        let id = player.named(name).unwrap();
        player.contents(id).unwrap().unwrap()
    };
    assert_eq!(get("sum"), expected.sum, "{what}: sum");
    assert_eq!(rows(&get("image")), expected.image, "{what}: image");
    assert_eq!(get("target"), expected.picture, "{what}: target");
    assert!(
        get("shared") == expected.shared,
        "{what}: memory the CPU sees"
    );
    for still in &expected.still {
        let id = player.named(still.name).unwrap();
        for (sub, (pitch, rows)) in still.subs.iter().enumerate() {
            let have = player.contents_of(id, sub as u32).unwrap().unwrap();
            for (r, row) in rows.iter().enumerate() {
                assert_eq!(
                    &have[r * pitch..][..row.len()],
                    &row[..],
                    "{what}: {} subresource {sub} row {r}",
                    still.name
                );
            }
        }
    }
    // Nothing was left out but what another queue ran.
    let known = [
        "submissions,",
        "waits of the drawing queue",
        "lists sent to a queue other than",
    ];
    assert!(
        !report
            .notes
            .iter()
            .any(|n| n.contains("recorder:") && !known.iter().any(|k| n.contains(k))),
        "{what}: {:?}",
        report.notes
    );
    assert!(report.notes.iter().any(|n| n.contains(known[2])));
    report
}

#[test]
fn a_frame_is_recorded_and_replayed() {
    let debug = std::env::var_os("OATHKEEP_TEST_D3D_DEBUG").is_some();
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: plain creation.
    unsafe {
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            D3D12GetDebugInterface(&mut layer).unwrap();
            layer.unwrap().EnableDebugLayer();
        }
        let _ = D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device);
    }
    let Some(device) = device else {
        eprintln!("no adapter: skipped");
        return;
    };
    let own = table_of(&device);
    // Without the recorder.
    let without = {
        let mut scene = Scene::new(&device, false);
        (0..5).for_each(|n| scene.frame(n));
        scene.outcome()
    };
    eprintln!("ran without the recorder");
    // Something is in the picture.
    assert!(without.sum.iter().any(|&b| b != 0));
    for s in &without.still {
        assert!(
            s.subs
                .iter()
                .all(|(_, rows)| rows.iter().flatten().any(|&b| b != 0)),
            "{}",
            s.name
        );
    }
    assert_eq!(
        without
            .still
            .iter()
            .map(|s| s.subs.len())
            .collect::<Vec<_>>(),
        [15, 3, 2]
    );
    let colors: std::collections::HashSet<[u8; 4]> = without
        .picture
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect();
    assert!(colors.len() > 30, "{}", colors.len());

    // With it: armed at the first present, the fourth frame is captured (a
    // frame whose lists are all recorded comes first).
    assert!(!record::on());
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        folder: dir.path().to_owned(),
        at: At::Frame(1),
        frames: 1,
        always: false,
    };
    let folder = record::start(settings, &device).unwrap();
    assert!(record::on());
    assert_ne!(table_of(&device), own, "the device is hooked");
    assert!(
        record::start(
            Settings {
                folder: dir.path().to_owned(),
                at: At::Trigger,
                frames: 1,
                always: false
            },
            &device
        )
        .is_err(),
        "one recorder a process"
    );
    eprintln!("recorder on");
    let mut scene = Scene::new(&device, true);
    eprintln!("scene made");
    (0..4).for_each(|n| scene.frame(n));
    assert_eq!(
        record::finished(),
        Some(folder.clone()),
        "see {}",
        folder.display()
    );
    let expected = scene.outcome();
    // It goes on as it did without the recorder.
    scene.frame(4);
    let after = scene.outcome();
    assert!(
        after == without,
        "the recorder changed what the program makes"
    );
    drop(scene);
    record::unhook(&device);
    assert_eq!(table_of(&device), own, "the device is as it was");
    for name in [
        "frame.bin",
        "contents.bin",
        "deltas.bin",
        "shaders.bin",
        "present.bin",
        "notes.txt",
        "frame-0.png",
    ] {
        assert!(folder.join(name).exists(), "{name}");
    }
    println!(
        "{}",
        std::fs::read_to_string(folder.join("notes.txt")).unwrap()
    );

    let plain = Options {
        loops: 3,
        ..Default::default()
    };
    let mut player = Player::load(dir.path(), &plain).unwrap();
    check(&mut player, &plain, &expected, "unchanged");
    let detailed = Options {
        detailed: true,
        ..plain.clone()
    };
    let report = check(&mut player, &detailed, &expected, "detailed");
    let count = |kind: &str| {
        report
            .categories
            .iter()
            .find(|c| c.0 == kind)
            .map_or(0, |c| c.2)
    };
    assert_eq!(count("argument clamps (ours)"), 3);
    assert_eq!(count("indirect draws"), 5);
    assert_eq!(count("draws"), 1);
    let meshes = count("indirect mesh draws");
    println!("{meshes} mesh draws");
    if meshes > 0 {
        assert!(
            report
                .shaders
                .iter()
                .any(|s| s.2.starts_with("a mesh pipeline (DXIL ")),
            "{:?}",
            report.shaders
        );
    }
    assert_eq!(count("dispatches"), 6);
    assert_eq!(count("clears"), 2);
    assert!(
        report
            .shaders
            .iter()
            .any(|s| s.2.starts_with("a drawing pipeline (DXIL ")),
        "{:?}",
        report.shaders
    );
    let merged = Options {
        merge_barriers: true,
        ..plain.clone()
    };
    check(&mut player, &merged, &expected, "merged barriers");
    let clamps = Options {
        skip_clamps: true,
        ..plain.clone()
    };
    let report = check(&mut player, &clamps, &expected, "without clamps");
    assert!(
        report.notes.iter().any(|n| n.contains("3 clamp passes")),
        "{:?}",
        report.notes
    );
    let one = Options {
        one_submit: true,
        ..plain.clone()
    };
    check(&mut player, &one, &expected, "one submission");
    let hash = Options {
        skip_hash_passes: true,
        ..plain.clone()
    };
    check(
        &mut player,
        &hash,
        &expected,
        "without hash passes (there are none)",
    );
    // These change what is drawn: they run, and then the frame is as before.
    let no_barriers = Options {
        no_barriers: true,
        ..plain.clone()
    };
    player.run(&no_barriers).unwrap();
    let some = Options {
        only: Some((0, 1)),
        ..plain.clone()
    };
    player.run(&some).unwrap();
    let skip = Options {
        skip_shaders: vec!["a drawing".into()],
        ..plain.clone()
    };
    let report = player.run(&skip).unwrap();
    assert!(
        !report.picture.unwrap().1,
        "without its draws the picture is another"
    );
    assert!(
        report.notes.iter().any(|n| n.contains("1 pipelines match"))
            && report
                .notes
                .iter()
                .any(|n| n.contains("left out 5 commands"))
    );
    let kept = Options {
        restore: false,
        ..plain.clone()
    };
    player.run(&kept).unwrap();
    check(&mut player, &plain, &expected, "unchanged again");
    drop(player);
    for buffers in [Buffers::Card, Buffers::Pc] {
        let o = Options {
            buffers,
            ..plain.clone()
        };
        let mut player = Player::load(dir.path(), &o).unwrap();
        check(&mut player, &o, &expected, &format!("buffers {buffers:?}"));
    }
    let o = Options {
        compare: true,
        merge_barriers: true,
        loops: 2,
        ..Default::default()
    };
    let both = play::replay(dir.path(), &o).unwrap();
    assert_eq!(both.len(), 2);
    assert!(both.iter().all(|(_, r)| r.picture.unwrap().1));
    assert!(folder.join("replay.png").exists() && folder.join("replay-unchanged.png").exists());
    if debug {
        let q = device.cast::<ID3D12InfoQueue>().unwrap();
        // SAFETY: plain queries; messages read into buffers of their size.
        unsafe {
            for i in 0..q.GetNumStoredMessages() {
                let mut size = 0;
                let _ = q.GetMessage(i, None, &mut size);
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let m = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if q.GetMessage(i, Some(m), &mut size).is_ok()
                    && (*m).Severity.0 <= D3D12_MESSAGE_SEVERITY_WARNING.0
                {
                    let text =
                        std::slice::from_raw_parts((*m).pDescription, (*m).DescriptionByteLength);
                    println!(
                        "D3D12 {} {}: {}",
                        (*m).Severity.0,
                        (*m).ID.0,
                        String::from_utf8_lossy(text)
                    );
                }
            }
        }
    }
}
