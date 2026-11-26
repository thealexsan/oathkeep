//! The recorder: what the host layer makes and records with D3D12, seen at
//! the D3D12 boundary (`super::hook`), and one frame of it written down
//! (`super::snap`). On with `OATHKEEP_RECORD=<folder>` only; without it
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

use super::hook::{self, method, noted_entry, orig};
use super::states::{self, Change, States};
use super::wire::{self, ALL, Barrier, Cmd, Loc, NONE, Part, Place, Slot, Va, Wire, slot};
use super::{gpu, pso, snap};

pub(super) type Ptr = *mut c_void;
pub(super) type Out = *mut *mut c_void;

/// What to record.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Captures go to `capture-<n>` in it.
    pub folder: PathBuf,
    pub at: At,
    /// Frames to capture.
    pub frames: u32,
    /// Record lists from the start on, not only once a capture is near: for
    /// a title that submits lists it recorded long before.
    pub always: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum At {
    /// Seconds after the device was made.
    Seconds(f64),
    /// A number of presents.
    Frame(u64),
    /// When a file named `go` appears in the folder (this works in the
    /// other cases too).
    Trigger,
}

impl Settings {
    /// From `OATHKEEP_RECORD`, `OATHKEEP_RECORD_AT` (seconds, or `f<frame>`),
    /// `OATHKEEP_RECORD_FRAMES` and `OATHKEEP_RECORD_ALWAYS=1`.
    pub fn from_env() -> Option<Settings> {
        let folder = PathBuf::from(std::env::var_os("OATHKEEP_RECORD").filter(|v| !v.is_empty())?);
        let at = parse_at(&std::env::var("OATHKEEP_RECORD_AT").unwrap_or_default());
        let frames = std::env::var("OATHKEEP_RECORD_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1);
        let always = std::env::var_os("OATHKEEP_RECORD_ALWAYS").is_some_and(|v| v == "1");
        Some(Settings {
            folder,
            at,
            frames: std::cmp::max(frames, 1),
            always,
        })
    }
}

fn parse_at(text: &str) -> At {
    let text = text.trim();
    if let Some(frame) = text.strip_prefix('f').and_then(|n| n.parse().ok()) {
        return At::Frame(frame);
    }
    match text.parse::<f64>() {
        Ok(s) => At::Seconds(s),
        Err(_) => At::Trigger,
    }
}

pub(in crate::replay) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(in crate::replay) fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

/// A resource that lives.
pub(in crate::replay) struct Res {
    pub(in crate::replay) id: u32,
    pub(in crate::replay) ptr: usize,
    pub(in crate::replay) place: Place,
    pub(in crate::replay) desc: wire::Desc,
    pub(in crate::replay) made_in: i32,
    pub(in crate::replay) clear: Option<[u32; 5]>,
    pub(in crate::replay) subs: u32,
    pub(in crate::replay) seen: gpu::Seen,
    /// Made while a frame was being captured.
    pub(in crate::replay) late: bool,
}

impl Res {
    pub(in crate::replay) fn is_buffer(&self) -> bool {
        self.desc.dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0
    }

    /// The object, without a reference of its own.
    pub(in crate::replay) fn object(&self) -> ManuallyDrop<ID3D12Resource> {
        borrow(self.ptr as Ptr)
    }
}

/// A COM object named by its address, without a reference.
pub(in crate::replay) fn borrow<T: Interface>(p: Ptr) -> ManuallyDrop<T> {
    // SAFETY: the caller's live object; never released here.
    ManuallyDrop::new(unsafe { T::from_raw(p) })
}

/// A descriptor heap: where it lies and what its slots hold.

struct Keeper<'a>(&'a Recorder);

impl pso::Keep for Keeper<'_> {
    fn root(&mut self, root: Ptr) -> u32 {
        self.0.object(root, "a root signature")
    }
    fn code(&mut self, code: &[u8]) -> u32 {
        self.0.code(code)
    }
    fn lost(&mut self, what: &str) {
        self.0.problem(what);
    }
}

/// Which object died.
enum Gone {
    Resource { id: u32, ptr: usize, va: u64 },
    List { ptr: usize },
    Heap { id: u32, ptr: usize },
}

/// A COM object an object of D3D12's holds and lets go of when it dies.
#[repr(C)]
struct Notice {
    methods: *const NoticeTable,
    references: AtomicU32,
    what: Gone,
}

#[repr(C)]
struct NoticeTable {
    query: unsafe extern "system" fn(Ptr, *const GUID, Out) -> HRESULT,
    add: unsafe extern "system" fn(Ptr) -> u32,
    release: unsafe extern "system" fn(Ptr) -> u32,
}
static NOTICE: NoticeTable = NoticeTable {
    query: notice_query,
    add: notice_add,
    release: notice_release,
};

