//! Per-Channel processing pipeline (spec §102, §99.1, §108).
//!
//! Wires one Channel's processing stages: chunk distribution → raw recording
//! tap (§53) → extraction → metadata → decoding (§107) → fan-out to display,
//! retention (bounded by count + bytes, §88), and diagnostics. The §152
//! backpressure invariants are tested against this pipeline.
//!
//! Acquisition priority (§5.9, §100): every edge here is non-blocking — display
//! drops oldest, retention evicts oldest, a recorder faults on overflow. The
//! only edge permitted to stall the reader is the Transport→Extractor channel
//! (§97.1), the bounded `tokio::sync::mpsc` that feeds [`run_channel`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use super::snapshot::{ChannelSnapshot, DiagnosticsSnapshot, DisplayViewSnapshot, SnapshotRequest};

use crate::core::{
    ChannelId, DisplayViewId, Message, MessageBytes, ProtocolMetadata, RecordingState, RuntimeEvent,
};
use crate::decode::Decoder;
use crate::diagnostics::{Diagnostic, DiagnosticLog};
use crate::display::{DisplayView, RenderedOutput, Renderer};
use crate::extract::MessageExtractor;
use crate::record::{Recording, RecordingStopReason};
use crate::retention::{ByteSized, MessageRetention, RetentionStore};
use crate::transport::{ReceivedData, ReceivedPayload};

use super::metadata::MessageNumbering;
use super::queue::DropOldestQueue;

/// Bounded capacities for a Channel's fan-out edges (§99, §124). Defaults are
/// generous; real values come from `RetentionConfig`/`RecordingConfig` later.
#[derive(Clone, Copy, Debug)]
pub struct PipelineCapacities {
    /// The bounded Transport→Extractor queue — the only edge that may stall the
    /// reader (§97.1, §99).
    pub ingest: usize,
    pub display: usize,
    /// Retained-Message **count** limit (§88).
    pub retention: usize,
    /// Retained-Message total **byte** limit (§88), if any.
    pub retention_bytes: Option<usize>,
    pub raw_recording: usize,
    /// Per-type retained-diagnostic limits (§88): events, warnings, errors.
    pub event_retention: Option<usize>,
    pub warning_retention: Option<usize>,
    pub error_retention: Option<usize>,
    /// Runtime→UI event stream ([`RuntimeEvent`], §137).
    pub events: usize,
}

impl Default for PipelineCapacities {
    fn default() -> Self {
        Self {
            ingest: 256,
            display: 1024,
            retention: 1024,
            retention_bytes: None,
            raw_recording: 1024,
            event_retention: None,
            warning_retention: None,
            error_retention: None,
            events: 256,
        }
    }
}

/// An immutable Message paired with its decoder annotation (§107, ADR-002). The
/// metadata is read-only and logically separate from the Message (§4.6); it is
/// `None` when no decoder is configured for the Channel.
#[derive(Clone, Debug)]
pub struct DecodedMessage {
    pub message: Arc<Message>,
    pub protocol: Option<ProtocolMetadata>,
}

impl ByteSized for DecodedMessage {
    fn byte_len(&self) -> usize {
        self.message.bytes.len()
    }
}

/// A handle to one Display View's runtime pause state (§11). Cloneable and
/// shareable across the pipeline-task boundary: a UI/orchestrator holds a clone
/// and flips pause/resume without a command channel into the pipeline.
#[derive(Clone, Debug)]
pub struct DisplayViewHandle {
    pub id: DisplayViewId,
    paused: Arc<AtomicBool>,
}

