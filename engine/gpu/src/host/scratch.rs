use super::*;

pub(super) struct Counted {
    /// Not reset since it was made.
    pub(super) fresh: std::sync::atomic::AtomicBool,
}

impl Counted {
    pub(super) fn new() -> Counted {
        ALIVE[0].fetch_add(1, Ordering::Relaxed);
        LISTS[3].fetch_add(1, Ordering::Relaxed);
        Counted {
            fresh: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// The list is reset: it is in use.
    pub(super) fn used(&self) {
        if self.fresh.swap(false, Ordering::Relaxed) {
            LISTS[3].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        ALIVE[0].fetch_sub(1, Ordering::Relaxed);
        self.used();
    }
}

/// A chunk of upload memory for lists' scratch data: the buffer, its
/// mapping.
pub(super) struct Chunk {
    pub(super) buffer: ID3D12Resource,
    pub(super) data: usize,
}

// SAFETY: a D3D12 resource (free-threaded) and its persistent mapping,
// written only by the list that holds the chunk.
unsafe impl Send for Chunk {}

/// Upload chunks handed out front to back: the chunks (each with its
/// bytes), the one in use and the bytes used in it.
#[derive(Default)]
pub(super) struct Scratch {
    pub(super) chunks: Vec<(Chunk, u64)>,
    pub(super) current: usize,
    pub(super) used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

impl Drop for Scratch {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.chunks));
    }
}

pub(super) const SCRATCH_CHUNK: u64 = 4 << 20;
/// A list's first chunk when the chunks are shared ([`scratch_pool`]): most
/// lists put a few constants or a rectangle's corners there, and every
/// list that did took 4 MiB of the PC's memory for it.
pub(super) const SCRATCH_FIRST: u64 = 256 << 10;
/// The most bytes of chunks kept while no list holds them.
pub(super) const SPARE_MOST: u64 = 128 << 20;
/// The same with [`keep_freed`].
pub(super) const SPARE_KEPT: u64 = 1 << 30;

/// Whether lists share their scratch chunks: a list takes a chunk when it
/// first needs one, a small one first, and gives all of them back when it
/// is reset or dropped, for the next list. Before, a list kept every chunk
/// it ever took (4 MiB each) for as long as it lived, used or not, and the
/// host lists of held submissions, which live for one submission, each
/// made theirs anew: some 57 upload buffers of 4 MiB made, zeroed and
/// freed per frame of gameplay (run 60).
/// `OATHKEEP_SCRATCH_POOL=0`: chunks of 4 MiB kept by their list, as before.
pub(super) fn scratch_pool() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_SCRATCH_POOL").is_none_or(|v| v != "0"))
}

/// Things of some sizes that nothing holds now, kept for whoever next asks
/// for one of the same size.
pub(super) struct Spare<T> {
    pub(super) kept: Vec<(T, u64)>,
    pub(super) bytes: u64,
}

impl<T> Spare<T> {
    pub(super) const fn new() -> Self {
        Spare { kept: Vec::new(), bytes: 0 }
    }

    /// A kept thing of `size` bytes, the one given last.
    pub(super) fn take(&mut self, size: u64) -> Option<T> {
        let at = self.kept.iter().rposition(|(_, s)| *s == size)?;
        self.bytes -= size;
        Some(self.kept.remove(at).0)
    }

    /// Keeps `thing` (`size` bytes) unless that takes the kept bytes past
    /// `most`: then it comes back.
    pub(super) fn give(&mut self, thing: T, size: u64, most: u64) -> Option<T> {
        if self.bytes + size > most {
            return Some(thing);
        }
        self.bytes += size;
        self.kept.push((thing, size));
        None
    }
}

/// Scratch chunks no list holds.
pub(super) static SPARE: Mutex<Spare<Chunk>> = Mutex::new(Spare::new());

/// A scratch chunk of `size` bytes: one kept ([`SPARE`]) or a new one.
pub(super) fn take_chunk(host: &Host, size: u64) -> Option<Chunk> {
    if scratch_pool()
        && let Some(chunk) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
    {
        ALIVE[3].fetch_sub(1, Ordering::Relaxed);
        ALIVE[4].fetch_sub(size, Ordering::Relaxed);
        ALIVE[5].fetch_add(1, Ordering::Relaxed);
        return Some(chunk);
    }
    let (buffer, data) = host.upload_buffer(size)?;
    name(&buffer, || "list scratch".into());
    ALIVE[1].fetch_add(1, Ordering::Relaxed);
    ALIVE[2].fetch_add(size, Ordering::Relaxed);
    Some(Chunk {
        buffer,
        data: data as usize,
    })
}

