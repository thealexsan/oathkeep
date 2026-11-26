use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::AtomicU32;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::Instant;

use super::objects::READING;
#[cfg(windows)]
use super::host;
use super::input::INPUT;
use super::types::{
    DEVICES, Device, Event, FALLBACK, HISTORY, KEYBOARD, KeyState, Object, PADS, POLL, PadState,
    Reading, Ref, Registration, STATUS_CONNECTED, Source, Value, fallback, merged, one_pad,
};

#[derive(Default)]
pub(super) struct Slot {
    pub(super) connected: bool,
    pub(super) sequence: u64,
    pub(super) history: VecDeque<Ref>,
}

pub(super) struct State {
    pub(super) slots: [Slot; DEVICES],
    pub(super) callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pub(super) pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    pub(super) running: Vec<(u64, ThreadId)>,
}

pub struct Hub {
    pub(super) object: Object,
    pub(super) devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    pub(super) source: Mutex<Box<dyn Source>>,
    /// The title sees one pad ([`one_pad`]).
    pub(super) one_pad: bool,
    state: Mutex<State>,
    /// A device callback returned.
    pub(super) returned: Condvar,
}

pub(super) fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Microseconds, the clock of every timestamp we hand out.
pub(super) fn now() -> u64 {
    START.elapsed().as_micros() as u64
}

impl Hub {
    pub(super) fn new(source: Box<dyn Source>, one_pad: bool) -> &'static Hub {
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            one_pad,
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.set_hub(hub);
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::new(source, one_pad());
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    pub(super) fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    pub(super) fn poll(&self) {
        let (pads, keys) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            (pads, source.keys())
        };
        let events = self.state().poll(&self.devices, self.one_pad, pads, keys);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    pub(super) fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    pub(super) fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        one_pad: bool,
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        if one_pad {
            let pad = pads.into_iter().flatten().fold(fallback(&keys), merged);
            self.update(devices, FALLBACK, Some(Value::Pad(pad)), now, &mut events);
            self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
            return events;
        }
        let any_pad = pads.iter().any(Option::is_some);
        for (slot, pad) in pads.into_iter().enumerate() {
            self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
        }
        let fallback = (!any_pad).then(|| Value::Pad(fallback(&keys)));
        self.update(devices, FALLBACK, fallback, now, &mut events);
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, connected = !was, "input device");
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::default())
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn rumble(&mut self, _: usize, _: u16, _: u16) {}
    }
    Box::new(Nothing)
}

pub(super) static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));
