use super::shader::make;
use super::*;
use crate::backend::host;

/// Subtle warm-neutral indirect floor for dark interiors (breaks the zero-light
/// feedback loop without washing out shadows or outdoor contrast).
const INTERIOR_FLOOR: [f32; 3] = [0.015, 0.0145, 0.0135];
/// Minimum crease openness so deep corners and character facial creases keep
/// crisp shading without crushing to pitch-black soot.
const CREASE_FLOOR: f32 = 0.12;

const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn pack_r9g9b9e5(c: [f32; 3]) -> u32 {
    let c = c.map(|v| v.clamp(0.0, 65408.0));
    let top = c[0].max(c[1]).max(c[2]);
    let mut e = (top.max(1e-9).log2().floor() as i32 + 16).clamp(0, 31);
    let mut scale = (24 - e) as f32;
    scale = scale.exp2();
    if top * scale >= 511.5 {
        e = (e + 1).min(31);
        scale = ((24 - e) as f32).exp2();
    }
    let m = c.map(|v| ((v * scale + 0.5) as u32).min(511));
    m[0] | (m[1] << 9) | (m[2] << 18) | ((e as u32) << 27)
}

fn unpack_r9g9b9e5(v: u32) -> [f32; 3] {
    let scale = (((v >> 27) as f32) - 24.0).exp2();
    [
        (v & 511) as f32 * scale,
        ((v >> 9) & 511) as f32 * scale,
        ((v >> 18) & 511) as f32 * scale,
    ]
}

fn crease_openness(strength: f32, occlusion_mean: f32) -> f32 {
    let raw = (1.0 - strength * 2.0 * occlusion_mean).clamp(0.0, 1.0);
    CREASE_FLOOR + (1.0 - CREASE_FLOOR) * raw
}

fn evaluate_raise(
    all: [f32; 3],
    dir_dot_n: [f32; 3],
    slow_mean: [f32; 3],
    was: [f32; 3],
    level: f32,
    open: f32,
    most: f32,
    depth_m: f32,
) -> [f32; 3] {
    let mut found = [0.0f32; 3];
    for i in 0..3 {
        let directional = (all[i] + 1.2 * dir_dot_n[i]).clamp(0.2 * all[i], 2.5 * all[i]);
        let blended = (directional * 0.8 + slow_mean[i] * 0.2) * level * open;
        found[i] = if depth_m < 400.0 {
            blended.max(INTERIOR_FLOOR[i] * level * open)
        } else {
            blended
        };
    }
    let top = (dot3(found, LUMA) * most).max(1e-6);
    let has = dot3(was, LUMA);
    let scale = (top / has.max(1e-6)).min(1.0);
    [
        (was[0] * scale).max(found[0]),
        (was[1] * scale).max(found[1]),
        (was[2] * scale).max(found[2]),
    ]
}

fn clamp_air_haze(scattered: [f32; 3], slow_mean: [f32; 3], seen_there: [f32; 3], far: f32, air: f32) -> [f32; 3] {
    let room_luma = dot3(slow_mean, LUMA);
    let seen_luma = dot3(seen_there.map(|v| v.max(0.0)), LUMA);
    let cap = (air * (room_luma + (seen_luma - room_luma) * far)).max(0.0);
    let has = dot3(scattered, LUMA);
    if has > cap && has > 1e-6 {
        let k = cap / has;
        scattered.map(|v| v * k)
    } else {
        scattered
    }
}

#[test]
fn settings() {
    assert_eq!(parse(""), Set::Off);
    assert_eq!(parse("off"), Set::Off);
    assert_eq!(parse("0"), Set::Off);
    assert_eq!(parse("0.8"), Set::Estimate(0.8));
    assert_eq!(parse("constant:0.3"), Set::Constant(0.3));
    assert_eq!(parse("constant:nan"), Set::Off);
}

#[test]
fn defaults_enable_estimate_and_air_clamp_when_unset() {
    assert_eq!(resolve_setting(None), Set::Estimate(DEFAULT_AMBIENT));
    assert_eq!(resolve_setting(Some("0")), Set::Off);
    assert_eq!(resolve_setting(Some("off")), Set::Off);
    assert_eq!(resolve_setting(Some("1.0")), Set::Estimate(1.0));
    assert_eq!(resolve_air(None), DEFAULT_AIR);
    assert_eq!(resolve_air(Some("0")), 0.0);
    assert_eq!(resolve_air(Some("off")), 0.0);
    assert_eq!(resolve_air(Some("2.0")), 2.0);
}

#[test]
fn ambient_stand_in_math_preserves_contrast_and_clamps_indoor_haze() {
    // R9G9B9E5 pack/unpack preserves indoor lighting values accurately.
    let sample = [0.24f32, 0.21, 0.18];
    let rt = unpack_r9g9b9e5(pack_r9g9b9e5(sample));
    for i in 0..3 {
        assert!((rt[i] - sample[i]).abs() < 0.002, "channel {i}: {rt:?}");
    }

    // Pitch-black room (zero radiance, zero probe output) gets the subtle
    // AO-shaded interior floor rather than staying at literal 0.0.
    let dark_open = evaluate_raise([0.0; 3], [0.0; 3], [0.0; 3], [0.0; 3], 0.8, 1.0, 4.0, 3.0);
    let dark_crease = evaluate_raise([0.0; 3], [0.0; 3], [0.0; 3], [0.0; 3], 0.8, crease_openness(1.0, 0.6), 4.0, 3.0);
    assert!(dark_open[0] > 0.01 && dark_open[0] < 0.03);
    // Crease retains visible shading contrast without crushing to literal zero.
    assert!(dark_crease[0] > 0.0 && dark_crease[0] < dark_open[0] * 0.25);

    // Corrupt stale probe memory (`was` = 100.0) is bounded to `most` * estimate.
    let bounded = evaluate_raise([0.2; 3], [0.0; 3], [0.2; 3], [100.0; 3], 0.8, 1.0, 4.0, 5.0);
    assert!(dot3(bounded, LUMA) <= dot3([0.2 * 0.8; 3], LUMA) * 4.0 + 1e-4);

    // Bright sunset sky fog bleeding into a dim room is clamped cleanly,
    // and zero scattering never produces NaN.
    let clamped = clamp_air_haze([2.0, 1.0, 0.5], [0.1, 0.1, 0.1], [0.1, 0.1, 0.1], 0.2, DEFAULT_AIR);
    assert!(dot3(clamped, LUMA) <= 0.15 + 1e-5);
    let zero = clamp_air_haze([0.0; 3], [0.0; 3], [0.0; 3], 0.5, DEFAULT_AIR);
    assert!(zero.iter().all(|v| v.is_finite() && *v == 0.0));
}

#[test]
fn roles_by_name() {
    assert_eq!(role(b"..CS_ScreenProbes_TemporalResolve_a911b49c_Wrapped.."), Some(Role::Resolve));
    assert_eq!(role(b"..CS_CacheScreenSpaceRadiance_eed2fc35.."), Some(Role::Radiance));
    assert_eq!(role(b"CS_AccumulateLocalLightInscatteringVolume_7e2f5eef"), Some(Role::Air));
    assert_eq!(role(b"CS_TemporalResolve"), None);
}

/// The passes compile with the system's compiler and the host takes them.
#[test]
fn the_passes_build() {
    let Some(host) = host() else {
        return;
    };
    make(&host.device, |bytes| host.uav_buffer(bytes)).expect("the passes");
}
