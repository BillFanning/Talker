//! OS timer-resolution management for automatic high-rate and explicit
//! precision schedules (ADR-017, ADR-034).
//!
//! The runner's deadline waits (`recv_deadline`, ADR-002) wake on the OS
//! scheduler tick — **15.625 ms** by default on Windows. That is finer than
//! any sensible sleep for a 10 Hz schedule, but *coarser than the interval*
//! of a 100 Hz one: every wake arrives more than one interval late, the
//! stall policy skips the backlog (spec §8.1), and ~40% of the grid never
//! fires. Requesting 1 ms resolution (`timeBeginPeriod(1)`) while a
//! sub-[`HIGH_RATE_THRESHOLD`] schedule is running fixes the miss rate.
//!
//! A channel in [`TimingMode::Precise`] also stages a coarse wait, requests
//! high resolution for the final [`PRECISION_WINDOW`] before a slower send,
//! then releases it after the deadline wake. This tightens deadline wakes without
//! holding the Windows power policy continuously. [`CadenceAlignment`] is the
//! separate choice that can phase deadlines to UTC; neither setting makes the wall
//! clock itself more accurate or proves when bytes reach the wire.
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
//! The shared resolution guard is an effective no-op off Windows because native
//! deadline waits are not governed by the Windows timer period. A future macOS App
//! Nap activity policy is separate from timer resolution; see talker TODO.md.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use wiredata_timing::{
    high_resolution, keep_timer_resolution_when_minimized, HighResolutionGuard,
};

/// Intervals shorter than this want 1 ms timer resolution: two default
/// Windows scheduler ticks (2 × 15.625 ms), the point below which a deadline
/// wait can no longer be trusted to land inside its own interval.
pub const HIGH_RATE_THRESHOLD: Duration = Duration::from_millis(32);

/// Lead time used by explicit precision mode. Two default Windows timer
/// ticks give the coarse first wait enough margin to enter the final window
/// before the actual send deadline.
pub const PRECISION_WINDOW: Duration = HIGH_RATE_THRESHOLD;

/// How a Channel establishes each message's cadence phase when a run starts.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CadenceAlignment {
    /// Preserve existing behavior: every active message is due immediately.
    #[default]
    Immediate,
    /// First fire at the next UTC/Unix-epoch interval boundary, then continue
    /// on a monotonic grid. Wall-clock steps rebase future deadlines only.
    UtcPhase,
}

impl CadenceAlignment {
    pub fn is_immediate(&self) -> bool {
        *self == Self::Immediate
    }
}

/// A channel's configured cadence-wait policy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingMode {
    /// Automatically request finer Windows resolution only for schedules
    /// whose shortest active interval is below [`HIGH_RATE_THRESHOLD`].
    #[default]
    Standard,
    /// For slower schedules, request finer Windows resolution only during
    /// the final [`PRECISION_WINDOW`] before each deadline.
    Precise,
}

impl TimingMode {
    pub fn is_standard(&self) -> bool {
        *self == Self::Standard
    }
}

/// Internal wait strategy selected from channel intent and active cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimerIntent {
    None,
    ContinuousHighRate,
    PrecisionWindow,
}

impl TimerIntent {
    fn reason(self) -> TimerReason {
        match self {
            Self::None => TimerReason::None,
            Self::ContinuousHighRate => TimerReason::HighRate,
            Self::PrecisionWindow => TimerReason::PrecisionWindow,
        }
    }
}

pub(crate) fn timer_intent(
    timing_mode: TimingMode,
    shortest_active_interval: Option<Duration>,
) -> TimerIntent {
    match shortest_active_interval {
        None => TimerIntent::None,
        Some(interval) if interval < HIGH_RATE_THRESHOLD => TimerIntent::ContinuousHighRate,
        Some(_) if timing_mode == TimingMode::Precise => TimerIntent::PrecisionWindow,
        Some(_) => TimerIntent::None,
    }
}

/// One blocking deadline selected for the runner's next command wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitPlan {
    /// One native deadline wait; no resolution staging is needed.
    Direct(Instant),
    /// Coarse wait to the beginning of the precision window.
    Stage(Instant),
    /// Final wait while the platform timer-resolution guard is held.
    Precision(Instant),
}

pub(crate) fn wait_plan(intent: TimerIntent, now: Instant, deadline: Instant) -> WaitPlan {
    wait_plan_for_platform(intent, now, deadline, cfg!(windows))
}

fn wait_plan_for_platform(
    intent: TimerIntent,
    now: Instant,
    deadline: Instant,
    supports_resolution_request: bool,
) -> WaitPlan {
    if intent != TimerIntent::PrecisionWindow || !supports_resolution_request {
        return WaitPlan::Direct(deadline);
    }

    let window_start = deadline.checked_sub(PRECISION_WINDOW).unwrap_or(now);
    if now < window_start {
        WaitPlan::Stage(window_start)
    } else {
        WaitPlan::Precision(deadline)
    }
}

/// The deadline-wait policy currently in effect for one channel runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimerMode {
    /// No active interval needs a platform-specific timer-resolution request.
    #[default]
    Standard,
    /// Windows accepted the process-wide 1 ms timer-resolution request.
    WindowsOneMillisecond,
    /// Windows rejected the process-wide 1 ms timer-resolution request.
    WindowsRequestFailed,
    /// This platform's native deadline waits are used; no Windows-style
    /// process-wide resolution request is needed.
    NativeDeadlineWaits,
}

/// Why the current channel selected its timer policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimerReason {
    #[default]
    None,
    /// The shortest active interval is below [`HIGH_RATE_THRESHOLD`], so the
    /// resolution request is continuous regardless of configured mode.
    HighRate,
    /// Explicit [`TimingMode::Precise`] uses final deadline windows.
    PrecisionWindow,
}

