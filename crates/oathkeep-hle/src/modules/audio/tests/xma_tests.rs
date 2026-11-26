use std::collections::VecDeque;
use std::time::Instant;

use oathkeep_formats::xma;

use super::super::xma::{
    ADVANCE_XMA_READ, Context, DISABLE_XMA, ENABLE_XMA, INPUT_INVALID, SAMPLES_PER_BLOCK, Trace,
    Voice, Xma, decode, xma,
};
use super::super::*;
use super::CONTEXT_TESTS;
use crate::modules::xgameruntime::S_OK;

/// Two packets of three silent mono frames each: a fixed one-subframe
/// layout, no transmitted coefficients.
fn silent_stream() -> Vec<u8> {
    let mut data = vec![0xffu8; 2 * xma::PACKET_BYTES];
    let mut put = |at: usize, value: u32, width: usize| {
        for i in 0..width {
            let (p, bit) = (at + i, (value >> (width - 1 - i)) & 1);
            data[p / 8] = (data[p / 8] & !(0x80 >> (p % 8))) | ((bit as u8) << (7 - p % 8));
        }
    };
    for packet in 0..2 {
        let base = packet * xma::PACKET_BYTES * 8;
        put(base, 3 << 26, 32);
        for frame in 0..3 {
            let at = base + 32 + frame * 31;
            put(at, 31, 15); // length
            put(at + 15, 0b10, 2); // fixed layout, one 512-sample subframe
            put(at + 17, 0, 8 + 1 + 3 + 2); // gain, no trim, empty subframe
        }
    }
    data
}

/// A read block past the end of the ring (6 on a ring of four) is block
/// 2 of it: a stream that loops forever fills blocks 0 and 1 and the
/// pass ends. It used to fill the ring for good, holding the voice.
#[test]
fn xma_read_blocks_past_the_ring_end_wrap() {
    let _serial = lock(&CONTEXT_TESTS);
    let mut request = ShapeContexts {
        counts: [0, 0, 0, 0, 4, 0],
        arrays: [std::ptr::null_mut(); 6],
        apu: [0; 6],
    };
    assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
    let context = Context(request.arrays[4].cast::<u32>().wrapping_add(16));
    let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
    assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
    let stream = silent_stream();
    // SAFETY: two packets were allocated.
    unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };
    let (mut output, mut output_apu) = (std::ptr::null_mut(), 0);
    assert_eq!(
        ApuAlloc(&mut output, &mut output_apu, 0x400, 0x100, 2),
        S_OK
    );
    // SAFETY: the ring was just allocated.
    unsafe { output.cast::<u8>().write_bytes(0x55, 0x400) };

    // As in `xma_contexts_decode_into_their_output_rings`, but looping
    // forever from the first frame to the last, and read block 6.
    context.set(0, 2 | 255 << 12 | 1 << 20 | 4 << 22);
    context.set(1, 1 << 31 | 3 << 27);
    context.set(2, 32);
    context.set(3, 32);
    context.set(4, (xma::PACKET_BYTES * 8 + 32 + 2 * 31) as u32);
    context.set(5, input_apu);
    context.set(7, output_apu);
    context.set(9, 6);

    let mut acp = std::ptr::null_mut();
    assert_eq!(AcpHalCreate(&mut acp), S_OK);
    // SAFETY: just created.
    let m = unsafe { (*acp).vtable };
    assert_eq!((m.connect)(acp, 0x1000, 0x1000), S_OK);
    let submit = |command: u32, words: &[u32]| {
        assert_eq!(
            (m.submit)(acp, command, 0xf1, u32::MAX, words.as_ptr().cast(), 0),
            S_OK
        );
    };
    submit(ENABLE_XMA, &[1]);
    // A pass on a thread of its own (the decoding thread runs them too),
    // so that one that never ends fails this test instead of hanging it.
    let (done, passed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        xma().pump();
        let _ = done.send(());
    });
    let five = std::time::Duration::from_secs(5);
    let wait = || passed.recv_timeout(five).is_ok();
    let ended = wait();
    if !ended {
        // End it as a full ring would, to release the contexts below.
        context.set_field(1, 31, 1, 0);
        assert!(wait(), "the XMA pass did not end on a full ring either");
    }
    let (write, writable) = (context.field(0, 27, 5), context.field(1, 31, 1));
    // SAFETY: the ring.
    let ring = unsafe { std::slice::from_raw_parts(output.cast::<u8>(), 0x400) }.to_vec();

    submit(DISABLE_XMA, &[1]);
    (m.release)(acp);
    ApuFree(input);
    ApuFree(output);
    assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
    assert!(ended, "the XMA pass never ended");
    // Blocks 0 and 1 written (silence), 2 and 3 left for the title.
    assert_eq!((write, writable), (2, 0));
    assert!(ring[..0x200].iter().all(|&b| b == 0));
    assert!(ring[0x200..].iter().all(|&b| b == 0x55));
}

