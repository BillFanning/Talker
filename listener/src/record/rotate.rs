//! Time-based recording file rotation (spec §59).
//!
//! A [`RotatingRawRecorder`] / [`RotatingDisplayRecorder`] wraps a per-period
//! file recorder ([`file`](super::file)). It writes to a **directory** and opens
//! a new file at each calendar period (Hourly/Daily), named for the period start:
//! `<channel>_<start-time><ext>` (§59). Rotation is **data-driven** — the period
//! is taken from each item's wall-clock arrival time, so a quiet period produces
//! no file and a rotation happens when the first item of the next period arrives.
//! Period keys are **UTC**: calendar-aligned, unambiguous, and free of DST
//! collisions (two local "01:00" hours would otherwise collide on a fall-back).
//! Each file stays contiguous and byte-exact for the data it holds; a rotation is
//! a clean file boundary, never a gap (§56).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::core::RecordError;
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

use super::file::{DisplayFileRecorder, RawFileRecorder};
use super::{DisplayRecorder, OverwritePolicy, RawRecorder, RecordingStopReason, RotationPolicy};

/// The period key for `at` under `policy` — the rotation trigger *and* the
/// filename time component (§59), in **UTC**. `None` policy has no period.
pub(crate) fn period_key(policy: RotationPolicy, at: SystemTime) -> Option<String> {
    let dt: DateTime<Utc> = at.into();
    match policy {
        RotationPolicy::None => None,
        RotationPolicy::Hourly => Some(dt.format("%Y-%m-%d_%H").to_string()),
        RotationPolicy::Daily => Some(dt.format("%Y-%m-%d").to_string()),
    }
}

/// `<channel>_<key><ext>` (§59), e.g. `GPS_2026-06-03_08.dat`.
fn rotation_filename(channel: &str, key: &str, ext: &str) -> String {
    format!("{channel}_{key}{ext}")
}

/// Whether `name` is safe to embed in a recording filename (§59, §71): non-empty,
/// bounded length, no path separators or reserved characters, no control bytes,
/// no trailing dot/space, and not a Windows reserved device name. Validated at
/// config time when rotation is enabled; rejected, never silently sanitized.
pub fn is_filesystem_safe(name: &str) -> bool {
    const MAX_LEN: usize = 64;
    if name.is_empty() || name.len() > MAX_LEN {
        return false;
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return false;
    }
    // Windows rejects names with a trailing space or dot.
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    // Windows reserved device names (case-insensitive, ignoring any extension).
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    !RESERVED.contains(&stem.as_str())
}

/// Raw Recording with time-based rotation (§59). Writes `.dat` (or `.ssdat`) files
/// into `dir`, one per calendar period.
pub struct RotatingRawRecorder {
    dir: PathBuf,
    channel: String,
    ext: String,
    policy: OverwritePolicy,
    timestamps: bool,
    rotation: RotationPolicy,
    current_key: String,
    inner: RawFileRecorder,
}

impl RotatingRawRecorder {
    /// Create the recorder and eagerly open the current period's file, so an
    /// enable-time failure (missing directory, refused overwrite) surfaces now
    /// (§55) rather than on first write. `rotation` must not be `None`.
    pub async fn create(
        dir: &Path,
        channel: &str,
        ext: &str,
        policy: OverwritePolicy,
        timestamps: bool,
        rotation: RotationPolicy,
    ) -> Result<Self, RecordError> {
        tokio::fs::create_dir_all(dir).await?;
        let key = period_key(rotation, SystemTime::now())
            .expect("RotatingRawRecorder requires a rotation period");
        let path = dir.join(rotation_filename(channel, &key, ext));
        let inner = RawFileRecorder::create(&path, policy, timestamps).await?;
        Ok(Self {
            dir: dir.to_owned(),
            channel: channel.to_owned(),
            ext: ext.to_owned(),
            policy,
            timestamps,
            rotation,
            current_key: key,
            inner,
        })
    }

    async fn rotate_to(&mut self, key: String) -> Result<(), RecordError> {
        // Flush + close the current file (a clean boundary, §56), then open the
        // next. Replacing `inner` drops the old recorder, closing its file.
        self.inner
            .finalize(RecordingStopReason::ChannelStopped)
            .await?;
        let path = self
            .dir
            .join(rotation_filename(&self.channel, &key, &self.ext));
        self.inner = RawFileRecorder::create(&path, self.policy, self.timestamps).await?;
        self.current_key = key;
        Ok(())
    }
}

#[async_trait::async_trait]
impl RawRecorder for RotatingRawRecorder {
    async fn write_chunk(&mut self, chunk: &ReceivedData) -> Result<(), RecordError> {
        if let Some(key) = period_key(self.rotation, chunk.received_at.wall_clock) {
            if key != self.current_key {
                self.rotate_to(key).await?;
            }
        }
        self.inner.write_chunk(chunk).await
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.inner.flush().await
    }

    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        self.inner.finalize(reason).await
    }
}

/// Display Recording with time-based rotation (§59). Writes `.disp` files into
/// `dir`, one per calendar period.
pub struct RotatingDisplayRecorder {
    dir: PathBuf,
    channel: String,
    ext: String,
    policy: OverwritePolicy,
    timestamps: bool,
    rotation: RotationPolicy,
    current_key: String,
    inner: DisplayFileRecorder,
}