/// A list is done with `chunks` (it was reset after its submissions ran,
/// or dropped): kept for the next list, or freed past [`SPARE_MOST`] and
/// without the pool.
pub(super) fn give_chunks(chunks: Vec<(Chunk, u64)>) {
    if chunks.is_empty() {
        return;
    }
    let mut freed = Vec::new();
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        for (chunk, size) in chunks {
            let back = if scratch_pool() {
                let limit = if keep_freed() && !crate::residency::limited_host_ram() {
                    SPARE_KEPT
                } else {
                    SPARE_MOST
                };
                spare.give(chunk, size, limit)
            } else {
                Some(chunk)
            };
            match back {
                None => {
                    ALIVE[3].fetch_add(1, Ordering::Relaxed);
                    ALIVE[4].fetch_add(size, Ordering::Relaxed);
                }
                Some(chunk) => {
                    ALIVE[1].fetch_sub(1, Ordering::Relaxed);
                    ALIVE[2].fetch_sub(size, Ordering::Relaxed);
                    freed.push(chunk);
                }
            }
        }
    }
    // Freed outside the lock.
    drop(freed);
}

/// Drops spare scratch chunks kept idle when host memory is pressured.
pub(super) fn trim_spare_chunks() {
    let freed: Vec<(Chunk, u64)> = {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        if spare.kept.is_empty() {
            return;
        }
        spare.bytes = 0;
        std::mem::take(&mut spare.kept)
    };
    let count = freed.len() as u64;
    let bytes: u64 = freed.iter().map(|(_, s)| *s).sum();
    ALIVE[1].fetch_sub(count, Ordering::Relaxed);
    ALIVE[2].fetch_sub(bytes, Ordering::Relaxed);
    ALIVE[3].fetch_sub(count, Ordering::Relaxed);
    ALIVE[4].fetch_sub(bytes, Ordering::Relaxed);
    drop(freed);
}

/// What the host keeps for copies in the card's own memory
/// ([`crate::shadow::cards`]): three lists of ours and the compare.
pub(super) struct CardRange {
    pub(super) shadow: std::sync::Arc<crate::shadow::Shadow>,
    /// As declared (title addresses), for the statistics.
    pub(super) declared: (usize, usize),
    pub(super) span: crate::shadow::CardSpan,
    /// Its first piece's dword in the compare's results.
    pub(super) at: u32,
    /// Whether the card compares it; else every piece is fetched.
    pub(super) compared: bool,
    /// Looked at by `crate::picked`, and not declared: what is found changed
    /// there was written through a view nobody declared. Not landed.
    pub(super) picked: bool,
}

impl CardPlan {
    /// `crate::picked` is told where what it looked at changed, by the
    /// compare's results.
    pub(super) fn picked_found(&self, differs: &[u32]) {
        for r in self.ranges.iter().filter(|r| r.picked) {
            let changed = (0..r.span.pieces).find(|p| differs.get((r.at + p) as usize).is_some_and(|&d| torn::flagged(d)));
            if let Some(p) = changed {
                crate::picked::found(&r.shadow, r.shadow.card_piece(&r.span, p).0);
            }
        }
    }
}

/// What a submission does for copies in the card's memory.
pub(super) struct CardPlan {
    pub(super) ranges: Vec<CardRange>,
    /// Deduplicated per-buffer joined piece runs to hash/compare on the GPU:
    /// `(shadow, before_gpu, hash_size, run_span, out_at, mode)`.
    pub(super) check_runs: Vec<(std::sync::Arc<crate::shadow::Shadow>, u64, u32, crate::shadow::CardSpan, u32, u32)>,
    /// The compare, run after the submission's lists.
    pub(super) check: Option<ID3D12CommandList>,
    /// The copies whose card side was brought up to date, alive until the
    /// submission has run.
    pub(super) _synced: Vec<std::sync::Arc<crate::shadow::Shadow>>,
    /// The lists of ours the plan recorded (whether the compare, the
    /// list), given back by `Host::card_done`.
    pub(super) lists: Vec<(bool, Prelude)>,
    /// With [`relay`]: the number of the send to copies in the card's memory
    /// that the submission ran behind ([`QUEUE_ORDER`]).
    pub(super) sent_as: u64,
    /// With [`upcheck`]: the look at what it sent up.
    pub(super) looks: Option<upcheck::Looks>,
    /// With [`send`]: the memory its sends come from.
    pub(super) sends: Option<send::Memory>,
}