impl DisplayViewHandle {
    fn new_active() -> Self {
        Self {
            id: DisplayViewId::new(),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Pause this view: it stops accumulating new display items. Reception,
    /// recording, numbering, retention, and *other* views are unaffected (§50).
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// A Display View's optional recorder: its renderer plus the display-recording
/// handle (§54). Recording runs regardless of pause (§58).
struct ViewRecorder {
    renderer: DisplayView,
    recording: Recording<RenderedOutput>,
}

/// One Display View's runtime state: pause handle, bounded history (§87), and an
/// optional Display Recording (§54).
struct PipelineDisplayView {
    handle: DisplayViewHandle,
    history: DropOldestQueue<DecodedMessage>,
    recorder: Option<ViewRecorder>,
}

impl PipelineDisplayView {
    fn new(capacity: usize) -> Self {
        Self {
            handle: DisplayViewHandle::new_active(),
            history: DropOldestQueue::with_capacity(capacity),
            recorder: None,
        }
    }
}

/// One Channel's processing pipeline (§102). Driven synchronously via
/// [`ChannelPipeline::ingest`]; [`run_channel`] is the async loop around it.
/// (No `Debug` derive: the boxed `dyn MessageExtractor` is not `Debug`.)
pub struct ChannelPipeline {
    channel_id: ChannelId,
    extractor: Box<dyn MessageExtractor + Send>,
    numbering: MessageNumbering,
    decoder: Option<Box<dyn Decoder + Send>>,
    /// Display Views (§48): a Channel may have several, each independently
    /// paused (§11). The first is the default view created by `new`.
    display_views: Vec<PipelineDisplayView>,
    retention: MessageRetention<DecodedMessage>,
    /// Retained diagnostics, count-limited per type (§88).
    diagnostics: DiagnosticLog,
    /// Raw Recording handle (§53). `None` when recording is disabled or its
    /// enable failed (§55). Faults non-blockingly on overflow (§56.1).
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
    /// Whether the raw recorder's fault has already been reported.
    recording_fault_reported: bool,
    events: Option<Sender<RuntimeEvent>>,
}

impl ChannelPipeline {
    pub fn new(
        channel_id: ChannelId,
        extractor: Box<dyn MessageExtractor + Send>,
        caps: PipelineCapacities,
    ) -> Self {
        Self {
            channel_id,
            extractor,
            numbering: MessageNumbering::new(),
            decoder: None,
            display_views: vec![PipelineDisplayView::new(caps.display)],
            retention: MessageRetention::new(Some(caps.retention), caps.retention_bytes),
            diagnostics: DiagnosticLog::new(
                caps.event_retention,
                caps.warning_retention,
                caps.error_retention,
            ),
            raw_recorder: None,
            recording_fault_reported: false,
            events: None,
        }
    }

    /// Attach a Raw Recording handle (§53). The orchestrator creates it at Start
    /// (the file open is async and may fail per §55); the pipeline only feeds and
    /// finalizes it.
    pub fn with_raw_recorder(mut self, recorder: Recording<Arc<ReceivedData>>) -> Self {
        self.raw_recorder = Some(recorder);
        self
    }

    /// Attach a runtime event sender so each completed Message emits a
    /// `MessageReceived` event (§137). Events are advisory and high-volume, so
    /// emission is non-blocking and drops on a full event channel — the
    /// authoritative record lives in retention/recording, not the event stream.
    pub fn with_event_sender(mut self, events: Sender<RuntimeEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// Attach a protocol decoder (§30, §107). Each completed Message is decoded;
    /// the resulting Protocol Metadata travels with the Message through fan-out,
    /// and any validation errors are surfaced as diagnostics (§32, §37).
    pub fn with_decoder(mut self, decoder: Box<dyn Decoder + Send>) -> Self {
        self.decoder = Some(decoder);
        self
    }

    /// Process one received chunk through the pipeline.
    ///
    /// Distribution order follows §99.1: the raw recorder is offered the chunk
    /// first with a non-blocking enqueue, then extraction runs. A `Bytes` chunk
    /// is framed by the extractor; a `Datagram` is already one complete Message
    /// and bypasses extraction entirely (§15).
    pub fn ingest(&mut self, data: ReceivedData) {
        let data = Arc::new(data);

        // 1. Raw recorder tap (pre-extraction, §53). Non-blocking: a full
        // recorder queue faults the recording rather than stalling reception
        // (§56.1). `try_record` updates the handle's state internally.
        let mut recording_just_faulted = false;
        if let Some(recorder) = self.raw_recorder.as_mut() {
            if recorder.state() == RecordingState::Enabled {
                recorder.try_record(Arc::clone(&data));
                if recorder.state() == RecordingState::Faulted && !self.recording_fault_reported {
                    self.recording_fault_reported = true;
                    recording_just_faulted = true;
                }
            }
        }
        if recording_just_faulted {
            self.diagnostics.record(Diagnostic::error(format!(
                "raw recording faulted on channel {}",
                self.channel_id
            )));
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingFaulted(self.channel_id));
            }
        }

        // 2. Extraction → metadata → fan-out.
        match &data.payload {
            ReceivedPayload::Datagram(bytes) => {
                let mb = MessageBytes {
                    bytes: Arc::from(bytes.as_slice()),
                    first_chunk: data.received_at,
                    last_chunk: data.received_at,
                };
                let msg = Arc::new(self.numbering.build(self.channel_id, mb));
                self.dispatch(msg);
            }
            ReceivedPayload::Bytes(bytes) => {
                for mb in self.extractor.push_chunk(bytes, data.received_at) {
                    let msg = Arc::new(self.numbering.build(self.channel_id, mb));
                    self.dispatch(msg);
                }
            }
        }
    }

    /// Decode (§107), then fan-out the Message + its annotation to the
    /// non-blocking consumer edges (§108).
    fn dispatch(&mut self, msg: Arc<Message>) {
        let number = msg.number;

        // Decoding stage (§102): read-only; failures are isolated (§32) and
        // surfaced as diagnostics (§37), never stopping the pipeline.
        let protocol = if let Some(decoder) = &self.decoder {
            let result = decoder.decode(&msg);
            for err in &result.errors {
                self.diagnostics.record(Diagnostic::warning(format!(
                    "decoder: {err} (channel {}, message {number})",
                    self.channel_id
                )));
            }
            result.metadata
        } else {
            None
        };
        let decoded = DecodedMessage {
            message: msg,
            protocol,
        };

        // Display fan-out (§48, §108). For each view:
        //  - Display Recording renders + records the Message regardless of pause
        //    (§58): pausing presentation never pauses recording.
        //  - Presentation history accumulates only while Active (§50), dropping
        //    oldest on overflow (§99), without affecting reception/retention/
        //    numbering or other views.
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.as_mut() {
                let rendered = rec.renderer.render(&decoded.message);
                rec.recording.try_record(rendered);
            }
            if !view.handle.is_paused() {
                let _ = view.history.push(decoded.clone());
            }
        }
        // Retention: bounded by count and bytes (§88), evicting oldest (§89).
        // Message Numbers are never rewritten, so survivors keep their numbers.
        self.retention.push(decoded);
        // Event stream (§137): advisory, non-blocking, drop on full.
        if let Some(events) = &self.events {
            let _ = events.try_send(RuntimeEvent::MessageReceived(self.channel_id, number));
        }
    }

    /// Called at Channel stop (§110, §112): discard any partial Message (§112)
    /// and finalize the recorder — flush and close (§56).
    pub async fn finish(&mut self) {
        let _ = self.extractor.finish();
        if let Some(recorder) = self.raw_recorder.take() {
            recorder.finalize(RecordingStopReason::ChannelStopped).await;
        }
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.take() {
                rec.recording
                    .finalize(RecordingStopReason::ChannelStopped)
                    .await;
            }
        }
    }

