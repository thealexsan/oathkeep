use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::modules::xgameruntime::HRESULT;

use super::execute::read_list;
use super::scheduler::{Command, Job};
use super::{MEMORY_SOURCE, PAGE, PAGE_DESTINATION, Request, stream_log};

/// The walls trace (`OATHKEEP_WALLS_TRACE`): 0 or unset: off; 1: a line for
/// every read request that fails, with its file; 2: for every request (the
/// list of a launch's reads, by file and offset, is what a launch with the
/// first room's walls and one without are compared by).
fn walls_trace() -> u8 {
    static ON: OnceLock<u8> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("OATHKEEP_WALLS_TRACE").ok().and_then(|v| v.trim().parse::<u8>().ok()).unwrap_or(0)
    })
}

/// The files opened, by the address of our file object, with their paths
/// (for the walls trace: a request names its file by that address).
pub(super) static WALLS_FILES: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());

/// The walls trace's line for a request that is done.
pub(super) fn walls_request(request: &Request, result: &Result<(), HRESULT>, started: std::time::Instant) {
    let level = walls_trace();
    if level == 0 || (level == 1 && result.is_ok()) {
        return;
    }
    let file = if request.options & MEMORY_SOURCE != 0 {
        "memory".to_string()
    } else {
        let files = WALLS_FILES.lock().unwrap_or_else(|e| e.into_inner());
        let source = request.source as usize;
        files.iter().find(|f| f.0 == source).map_or_else(|| "?".to_string(), |f| f.1.clone())
    };
    let hr = match result {
        Ok(()) => 0,
        Err(hr) => *hr as u32,
    };
    tracing::info!(
        file,
        offset = request.offset,
        source_size = request.source_size,
        destination_size = request.destination_size,
        to_pages = request.options & PAGE_DESTINATION != 0,
        options = format_args!("{:#x}", request.options),
        ms = started.elapsed().as_millis() as u64,
        ok = result.is_ok(),
        hr = format_args!("{hr:#010x}"),
        "walls trace: a read request is done"
    );
}

/// The streaming trace (`OATHKEEP_DSTORAGE_TRACE`): 0 or unset: off; 1: a
/// line for every batch, and for every request whose page list, or whose
/// pages' mapping, changed when it should not have, or whose bytes another
/// running request writes too; 2: a line for every request.
fn trace() -> u8 {
    static ON: OnceLock<u8> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("OATHKEEP_DSTORAGE_TRACE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    })
}

/// When a request was enqueued and submitted, and what its page list held
/// then (a hash; 0 without a list). Only filled in for the trace, but for
/// `early`.
#[derive(Clone, Copy, Default)]
pub(super) struct Note {
    pub(super) enqueued: Option<std::time::Instant>,
    pub(super) at_enqueue: u64,
    pub(super) submitted: Option<std::time::Instant>,
    pub(super) at_submit: u64,
    /// Started before the title's submit.
    pub(super) early: bool,
}

impl Note {
    pub(super) fn enqueued(request: &Request) -> Note {
        match trace() {
            0 if stream_log::on() => Note { enqueued: Some(std::time::Instant::now()), ..Note::default() },
            0 => Note::default(),
            _ => Note {
                enqueued: Some(std::time::Instant::now()),
                at_enqueue: list_hash(request),
                ..Note::default()
            },
        }
    }

    pub(super) fn submitted(&mut self, request: &Request, early: bool) {
        self.early = early;
        if trace() > 0 || stream_log::on() {
            self.submitted = Some(std::time::Instant::now());
        }
        if trace() > 0 {
            self.at_submit = list_hash(request);
        }
    }
}

/// A hash of the page numbers a request's list holds now; 0 without one.
fn list_hash(request: &Request) -> u64 {
    list_of(request).map_or(0, |pages| {
        // FNV-1a; never 0.
        pages.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &p| (h ^ p as u64).wrapping_mul(0x100_0000_01b3)) | 1
    })
}

/// The page numbers a request's list holds now.
fn list_of(request: &Request) -> Option<Vec<usize>> {
    (request.options & PAGE_DESTINATION != 0 && !request.destination.is_null()).then(|| {
        let (offset, len) = Traced::bytes(request);
        read_list(request.destination.cast(), offset, len)
    })
}