/// One piece of submission-time work, in recorded order.
pub(super) enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory, and how
    /// the card could bring the bytes instead.
    Fill(usize, usize, usize, Option<card_steps::FillTo>),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only, and the title memory it reads
    /// (from, to; empty for none).
    Host(Box<dyn Fn() + Send>, (usize, usize)),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(std::sync::Arc<crate::texture::Staging>),
    /// Reads a CPU-written linear movie plane into a per-list upload buffer.
    Plane((usize, usize), Box<dyn Fn() + Send>),
    /// A title buffer copy that the list does on the card ([`place`]): what
    /// is done on the CPU for it.
    Placed(place::Placed),
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
pub(super) struct Deferred {
    /// Destination, source, bytes.
    pub(super) copies: Vec<Step>,
    pub(super) uploads: Vec<ID3D12Resource>,
    /// Standalone upload footprints kept through this list's fence (movie planes).
    pub(super) plane_uploads: Vec<(Chunk, u64)>,
    /// The bytes of `uploads` (counted in [`ALIVE`] while they are held).
    pub(super) upload_bytes: u64,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    pub(super) kept: Vec<windows::core::IUnknown>,
    /// The pipelines [`HostList::bind_pipeline`] kept, by address.
    pub(super) bound: std::collections::HashSet<usize, std::hash::BuildHasherDefault<AddressHash>>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    pub(super) refreshed: std::collections::HashSet<(usize, usize, usize), std::hash::BuildHasherDefault<AddressHash>>,
    /// Ranges declared written by shaders since the list last wrote title
    /// memory (host copy, address, bytes): declaring them again would
    /// change nothing.
    pub(super) gpu_written: std::collections::HashSet<(usize, usize, usize), std::hash::BuildHasherDefault<AddressHash>>,
    /// Scratch memory [`HostList::fill_scratch`] fills since the list last
    /// wrote title memory (source, bytes): its host address.
    pub(super) filled: std::collections::HashMap<(usize, usize), u64, std::hash::BuildHasherDefault<AddressHash>>,
    /// Textures whose contents the list brings up to date.
    pub(super) textures: std::collections::HashSet<usize>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    pub(super) writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Textures read back (texture to buffer copies), done once the list
    /// has run: the host buffer the list copies the texels into, the title
    /// memory they go to, the bytes of a row, the rows, the title's row
    /// pitch.
    pub(super) backs: Vec<(ID3D12Resource, usize, [usize; 3])>,
    /// Title textures of the kinds copied back ([`crate::texback`]) that
    /// the list's shaders write, copied when the list closes.
    pub(super) back_wanted: Vec<crate::texback::Target>,
    /// Those copies, whose texels go to title memory once the list has run.
    pub(super) texture_backs: Vec<crate::texback::Back>,
    /// Whether the indirect-work budget is zeroed for this recording (a
    /// step on the CPU, or in the list for the card's copy).
    pub(super) budget_zeroed: bool,
    /// The indirect scratch buffer (`HostList::indirect_args`, by its
    /// interface pointer) that a clamp in this recording left readable as
    /// indirect arguments; 0 if none. It stays readable until the next
    /// clamp into it ([`HostList::indirect_state`]).
    pub(super) indirect_readable: usize,
}

impl Deferred {
    /// Holds `buffer` (`size` bytes of upload memory) until the list is
    /// reset.
    pub(super) fn hold_upload(&mut self, buffer: ID3D12Resource, size: u64) {
        ALIVE[6].fetch_add(1, Ordering::Relaxed);
        ALIVE[7].fetch_add(size, Ordering::Relaxed);
        self.upload_bytes += size;
        self.uploads.push(buffer);
    }

    /// The steps recorded last write title memory: what was brought up to
    /// date before them is not any more.
    pub(super) fn title_written(&mut self) {
        self.refreshed.clear();
        self.gpu_written.clear();
        self.filled.clear();
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.plane_uploads));
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
    }
}
