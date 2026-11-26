//! The recorder: what the host layer makes and records with D3D12, seen at
//! the D3D12 boundary (`super::super::hook`), and one frame of it written down
//! (`super::super::snapshot`). On with `OATHKEEP_RECORD=<folder>` only; without it
//! nothing is hooked.
//!
//! From the device's creation on it keeps what objects were made from and
//! what each descriptor slot holds, and follows the states of textures.
//! Lists are recorded only while it is armed: a few frames ahead of the one
//! captured, since the title records lists before they run.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR};

use super::super::hook::{self, method, noted_entry, orig};
use super::super::states::{self, Change, States};
use super::super::wire::{self, ALL, Barrier, Cmd, Loc, NONE, Part, Place, Slot, Va, Wire, slot};
use super::super::{gpu, pso, snapshot};

use super::{At, Ptr, Res, Settings, lock, read, write};

pub(in crate::replay) struct Heap {
    pub(in crate::replay) id: u32,
    pub(in crate::replay) kind: i32,
    pub(in crate::replay) count: u32,
    pub(in crate::replay) flags: i32,
    pub(super) cpu: usize,
    pub(super) gpu: u64,
    pub(super) step: usize,
    pub(super) slots: *mut Slot,
}

// SAFETY: slots are written by the thread that writes the descriptor (the
// same rule as the descriptors': one writer at a time) and read as plain
// data.
unsafe impl Send for Heap {}
// SAFETY: as above.
unsafe impl Sync for Heap {}

impl Heap {
    pub(super) fn new(id: u32, desc: &D3D12_DESCRIPTOR_HEAP_DESC, cpu: usize, gpu: u64, step: usize) -> Heap {
        let layout = Self::layout(desc.NumDescriptors);
        // SAFETY: a layout of at least one slot; a zeroed slot is an empty
        // one.
        let slots = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<Slot>();
        if slots.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Heap {
            id,
            kind: desc.Type.0,
            count: desc.NumDescriptors,
            flags: desc.Flags.0,
            cpu,
            gpu,
            step,
            slots,
        }
    }

    pub(super) fn layout(count: u32) -> std::alloc::Layout {
        std::alloc::Layout::array::<Slot>(count.max(1) as usize)
            .unwrap_or(std::alloc::Layout::new::<Slot>())
    }

    pub(in crate::replay) fn visible(&self) -> bool {
        self.flags & D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE.0 != 0
    }

    pub(in crate::replay) fn get(&self, i: u32) -> Slot {
        if i >= self.count {
            return Slot::EMPTY;
        }
        // SAFETY: a slot of ours.
        unsafe { self.slots.add(i as usize).read_volatile() }
    }

    pub(super) fn set(&self, i: u32, s: &Slot) {
        if i < self.count {
            // SAFETY: a slot of ours.
            unsafe { self.slots.add(i as usize).write_volatile(*s) };
        }
    }

    /// All slots, as they are now.
    pub(in crate::replay) fn all(&self) -> Vec<Slot> {
        let mut out = Vec::with_capacity(self.count as usize);
        // SAFETY: `count` slots of ours.
        unsafe {
            std::ptr::copy_nonoverlapping(self.slots, out.as_mut_ptr(), self.count as usize);
            out.set_len(self.count as usize);
        }
        out
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // SAFETY: as allocated.
        unsafe { std::alloc::dealloc(self.slots.cast(), Self::layout(self.count)) };
    }
}

#[derive(Default)]
pub(in crate::replay) struct Heaps {
    pub(super) by_cpu: BTreeMap<usize, Arc<Heap>>,
    pub(super) by_gpu: BTreeMap<u64, Arc<Heap>>,
}

impl Heaps {
    pub(super) fn add(&mut self, heap: Arc<Heap>) {
        let end = heap.cpu + heap.count as usize * heap.step;
        let gone: Vec<usize> = self
            .by_cpu
            .range(..end)
            .rev()
            .take_while(|(_, h)| h.cpu + h.count as usize * h.step > heap.cpu)
            .map(|(&k, _)| k)
            .collect();
        for k in gone {
            self.by_cpu.remove(&k);
        }
        if heap.visible() {
            self.by_gpu.insert(heap.gpu, heap.clone());
        }
        self.by_cpu.insert(heap.cpu, heap);
    }

    pub(super) fn remove(&mut self, id: u32) {
        self.by_cpu.retain(|_, h| h.id != id);
        self.by_gpu.retain(|_, h| h.id != id);
    }

    pub(super) fn cpu(&self, at: usize) -> Option<(&Arc<Heap>, u32)> {
        let (_, h) = self.by_cpu.range(..=at).next_back()?;
        let i = (at - h.cpu) / h.step;
        (i < h.count as usize).then_some((h, i as u32))
    }

    pub(super) fn gpu(&self, at: u64) -> Option<(&Arc<Heap>, u32)> {
        let (_, h) = self.by_gpu.range(..=at).next_back()?;
        let i = (at - h.gpu) / h.step as u64;
        (i < u64::from(h.count)).then_some((h, i as u32))
    }

    pub(in crate::replay) fn visible(&self) -> impl Iterator<Item = &Arc<Heap>> {
        self.by_gpu.values()
    }
}

/// A command list's recording.
pub(in crate::replay) struct ListState {
    pub(in crate::replay) kind: i32,
    pub(super) inner: Mutex<ListInner>,
}

