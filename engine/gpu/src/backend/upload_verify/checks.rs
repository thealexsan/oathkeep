use super::*;

/// Reads of title memory through one copy while a flight in the air may
/// write it through another: (reader, writer) by their spans: how often, and
/// the last range they share.
type Crossed = BTreeMap<((u64, u64), (u64, u64)), (u64, usize, usize)>;
static CROSSED: Mutex<(u64, Crossed)> = Mutex::new((0, BTreeMap::new()));

/// Flights are in the air and `runs` are about to be prepared: which
/// refreshes of theirs read title memory that a flight may be writing through
/// another copy of it. Such a refresh takes what was there before the flight
/// (nothing waits for it), and no later one knows that it was too early but
/// for the landing's own write.
pub(in crate::backend) fn reads(runs: &[&HostList]) {
    let mut writes: Vec<(std::sync::Arc<Shadow>, usize, usize)> = Vec::new();
    let mut all = Vec::new();
    for item in &crate::backend::line().line {
        if let Late::Run(flight) = item {
            for (shadow, ranges) in &flight.back {
                writes.extend(ranges.iter().map(|&(a, b)| (shadow.clone(), a, b)));
                join_ranges(&mut all, ranges.iter().copied());
            }
        }
    }
    if writes.is_empty() {
        return;
    }
    let mut crossed = CROSSED.lock().unwrap_or_else(|e| e.into_inner());
    for list in runs {
        for step in &list.deferred().copies {
            let Step::Refresh(shadow, at, len) = step else {
                continue;
            };
            let (from, to) = (*at, *at + *len);
            if !touches(&all, from, to) {
                continue;
            }
            for (writer, a, b) in &writes {
                if *a < to && from < *b && !std::sync::Arc::ptr_eq(writer, shadow) {
                    let e = crossed.1.entry((shadow.span(), writer.span())).or_default();
                    *e = (e.0 + 1, (*a).max(from), (*b).min(to));
                }
            }
        }
    }
    let now = now_ms();
    if now < crossed.0 + 4000 {
        return;
    }
    crossed.0 = now;
    let mut all: Vec<_> = std::mem::take(&mut crossed.1).into_iter().collect();
    all.sort_by_key(|(_, e)| std::cmp::Reverse(e.0));
    for (((r, rn), (w, wn)), (n, a, b)) in all.into_iter().take(12) {
        tracing::warn!(
            "up-copy check: {n} refreshes of {r:#x}+{rn:#x} read title memory that flights in the air may write through              {w:#x}+{wn:#x} (the last at {a:#x}..{b:#x})"
        );
    }
}

/// The title's buffer copies that are out of their place in a submission:
/// by what is wrong, bytes and destination: how often, the last source, and
/// (for shader writes) how much was declared.
type Misplaced = BTreeMap<(&'static str, usize, usize), (u64, usize, usize)>;
static MISPLACED: Mutex<(u64, Misplaced)> = Mutex::new((0, BTreeMap::new()));

/// A submission is about to be prepared: its title buffer copies are done on
/// the CPU then, all of them before the first of its draws runs. Where that
/// is not the order of its lists: what an earlier draw of it reads of the
/// destination, or shaders of it write there, or an earlier copy of it put
/// there for what lies between; and a source that shaders of it write first.
pub(in crate::backend) fn order(runs: &[&HostList]) {
    let (mut read, mut written, mut copied): (Vec<(usize, usize)>, Vec<(usize, usize)>, Vec<(usize, usize)>) =
        (Vec::new(), Vec::new(), Vec::new());
    let meets = |all: &[(usize, usize)], from: usize, to: usize| all.iter().rev().find(|r| r.0 < to && from < r.1).copied();
    let mut misplaced = MISPLACED.lock().unwrap_or_else(|e| e.into_inner());
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                Step::Refresh(_, at, len) => read.push((*at, *at + *len)),
                Step::GpuWrite(_, at, len) => written.push((*at, *at + *len)),
                &Step::Copy(dst, src, len) => {
                    let mut say = |what: &'static str, range: (usize, usize)| {
                        let e = misplaced.1.entry((what, len, dst)).or_default();
                        *e = (e.0 + 1, src, range.1 - range.0);
                    };
                    if let Some(r) = meets(&written, src, src + len) {
                        say("its source is declared written earlier in the submission", r);
                    }
                    if let Some(r) = meets(&written, dst, dst + len) {
                        say("its destination is declared written earlier in the submission", r);
                    }
                    if let Some(r) = meets(&copied, dst, dst + len) {
                        say("its destination is copied to earlier in the submission", r);
                    } else if let Some(r) = meets(&read, dst, dst + len) {
                        say("its destination is read earlier in the submission", r);
                    }
                    copied.push((dst, dst + len));
                }
                _ => {}
            }
        }
    }
    let now = now_ms();
    if now < misplaced.0 + 4000 {
        return;
    }
    misplaced.0 = now;
    let mut all: Vec<_> = std::mem::take(&mut misplaced.1).into_iter().collect();
    all.sort_by_key(|(_, e)| std::cmp::Reverse(e.0));
    let mut kinds: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for ((what, len, _), (n, ..)) in &all {
        let e = kinds.entry(what).or_default();
        *e = (e.0 + n, e.1 + n * *len as u64);
    }
    for (what, (n, bytes)) in kinds {
        tracing::warn!("up-copy check, copies out of place: {n} ({bytes} bytes): {what}");
    }
    for ((what, len, dst), (n, src, declared)) in all.into_iter().take(16) {
        tracing::warn!("up-copy check, a copy out of place: {n} times {len:#x} bytes from {src:#x} to {dst:#x}: {what} ({declared:#x} bytes there)");
    }
}
