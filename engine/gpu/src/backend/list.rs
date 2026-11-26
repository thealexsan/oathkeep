use super::*;


/// The batches of clamped records in one scratch buffer
/// ([`HostList::batch_place`]).
pub(super) const BATCHES: u32 = 8;

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    pub(super) allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    /// Work for submission time, in recorded order.
    pub(super) deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    pub(super) scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    pub(super) states: Mutex<States>,
    /// What the list does with depth textures that have a colour twin, by
    /// twin ([`TwinUse`]).
    pub(super) twins: Mutex<std::collections::HashMap<usize, TwinUse>>,
    /// The list that runs those transitions just before this one, made
    /// when the list first has such transitions ([`lazy_prelude`]).
    pub(super) prelude: OnceLock<Option<Prelude>>,
    /// The pipelines the list binds, for reports of slow submissions.
    pub(super) pipelines: std::sync::Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// What its indirect executions ran, for the report of a removed device.
    #[cfg(feature = "diagnostics")]
    pub(super) indirects: dred_indirects::Shared,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted, and after
    /// it what the clamp notes about mesh records. In memory the CPU maps.
    pub(super) budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    /// The same words in the card's own memory ([`card_budget`]), which the
    /// clamp uses instead when there is one: zeroed in the list before its
    /// first clamp, its notes copied to `budget` when the list closes.
    pub(super) card_budget: OnceLock<Option<ID3D12Resource>>,
    /// Device-local scratch buffer for clamped indirect arguments (`indirect`),
    /// grown to the largest indirect execution recorded on the list.
    pub(super) indirect_args: Mutex<Option<(ID3D12Resource, u64)>>,
    /// Scratch buffers of the batched clamps (`crate::execution::indirect::batch`), and
    /// how many batches of this recording have a place in them.
    pub(super) batch_args: Mutex<(Vec<ID3D12Resource>, u32)>,
    /// Refreshes of host copies that a thread of ours is still finding, and
    /// where among the list's steps they belong ([`aside`]).
    pub(super) coming: Mutex<Option<(usize, std::sync::Arc<heap_pass_async::Coming>)>>,
    /// Another number each time something changes that what a draw reads
    /// depends on ([`HostList::changed_something`]); no two lists have the
    /// same.
    pub(super) epoch: std::sync::atomic::AtomicU64,
    /// Set whenever an inline depth-plane pass or depth-to-plane copy-back
    /// overwrites the list's descriptor heaps, root signatures, pipeline, or
    /// graphics target/viewport/scissor/topology state.
    pub(super) state_clobbered: std::sync::atomic::AtomicBool,
    /// Counts the list in [`ALIVE`] while it lives.
    pub(super) _alive: Counted,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    pub(super) notes: std::sync::Arc<Mutex<Vec<Note>>>,
    /// With `OATHKEEP_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    pub(super) stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

/// A list's prelude: an allocator and a closed list.
pub(super) type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

/// Whether a host list's prelude is made when the list first needs one. A
/// prelude runs the texture transitions and twin copies a list needs before
/// it; lists that name no texture never have one, and every host list was
/// made with one all the same: twice the host objects per list, for the
/// title's 630 lists and for every list made anew.
/// `OATHKEEP_LAZY_PRELUDE=0`: made with the list, as before.
pub(super) fn lazy_prelude() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_LAZY_PRELUDE").is_none_or(|v| v != "0"))
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
pub(super) const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
pub(super) type Note = (String, Option<(u64, u64)>);

pub(super) type States = std::collections::HashMap<
    usize,
    (u32, u32, std::sync::Arc<crate::d3d12::resource::HostTexture>),
>;

/// What a list does with a depth texture that has a colour twin
/// ([`crate::d3d12::resource::Twin`]). Lists are recorded on many threads and run
/// in submission order, so a list cannot know which side an earlier list
/// leaves behind: it notes which sides it uses as it finds them, and its
/// prelude copies one side into the other at submission if that side is
/// behind then. From its first use of a side on, the list knows.
pub(super) struct TwinUse {
    /// The depth texture (the title resource's host copy) and its twin.
    pub(super) depth: std::sync::Arc<crate::d3d12::resource::HostTexture>,
    pub(super) twin: std::sync::Arc<crate::d3d12::resource::Twin>,
    /// The sides (depth texture, twin) the list uses as it finds them.
    pub(super) needs: [bool; 2],
    /// Whether each side lacks what the other was written last, where the
    /// list is now; `None`: as the list found it.
    pub(super) behind: [Option<bool>; 2],
    /// The draw or dispatch being prepared writes the twin.
    pub(super) writing: bool,
}

