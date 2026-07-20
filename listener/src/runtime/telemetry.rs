//! Fixed-size cumulative timing telemetry for Listener runtime boundaries.

use std::time::Duration;

use crate::core::{ArrivalTimestampSource, ArrivalTimestampStatus};

const BUCKET_UPPER_US: [u64; 15] = [
    50, 100, 250, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 256_000,
    512_000, 1_000_000,
];
const BUCKET_COUNT: usize = BUCKET_UPPER_US.len() + 1;
const BYTE_BUCKET_UPPER: [u64; 16] = [
    1, 8, 16, 32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768, 65_536, 131_072,
];
const BYTE_BUCKET_COUNT: usize = BYTE_BUCKET_UPPER.len() + 1;
const RECENT_SEGMENTS: usize = 10;
const RECENT_SEGMENT: Duration = Duration::from_secs(1);
pub const RECENT_WINDOW: Duration = Duration::from_secs(RECENT_SEGMENTS as u64);

/// A cumulative duration histogram with fixed memory and allocation-free recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DurationHistogram {
    samples: u64,
    buckets: [u64; BUCKET_COUNT],
    total_nanos: u128,
    max_nanos: u64,
}

impl DurationHistogram {
    pub fn record(&mut self, duration: Duration) {
        let full_nanos = duration.as_nanos();
        let nanos = full_nanos.min(u64::MAX as u128) as u64;
        let bucket =
            BUCKET_UPPER_US.partition_point(|upper| full_nanos > u128::from(*upper) * 1_000);

        self.samples = self.samples.saturating_add(1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.total_nanos = self.total_nanos.saturating_add(full_nanos);
        self.max_nanos = self.max_nanos.max(nanos);
    }

    pub fn sample_count(&self) -> u64 {
        self.samples
    }

    pub fn mean(&self) -> Option<Duration> {
        (self.samples > 0).then(|| {
            Duration::from_nanos(
                (self.total_nanos / self.samples as u128).min(u64::MAX as u128) as u64,
            )
        })
    }

    pub fn max(&self) -> Option<Duration> {
        (self.samples > 0).then(|| Duration::from_nanos(self.max_nanos))
    }

    /// Upper bound of the bucket containing `percentile`; the overflow bucket
    /// returns the exact observed maximum.
    pub fn percentile_upper_bound(&self, percentile: u8) -> Option<Duration> {
        if self.samples == 0 {
            return None;
        }

        let percentile = u128::from(percentile.clamp(1, 100));
        let rank = (u128::from(self.samples) * percentile).div_ceil(100);
        let mut cumulative = 0u128;
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative += u128::from(*count);
            if cumulative >= rank {
                return Some(if index < BUCKET_UPPER_US.len() {
                    Duration::from_micros(BUCKET_UPPER_US[index])
                } else {
                    Duration::from_nanos(self.max_nanos)
                });
            }
        }
        Some(Duration::from_nanos(self.max_nanos))
    }

    fn merge(&mut self, other: &Self) {
        self.samples = self.samples.saturating_add(other.samples);
        for (count, other_count) in self.buckets.iter_mut().zip(other.buckets) {
            *count = count.saturating_add(other_count);
        }
        self.total_nanos = self.total_nanos.saturating_add(other.total_nanos);
        self.max_nanos = self.max_nanos.max(other.max_nanos);
    }
}

/// A cumulative chunk-size histogram with fixed memory and allocation-free recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ByteHistogram {
    samples: u64,
    buckets: [u64; BYTE_BUCKET_COUNT],
    total_bytes: u128,
    max_bytes: u64,
}

impl ByteHistogram {
    pub fn record(&mut self, bytes: usize) {
        let bytes = bytes.min(u64::MAX as usize) as u64;
        let bucket = BYTE_BUCKET_UPPER.partition_point(|upper| bytes > *upper);

        self.samples = self.samples.saturating_add(1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(u128::from(bytes));
        self.max_bytes = self.max_bytes.max(bytes);
    }

    pub fn sample_count(&self) -> u64 {
        self.samples
    }

    pub fn mean(&self) -> Option<u64> {
        (self.samples > 0)
            .then(|| (self.total_bytes / u128::from(self.samples)).min(u128::from(u64::MAX)) as u64)
    }

    pub fn max(&self) -> Option<u64> {
        (self.samples > 0).then_some(self.max_bytes)
    }

    /// Upper bound of the bucket containing `percentile`; the overflow bucket
    /// returns the exact observed maximum.
    pub fn percentile_upper_bound(&self, percentile: u8) -> Option<u64> {
        if self.samples == 0 {
            return None;
        }

        let percentile = u128::from(percentile.clamp(1, 100));
        let rank = (u128::from(self.samples) * percentile).div_ceil(100);
        let mut cumulative = 0u128;
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative += u128::from(*count);
            if cumulative >= rank {
                return Some(if index < BYTE_BUCKET_UPPER.len() {
                    BYTE_BUCKET_UPPER[index]
                } else {
                    self.max_bytes
                });
            }
        }
        Some(self.max_bytes)
    }
}