impl RotatingDisplayRecorder {
    pub async fn create(
        dir: &Path,
        channel: &str,
        ext: &str,
        policy: OverwritePolicy,
        timestamps: bool,
        rotation: RotationPolicy,
    ) -> Result<Self, RecordError> {
        tokio::fs::create_dir_all(dir).await?;
        let key = period_key(rotation, SystemTime::now())
            .expect("RotatingDisplayRecorder requires a rotation period");
        let path = dir.join(rotation_filename(channel, &key, ext));
        let inner = DisplayFileRecorder::create(&path, policy, timestamps).await?;
        Ok(Self {
            dir: dir.to_owned(),
            channel: channel.to_owned(),
            ext: ext.to_owned(),
            policy,
            timestamps,
            rotation,
            current_key: key,
            inner,
        })
    }

    async fn rotate_to(&mut self, key: String) -> Result<(), RecordError> {
        self.inner
            .finalize(RecordingStopReason::ChannelStopped)
            .await?;
        let path = self
            .dir
            .join(rotation_filename(&self.channel, &key, &self.ext));
        self.inner = DisplayFileRecorder::create(&path, self.policy, self.timestamps).await?;
        self.current_key = key;
        Ok(())
    }
}

#[async_trait::async_trait]
impl DisplayRecorder for RotatingDisplayRecorder {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError> {
        // A rendered Message carries its own timestamp; fall back to now() if the
        // view emits none, so rotation still advances.
        let at = output
            .timestamp
            .map(|t| t.wall_clock)
            .unwrap_or_else(SystemTime::now);
        if let Some(key) = period_key(self.rotation, at) {
            if key != self.current_key {
                self.rotate_to(key).await?;
            }
        }
        self.inner.write_rendered(output).await
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.inner.flush().await
    }

    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        self.inner.finalize(reason).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime, MessageTimestamp};
    use crate::transport::ReceivedPayload;
    use chrono::TimeZone;
    use std::time::Instant;

    /// A `SystemTime` at a fixed UTC instant, for deterministic period keys.
    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> SystemTime {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().into()
    }

    fn chunk_at(bytes: &[u8], at: SystemTime) -> ReceivedData {
        ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime {
                monotonic: Instant::now(),
                wall_clock: at,
            },
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("listener-rot-{tag}-{}", uuid::Uuid::new_v4()));
        p
    }

    #[test]
    fn period_keys_use_utc_at_the_right_resolution() {
        let at = utc(2026, 6, 3, 8, 30);
        assert_eq!(
            period_key(RotationPolicy::Hourly, at).as_deref(),
            Some("2026-06-03_08")
        );
        assert_eq!(
            period_key(RotationPolicy::Daily, at).as_deref(),
            Some("2026-06-03")
        );
        assert_eq!(period_key(RotationPolicy::None, at), None);
    }

    #[test]
    fn filesystem_safety_accepts_plain_names_and_rejects_unsafe_ones() {
        for ok in ["GPS", "GPS-1", "ais_receiver", "Bow Antenna"] {
            assert!(is_filesystem_safe(ok), "{ok} should be safe");
        }
        for bad in [
            "",
            "a/b",
            "a\\b",
            "c:name",
            "star*",
            "q?",
            "CON",
            "com1",
            "nul.dat",
            "trailing.",
            "trailing ",
            "ctrl\u{0007}",
        ] {
            assert!(!is_filesystem_safe(bad), "{bad:?} should be rejected");
        }
        assert!(!is_filesystem_safe(&"x".repeat(65)));
    }

    #[tokio::test]
    async fn raw_rotates_on_the_hour_with_correct_names_and_contiguous_files() {
        let dir = temp_dir("raw");
        let mut rec = RotatingRawRecorder::create(
            &dir,
            "GPS",
            ".dat",
            OverwritePolicy::Overwrite,
            false,
            RotationPolicy::Hourly,
        )
        .await
        .unwrap();

        // Two chunks in the 08:00 hour, one in 09:00 → two files, no gap, no backfill.
        rec.write_chunk(&chunk_at(b"A", utc(2026, 6, 3, 8, 30)))
            .await
            .unwrap();
        rec.write_chunk(&chunk_at(b"B", utc(2026, 6, 3, 8, 45)))
            .await
            .unwrap();
        rec.write_chunk(&chunk_at(b"C", utc(2026, 6, 3, 9, 5)))
            .await
            .unwrap();
        rec.finalize(RecordingStopReason::ChannelStopped)
            .await
            .unwrap();
        drop(rec); // close the final file

        let f08 = dir.join("GPS_2026-06-03_08.dat");
        let f09 = dir.join("GPS_2026-06-03_09.dat");
        assert_eq!(tokio::fs::read(&f08).await.unwrap(), b"AB");
        assert_eq!(tokio::fs::read(&f09).await.unwrap(), b"C");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn display_rotates_on_the_day_with_correct_names() {
        let dir = temp_dir("disp");
        let mut rec = RotatingDisplayRecorder::create(
            &dir,
            "AIS",
            ".disp",
            OverwritePolicy::Overwrite,
            false,
            RotationPolicy::Daily,
        )
        .await
        .unwrap();

        let render = |text: &str, at: SystemTime| RenderedOutput {
            channel_id: ChannelId::new(),
            message_number: Some(1),
            text: text.to_string(),
            timestamp: Some(MessageTimestamp {
                monotonic: Instant::now(),
                wall_clock: at,
            }),
        };
        rec.write_rendered(&render("day1", utc(2026, 6, 3, 23, 50)))
            .await
            .unwrap();
        rec.write_rendered(&render("day2", utc(2026, 6, 4, 0, 10)))
            .await
            .unwrap();
        rec.finalize(RecordingStopReason::ChannelStopped)
            .await
            .unwrap();
        drop(rec);

        assert_eq!(
            tokio::fs::read_to_string(dir.join("AIS_2026-06-03.disp"))
                .await
                .unwrap(),
            "day1\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(dir.join("AIS_2026-06-04.disp"))
                .await
                .unwrap(),
            "day2\n"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
