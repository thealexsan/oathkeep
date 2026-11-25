#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

const TIMEOUT: HRESULT = S_FALSE;
const ORIGIN: u32 = 0;
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    tracing::info!(?interval, "frame interval");
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

fn next_tick(kind: u32) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let tick =
        (c.first + (elapsed / interval + 1) as u64).max(last(kind).load(Ordering::Relaxed) + 1);
    let at = u128::from(tick - c.first) * interval;
    (
        tick,
        Duration::from_nanos(at.saturating_sub(elapsed) as u64),
    )
}

pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    let (mut tick, mut wait) = next_tick(kind);
    let limit = Duration::from_millis(u64::from(timeout_ms));
    static SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*SLEEP.get_or_init(|| std::env::var_os("OATHKEEP_LATE_WAITS").is_some_and(|v| v == "1"))
        && limit >= wait
        && tick > last(kind).load(Ordering::Relaxed) + 1
    {
        tick -= 1;
        wait = Duration::ZERO;
    }
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        std::thread::sleep(wait);
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        assert!(before.elapsed() <= Duration::from_millis(50));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        const FLIP: u32 = 2;
        assert_eq!(set_interval(null, null, 5_000, 3, 0), S_OK);
        let mut token = 0;
        let flips: Vec<u64> = (0..4)
            .map(|_| {
                let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                assert_eq!(
                    wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                    S_OK
                );
                LAST[2].load(Ordering::Relaxed)
            })
            .collect();
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        let (tick, _) = next_tick(FLIP);
        assert!(tick > flips[3]);
    }
}
