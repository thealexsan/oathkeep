/// Frames for which a slot a draw read through a table counts as a table's
/// slot ([`HostHeap::tabled`]).
pub(super) const TABLED_FRAMES: u64 = 240;

/// Frames a write of a table's slot waits in the queue for the draw that
/// reads it, before the slot is taken for one shaders pick by index.
/// Frames as `crate::d3d12::frame::current` counts them: in its opening cutscene
/// The title begins two or three of them per frame it presents
/// (435 begun in the 353 s of run 52's, for some 140 presented).
pub(super) const TABLED_GRACE: u64 = 8;

/// Frames for which a record copied into a slot a draw read through a
/// table is left to such a draw ([`HostHeap::copied_tabled`]).
pub(super) const COPIED_FRAMES: u32 = 8;

/// Frames a record copied into any other slot waits for one, before it is
/// taken for one shaders pick by index ([`HostHeap::copied_wait`]).
pub(super) const COPIED_GRACE: u64 = 4;

/// In [`HostHeap::tabled`]: a draw read the slot in that frame.
pub(super) const TABLED_READ: u32 = 1 << 31;

/// Since the start: writes of texture views into slots that count as a
/// table's (due later), and slots that left the queue because a draw read
/// them through a table.
pub(crate) static TABLED_WRITES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether slots draws read through tables leave the queue of textures
/// picked by index; `OATHKEEP_TABLED_TEXTURES=1` keeps them in it as
/// before.
pub(super) fn tabled_out() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TABLED_TEXTURES").is_none_or(|v| v != "1"))
}

/// What the queue of textures picked by index knows of a title slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Waiting {
    /// The frame from which the slot is due.
    pub(super) due: u64,
    /// The frame it was written in.
    pub(super) written: u64,
    /// Which write of the queue put it here: the pass hands it back with
    /// what it did, so a slot written again meanwhile stays as written.
    pub(super) write: u64,
    /// Looks in a row that found nothing to upload (its memory not mapped,
    /// or no more of it than at the last upload).
    pub(super) looks: u8,
    /// Bytes of its memory that were mapped when it was last uploaded
    /// without all of it there; 0: not uploaded yet.
    pub(super) mapped: u64,
    /// For a slot uploaded whole and looked at again
    /// ([`TextureQueue::recheck`]): where its memory is (address, bytes),
    /// the fingerprint it had at the upload, how many looks found it
    /// unchanged, and the frame of the upload. A fingerprint of 0: none.
    pub(super) checked: (u64, u64, u64, u8, u64),
}

/// The queue of title slots whose texture view waits to be brought up to
/// date for shaders that pick textures by index: which are due, and in
/// which order (no host heap needed, so it is tested without one).
#[derive(Default)]
pub(crate) struct TextureQueue {
    pub(super) slots: std::collections::HashMap<u32, Waiting>,
    /// Slots indexed by their `due` frame so `due` and `census` only inspect
    /// buckets with `due <= frame` instead of scanning all waiting slots.
    by_due: std::collections::BTreeMap<u64, std::collections::BTreeSet<u32>>,
    pub(super) writes: u64,
    /// Incremental count of slots with `mapped == 0 && looks > 0`.
    waiting_memory: u64,
    /// Incremental count of slots with `mapped > 0`.
    partly_mapped: u64,
    /// Slots held back because the card had no room for the host copy
    /// they need ([`TextureQueue::hold`]), each with the write it was
    /// held as.
    pub(super) held: std::collections::BTreeMap<u32, u64>,
}

impl TextureQueue {
    fn account_delta(&mut self, w: &Waiting, add: bool) {
        if w.mapped > 0 {
            if add {
                self.partly_mapped += 1;
            } else {
                self.partly_mapped -= 1;
            }
        } else if w.looks > 0 {
            if add {
                self.waiting_memory += 1;
            } else {
                self.waiting_memory -= 1;
            }
        }
    }

    fn due_insert(&mut self, due: u64, index: u32) {
        self.by_due.entry(due).or_default().insert(index);
    }

