use super::*;
use super::io::{changed_runs, zero};
use super::reads::Reads;

use super::*;

#[test]
fn changes_are_found_by_page() {
    let mut live = vec![0u8; 5 * PAGE + 100];
    let mut kept = live.clone();
    let find = |live: &[u8], kept: &mut [u8]| {
        // SAFETY: two arrays of one length.
        unsafe { changed_runs(live.as_ptr(), kept.as_mut_ptr(), live.len()) }
    };
    assert!(find(&live, &mut kept).is_empty());
    live[PAGE + 5] = 1;
    live[2 * PAGE] = 2;
    live[5 * PAGE + 99] = 3;
    assert_eq!(
        find(&live, &mut kept),
        [(PAGE, 3 * PAGE), (5 * PAGE, 5 * PAGE + 100)]
    );
    assert_eq!(live, kept);
    assert!(find(&live, &mut kept).is_empty());
}

#[test]
fn what_a_submission_reads() {
    let mut reads = Reads::default();
    let part = |from, len| Cmd::CopyBuffer {
        dst: 9,
        dst_at: 0,
        src: 1,
        src_at: from,
        len,
    };
    reads.note(&[
        part(100, 50),
        part(4000, 200),
        part(20_000, 1),
        Cmd::CopyTexture {
            dst: Loc::Sub { res: 9, sub: 0 },
            at: [0; 3],
            src: Loc::Placed {
                res: 2,
                offset: 4096,
                format: 28,
                size: [16, 16, 1],
                pitch: 256,
            },
            part: None,
        },
        Cmd::Indirect {
            signature: 1,
            most: 1,
            args: 3,
            at: 8,
            count: 0,
            count_at: 0,
        },
        Cmd::RootView {
            graphics: true,
            kind: slot::CBV,
            param: 0,
            va: Va::At { res: 4, offset: 5 },
        },
    ]);
    assert_eq!(reads.pages(1, 1 << 20, false), [(0, 8192), (16384, 20480)]);
    assert_eq!(
        reads.pages(1, 20_100, false),
        [(0, 8192), (16384, 20_100)],
        "its end"
    );
    assert_eq!(reads.pages(2, 1 << 20, false), [(4096, 8192)]);
    assert_eq!(reads.pages(3, 100, false), [(0, 100)]);
    assert_eq!(reads.pages(4, 100, false), [(0, 100)]);
    assert_eq!(
        reads.pages(5, 100, true),
        [(0, 100)],
        "a descriptor names it"
    );
    assert!(reads.pages(5, 100, false).is_empty() && reads.pages(9, 100, false).is_empty());
}

#[test]
fn zeros() {
    assert!(zero(&[]));
    assert!(zero(&[0; 1001]));
    for i in [0, 7, 500, 1000] {
        let mut b = [0u8; 1001];
        b[i] = 1;
        assert!(!zero(&b));
    }
}
