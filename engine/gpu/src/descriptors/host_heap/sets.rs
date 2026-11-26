use std::sync::Mutex;

/// A set of a heap's slots, a bit for each: no lock (a pass that went through
/// a set of 70,000 under its lock kept everyone who writes slots waiting for
/// 5 ms of a frame, timeline thin-6).
pub(super) struct Slots(Vec<std::sync::atomic::AtomicU64>);

impl Slots {
    pub(super) fn new(count: usize) -> Self {
        let mut bits = Vec::new();
        bits.resize_with(count.div_ceil(64), Default::default);
        Slots(bits)
    }

    pub(super) fn put(&self, slot: u32) {
        if let Some(bits) = self.0.get(slot as usize / 64) {
            bits.fetch_or(1 << (slot % 64), std::sync::atomic::Ordering::AcqRel);
        }
    }

    /// Whether it was there.
    pub(super) fn remove(&self, slot: u32) -> bool {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let bit = 1 << (slot % 64);
        self.0.get(slot as usize / 64).is_some_and(|bits| {
            bits.load(Acquire) & bit != 0 && bits.fetch_and(!bit, AcqRel) & bit != 0
        })
    }

    /// Whether `slot` is in the set.
    #[inline]
    pub(super) fn contains(&self, slot: u32) -> bool {
        self.0.get(slot as usize / 64).is_some_and(|bits| {
            bits.load(std::sync::atomic::Ordering::Acquire) & (1 << (slot % 64)) != 0
        })
    }

    fn of(n: usize, mut bits: u64, out: &mut Vec<u32>, which: &impl Fn(u32) -> bool) {
        while bits != 0 {
            let slot = n as u32 * 64 + bits.trailing_zeros();
            bits &= bits - 1;
            if which(slot) {
                out.push(slot);
            }
        }
    }

    /// Those that `which` says, in order.
    pub(super) fn all(&self, which: impl Fn(u32) -> bool) -> Vec<u32> {
        let mut out = Vec::new();
        for (n, bits) in self.0.iter().enumerate() {
            let word = bits.load(std::sync::atomic::Ordering::Acquire);
            if word != 0 {
                Self::of(n, word, &mut out, &which);
            }
        }
        out
    }

    /// Those with `slot % parts == part`, in order.
    pub(super) fn all_mod(&self, part: u32, parts: u32) -> Vec<u32> {
        if parts != 0 && 64 % parts == 0 && part < parts {
            let mut mask = 0u64;
            let mut bit = part;
            while bit < 64 {
                mask |= 1u64 << bit;
                bit += parts;
            }
            let mut out = Vec::new();
            for (n, bits) in self.0.iter().enumerate() {
                let mut masked = bits.load(std::sync::atomic::Ordering::Acquire) & mask;
                while masked != 0 {
                    let slot = n as u32 * 64 + masked.trailing_zeros();
                    masked &= masked - 1;
                    out.push(slot);
                }
            }
            out
        } else {
            self.all(|slot| slot % parts == part)
        }
    }

    /// All of them, and none is left.
    pub(super) fn take(&self) -> Vec<u32> {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let mut out = Vec::new();
        for (n, bits) in self.0.iter().enumerate() {
            if bits.load(Acquire) != 0 {
                Self::of(n, bits.swap(0, AcqRel), &mut out, &|_| true);
            }
        }
        out
    }
}

/// The record each slot of a heap was last made from, and a lock for every
/// few hundredth of the slots: whoever makes a slot holds its lock, and no
/// other.
///
/// One lock for the heap was what everything of a frame waited for once the
/// lists were translated by threads of ours: half of what those did, two
/// thirds of the heaps' records and half of the pass over the buffer views
/// (profile thin-2).
pub(super) struct Made {
    keys: Vec<std::sync::atomic::AtomicU64>,
    locks: [Mutex<()>; Self::LOCKS],
}

impl Made {
    const LOCKS: usize = 512;

    pub(super) fn new(count: usize) -> Self {
        let mut keys = Vec::new();
        keys.resize_with(count, Default::default);
        Made { keys, locks: [const { Mutex::new(()) }; Self::LOCKS] }
    }

    /// The lock slot `index` is made under. Neighbours have different ones.
    pub(super) fn lock(&self, index: u32) -> std::sync::MutexGuard<'_, ()> {
        self.locks[index as usize % Self::LOCKS].lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn get(&self, index: u32) -> Option<u64> {
        Some(self.keys.get(index as usize)?.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Under the slot's lock.
    pub(super) fn set(&self, index: u32, key: u64) {
        if let Some(k) = self.keys.get(index as usize) {
            k.store(key, std::sync::atomic::Ordering::Release);
        }
    }
}