    fn due_remove(&mut self, due: u64, index: u32) {
        if let Some(bucket) = self.by_due.get_mut(&due) {
            bucket.remove(&index);
            if bucket.is_empty() {
                self.by_due.remove(&due);
            }
        }
    }

    fn insert_waiting(&mut self, index: u32, w: Waiting) {
        self.account_delta(&w, true);
        let new_due = w.due;
        if let Some(old) = self.slots.insert(index, w) {
            self.account_delta(&old, false);
            if old.due != new_due {
                self.due_remove(old.due, index);
                self.due_insert(new_due, index);
            }
        } else {
            self.due_insert(new_due, index);
        }
    }

    fn remove_waiting(&mut self, index: u32) -> Option<Waiting> {
        let old = self.slots.remove(&index)?;
        self.account_delta(&old, false);
        self.due_remove(old.due, index);
        Some(old)
    }

    fn update_waiting(&mut self, index: u32, write: u64, f: impl FnOnce(&mut Waiting)) {
        let Some(w) = self.slots.get_mut(&index).filter(|w| w.write == write) else {
            return;
        };
        let old_due = w.due;
        let old_partly = w.mapped > 0;
        let old_waiting = !old_partly && w.looks > 0;
        f(w);
        let new_due = w.due;
        let new_partly = w.mapped > 0;
        let new_waiting = !new_partly && w.looks > 0;
        if old_partly != new_partly {
            if new_partly {
                self.partly_mapped += 1;
            } else {
                self.partly_mapped -= 1;
            }
        }
        if old_waiting != new_waiting {
            if new_waiting {
                self.waiting_memory += 1;
            } else {
                self.waiting_memory -= 1;
            }
        }
        if new_due != old_due {
            self.due_remove(old_due, index);
            self.due_insert(new_due, index);
        }
    }

    /// A texture view was written into slot `index` in `frame`, due from
    /// frame `due`. `seen`: it was not written, a draw or the pass made
    /// the host slot from a view the queue may hold already, which then
    /// stays as it is.
    pub(super) fn written(&mut self, index: u32, frame: u64, due: u64, seen: bool) {
        if seen && self.slots.contains_key(&index) {
            return;
        }
        self.writes += 1;
        let write = self.writes;
        self.insert_waiting(index, Waiting { due, written: frame, write, looks: 0, mapped: 0, checked: (0, 0, 0, 0, 0) });
    }

    pub(super) fn remove(&mut self, index: u32) -> bool {
        self.remove_waiting(index).is_some()
    }

    /// Up to `max` slots due at `frame`, each with its write number, in
    /// the order to serve them:
    /// - first the ones looked at before (their memory was not mapped, or
    ///   only part of it): the look is a query, and memory that has
    ///   arrived since is what the title streamed in last;
    /// - then the others, the newest write first and the oldest next, in
    ///   turns: slot order served the low slots of a backlog of
    ///   25,000-40,000 while the textures of the shot on screen waited
    ///   (run 52: 98% of the drawn pixels had no colour texture in the
    ///   first 3D frames).
    pub(super) fn due(&self, frame: u64, max: usize) -> Vec<(u32, u64)> {
        let mut again: Vec<(u64, u32, u64)> = Vec::new();
        let mut fresh: Vec<(u64, u32, u64)> = Vec::new();
        for (_, bucket) in self.by_due.range(..=frame) {
            for &index in bucket {
                let Some(w) = self.slots.get(&index) else {
                    continue;
                };
                let class = if w.looks > 0 || w.mapped > 0 { &mut again } else { &mut fresh };
                class.push((w.written, index, w.write));
            }
        }
        // By the number of the write, counted over the run.
        let newest_first = |a: &(u64, u32, u64), b: &(u64, u32, u64)| b.2.cmp(&a.2);
        again.sort_unstable_by(newest_first);
        fresh.sort_unstable_by(newest_first);
        let mut order: Vec<(u32, u64)> = again.iter().map(|&(_, index, write)| (index, write)).collect();
        let (mut new, mut old) = (0, fresh.len());
        while new < old && order.len() < max {
            order.push((fresh[new].1, fresh[new].2));
            new += 1;
            if new < old && order.len() < max {
                old -= 1;
                order.push((fresh[old].1, fresh[old].2));
            }
        }
        order.truncate(max);
        order
    }

