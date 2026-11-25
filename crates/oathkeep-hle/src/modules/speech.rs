#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};

use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

pub const PROVIDER: Guid = Guid::new(
    0x3e24_1977,
    0x6237,
    0x41e9,
    [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
);
pub const IID: Guid = Guid::new(
    0xd722_b373,
    0x8c4d,
    0x4692,
    [0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d],
);

pub static OBJECT: Object = Object::new("accessibility", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT)];

const SILENCE: [u8; 44] =
    *b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\0\0\0\0";

static SYNTHESIZER: u8 = 0;
static STREAM: u8 = 0;

type This = *const Object;

extern "system" fn enumerate_voices(
    _: This,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    S_OK
}
extern "system" fn create(_: This, out: *mut *const u8) -> HRESULT {
    put(out, &raw const SYNTHESIZER)
}
extern "system" fn close(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_default_voice(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_custom_voice(_: This, _: *const u8, _voice: *const c_char) -> HRESULT {
    S_OK
}
extern "system" fn create_stream(
    _: This,
    _: *const u8,
    words: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    tracing::debug!(text = text(words).unwrap_or_default(), "speech (silent)");
    put(out, &raw const STREAM)
}
extern "system" fn stream_size(_: This, _: *const u8, out: *mut usize) -> HRESULT {
    put(out, SILENCE.len())
}
extern "system" fn stream_data(
    _: This,
    _: *const u8,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    if buffer.is_null() || size < SILENCE.len() {
        return super::xasync::E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { buffer.copy_from_nonoverlapping(SILENCE.as_ptr(), SILENCE.len()) };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(SILENCE.len()) };
    }
    S_OK
}

static VTABLE: Methods<22> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),
    missing::<4> as *const (),
    missing::<5> as *const (),
    missing::<6> as *const (),
    missing::<7> as *const (),
    enumerate_voices as *const (),
    create as *const (),
    close as *const (),
    set_default_voice as *const (),
    set_custom_voice as *const (),
    create_stream as *const (),
    close as *const (),
    stream_size as *const (),
    stream_data as *const (),
    missing::<17> as *const (),
    missing::<18> as *const (),
    missing::<19> as *const (),
    missing::<20> as *const (),
    create_stream as *const (),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn streams_are_empty_wave_files() {
        let this: This = &OBJECT;
        type Create = extern "system" fn(This, *mut *const u8) -> HRESULT;
        type Stream = extern "system" fn(This, *const u8, *const c_char, *mut *const u8) -> HRESULT;
        type Size = extern "system" fn(This, *const u8, *mut usize) -> HRESULT;
        type Data = extern "system" fn(This, *const u8, usize, *mut u8, *mut usize) -> HRESULT;
        let (mut synth, mut stream, mut size) = (std::ptr::null(), std::ptr::null(), 0);
        assert_eq!(method::<Create>(&OBJECT, 0x48)(this, &mut synth), S_OK);
        let say = method::<Stream>(&OBJECT, 0x68);
        assert_eq!(say(this, synth, c"Hello".as_ptr(), &mut stream), S_OK);
        assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
        let mut wav = vec![0u8; size];
        let mut used = 0;
        let data = method::<Data>(&OBJECT, 0x80);
        assert_eq!(data(this, stream, size, wav.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(
            (&wav[..4], &wav[8..16], &wav[36..40]),
            (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..])
        );
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(used, 44);
    }
}