/// User-facing timer policy plus the schedule input that selected it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerStatus {
    pub mode: TimerMode,
    pub timing_mode: TimingMode,
    pub reason: TimerReason,
    pub shortest_active_interval: Option<Duration>,
    pub cadence_alignment: CadenceAlignment,
    pub clock_realignments: u64,
}

/// Describe the timer policy after the runner has reconciled its guard with
/// the current schedule.
pub(crate) fn timer_status(
    timing_mode: TimingMode,
    intent: TimerIntent,
    shortest_active_interval: Option<Duration>,
    guard: Option<&HighResolutionGuard>,
    previous_mode: TimerMode,
    cadence_alignment: CadenceAlignment,
    clock_realignments: u64,
) -> TimerStatus {
    let mode = match intent {
        TimerIntent::None => TimerMode::Standard,
        TimerIntent::ContinuousHighRate | TimerIntent::PrecisionWindow => {
            #[cfg(windows)]
            {
                match guard {
                    Some(guard) if guard.is_effective() => TimerMode::WindowsOneMillisecond,
                    Some(_) => TimerMode::WindowsRequestFailed,
                    None if intent == TimerIntent::PrecisionWindow => match previous_mode {
                        TimerMode::WindowsOneMillisecond | TimerMode::WindowsRequestFailed => {
                            previous_mode
                        }
                        _ => TimerMode::Standard,
                    },
                    None => TimerMode::WindowsRequestFailed,
                }
            }
            #[cfg(not(windows))]
            {
                let _ = (guard, previous_mode);
                TimerMode::NativeDeadlineWaits
            }
        }
    };
    TimerStatus {
        mode,
        timing_mode,
        reason: intent.reason(),
        shortest_active_interval,
        cadence_alignment,
        clock_realignments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_preserves_automatic_high_rate_and_adds_explicit_precision() {
        assert_eq!(timer_intent(TimingMode::Standard, None), TimerIntent::None);
        assert_eq!(
            timer_intent(TimingMode::Precise, None),
            TimerIntent::None,
            "dormant messages never hold timer resolution"
        );
        assert_eq!(
            timer_intent(TimingMode::Standard, Some(Duration::from_millis(10))),
            TimerIntent::ContinuousHighRate
        );
        assert_eq!(
            timer_intent(TimingMode::Precise, Some(Duration::from_secs(1))),
            TimerIntent::PrecisionWindow
        );
        assert_eq!(
            timer_intent(TimingMode::Standard, Some(Duration::from_secs(1))),
            TimerIntent::None
        );
    }

    #[test]
    fn precision_wait_stages_before_the_final_deadline_window() {
        let now = std::time::Instant::now();
        let deadline = now + Duration::from_secs(1);
        assert_eq!(
            wait_plan_for_platform(TimerIntent::PrecisionWindow, now, deadline, true),
            WaitPlan::Stage(deadline - PRECISION_WINDOW)
        );
        assert_eq!(
            wait_plan_for_platform(
                TimerIntent::PrecisionWindow,
                deadline - Duration::from_millis(10),
                deadline,
                true,
            ),
            WaitPlan::Precision(deadline)
        );
        assert_eq!(
            wait_plan_for_platform(TimerIntent::PrecisionWindow, now, deadline, false),
            WaitPlan::Direct(deadline),
            "native high-resolution waits need no staging wake"
        );
        assert_eq!(
            wait_plan_for_platform(TimerIntent::None, now, deadline, true),
            WaitPlan::Direct(deadline)
        );
    }

    #[test]
    fn timer_status_explains_standard_high_rate_and_windowed_waits() {
        let standard = timer_status(
            TimingMode::Standard,
            TimerIntent::None,
            Some(HIGH_RATE_THRESHOLD),
            None,
            TimerMode::Standard,
            CadenceAlignment::Immediate,
            0,
        );
        assert_eq!(standard.mode, TimerMode::Standard);
        assert_eq!(standard.reason, TimerReason::None);
        assert_eq!(standard.shortest_active_interval, Some(HIGH_RATE_THRESHOLD));

        let guard = high_resolution();
        let fast = timer_status(
            TimingMode::Standard,
            TimerIntent::ContinuousHighRate,
            Some(Duration::from_millis(1)),
            Some(&guard),
            TimerMode::Standard,
            CadenceAlignment::Immediate,
            0,
        );
        assert_eq!(fast.reason, TimerReason::HighRate);
        #[cfg(windows)]
        assert_eq!(
            fast.mode,
            if guard.is_effective() {
                TimerMode::WindowsOneMillisecond
            } else {
                TimerMode::WindowsRequestFailed
            }
        );
        #[cfg(not(windows))]
        assert_eq!(fast.mode, TimerMode::NativeDeadlineWaits);

        let retained = timer_status(
            TimingMode::Precise,
            TimerIntent::PrecisionWindow,
            Some(Duration::from_secs(1)),
            None,
            TimerMode::WindowsOneMillisecond,
            CadenceAlignment::UtcPhase,
            2,
        );
        assert_eq!(retained.timing_mode, TimingMode::Precise);
        assert_eq!(retained.reason, TimerReason::PrecisionWindow);
        assert_eq!(retained.cadence_alignment, CadenceAlignment::UtcPhase);
        assert_eq!(retained.clock_realignments, 2);
        #[cfg(windows)]
        assert_eq!(retained.mode, TimerMode::WindowsOneMillisecond);
        #[cfg(not(windows))]
        assert_eq!(retained.mode, TimerMode::NativeDeadlineWaits);
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
