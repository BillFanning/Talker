//! Raw and display recording (spec §51–§59, §142).
//!
//! This is `listener-record` (§128). There are **two independent recording
//! systems** with different inputs, pipeline positions, and guarantees (§51):
//!
//! - **Raw Recording** ([`file::RawFileRecorder`]) — byte-oriented; taps the
//!   received-chunk stream verbatim, exactly as received (§53). Byte-exact and
//!   contiguous up to a known end (§5.6, §56.1).
//! - **Display Recording** ([`file::DisplayFileRecorder`]) — consumes a display
//!   view's rendered output *after* rendering (§54). Not byte-exact.
//!
//! Each runs as its own task draining a bounded queue (§142). The producer's
//! enqueue is non-blocking: on a full queue the recorder **faults** rather than
//! stalling reception (§56.1, §100). A faulted recording is contiguous from
//! start to a single truncation point, then ends — it never silently gaps and
//! resumes (§56.1).

pub mod file;
pub mod file_rotation;

use std::sync::{Arc, OnceLock};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::core::{RecordError, RecordingState};
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

pub use file::{DisplayFileRecorder, RawFileRecorder};
pub use file_rotation::{is_filesystem_safe, RotatingDisplayRecorder, RotatingRawRecorder};

/// Time-based recording file rotation (§59). `None` writes a single file; the
/// others write a new file per calendar period, named for the period start (§59).
/// Size-based rotation and pruning remain deferred (Appendix A).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum FileRotationPolicy {
    #[default]
    None,
    Hourly,
    Daily,
}

/// What to do when the destination file already exists (§80.1). Enforced when
/// recording is enabled (§55, §121); `Refuse` is the default and never clobbers.
///
/// Defined here (record owns file lifecycle, §128); the profile schema will
/// reference this type when the config module lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OverwritePolicy {
    #[default]
    Refuse,
    Overwrite,
    AppendIfExists,
}

/// The §59 on-exists rule under rotation, in one place: `Refuse` is meaningless
/// when each period opens a fresh file — and re-opening the current period's
/// file (e.g. after a restart) must append, not fail — so it coerces to
/// `AppendIfExists`. Every other combination passes through. Shared by the
/// runtime settings builders and the GUI editor so the rule cannot drift.
pub fn effective_overwrite(
    policy: OverwritePolicy,
    rotation: FileRotationPolicy,
) -> OverwritePolicy {
    if rotation != FileRotationPolicy::None && policy == OverwritePolicy::Refuse {
        OverwritePolicy::AppendIfExists
    } else {
        policy
    }
}

/// Why a recording stopped (§56). In every case data received after the
/// stopping instant is not written and the file is finalized.
#[derive(Debug)]
pub enum RecordingStopReason {
    /// The user disabled recording.
    Disabled,
    /// The Channel left the Running state.
    ChannelStopped,
    /// A fault occurred; the artifact ends at its truncation point (§56.1).
    Faulted(RecordError),
}

impl RecordingStopReason {
    fn is_fault(&self) -> bool {
        matches!(self, RecordingStopReason::Faulted(_))
    }
}

/// Writes received chunks verbatim, exactly as received (§53, §142).
#[async_trait::async_trait]
pub trait RawRecorder: Send {
    /// Append one received chunk verbatim, exactly as received.
    async fn write_chunk(&mut self, chunk: &ReceivedData) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    /// Flush, close, and (for faults) note the truncation point (§56.1).
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
}

/// Writes a display view's rendered output, after rendering (§54, §142).
#[async_trait::async_trait]
pub trait DisplayRecorder: Send {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
}

/// Internal unification of the two §142 recorders so the queue-draining task and
/// the producer handle are written once. Each public recorder bridges to this.
#[async_trait::async_trait]
trait RecorderWriter<I: Send>: Send {
    async fn write(&mut self, item: &I) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
}

#[async_trait::async_trait]
impl<R: RawRecorder> RecorderWriter<Arc<ReceivedData>> for R {
    async fn write(&mut self, item: &Arc<ReceivedData>) -> Result<(), RecordError> {
        self.write_chunk(item).await
    }
    async fn flush(&mut self) -> Result<(), RecordError> {
        RawRecorder::flush(self).await
    }
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        RawRecorder::finalize(self, reason).await
    }
}