    // --- Inspection (used by the runtime and tests) ---

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// The Message Number that will be assigned next (== messages produced + 1).
    pub fn next_message_number(&self) -> u64 {
        self.numbering.peek()
    }

    /// Add a Display View and return its pause handle (§48). The caller keeps
    /// the handle to pause/resume the view across the pipeline-task boundary.
    pub fn add_display_view(&mut self, capacity: usize) -> DisplayViewHandle {
        let view = PipelineDisplayView::new(capacity);
        let handle = view.handle.clone();
        self.display_views.push(view);
        handle
    }

    /// Attach a Display Recording to the primary (first) Display View (§54),
    /// rendering each Message with `renderer`. v1 records the primary view;
    /// per-view display-recording config is deferred (Appendix A).
    pub fn set_display_recorder(
        &mut self,
        renderer: DisplayView,
        recording: Recording<RenderedOutput>,
    ) {
        if let Some(view) = self.display_views.first_mut() {
            view.recorder = Some(ViewRecorder {
                renderer,
                recording,
            });
        }
    }

    /// Pause/resume handles for every Display View, default first (§48).
    pub fn display_view_handles(&self) -> Vec<DisplayViewHandle> {
        self.display_views
            .iter()
            .map(|v| v.handle.clone())
            .collect()
    }

