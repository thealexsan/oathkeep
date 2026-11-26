//! Text summary generation from a reconstructed timeline trace.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use super::trace::{Part, Run, Trace, Wait};
use super::{Names, OUTSIDE, Threads, ms};
use crate::timeline::record::{COPY_QUEUE, DEPTH, Kind};

/// `parts` by cause, the largest first.
fn by_cause(parts: &[Part], label: &dyn Fn(u32) -> String) -> Vec<(String, u64)> {
    let mut sums: BTreeMap<String, u64> = BTreeMap::new();
    for p in parts {
        let cause = if p.cause == OUTSIDE { format!("{} on {}", p.cause, label(p.thread)) } else { p.cause.clone() };
        *sums.entry(cause).or_default() += p.to - p.from;
    }
    let mut sums: Vec<(String, u64)> = sums.into_iter().collect();
    sums.sort_by_key(|s| std::cmp::Reverse(s.1));
    sums
}

/// The summary.
pub(super) fn summary(
    trace: &Trace,
    names: &mut Names,
    stacks: &[[usize; DEPTH]],
    before: &Threads,
    after: &Threads,
) -> String {
    let mut text = String::new();
    let Some((&(start, _), &(stop, _))) = trace.frames.first().zip(trace.frames.last()) else {
        return "no frames\n".into();
    };
    let frames = (trace.frames.len() - 1).max(1) as f64;
    let mut labels: HashMap<u32, String> = HashMap::new();
    for (&id, (_, name, begun)) in after.all.iter().chain(before.all.iter()) {
        labels.entry(id).or_insert_with(|| match name.is_empty() {
            true => format!("{id} ({})", names.of(*begun + 1)),
            false => format!("{id} \"{name}\""),
        });
    }
    let label = |id: u32| match id {
        0 => "the card".to_string(),
        id => labels.get(&id).cloned().unwrap_or_else(|| id.to_string()),
    };
    let _ = writeln!(text, "{} frames in {:.1} ms ({:.1} ms a frame); {} events", frames, ms(stop - start), ms(stop - start) / frames, trace.events.len());

    // The chains.
    let mut all: Vec<Part> = Vec::new();
    for pair in trace.frames.windows(2) {
        let chain = trace.chain(pair[1].1, pair[1].0, pair[0].0);
        let _ = writeln!(text, "\nframe of {:.1} ms, present on {}: the chain it came out of, by cause", ms(pair[1].0 - pair[0].0), label(pair[1].1));
        for (cause, sum) in by_cause(&chain, &label).into_iter().take(24) {
            let _ = writeln!(text, "  {:7.2} ms  {cause}", ms(sum));
        }
        all.extend(chain);
    }
    let whole = trace.chain(trace.frames.last().map_or(0, |f| f.1), stop, start);
    let _ = writeln!(text, "\nthe chain back through all frames, per frame, by cause");
    for (cause, sum) in by_cause(&whole, &label).into_iter().take(40) {
        let _ = writeln!(text, "  {:7.2} ms  {cause}", ms(sum) / frames);
    }
    let _ = writeln!(text, "\nby thread, per frame");
    let mut by_thread: BTreeMap<u32, u64> = BTreeMap::new();
    for p in &whole {
        *by_thread.entry(p.thread).or_default() += p.to - p.from;
    }
    let mut by_thread: Vec<(u32, u64)> = by_thread.into_iter().collect();
    by_thread.sort_by_key(|t| std::cmp::Reverse(t.1));
    for (thread, sum) in by_thread {
        let _ = writeln!(text, "  {:7.2} ms  {}", ms(sum) / frames, label(thread));
    }
    let _ = writeln!(text, "\nthat chain, oldest first (parts under 0.15 ms left out; ms since the first present)");
    for p in whole.iter().rev().filter(|p| ms(p.to - p.from) >= 0.15) {
        let _ = writeln!(text, "  {:8.2} +{:6.2}  {:32} {}", ms(p.from - start), ms(p.to - p.from), label(p.thread), p.cause);
    }

    // The card.
    let runs: Vec<&Run> = trace.runs.iter().filter(|r| r.on != (0, 0) && r.on.1 > start && r.on.0 < stop).collect();
    let (mut busy, mut reach) = (0, start);
    let mut idle: BTreeMap<String, u64> = BTreeMap::new();
    let mut idle_all = 0;
    for r in &runs {
        let (a, b) = (r.on.0.clamp(start, stop), r.on.1.clamp(start, stop));
        if a > reach {
            idle_all += a - reach;
            if r.t1 > reach {
                for p in trace.chain(r.thread, r.t1.min(a), reach) {
                    let cause = if p.cause == OUTSIDE { format!("{} on {}", p.cause, label(p.thread)) } else { p.cause };
                    *idle.entry(cause).or_default() += p.to - p.from;
                }
            }
        }
        busy += b.saturating_sub(a.max(reach));
        reach = reach.max(b);
    }
    let _ = writeln!(
        text,
        "\nthe card, per frame: {:.2} ms running {:.1} submissions, {:.2} ms idle between them; of the idle time, by what the chain to the \
         next submission was",
        ms(busy) / frames,
        runs.len() as f64 / frames,
        ms(idle_all) / frames
    );
    let mut idle: Vec<(String, u64)> = idle.into_iter().collect();
    idle.sort_by_key(|i| std::cmp::Reverse(i.1));
    for (cause, sum) in idle.into_iter().take(30) {
        let _ = writeln!(text, "  {:7.2} ms  {cause}", ms(sum) / frames);
    }

    // Threads.
    let rate = (after.cycles.saturating_sub(before.cycles)) as f64 / ms(after.ticks.saturating_sub(before.ticks)).max(1e-9);
    let mut threads: Vec<(u32, f64, u64)> = after
        .all
        .iter()
        .map(|(&id, t)| {
            let ran = t.0.saturating_sub(before.all.get(&id).map_or(0, |b| b.0)) as f64 / rate.max(1e-9);
            let waits = trace.waits.get(&id).map_or(&[][..], |w| w);
            let waited = waits.iter().map(|w| w.t1.min(stop).saturating_sub(w.t0.max(start))).sum();
            (id, ran, waited)
        })
        .collect();
    threads.sort_by(|a, b| b.1.total_cmp(&a.1));
    let _ = writeln!(text, "\nthreads, per frame: running, waiting in what is wrapped, outermost spans of ours");
    for (id, ran, waited) in threads.into_iter().filter(|t| t.1 / frames >= 0.2).take(60) {
        let mut sums: BTreeMap<&str, u64> = BTreeMap::new();
        let mut reach = 0;
        for &i in trace.spans.get(&id).map_or(&[][..], |s| s) {
            let e = &trace.events[i];
            if e.t1 > reach {
                *sums.entry(e.name).or_default() += e.t1.min(stop).saturating_sub(e.t0.max(reach).max(start));
                reach = e.t1;
            }
        }
        let mut sums: Vec<(&str, u64)> = sums.into_iter().collect();
        sums.sort_by_key(|s| std::cmp::Reverse(s.1));
        let spans: Vec<String> = sums.iter().take(5).map(|(name, sum)| format!("{name} {:.1}", ms(*sum) / frames)).collect();
        let _ = writeln!(text, "  {:6.1} ran {:6.1} waited  {:36} {}", ran / frames, ms(waited) / frames, label(id), spans.join(", "));
    }

    // Spans, all threads, each by name with what lies in it.
    let mut sums: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
    for e in trace.events.iter().filter(|e| matches!(e.kind, Kind::Span | Kind::Kernel | Kind::Sent) && e.t1 > start && e.t0 < stop) {
        let s = sums.entry(e.name).or_default();
        let took = e.t1.min(stop) - e.t0.max(start);
        *s = (s.0 + took, s.1 + 1, s.2.max(took));
    }
    let mut sums: Vec<_> = sums.into_iter().collect();
    sums.sort_by_key(|s| std::cmp::Reverse(s.1.0));
    let _ = writeln!(text, "\nspans and kernel calls, all threads, per frame (a span counts what lies in it)");
    for (name, (sum, n, most)) in sums.into_iter().take(80) {
        let _ = writeln!(text, "  {:8.2} ms in {:8.1}, longest {:6.2}  {name}", ms(sum) / frames, n as f64 / frames, ms(most));
    }

    // Waits: who, where, for whom.
    let mut groups: HashMap<(u32, String, String), (u64, u64, u64)> = HashMap::new();
    for (&thread, waits) in &trace.waits {
        for w in waits.iter().filter(|w| w.t1 > start && w.t0 < stop && ms(w.t1 - w.t0) >= 0.05) {
            let e = &trace.events[w.event];
            let site = (e.stack as usize).checked_sub(1).and_then(|s| stacks.get(s)).map_or(String::new(), |s| names.site(s));
            let by = match (w.fence, self_waker(trace, thread, w)) {
                (COPY_QUEUE, _) => "the card's copy queue".to_string(),
                (0, Some((other, _))) => label(other),
                (0, None) => "nobody seen".into(),
                _ => "the card".into(),
            };
            let g = groups.entry((thread, format!("{} at {site}", e.name), by)).or_default();
            let took = w.t1.min(stop) - w.t0.max(start);
            *g = (g.0 + took, g.1 + 1, g.2.max(took));
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.1.0));
    let _ = writeln!(text, "\nwaits of 50 us or more, per frame: who, what and where, who ended them");
    for ((thread, what, by), (sum, n, most)) in groups.into_iter().take(150) {
        let _ = writeln!(
            text,
            "  {:7.2} ms in {:7.1}, longest {:6.2}  {} | {what} | ended by {by}",
            ms(sum) / frames,
            n as f64 / frames,
            ms(most),
            label(thread)
        );
    }

    // Long list calls.
    let mut calls: HashMap<String, (u64, u64)> = HashMap::new();
    for e in trace.events.iter().filter(|e| e.name == "a long list call" && e.t1 > start && e.t0 < stop) {
        let site = (e.stack as usize).checked_sub(1).and_then(|s| stacks.get(s)).map_or(String::new(), |s| names.site(s));
        let c = calls.entry(site).or_default();
        *c = (c.0 + (e.t1 - e.t0), c.1 + 1);
    }
    let mut calls: Vec<_> = calls.into_iter().collect();
    calls.sort_by_key(|c| std::cmp::Reverse(c.1.0));
    let _ = writeln!(text, "\nlist calls of 50 us or more, per frame");
    for (site, (sum, n)) in calls.into_iter().take(25) {
        let _ = writeln!(text, "  {:7.2} ms in {:7.1}  {site}", ms(sum) / frames, n as f64 / frames);
    }
    text
}

fn self_waker(trace: &Trace, thread: u32, w: &Wait) -> Option<(u32, u64)> {
    trace.waker(thread, w)
}