#[async_trait::async_trait]
impl<R: DisplayRecorder> RecorderWriter<RenderedOutput> for R {
    async fn write(&mut self, item: &RenderedOutput) -> Result<(), RecordError> {
        self.write_rendered(item).await
    }
    async fn flush(&mut self) -> Result<(), RecordError> {
        DisplayRecorder::flush(self).await
    }
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        DisplayRecorder::finalize(self, reason).await
    }
}

/// How often a running recorder flushes its buffered output to the OS (§56).
/// Without this, bytes sat in the writer's buffer until finalize: on a slow
/// stream a `.raw`/`.disp` lagged what another tool could read by minutes, and a
/// crash/power cut lost the whole buffered tail — the wrong trade for a
/// long-running capture tool. One flush per second is negligible I/O.
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// The recorder task (§142): drain the bounded queue and write each item until
/// terminated (fault or stop), flushing buffered output on a timer so the file
/// on disk never lags a slow stream by more than [`FLUSH_INTERVAL`]. On a
/// write/flush error it faults and finalizes at the truncation point; on
/// graceful stop it drains the accepted backlog first (§110); a fault never
/// drains (§56.1).
async fn run_recorder<I, W>(
    mut writer: W,
    mut items: mpsc::Receiver<I>,
    mut terminate: oneshot::Receiver<RecordingStopReason>,
    fault: Arc<OnceLock<String>>,
) where
    I: Send + 'static,
    W: RecorderWriter<I> + 'static,
{
    // First flush one interval from now (an immediate tick would flush an empty
    // file); skipped ticks (a long write) collapse into one.
    let mut flush_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + FLUSH_INTERVAL, FLUSH_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            reason = &mut terminate => {
                let reason = reason.unwrap_or(RecordingStopReason::ChannelStopped);
                if !reason.is_fault() {
                    // Graceful: drain the already-accepted backlog (§110). A
                    // write failure mid-drain truncates ACCEPTED data — that
                    // is a fault, not a clean stop: publish it and finalize
                    // at the truncation point (§56.1), never report clean.
                    while let Ok(item) = items.try_recv() {
                        if let Err(err) = writer.write(&item).await {
                            let _ = fault.set(err.to_string());
                            let _ = writer.finalize(RecordingStopReason::Faulted(err)).await;
                            return;
                        }
                    }
                }
                // A failed finalize (flush/close) is equally a dirty stop —
                // the buffered tail may be gone. Publish it.
                if let Err(err) = writer.finalize(reason).await {
                    let _ = fault.set(format!("finalize failed: {err}"));
                }
                return;
            }
            maybe = items.recv() => match maybe {
                Some(item) => {
                    if let Err(err) = writer.write(&item).await {
                        // Write fault: truncate here, do not drain (§56.1). Publish
                        // the fault before exiting so the producer handle reads
                        // Faulted even if the stream then goes quiet and no later
                        // enqueue trips over the closed queue.
                        let _ = fault.set(err.to_string());
                        let _ = writer.finalize(RecordingStopReason::Faulted(err)).await;
                        return;
                    }
                }
                None => {
                    if let Err(err) = writer.finalize(RecordingStopReason::ChannelStopped).await {
                        let _ = fault.set(format!("finalize failed: {err}"));
                    }
                    return;
                }
            },
            _ = flush_tick.tick() => {
                if let Err(err) = writer.flush().await {
                    // A failed flush is a write fault: truncate here (§56.1).
                    // Published like a write fault: a flush failure needs no
                    // traffic at all, so the eager fault cell is the only way a
                    // quiet stream's handle ever learns of it.
                    let _ = fault.set(err.to_string());
                    let _ = writer.finalize(RecordingStopReason::Faulted(err)).await;
                    return;
                }
            }
        }
    }
}

/// Producer handle for a running recorder. Enqueue is non-blocking; a full queue
/// faults the recording rather than stalling the producer (§56.1).
pub struct Recording<I> {
    items: mpsc::Sender<I>,
    terminate: Option<oneshot::Sender<RecordingStopReason>>,
    state: RecordingState,
    task: JoinHandle<()>,
    /// Terminal fault message, published by the recorder task (write/flush
    /// failure) or locally (queue overflow). Once set, [`state`](Self::state)
    /// reads `Faulted` immediately — without this, the handle only learned of a
    /// task-side fault when a *later* enqueue hit the closed queue, and a stream
    /// that goes quiet after a disk fault never enqueues again (§56.1).
    fault: Arc<OnceLock<String>>,
    /// High-water mark of the queue depth seen at enqueue time (§99) — for stress
    /// testing. A 5 Hz stats poll would miss a transient backlog; this peak does not.
    peak_depth: usize,
}