/// Cumulative facts describing how a transport divided one run into read chunks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkShape {
    pub sizes: ByteHistogram,
    /// Gaps between consecutive transport post-read timestamps. The first chunk
    /// has no predecessor and therefore contributes no sample.
    pub inter_read_gaps: DurationHistogram,
}

/// Availability of an OS counter for this Channel, kept distinct from a real zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CounterAvailability {
    /// This counter does not apply to the Channel's transport.
    #[default]
    NotApplicable,
    /// The transport applies, but this OS offers no attributable per-socket counter.
    Unsupported,
    /// The counter is supported; the value may legitimately be zero.
    Available(u64),
}

/// Cumulative serial Transport-to-Pipeline backpressure for one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SerialStallSummary {
    pub episodes: u64,
    pub total: Duration,
    pub max: Duration,
    pub active: bool,
}

/// Cumulative arrival timestamp sources for one run. The configured status and
/// observed per-chunk counts are separate so a rare fallback cannot be hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArrivalTimestampSummary {
    pub status: ArrivalTimestampStatus,
    pub kernel_samples: u64,
    pub post_read_samples: u64,
}

/// Platform wait mechanism used for one completed Idle-rule deadline wake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleDeadlineTimerMode {
    NativeDeadlineWait,
    WindowsOneMillisecond,
    WindowsRequestFailed,
}

/// Cumulative completed Idle deadline waits by platform timer policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleDeadlineTimerSummary {
    pub native_waits: u64,
    pub windows_one_millisecond_waits: u64,
    pub windows_request_failures: u64,
}

impl IdleDeadlineTimerSummary {
    pub fn record(&mut self, mode: IdleDeadlineTimerMode) {
        let count = match mode {
            IdleDeadlineTimerMode::NativeDeadlineWait => &mut self.native_waits,
            IdleDeadlineTimerMode::WindowsOneMillisecond => &mut self.windows_one_millisecond_waits,
            IdleDeadlineTimerMode::WindowsRequestFailed => &mut self.windows_request_failures,
        };
        *count = count.saturating_add(1);
    }

    pub fn total_waits(&self) -> u64 {
        self.native_waits
            .saturating_add(self.windows_one_millisecond_waits)
            .saturating_add(self.windows_request_failures)
    }
}

impl ArrivalTimestampSummary {
    pub fn record(&mut self, source: ArrivalTimestampSource) {
        match source {
            ArrivalTimestampSource::PostRead => {
                self.post_read_samples = self.post_read_samples.saturating_add(1);
            }
            ArrivalTimestampSource::KernelSoftware => {
                self.kernel_samples = self.kernel_samples.saturating_add(1);
            }
        }
    }

    pub fn sample_count(&self) -> u64 {
        self.kernel_samples.saturating_add(self.post_read_samples)
    }
}

/// Transport-specific health facts. Fields remain explicit about applicability
/// and platform support rather than turning missing measurements into zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportHealth {
    pub serial_stalls: Option<SerialStallSummary>,
    pub udp_kernel_drops: CounterAvailability,
    pub arrival_timestamps: ArrivalTimestampSummary,
}