    /// Prioritized due slots for `heap_textures`, separating high-priority
    /// streaming (`partly` and `fresh`) from low-priority `recheck` and `away`
    /// probes so thousands of unmapped/recheck slots cannot starve new
    /// textures or blow the per-frame time budget. Returns `(ordered,
    /// priority_due_count)`.
    pub(super) fn due_prioritized(
        &self,
        frame: u64,
        max_partly: usize,
        max_fresh: usize,
        max_recheck: usize,
        max_away: usize,
    ) -> (Vec<(u32, u64)>, usize) {
        let total_due: usize = self.by_due.range(..=frame).map(|(_, b)| b.len()).sum();
        if total_due == 0 {
            return (Vec::new(), 0);
        }
        let scan_cap = if total_due > 4096 { 4096 } else { total_due };
        let mut partly: Vec<(u32, u64)> = Vec::new();
        let mut fresh: Vec<(u32, u64)> = Vec::new();
        let mut recheck: Vec<(u32, u64)> = Vec::new();
        let mut away: Vec<(u32, u64)> = Vec::new();
        let mut scanned = 0usize;
        'outer: for (_, bucket) in self.by_due.range(..=frame).rev() {
            for &index in bucket {
                if scanned >= scan_cap {
                    break 'outer;
                }
                scanned += 1;
                let Some(w) = self.slots.get(&index) else {
                    continue;
                };
                if w.mapped > 0 {
                    partly.push((index, w.write));
                } else if w.checked.2 != 0 {
                    recheck.push((index, w.write));
                } else if w.looks > 0 {
                    away.push((index, w.write));
                } else {
                    fresh.push((index, w.write));
                }
            }
        }
        let newest_first = |a: &(u32, u64), b: &(u32, u64)| b.1.cmp(&a.1);
        partly.sort_unstable_by(newest_first);
        fresh.sort_unstable_by(newest_first);
        recheck.sort_unstable_by(newest_first);
        away.sort_unstable_by(newest_first);
        let priority_due = partly.len() + fresh.len();

