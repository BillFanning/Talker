//! OS timer-resolution management for high-rate schedules (ADR-017).
//!
//! The runner's deadline waits (`recv_deadline`, ADR-002) wake on the OS
//! scheduler tick — **15.625 ms** by default on Windows. That is finer than
//! any sensible sleep for a 10 Hz schedule, but *coarser than the interval*
//! of a 100 Hz one: every wake arrives more than one interval late, the
//! stall policy skips the backlog (spec §8.1), and ~40% of the grid never
//! fires. Requesting 1 ms resolution (`timeBeginPeriod(1)`) while — and only
//! while — a sub-[`HIGH_RATE_THRESHOLD`] schedule is running fixes the miss
//! rate without paying the idle power cost.
//!
//! - **No elevation needed**; the request is per-process since Windows 10
//!   2004, so other processes never see it.
//! - **Leak-proof across death**: the kernel releases a process's resolution
//!   request on termination (including a kill), so the RAII guard here is
//!   about releasing *early* when the last fast channel stops, not about
//!   correctness after a crash.
//! - **Windows 11 throttling**: by default the OS ignores the request while
//!   the process window is minimized/occluded — exactly when a long soak
//!   runs. [`keep_timer_resolution_when_minimized`] opts out once at startup.
//!
//! Everything is a no-op off Windows for now (Unix futex waits are not bound
//! to a 15 ms tick, so there is no resolution to raise). When the planned
//! macOS target lands, [`raise`]/[`lower`] are the slot for its App Nap
//! opt-out (hold an `NSProcessInfo` latency-critical activity token while
//! any high-rate schedule runs) — see talker TODO.md, "macOS target".

use std::sync::Mutex;
use std::time::Duration;

/// Intervals shorter than this want 1 ms timer resolution: two default
/// Windows scheduler ticks (2 × 15.625 ms), the point below which a deadline
/// wait can no longer be trusted to land inside its own interval.
pub const HIGH_RATE_THRESHOLD: Duration = Duration::from_millis(32);

/// Refcount that fires `raise` on 0→1 and `lower` on 1→0. One lock around
/// the count *and* the OS call, so a concurrent acquire and release can't
/// reorder the raise/lower pair (an atomic counter could lower after a fresh
/// raise, leaving a live holder without the resolution it asked for).
struct ResolutionCounter(Mutex<usize>);

impl ResolutionCounter {
    const fn new() -> Self {
        Self(Mutex::new(0))
    }

    fn acquire(&self, raise: impl FnOnce()) {
        let mut n = self.0.lock().unwrap();
        if *n == 0 {
            raise();
        }
        *n += 1;
    }

    fn release(&self, lower: impl FnOnce()) {
        let mut n = self.0.lock().unwrap();
        *n -= 1;
        if *n == 0 {
            lower();
        }
    }
}

static ACTIVE: ResolutionCounter = ResolutionCounter::new();

/// Holds the process's 1 ms timer-resolution request. Acquired by a runner
/// whose schedule has an interval below [`HIGH_RATE_THRESHOLD`]; the OS
/// request is raised by the first holder and released by the last.
#[must_use = "the resolution is released when the guard drops"]
pub struct HighResolutionGuard(());

/// Request 1 ms OS timer resolution for the life of the returned guard.
pub fn high_resolution() -> HighResolutionGuard {
    ACTIVE.acquire(raise);
    HighResolutionGuard(())
}

impl Drop for HighResolutionGuard {
    fn drop(&mut self) {
        ACTIVE.release(lower);
    }
}

#[cfg(windows)]
fn raise() {
    // SAFETY: takes an integer, no pointers; TIMERR_NOCANDO (out-of-range) is
    // the only failure and 1 ms is always in range on supported Windows.
    let r = unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
    if r == windows_sys::Win32::Media::TIMERR_NOERROR {
        tracing::info!("timer resolution raised to 1 ms (high-rate schedule active)");
    } else {
        tracing::warn!("timeBeginPeriod(1) failed ({r}) — high-rate sends may miss cadence");
    }
}

#[cfg(windows)]
fn lower() {
    // SAFETY: as above. Must pair the raise; the kernel would also clean up
    // on process exit, this just stops paying the power cost early.
    unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
    tracing::info!("timer resolution restored to the OS default (no high-rate schedule active)");
}

#[cfg(not(windows))]
fn raise() {}

#[cfg(not(windows))]
fn lower() {}

/// Opt out of Windows 11's timer-resolution throttling for minimized /
/// occluded windows, once at process start. Without this a long soak run
/// minimized to the taskbar silently falls back to 15.625 ms wakes — the
/// exact miss-rate failure the guard exists to prevent, but intermittent.
/// Needs no elevation; fails harmlessly (and quietly) on Windows 10, which
/// doesn't throttle by window state in the first place. No-op off Windows.
pub fn keep_timer_resolution_when_minimized() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            // ControlMask names the policy being configured; StateMask 0
            // disables it (= never ignore our timer-resolution request).
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        // SAFETY: the pointer and size describe the local `state` for the
        // duration of the call only.
        let ok = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                std::ptr::from_ref(&state).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok == 0 {
            // Expected on Windows 10 (class unsupported) — debug, not warn.
            tracing::debug!("timer-resolution throttling opt-out unavailable on this Windows");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn refcount_raises_once_and_lowers_at_zero() {
        // A local counter (not ACTIVE) so parallel tests that start real
        // runners can't perturb the counts under assertion.
        let c = ResolutionCounter::new();
        let raises = Cell::new(0);
        let lowers = Cell::new(0);
        c.acquire(|| raises.set(raises.get() + 1));
        c.acquire(|| raises.set(raises.get() + 1)); // nested: no second raise
        assert_eq!(raises.get(), 1);
        c.release(|| lowers.set(lowers.get() + 1));
        assert_eq!(lowers.get(), 0); // still one holder
        c.release(|| lowers.set(lowers.get() + 1));
        assert_eq!(lowers.get(), 1);
        // A fresh cycle raises again.
        c.acquire(|| raises.set(raises.get() + 1));
        assert_eq!(raises.get(), 2);
        c.release(|| lowers.set(lowers.get() + 1));
        assert_eq!(lowers.get(), 2);
    }

    #[test]
    fn guard_acquires_and_releases_the_real_request() {
        // Exercises the real timeBeginPeriod/timeEndPeriod path (a no-op off
        // Windows); nesting and dropping must not panic or underflow.
        let a = high_resolution();
        let b = high_resolution();
        drop(a);
        drop(b);
    }
}