/// A write block past the end of the ring (6 on a ring of four) is block
/// 2 of it: blocks 2, 3 and 0 are filled, up to the read block (1). It
/// used to write block 6 first, past the ring's end.
#[test]
fn xma_write_blocks_past_the_ring_end_wrap() {
    let _serial = lock(&CONTEXT_TESTS);
    let mut request = ShapeContexts {
        counts: [0, 0, 0, 0, 4, 0],
        arrays: [std::ptr::null_mut(); 6],
        apu: [0; 6],
    };
    assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
    let context = Context(request.arrays[4].cast::<u32>().wrapping_add(16));
    let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
    assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
    let stream = silent_stream();
    // SAFETY: two packets were allocated.
    unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };
    let (mut output, mut output_apu) = (std::ptr::null_mut(), 0);
    assert_eq!(
        ApuAlloc(&mut output, &mut output_apu, 0x800, 0x100, 2),
        S_OK
    );
    // SAFETY: the allocation was just made: the ring (four blocks) and a guard
    // area after it, which the pass must not touch.
    unsafe { output.cast::<u8>().write_bytes(0x55, 0x800) };

    // As in `xma_contexts_decode_into_their_output_rings`, but looping
    // forever from the first frame to the last; write block 6, read block 1.
    context.set(0, 2 | 255 << 12 | 1 << 20 | 4 << 22 | 6 << 27);
    context.set(1, 1 << 31 | 3 << 27);
    context.set(2, 32);
    context.set(3, 32);
    context.set(4, (xma::PACKET_BYTES * 8 + 32 + 2 * 31) as u32);
    context.set(5, input_apu);
    context.set(7, output_apu);
    context.set(9, 1);

    let mut acp = std::ptr::null_mut();
    assert_eq!(AcpHalCreate(&mut acp), S_OK);
    // SAFETY: just created.
    let m = unsafe { (*acp).vtable };
    assert_eq!((m.connect)(acp, 0x1000, 0x1000), S_OK);
    let submit = |command: u32, words: &[u32]| {
        assert_eq!(
            (m.submit)(acp, command, 0xf1, u32::MAX, words.as_ptr().cast(), 0),
            S_OK
        );
    };
    submit(ENABLE_XMA, &[1]);
    // A pass on a thread of its own (the decoding thread runs them too),
    // so that one that never ends fails this test instead of hanging it.
    let (done, passed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        xma().pump();
        let _ = done.send(());
    });
    let five = std::time::Duration::from_secs(5);
    let wait = || passed.recv_timeout(five).is_ok();
    let ended = wait();
    if !ended {
        // End it as a full ring would, to release the contexts below.
        context.set_field(1, 31, 1, 0);
        assert!(wait(), "the XMA pass did not end on a full ring either");
    }
    let (write, writable) = (context.field(0, 27, 5), context.field(1, 31, 1));
    // SAFETY: the ring.
    let ring = unsafe { std::slice::from_raw_parts(output.cast::<u8>(), 0x800) }.to_vec();

    submit(DISABLE_XMA, &[1]);
    (m.release)(acp);
    ApuFree(input);
    ApuFree(output);
    assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
    assert!(ended, "the XMA pass never ended");
    // Blocks 2, 3 and 0 written (silence), 1 left for the title, and
    // nothing past the ring.
    assert_eq!((write, writable), (1, 0));
    assert!(ring[0x400..].iter().all(|&b| b == 0x55), "written past the ring");
    let block = |n: usize| &ring[n * 0x100..(n + 1) * 0x100];
    assert!([0, 2, 3].iter().all(|&n| block(n).iter().all(|&b| b == 0)));
    assert!(block(1).iter().all(|&b| b == 0x55));
}

