//! File-backed implementations of the two recorders (spec §53, §54, §55, §57).
//!
//! Both use async file I/O (`tokio::fs` + buffered `tokio::io`) so the recorder
//! task never blocks a runtime worker (§142). [`open_recording_file`] enforces
//! the [`OverwritePolicy`](super::OverwritePolicy) atomically at enable time
//! (§55, §121): `Refuse` uses `create_new`, so an existing file is never
//! clobbered.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::core::RecordError;
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

use super::{DisplayRecorder, OverwritePolicy, RawRecorder, RecordingStopReason};

/// Open a recording destination, enforcing the overwrite policy (§55, §121).
///
/// `Refuse` opens with `create_new`, which fails atomically with
/// `AlreadyExists` if the file is present — so enabling fails and no existing
/// file is touched. `Overwrite` truncates; `AppendIfExists` appends.
pub async fn open_recording_file(
    path: &Path,
    policy: OverwritePolicy,
) -> Result<File, RecordError> {
    let mut opts = OpenOptions::new();
    opts.write(true);
    match policy {
        OverwritePolicy::Refuse => {
            opts.create_new(true);
        }
        OverwritePolicy::Overwrite => {
            opts.create(true).truncate(true);
        }
        OverwritePolicy::AppendIfExists => {
            opts.create(true).append(true);
        }
    }
    Ok(opts.open(path).await?)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Take a cross-platform advisory exclusive lock guarding a recording destination
/// (§121, ADR-014), so two recordings can never write the same file — whether two
/// channels here or a second `listener` process.
///
/// The lock is taken on a companion `<path>.lock` file, **not** the data file itself:
/// that keeps locking independent of the destination's overwrite policy (Refuse must
/// still fail atomically via the data open; Overwrite/Append must not be clobbered by
/// the lock handle), and it is taken *before* the data file is opened, so a lock
/// conflict never touches the destination. The returned **synchronous**
/// [`std::fs::File`] is the lock holder: a `std::fs::File` closes *deterministically*
/// on drop, releasing the lock the instant a recorder is dropped (so a Stop→Start can
/// immediately re-lock). A `tokio::fs::File` is unsuitable — it closes the OS handle
/// asynchronously, so its lock would linger past drop. Returns
/// [`RecordError::DestinationInUse`] if the destination is already locked.
fn lock_recording_destination(path: &Path) -> Result<std::fs::File, RecordError> {
    // `std::fs::File::try_lock` (stable since Rust 1.89; MSRV is 1.95) — no fs4 needed
    // for the lock; fs4 stays for the disk-space free functions (§168).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(path))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(RecordError::DestinationInUse),
        Err(std::fs::TryLockError::Error(e)) => Err(RecordError::Io(e)),
    }
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".idx");
    PathBuf::from(name)
}

fn wall_clock_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Raw Recording to a file: byte-exact, contiguous, with an optional timestamp
/// sidecar keyed by byte offset (§53, §57). The byte stream contains received
/// payload bytes only — timestamps never interleave it (§53).
pub struct RawFileRecorder {
    file: BufWriter<File>,
    /// Timestamp index: one `offset,wall_clock_nanos` line per chunk (§57).
    sidecar: Option<BufWriter<File>>,
    bytes_written: u64,
    /// Advisory lock on the destination (§121, ADR-014); held for the recorder's
    /// lifetime, released deterministically when this std handle drops.
    _lock: std::fs::File,
}

impl RawFileRecorder {
    /// Create the recording (and, if `timestamps`, its sidecar), failing per the
    /// overwrite policy if the destination exists (§55).
    pub async fn create(
        path: &Path,
        policy: OverwritePolicy,
        timestamps: bool,
    ) -> Result<Self, RecordError> {
        // Lock the main destination first (§121, ADR-014): if it is already in use, fail
        // before touching it. Only the main destination is locked — the `.idx` sidecar
        // is derived from it (`<path>.idx`) and shares the recording's lifetime, so two
        // recordings collide on the main path (caught here) before their sidecars could.
        // The one uncovered edge — a user pointing one channel's *main* destination at
        // another's sidecar path — is left unguarded as vanishingly unlikely.
        let lock = lock_recording_destination(path)?;
        let file = BufWriter::new(open_recording_file(path, policy).await?);
        let sidecar = if timestamps {
            Some(BufWriter::new(
                open_recording_file(&sidecar_path(path), policy).await?,
            ))
        } else {
            None
        };
        Ok(Self {
            file,
            sidecar,
            bytes_written: 0,
            _lock: lock,
        })
    }

    /// Total bytes durably offered to the byte stream — the truncation point on
    /// fault (§56.1).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

#[async_trait::async_trait]
impl RawRecorder for RawFileRecorder {
    async fn write_chunk(&mut self, chunk: &ReceivedData) -> Result<(), RecordError> {
        let bytes = chunk.payload.bytes();
        if let Some(sidecar) = &mut self.sidecar {
            // Key the timestamp by the offset of this chunk's first byte (§57).
            let line = format!(
                "{},{}\n",
                self.bytes_written,
                wall_clock_nanos(chunk.received_at.wall_clock)
            );
            sidecar.write_all(line.as_bytes()).await?;
        }
        self.file.write_all(bytes).await?;
        self.bytes_written += bytes.len() as u64;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.file.flush().await?;
        if let Some(sidecar) = &mut self.sidecar {
            sidecar.flush().await?;
        }
        Ok(())
    }

    async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
        // Flush buffered data to disk before close (truncation point is recorded
        // by the runtime via RecordingFaulted, §56.1). The OS closes the file
        // when the recorder is dropped after the task ends.
        self.flush().await
    }
}

