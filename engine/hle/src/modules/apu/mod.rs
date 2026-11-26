//! The console's audio decoder (`xapu.dll`): Opus packets in, float PCM out.
//!
//! A title connects once for one kind of processing, gets a block of memory
//! for its packets and output, and then queues per-stream commands
//! (activate, process, reset, deactivate) and dequeues one result per
//! command. We decode each command when it is queued, so its result is ready
//! at once; the order of results is the order of commands.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::Layout;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use opus_pure::OpusDecoder;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
const QUEUE_FULL: HRESULT = 0x8AC8_1000_u32 as i32;
/// No result yet, which titles poll for.
pub(crate) const PENDING_RESULTS: HRESULT = 0x8AC8_1001_u32 as i32;
const INVALID_COMMAND: HRESULT = 0x8AC8_1002_u32 as i32;
const INVALID_STREAM: HRESULT = 0x8AC8_1003_u32 as i32;
const ALREADY_ACTIVE: HRESULT = 0x8AC8_1005_u32 as i32;
const BAD_PACKET: HRESULT = 0x8AC8_1008_u32 as i32;

/// Processing kinds; only plain decoding is implemented.
const DECODE_OPUS: u32 = 1;

const ACTIVATE: u32 = 1;
const DEACTIVATE: u32 = 2;
const PROCESS: u32 = 3;
const NOTIFY: u32 = 4;
const RESET: u32 = 5;

/// Command option: signal the connection's event when the result is ready.
const SIGNAL: u32 = 1;
/// Connect options.
const DEINTERLEAVED_OUTPUT: u32 = 4;
const RESULT_EVENT: u32 = 8;

/// The longest Opus packet duration at 48 kHz (120 ms).
const MAX_FRAMES: usize = 5760;

#[repr(C)]
pub struct ConnectInput {
    process: u32,
    streams: u32,
    commands_per_stream: u32,
    memory: u32,
    frames_per_output: u32,
    options: u32,
}

#[repr(C)]
pub struct ConnectOutput {
    memory: *mut c_void,
    result_event: HANDLE,
}

/// What every command starts with, and what its result echoes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct CommandId {
    kind: u32,
    stream: u32,
    sequence: u32,
    options: u32,
    context: u64,
}