#[derive(Default)]
pub(in crate::replay) struct ListInner {
    /// Recorded from its reset on.
    pub(super) whole: bool,
    pub(super) bytes: Vec<u8>,
    pub(super) changes: Vec<Change>,
    pub(super) closed: Option<(Option<Arc<Vec<u8>>>, Arc<Vec<Change>>)>,
}

impl ListState {
    pub(super) fn push(&self, cmd: &Cmd) {
        let mut inner = lock(&self.inner);
        if inner.whole {
            cmd.put(&mut inner.bytes);
        }
    }

    /// The recording closed last: its commands if it was recorded whole,
    /// and the states it changes. None if it was not seen closed since its
    /// reset.
    pub(in crate::replay) fn closed(&self) -> Option<(Option<Arc<Vec<u8>>>, Arc<Vec<Change>>)> {
        lock(&self.inner).closed.clone()
    }
}

pub(in crate::replay) struct QueueState {
    pub(super) kind: i32,
}

/// Shader code, written to `shaders.bin` as it is met, once each.
pub(in crate::replay) struct Codes {
    pub(super) file: Option<std::fs::File>,
    pub(super) at: u64,
    pub(super) index: HashMap<[u8; 32], u32>,
    pub(in crate::replay) all: Vec<wire::Code>,
}

/// Objects by their address: ids.
#[derive(Default)]
pub(in crate::replay) struct Ids([RwLock<HashMap<usize, u32>>; 32]);

impl Ids {
    pub(super) fn shard(&self, p: usize) -> &RwLock<HashMap<usize, u32>> {
        &self.0[(p >> 5 ^ p >> 12) % 32]
    }
    pub(super) fn get(&self, p: usize) -> Option<u32> {
        read(self.shard(p)).get(&p).copied()
    }
    pub(super) fn set(&self, p: usize, id: u32) {
        write(self.shard(p)).insert(p, id);
    }
    pub(super) fn remove(&self, p: usize, id: u32) {
        let mut map = write(self.shard(p));
        if map.get(&p) == Some(&id) {
            map.remove(&p);
        }
    }
}

pub(in crate::replay) struct Recorder {
    pub(in crate::replay) settings: Settings,
    /// Where this process' capture goes.
    pub(in crate::replay) folder: PathBuf,
    pub(in crate::replay) began: std::time::Instant,
    pub(super) next: AtomicU32,
    pub(super) ids: Ids,
    pub(in crate::replay) resources: RwLock<HashMap<u32, Arc<Res>>>,
    /// Buffers by their GPU address: size and id.
    pub(super) addresses: RwLock<BTreeMap<u64, (u64, u32)>>,
    pub(in crate::replay) descriptors: RwLock<Heaps>,
    pub(super) lists: RwLock<HashMap<usize, Arc<ListState>>>,
    pub(in crate::replay) heaps: Mutex<HashMap<u32, wire::Heap>>,
    pub(in crate::replay) roots: Mutex<HashMap<u32, Arc<Vec<u8>>>>,
    pub(in crate::replay) pipelines: Mutex<HashMap<u32, Arc<Vec<Part>>>>,
    pub(in crate::replay) signatures: Mutex<HashMap<u32, wire::Signature>>,
    pub(in crate::replay) queries: Mutex<HashMap<u32, wire::QueryHeap>>,
    pub(in crate::replay) names: Mutex<HashMap<u32, String>>,
    pub(in crate::replay) codes: Mutex<Codes>,
    pub(in crate::replay) states: Mutex<States>,
    pub(in crate::replay) queues: Mutex<Vec<(ID3D12CommandQueue, i32)>>,
    pub(super) device: Mutex<Option<ID3D12Device>>,
    /// Lists are recorded.
    pub(super) armed: AtomicBool,
    /// A frame is being captured: descriptor writes are noted, resources
    /// made are kept alive.
    pub(in crate::replay) watching: AtomicBool,
    pub(in crate::replay) writes: Mutex<Vec<(u32, u32, u32)>>,
    /// Resources kept alive for a capture, by id. One that dies all the same
    /// (its last reference was going when ours was taken) is taken out: see
    /// `Recorder::gone`.
    pub(in crate::replay) held: Mutex<HashMap<u32, ID3D12Resource>>,
    /// What could not be recorded, with how often.
    pub(super) problems: Mutex<BTreeMap<String, u64>>,
    /// Submissions and presents, one at a time.
    pub(in crate::replay) submits: Mutex<snapshot::Phase>,
}

pub(super) static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// The recorder, if it is on.
pub(in crate::replay) fn recorder() -> Option<&'static Recorder> {
    RECORDER.get()
}

/// Whether the recorder is on.
pub fn on() -> bool {
    RECORDER.get().is_some()
}

thread_local! {
    static OURS: Cell<u32> = const { Cell::new(0) };
}

/// While it lives, this thread's D3D12 calls are our own: not recorded.
pub(in crate::replay) struct Ours;

impl Ours {
    pub(in crate::replay) fn new() -> Ours {
        OURS.with(|o| o.set(o.get() + 1));
        Ours
    }
}

impl Drop for Ours {
    fn drop(&mut self) {
        OURS.with(|o| o.set(o.get() - 1));
    }
}

/// While what this returns lives, this thread's D3D12 calls are not
/// recorded.
pub(crate) fn unrecorded() -> impl Drop {
    Ours::new()
}

pub(super) fn ours() -> bool {
    OURS.with(|o| o.get() > 0)
}