/// The trace's line for a batch: how much of it goes to pages, and how many
/// of those lists the title filled in since it enqueued them (its protocol:
/// most of them; a batch started `early` runs on what they held before).
pub(super) fn trace_batch(queue: u64, commands: &[Command], early: bool) {
    if trace() == 0 {
        return;
    }
    let reads = || {
        commands.iter().filter_map(|c| match c {
            Command::Read(read) => Some(read),
            Command::Status(_) => None,
        })
    };
    let (mut to_pages, mut filled, mut empty) = (0, 0, 0);
    for read in reads().filter(|r| r.request.options & PAGE_DESTINATION != 0) {
        to_pages += 1;
        filled += usize::from(list_hash(&read.request) != read.note.at_enqueue);
        empty += usize::from(list_of(&read.request).is_some_and(|l| l.contains(&0)));
    }
    tracing::info!(
        queue,
        early,
        commands = commands.len(),
        to_pages,
        lists_filled_since_enqueued = filled,
        lists_not_filled = empty,
        oldest_ms = reads().next().and_then(|r| r.note.enqueued).map_or(0, |t| t.elapsed().as_millis() as u64),
        "dstorage trace: a batch"
    );
}

/// The bytes of pages that running requests write, for the trace: by page
/// number, the request and its bytes in the page.
type Writers = HashMap<usize, Vec<(u64, usize, usize)>>;
static WRITERS: Mutex<Option<Writers>> = Mutex::new(None);

/// A running request in the trace.
pub(super) struct Traced {
    pages: Vec<usize>,
    at_start: u64,
    shown: Vec<(usize, usize)>,
}

impl Traced {
    pub(super) fn start(job: &Job) -> Option<Traced> {
        if trace() == 0 {
            return None;
        }
        let at_start = list_hash(&job.request);
        let pages = list_of(&job.request).unwrap_or_default();
        let (offset, len) = Traced::bytes(&job.request);
        {
            let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
            let writers = writers.get_or_insert_with(Writers::new);
            for (i, &page) in pages.iter().enumerate().filter(|(_, p)| **p != 0) {
                let from = offset.max(i * PAGE) - i * PAGE;
                let to = (offset + len).min((i + 1) * PAGE) - i * PAGE;
                let others = writers.entry(page).or_default();
                for &(other, a, b) in others.iter().filter(|&&(_, a, b)| a < to && from < b) {
                    tracing::warn!(
                        page,
                        from,
                        to,
                        seq = job.seq,
                        other,
                        other_from = a,
                        other_to = b,
                        "dstorage trace: two running requests write the same bytes of a page"
                    );
                }
                others.push((job.seq, from, to));
            }
        }
        let shown = crate::modules::xmem::pages_shown(&pages, offset, len);
        Some(Traced { pages, at_start, shown })
    }

    /// Where in its first page a request's destination starts, and its size.
    fn bytes(request: &Request) -> (usize, usize) {
        ((request.page_offset & 0xffff) as usize, request.destination_size as usize)
    }

    pub(super) fn end(self, job: &Job, result: &Result<(), HRESULT>) {
        let (offset, len) = Traced::bytes(&job.request);
        let at_end = list_hash(&job.request);
        let moved = crate::modules::xmem::pages_shown(&self.pages, offset, len) != self.shown;
        {
            let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
            let writers = writers.get_or_insert_with(Writers::new);
            for page in &self.pages {
                if let Some(others) = writers.get_mut(page) {
                    others.retain(|w| w.0 != job.seq);
                    if others.is_empty() {
                        writers.remove(page);
                    }
                }
            }
        }
        let note = &job.note;
        // A request of the title's submit: its list is filled in by then,
        // and stays.
        let late = note.at_submit != self.at_start || self.at_start != at_end;
        if trace() < 2 && !late && !moved && !note.early && result.is_ok() {
            return;
        }
        let since = |t: Option<std::time::Instant>| t.map_or(0, |t| t.elapsed().as_millis() as u64);
        tracing::info!(
            seq = job.seq,
            queue = job.lane,
            early = note.early,
            list = format_args!("{:p}", job.request.destination),
            page_offset = job.request.page_offset,
            size = len,
            enqueued_ms_ago = since(note.enqueued),
            submitted_ms_ago = since(note.submitted),
            at_enqueue = format_args!("{:016x}", note.at_enqueue),
            at_submit = format_args!("{:016x}", note.at_submit),
            at_start = format_args!("{:016x}", self.at_start),
            at_end = format_args!("{at_end:016x}"),
            list_changed_late = late,
            mapping_changed = moved,
            first_page = self.pages.first().copied().unwrap_or(0),
            ok = result.is_ok(),
            "dstorage trace: a request"
        );
    }
}
