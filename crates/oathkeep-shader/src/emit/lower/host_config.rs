use std::collections::HashSet;

use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Inst, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

/// Masks held as numbers or constant lane sets in guards or lane branches: the lane tests its
/// own bit by its console lane.
pub(super) fn lane_constant_masks(st: &Structured) -> bool {
    let whens = st.exits.iter().filter_map(|e| match *e {
        LaneExit::Branch { when, .. } => Some(when),
        _ => None,
    });
    let mut work: Vec<Bool> = st.guards.iter().flatten().flatten().copied().chain(whens).collect();
    // Through lane-mask joins too: an edge that brings a number or a constant lane set needs
    // the lane's own bit of it.
    // shortcut: masks only selects read (`v_cndmask_b32`, carries in) are not followed; such
    // a program still stops with "a constant lane mask" or "an opaque lane mask".
    let (mut seen, mut joins) = (HashSet::new(), HashSet::new());
    while let Some(g) = work.pop() {
        if !seen.insert(g) {
            continue;
        }
        for a in st.bools.support(g) {
            match a {
                Atom::Opaque(_) | Atom::Constant(_) => return true,
                Atom::Phi(id) if joins.insert(id) => {
                    if let Some(edges) = st.phi_masks.get(&id) {
                        work.extend(edges.iter().copied());
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// The widest wave the host GPU runs (0: not known – no host-specific forms); see
/// [`set_host_max_wave`].
static HOST_MAX_WAVE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The host GPU's `WaveLaneCountMax`: 32 lets console wave64 compute programs whose cross-lane
/// code stays inside 32-lane halves run in waves of 32 instead of being refused.
pub fn set_host_max_wave(lanes: u32) {
    HOST_MAX_WAVE.store(lanes, std::sync::atomic::Ordering::Relaxed);
}

thread_local! {
    /// See [`take_needs`].
    pub(super) static NEEDS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    /// See [`with_local_waves`].
    pub(super) static LOCAL_WAVES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// See [`with_local_waves_any`].
    pub(super) static LOCAL_ANY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// As [`with_local_waves`], with every other use of a lane mask as a number
/// and every lane read and write taken per host wave too: the program runs
/// as it would on a console of 32-lane waves, lanes numbered 0..31 or
/// 32..63 by half. Right for code that only relies on what HLSL's wave
/// operations promise; a program that takes one wave to be its whole
/// thread group gets per-half results. The last resort for programs that
/// do not translate otherwise.
pub(in crate::emit) fn with_local_waves_any<R>(f: impl FnOnce() -> R) -> R {
    let was = LOCAL_ANY.replace(true);
    let r = with_local_waves(f);
    LOCAL_ANY.set(was);
    r
}

/// Runs `f` with console wave64 programs on 32-lane hosts lowered with each
/// host wave as a wave of its own ("local"): no exchange between the two
/// halves of a console wave, so no barriers and no need for every lane to
/// reach the code. What HLSL's wave operations promise holds for any
/// division of the threads into waves, and the idiom the title's compute
/// programs use them for does not care which wave a thread is in: count or
/// sum over the wave, the first lane reserves that many slots with an atomic
/// add, every lane takes the base (`WaveReadLaneFirst`) plus its prefix.
/// Each lane still gets a slot of its own; only the order differs.
/// Only that idiom's operations are lowered this way: a lane mask read as
/// a number by anything but a bit count, a first-set-bit or `v_mbcnt`, and
/// lane reads and writes outside whole-wave sections, stop the translation.
/// shortcut: where a count or sum goes is not checked. A program that uses
/// a wave-wide result as data for the whole console wave (a tile's maximum,
/// say, with one wave per thread group) gets its half's result. It is used
/// only for programs that do not translate otherwise.
pub(in crate::emit) fn with_local_waves<R>(f: impl FnOnce() -> R) -> R {
    let was = LOCAL_WAVES.replace(true);
    let r = f();
    LOCAL_WAVES.set(was);
    r
}

/// `OATHKEEP_F16_NEAREST=1`: the results of 16-bit float operations (`v_add_f16`,
/// `v_mul_f16`, `v_cvt_f16_f32` ...) are rounded to nearest as the console rounds them
/// ([`Lower::f16_nearest`]) instead of as the host's `f32tof16` does (toward zero on NVIDIA:
/// the last place of a result that is not exact is then one lower than the console's).
/// Off by default: it is exact but untried in a game run, and adds instructions to every
/// such operation. The wave sum of 16-bit floats ([`Lower::half_sum`]) is always exact.
pub(super) fn f16_nearest_on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_F16_NEAREST.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_F16_NEAREST").is_some_and(|v| v == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`f16_nearest_on`], for its thread only.
    pub(in crate::emit) static TEST_F16_NEAREST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `OATHKEEP_WAVE_OWN_BITS=0`: a wave OR where not every lane runs is refused as before
/// ([`super::own_bits`]).
pub(super) fn own_bits_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_WAVE_OWN_BITS").is_none_or(|v| v != "0"))
}

/// What the translator was told of the host, for a cache's key: the widest
/// wave, the view heap's limit, where the raw twins of its views start, and
/// what it writes for the console's own hardware.
pub fn host_settings() -> [u32; 4] {
    [host_max_wave(), crate::emit::shader::view_heap_limit(), crate::emit::shader::raw_views(), host_rdna()]
}

static HOST_RDNA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tells the translator that the host's shaders run on AMD RDNA hardware
/// ([`crate::emit::rdna_float`]).
pub fn set_host_rdna(on: bool) {
    HOST_RDNA.store(on, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`host_rdna`], for its thread only.
    pub(in crate::emit) static TEST_RDNA: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// The bits of [`super::rdna`] that count on this host.
pub(in crate::emit) fn host_rdna() -> u32 {
    #[cfg(test)]
    if let Some(bits) = TEST_RDNA.get() {
        return bits;
    }
    if HOST_RDNA.load(std::sync::atomic::Ordering::Relaxed) { crate::emit::rdna_float() } else { 0 }
}

/// The refusal of a first half of an interpolation whose result is read
/// as data ([`Lower::interp`]).
pub(in crate::emit) const FIRST_HALF: &str = "the first half of an interpolation";

thread_local! {
    /// Whether first halves of interpolations are evaluated as the pair
    /// they start ([`with_first_half_pairs`]).
    pub(super) static FIRST_HALF_PAIRS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `translate` with first halves of interpolations evaluated as the
/// pair they start: the translation of a program refused with
/// [`FIRST_HALF`] (`OATHKEEP_FIRST_HALF=0`: refused again, as before).
pub(in crate::emit) fn with_first_half_pairs<R>(translate: impl FnOnce() -> R) -> R {
    let before = FIRST_HALF_PAIRS.replace(true);
    let result = translate();
    FIRST_HALF_PAIRS.set(before);
    result
}

pub(in crate::emit) fn host_max_wave() -> u32 {
    #[cfg(test)]
    if let Some(lanes) = TEST_HOST_WAVE.get() {
        return lanes;
    }
    HOST_MAX_WAVE.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    /// A test's host wave size, for its thread only ([`set_host_max_wave`] is the process's).
    pub(in crate::emit) static TEST_HOST_WAVE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

pub(in crate::emit) fn reads_mask_as_data(st: &Structured, i: &Inst) -> bool {
    // A scalar select uses the condition of the whole guest wave, even
    // when the preceding mask operation has no scalar-dword consumers.
    if matches!(i.fields, Fields::Sop { .. } | Fields::Valu(_))
        && i.read(Reg::Scc).is_some_and(|s| st.scc_some.contains_key(&s))
    {
        return true;
    }
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let ops: Vec<&Operand> = match &i.fields {
        // A mask operation (its result a mask) is no data use.
        Fields::Sop { .. }
            if i.writes
                .iter()
                .any(|&(r, w)| matches!(r, Reg::S(_)) && is_mask(w)) =>
        {
            return false;
        }
        Fields::Sop { src, .. } => src.iter().collect(),
        Fields::Valu(v)
            if !matches!(i.name(), "v_cndmask_b32") && !i.name().contains("_co_ci_") =>
        {
            v.src.iter().collect()
        }
        _ => return false,
    };
    ops.into_iter().any(|op| match *op {
        Operand::Scalar { code, .. } if code != NULL => i.read(Reg::S(code)).is_some_and(is_mask),
        _ => false,
    })
}