unsafe extern "system" fn notice_query(this: Ptr, iid: *const GUID, out: Out) -> HRESULT {
    // SAFETY: COM's rules: a valid id and place for the answer.
    unsafe {
        if *iid == IUnknown::IID {
            notice_add(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    HRESULT(0x8000_4002_u32 as i32)
}

unsafe extern "system" fn notice_add(this: Ptr) -> u32 {
    // SAFETY: one of ours, alive while a reference is.
    unsafe {
        (*this.cast::<Notice>())
            .references
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}

unsafe extern "system" fn notice_release(this: Ptr) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Notice>())
            .references
            .fetch_sub(1, Ordering::AcqRel)
            - 1;
        if left == 0 {
            let notice = Box::from_raw(this.cast::<Notice>());
            if let Some(r) = recorder() {
                r.gone(&notice.what);
            }
        }
        left
    }
}

/// Starts the recorder if the environment asks for it, and hooks `device`.
/// Called right after the device was made.
pub(crate) fn from_env(device: &ID3D12Device) {
    let Some(settings) = Settings::from_env() else {
        return;
    };
    match start(settings, device) {
        Ok(folder) => tracing::info!(folder = %folder.display(), "frame recorder on"),
        Err(e) => tracing::warn!(error = %e, "frame recorder not started"),
    }
}

/// Starts the recorder (once a process) and hooks `device`, which must have
/// made nothing the frame will use. Where the capture will be.
pub fn start(settings: Settings, device: &ID3D12Device) -> std::io::Result<PathBuf> {
    let r = Recorder::new(settings)?;
    let folder = r.folder.clone();
    if RECORDER.set(r).is_err() {
        return Err(std::io::Error::other("the recorder is already on"));
    }
    let r = recorder().ok_or_else(|| std::io::Error::other("no recorder"))?;
    *lock(&r.device) = Some(device.clone());
    r.arm(r.settings.always);
    // The newer interfaces may be other pointers (they are the same one
    // without the debug layer).
    let mut all = vec![device.as_raw()];
    let mut more: Vec<IUnknown> = Vec::new();
    for iid in [
        ID3D12Device1::IID,
        ID3D12Device2::IID,
        ID3D12Device3::IID,
        ID3D12Device4::IID,
        ID3D12Device5::IID,
        ID3D12Device8::IID,
        ID3D12Device10::IID,
        ID3D12Device11::IID,
        ID3D12Device13::IID,
    ] {
        if let Some(p) = query(device.as_raw(), &iid) {
            all.push(p.as_raw());
            more.push(p);
        }
    }
    for p in all {
        // SAFETY: a live device; the entries below are of their methods'
        // types (the compiler checks them).
        unsafe { hook::install(p, std::ptr::null(), device_table) };
    }
    Ok(folder)
}

/// Unhooks `device` (tests).
pub fn unhook(device: &ID3D12Device) {
    // SAFETY: a live device, used by no other thread (the caller's word).
    unsafe { hook::remove(device.as_raw()) };
}

/// The object's interface `iid`.
pub(super) fn query(p: Ptr, iid: &GUID) -> Option<IUnknown> {
    let object = borrow::<IUnknown>(p);
    let mut out = std::ptr::null_mut();
    // SAFETY: a live COM object.
    unsafe {
        object.query(iid, &mut out).ok().ok()?;
        (!out.is_null()).then(|| IUnknown::from_raw(out))
    }
}

/// Names an object for reports.
pub fn name(object: &impl Interface, text: &str) {
    if let Some(r) = recorder() {
        r.name(object.as_raw(), text);
    }
}

/// At the end of a frame, before the swap chain's present: `texture`, in
/// `state`, is shown.
pub fn present(texture: &ID3D12Resource, state: D3D12_RESOURCE_STATES) {
    if let Some(r) = recorder() {
        snap::present(r, texture, state.0 as u32);
    }
}

/// Where the capture is, once it is done.
pub fn finished() -> Option<PathBuf> {
    let r = recorder()?;
    lock(&r.submits).done().then(|| r.folder.clone())
}

/// Sets a method of a table to a function of its type.
macro_rules! set {
    ($t:ident, $vtbl:ident . $name:ident = $f:expr) => {{
        // The compiler checks the function's type here.
        let _ = |v: &mut $vtbl| v.$name = $f;
        $t[method!($vtbl.$name)] = $f as *const () as usize;
    }};
}

/// The object's own method.
macro_rules! own {
    ($this:ident, $vtbl:ident . $name:ident) => {
        orig::<$vtbl>($this).$name
    };
}

/// Entries for methods that are not recorded: counted, and named in the
/// capture's notes if they were called.
macro_rules! not_recorded {
    ($t:ident, $class:ident, $names:ident: $($vtbl:ident . $name:ident),* $(,)?) => {
        const $names: &[(usize, &str)] = &[$((method!($vtbl.$name), stringify!($name))),*];
        $($t[method!($vtbl.$name)] = noted_entry::<{ hook::$class * hook::SLOTS + method!($vtbl.$name) }>();)*
    };
}


mod device_hooks;
mod list_hooks;
mod list_state_hooks;
mod methods;
mod resource_hooks;
mod state;
mod tables;
use tables::device_table;

pub(in crate::replay) use state::*;
pub(in crate::replay) use tables::not_recorded_calls;
pub use state::on;
pub(crate) use state::unrecorded;

#[cfg(test)]
mod tests;
