use std::sync::atomic::{AtomicU64, Ordering};

use super::{HRESULT, S_OK, This};
#[cfg(all(windows, feature = "diagnostics"))]
use super::before_sync;
#[cfg(windows)]
use super::{behind, forces, release_held};

/// The start of a present plane: frame token, then the plane's resources.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
}

/// Retail PresentX adds source-rectangle and destination pointers, a color
/// space and seven option words after the common prefix. Older callers pass
/// only the prefix for single-plane presents.
#[cfg(windows)]
#[repr(C)]
struct RetailPlane {
    prefix: Plane,
    source_rects: *const (),
    destination: *const (),
    color_space: u32,
    options: [u32; 7],
}
#[cfg(windows)]
const _: () = assert!(size_of::<RetailPlane>() == 72);

#[cfg(windows)]
fn overlay_resource(parameters: *const Plane) -> Option<This> {
    // Read only the second prefix at the 72-byte retail PresentX stride. A
    // caller using a different layout can still present its base plane safely.
    let second = parameters.cast::<RetailPlane>().wrapping_add(1).cast::<u8>();
    let mut prefix = [0; size_of::<Plane>()];
    if !crate::host::read_into(second, &mut prefix) {
        return None;
    }
    // SAFETY: the prefix has the C layout above and its bytes were copied.
    let plane = unsafe { prefix.as_ptr().cast::<Plane>().read_unaligned() };
    let mut pointer = [0; size_of::<This>()];
    if plane.count == 0
        || plane.resources.is_null()
        || !crate::host::read_into(plane.resources.cast(), &mut pointer)
    {
        return None;
    }
    let resource = usize::from_ne_bytes(pointer) as This;
    crate::resource::is_resource(resource).then_some(resource)
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first plane's first resource in our window.
///
/// shortcut: one plane, no scaling or placement, the source expected in
/// the common state.
pub(super) extern "system" fn present(
    this: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    #[cfg(not(windows))]
    let _ = this;
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::copystat::present_begins();
    let called = crate::timeline::span("the title's Present");
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::title_thread();
    // Submissions sent to the card and not waited for are settled first,
    // and what that frees runs in its own order, before anything is forced.
    #[cfg(windows)]
    if behind::on() {
        // The present goes into the line of what the queues were given.
    } else if crate::host::late() && !crate::host::late_free_present() {
        crate::host::settle_for_present();
        release_held(false);
        crate::host::settle_for_present();
    } else if crate::host::late() {
        // The present is sent ahead too: what waits for a value a flight
        // will write runs behind that flight, and nothing is settled here.
        release_held(false);
    }
    // shortcut: what a queue still holds at a present runs now, whatever
    // its fence: a wait is not kept across frames.
    #[cfg(windows)]
    if !behind::on() {
        release_held(forces());
    }
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        crate::capture::frame_drew(frame, crate::draw::drawn());
        if crate::capture::listed(frame) {
            before_sync();
            crate::capture::dump_targets(frame);
        }
    }
    #[cfg(windows)]
    if behind::on() && !this.is_null() {
        const COPY_SOURCE: u32 = 0x800;
        let base_res = if planes > 0
            // SAFETY: the title passes `planes` plane descriptions.
            && let Some(plane) = unsafe { plane_parameters.as_ref() }
            && plane.count > 0
            && !plane.resources.is_null()
        {
            // SAFETY: the plane lists `count` resources.
            Some(unsafe { *plane.resources })
        } else {
            None
        };
        let take_copy = |resource: This| {
            crate::resource::host_copy(resource).map(|copy| {
                let state = crate::resource::host_transition(resource, COPY_SOURCE).map_or(COPY_SOURCE, |(_, before)| before);
                crate::resource::host_transition(resource, state);
                // (Lists not translated yet leave it in no state yet: the
                // one it is in when its turn in the line comes.)
                let state = if crate::list::thin::wanted() { u32::MAX } else { state };
                (resource as usize, copy, state)
            })
        };
        let texture = base_res.and_then(take_copy);
        let overlay = (planes > 1 && !plane_parameters.is_null())
            .then(|| overlay_resource(plane_parameters))
            .flatten()
            .filter(|&over_res| base_res != Some(over_res))
            .and_then(take_copy);
        let resource = texture.as_ref().map_or(0, |t| t.0);
        behind::present(
            this,
            texture.map(|(_, copy, state)| (copy, state)),
            overlay.map(|(_, copy, state)| (copy, state)),
            Shown { frame, resource },
        );
        called.end();
        #[cfg(feature = "diagnostics")]
        frame_ends();
        return S_OK;
    }
    #[cfg(windows)]
    if planes > 0
        // SAFETY: the title passes `planes` plane descriptions.
        && let Some(plane) = unsafe { plane_parameters.as_ref() }
        && plane.count > 0
        && !plane.resources.is_null()
        // SAFETY: the plane lists `count` resources.
        && let resource = unsafe { *plane.resources }
        && let Some(texture) = crate::resource::host_resource(resource)
        && let Some(host) = crate::host::host()
    {
        const COPY_SOURCE: u32 = 0x800;
        let state = crate::resource::host_transition(resource, COPY_SOURCE)
            .map_or(COPY_SOURCE, |(_, before)| before);
        let overlay = (planes > 1)
            .then(|| overlay_resource(plane_parameters))
            .flatten()
            .filter(|&over_res| over_res != resource)
            .and_then(|over_res| {
                crate::resource::host_resource(over_res).map(|over_tex| {
                    let over_state = crate::resource::host_transition(over_res, COPY_SOURCE)
                        .map_or(COPY_SOURCE, |(_, before)| before);
                    (over_res, over_tex, over_state)
                })
            });
        show_planes(
            texture,
            state,
            overlay.as_ref().map(|(_, tex, st)| (*tex, *st)),
            Shown { frame, resource: resource as usize },
        );
        crate::resource::host_transition(resource, state);
        if let Some((over_res, _, over_state)) = overlay {
            crate::resource::host_transition(over_res, over_state);
        }
        let _ = host;
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters);
    called.end();
    #[cfg(all(windows, feature = "diagnostics"))]
    frame_ends();
    S_OK
}

/// Diagnostics: the title's present is over.
#[cfg(all(windows, feature = "diagnostics"))]
fn frame_ends() {
    crate::timeline::presented();
    crate::faultbook::frame();
    crate::ab::presented(|switch, was, will| {
        if switch == "OATHKEEP_ASYNC_SUBMIT" && was && !will {
            behind::leave();
        }
    });
}

/// The frame a present shows, and the title's resource it shows.
#[cfg(windows)]
#[derive(Clone, Copy)]
pub(super) struct Shown {
    frame: u64,
    resource: usize,
}

/// Shows `texture` (with optional `overlay` plane blended over it) in our window.
#[cfg(windows)]
pub(super) fn show_planes(
    texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    state: u32,
    overlay: Option<(&windows::Win32::Graphics::Direct3D12::ID3D12Resource, u32)>,
    what: Shown,
) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let shown = host.present_planes(texture, state, overlay, what.frame);
    #[cfg(feature = "diagnostics")]
    blinks(what.frame, what.resource as This, state, shown);
    #[cfg(not(feature = "diagnostics"))]
    let _ = (shown, what.resource);
}

/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}