#[repr(C, packed)]
struct Activate {
    id: CommandId,
    channels: u32,
    init_length: u32,
    init: *const c_void,
    start_pitch: f32,
    processing_length: u32,
    processing: *mut c_void,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Process {
    id: CommandId,
    pitch_ramp: f32,
    target_pitch: f32,
    first_frame: u32,
    frame_count: u32,
    input_length: u32,
    input: *const u8,
    output_length: u32,
    output: *mut u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ApuResult {
    id: CommandId,
    state: u32,
    state_data: u64,
    usec: u64,
    error: HRESULT,
    frames: u32,
    output_length: u32,
    output: *mut u8,
    pitch: f32,
    input_length: u32,
    input: *const u8,
}

// SAFETY: the raw pointers are title addresses carried through, never
// dereferenced after the result is made.
unsafe impl Send for ApuResult {}

pub struct Connection {
    memory: *mut u8,
    layout: Layout,
    event: HANDLE,
    options: u32,
    per_stream: u32,
    state: Mutex<State>,
}

struct State {
    streams: Vec<Option<OpusDecoder>>,
    /// Results not yet dequeued, per stream.
    queued: Vec<u32>,
    results: VecDeque<ApuResult>,
    commands: u32,
    dequeued: u32,
}

fn lock(c: &Connection) -> std::sync::MutexGuard<'_, State> {
    c.state.lock().unwrap_or_else(|e| e.into_inner())
}

pub extern "system" fn XApuConnect(
    input: *const ConnectInput,
    output: *mut ConnectOutput,
    handle: *mut *mut Connection,
) -> HRESULT {
    if input.is_null() || output.is_null() || handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    let input = unsafe { &*input };
    if !(1..=500).contains(&input.streams)
        || !(1..=16).contains(&input.commands_per_stream)
        || input.memory == 0
    {
        return E_INVALIDARG;
    }
    if input.process != DECODE_OPUS {
        // shortcut: rate conversion (decode-convert, PCM convert) is not
        // implemented; add a resampler if a title connects for it.
        tracing::warn!(
            process = input.process,
            "audio decoder: processing kind not implemented"
        );
        return E_NOTIMPL;
    }
    if input.frames_per_output != 0 {
        return E_INVALIDARG;
    }
    let Ok(layout) = Layout::from_size_align(input.memory as usize, 16) else {
        return E_INVALIDARG;
    };
    // SAFETY: non-zero size.
    let memory = unsafe { std::alloc::alloc_zeroed(layout) };
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    let event = if input.options & RESULT_EVENT != 0 {
        // SAFETY: plain auto-reset event, no name.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            // SAFETY: allocated above with `layout`.
            unsafe { std::alloc::dealloc(memory, layout) };
            return E_OUTOFMEMORY;
        }
        event
    } else {
        std::ptr::null_mut()
    };
    let streams = input.streams as usize;
    tracing::info!(
        streams,
        memory = input.memory,
        options = input.options,
        "audio decoder connected"
    );
    let connection = Box::new(Connection {
        memory,
        layout,
        event,
        options: input.options,
        per_stream: input.commands_per_stream,
        state: Mutex::new(State {
            streams: (0..streams).map(|_| None).collect(),
            queued: vec![0; streams],
            results: VecDeque::new(),
            commands: 0,
            dequeued: 0,
        }),
    });
    // SAFETY: checked non-null.
    unsafe {
        output.write(ConnectOutput {
            memory: memory.cast(),
            result_event: event,
        });
        handle.write(Box::into_raw(connection));
    }
    S_OK
}