impl<I: Send + 'static> Recording<I> {
    pub fn state(&self) -> RecordingState {
        if self.fault.get().is_some() {
            return RecordingState::Faulted;
        }
        self.state
    }

    /// Why the recording faulted (the rendered terminal error), if it has.
    pub fn fault_error(&self) -> Option<&str> {
        self.fault.get().map(String::as_str)
    }

    /// Current/peak/capacity of the recorder queue as a `(current, peak, capacity)`
    /// triple — surfaced in `ChannelStats.raw_recording_queue` for backpressure
    /// diagnosis. `current` is `max_capacity - capacity` (queued = bound − free slots).
    pub fn queue_depth(&self) -> (usize, usize, usize) {
        let capacity = self.items.max_capacity();
        let current = capacity.saturating_sub(self.items.capacity());
        (current, self.peak_depth.max(current), capacity)
    }

    /// Offer one item to the recorder. Non-blocking (§56.1): a full queue faults
    /// the recording. (A closed queue means the task already faulted and
    /// published its error — [`state`](Self::state) reads it eagerly, so the
    /// local transition here is just belt-and-braces.)
    pub fn try_record(&mut self, item: I) {
        if self.state != RecordingState::Enabled {
            return;
        }
        // Sample the depth *before* this send (free slots = remaining capacity) so the
        // peak reflects the deepest the queue actually got under load.
        let depth = self
            .items
            .max_capacity()
            .saturating_sub(self.items.capacity());
        self.peak_depth = self.peak_depth.max(depth);
        match self.items.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.fault(RecordError::QueueOverflow),
            Err(mpsc::error::TrySendError::Closed(_)) => self.state = RecordingState::Faulted,
        }
    }

    fn fault(&mut self, err: RecordError) {
        self.state = RecordingState::Faulted;
        let _ = self.fault.set(err.to_string());
        if let Some(terminate) = self.terminate.take() {
            let _ = terminate.send(RecordingStopReason::Faulted(err));
        }
    }

    /// Stop recording for a non-fault reason (user disable or Channel stop),
    /// letting the recorder drain the accepted backlog and finalize (§56, §110).
    ///
    /// Returns the terminal fault if the stop was **not clean**: a backlog
    /// write failure (accepted data truncated at that point) or a failed
    /// finalize (the buffered tail may be lost). A stop that lost data must
    /// never read as clean (§56.1) — callers surface `Some` as a recording
    /// fault, not a "stopped" note.
    pub async fn finalize(mut self, reason: RecordingStopReason) -> Option<String> {
        if self.state == RecordingState::Enabled {
            self.state = RecordingState::Disabled;
            if let Some(terminate) = self.terminate.take() {
                let _ = terminate.send(reason);
            }
        }
        drop(self.items);
        let _ = self.task.await;
        self.fault.get().cloned()
    }
}

/// Start a Raw Recording task and return its producer handle (§53, §142).
pub fn start_raw_recording<R: RawRecorder + 'static>(
    recorder: R,
    capacity: usize,
) -> Recording<Arc<ReceivedData>> {
    spawn_recording(recorder, capacity)
}

/// Start a Display Recording task and return its producer handle (§54, §142).
pub fn start_display_recording<R: DisplayRecorder + 'static>(
    recorder: R,
    capacity: usize,
) -> Recording<RenderedOutput> {
    spawn_recording(recorder, capacity)
}

