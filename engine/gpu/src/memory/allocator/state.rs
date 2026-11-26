use std::collections::HashMap;
use std::sync::Arc;

use oathkeep_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES, MEMORY_READ_ONLY};
use windows::Win32::Graphics::Direct3D12::*;

use super::{ARENA, BACK, COPIES, MOST, PAGE, SWEEP, WINDOWS, close, fill};
use crate::backend::host;
use crate::memory::shadow::Shadow;

/// A page of title memory in an arena.
pub(super) struct Held {
    /// Where in the arena's memory, and the arena's place in `copies`.
    pub(super) at: usize,
    pub(super) copy: usize,
    /// The write-watch asking after which it was last taken (0: never).
    pub(super) taken: u64,
    /// The frame it was last looked at in, and the one it came in.
    pub(super) looked: u64,
    pub(super) born: u64,
    /// Packed structures that have bytes in it, and whether it is held for
    /// them alone.
    pub(super) owners: u32,
    pub(super) theirs: bool,
}

/// A packed structure the builder came to.
pub(super) struct Packed {
    /// The frame it last did.
    pub(super) frame: u64,
    pub(super) len: u64,
    /// Its first two dwords in title memory.
    pub(super) header: u64,
}

pub(super) struct Memory {
    /// The CPU's entries (number, offset), and the copy shaders read.
    pub(super) directory: (*mut u32, Arc<Shadow>),
    pub(super) faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    pub(super) copies: Vec<Arc<Shadow>>,
    /// Places of title buffers' copies among `copies`.
    pub(super) numbers: HashMap<usize, usize>,
    /// Arenas: place in `copies`, address.
    pub(super) arenas: Vec<(usize, usize)>,
    /// By title page.
    pub(super) pages: HashMap<u64, Held>,
    /// Their numbers in the order they came, and where the sweep is.
    pub(super) order: Vec<u64>,
    pub(super) sweep: usize,
    /// Windows of title memory itself ([`Memory::map`]).
    pub(super) windows: usize,
    /// Packed structures the builder came to: the frame it last did.
    pub(super) packed: HashMap<u64, Packed>,
    /// Their addresses in the order they came, and where the sweep is.
    pub(super) packed_order: Vec<u64>,
    pub(super) packed_sweep: usize,
    /// Places of pages that are free: the copy, the address in the arena.
    pub(super) free: Vec<(usize, usize)>,
    /// Copies of title buffers the builder may store in.
    pub(super) open: std::collections::HashSet<usize>,
    /// Pages taken from faults.
    pub(super) faulted: usize,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    pub(super) heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    pub(super) frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    pub(super) refused: std::collections::HashSet<u64>,
    pub(super) stats: Stats,
    #[cfg(feature = "diagnostics")]
    pub(super) dump: super::dump::Dump,
    #[cfg(feature = "diagnostics")]
    pub(super) trace: super::trace::Trace,
}

/// Since the last report.
#[derive(Default)]
pub(super) struct Stats {
    pub(super) from: Option<(std::time::Instant, u64)>,
    pub(super) uses: [u64; 2],
    pub(super) faults: u64,
    pub(super) unknown: u64,
    pub(super) back: u64,
    /// Pages looked at, bytes that changed, packed structures sent.
    pub(super) looked: u64,
    pub(super) changed: u64,
    pub(super) packed: u64,
    pub(super) packed_bytes: u64,
    pub(super) gone: u64,
    /// Pages that came for an address in constants, a written view, another
    /// view, a structure named in upload memory.
    pub(super) came: [u64; 4],
    pub(super) micros: u64,
}

// SAFETY: the mappings are of memory the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