pub extern "system" fn XApuDisconnect(handle: *mut Connection) -> HRESULT {
    if handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XApuConnect`; disconnected once.
    let connection = unsafe { Box::from_raw(handle) };
    if !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { CloseHandle(connection.event) };
    }
    // SAFETY: allocated in `XApuConnect` with this layout.
    unsafe { std::alloc::dealloc(connection.memory, connection.layout) };
    S_OK
}

pub extern "system" fn XApuEnqueueCommand(
    handle: *mut Connection,
    command: *const CommandId,
    total: *mut u32,
) -> HRESULT {
    if handle.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a connection from `XApuConnect`; every command starts with an id.
    let (connection, id) = unsafe { (&*handle, command.read_unaligned()) };
    let mut state = lock(connection);
    let stream = id.stream as usize;
    if stream >= state.streams.len() {
        return INVALID_STREAM;
    }
    if state.queued[stream] >= connection.per_stream {
        return QUEUE_FULL;
    }
    let mut result = ApuResult {
        id,
        state: 0,
        state_data: 0,
        usec: 0,
        error: S_OK,
        frames: 0,
        output_length: 0,
        output: std::ptr::null_mut(),
        pitch: 1.0,
        input_length: 0,
        input: std::ptr::null(),
    };
    let decoder = &mut state.streams[stream];
    match id.kind {
        ACTIVATE => {
            // SAFETY: an activate command is this struct.
            let channels = unsafe { command.cast::<Activate>().read_unaligned() }.channels;
            tracing::debug!(stream, channels, "Opus stream activated");
            result.error = if decoder.is_some() {
                ALREADY_ACTIVE
            } else if !(1..=2).contains(&channels) {
                E_INVALIDARG
            } else {
                match OpusDecoder::new(48_000, channels as usize) {
                    Ok(d) => {
                        *decoder = Some(d);
                        S_OK
                    }
                    Err(_) => E_INVALIDARG,
                }
            };
        }
        DEACTIVATE => *decoder = None,
        RESET => {
            if let Some(d) = decoder {
                let _ = d.reset_state();
            }
        }
        NOTIFY => {}
        PROCESS => {
            // SAFETY: a process command is this struct.
            let command = unsafe { command.cast::<Process>().read_unaligned() };
            match decoder {
                Some(d) => process(d, connection.options, &command, &mut result),
                None => result.error = INVALID_STREAM,
            }
        }
        _ => return INVALID_COMMAND,
    }
    state.queued[stream] += 1;
    state.commands = state.commands.wrapping_add(1);
    state.results.push_back(result);
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.commands) };
    }
    if id.options & SIGNAL != 0 && !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { SetEvent(connection.event) };
    }
    S_OK
}

/// Decodes one packet into the command's output buffer, keeping the frames
/// from `first_frame` on, at most `frame_count` of them.
fn process(decoder: &mut OpusDecoder, options: u32, command: &Process, result: &mut ApuResult) {
    let channels = decoder.channels();
    let (input, input_length) = (command.input, command.input_length);
    let (output, output_length) = (command.output, command.output_length);
    result.input = input;
    result.input_length = input_length;
    result.output = output;
    if input.is_null() || output.is_null() || input_length == 0 {
        result.error = E_INVALIDARG;
        return;
    }
    // SAFETY: the title's packet in its connection memory.
    let packet = unsafe { std::slice::from_raw_parts(input, input_length as usize) };
    let mut pcm = vec![0f32; MAX_FRAMES * channels];
    let decoded = match decoder.decode(packet, MAX_FRAMES, &mut pcm) {
        Ok(n) => n,
        Err(_) => {
            result.error = BAD_PACKET;
            return;
        }
    };
    let first = (command.first_frame as usize).min(decoded);
    let frames = (decoded - first)
        .min(command.frame_count as usize)
        .min(output_length as usize / (4 * channels));
    let kept = &pcm[first * channels..(first + frames) * channels];
    let bytes = frames * channels * 4;
    if bytes > 0 {
        // SAFETY: `output` is non-null and holds at least `bytes <= output_length` bytes;
        // writing raw bytes avoids requiring 4-byte alignment on the title's `*mut u8`.
        let out = unsafe { std::slice::from_raw_parts_mut(output, bytes) };
        if options & DEINTERLEAVED_OUTPUT != 0 && channels == 2 {
            let (left, right) = out.split_at_mut(frames * 4);
            let (left, _) = left.as_chunks_mut::<4>();
            let (right, _) = right.as_chunks_mut::<4>();
            for (i, frame) in kept.as_chunks::<2>().0.iter().enumerate() {
                left[i] = frame[0].to_le_bytes();
                right[i] = frame[1].to_le_bytes();
            }
        } else {
            for (dst, &sample) in out.as_chunks_mut::<4>().0.iter_mut().zip(kept) {
                *dst = sample.to_le_bytes();
            }
        }
    }
    result.frames = frames as u32;
    result.output_length = bytes as u32;
}

pub extern "system" fn XApuDequeueResult(handle: *mut Connection, out: *mut ApuResult) -> HRESULT {
    if handle.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a connection from `XApuConnect`.
    let mut state = lock(unsafe { &*handle });
    let Some(result) = state.results.pop_front() else {
        return PENDING_RESULTS;
    };
    let stream = result.id.stream as usize;
    state.queued[stream] -= 1;
    state.dequeued = state.dequeued.wrapping_add(1);
    // SAFETY: the title passes room for one result.
    unsafe { out.write_unaligned(result) };
    S_OK
}

pub extern "system" fn XApuGetResultCount(handle: *mut Connection, total: *mut u32) -> u32 {
    if handle.is_null() {
        return 0;
    }
    // SAFETY: a connection from `XApuConnect`.
    let state = lock(unsafe { &*handle });
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.dequeued) };
    }
    state.results.len() as u32
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "apu",
        dlls: &["xapu.dll"],
        exports: vec![
            hle_export!(XApuConnect),
            hle_export!(XApuDisconnect),
            hle_export!(XApuEnqueueCommand),
            hle_export!(XApuDequeueResult),
            hle_export!(XApuGetResultCount),
        ],
    }
}

#[cfg(test)]
mod tests;
