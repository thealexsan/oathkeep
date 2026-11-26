// The audio coprocessor.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use super::super::xasync::E_INVALIDARG;
use super::super::xgameruntime::{HRESULT, S_OK};
use super::heap::heap_unmap;
use super::xma::xma_command;
use super::{E_FAIL, lock};

const LOAD_FLOWGRAPH: u32 = 1;
pub(super) const REGISTER_MESSAGES: u32 = 2;
const UNREGISTER_MESSAGES: u32 = 3;

const FLOWGRAPH_COMPLETED: u32 = 0x02;
pub(super) const COMMAND_COMPLETED: u32 = 0x10;
const DISCONNECTED: u32 = 0x80;

/// Audio frames of 128 samples at 48 kHz.
const FRAMES_PER_SECOND: u64 = 375;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(super) struct Message {
    pub(super) kind: u32,
    pub(super) dropped: u32,
    pub(super) usec: u32,
    pub(super) data: [u8; 16],
}

struct State {
    connected: bool,
    /// Message kinds the title registered for.
    mask: u32,
    messages: VecDeque<Message>,
    /// Queue size given to `connect`.
    capacity: usize,
    /// Messages lost to a full queue since the last one queued.
    dropped: u32,
    since: Instant,
}

impl State {
    fn usec(&self) -> u64 {
        self.since.elapsed().as_micros() as u64
    }

    fn frame(&self) -> u32 {
        (self.usec() * FRAMES_PER_SECOND / 1_000_000) as u32
    }

    /// Queues a registered message; a full queue drops it and counts it in
    /// the next one that fits.
    fn post(&mut self, kind: u32, data: &[u8]) {
        if self.mask & kind == 0 {
            return;
        }
        if self.messages.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            tracing::debug!(kind, dropped = self.dropped, "coprocessor message dropped");
            return;
        }
        let mut message = Message {
            kind,
            dropped: std::mem::take(&mut self.dropped),
            usec: self.usec() as u32,
            data: [0; 16],
        };
        message.data[..data.len()].copy_from_slice(data);
        self.messages.push_back(message);
    }
}

/// The interface object: its method table, then our state.
#[repr(C)]
pub struct Acp {
    pub(super) vtable: &'static AcpMethods,
    state: Mutex<State>,
}

#[repr(C)]
pub(super) struct AcpMethods {
    pub(super) connect: extern "system" fn(*mut Acp, u32, u32) -> HRESULT,
    pub(super) disconnect: extern "system" fn(*mut Acp) -> HRESULT,
    pub(super) submit: extern "system" fn(*mut Acp, u32, u64, u32, *const c_void, u32) -> HRESULT,
    pub(super) pop_message: extern "system" fn(*mut Acp, *mut Message) -> bool,
    pub(super) message_count: extern "system" fn(*mut Acp) -> u32,
    pub(super) release: extern "system" fn(*mut Acp),
}

static ACP_METHODS: AcpMethods = AcpMethods {
    connect,
    disconnect,
    submit,
    pop_message,
    message_count,
    release,
};

fn state(this: *mut Acp) -> MutexGuard<'static, State> {
    // SAFETY: title code calls methods on the object `AcpHalCreate` made,
    // which lives until `release`.
    lock(unsafe { &(*this).state })
}

extern "system" fn connect(this: *mut Acp, commands: u32, messages: u32) -> HRESULT {
    if commands == 0 {
        return E_INVALIDARG;
    }
    let mut state = state(this);
    state.connected = true;
    state.capacity = messages as usize;
    S_OK
}

extern "system" fn disconnect(this: *mut Acp) -> HRESULT {
    let mut state = state(this);
    if !state.connected {
        return E_FAIL;
    }
    state.post(DISCONNECTED, &[]);
    state.connected = false;
    S_OK
}

/// Done at once: message registrations take effect, a flowgraph completes
/// unprocessed, a command with an id is acknowledged, and its notification
/// word is set.
extern "system" fn submit(
    this: *mut Acp,
    command: u32,
    id: u64,
    _frame: u32,
    data: *const c_void,
    notification: u32,
) -> HRESULT {
    static SEEN: Mutex<u64> = Mutex::new(0);
    if command < 64 && *lock(&SEEN) & 1 << command == 0 {
        *lock(&SEEN) |= 1 << command;
        tracing::info!(command, "first coprocessor command of this kind");
    }
    let mut state = state(this);
    let word = |at: usize| -> Option<u32> {
        // SAFETY: the command's data starts with these u32 fields.
        (!data.is_null()).then(|| unsafe { data.cast::<u32>().add(at).read_unaligned() })
    };
    match command {
        REGISTER_MESSAGES => state.mask |= word(0).unwrap_or(0),
        UNREGISTER_MESSAGES => state.mask &= !word(0).unwrap_or(0),
        LOAD_FLOWGRAPH => {
            let flowgraph = word(1).unwrap_or(0);
            state.post(FLOWGRAPH_COMPLETED, &flowgraph.to_le_bytes());
        }
        _ if xma_command(command, word) => {}
        // shortcut: context updates and the other SHAPE units' commands are
        // acknowledged but not carried out; add them when a title sends
        // them (each kind is logged once above).
        _ => {}
    }
    if id != 0 {
        let mut done = [0u8; 16];
        done[..4].copy_from_slice(&command.to_le_bytes());
        done[4..12].copy_from_slice(&id.to_le_bytes());
        done[12..].copy_from_slice(&state.frame().to_le_bytes());
        state.post(COMMAND_COMPLETED, &done);
    }
    if notification != 0
        && let Some(word) = heap_unmap(notification)
    {
        // SAFETY: a u32 in a heap block, which the title reads as volatile.
        unsafe { word.cast::<u32>().write_volatile(1) };
    }
    S_OK
}

extern "system" fn pop_message(this: *mut Acp, out: *mut Message) -> bool {
    let Some(message) = state(this).messages.pop_front() else {
        return false;
    };
    if !out.is_null() {
        // SAFETY: the title passes room for one message.
        unsafe { out.write_unaligned(message) };
    }
    true
}

extern "system" fn message_count(this: *mut Acp) -> u32 {
    state(this).messages.len() as u32
}

extern "system" fn release(this: *mut Acp) {
    // SAFETY: made by `AcpHalCreate` with `Box::into_raw`; released once.
    drop(unsafe { Box::from_raw(this) });
}

pub extern "system" fn AcpHalCreate(out: *mut *mut Acp) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let acp = Box::new(Acp {
        vtable: &ACP_METHODS,
        state: Mutex::new(State {
            connected: false,
            mask: 0,
            messages: VecDeque::new(),
            capacity: 0,
            dropped: 0,
            since: Instant::now(),
        }),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(acp)) };
    S_OK
}