/// Display Recording to a file: writes a view's rendered text, one rendered chunk per
/// write, optionally prefixed with an inline timestamp (§54, §57). Not byte-exact.
pub struct DisplayFileRecorder {
    file: BufWriter<File>,
    timestamps: bool,
    /// Advisory lock on the destination (§121, ADR-014); see `RawFileRecorder._lock`.
    _lock: std::fs::File,
}

impl DisplayFileRecorder {
    pub async fn create(
        path: &Path,
        policy: OverwritePolicy,
        timestamps: bool,
    ) -> Result<Self, RecordError> {
        // Lock the destination first (§121, ADR-014) — same as Raw, so a `.disp` cannot
        // be shared by two recordings either.
        let lock = lock_recording_destination(path)?;
        let file = BufWriter::new(open_recording_file(path, policy).await?);
        Ok(Self {
            file,
            timestamps,
            _lock: lock,
        })
    }
}

#[async_trait::async_trait]
impl DisplayRecorder for DisplayFileRecorder {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError> {
        // Display Recording is not byte-exact, so inline timestamps are allowed
        // (§57): they go in the formatted artifact, unlike Raw Recording.
        if self.timestamps {
            if let Some(ts) = &output.timestamp {
                let line = format!("[{}] ", wall_clock_nanos(ts.wall_clock));
                self.file.write_all(line.as_bytes()).await?;
            }
        }
        self.file.write_all(output.text.as_bytes()).await?;
        self.file.write_all(b"\n").await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), RecordError> {
        self.file.flush().await?;
        Ok(())
    }

    async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
        self.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime};
    use crate::record::{start_display_recording, start_raw_recording};
    use crate::transport::ReceivedPayload;
    use std::sync::Arc;

    fn temp_path(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("listener-rec-{tag}-{}.bin", uuid::Uuid::new_v4()));
        path
    }

    fn chunk(bytes: &[u8]) -> Arc<ReceivedData> {
        Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime::now(),
        })
    }

    #[tokio::test]
    async fn refuse_policy_does_not_clobber_an_existing_file() {
        let path = temp_path("refuse");
        tokio::fs::write(&path, b"original").await.unwrap();

        let result = RawFileRecorder::create(&path, OverwritePolicy::Refuse, false).await;
        assert!(result.is_err());
        // The existing file is untouched.
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"original");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn a_second_recorder_on_the_same_file_is_refused_while_the_first_is_open() {
        // §121 / ADR-014: an advisory lock stops two live recordings sharing one file.
        let path = temp_path("locked");
        let _first = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .expect("first recorder takes the lock");

        // While the first holds the file, a second open is refused as in-use.
        let second = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, false).await;
        assert!(
            matches!(second, Err(RecordError::DestinationInUse)),
            "a second recorder must be refused while the first is open"
        );

        // After the first is dropped (lock released), the destination is free again.
        drop(_first);
        let third = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, false).await;
        assert!(third.is_ok(), "the lock releases on drop");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn overwrite_policy_truncates_then_appends_policy_keeps() {
        // Overwrite replaces existing content.
        let path = temp_path("overwrite");
        tokio::fs::write(&path, b"stale-and-longer").await.unwrap();
        {
            let mut rec = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
                .await
                .unwrap();
            rec.write_chunk(&chunk(b"new")).await.unwrap();
            rec.finalize(RecordingStopReason::Disabled).await.unwrap();
        }
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"new");

        // AppendIfExists keeps existing content and adds to it.
        {
            let mut rec = RawFileRecorder::create(&path, OverwritePolicy::AppendIfExists, false)
                .await
                .unwrap();
            rec.write_chunk(&chunk(b"-more")).await.unwrap();
            rec.finalize(RecordingStopReason::Disabled).await.unwrap();
        }
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"new-more");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn raw_recording_is_byte_exact_end_to_end() {
        let path = temp_path("byte-exact");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let mut recording = start_raw_recording(recorder, 16);

        // Chunk boundaries must not appear in the byte stream (§53).
        recording.try_record(chunk(b"$GPGGA,"));
        recording.try_record(chunk(b"123.4*7F\r\n"));
        recording.try_record(chunk(b"\x00\x01\x02"));
        recording.finalize(RecordingStopReason::Disabled).await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"$GPGGA,123.4*7F\r\n\x00\x01\x02");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn raw_timestamp_sidecar_keeps_the_byte_stream_pure() {
        let path = temp_path("sidecar");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, true)
            .await
            .unwrap();
        let mut recording = start_raw_recording(recorder, 16);
        recording.try_record(chunk(b"AB"));
        recording.try_record(chunk(b"CDE"));
        recording.finalize(RecordingStopReason::Disabled).await;

        // Byte stream holds only payload bytes — no timestamps interleaved (§53).
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"ABCDE");
        // Sidecar has one offset-keyed line per chunk (offsets 0 and 2).
        let sidecar = tokio::fs::read_to_string(&sidecar_path(&path))
            .await
            .unwrap();
        let offsets: Vec<&str> = sidecar
            .lines()
            .map(|l| l.split(',').next().unwrap())
            .collect();
        assert_eq!(offsets, vec!["0", "2"]);

        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_file(&sidecar_path(&path)).await;
    }

    #[tokio::test]
    async fn display_recording_writes_rendered_lines() {
        let path = temp_path("display");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let mut recording = start_display_recording(recorder, 16);
        let cid = ChannelId::new();
        recording.try_record(RenderedOutput {
            channel_id: cid,
            text: "first".to_string(),
            timestamp: None,
        });
        recording.try_record(RenderedOutput {
            channel_id: cid,
            text: "second".to_string(),
            timestamp: None,
        });
        recording.finalize(RecordingStopReason::Disabled).await;

        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "first\nsecond\n"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }
}