    /// The retained history of a specific Display View.
    pub fn display_view(&self, id: DisplayViewId) -> Option<&DropOldestQueue<DecodedMessage>> {
        self.display_views
            .iter()
            .find(|v| v.handle.id == id)
            .map(|v| &v.history)
    }

    /// The default Display View's history (the first view, created by `new`).
    pub fn display_queue(&self) -> &DropOldestQueue<DecodedMessage> {
        &self.display_views[0].history
    }

    pub fn retention(&self) -> &MessageRetention<DecodedMessage> {
        &self.retention
    }

    pub fn diagnostics(&self) -> &DiagnosticLog {
        &self.diagnostics
    }

    /// Current raw-recording state, or `None` if raw recording is not attached.
    pub fn raw_recording_state(&self) -> Option<RecordingState> {
        self.raw_recorder.as_ref().map(|r| r.state())
    }

    /// Build an owned, point-in-time snapshot of the observable state (§137,
    /// ADR-006): retained Messages, per-view display history, diagnostics, and
    /// recording state. Messages are shared as `Arc`s, so this clones references,
    /// not payloads.
    pub fn snapshot(&self) -> ChannelSnapshot {
        ChannelSnapshot {
            channel_id: self.channel_id,
            next_message_number: self.numbering.peek(),
            retained: self.retention.iter().cloned().collect(),
            display_views: self
                .display_views
                .iter()
                .map(|v| DisplayViewSnapshot {
                    id: v.handle.id,
                    paused: v.handle.is_paused(),
                    messages: v.history.iter().cloned().collect(),
                })
                .collect(),
            diagnostics: DiagnosticsSnapshot {
                events: self.diagnostics.events().cloned().collect(),
                warnings: self.diagnostics.warnings().cloned().collect(),
                errors: self.diagnostics.errors().cloned().collect(),
            },
            raw_recording: self.raw_recorder.as_ref().map(|r| r.state()),
        }
    }
}

