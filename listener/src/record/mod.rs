//! Raw and display recording (spec §51–§59, §142).
//!
//! This is `listener-record` (§128). There are **two independent recording
//! systems** with different inputs, pipeline positions, and guarantees (§51):
//!
//! - **Raw Recording** ([`file::RawFileRecorder`]) — byte-oriented; taps the
//!   received-chunk stream *before* extraction (§53). Byte-exact and contiguous
//!   up to a known end (§5.6, §56.1).
//! - **Display Recording** ([`file::DisplayFileRecorder`]) — consumes a display
//!   view's rendered output *after* rendering (§54). Not byte-exact.
//!
//! Each runs as its own task draining a bounded queue (§142). The producer's
//! enqueue is non-blocking: on a full queue the recorder **faults** rather than
//! stalling reception (§56.1, §100). A faulted recording is contiguous from
//! start to a single truncation point, then ends — it never silently gaps and
//! resumes (§56.1).

pub mod file;

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::core::{RecordError, RecordingState};
use crate::display::RenderedOutput;
use crate::transport::ReceivedData;

pub use file::{DisplayFileRecorder, RawFileRecorder};

/// Which recording system is active for a Channel (§52). Raw is primary;
/// Display is optional; a Channel may run both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingMode {
    Disabled,
    Raw,
    Display,
}

/// What to do when the destination file already exists (§80.1). Enforced when
/// recording is enabled (§55, §121); `Refuse` is the default and never clobbers.
///
/// Defined here (record owns file lifecycle, §128); the profile schema will
/// reference this type when the config module lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OverwritePolicy {
    #[default]
    Refuse,
    Overwrite,
    AppendIfExists,
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

/// Writes received chunks exactly as received, before extraction (§53, §142).
#[async_trait::async_trait]
pub trait RawRecorder: Send {
    /// Append one received chunk (pre-extraction) exactly as received.
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
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
}

#[async_trait::async_trait]
impl<R: RawRecorder> RecorderWriter<Arc<ReceivedData>> for R {
    async fn write(&mut self, item: &Arc<ReceivedData>) -> Result<(), RecordError> {
        self.write_chunk(item).await
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
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError> {
        DisplayRecorder::finalize(self, reason).await
    }
}

/// The recorder task (§142): drain the bounded queue and write each item until
/// terminated (fault or stop). On a write error it faults and finalizes at the
/// truncation point; on graceful stop it drains the accepted backlog first
/// (§110); a fault never drains (§56.1).
async fn run_recorder<I, W>(
    mut writer: W,
    mut items: mpsc::Receiver<I>,
    mut terminate: oneshot::Receiver<RecordingStopReason>,
) where
    I: Send + 'static,
    W: RecorderWriter<I> + 'static,
{
    loop {
        tokio::select! {
            biased;
            reason = &mut terminate => {
                let reason = reason.unwrap_or(RecordingStopReason::ChannelStopped);
                if !reason.is_fault() {
                    // Graceful: drain the already-accepted backlog (§110).
                    while let Ok(item) = items.try_recv() {
                        if writer.write(&item).await.is_err() {
                            break;
                        }
                    }
                }
                let _ = writer.finalize(reason).await;
                return;
            }
            maybe = items.recv() => match maybe {
                Some(item) => {
                    if let Err(err) = writer.write(&item).await {
                        // Write fault: truncate here, do not drain (§56.1).
                        let _ = writer.finalize(RecordingStopReason::Faulted(err)).await;
                        return;
                    }
                }
                None => {
                    let _ = writer.finalize(RecordingStopReason::ChannelStopped).await;
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
}

impl<I: Send + 'static> Recording<I> {
    pub fn state(&self) -> RecordingState {
        self.state
    }

    /// Offer one item to the recorder. Non-blocking (§56.1): a full queue faults
    /// the recording; a closed queue (task already faulted on a write error)
    /// transitions the handle to `Faulted` lazily.
    pub fn try_record(&mut self, item: I) {
        if self.state != RecordingState::Enabled {
            return;
        }
        match self.items.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.fault(RecordError::QueueOverflow),
            Err(mpsc::error::TrySendError::Closed(_)) => self.state = RecordingState::Faulted,
        }
    }

    fn fault(&mut self, err: RecordError) {
        self.state = RecordingState::Faulted;
        if let Some(terminate) = self.terminate.take() {
            let _ = terminate.send(RecordingStopReason::Faulted(err));
        }
    }

    /// Stop recording for a non-fault reason (user disable or Channel stop),
    /// letting the recorder drain the accepted backlog and finalize (§56, §110).
    pub async fn finalize(mut self, reason: RecordingStopReason) {
        if self.state == RecordingState::Enabled {
            self.state = RecordingState::Disabled;
            if let Some(terminate) = self.terminate.take() {
                let _ = terminate.send(reason);
            }
        }
        drop(self.items);
        let _ = self.task.await;
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
    let task = tokio::spawn(run_recorder(writer, items_rx, term_rx));
    Recording {
        items: items_tx,
        terminate: Some(term_tx),
        state: RecordingState::Enabled,
        task,
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
        let task = tokio::spawn(run_recorder(FailingRecorder, rx, term_rx));

        tx.send(chunk(b"data")).await.unwrap();
        // The write fails, so the task finalizes and ends on its own.
        task.await.unwrap();
        // The receiver is gone: further sends fail (the channel is closed).
        assert!(tx.send(chunk(b"more")).await.is_err());
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