#[test]
fn xma_contexts_decode_into_their_output_rings() {
    let _serial = lock(&CONTEXT_TESTS);
    let mut request = ShapeContexts {
        counts: [0, 0, 0, 0, 4, 0],
        arrays: [std::ptr::null_mut(); 6],
        apu: [0; 6],
    };
    assert_eq!(AcpHalAllocateShapeContexts(&mut request), S_OK);
    let context = Context(request.arrays[4].cast::<u32>().wrapping_add(16));

    let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
    assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
    let stream = silent_stream();
    // SAFETY: two packets were allocated.
    unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };
    let (mut output, mut output_apu) = (std::ptr::null_mut(), 0);
    assert_eq!(
        ApuAlloc(&mut output, &mut output_apu, 0x400, 0x100, 2),
        S_OK
    );
    // SAFETY: the ring was just allocated.
    unsafe { output.cast::<u8>().write_bytes(0x55, 0x400) };

    // As the title sets one up: two packets, input 0 valid, a ring of
    // four blocks, writable, 48 kHz mono, reading from the first frame.
    context.set(0, 2 | 1 << 20 | 4 << 22);
    context.set(1, 1 << 31 | 3 << 27);
    context.set(2, 32);
    context.set(5, input_apu);
    context.set(7, output_apu);

    let mut acp = std::ptr::null_mut();
    assert_eq!(AcpHalCreate(&mut acp), S_OK);
    // SAFETY: just created.
    let m = unsafe { (*acp).vtable };
    assert_eq!((m.connect)(acp, 0x1000, 0x1000), S_OK);
    let submit = |command: u32, words: &[u32]| {
        assert_eq!(
            (m.submit)(acp, command, 0xf1, u32::MAX, words.as_ptr().cast(), 0),
            S_OK
        );
    };
    submit(ENABLE_XMA, &[1]);
    xma().pump();
    // Full: all four blocks written (silence), the write block wrapped
    // onto the read block, and the ring no longer writable.
    assert_eq!(context.field(0, 27, 5), 0);
    assert_eq!(context.field(1, 31, 1), 0);
    // SAFETY: the ring.
    let ring = unsafe { std::slice::from_raw_parts(output.cast::<u8>(), 0x400) };
    assert!(ring.iter().all(|&b| b == 0));

    // Read four blocks at a time until the input has run out and the
    // ring keeps room (the decoding thread may fill it too).
    for _ in 0..20 {
        submit(ADVANCE_XMA_READ, &[1, 4]);
        xma().pump();
        if context.field(2, 31, 1) == 1 && context.field(1, 31, 1) == 1 {
            break;
        }
    }
    // Six frames and the flushed tail: 3,328 samples, 26 blocks.
    let produced = |x: &Xma| lock(&x.voices[1]).as_ref().map(|v| v.trace.produced);
    assert_eq!(produced(&xma()), Some(26 * SAMPLES_PER_BLOCK));
    assert_eq!(context.field(0, 20, 2), 0, "input used up");
    assert_eq!(context.field(2, 26, 5), INPUT_INVALID);

    // A count near `u32::MAX` wraps the read block like any other
    // (3 + 0xffff_ffff blocks on a ring of four is block 2); the sum
    // used to overflow.
    context.set_field(9, 0, 5, 3);
    submit(ADVANCE_XMA_READ, &[1, u32::MAX]);
    assert_eq!(context.field(9, 0, 5), 2);

    submit(DISABLE_XMA, &[1]);
    assert_eq!(produced(&xma()), None);
    (m.release)(acp);
    ApuFree(input);
    ApuFree(output);
    assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
}

/// A loop over the first two frames, which ends after the second
/// frame's second subframe and starts two subframes into the first.
/// Each end of the loop is cut in its own frame.
#[test]
fn a_loop_skips_subframes_of_its_first_frame() {
    let _serial = lock(&CONTEXT_TESTS);
    let (mut input, mut input_apu) = (std::ptr::null_mut(), 0);
    assert_eq!(ApuAlloc(&mut input, &mut input_apu, 0x1000, 0x800, 1), S_OK);
    let stream = silent_stream();
    // SAFETY: two packets were allocated.
    unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input.cast(), stream.len()) };

    // Frames start at bits 32, 63 and 94 of each packet.
    let (first, second, third) = (32, 63, 94);
    let (end_subframe, skip_subframes) = (1, 2);
    let mut words = [0u32; 16];
    let context = Context(words.as_mut_ptr());
    // Two packets, one loop, input 0 valid; 48 kHz mono.
    context.set(0, 2 | 1 << 12 | 1 << 20 | 4 << 22);
    context.set(
        1,
        1 << 31 | 3 << 27 | end_subframe << 12 | skip_subframes << 17,
    );
    context.set(2, first);
    context.set(3, first);
    context.set(4, second);
    context.set(5, input_apu);
    let mut voice = Voice {
        decoder: None,
        pcm: VecDeque::new(),
        skip: 0,
        ended: false,
        trace: Trace {
            since: Instant::now(),
            produced: 0,
            taken: 0,
            takes: 0,
            partial_takes: 0,
            frames: 0,
            bad_frames: 0,
            peak: 0,
            ended_ms: None,
        },
    };
    let mut step = |expect_next: u32| {
        let before = voice.pcm.len();
        assert!(decode(context, &mut voice));
        assert_eq!(context.field(2, 0, 26), expect_next);
        voice.pcm.len() - before
    };
    let subframe = xma::SUBFRAME_SAMPLES;
    // Before the loop: the first frame whole.
    assert_eq!(step(second), 4 * subframe);
    // The loop's last frame: its first two subframes, then back.
    assert_eq!(step(first), 2 * subframe, "the loop's last frame");
    assert_eq!(context.field(0, 12, 8), 0, "one loop, taken");
    // The loop's first frame again: without its first two subframes.
    assert_eq!(step(second), 2 * subframe, "the loop's first frame");
    // No loops left: both frames whole, and on to the third.
    assert_eq!(step(third), 4 * subframe);
    ApuFree(input);
}
