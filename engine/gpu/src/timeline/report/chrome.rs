//! Chrome Trace JSON output.

use super::trace::Trace;
use super::{Names, Threads, ms};
use crate::timeline::record::{DEPTH, Kind, frequency};

fn escaped(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if (c as u32) < 32 => vec![' '],
            c => vec![c],
        })
        .collect()
}

/// The Chrome trace.
pub(super) fn chrome(
    trace: &Trace,
    names: &mut Names,
    stacks: &[[usize; DEPTH]],
    threads: &Threads,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let start = trace.events.first().map_or(0, |e| e.t0);
    let us = |t: u64| t.saturating_sub(start) as f64 * 1e6 / frequency() as f64;
    write!(out, "{{\"traceEvents\":[\n{{\"ph\":\"M\",\"pid\":1,\"tid\":0,\"name\":\"thread_name\",\"args\":{{\"name\":\"the card\"}}}}")?;
    for (id, (_, name, begun)) in &threads.all {
        let name = if name.is_empty() { names.of(*begun + 1) } else { name.clone() };
        write!(
            out,
            ",\n{{\"ph\":\"M\",\"pid\":1,\"tid\":{id},\"name\":\"thread_name\",\"args\":{{\"name\":\"{}\"}}}}",
            escaped(&name)
        )?;
    }
    for e in &trace.events {
        let (ts, dur) = (us(e.t0), us(e.t1) - us(e.t0));
        let (pid, tid, cpu) = (1, e.thread, e.cpu);
        match e.kind {
            Kind::Wake => {}
            Kind::Told => write!(
                out,
                ",\n{{\"ph\":\"i\",\"s\":\"t\",\"pid\":{pid},\"tid\":{tid},\"ts\":{ts:.1},\"cat\":\"told\",\"name\":\"{}\",\"args\":{{\"at\":\"{:#x}\",\"value\":\"{:#x}\"}}}}",
                escaped(e.name),
                e.a,
                e.b
            )?,
            Kind::Sample => {
                let stack = (e.stack as usize).checked_sub(1).and_then(|s| stacks.get(s)).map_or(&[][..], |s| &s[..]);
                let stack: Vec<String> = stack.iter().take_while(|a| **a != 0).map(|a| format!("{a:#x}")).collect();
                write!(
                    out,
                    ",\n{{\"ph\":\"i\",\"s\":\"t\",\"pid\":{pid},\"tid\":{tid},\"ts\":{ts:.1},\"cat\":\"sample\",\"name\":\"{}\",\"args\":{{\"at\":\"{:#x}\",\"stack\":\"{}\"}}}}",
                    escaped(&names.of(e.a as usize)),
                    e.a,
                    stack.join(" ")
                )?;
            }
            Kind::Frame => write!(out, ",\n{{\"ph\":\"i\",\"s\":\"g\",\"pid\":{pid},\"tid\":{tid},\"ts\":{ts:.1},\"name\":\"a frame ends\"}}")?,
            Kind::Wait => {
                let site = (e.stack as usize).checked_sub(1).and_then(|s| stacks.get(s)).map_or(String::new(), |s| names.site(s));
                write!(
                    out,
                    ",\n{{\"ph\":\"X\",\"pid\":{pid},\"tid\":{tid},\"ts\":{ts:.1},\"dur\":{dur:.1},\"cat\":\"wait\",\"cname\":\"grey\",\
                     \"name\":\"wait\",\"args\":{{\"what\":\"{}\",\"object\":\"{:#x}\",\"fence\":\"{}\",\"at\":\"{}\"}}}}",
                    e.name,
                    e.a,
                    e.b,
                    escaped(&site)
                )?;
            }
            _ => write!(
                out,
                ",\n{{\"ph\":\"X\",\"pid\":{pid},\"tid\":{tid},\"ts\":{ts:.1},\"dur\":{dur:.1},\"cat\":\"{:?}\",\"name\":\"{}\",\
                 \"args\":{{\"a\":{},\"b\":{},\"cpu\":{cpu}}}}}",
                e.kind,
                escaped(e.name),
                e.a,
                e.b
            )?,
        }
    }
    for r in trace.runs.iter().filter(|r| r.on != (0, 0)) {
        write!(
            out,
            ",\n{{\"ph\":\"X\",\"pid\":1,\"tid\":0,\"ts\":{:.1},\"dur\":{:.1},\"name\":\"submission {}\",\"args\":{{\"sent by\":{}}}}}",
            us(r.on.0),
            us(r.on.1) - us(r.on.0),
            r.fence,
            r.thread
        )?;
    }
    // Who woke whom.
    let mut id = 0u64;
    for (&thread, waits) in &trace.waits {
        for w in waits.iter().filter(|w| ms(w.t1 - w.t0) >= 0.1) {
            if let Some((other, at)) = trace.waker(thread, w) {
                id += 1;
                write!(
                    out,
                    ",\n{{\"ph\":\"s\",\"pid\":1,\"tid\":{other},\"ts\":{:.1},\"id\":{id},\"cat\":\"wake\",\"name\":\"woke\"}},\
                     \n{{\"ph\":\"f\",\"bp\":\"e\",\"pid\":1,\"tid\":{thread},\"ts\":{:.1},\"id\":{id},\"cat\":\"wake\",\"name\":\"woke\"}}",
                    us(at),
                    us(w.t1)
                )?;
            }
        }
    }
    writeln!(out, "\n]}}")
}