fn spawn_recording<I, W>(writer: W, capacity: usize) -> Recording<I>
where
    I: Send + 'static,
    W: RecorderWriter<I> + 'static,
{
    let (items_tx, items_rx) = mpsc::channel(capacity.max(1));
    let (term_tx, term_rx) = oneshot::channel();
    let fault = Arc::new(OnceLock::new());
    let task = tokio::spawn(run_recorder(writer, items_rx, term_rx, Arc::clone(&fault)));
    Recording {
        items: items_tx,
        terminate: Some(term_tx),
        state: RecordingState::Enabled,
        task,
        fault,
        peak_depth: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime};
    use crate::transport::ReceivedPayload;

    fn chunk(bytes: &[u8]) -> Arc<ReceivedData> {
        Arc::new(ReceivedData {
            channel_id: ChannelId::new(),
            payload: ReceivedPayload::Bytes(bytes.to_vec()),
            received_at: ChunkTime::now(),
        })
    }

    /// A recorder whose write always fails — exercises the write-fault path.
    struct FailingRecorder;

    #[async_trait::async_trait]
    impl RawRecorder for FailingRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            Err(RecordError::QueueOverflow)
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn write_error_faults_and_ends_the_recorder_task() {
        let (tx, rx) = mpsc::channel(4);
        let (_term_tx, term_rx) = oneshot::channel();
        let fault = Arc::new(OnceLock::new());
        let task = tokio::spawn(run_recorder(FailingRecorder, rx, term_rx, fault));

        tx.send(chunk(b"data")).await.unwrap();
        // The write fails, so the task finalizes and ends on its own.
        task.await.unwrap();
        // The receiver is gone: further sends fail (the channel is closed).
        assert!(tx.send(chunk(b"more")).await.is_err());
    }

    /// A recorder whose write fails with a disk-style I/O error — for the
    /// quiet-stream fault-visibility tests.
    struct DiskFailRecorder;

    #[async_trait::async_trait]
    impl RawRecorder for DiskFailRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            Err(RecordError::Io(std::io::Error::other("disk full")))
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// The core §56.1 visibility guarantee: after a write fault, the *handle*
    /// reads `Faulted` with **no further enqueues** — a stream that goes quiet
    /// right after the disk fails must not show Enabled forever.
    #[tokio::test]
    async fn write_fault_is_visible_on_the_handle_without_further_enqueues() {
        let mut recording = start_raw_recording(DiskFailRecorder, 8);
        recording.try_record(chunk(b"data")); // accepted; the write itself fails

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while recording.state() != RecordingState::Faulted {
            assert!(
                tokio::time::Instant::now() < deadline,
                "handle never observed the write fault"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let why = recording.fault_error().expect("fault carries its error");
        assert!(why.contains("disk full"), "unexpected fault message: {why}");
    }

    /// A recorder whose first write succeeds and second fails — the
    /// backlog-drain-during-stop fault path.
    #[derive(Default)]
    struct SecondWriteFails {
        writes: usize,
    }

    #[async_trait::async_trait]
    impl RawRecorder for SecondWriteFails {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            self.writes += 1;
            if self.writes >= 2 {
                Err(RecordError::Io(std::io::Error::other("disk full")))
            } else {
                Ok(())
            }
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// §56.1 honesty: a stop that truncates the accepted backlog (a write
    /// fails while draining) must NOT read as clean — `finalize` returns the
    /// fault instead of `None`.
    #[tokio::test]
    async fn dirty_stop_truncating_backlog_reports_a_fault() {
        let mut recording = start_raw_recording(SecondWriteFails::default(), 8);
        recording.try_record(chunk(b"one"));
        recording.try_record(chunk(b"two")); // this write fails
        let fault = recording.finalize(RecordingStopReason::Disabled).await;
        assert!(
            fault
                .expect("dirty stop must surface")
                .contains("disk full"),
            "the fault carries the cause"
        );
    }

    /// The counterpart: a stop with a healthy writer reads clean.
    #[tokio::test]
    async fn clean_stop_reports_no_fault() {
        let mut recording = start_raw_recording(CountingRecorder::default(), 8);
        recording.try_record(chunk(b"data"));
        let fault = recording.finalize(RecordingStopReason::Disabled).await;
        assert!(fault.is_none());
    }

    /// A recorder whose writes succeed but whose periodic flush fails — the
    /// fault path that needs no traffic at all.
    struct FlushFailRecorder;

    #[async_trait::async_trait]
    impl RawRecorder for FlushFailRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Err(RecordError::Io(std::io::Error::other("flush: device gone")))
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// A periodic-flush failure faults the recording and the handle sees it —
    /// paused tokio time drives the [`FLUSH_INTERVAL`] tick without real waits.
    #[tokio::test(start_paused = true)]
    async fn flush_failure_faults_the_recording() {
        let recording = start_raw_recording(FlushFailRecorder, 8);
        // No traffic at all: only the flush timer can fault this recording.
        let mut waited = std::time::Duration::ZERO;
        while recording.state() != RecordingState::Faulted {
            assert!(
                waited < FLUSH_INTERVAL * 10,
                "handle never observed the flush fault"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            waited += std::time::Duration::from_millis(100);
        }
        let why = recording.fault_error().expect("fault carries its error");
        assert!(
            why.contains("device gone"),
            "unexpected fault message: {why}"
        );
    }

    /// A recorder that counts the chunks it accepts — for the handle's
    /// graceful-drain path.
    #[derive(Clone, Default)]
    struct CountingRecorder {
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RawRecorder for CountingRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    /// A recorder whose **first** write blocks until released, then all writes pass —
    /// so items pile up in the queue (observable depth high-water mark) without
    /// deadlocking the graceful-drain on finalize.
    struct BlockFirstRecorder {
        release: Arc<tokio::sync::Notify>,
        blocked_once: bool,
    }

    #[async_trait::async_trait]
    impl RawRecorder for BlockFirstRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            if !self.blocked_once {
                self.blocked_once = true;
                self.release.notified().await; // hold only the first write
            }
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn queue_depth_tracks_current_and_peak() {
        let release = Arc::new(tokio::sync::Notify::new());
        let recorder = BlockFirstRecorder {
            release: release.clone(),
            blocked_once: false,
        };
        let mut recording = start_raw_recording(recorder, 8);

        // Empty to start.
        assert_eq!(recording.queue_depth(), (0, 0, 8));

        // The task takes the first item and blocks in write_chunk; the rest sit in the
        // queue. Yield so the task can pull the first item before we measure.
        for _ in 0..5 {
            recording.try_record(chunk(b"x"));
        }
        tokio::task::yield_now().await;

        let (_current, peak, capacity) = recording.queue_depth();
        assert_eq!(capacity, 8);
        assert!(peak >= 1, "the peak should reflect the backlog, got {peak}");
        assert!(peak <= 8, "the peak can never exceed capacity, got {peak}");

        // Release the first write so the backlog drains, then finalize cleanly.
        // `notify_one` leaves a stored permit even if the task isn't parked yet, so this
        // can't race ahead of the writer's `.notified().await` and hang.
        release.notify_one();
        recording.finalize(RecordingStopReason::Disabled).await;
    }

    /// A recorder that counts flush calls — for the periodic-flush contract.
    #[derive(Clone, Default)]
    struct FlushCountingRecorder {
        flushes: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RawRecorder for FlushCountingRecorder {
        async fn write_chunk(&mut self, _chunk: &ReceivedData) -> Result<(), RecordError> {
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), RecordError> {
            self.flushes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn finalize(&mut self, _reason: RecordingStopReason) -> Result<(), RecordError> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_recorder_flushes_on_a_timer() {
        // §56: buffered output reaches the OS within FLUSH_INTERVAL while the
        // recording runs — not only at finalize — so a slow stream's file never
        // lags by more than a tick and a crash loses at most one interval.
        let recorder = FlushCountingRecorder::default();
        let flushes = recorder.flushes.clone();
        let mut recording = start_raw_recording(recorder, 8);

        recording.try_record(chunk(b"x"));
        tokio::task::yield_now().await; // let the write land
        let before = flushes.load(std::sync::atomic::Ordering::SeqCst);

        // Paused-clock test: advancing time fires the flush interval.
        tokio::time::advance(FLUSH_INTERVAL + std::time::Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        assert!(
            flushes.load(std::sync::atomic::Ordering::SeqCst) > before,
            "a running recorder flushes on the timer, not only at finalize"
        );

        recording.finalize(RecordingStopReason::Disabled).await;
    }

    #[tokio::test]
    async fn graceful_finalize_drains_the_backlog() {
        let recorder = CountingRecorder::default();
        let count = recorder.count.clone();
        let mut recording = start_raw_recording(recorder, 16);

        for _ in 0..3 {
            recording.try_record(chunk(b"x"));
        }
        assert_eq!(recording.state(), RecordingState::Enabled);

        // Graceful stop drains all three before finalizing (§110).
        recording.finalize(RecordingStopReason::Disabled).await;
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
