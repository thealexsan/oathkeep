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

use super::*;

impl Recorder {
    pub(super) fn new(settings: Settings) -> std::io::Result<Recorder> {
        std::fs::create_dir_all(&settings.folder)?;
        let mut n = 1;
        let folder = loop {
            let folder = settings.folder.join(format!("capture-{n}"));
            match std::fs::create_dir(&folder) {
                Ok(()) => break folder,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e),
            }
        };
        let file = std::fs::File::create(folder.join("shaders.bin"))?;
        Ok(Recorder {
            settings,
            folder,
            began: std::time::Instant::now(),
            next: AtomicU32::new(1),
            ids: Ids::default(),
            resources: RwLock::default(),
            addresses: RwLock::default(),
            descriptors: RwLock::default(),
            lists: RwLock::default(),
            heaps: Mutex::default(),
            roots: Mutex::default(),
            pipelines: Mutex::default(),
            signatures: Mutex::default(),
            queries: Mutex::default(),
            names: Mutex::default(),
            codes: Mutex::new(Codes {
                file: Some(file),
                at: 0,
                index: HashMap::new(),
                all: Vec::new(),
            }),
            states: Mutex::default(),
            queues: Mutex::default(),
            device: Mutex::default(),
            armed: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            writes: Mutex::default(),
            held: Mutex::default(),
            problems: Mutex::default(),
            submits: Mutex::new(snapshot::Phase::default()),
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Notes something that is not in the capture.
    pub(in crate::replay) fn problem(&self, what: &str) {
        self.problem_times(what, 1);
    }

    pub(in crate::replay) fn problem_times(&self, what: &str, n: u64) {
        let mut problems = lock(&self.problems);
        let seen = problems.entry(what.to_owned()).or_insert(0);
        if *seen == 0 {
            tracing::warn!("frame recorder: {what}");
        }
        *seen += n;
    }

    /// The problems so far.
    pub(in crate::replay) fn problems(&self) -> Vec<String> {
        lock(&self.problems)
            .iter()
            .map(|(what, n)| {
                if *n == 1 {
                    what.clone()
                } else {
                    format!("{what} ({n} times)")
                }
            })
            .collect()
    }

    pub(in crate::replay) fn arm(&self, on: bool) {
        self.armed.store(on, Ordering::SeqCst);
    }

    pub(in crate::replay) fn device(&self) -> Option<ID3D12Device> {
        lock(&self.device).clone()
    }

    /// Whether a list was made through the hooked device (the swap chain may
    /// have others).
    pub(in crate::replay) fn knows(&self, p: Ptr) -> bool {
        read(&self.lists).contains_key(&(p as usize))
    }

    /// The state of a list, if its methods are still ours.
    pub(in crate::replay) fn list(&self, p: Ptr) -> Option<Arc<ListState>> {
        let state = read(&self.lists).get(&(p as usize)).cloned()?;
        // SAFETY: a live list: it is being submitted.
        if !unsafe { hook::ours(p) } {
            self.problem("a list lost its hooks");
            return None;
        }
        Some(state)
    }

    /// The buffer an address lies in.
    pub(in crate::replay) fn va(&self, address: u64) -> Va {
        if address == 0 {
            return Va::Null {};
        }
        match read(&self.addresses).range(..=address).next_back() {
            Some((&start, &(size, res))) if address - start < size => Va::At {
                res,
                offset: address - start,
            },
            _ => Va::Unknown { address },
        }
    }

    /// The id of an object, [`NONE`] for null; an unknown one is a problem.
    pub(super) fn object(&self, p: Ptr, what: &str) -> u32 {
        if p.is_null() {
            return NONE;
        }
        self.ids.get(p as usize).unwrap_or_else(|| {
            self.problem(&format!("{what} was used that was not seen made"));
            NONE
        })
    }

    /// The id of a resource; one not seen made (a swap chain's buffer) is
    /// taken in now.
    pub(in crate::replay) fn resource(&self, p: Ptr) -> u32 {
        if p.is_null() {
            return NONE;
        }
        if let Some(id) = self.ids.get(p as usize) {
            return id;
        }
        self.add(p, Place::BackBuffer {}, 0, None)
    }

    /// Takes a new resource in.
    pub(super) fn add(&self, p: Ptr, place: Place, made_in: i32, clear: Option<[u32; 5]>) -> u32 {
        let object = borrow::<ID3D12Resource>(p);
        // SAFETY: a live resource.
        let d = unsafe { object.GetDesc() };
        let desc = gpu::from_desc(&d);
        let id = self.id();
        let seen = match &place {
            Place::Committed { props, .. } => gpu::seen(props),
            Place::Placed { heap, .. } => lock(&self.heaps)
                .get(heap)
                .map_or(gpu::Seen::No, |h| gpu::seen(&h.props)),
            Place::BackBuffer {} => gpu::Seen::No,
        };
        let subs = gpu::subresources(&desc);
        let is_buffer = desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0;
        // SAFETY: a live buffer.
        let va = if is_buffer {
            unsafe { object.GetGPUVirtualAddress() }
        } else {
            0
        };
        let placed = matches!(place, Place::Placed { .. });
        let late = self.watching.load(Ordering::SeqCst);
        {
            let mut states = lock(&self.states);
            if !is_buffer {
                let decays = d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS.0 != 0;
                states.add(id, subs, made_in as u32, decays);
            }
            if placed {
                states.in_use(id);
            }
        }
        if va != 0 {
            write(&self.addresses).insert(va, (desc.width, id));
        }
        let res = Arc::new(Res {
            id,
            ptr: p as usize,
            place,
            desc,
            made_in,
            clear,
            subs,
            seen,
            late,
        });
        write(&self.resources).insert(id, res);
        self.ids.set(p as usize, id);
        if late {
            lock(&self.held).insert(id, ID3D12Resource::clone(&object));
        }
        self.tell_gone(
            &*object,
            Gone::Resource {
                id,
                ptr: p as usize,
                va,
            },
        );
        id
    }

    pub(super) fn gone(&self, what: &Gone) {
        match *what {
            Gone::Resource { id, ptr, va } => {
                // The memory goes whatever we hold: letting go of it later
                // would be a call into what is there then.
                if let Some(dead) = lock(&self.held).remove(&id) {
                    std::mem::forget(dead);
                    self.problem(
                        "a resource died as the frame started: its contents may be missing",
                    );
                }
                self.ids.remove(ptr, id);
                write(&self.resources).remove(&id);
                if va != 0 {
                    let mut all = write(&self.addresses);
                    if all.get(&va).is_some_and(|&(_, i)| i == id) {
                        all.remove(&va);
                    }
                }
                lock(&self.states).remove(id);
                lock(&self.names).remove(&id);
            }
            Gone::List { ptr } => {
                write(&self.lists).remove(&ptr);
            }
            Gone::Heap { id, ptr } => {
                self.ids.remove(ptr, id);
                write(&self.descriptors).remove(id);
            }
        }
    }

    /// Has us told when an object dies.
    pub(super) fn tell_gone(&self, object: &impl Interface, what: Gone) {
        const KEY: GUID = GUID::from_u128(0x3d0d5c1e_7a1b_4a53_9a3e_5f2b1e6b9c41);
        let notice = Box::into_raw(Box::new(Notice {
            methods: &NOTICE,
            references: AtomicU32::new(1),
            what,
        }));
        // SAFETY: a COM object of ours with one reference, which this drops
        // at the end; the object takes its own.
        unsafe {
            let notice = IUnknown::from_raw(notice.cast());
            let set = object
                .cast::<ID3D12Object>()
                .and_then(|o| o.SetPrivateDataInterface(&KEY, &notice));
            if set.is_err() {
                self.problem("an object's end cannot be followed");
                std::mem::forget(notice);
            }
        }
    }

    pub(super) fn view(&self, at: usize) -> Slot {
        match read(&self.descriptors).cpu(at) {
            Some((h, i)) => h.get(i),
            None => {
                self.problem("a descriptor handle in no heap seen made");
                Slot::EMPTY
            }
        }
    }

    pub(super) fn set_view(&self, at: usize, s: &Slot) {
        let all = read(&self.descriptors);
        let Some((h, i)) = all.cpu(at) else {
            drop(all);
            self.problem("a view was made in no descriptor heap seen made");
            return;
        };
        h.set(i, s);
        self.wrote(h, i, 1);
    }

    /// After slots were written: noted if a frame is being captured.
    pub(super) fn wrote(&self, h: &Heap, first: u32, n: u32) {
        if h.visible() && self.watching.load(Ordering::SeqCst) {
            lock(&self.writes).push((h.id, first, n));
        }
    }

    pub(super) fn copy_views(&self, to: usize, from: usize, n: u32) {
        let all = read(&self.descriptors);
        let (Some((a, i)), Some((b, j))) = (all.cpu(to), all.cpu(from)) else {
            drop(all);
            self.problem("descriptors were copied in no heap seen made");
            return;
        };
        for k in 0..n {
            a.set(i + k, &b.get(j + k));
        }
        self.wrote(a, i, n);
    }

    pub(super) fn code(&self, bytes: &[u8]) -> u32 {
        use std::io::Write;
        let hash = *blake3::hash(bytes).as_bytes();
        let mut codes = lock(&self.codes);
        if let Some(&i) = codes.index.get(&hash) {
            return i;
        }
        let i = codes.all.len() as u32;
        let at = codes.at;
        let wrote = codes
            .file
            .as_mut()
            .is_some_and(|f| f.write_all(bytes).is_ok());
        if !wrote {
            drop(codes);
            self.problem("shader code could not be written");
            return u32::MAX;
        }
        codes.at += bytes.len() as u64;
        codes.all.push(wire::Code {
            at,
            len: bytes.len() as u64,
            hash,
        });
        codes.index.insert(hash, i);
        i
    }

    pub(super) fn pipeline(&self, p: Ptr, parts: Vec<Part>) {
        let id = self.id();
        lock(&self.pipelines).insert(id, Arc::new(parts));
        self.ids.set(p as usize, id);
    }

    /// `name` is an object's, for reports.
    pub(super) fn name(&self, p: Ptr, name: &str) {
        if let Some(id) = self.ids.get(p as usize) {
            lock(&self.names).insert(id, name.to_owned());
        }
    }
}

