//! Chunk-arrival timing types (spec §138).
//!
//! In the stream-only design (ADR-010) there is no Message model; the only
//! timing the runtime needs is the per-chunk arrival time. Per-byte arrival time
//! is not available from the OS, so all timing is chunk-granular (`ChunkTime`,
//! §138): a chunk's [`ChunkTime`] is captured when the transport reads it, and a
//! recording timestamp ([`ChunkTimestamp`]) is derived from it.

use std::time::{Instant, SystemTime};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// A single timing capture taken when a transport chunk is read (§138).
///
/// `monotonic` orders chunks, measures durations, and breaks timestamp ties
/// (§125); `wall_clock` renders Local/UTC display times (§26). Precision is not
/// accuracy — userland arrival times carry OS scheduling jitter (§26, §133).
#[derive(Clone, Copy, Debug)]
pub struct ChunkTime {
    pub monotonic: Instant,
    pub wall_clock: SystemTime,
}

impl ChunkTime {
    /// Capture the current monotonic and wall-clock time together.
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall_clock: SystemTime::now(),
        }
    }
}

/// A timestamp derived from the `ChunkTime` of a received chunk (§133), carried
/// alongside rendered/recorded output. Formatting to a display string — source
/// (Local / UTC / Relative) and resolution (s / ms / µs) — happens in the display
/// layer, never in stored state.
#[derive(Clone, Copy, Debug)]
pub struct ChunkTimestamp {
    pub monotonic: Instant,
    pub wall_clock: SystemTime,
}

impl From<ChunkTime> for ChunkTimestamp {
    fn from(chunk: ChunkTime) -> Self {
        Self {
            monotonic: chunk.monotonic,
            wall_clock: chunk.wall_clock,
        }
    }
}

/// Configuration for an inline timestamp rendered next to a matched byte pattern
/// (§50.2 Mark). Mirrors talker's `TimestampConfig` field-for-field, but formats in
/// **Local** time (talker formats UTC) — the listener shows a local wall-clock time
/// because it is a field troubleshooting/logging tool (§26).
///
/// Time-of-day (`HH:MM:SS`) is always present; the date, milliseconds, and timezone
/// offset are independently toggleable. Default = `HH:MM:SS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TimestampConfig {
    #[serde(default)]
    pub include_date: bool,
    #[serde(default)]
    pub include_millis: bool,
    #[serde(default)]
    pub include_timezone: bool,
}

impl TimestampConfig {
    /// Format a wall-clock instant as a **local** timestamp per this configuration.
    /// The timezone toggle emits the local UTC offset (e.g. `-07:00`), not `Z`.
    pub fn format(&self, at: SystemTime) -> String {
        let local: DateTime<Local> = at.into();
        let mut s = String::new();
        if self.include_date {
            s.push_str(&local.format("%Y-%m-%dT").to_string());
        }
        s.push_str(&local.format("%H:%M:%S").to_string());
        if self.include_millis {
            s.push_str(&local.format("%.3f").to_string());
        }
        if self.include_timezone {
            s.push_str(&local.format("%:z").to_string());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_taken_from_the_chunk_time() {
        let chunk = ChunkTime::now();
        let ts = ChunkTimestamp::from(chunk);
        assert_eq!(ts.monotonic, chunk.monotonic);
        assert_eq!(ts.wall_clock, chunk.wall_clock);
    }

    /// A fixed *local* instant (2026-05-22 14:30:45.123 local) round-tripped through
    /// `SystemTime`, so `format` reproduces the same local wall-clock regardless of
    /// the host timezone.
    fn sample() -> SystemTime {
        use chrono::{TimeZone, Timelike};
        let local = Local
            .with_ymd_and_hms(2026, 5, 22, 14, 30, 45)
            .unwrap()
            .with_nanosecond(123_000_000)
            .unwrap();
        SystemTime::from(local)
    }

    #[test]
    fn time_only_is_the_default() {
        assert_eq!(TimestampConfig::default().format(sample()), "14:30:45");
    }

    #[test]
    fn time_and_millis_only() {
        let cfg = TimestampConfig {
            include_date: false,
            include_millis: true,
            include_timezone: false,
        };
        assert_eq!(cfg.format(sample()), "14:30:45.123");
    }

    #[test]
    fn date_and_time_no_millis() {
        let cfg = TimestampConfig {
            include_date: true,
            include_millis: false,
            include_timezone: false,
        };
        assert_eq!(cfg.format(sample()), "2026-05-22T14:30:45");
    }

    #[test]
    fn timezone_offset_matches_the_local_offset() {
        use chrono::{DateTime, Local};
        let cfg = TimestampConfig {
            include_date: false,
            include_millis: false,
            include_timezone: true,
        };
        // The emitted suffix is the local UTC offset for that instant (e.g. -07:00),
        // never a fixed `Z`. Compare against chrono's own offset for the same instant.
        let local: DateTime<Local> = sample().into();
        let expected = format!("14:30:45{}", local.format("%:z"));
        assert_eq!(cfg.format(sample()), expected);
    }
}
