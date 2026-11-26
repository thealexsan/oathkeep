//! The streaming log (`OATHKEEP_DSTORAGE_LOG=<file>`): a line for every
//! request (`R`: queue, number, file, offset, source bytes, destination
//! bytes, options, when enqueued, submitted, started, done, result), every
//! status entry written (`S`: array, number, when enqueued, when written),
//! the title's first look at it after (`P`: array, when written, when
//! seen), every file opened (`F`), and each second what the title's mapping
//! calls cost (`M`, `xmem::map_costs`). Times are milliseconds since the
//! Unix epoch.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

use crate::modules::xgameruntime::HRESULT;

use super::scheduler::Job;

type Log = Mutex<std::io::BufWriter<std::fs::File>>;

fn log() -> Option<&'static Log> {
    static LOG: OnceLock<Option<Log>> = OnceLock::new();
    LOG.get_or_init(|| {
        let path = std::env::var_os("OATHKEEP_DSTORAGE_LOG")?;
        let file = std::fs::File::create(&path).ok()?;
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if let Some(costs) = crate::modules::xmem::map_costs() {
                    line(format_args!("M\t{}\t{costs}", now()));
                }
                if let Some(log) = log() {
                    let _ = log.lock().unwrap_or_else(|e| e.into_inner()).flush();
                }
            }
        });
        Some(Mutex::new(std::io::BufWriter::with_capacity(1 << 20, file)))
    })
    .as_ref()
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_DSTORAGE_LOG").is_some())
}

/// Milliseconds since the Unix epoch.
pub(super) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.as_millis() as u64)
}

pub(super) fn line(text: std::fmt::Arguments) {
    if let Some(log) = log() {
        let _ = writeln!(log.lock().unwrap_or_else(|e| e.into_inner()), "{text}");
    }
}

pub(super) fn request(job: &Job, started: std::time::Instant, result: &Result<(), HRESULT>) {
    if !on() {
        return;
    }
    let now = now();
    let ago = |t: Option<std::time::Instant>| now - t.map_or(0, |t| t.elapsed().as_millis() as u64);
    let r = &job.request;
    line(format_args!(
        "R\t{}\t{}\t{:x}\t{}\t{}\t{}\t{:x}\t{}\t{}\t{}\t{now}\t{:x}",
        job.lane,
        job.seq,
        r.source as usize,
        r.offset,
        r.source_size,
        r.destination_size,
        r.options,
        ago(job.note.enqueued),
        ago(job.note.submitted),
        ago(Some(started)),
        result.err().unwrap_or(0)
    ));
}
