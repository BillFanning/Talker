//! Bounded cumulative timing measurements for the send hot path.
//!
//! Histograms retain fixed-size bucket counts, not individual samples. That
//! keeps recording allocation-free and makes a cumulative snapshot cheap to
//! copy through the existing observer lane.

use std::time::{Duration, Instant};

pub(crate) use wiredata_telemetry::RecentDurationHistogram;
pub use wiredata_telemetry::{DurationHistogram, RECENT_WINDOW};

/// Whether the observer's last collapsed recent-window snapshot is suitable
/// for live decisions.
///
/// A running snapshot expires once its capture age reaches the window it
/// summarized. A stopped channel retains its last exact-at-stop snapshot as
/// final evidence rather than aging it as though the runner were still live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecentSnapshotState {
    Pending,
    Current(Duration),
    Expired(Duration),
    Final,
}

/// Classify a collapsed recent-window snapshot from its exact compute instant.
///
/// `observed_at` can precede `captured_at` only through a caller race or a
/// synthetic test; saturating that age to zero keeps presentation robust.
/// `final_snapshot` is provenance carried by the runner's exact-at-rest
/// counter update; it is not inferred from thread or UI lifecycle.
pub fn recent_snapshot_state(
    captured_at: Option<Instant>,
    observed_at: Instant,
    final_snapshot: bool,
) -> RecentSnapshotState {
    let Some(captured_at) = captured_at else {
        return RecentSnapshotState::Pending;
    };
    if final_snapshot {
        return RecentSnapshotState::Final;
    }

    let age = observed_at
        .checked_duration_since(captured_at)
        .unwrap_or_default();
    if age >= RECENT_WINDOW {
        RecentSnapshotState::Expired(age)
    } else {
        RecentSnapshotState::Current(age)
    }
}

/// Cumulative timing boundaries observed by one channel runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendTimingTelemetry {
    /// Delay from a message's monotonic schedule deadline until it is handled.
    pub deadline_lateness: DurationHistogram,
    /// Time spent rendering a due payload immediately before its send attempt.
    pub render_duration: DurationHistogram,
    /// Time spent inside the interface's application-level `send` call.
    pub send_duration: DurationHistogram,
}

/// One counter-lane snapshot: exact run-to-date totals plus a bounded recent view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendTimingReport {
    pub cumulative: SendTimingTelemetry,
    pub recent: SendTimingTelemetry,
}

/// Runner-owned timing state. Only fixed-size recent segments are retained.
#[derive(Debug, Default)]
pub(crate) struct SendTimingRecorder {
    cumulative: SendTimingTelemetry,
    recent_deadline_lateness: RecentDurationHistogram,
    recent_render_duration: RecentDurationHistogram,
    recent_send_duration: RecentDurationHistogram,
}

impl SendTimingRecorder {
    pub(crate) fn record_deadline_lateness(&mut self, at: std::time::Instant, duration: Duration) {
        self.cumulative.deadline_lateness.record(duration);
        self.recent_deadline_lateness.record_at(at, duration);
    }

    pub(crate) fn record_render_duration(&mut self, at: std::time::Instant, duration: Duration) {
        self.cumulative.render_duration.record(duration);
        self.recent_render_duration.record_at(at, duration);
    }

    pub(crate) fn record_send_duration(&mut self, at: std::time::Instant, duration: Duration) {
        self.cumulative.send_duration.record(duration);
        self.recent_send_duration.record_at(at, duration);
    }

    pub(crate) fn snapshot_at(&self, now: std::time::Instant) -> SendTimingReport {
        SendTimingReport {
            cumulative: self.cumulative,
            recent: SendTimingTelemetry {
                deadline_lateness: self.recent_deadline_lateness.snapshot_at(now),
                render_duration: self.recent_render_duration.snapshot_at(now),
                send_duration: self.recent_send_duration.snapshot_at(now),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_snapshot_state_has_an_exact_window_boundary() {
        let captured_at = std::time::Instant::now();

        assert_eq!(
            recent_snapshot_state(None, captured_at, false),
            RecentSnapshotState::Pending
        );
        assert_eq!(
            recent_snapshot_state(
                Some(captured_at),
                captured_at + RECENT_WINDOW - Duration::from_nanos(1),
                false,
            ),
            RecentSnapshotState::Current(RECENT_WINDOW - Duration::from_nanos(1))
        );
        assert_eq!(
            recent_snapshot_state(Some(captured_at), captured_at + RECENT_WINDOW, false,),
            RecentSnapshotState::Expired(RECENT_WINDOW)
        );
    }

    #[test]
    fn final_snapshot_does_not_expire_and_future_capture_saturates() {
        let captured_at = std::time::Instant::now();

        assert_eq!(
            recent_snapshot_state(
                Some(captured_at),
                captured_at + RECENT_WINDOW + Duration::from_secs(1),
                true,
            ),
            RecentSnapshotState::Final
        );
        assert_eq!(
            recent_snapshot_state(
                Some(captured_at + Duration::from_secs(1)),
                captured_at,
                false,
            ),
            RecentSnapshotState::Current(Duration::ZERO)
        );
    }

    #[test]
    fn recorder_keeps_cumulative_truth_after_recent_samples_age_out() {
        let t0 = std::time::Instant::now();
        let mut recorder = SendTimingRecorder::default();
        recorder.record_deadline_lateness(t0, Duration::from_micros(100));
        recorder.record_render_duration(t0, Duration::from_micros(200));
        recorder.record_send_duration(t0, Duration::from_micros(300));

        let live = recorder.snapshot_at(t0 + Duration::from_secs(9));
        assert_eq!(live.cumulative.deadline_lateness.sample_count(), 1);
        assert_eq!(live.cumulative.render_duration.sample_count(), 1);
        assert_eq!(live.cumulative.send_duration.sample_count(), 1);
        assert_eq!(live.recent.deadline_lateness.sample_count(), 1);
        assert_eq!(live.recent.render_duration.sample_count(), 1);
        assert_eq!(live.recent.send_duration.sample_count(), 1);

        let aged = recorder.snapshot_at(t0 + RECENT_WINDOW);
        assert_eq!(aged.cumulative.deadline_lateness.sample_count(), 1);
        assert_eq!(aged.cumulative.render_duration.sample_count(), 1);
        assert_eq!(aged.cumulative.send_duration.sample_count(), 1);
        assert_eq!(aged.recent, SendTimingTelemetry::default());
    }
}