/// The async ingest loop for one Channel (§102, §110, §111).
///
/// Drains the bounded Transport→Extractor channel until cancelled or the sender
/// is dropped, then discards partials (§112) and returns the pipeline so the
/// caller can finalize/inspect it. Cancellation is cooperative and checked
/// first (`biased`) so shutdown does not depend on draining the queue (§111).
///
/// Between reads it also serves snapshot requests (§137, ADR-006): a requester
/// sends a oneshot reply on `snapshots` and the loop answers from current state.
/// Snapshots are checked ahead of reads (they are rare and cheap) so a request
/// is served promptly; a closed `snapshots` channel simply stops being polled.
pub async fn run_channel(
    mut ingest: Receiver<ReceivedData>,
    mut snapshots: Receiver<SnapshotRequest>,
    mut pipeline: ChannelPipeline,
    cancel: CancellationToken,
) -> ChannelPipeline {
    let mut snapshots_open = true;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            reply = snapshots.recv(), if snapshots_open => match reply {
                Some(tx) => {
                    let _ = tx.send(pipeline.snapshot());
                }
                None => snapshots_open = false, // all requesters gone; keep running
            },
            maybe = ingest.recv() => match maybe {
                Some(data) => pipeline.ingest(data),
                None => break,
            },
        }
    }
    pipeline.finish().await;
    pipeline
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ChunkTime;
    use crate::extract::{DelimiterExtractor, StreamExtractor};

    fn lf_pipeline(cid: ChannelId, caps: PipelineCapacities) -> ChannelPipeline {
        ChannelPipeline::new(
            cid,
            Box::new(DelimiterExtractor::new(vec![b'\n'], false)),
            caps,
        )
    }

    fn bytes_chunk(cid: ChannelId, data: &[u8]) -> ReceivedData {
        ReceivedData {
            channel_id: cid,
            payload: ReceivedPayload::Bytes(data.to_vec()),
            received_at: ChunkTime::now(),
        }
    }

    fn datagram(cid: ChannelId, data: &[u8]) -> ReceivedData {
        ReceivedData {
            channel_id: cid,
            payload: ReceivedPayload::Datagram(data.to_vec()),
            received_at: ChunkTime::now(),
        }
    }

    #[test]
    fn bytes_are_extracted_numbered_and_fanned_out() {
        let cid = ChannelId::new();
        let mut p = lf_pipeline(cid, PipelineCapacities::default());
        p.ingest(bytes_chunk(cid, b"A\nB\n"));
        let nums: Vec<u64> = p.retention().iter().map(|d| d.message.number).collect();
        assert_eq!(nums, vec![1, 2]);
        assert_eq!(p.display_queue().len(), 2);
        assert_eq!(p.next_message_number(), 3);
    }

    #[test]
    fn datagram_becomes_one_message_bypassing_extraction() {
        let cid = ChannelId::new();
        // Stream extractor would never emit; the datagram path must not use it.
        let mut p = ChannelPipeline::new(
            cid,
            Box::new(StreamExtractor::new()),
            PipelineCapacities::default(),
        );
        p.ingest(datagram(cid, b"hello"));
        assert_eq!(p.retention().len(), 1);
        let decoded = p.retention().iter().next().unwrap();
        assert_eq!(decoded.message.number, 1);
        assert_eq!(&*decoded.message.bytes, b"hello");
    }

    #[test]
    fn decoder_annotates_messages_and_surfaces_validation_errors() {
        use crate::decode::NmeaDecoder;
        use nmea0183::{NmeaChecksumMode, NmeaSentence, SentenceType, TalkerId};

        let cid = ChannelId::new();
        let mut p = ChannelPipeline::new(
            cid,
            Box::new(StreamExtractor::new()),
            PipelineCapacities::default(),
        )
        .with_decoder(Box::new(NmeaDecoder::standard()));

        // Valid sentence → metadata attached, no diagnostic.
        let valid = NmeaSentence::new(TalkerId::GP, SentenceType::GLL, vec![]).to_wire();
        p.ingest(datagram(cid, valid.as_bytes()));
        // Bad checksum → still annotated, but a warning diagnostic is raised (§37).
        let bad = NmeaSentence::new(TalkerId::GP, SentenceType::HDT, vec!["1".into()])
            .to_wire_with(NmeaChecksumMode::Wrong);
        p.ingest(datagram(cid, bad.as_bytes()));

        let decoded: Vec<_> = p.retention().iter().collect();
        assert_eq!(decoded.len(), 2);
        assert_eq!(
            decoded[0]
                .protocol
                .as_ref()
                .unwrap()
                .message_type
                .as_deref(),
            Some("GLL")
        );
        assert_eq!(
            decoded[1]
                .protocol
                .as_ref()
                .unwrap()
                .message_type
                .as_deref(),
            Some("HDT")
        );
        // The invalid checksum produced exactly one decoder warning diagnostic.
        assert_eq!(p.diagnostics().warnings().count(), 1);
    }

    #[test]
    fn diagnostic_warning_retention_limit_is_applied() {
        // §88: the per-type diagnostic limit bounds retained warnings.
        use crate::decode::NmeaDecoder;
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            warning_retention: Some(2),
            ..PipelineCapacities::default()
        };
        let mut p = ChannelPipeline::new(cid, Box::new(StreamExtractor::new()), caps)
            .with_decoder(Box::new(NmeaDecoder::standard()));
        // Four non-NMEA datagrams → four decoder warnings, capped at two.
        for _ in 0..4 {
            p.ingest(datagram(cid, b"not-nmea"));
        }
        assert_eq!(p.diagnostics().warnings().count(), 2);
    }

    #[test]
    fn display_overflow_does_not_stop_reception() {
        // §152: a slow/full display must not stall the pipeline.
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            display: 2,
            retention: 100,
            ..PipelineCapacities::default()
        };
        let mut p = lf_pipeline(cid, caps);
        p.ingest(bytes_chunk(cid, b"1\n2\n3\n4\n5\n"));
        // Reception continued: all five were numbered and retained.
        assert_eq!(p.next_message_number(), 6);
        assert_eq!(p.retention().len(), 5);
        // Display kept only the two newest (dropped oldest).
        let disp: Vec<u64> = p.display_queue().iter().map(|d| d.message.number).collect();
        assert_eq!(disp, vec![4, 5]);
    }

    #[test]
    fn retention_eviction_preserves_message_numbering() {
        // §152 / §89: evicted messages are not renumbered.
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            display: 100,
            retention: 3,
            ..PipelineCapacities::default()
        };
        let mut p = lf_pipeline(cid, caps);
        p.ingest(bytes_chunk(cid, b"1\n2\n3\n4\n5\n"));
        let nums: Vec<u64> = p.retention().iter().map(|d| d.message.number).collect();
        // Oldest two evicted; survivors keep their original numbers 3,4,5.
        assert_eq!(nums, vec![3, 4, 5]);
        assert_eq!(p.next_message_number(), 6);
    }

    #[test]
    fn pausing_one_view_does_not_affect_others_or_reception() {
        // §11/§50: pausing one Display View must not pause reception, numbering,
        // retention, or the other views.
        let cid = ChannelId::new();
        let mut p = lf_pipeline(cid, PipelineCapacities::default());
        let default_view = p.display_view_handles()[0].clone();
        let second_view = p.add_display_view(1024);

        default_view.pause();
        p.ingest(bytes_chunk(cid, b"1\n2\n3\n"));

        // Paused view accumulated nothing; the active view got all three.
        assert_eq!(p.display_view(default_view.id).unwrap().len(), 0);
        assert_eq!(p.display_view(second_view.id).unwrap().len(), 3);
        // Reception, numbering, and retention were unaffected.
        assert_eq!(p.retention().len(), 3);
        assert_eq!(p.next_message_number(), 4);

        // Resuming lets the view accumulate again — but only new Messages (no
        // backfill of what was missed while paused, §55-style live semantics).
        default_view.resume();
        p.ingest(bytes_chunk(cid, b"4\n"));
        assert_eq!(p.display_view(default_view.id).unwrap().len(), 1);
        assert_eq!(p.display_view(second_view.id).unwrap().len(), 4);
    }

    #[test]
    fn retention_byte_limit_evicts_oldest() {
        // §88: a total-byte limit bounds retention independently of count.
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            retention: 100,
            retention_bytes: Some(3),
            ..PipelineCapacities::default()
        };
        let mut p = lf_pipeline(cid, caps);
        // Five 1-byte messages; a 3-byte budget keeps the newest three.
        p.ingest(bytes_chunk(cid, b"1\n2\n3\n4\n5\n"));
        let nums: Vec<u64> = p.retention().iter().map(|d| d.message.number).collect();
        assert_eq!(nums, vec![3, 4, 5]);
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("listener-pipe-{tag}-{}.bin", uuid::Uuid::new_v4()));
        path
    }

    #[tokio::test]
    async fn raw_recording_captures_pre_extraction_bytes() {
        use crate::record::{start_raw_recording, OverwritePolicy, RawFileRecorder};

        let path = temp_path("capture");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let cid = ChannelId::new();
        let mut p = lf_pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(start_raw_recording(recorder, 64));

        p.ingest(bytes_chunk(cid, b"$GPGGA,"));
        p.ingest(bytes_chunk(cid, b"123\r\n"));
        // Finalize flushes + closes the file (drains the recorder queue).
        p.finish().await;

        // Raw recording is byte-exact and pre-extraction (§53): the CRLF and the
        // un-split chunk boundary are both present.
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"$GPGGA,123\r\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn display_recording_captures_rendered_output() {
        use crate::display::DisplayView;
        use crate::record::{start_display_recording, DisplayFileRecorder, OverwritePolicy};

        let path = temp_path("display-rec");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let cid = ChannelId::new();
        let mut p = lf_pipeline(cid, PipelineCapacities::default());
        p.set_display_recorder(
            DisplayView::default(),
            start_display_recording(recorder, 16),
        );

        p.ingest(datagram(cid, b"Hi"));
        p.finish().await;

        // Raw/Native render of "Hi" → one rendered line in the artifact (§54).
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "Hi\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn display_recording_continues_while_the_view_is_paused() {
        use crate::display::DisplayView;
        use crate::record::{start_display_recording, DisplayFileRecorder, OverwritePolicy};

        let path = temp_path("display-paused");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let cid = ChannelId::new();
        let mut p = lf_pipeline(cid, PipelineCapacities::default());
        let view = p.display_view_handles()[0].clone();
        p.set_display_recorder(
            DisplayView::default(),
            start_display_recording(recorder, 16),
        );

        // §58: pausing the view's presentation must not pause its recording.
        view.pause();
        p.ingest(datagram(cid, b"A"));
        p.ingest(datagram(cid, b"B"));
        p.finish().await;

        // Presentation history is frozen...
        assert_eq!(p.display_view(view.id).unwrap().len(), 0);
        // ...but both Messages were still recorded.
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "A\nB\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn recording_overflow_faults_emits_event_and_reception_continues() {
        // §56.1 / §152: a full recorder queue faults recording (emitting
        // RecordingFaulted) but reception, extraction, and retention continue.
        use crate::record::{start_raw_recording, OverwritePolicy, RawFileRecorder};

        let path = temp_path("overflow");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let cid = ChannelId::new();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel(16);
        // Recorder queue capacity 1; two back-to-back sync ingests with no await
        // between cannot let the recorder task drain, so the second overflows.
        let mut p = lf_pipeline(cid, PipelineCapacities::default())
            .with_event_sender(ev_tx)
            .with_raw_recorder(start_raw_recording(recorder, 1));

        p.ingest(bytes_chunk(cid, b"A\n"));
        p.ingest(bytes_chunk(cid, b"B\n"));

        assert_eq!(p.raw_recording_state(), Some(RecordingState::Faulted));
        // Reception continued: both messages extracted and retained.
        assert_eq!(p.retention().len(), 2);
        // The fault was recorded as an error diagnostic (§94).
        assert_eq!(p.diagnostics().errors().count(), 1);

        let mut saw_fault = false;
        while let Ok(event) = ev_rx.try_recv() {
            if matches!(event, RuntimeEvent::RecordingFaulted(_)) {
                saw_fault = true;
            }
        }
        assert!(saw_fault, "RecordingFaulted event must be emitted");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn run_channel_drains_then_stops_when_sender_drops() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let p = lf_pipeline(cid, PipelineCapacities::default());
        let (_snap_tx, snap_rx) = tokio::sync::mpsc::channel(4);
        tx.send(bytes_chunk(cid, b"X\nY\n")).await.unwrap();
        drop(tx); // loop drains the buffered chunk, then sees the channel close
        let p = run_channel(rx, snap_rx, p, CancellationToken::new()).await;
        assert_eq!(p.retention().len(), 2);
    }

    #[tokio::test]
    async fn run_channel_stops_on_cancellation_with_live_sender() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<ReceivedData>(8);
        let p = lf_pipeline(cid, PipelineCapacities::default());
        let (_snap_tx, snap_rx) = tokio::sync::mpsc::channel(4);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_channel(rx, snap_rx, p, cancel.clone()));
        cancel.cancel(); // only cancellation can end the loop — tx is still alive
        let p = handle.await.unwrap();
        drop(tx);
        assert_eq!(p.next_message_number(), 1);
    }

    #[tokio::test]
    async fn run_channel_serves_snapshots_while_running() {
        // §137/ADR-006: a snapshot is built on request from live pipeline state,
        // without stopping the channel.
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (snap_tx, snap_rx) = tokio::sync::mpsc::channel(4);
        let p = lf_pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_channel(rx, snap_rx, p, cancel.clone()));

        tx.send(bytes_chunk(cid, b"one\ntwo\n")).await.unwrap();

        // Request a snapshot; retry until both Messages are visible (the ingest
        // and the snapshot race in the select loop).
        let snapshot = loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            snap_tx.send(reply_tx).await.unwrap();
            let snap = reply_rx.await.unwrap();
            if snap.retained.len() == 2 {
                break snap;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(snapshot.channel_id, cid);
        assert_eq!(snapshot.next_message_number, 3);
        let nums: Vec<u64> = snapshot.retained.iter().map(|d| d.message.number).collect();
        assert_eq!(nums, vec![1, 2]);
        assert_eq!(snapshot.display_views.len(), 1);
        assert_eq!(snapshot.display_views[0].messages.len(), 2);

        cancel.cancel();
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn transport_to_extractor_queue_is_bounded() {
        // §97.1: the Transport→Extractor edge is the only one that may stall the
        // reader. A bounded channel rejects when full; the production path uses
        // `blocking_send`/`send().await`, which awaits (stalls) instead of erroring.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(1);
        tx.try_send(1).unwrap();
        assert!(tx.try_send(2).is_err()); // full → bounded backpressure
        assert_eq!(rx.recv().await, Some(1)); // draining frees a slot
        tx.try_send(2).unwrap();
    }
}
