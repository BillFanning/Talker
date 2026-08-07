//! Timing and runtime details: the work behind a send, and the timer policy that
//! was actually applied.
//!
//! Separate from the Cadence row deliberately. Cadence answers "is the schedule
//! being kept"; this answers "what did the work cost, under what policy".

use super::*;

pub(in crate::gui) const TIMING_TOOLTIP: &str = "Each recent snapshot merges up to approximately ten seconds of \
fixed one-second segments ending when the runner captured its latest counter update. A slow or \
dormant schedule can therefore leave the displayed snapshot unchanged; its “as of” age is the \
snapshot compute time, not necessarily the newest sample time. While running, a snapshot is no \
longer used as recent evidence once that age reaches ten seconds. After a normal run end, the \
channel retains its exact final snapshot; an abnormal exit can leave the last non-final snapshot \
instead. Deadline lateness runs from a message's monotonic cadence deadline \
until the runner handles that send; handled retry-suppressed sends are included, while \
cadence points counted as Missed are not sampled. Render covers payload and timestamp construction. \
Send call ends when the configured-interface write returns and includes failed attempts; it does \
not prove physical-wire or peer delivery. Each boundary reports its own worst value and its own \
sample count; a percentile is added only where it differs from that worst value, which below a \
hundred samples it never does. Percentiles are histogram-bucket upper bounds.";

pub(in crate::gui) const TIMER_TOOLTIP: &str =
    "The shortest active interval selects the deadline-wait policy. On \
Windows, intervals below 32 ms hold the shared process-wide 1 ms timer-resolution request in \
either mode. At 32 ms or longer, Standard uses ordinary deadline waits; Precise requests 1 ms \
only for the final 32 ms before a waited deadline. An Immediate schedule's first send has \
no preceding precision window. This channel releases a bounded-window request before rendering \
and writing, although another channel may keep the process-wide request active. Commands interrupt \
both wait stages. Other platforms use native deadline waits. This affects wake timing, not \
timestamp accuracy or physical-wire arrival.";

pub(in crate::gui) const ALIGNMENT_TOOLTIP: &str =
    "Immediate makes every active message due when the interface \
opens. UTC phase places each first application deadline on the strict next Unix-epoch multiple of \
its interval, then advances on monotonic deadlines. For example, 1000 ms aligns to whole UTC \
seconds, while 1500 ms alternates between whole- and half-second phases. When the runner loops, it \
compares wall clock with its elapsed-time projection no more often than once per second; a \
displacement of at least 250 ms rebuilds future deadlines without replaying bypassed points or \
adding scheduler misses. This aligns application deadlines, not completion of an interface write \
or physical-wire arrival.";

pub(in crate::gui) fn timing_detail_text(
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
    snapshot_state: RecentSnapshotState,
) -> String {
    let run_samples = cumulative.deadline_lateness.sample_count();
    if run_samples == 0 {
        return "Work per send: awaiting first send".to_owned();
    }

    let run_max = cumulative
        .deadline_lateness
        .max()
        .map(compact_duration)
        .unwrap_or_else(|| "n/a".to_owned());
    match snapshot_state {
        RecentSnapshotState::Expired(_) => format!(
            "Work per send: {} · run max late {run_max}",
            recent_snapshot_label(snapshot_state)
        ),
        RecentSnapshotState::Pending => {
            format!("Work per send: recent snapshot pending · run max late {run_max}")
        }
        // Deadline lateness is deliberately absent: the Cadence row renders the
        // same recent histogram with the schedule context that makes it
        // readable, so repeating it here was one fact in two places. What is
        // left is what nothing else shows — how long the two stages of the work
        // itself took, channel-wide and recently, since the per-message table is
        // cumulative — plus the run-wide lateness ceiling the Cadence row omits
        // while a current window is available.
        RecentSnapshotState::Current(_) | RecentSnapshotState::Final => {
            // Both boundaries share one population, so the count belongs to the
            // line rather than to each figure on it.
            let samples = recent.send_duration.sample_count();
            format!(
                "Work per send ({}, {} sends): {} · {} · run max late {run_max}",
                recent_snapshot_label(snapshot_state),
                thousands(samples),
                timing_metric("render", recent.render_duration, samples),
                timing_metric("send call", recent.send_duration, samples),
            )
        }
    }
}

pub(in crate::gui) fn timer_status_detail(status: TimerStatus) -> (String, bool) {
    let shortest = status
        .shortest_active_interval()
        .map(compact_duration)
        .unwrap_or_else(|| "no active messages".to_owned());
    match (status.reason, status.mode) {
        (TimerReason::HighRate, TimerMode::WindowsOneMillisecond) => (
            format!("Windows 1 ms continuous (high rate) · shortest {shortest}"),
            false,
        ),
        (TimerReason::HighRate, TimerMode::WindowsRequestFailed) => (
            format!("Windows 1 ms request failed (high rate) · shortest {shortest}"),
            true,
        ),
        (TimerReason::HighRate, TimerMode::NativeDeadlineWaits) => (
            format!("native deadline waits (high rate) · shortest {shortest}"),
            false,
        ),
        (TimerReason::HighRate, TimerMode::Standard) => (
            format!("high-rate timer request pending · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::WindowsOneMillisecond) => (
            format!("Windows 1 ms deadline windows · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::WindowsRequestFailed) => (
            format!("Windows 1 ms request failed · shortest {shortest}"),
            true,
        ),
        (TimerReason::PrecisionWindow, TimerMode::NativeDeadlineWaits) => (
            format!("native deadline waits · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::Standard) => (
            format!("first waited deadline window pending · shortest {shortest}"),
            false,
        ),
        (TimerReason::None, _) if status.active_cadence.is_some() => {
            (format!("standard deadline waits · {shortest}"), false)
        }
        (TimerReason::None, _) => (
            "idle · no active messages; no timer request".to_owned(),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::timing::{
        ActiveCadence, CadenceAlignment, TimerMode, TimerReason, TimerStatus,
    };

    #[test]
    fn precise_near_threshold_status_names_the_deadline_window_policy() {
        let (detail, hot) = timer_status_detail(TimerStatus {
            mode: TimerMode::WindowsOneMillisecond,
            reason: TimerReason::PrecisionWindow,
            active_cadence: Some(ActiveCadence {
                messages: 1,
                shortest: Duration::from_millis(50),
            }),
            cadence_alignment: CadenceAlignment::Immediate,
            clock_realignments: 0,
        });

        assert!(detail.contains("deadline windows"), "{detail}");
        assert!(!detail.contains("continuous"), "{detail}");
        assert!(!hot);
    }

    #[test]
    fn dormant_timer_status_names_idle_state_and_no_request() {
        // One idle state now, not two: with no configured mode there is no
        // "Precise selected but idle" to distinguish from plain idle.
        let (standard, hot) = timer_status_detail(TimerStatus::default());
        assert_eq!(standard, "idle · no active messages; no timer request");
        assert!(!hot);
    }
}
