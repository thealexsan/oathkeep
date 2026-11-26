use super::{bitcode, lower, shader};

/// The loop limit without an override: 2^20.
pub const DEFAULT_LOOP_LIMIT: u32 = lower::LOOP_LIMIT;

/// The iteration count after which a translated loop is left, per lane and
/// per entry into the loop: [`DEFAULT_LOOP_LIMIT`].
///
/// With the `diagnostics` feature, `OATHKEEP_SHADER_LOOP_LIMIT` (decimal or
/// `0x` hex, at least 1) sets it instead. A loop that hits the limit in
/// every lane of a full-screen pass still runs for seconds (2^20 times a
/// 4K screen is about 10^13 iterations); a much lower limit shows whether
/// such loops are what makes a frame slow. The value is part of the
/// translation, so the shader cache must key on it (`oathkeep-gpu`'s does).
pub fn loop_limit() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        if cfg!(feature = "diagnostics") {
            parse_loop_limit(std::env::var("OATHKEEP_SHADER_LOOP_LIMIT").ok().as_deref())
        } else {
            lower::LOOP_LIMIT
        }
    })
}

/// Whether a pixel shader's helper lane reads its own value where the console reads the first
/// lane's (`lower.rs` `first_lane`); on unless `OATHKEEP_HELPER_OWN=0`. Part of the translation:
/// the shader cache keys on it.
pub fn helper_own() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_HELPER_OWN").is_none_or(|v| v != "0"))
}

/// Which of a translated program's group barriers wait for the whole
/// group (`GroupMemoryBarrierWithGroupSync`); the others keep their memory
/// fence and lose the wait. A wait that some thread of the group never
/// reaches is undefined on the host, and NVIDIA's wait is for every
/// thread: the group stands until Windows resets the card. (The console's
/// barrier is the wave's, whatever lanes are live, and a wave that ended
/// is not waited for.) `OATHKEEP_GROUP_SYNC`:
/// - unset or `2`: every barrier waits, as before.
/// - `1`: the title's `s_barrier` waits only in a block every lane runs,
///   in a program no lane can be cut out of at the iteration limit.
/// - `0`: no barrier waits, neither the title's nor the ones the
///   translation adds (wave64 programs in two host waves exchange through
///   them, so such programs compute wrong values). For one run that tells
///   whether a barrier is what stops the card: with it no run can stand in
///   a barrier.
///
/// Part of the translation: the shader cache keys on it (`oathkeep-gpu`'s
/// does).
pub fn group_sync() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("OATHKEEP_GROUP_SYNC").ok().as_deref().map(str::trim) {
        Some("0") => 0,
        Some("1") => 1,
        _ => 2,
    })
}

/// Whether buffer atomics of the ray-tracing structure builder stand between
/// two fences (`OATHKEEP_BVH_FENCES=0`: they do not). Part of the
/// translation: the shader cache keys on it.
pub fn builder_fences() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_BVH_FENCES").is_none_or(|v| v != "0"))
}

/// Whether the structure builder's accesses through its root views stay near
/// them, and a size in a structure's header that cannot be one counts as none
/// (`OATHKEEP_BVH_BOUNDS=0`: neither). Part of the translation: the shader
/// cache keys on it.
pub fn builder_bounds() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_BVH_BOUNDS").is_none_or(|v| v != "0"))
}

/// The clean-up passes that run over a translated program before it is
/// written (`bitcode::opt`): a set of that module's pass bits. Every pass
/// is exact, so the switch is for telling a translator fault from a
/// clean-up fault, and for measuring what the clean-up is worth.
/// `OATHKEEP_SHADER_OPT`:
/// - unset: every pass but the last one below (63).
/// - `0`: none (the module as the lowering made it).
/// - a number (decimal or `0x` hex): those passes; 1 dead code, 2 constants
///   and identities, 4 one instruction for one value, 8 what a branch taken
///   says about its condition, 16 one read for one place within a block,
///   32 a first-lane read of two host waves without the round that asks
///   which half has a lane (where every lane has one).
///   64 the dwords of one console load read by one call for every four
///   (not on unless asked: `127` is every pass with it).
///
/// Part of the translation: the shader cache keys on it (`oathkeep-gpu`'s
/// does).
pub fn shader_opt() -> u32 {
    static PASSES: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *PASSES.get_or_init(|| parse_shader_opt(std::env::var("OATHKEEP_SHADER_OPT").ok().as_deref()))
}