/// The pass that draws the twin of a depth plane into the plane: one
/// triangle over each mip of each slice, its pixel shader writing the
/// twin's texel as the depth, with the depth test "always" and no stencil
/// (the stencil plane is copied through a buffer and not touched here).
///
/// A 16-bit twin holds whole steps of 1/65535, and the depth written is
/// the step times the float nearest 1/65535 (0x37800080). That is how
/// The title's tiled lighting computes the depth of the
/// rectangles it then draws with the test "equal", one class of materials
/// a draw (all six of its vertex shaders multiply the class number by that
/// constant), so the pass and the draw give the host the same float, and
/// the host turns the same float into the same 24 bits. The step divided
/// by 65535 is another float for 512 of the 65,536 steps (none under 257),
/// and lands in another 24-bit step for 127 of them (none under 385).
impl Host {
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let before = private_bytes();
        let list = self.make_list();
        LISTS[0].fetch_add(1, Ordering::Relaxed);
        // (May shrink while a list is made, on another thread's account:
        // added as it is, the sum is what counts.)
        #[cfg(feature = "diagnostics")]
        LISTS[2].fetch_add(private_bytes().wrapping_sub(before), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            use crate::diagnostics::copy_stats::{C, add};
            add(C::HostLists, 1);
            add(C::HostListNs, started.elapsed().as_nanos() as u64);
        }
        list
    }

    /// An allocator and a closed list for a prelude.
    pub(in crate::backend) fn make_prelude(&self) -> Option<Prelude> {
        // SAFETY: plain object creation.
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator =
                    self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList =
                    self.device
                        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        match made {
            Ok(prelude) => Some(prelude),
            Err(e) => {
                tracing::warn!(error = %e, "no host list for a prelude");
                None
            }
        }
    }

    pub(in crate::backend) fn make_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude = OnceLock::new();
            if !lazy_prelude() {
                let _ = prelude.set(self.make_prelude());
            }
            #[cfg(feature = "diagnostics")]
            let notes = std::sync::Arc::<Mutex<Vec<Note>>>::default();
            let pipelines = std::sync::Arc::<Mutex<std::collections::BTreeSet<String>>>::default();
            #[cfg(feature = "diagnostics")]
            let indirects = dred_indirects::Shared::default();
            #[cfg(feature = "diagnostics")]
            if dred_on() || dred_light() {
                static NUMBER: AtomicU32 = AtomicU32::new(0);
                let number = NUMBER.fetch_add(1, Ordering::Relaxed);
                name(&list, || format!("victory host list {number}"));
                let mut all = LIST_NOTES.lock().unwrap_or_else(|e| e.into_inner());
                if number.is_multiple_of(256) {
                    all.retain(|_, l| l.0.strong_count() > 0);
                }
                all.insert(
                    number,
                    (
                        std::sync::Arc::downgrade(&notes),
                        std::sync::Arc::downgrade(&pipelines),
                        std::sync::Arc::downgrade(&indirects),
                    ),
                );
            }
            Ok(HostList {
                allocator,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                prelude,
                pipelines,
                #[cfg(feature = "diagnostics")]
                indirects,
                budget: OnceLock::new(),
                card_budget: OnceLock::new(),
                indirect_args: Mutex::default(),
                batch_args: Mutex::default(),
                coming: Mutex::default(),
                epoch: std::sync::atomic::AtomicU64::new(next_epoch()),
                state_clobbered: std::sync::atomic::AtomicBool::new(false),
                _alive: Counted::new(),
                #[cfg(feature = "diagnostics")]
                notes,
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }
}

impl HostList {
    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    pub(in crate::backend) fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: STAMPS, NodeMask: 0 },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    pub(in crate::backend) fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (ticks as f64 * 1e3 / frequency.max(1) as f64, notes[i].0.chars().take(300).collect())
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        let _closing = crate::diagnostics::timeline::span("closing a host list");
        self.twins_to_depth();
        self.textures_back();
        let scratch = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((scratch, _)) = scratch {
            self.indirect_common(&scratch);
        }
        self.batches_common();
        self.card_budget_out();
        self.take_coming();
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count = (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// `work` finds, on a thread of ours, refreshes that belong at this
    /// place among the list's steps; they are there when the list is closed.
    pub(crate) fn refreshes_coming(&self, work: impl FnOnce() -> Vec<heap_pass_async::Refresh> + Send + 'static) {
        self.take_coming();
        let at = self.deferred().copies.len();
        let coming = heap_pass_async::begin(work);
        *self.coming.lock().unwrap_or_else(|e| e.into_inner()) = Some((at, coming));
    }

    /// The refreshes that were still to come are put among the steps.
    pub(in crate::backend) fn take_coming(&self) {
        let Some((at, coming)) = self.coming.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let found = coming.wait();
        let mut deferred = self.deferred();
        let at = at.min(deferred.copies.len());
        deferred
            .copies
            .splice(at..at, found.into_iter().map(|(shadow, from, to)| Step::Refresh(shadow, from as usize, (to - from) as usize)));
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    pub(crate) fn reset(&self) {
        self.changed_something();
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let _resetting = crate::diagnostics::timeline::span("resetting a host list");
        self._alive.used();
        *self.coming.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.deferred() = Deferred::default();
        self.states.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.twins.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.state_clobbered.store(false, Ordering::Relaxed);
        self.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clear();
        #[cfg(feature = "diagnostics")]
        self.indirects.lock().unwrap_or_else(|e| e.into_inner()).clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
            // Every submission of the list has run: nothing reads its
            // scratch memory any more, and the next list to ask takes it.
            if scratch_pool() {
                give_chunks(std::mem::take(&mut scratch.chunks));
            }
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
        #[cfg(feature = "diagnostics")]
        crate::diagnostics::copy_stats::add(crate::diagnostics::copy_stats::C::ResetNs, started.elapsed().as_nanos() as u64);
    }

    /// Something changed in this list that bringing a draw's textures and
    /// buffers up to date depends on: a texture's state, a twin, what was
    /// refreshed.
    pub(crate) fn changed_something(&self) {
        self.epoch.store(next_epoch(), Ordering::Relaxed);
    }

    /// See [`HostList::changed_something`].
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Whether any deferred steps or texture state transitions have been
    /// recorded into this list since creation or its last [`HostList::reset`].
    pub(crate) fn has_recorded_work(&self) -> bool {
        !self.deferred().copies.is_empty()
            || !self.states.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }
}