impl ChunkShape {
    pub fn chunk_count(&self) -> u64 {
        self.sizes.sample_count()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TimedHistogram {
    started_at: Option<std::time::Instant>,
    histogram: DurationHistogram,
}

/// A fixed-memory approximation of the last [`RECENT_WINDOW`] of samples.
#[derive(Debug, Default)]
pub(crate) struct RecentDurationHistogram {
    segments: [TimedHistogram; RECENT_SEGMENTS],
    current: usize,
    current_started: Option<std::time::Instant>,
}

impl RecentDurationHistogram {
    pub(crate) fn record_at(&mut self, now: std::time::Instant, duration: Duration) {
        self.advance(now);
        self.segments[self.current].histogram.record(duration);
    }

    pub(crate) fn snapshot_at(&self, now: std::time::Instant) -> DurationHistogram {
        let mut snapshot = DurationHistogram::default();
        for segment in &self.segments {
            if segment
                .started_at
                .is_some_and(|start| now.saturating_duration_since(start) < RECENT_WINDOW)
            {
                snapshot.merge(&segment.histogram);
            }
        }
        snapshot
    }

    fn advance(&mut self, now: std::time::Instant) {
        let Some(started) = self.current_started else {
            self.current_started = Some(now);
            self.segments[self.current].started_at = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(started);
        let steps = elapsed.as_nanos() / RECENT_SEGMENT.as_nanos();
        if steps == 0 {
            return;
        }

        let remainder = elapsed.as_nanos() % RECENT_SEGMENT.as_nanos();
        let aligned_start = now
            .checked_sub(Duration::from_nanos(remainder as u64))
            .unwrap_or(now);
        if steps >= RECENT_SEGMENTS as u128 {
            self.segments.fill(TimedHistogram::default());
            self.current = 0;
        } else {
            for _ in 0..steps as usize {
                self.current = (self.current + 1) % RECENT_SEGMENTS;
                self.segments[self.current] = TimedHistogram::default();
            }
        }
        self.current_started = Some(aligned_start);
        self.segments[self.current].started_at = Some(aligned_start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_summary() {
        let histogram = DurationHistogram::default();
        assert_eq!(histogram.sample_count(), 0);
        assert_eq!(histogram.mean(), None);
        assert_eq!(histogram.max(), None);
        assert_eq!(histogram.percentile_upper_bound(99), None);
    }

    #[test]
    fn records_cumulative_bounded_summaries() {
        let mut histogram = DurationHistogram::default();
        histogram.record(Duration::from_micros(1));
        histogram.record(Duration::from_micros(100));
        histogram.record(Duration::from_micros(900));
        histogram.record(Duration::from_secs(2));

        assert_eq!(histogram.sample_count(), 4);
        assert_eq!(histogram.mean(), Some(Duration::from_nanos(500_250_250)));
        assert_eq!(histogram.max(), Some(Duration::from_secs(2)));
        assert_eq!(
            histogram.percentile_upper_bound(50),
            Some(Duration::from_micros(100))
        );
        assert_eq!(
            histogram.percentile_upper_bound(75),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            histogram.percentile_upper_bound(99),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn byte_histogram_reports_chunk_count_and_bounded_percentiles() {
        let mut histogram = ByteHistogram::default();
        for bytes in [1, 9, 64, 1_500, 200_000] {
            histogram.record(bytes);
        }

        assert_eq!(histogram.sample_count(), 5);
        assert_eq!(histogram.mean(), Some(40_314));
        assert_eq!(histogram.max(), Some(200_000));
        assert_eq!(histogram.percentile_upper_bound(50), Some(64));
        assert_eq!(histogram.percentile_upper_bound(80), Some(2_048));
        assert_eq!(histogram.percentile_upper_bound(99), Some(200_000));
    }

    #[test]
    fn empty_chunk_shape_has_no_synthetic_gap() {
        let shape = ChunkShape::default();
        assert_eq!(shape.chunk_count(), 0);
        assert_eq!(shape.sizes.max(), None);
        assert_eq!(shape.inter_read_gaps.sample_count(), 0);
    }

    #[test]
    fn transport_counter_availability_distinguishes_zero_from_unsupported() {
        assert_ne!(
            CounterAvailability::Available(0),
            CounterAvailability::Unsupported
        );
        assert_eq!(
            TransportHealth::default().udp_kernel_drops,
            CounterAvailability::NotApplicable
        );
    }

    #[test]
    fn arrival_timestamp_summary_keeps_policy_and_observed_fallbacks_separate() {
        let mut summary = ArrivalTimestampSummary {
            status: ArrivalTimestampStatus::KernelSoftware,
            ..ArrivalTimestampSummary::default()
        };
        summary.record(ArrivalTimestampSource::KernelSoftware);
        summary.record(ArrivalTimestampSource::PostRead);

        assert_eq!(summary.sample_count(), 2);
        assert_eq!(summary.kernel_samples, 1);
        assert_eq!(summary.post_read_samples, 1);
        assert_eq!(summary.status, ArrivalTimestampStatus::KernelSoftware);
    }

    #[test]
    fn idle_timer_summary_distinguishes_native_effective_and_failed_waits() {
        let mut summary = IdleDeadlineTimerSummary::default();
        summary.record(IdleDeadlineTimerMode::NativeDeadlineWait);
        summary.record(IdleDeadlineTimerMode::WindowsOneMillisecond);
        summary.record(IdleDeadlineTimerMode::WindowsRequestFailed);

        assert_eq!(summary.total_waits(), 3);
        assert_eq!(summary.windows_request_failures, 1);
    }

    #[test]
    fn recent_window_ages_out_whole_fixed_segments() {
        let t0 = std::time::Instant::now();
        let mut recent = RecentDurationHistogram::default();
        recent.record_at(t0, Duration::from_micros(100));
        recent.record_at(t0 + Duration::from_secs(9), Duration::from_micros(900));

        assert_eq!(
            recent
                .snapshot_at(t0 + Duration::from_secs(9))
                .sample_count(),
            2
        );
        let later = recent.snapshot_at(t0 + Duration::from_secs(10));
        assert_eq!(later.sample_count(), 1, "the oldest segment aged out");
        assert_eq!(later.max(), Some(Duration::from_micros(900)));
        assert_eq!(
            recent
                .snapshot_at(t0 + Duration::from_secs(20))
                .sample_count(),
            0,
            "an idle window contains no stale samples"
        );
    }

    #[test]
    fn a_gap_larger_than_the_window_reuses_bounded_storage() {
        let t0 = std::time::Instant::now();
        let mut recent = RecentDurationHistogram::default();
        recent.record_at(t0, Duration::from_millis(1));
        recent.record_at(t0 + Duration::from_secs(30), Duration::from_millis(2));

        let snapshot = recent.snapshot_at(t0 + Duration::from_secs(30));
        assert_eq!(snapshot.sample_count(), 1);
        assert_eq!(snapshot.max(), Some(Duration::from_millis(2)));
    }
}