        let mut order = Vec::with_capacity(
            partly.len().min(max_partly)
                + fresh.len().min(max_fresh)
                + recheck.len().min(max_recheck)
                + away.len().min(max_away),
        );
        order.extend(partly.into_iter().take(max_partly));
        let (mut new, mut old) = (0, fresh.len());
        let fresh_limit = order.len() + max_fresh;
        while new < old && order.len() < fresh_limit {
            order.push(fresh[new]);
            new += 1;
            if new < old && order.len() < fresh_limit {
                old -= 1;
                order.push(fresh[old]);
            }
        }
        order.extend(recheck.into_iter().take(max_recheck));
        order.extend(away.into_iter().take(max_away));
        (order, priority_due)
    }

    /// Slot `index` is up to date: out of the queue until written again.
    pub(super) fn done(&mut self, index: u32, write: u64) {
        if self.slots.get(&index).is_some_and(|w| w.write == write) {
            self.remove_waiting(index);
        }
    }

    /// Nothing to upload at slot `index` now: looked at again after 1, 2,
    /// 4, ... frames, at most `most`.
    pub(super) fn later(&mut self, index: u32, write: u64, frame: u64, most: u64) {
        self.update_waiting(index, write, |w| {
            w.due = frame + (1u64 << w.looks.min(16)).min(most.max(1));
            w.looks = w.looks.saturating_add(1);
        });
    }

    /// The card has no room for the host copy slot `index` needs: it is
    /// not handed out again before frame `due`, or before the card has
    /// room again ([`TextureQueue::room`]). Nothing else of it changes: it
    /// is not a slot "waiting for its memory", and when it is due it has
    /// its old place among the slots never looked at. (It stayed due, so
    /// every pass on a full card looked at the same few hundred slots
    /// again and stopped there, and the slots behind them were never
    /// reached: the ones whose host copy exists and only wants bringing up
    /// to date, and the ones whose memory went, which is what lets host
    /// copies go and makes room.)
    pub(super) fn hold(&mut self, index: u32, write: u64, due: u64) {
        let mut held = false;
        self.update_waiting(index, write, |w| {
            w.due = w.due.max(due);
            held = true;
        });
        if held {
            self.held.insert(index, write);
        }
    }

    /// The card has room again at `frame`: the slots held back for lack of
    /// it are due now, as if they had never been held. Returns how many.
    pub(super) fn room(&mut self, frame: u64) -> usize {
        let held = std::mem::take(&mut self.held);
        let mut back = 0;
        for (index, write) in held {
            self.update_waiting(index, write, |w| {
                w.due = w.due.min(frame);
                back += 1;
            });
        }
        back
    }

    /// Slot `index` was uploaded with `mapped` bytes of its memory there,
    /// not all of it: looked at again from frame `due`.
    pub(super) fn partly(&mut self, index: u32, write: u64, due: u64, mapped: u64) {
        self.update_waiting(index, write, |w| {
            (w.due, w.looks, w.mapped) = (due, 0, mapped.max(1));
        });
    }

    /// Slot `index` was uploaded whole from memory `at` (address, bytes)
    /// with fingerprint `print`: looked at again from frame `due`, for a
    /// title that fills a texture's memory after it wrote the view (the
    /// `looks`-th such look; `uploaded` is the upload's frame).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn recheck(&mut self, index: u32, write: u64, due: u64, at: (u64, u64), print: u64, looks: u8, uploaded: u64) {
        self.update_waiting(index, write, |w| {
            (w.due, w.looks, w.mapped) = (due, 1, 0);
            w.checked = (at.0, at.1, print, looks, uploaded);
        });
    }

    /// What [`TextureQueue::recheck`] noted of slot `index`, if it is
    /// waiting for such a look.
    pub(super) fn checked(&self, index: u32) -> Option<(u64, u64, u64, u8, u64)> {
        self.slots.get(&index).map(|w| w.checked).filter(|c| c.2 != 0)
    }

    /// The memory slot `index`'s texture lies in was mapped, unmapped or
    /// written in `frame`: the slot is looked at from frame `due` as one
    /// written now, whatever was known of it (a pass at work on it does
    /// not take it out).
    pub(super) fn changed(&mut self, index: u32, frame: u64, due: u64) {
        self.writes += 1;
        let write = self.writes;
        let due = self.slots.get(&index).map_or(due, |w| w.due.min(due));
        self.insert_waiting(index, Waiting { due, written: frame, write, looks: 0, mapped: 0, checked: (0, 0, 0, 0, 0) });
    }

    /// Bytes of slot `index`'s memory that were mapped at its last upload
    /// (0: not uploaded, or all of it was).
    pub(super) fn mapped(&self, index: u32) -> u64 {
        self.slots.get(&index).map_or(0, |w| w.mapped)
    }

    /// For the frame line: slots in the queue; of them due at `frame`;
    /// waiting for their memory (looked at, never uploaded); uploaded in
    /// part; and the frames the oldest due one has waited since its write.
    pub(super) fn census(&self, frame: u64) -> [u64; 5] {
        let mut c = [self.slots.len() as u64, 0, self.waiting_memory, self.partly_mapped, 0];
        let mut sampled = 0usize;
        for (_, bucket) in self.by_due.range(..=frame) {
            c[1] += bucket.len() as u64;
            if sampled < 4096 {
                for &index in bucket {
                    if let Some(w) = self.slots.get(&index) {
                        c[4] = c[4].max(frame.saturating_sub(w.written));
                    }
                    sampled += 1;
                    if sampled >= 4096 {
                        break;
                    }
                }
            }
        }
        c
    }
}