/// On a host whose shaders run on the console's own kind of hardware (AMD
/// RDNA, [`set_host_rdna`]): which of the console's float instructions are
/// written as the one host operation the driver makes the same instruction
/// of, instead of spelled out in operations every host agrees on.
/// `OATHKEEP_RDNA_FLOAT`: a set of [`rdna`]'s bits (decimal or `0x` hex); all
/// of them when it is unset, `0` none. Nothing on other hosts.
///
/// Measured on an RX 7900 XTX (a frame of the yard, replayed turn by turn with
/// and without): the title's dispatches and draws 18.4 -> 17.6 ms, 0.4 of it
/// without [`rdna::LEGACY`]. Tried and worth nothing there: `v_mad_f32` as one
/// multiply-add, a fast reciprocal, scalar buffer loads of four dwords at once,
/// a wave size for every compute program (32, 64, the console's).
///
/// Part of the translation: the shader cache keys on it ([`host_settings`]).
pub fn rdna_float() -> u32 {
    static BITS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BITS.get_or_init(|| {
        let value = std::env::var("OATHKEEP_RDNA_FLOAT").ok();
        parse_set(value.as_deref(), rdna::ALL, rdna::ALL)
    })
}

/// The bits of [`rdna_float`].
pub mod rdna {
    /// `v_max_f32` and the like: the host's FMax and FMin, without the code
    /// that orders -0 below +0.
    pub const MAX_MIN: u32 = 1;
    /// `v_mul_legacy_f32` and the like (0 times anything is 0): a plain product
    /// where no factor can be an infinity or no number where the other is 0,
    /// and one comparison where it can. A zero may have the other sign.
    pub const LEGACY: u32 = 2;
    /// `v_med3_f32` of a value between two ordered constants: a minimum of a
    /// maximum.
    pub const MED3: u32 = 4;
    /// `v_fract_f32`: the host's Frc as it is.
    pub const FRACT: u32 = 8;
    pub const ALL: u32 = 15;
}

/// A switch's set of bits; `unset` when it is unset or unreadable. Pure, for
/// testing.
fn parse_set(value: Option<&str>, unset: u32, all: u32) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    parsed.map_or(unset, |n| n & all)
}

/// Everything a translation depends on besides the container: the
/// translator's own source (hashed at build time), the value of every
/// `OATHKEEP_*` switch it reads, the host facts the device hands it (widest
/// wave, view heap limit) and the settings above. The shader cache keys on
/// this instead of on the whole executable, so a rebuild that leaves the
/// translator alone keeps its translations. Call it after the device is made.
pub fn translation_fingerprint() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(env!("OATHKEEP_SHADER_SOURCE_HASH").as_bytes());
    out.push(u8::from(cfg!(feature = "diagnostics")));
    out.push(u8::from(cfg!(debug_assertions)));
    for name in env!("OATHKEEP_SHADER_SWITCHES").split(',').filter(|n| !n.is_empty()) {
        out.extend_from_slice(name.as_bytes());
        out.push(b'=');
        if let Some(value) = std::env::var_os(name) {
            out.extend_from_slice(value.as_encoded_bytes());
        }
        out.push(0);
    }
    out.extend_from_slice(&lower::host_max_wave().to_le_bytes());
    out.extend_from_slice(&lower::host_rdna().to_le_bytes());
    out.extend_from_slice(&shader::view_heap_limit().to_le_bytes());
    out.extend_from_slice(&shader::raw_views().to_le_bytes());
    out.extend_from_slice(&loop_limit().to_le_bytes());
    out.push(group_sync());
    out.extend_from_slice(&shader_opt().to_le_bytes());
    out
}

/// `OATHKEEP_SHADER_OPT`'s value; every pass when it is unset or
/// unreadable. Pure, for testing.
pub(super) fn parse_shader_opt(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    parsed.map_or(bitcode::opt::DEFAULT, |n| n & bitcode::opt::ALL)
}

/// `OATHKEEP_SHADER_LOOP_LIMIT`'s value; the default when it is unset,
/// unreadable or 0. Pure, for testing.
pub(super) fn parse_loop_limit(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    match parsed {
        Some(n) if n > 0 => n,
        _ => lower::LOOP_LIMIT,
    }
}
