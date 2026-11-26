use super::staging::{Last, Staging, clamp_speculative_mips};
use super::{REFRESH_WAITS, UNLOCKED, committed, digest, mapped_bytes, waited, with_read};

/// Frames a guarded texture is hashed at every check from its first check,
/// and after the hash found a change the guard did not see.
const HOT_FRAMES: u64 = 64;

/// Whether a guarded texture whose memory the hash found changed although
/// the guard saw no write is hashed at every check for a while
/// ([`HOT_FRAMES`]), not every eighth frame.
fn hot_textures() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_HOT").is_none_or(|v| v != "0"))
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`OATHKEEP_TEXTURE_NO_WAIT=1`; off by
/// default).
pub(super) fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Whether a refresh that does not wait ([`no_wait`]) and finds a page
/// missing fills a staging buffer that was never filled from the pages that
/// are there (`OATHKEEP_TEXTURE_FILL=0`: it was left as it was made).
pub(super) fn fill_unlocked() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_FILL").is_none_or(|v| v != "0"))
}

/// Whether a texture all of whose memory is mapped is read through the
/// kernel's copy instead of in place (`OATHKEEP_TEXTURE_COPY=1`; off by
/// default).
fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

impl Staging {
    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let _texture = crate::timeline::span("a texture's refresh");
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        REFRESH_WAITS[3].fetch_add(whole.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        read
    }

    fn refresh_timed(&self) -> usize {
        let size = self.read;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        let exact = self.exact.swap(false, std::sync::atomic::Ordering::Relaxed);
        let alone = self.once.load(std::sync::atomic::Ordering::Relaxed);
        if alone && !exact && last.hash.is_some() {
            return 0;
        }
        if !alone && let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, false)) {
            if !exact && last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let mut blind = false;
        if !alone
            && *GUARD.get_or_init(|| std::env::var_os("OATHKEEP_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, true))
        {
            const HASH_EVERY: u64 = 8;
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let frame = crate::frame::current();
            let turn = frame
                .wrapping_add((start >> 16) as u64)
                .is_multiple_of(HASH_EVERY);
            if last.hot_until == 0 {
                last.hot_until = frame + HOT_FRAMES;
            }
            let hot = hot_textures() && frame < last.hot_until;
            if !exact && unwritten && !turn && !hot {
                return 0;
            }
            blind = unwritten;
        }
        let found_blind = |last: &mut Last| {
            if blind && hot_textures() {
                last.hot_until = crate::frame::current() + HOT_FRAMES;
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHot, 1);
            }
        };
        let _mapped = if no_wait() && !exact {
            let Some(mapped) = crate::watch::title_memory_reading_now() else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            waited(2, crate::watch::title_memory_reading)
        };
        if !committed(self.memory, size) {
            let frame = crate::frame::current();
            let partial_wait = if last.hash.is_none() || (hot_textures() && frame < last.hot_until) { 4 } else { 30 };
            if !exact && last.partial.is_some_and(|at| frame < at + partial_wait) {
                return 0;
            }
            last.partial = Some(frame);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            let changed = with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                let hash = digest(read);
                if last.hash == Some(hash) {
                    return false;
                }
                let all_zero = read.iter().all(|&b| b == 0);
                if all_zero && last.hash.is_none() {
                    return false;
                }
                last.hash = Some(hash);
                self.detile(read);
                !all_zero
            });
            #[cfg(feature = "diagnostics")]
            if changed {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            }
            if changed {
                found_blind(last);
            }
            return size;
        }
        if through_copy() {
            return with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                let hash = digest(read);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                last.hash = Some(hash);
                found_blind(last);
                self.detile(read);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        // SAFETY: committed memory of the texture's layout size, and it
        // stays committed while `_mapped` is held.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = digest(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        found_blind(last);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// A refresh while the runtime is remapping title memory ([`no_wait`]).
    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = digest(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else if last.hash.is_none() && fill_unlocked() {
            self.fill(last, size)
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    /// The first filling of the staging buffer while the runtime is
    /// remapping title memory, of a texture not all of which can be read.
    fn fill(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let start = self.memory as u64;
        let mut there = 0;
        let got = with_read(size, |read| {
            let mut got = 0;
            for _ in 0..4 {
                got = crate::host::read_there(self.memory, read) as u64;
                there = mapped_bytes(self.memory, size);
                if got >= there.max(self.expected.load(Relaxed)) {
                    break;
                }
                crate::watch::change_over();
            }
            if got != 0 {
                last.hash = Some(digest(read));
                self.detile(read);
            }
            got
        });
        last.partial = Some(crate::frame::current());
        UNLOCKED[2].fetch_add(1, Relaxed);
        if got < there.max(self.expected.load(Relaxed)) {
            UNLOCKED[3].fetch_add(1, Relaxed);
            last.asked = 0;
            crate::descriptor::host::read_short(start);
        } else {
            crate::descriptor::host::read_all(start);
        }
        size
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        let held = if self.on_demand {
            use std::sync::atomic::Ordering::Relaxed;
            let judged = self.judgement();
            self.judged[0].store(judged[0], Relaxed);
            self.judged[1].store(judged[1], Relaxed);
            let full_held = self.held_levels(judged);
            let held = clamp_speculative_mips(
                full_held,
                self.layout.first_tail_mip,
                self.layout.mips.len(),
                self.once.load(Relaxed),
                crate::residency::vram_level(),
            );
            self.clamped_mips.store(held != full_held, Relaxed);
            self.detiled.store(held, Relaxed);
            held
        } else {
            u32::MAX
        };
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = self.part(i);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = (pitch * rows as usize * f.Depth as usize)
                .min(self.bytes.saturating_sub(footprint.Offset) as usize);
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if mip < 32 && held & (1 << mip) == 0 {
                // Not the title's: zeros, not what lies in that memory.
                linear.fill(0);
                continue;
            }
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}