impl Memory {
    pub(super) fn new() -> Option<Memory> {
        let host = host()?;
        let bytes = u64::from(MEMORY_PAGES) * 8;
        // Zeroed, and watched for writes as the title's memory is.
        let entries = crate::memory::upload::reserve(bytes as usize)?;
        let directory = crate::memory::shadow::by_address(entries as u64, bytes)?;
        crate::backend::name(&directory.buffer, || "title memory by address: directory".into());
        let (faults, data) = host.shared_buffer(u64::from(MEMORY_FAULTS) * 4)?;
        crate::backend::name(&faults, || "title memory by address: faults".into());
        // SAFETY: a fresh mapping of at least that many bytes.
        unsafe { std::ptr::write_bytes(data, 0, MEMORY_FAULTS as usize * 4) };
        // SAFETY: a live buffer.
        let gpu = unsafe { faults.GetGPUVirtualAddress() };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: (entries as *mut u32, directory),
            faults: (faults, data.cast::<u32>(), gpu),
            copies: Vec::new(),
            numbers: HashMap::new(),
            arenas: Vec::new(),
            pages: HashMap::new(),
            order: Vec::new(),
            sweep: 0,
            windows: 0,
            packed: HashMap::new(),
            packed_order: Vec::new(),
            packed_sweep: 0,
            free: Vec::new(),
            open: Default::default(),
            faulted: 0,
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
            stats: Stats::default(),
            #[cfg(feature = "diagnostics")]
            dump: Default::default(),
            #[cfg(feature = "diagnostics")]
            trace: Default::default(),
        })
    }

    /// The directory's entry of `page`.
    pub(super) fn entry(&self, page: u64) -> *mut u32 {
        debug_assert!(page < u64::from(MEMORY_PAGES));
        // SAFETY: inside the directory.
        unsafe { self.directory.0.add(page as usize * 2) }
    }

    /// The place in `copies` of the copy that holds `page`, if one does.
    pub(super) fn copy_of(&self, page: u64) -> Option<usize> {
        if page >= u64::from(MEMORY_PAGES) {
            return None;
        }
        // SAFETY: an entry of the directory.
        ((unsafe { self.entry(page).read() } & !MEMORY_READ_ONLY) as usize).checked_sub(1)
    }

    /// Whether `page` can be given a place: in the directory, not turned down
    /// this frame, and the title's GPU memory. (A shader's pointer, or a
    /// constant taken for an address, may be anything.)
    pub(super) fn possible(&mut self, page: u64) -> bool {
        // Not the last entry: addresses past the directory read it, and it
        // stays empty.
        if page < 0x1_0000 || page >= u64::from(MEMORY_PAGES) - 1 || self.refused.contains(&page) {
            return false;
        }
        let at = (page * PAGE) as usize;
        let ours = crate::textures::texture::committed(at as *const u8, PAGE as usize)
            && crate::memory::watch::writes(at, at + PAGE as usize, true).is_some();
        if !ours {
            self.refused.insert(page);
            self.stats.unknown += 1;
        }
        ours
    }

    /// Gives a copy a number, if one is left.
    pub(super) fn number(&mut self, shadow: Arc<Shadow>) -> Option<usize> {
        let key = Arc::as_ptr(&shadow) as usize;
        if let Some(&n) = self.numbers.get(&key) {
            return Some(n);
        }
        if self.copies.len() >= COPIES as usize {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros"));
            return None;
        }
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return None;
        }
        self.copies.push(shadow);
        let n = self.copies.len() - 1;
        self.numbers.insert(key, n);
        Some(n)
    }

    /// Gives `page` a place, if it has none and the memory is the title's;
    /// whether it has one then.
    pub(super) fn map(&mut self, page: u64) -> bool {
        if self.copy_of(page).is_some() {
            return true;
        }
        if !self.possible(page) {
            return false;
        }
        let start = page * PAGE;
        if crate::memory::shadow::in_buffer(start, PAGE).is_some() {
            let Some(shadow) = crate::memory::shadow::find(start, PAGE) else {
                return false;
            };
            let fresh = !self.numbers.contains_key(&(Arc::as_ptr(&shadow) as usize));
            let Some(n) = self.number(shadow.clone()) else {
                return false;
            };
            // Every page the copy holds whole, not only the one asked for.
            // shortcut: a page of a title buffer inside this one that has a
            // copy of its own is given this copy too.
            if fresh {
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    let entry = self.entry(at / PAGE);
                    // SAFETY: an entry of the directory.
                    if unsafe { entry.read() } == 0 {
                        close(entry, n, at - first);
                    }
                    at += PAGE;
                }
            }
            // SAFETY: as above.
            if unsafe { self.entry(page).read() } == 0 {
                close(self.entry(page), n, start - shadow.span().0);
            }
            return true;
        }
        // Small title buffers in it: what the builder stores in them comes
        // back, so title memory itself.
        if self.windows < WINDOWS && crate::memory::shadow::small_buffer_in(start, PAGE, BACK) {
            if let Some(shadow) = crate::memory::shadow::by_address(start, PAGE)
                && let Some(n) = self.number(shadow)
            {
                self.windows += 1;
                fill(self.entry(page), n, 0);
                return true;
            }
            return false;
        }
        if self.pages.len() >= MOST {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(MOST, "title memory by address: no more pages are taken; others read zeros"));
            return false;
        }
        if let Some((copy, at)) = self.free.pop() {
            self.hold(page, at, copy);
            return true;
        }
        if !self.another() {
            return false;
        }
        let Some((copy, at)) = self.free.pop() else {
            return false;
        };
        self.hold(page, at, copy);
        true
    }

    /// Another arena's pages.
    pub(super) fn another(&mut self) -> bool {
        let Some(at) = crate::memory::upload::reserve(ARENA as usize) else {
            return false;
        };
        let Some(n) = crate::memory::shadow::by_address(at as u64, ARENA).and_then(|s| self.number(s)) else {
            return false;
        };
        self.arenas.push((n, at));
        // The lowest first.
        self.free.extend((0..(ARENA / PAGE) as usize).rev().map(|i| (n, at + i * PAGE as usize)));
        true
    }

    /// `page` is held at `at` in the arena that is `copy`.
    pub(super) fn hold(&mut self, page: u64, at: usize, copy: usize) {
        self.pages.insert(page, Held { at, copy, taken: 0, looked: u64::MAX, born: self.frame, owners: 0, theirs: false });
        self.order.push(page);
        fill(self.entry(page), copy, at as u64 - self.copies[copy].span().0);
    }

    /// The packed structure at `address` is gone: the pages it alone had are
    /// given back.
    pub(super) fn gone(&mut self, address: u64) {
        let Some(packed) = self.packed.remove(&address) else { return };
        for page in address / PAGE..=(address + packed.len - 1) / PAGE {
            let Some(held) = self.pages.get_mut(&page) else { continue };
            held.owners = held.owners.saturating_sub(1);
            if held.owners == 0 && held.theirs {
                self.free.push((held.copy, held.at));
                self.pages.remove(&page);
                // SAFETY: an entry of the directory, under the mutex.
                unsafe { self.entry(page).write_volatile(0) };
            }
        }
        self.stats.gone += 1;
    }

    /// Whether the packed structure at `address` is still in title memory.
    fn there(&self, address: u64) -> bool {
        let mut header = [0u8; 8];
        crate::textures::texture::committed(address as *const u8, 8) && {
            crate::backend::copy_mapped(header.as_mut_ptr(), address as *const u8, 8, false);
            self.packed.get(&address).is_some_and(|p| p.header == u64::from_le_bytes(header))
        }
    }

    /// Looks at some of the pages and packed structures again.
    pub(super) fn look_again(&mut self) {
        for _ in 0..self.order.len().div_ceil(SWEEP) {
            self.sweep = (self.sweep + 1) % self.order.len();
            let page = self.order[self.sweep];
            if self.pages.contains_key(&page) {
                self.bring(page, page + 1);
            } else {
                self.order.swap_remove(self.sweep);
                if self.order.is_empty() {
                    break;
                }
            }
        }
        for _ in 0..self.packed_order.len().div_ceil(SWEEP) {
            self.packed_sweep = (self.packed_sweep + 1) % self.packed_order.len();
            let address = self.packed_order[self.packed_sweep];
            if !self.packed.contains_key(&address) {
                self.packed_order.swap_remove(self.packed_sweep);
                if self.packed_order.is_empty() {
                    break;
                }
            } else if !self.there(address) {
                self.gone(address);
            }
        }
    }

    /// A dispatch of the builder was given `address` to write: if it is in a
    /// title buffer's copy, stores by address reach that from here on.
    pub(super) fn opens(&mut self, address: u64) {
        let Some(n) = self.map(address / PAGE).then(|| self.copy_of(address / PAGE)).flatten() else {
            return;
        };
        if self.pages.contains_key(&(address / PAGE)) || !self.open.insert(n) {
            return;
        }
        // (Its pages hold what the CPU writes beside the builder's scratch.)
        self.copies[n].keep_stores();
        let (first, len) = self.copies[n].span();
        tracing::info!(
            start = format_args!("{first:#x}"),
            len,
            "title memory by address: the structure builder may store in a title buffer"
        );
        for page in first.div_ceil(PAGE)..(first + len) / PAGE {
            if self.copy_of(page) == Some(n) {
                fill(self.entry(page), n, page * PAGE - first);
            }
        }
    }

    /// Every ten seconds or so, what was used.
    pub(super) fn report(&mut self) {
        let now = std::time::Instant::now();
        let (since, frame) = *self.stats.from.get_or_insert((now, self.frame));
        if now.duration_since(since).as_secs_f64() < 10.0 {
            return;
        }
        let frames = self.frame.wrapping_sub(frame).max(1) as f64;
        let s = std::mem::take(&mut self.stats);
        tracing::info!(
            "title memory by address: {} pages ({} MB) in {} arenas, {} copies in all; per frame {:.0} dispatches that read, {:.0} that build, \
             {:.1} pages that had no copy, {:.1} pages turned down, {:.1} results brought back; {:.0} pages looked at, {:.1} KB changed, \
             {:.1} packed structures ({} there), {:.2} ms; {} windows; since the last line {} KB of packed structures came and {} of them went, \
             and pages for constants {}, views written {}, others {}, upload memory {}, faults in all {}",
            self.pages.len(),
            (self.pages.len() as u64 * PAGE) >> 20,
            self.arenas.len(),
            self.copies.len(),
            s.uses[0] as f64 / frames,
            s.uses[1] as f64 / frames,
            s.faults as f64 / frames,
            s.unknown as f64 / frames,
            s.back as f64 / frames,
            s.looked as f64 / frames,
            s.changed as f64 / 1024.0 / frames,
            s.packed as f64 / frames,
            self.packed.len(),
            s.micros as f64 / 1000.0 / frames,
            self.windows,
            s.packed_bytes >> 10,
            s.gone,
            s.came[0],
            s.came[1],
            s.came[2],
            s.came[3],
            self.faulted
        );
        self.stats.from = Some((now, self.frame));
    }
}
