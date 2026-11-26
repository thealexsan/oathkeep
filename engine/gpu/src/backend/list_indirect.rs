use super::*;

impl HostList {
    pub(crate) fn indirect_scratch(&self, size: u64) -> Option<ID3D12Resource> {
        let needed = size.max(64 << 10);
        let mut slot = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buffer, cap)) = &*slot
            && *cap >= needed
        {
            return Some(buffer.clone());
        }
        let prev = slot.as_ref().map_or(0, |(_, cap)| *cap);
        let alloc = prev
            .saturating_mul(2)
            .max(needed)
            .next_multiple_of(64 << 10);
        let buffer = host()?.uav_buffer(alloc)?;
        name(&buffer, || format!("indirect arguments {alloc:#x}"));
        let old = slot.replace((buffer.clone(), alloc));
        drop(slot);
        if let Some((old_buf, _)) = old {
            // It ends the list common, as every scratch buffer does.
            self.indirect_common(&old_buf);
            self.keep(&old_buf);
        }
        Some(buffer)
    }

    /// Where the next batch of clamped records goes: a device-local buffer
    /// the list keeps from recording to recording, the batch's place in it
    /// and the state the buffer is in. Every batch of a recording has a place
    /// of its own: the executions before it may still read theirs.
    pub(crate) fn batch_place(&self) -> Option<(ID3D12Resource, u32, D3D12_RESOURCE_STATES)> {
        use crate::execution::indirect::batch::{JOBS, SLOT};
        let mut args = self.batch_args.lock().unwrap_or_else(|e| e.into_inner());
        let (buffer, i) = ((args.1 / BATCHES) as usize, args.1 % BATCHES);
        if buffer == args.0.len() {
            let size = u64::from(BATCHES * JOBS * SLOT);
            let made = host()?.uav_buffer(size)?;
            name(&made, || format!("indirect batches {size:#x}"));
            args.0.push(made);
        }
        args.1 += 1;
        // (Its first batch finds it as the recording before left it:
        // `batches_common`.)
        let state = if i == 0 { D3D12_RESOURCE_STATE_COMMON } else { D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT };
        Some((args.0[buffer].clone(), i * JOBS * SLOT, state))
    }

    /// Before the list closes: the scratch buffers of the batched clamps go
    /// back to the common state, where the next recording expects them.
    pub(in crate::backend) fn batches_common(&self) {
        let mut args = self.batch_args.lock().unwrap_or_else(|e| e.into_inner());
        let used = std::mem::take(&mut args.1).div_ceil(BATCHES) as usize;
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = args.0[..used]
            .iter()
            .map(|b| transition(b, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COMMON))
            .collect();
        if !barriers.is_empty() {
            // SAFETY: an open list and live buffers this recording left in
            // `INDIRECT_ARGUMENT`.
            unsafe { self.list.ResourceBarrier(&barriers) };
        }
    }

    /// Moves `scratch` back to the common state if a clamp left it readable
    /// as indirect arguments ([`HostList::indirect_readable`]): where the
    /// list's first clamp expects it. Done before the list closes and before
    /// a bigger scratch buffer replaces it: one barrier per list (where
    /// there was one per indirect execution), and the list ends as it
    /// begins however it is submitted.
    pub(in crate::backend) fn indirect_common(&self, scratch: &ID3D12Resource) {
        {
            let mut deferred = self.deferred();
            if deferred.indirect_readable != scratch.as_raw() as usize {
                return;
            }
            deferred.indirect_readable = 0;
        }
        let barrier = transition(
            scratch,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COMMON,
        );
        // SAFETY: an open list and a live buffer this recording left in
        // `INDIRECT_ARGUMENT`.
        unsafe { self.list.ResourceBarrier(&[barrier]) };
    }

    /// The state the list's indirect scratch buffer `scratch` is in at this
    /// point of the recording: indirect arguments if a clamp in this
    /// recording left it so ([`HostList::indirect_readable`]), otherwise
    /// common: the state a buffer is made in, and the one a recording
    /// leaves it in when it closes or replaces it ([`HostList::indirect_common`]).
    pub(crate) fn indirect_state(&self, scratch: &ID3D12Resource) -> D3D12_RESOURCE_STATES {
        if self.deferred().indirect_readable == scratch.as_raw() as usize {
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT
        } else {
            D3D12_RESOURCE_STATE_COMMON
        }
    }

    /// A clamp recorded in this list left `scratch` readable as indirect
    /// arguments. It stays so for every execution that reads it, until the
    /// next clamp moves it back to unordered access.
    pub(crate) fn indirect_readable(&self, scratch: &ID3D12Resource) {
        self.deferred().indirect_readable = scratch.as_raw() as usize;
    }

    /// Keeps `object` alive until the list is reset.
    pub(in crate::backend) fn run_deferred(&self, batch: &mut Batch, air: &[(usize, usize)], taken: impl Fn(usize) -> bool) {
        #[cfg(feature = "diagnostics")]
        use crate::diagnostics::copy_stats::{C, add};
        // The time and the bytes of the copies and fills, some 13,000 a
        // frame between 20,000 refreshes: summed here and added once.
        let (mut us, mut bytes) = (std::time::Duration::ZERO, 0u64);
        #[cfg(feature = "diagnostics")]
        let (mut copies, mut refreshes, mut refreshed) = (0u64, 0u64, 0u64);
        for (i, step) in self.deferred().copies.iter().enumerate() {
            if matches!(step, Step::Copy(..) | Step::Fill(..)) && taken(i) {
                continue;
            }
            let (started, copied) = match step {
                &Step::Copy(dst, src, len) => {
                    let started = std::time::Instant::now();
                    #[cfg(feature = "diagnostics")]
                    {
                        copies += 1;
                    }
                    batch.fill();
                    batch.flush();
                    copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    // With the submission's one asking: the journal learns it.
                    let aliases = crate::memory::watch::wrote(dst, len);
                    batch.invalidate_written(dst, len, aliases);
                    // What the card holds there may be another value although
                    // ours did not change.
                    if upload_snapshot::own() {
                        crate::memory::shadow::send_copied(dst, len);
                    }
                    (started, len)
                }
                &Step::Fill(dst, src, len, _) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        copies += 1;
                    }
                    if wide_fills() {
                        batch.fills.push((dst, src, len));
                        bytes += len as u64;
                        continue;
                    }
                    let started = std::time::Instant::now();
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    (started, len)
                }
                Step::Placed(p) => {
                    let started = std::time::Instant::now();
                    let free = !touches(air, p.src, p.src + p.len) && !touches(air, p.dst, p.dst + p.len);
                    batch.fill();
                    if free {
                        batch.flush();
                    }
                    if let Some(bytes) = p.prepare(free) {
                        batch.placed.push((p.shadow.clone(), p.dst, bytes));
                    }
                    if free {
                        batch.invalidate_written(p.dst, p.len, crate::memory::watch::has_aliases(p.dst, p.len));
                    }
                    (started, p.len)
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        refreshes += 1;
                        refreshed += *len as u64;
                    }
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work) | Step::Host(work, _) => {
                    batch.fill();
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    let _step = crate::diagnostics::timeline::span("a step of ours");
                    work();
                    // It wrote title memory, where is not known here: the
                    // submission's one asking ends, later refreshes ask anew.
                    if matches!(step, Step::Run(_)) {
                        crate::memory::watch::ask_once_end();
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    // The shaders start from the title's data: this copy's
                    // pending refresh first. The other copies' wait for
                    // the next flush of all (flushing all of them at every
                    // range a dispatch writes made thousands of write-watch
                    // askings a frame).
                    batch.flush_one(shadow);
                    #[cfg(feature = "diagnostics")]
                    let snapshot = std::time::Instant::now();
                    shadow.writing(*address, *len);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::GpuWrites, 1);
                        add(C::SnapshotUs, snapshot.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::Texture(staging) => {
                    if batch.texture_due(staging) {
                        batch.due.push(staging.clone());
                        if !wide_textures() {
                            batch.refresh_textures();
                        }
                    }
                    continue;
                }
                Step::Plane((_, len), fill) => {
                    let started = std::time::Instant::now();
                    fill();
                    PREPARING[2].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    PREPARING[3].fetch_add(*len as u64, Ordering::Relaxed);
                    continue;
                }
            };
            us += started.elapsed();
            bytes += copied as u64;
        }
        PREPARING[0].fetch_add(us.as_micros() as u64, Ordering::Relaxed);
        PREPARING[1].fetch_add(bytes, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            add(C::Copies, copies);
            add(C::CopyBytes, bytes);
            add(C::Refreshes, refreshes);
            add(C::RefreshBytes, refreshed);
            SPLIT[7].fetch_add(us.as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

