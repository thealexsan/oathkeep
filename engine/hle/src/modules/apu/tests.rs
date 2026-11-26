use super::*;
use opus_pure::{Application, OpusEncoder};

fn id(kind: u32, stream: u32) -> CommandId {
    CommandId {
        kind,
        stream,
        sequence: 7,
        options: 0,
        context: 0x1234,
    }
}

#[test]
fn decodes_opus_as_the_title_drives_it() {
    assert_eq!(size_of::<ApuResult>(), 0x50);
    // 256 mono/stereo streams, two commands each, as the title connects.
    let input = ConnectInput {
        process: DECODE_OPUS,
        streams: 256,
        commands_per_stream: 2,
        memory: 0x10_4000,
        frames_per_output: 0,
        options: 1,
    };
    let mut output = ConnectOutput {
        memory: std::ptr::null_mut(),
        result_event: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();
    assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
    let base = output.memory.cast::<u8>();

    let activate = Activate {
        id: id(ACTIVATE, 3),
        channels: 2,
        init_length: 0,
        init: std::ptr::null(),
        start_pitch: 1.0,
        processing_length: 0,
        processing: std::ptr::null_mut(),
    };
    assert_eq!(
        XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()),
        S_OK
    );

    // A 20 ms stereo packet of a 440 Hz tone.
    let tone: Vec<f32> = (0..960 * 2)
        .map(|i| ((i / 2) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
        .collect();
    let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
    let mut packet = [0u8; 1500];
    let n = encoder.encode(&tone, 960, &mut packet).unwrap();
    // SAFETY: inside the connection memory.
    unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
    let process = Process {
        id: id(PROCESS, 3),
        pitch_ramp: 0.0,
        target_pitch: 1.0,
        first_frame: 0,
        frame_count: 960,
        input_length: n as u32,
        input: base,
        output_length: 0xf00 * 2,
        output: base.wrapping_add(0x14000),
    };
    let mut total = 0;
    assert_eq!(
        XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
        S_OK
    );
    assert_eq!(total, 2);
    // Two results queued for stream 3: its queue is full.
    assert_eq!(
        XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
        QUEUE_FULL
    );
    assert_eq!(XApuGetResultCount(handle, std::ptr::null_mut()), 2);

    let mut result = std::mem::MaybeUninit::<ApuResult>::zeroed();
    assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
    // SAFETY: written above.
    let first = unsafe { result.assume_init() };
    let (kind, error, context) = (first.id.kind, first.error, first.id.context);
    assert_eq!((kind, error, context), (ACTIVATE, S_OK, 0x1234));
    assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
    // SAFETY: written above.
    let decoded = unsafe { result.assume_init() };
    let (kind, error, frames, length) = (
        decoded.id.kind,
        decoded.error,
        decoded.frames,
        decoded.output_length,
    );
    assert_eq!((kind, error, frames, length), (PROCESS, S_OK, 960, 960 * 8));
    // SAFETY: the decoder wrote 960 stereo frames there.
    let pcm =
        unsafe { std::slice::from_raw_parts(base.wrapping_add(0x14000).cast::<f32>(), 1920) };
    let energy: f32 = pcm.iter().map(|s| s * s).sum::<f32>() / 1920.0;
    assert!(energy > 0.01, "decoded a tone, energy {energy}");
    assert_eq!(
        XApuDequeueResult(handle, result.as_mut_ptr()),
        PENDING_RESULTS
    );
    let mut dequeued = 0;
    assert_eq!(XApuGetResultCount(handle, &mut dequeued), 0);
    assert_eq!(dequeued, 2);

    // A process on a stream that was never activated fails in its result.
    let mut stray = process;
    stray.id.stream = 4;
    assert_eq!(
        XApuEnqueueCommand(handle, (&raw const stray).cast(), &mut total),
        S_OK
    );
    assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
    // SAFETY: written above.
    let error = unsafe { result.assume_init() }.error;
    assert_eq!(error, INVALID_STREAM);
    assert_eq!(XApuDisconnect(handle), S_OK);
}

#[test]
fn decodes_into_unaligned_output_interleaved_and_deinterleaved() {
    let tone: Vec<f32> = (0..960 * 2)
        .map(|i| {
            let phase = (i / 2) as f32 * std::f32::consts::TAU / 48_000.0;
            if i % 2 == 0 {
                (phase * 440.0).sin() * 0.5
            } else {
                (phase * 880.0).sin() * 0.25
            }
        })
        .collect();
    let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
    let mut packet = [0u8; 1500];
    let n = encoder.encode(&tone, 960, &mut packet).unwrap();

    for (options, deinterleaved) in [(0u32, false), (DEINTERLEAVED_OUTPUT, true)] {
        let input = ConnectInput {
            process: DECODE_OPUS,
            streams: 4,
            commands_per_stream: 4,
            memory: 0x4_0000,
            frames_per_output: 0,
            options,
        };
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
        let base = output.memory.cast::<u8>();

        // Invalid channel counts fail cleanly without activating the stream.
        for bad_channels in [0u32, 3, u32::MAX] {
            let bad = Activate {
                id: id(ACTIVATE, 0),
                channels: bad_channels,
                init_length: 0,
                init: std::ptr::null(),
                start_pitch: 1.0,
                processing_length: 0,
                processing: std::ptr::null_mut(),
            };
            assert_eq!(
                XApuEnqueueCommand(handle, (&raw const bad).cast(), std::ptr::null_mut()),
                S_OK
            );
            let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
            assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
            // SAFETY: written by `XApuDequeueResult`.
            let err = unsafe { res.assume_init() }.error;
            assert_eq!(err, E_INVALIDARG, "channels={bad_channels}");
        }

        // Activate two identical stereo decoders so aligned and unaligned outputs match bit-for-bit.
        for stream in [0u32, 1] {
            let activate = Activate {
                id: id(ACTIVATE, stream),
                channels: 2,
                init_length: 0,
                init: std::ptr::null(),
                start_pitch: 1.0,
                processing_length: 0,
                processing: std::ptr::null_mut(),
            };
            assert_eq!(
                XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()),
                S_OK
            );
            let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
            assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
            // SAFETY: written by `XApuDequeueResult`.
            let err = unsafe { res.assume_init() }.error;
            assert_eq!(err, S_OK);
        }

        // SAFETY: inside the connection memory.
        unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
        let aligned_out = base.wrapping_add(0x10000);
        let unaligned_out = base.wrapping_add(0x20001); // 1 mod 4: misaligned for f32

        for (stream, out_ptr, frame_count) in [
            (0u32, aligned_out, 960u32),
            (1u32, unaligned_out, 960u32),
            (1u32, base.wrapping_add(0x30003), 0u32), // zero frames at misaligned ptr
        ] {
            let proc_cmd = Process {
                id: id(PROCESS, stream),
                pitch_ramp: 0.0,
                target_pitch: 1.0,
                first_frame: 0,
                frame_count,
                input_length: n as u32,
                input: base,
                output_length: 960 * 8,
                output: out_ptr,
            };
            assert_eq!(
                XApuEnqueueCommand(handle, (&raw const proc_cmd).cast(), std::ptr::null_mut()),
                S_OK
            );
            let mut res = std::mem::MaybeUninit::<ApuResult>::zeroed();
            assert_eq!(XApuDequeueResult(handle, res.as_mut_ptr()), S_OK);
            // SAFETY: written by `XApuDequeueResult`.
            let decoded = unsafe { res.assume_init() };
            let (err, frames, len) = (decoded.error, decoded.frames, decoded.output_length);
            assert_eq!((err, frames, len), (S_OK, frame_count, frame_count * 8));
        }

        // SAFETY: both output regions are inside connection memory and hold 960 * 8 bytes.
        let (aligned_bytes, unaligned_bytes) = unsafe {
            (
                std::slice::from_raw_parts(aligned_out, 960 * 8),
                std::slice::from_raw_parts(unaligned_out, 960 * 8),
            )
        };
        assert_eq!(
            unaligned_bytes, aligned_bytes,
            "unaligned output must match aligned output (deinterleaved={deinterleaved})"
        );
        let samples: Vec<f32> = unaligned_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        let energy: f32 = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
        assert!(
            energy > 0.01,
            "expected decoded tone (deinterleaved={deinterleaved}), energy={energy}"
        );
        assert_eq!(XApuDisconnect(handle), S_OK);
    }
}
