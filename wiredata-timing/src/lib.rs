//! Shared process timing policy for Talker and Listener.
//!
//! Windows timer resolution is process-wide, so both applications use the same
//! ref-counted RAII implementation. Other platforms retain native deadline waits.

use std::sync::Mutex;
#[cfg(windows)]
use std::sync::Once;

#[derive(Clone, Copy, Debug, Default)]
struct ResolutionState {
    holders: usize,
    effective: bool,
}

struct ResolutionCounter(Mutex<ResolutionState>);

impl ResolutionCounter {
    const fn new() -> Self {
        Self(Mutex::new(ResolutionState {
            holders: 0,
            effective: false,
        }))
    }

    fn acquire(&self, raise: impl FnOnce() -> bool) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.holders == 0 {
            state.effective = raise();
        }
        state.holders = state.holders.saturating_add(1);
        state.effective
    }

    fn release(&self, lower: impl FnOnce()) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(state.holders > 0, "timer-resolution guard underflow");
        if state.holders == 0 {
            return;
        }
        state.holders -= 1;
        if state.holders == 0 {
            if state.effective {
                lower();
            }
            state.effective = false;
        }
    }
}

static ACTIVE: ResolutionCounter = ResolutionCounter::new();

/// Holds a process-wide 1 ms Windows timer-resolution request. Off Windows the
/// guard is an effective no-op so callers can keep one policy shape.
#[must_use = "the resolution request is released when the guard drops"]
pub struct HighResolutionGuard {
    effective: bool,
}

impl HighResolutionGuard {
    /// Whether the platform request took effect. Always true on native-wait platforms.
    pub fn is_effective(&self) -> bool {
        self.effective
    }
}

/// Whether this platform needs and supports a process timer-resolution request.
pub const fn supports_timer_resolution_request() -> bool {
    cfg!(windows)
}

/// Request 1 ms Windows timer resolution for the returned guard's lifetime.
pub fn high_resolution() -> HighResolutionGuard {
    HighResolutionGuard {
        effective: ACTIVE.acquire(raise),
    }
}

impl Drop for HighResolutionGuard {
    fn drop(&mut self) {
        ACTIVE.release(lower);
    }
}

#[cfg(windows)]
fn raise() -> bool {
    // SAFETY: this WinMM call takes only an integer period. The matching release
    // is serialized by `ResolutionCounter` and process termination is a final backstop.
    let result = unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
    result == windows_sys::Win32::Media::TIMERR_NOERROR
}

#[cfg(windows)]
fn lower() {
    // SAFETY: paired with the successful `timeBeginPeriod(1)` transition above.
    unsafe {
        windows_sys::Win32::Media::timeEndPeriod(1);
    }
}

#[cfg(not(windows))]
fn raise() -> bool {
    true
}

#[cfg(not(windows))]
fn lower() {}

/// Opt out of Windows 11's minimized/occluded timer-resolution throttling once
/// at process startup. No-op on non-Windows systems and harmless when unsupported.
pub fn keep_timer_resolution_when_minimized() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        };
        static CONFIGURE: Once = Once::new();
        CONFIGURE.call_once(|| {
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                StateMask: 0,
            };
            // SAFETY: the pointer and size describe `state` for the duration of the call.
            unsafe {
                SetProcessInformation(
                    GetCurrentProcess(),
                    ProcessPowerThrottling,
                    std::ptr::from_ref(&state).cast(),
                    std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn refcount_raises_once_and_lowers_at_zero() {
        let counter = ResolutionCounter::new();
        let raises = Cell::new(0);
        let lowers = Cell::new(0);
        assert!(counter.acquire(|| {
            raises.set(raises.get() + 1);
            true
        }));
        assert!(counter.acquire(|| {
            raises.set(raises.get() + 1);
            true
        }));
        assert_eq!(raises.get(), 1);
        counter.release(|| lowers.set(lowers.get() + 1));
        assert_eq!(lowers.get(), 0);
        counter.release(|| lowers.set(lowers.get() + 1));
        assert_eq!(lowers.get(), 1);
    }

    #[test]
    fn failed_raise_is_shared_and_never_lowered() {
        let counter = ResolutionCounter::new();
        let raises = Cell::new(0);
        let lowers = Cell::new(0);
        assert!(!counter.acquire(|| {
            raises.set(raises.get() + 1);
            false
        }));
        assert!(!counter.acquire(|| true));
        counter.release(|| lowers.set(lowers.get() + 1));
        counter.release(|| lowers.set(lowers.get() + 1));
        assert_eq!(raises.get(), 1);
        assert_eq!(lowers.get(), 0);
    }

    #[test]
    fn real_guard_nests_without_underflow() {
        let first = high_resolution();
        let second = high_resolution();
        drop(first);
        drop(second);
    }
}
