//! Per-Channel processing pipeline (spec §102, §99.1, §108).
//!
//! Wires one Channel's processing stages: chunk distribution → extraction →
//! metadata → fan-out. This is the skeleton substrate the backpressure
//! invariants (§152) are tested against; the rich display/recording/retention
//! modules layer onto these fan-out edges in later steps. Decoding (§107) is an
//! optional stage wired once the `decode` module lands.
//!
//! Acquisition priority (§5.9, §100): every edge here is non-blocking — display
//! drops oldest, retention evicts oldest, a recorder faults on overflow. The
//! only edge permitted to stall the reader is the Transport→Extractor channel
//! (§97.1), the bounded `tokio::sync::mpsc` that feeds [`run_channel`].

use std::sync::Arc;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::core::{
    ChannelId, Message, MessageBytes, ProtocolMetadata, RecordingState, RuntimeEvent,
};
use crate::decode::Decoder;
use crate::extract::MessageExtractor;
use crate::transport::{ReceivedData, ReceivedPayload};

use super::metadata::MessageNumbering;
use super::queue::{
    Diagnostic, DiagnosticSeverity, DiagnosticsQueue, DropOldestQueue, FaultOnFullQueue,
};

/// Bounded capacities for a Channel's fan-out edges (§99, §124). Defaults are
/// generous; real values come from `RetentionConfig`/`RecordingConfig` later.
#[derive(Clone, Copy, Debug)]
pub struct PipelineCapacities {
    /// The bounded Transport→Extractor queue — the only edge that may stall the
    /// reader (§97.1, §99).
    pub ingest: usize,
    pub display: usize,
    pub retention: usize,
    pub raw_recording: usize,
    pub diagnostics: usize,
    /// Runtime→UI event stream ([`RuntimeEvent`], §137).
    pub events: usize,
}

impl Default for PipelineCapacities {
    fn default() -> Self {
        Self {
            ingest: 256,
            display: 1024,
            retention: 1024,
            raw_recording: 1024,
            diagnostics: 256,
            events: 256,
        }
    }
}

/// The raw-recording tap (§53): consumes the chunk stream *before* extraction.
/// On overflow it faults rather than stalling reception (§56.1).
#[derive(Debug)]
struct RawRecorderSink {
    queue: FaultOnFullQueue<Arc<ReceivedData>>,
    state: RecordingState,
}

/// An immutable Message paired with its decoder annotation (§107, ADR-002). The
/// metadata is read-only and logically separate from the Message (§4.6); it is
/// `None` when no decoder is configured for the Channel.
#[derive(Clone, Debug)]
pub struct DecodedMessage {
    pub message: Arc<Message>,
    pub protocol: Option<ProtocolMetadata>,
}

/// One Channel's processing pipeline (§102). Driven synchronously via
/// [`ChannelPipeline::ingest`]; [`run_channel`] is the async loop around it.
/// (No `Debug` derive: the boxed `dyn MessageExtractor` is not `Debug`.)
pub struct ChannelPipeline {
    channel_id: ChannelId,
    extractor: Box<dyn MessageExtractor + Send>,
    numbering: MessageNumbering,
    decoder: Option<Box<dyn Decoder + Send>>,
    display: DropOldestQueue<DecodedMessage>,
    retention: DropOldestQueue<DecodedMessage>,
    diagnostics: DiagnosticsQueue,
    raw_recorder: Option<RawRecorderSink>,
    events: Option<Sender<RuntimeEvent>>,
}

impl ChannelPipeline {
    pub fn new(
        channel_id: ChannelId,
        extractor: Box<dyn MessageExtractor + Send>,
        caps: PipelineCapacities,
        raw_recording_enabled: bool,
    ) -> Self {
        let raw_recorder = raw_recording_enabled.then(|| RawRecorderSink {
            queue: FaultOnFullQueue::with_capacity(caps.raw_recording),
            state: RecordingState::Enabled,
        });
        Self {
            channel_id,
            extractor,
            numbering: MessageNumbering::new(),
            decoder: None,
            display: DropOldestQueue::with_capacity(caps.display),
            retention: DropOldestQueue::with_capacity(caps.retention),
            diagnostics: DiagnosticsQueue::with_capacity(caps.diagnostics),
            raw_recorder,
            events: None,
        }
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

        // 1. Raw recorder tap (pre-extraction). Non-blocking; fault on overflow.
        let mut recording_faulted = false;
        if let Some(sink) = self.raw_recorder.as_mut() {
            if sink.state == RecordingState::Enabled
                && sink.queue.try_push(Arc::clone(&data)).is_err()
            {
                sink.state = RecordingState::Faulted; // §56.1: never stall the reader
                recording_faulted = true;
            }
        }
        if recording_faulted {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticSeverity::Error,
                format!("raw recording faulted on channel {}", self.channel_id),
            ));
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
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticSeverity::Warning,
                    format!(
                        "decoder: {err} (channel {}, message {number})",
                        self.channel_id
                    ),
                ));
            }
            result.metadata
        } else {
            None
        };
        let decoded = DecodedMessage {
            message: msg,
            protocol,
        };

        // Display: drop oldest on overflow (§99). Per-item loss is coalesced and
        // reported later (§101); we do not emit a diagnostic per dropped item.
        let _ = self.display.push(decoded.clone());
        // Retention: evict oldest (§89). Message Numbers are never rewritten, so
        // survivors keep their original numbers.
        let _ = self.retention.push(decoded);
        // Event stream (§137): advisory, non-blocking, drop on full.
        if let Some(events) = &self.events {
            let _ = events.try_send(RuntimeEvent::MessageReceived(self.channel_id, number));
        }
    }

    /// Called at Channel stop: any partial Message is discarded (§112).
    pub fn finish(&mut self) {
        let _ = self.extractor.finish();
    }

    // --- Inspection (used by the runtime and tests) ---

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// The Message Number that will be assigned next (== messages produced + 1).
    pub fn next_message_number(&self) -> u64 {
        self.numbering.peek()
    }

    pub fn display_queue(&self) -> &DropOldestQueue<DecodedMessage> {
        &self.display
    }

    pub fn retention(&self) -> &DropOldestQueue<DecodedMessage> {
        &self.retention
    }

    pub fn diagnostics(&self) -> &DiagnosticsQueue {
        &self.diagnostics
    }

    /// Current raw-recording state, or `None` if raw recording is not attached.
    pub fn raw_recording_state(&self) -> Option<RecordingState> {
        self.raw_recorder.as_ref().map(|s| s.state)
    }
}

/// The async ingest loop for one Channel (§102, §110, §111).
///
/// Drains the bounded Transport→Extractor channel until cancelled or the sender
/// is dropped, then discards partials (§112) and returns the pipeline so the
/// caller can finalize/inspect it. Cancellation is cooperative and checked
/// first (`biased`) so shutdown does not depend on draining the queue (§111).
pub async fn run_channel(
    mut ingest: Receiver<ReceivedData>,
    mut pipeline: ChannelPipeline,
    cancel: CancellationToken,
) -> ChannelPipeline {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            maybe = ingest.recv() => match maybe {
                Some(data) => pipeline.ingest(data),
                None => break,
            },
        }
    }
    pipeline.finish();
    pipeline
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ChunkTime;
    use crate::extract::{DelimiterExtractor, StreamExtractor};

    fn lf_pipeline(cid: ChannelId, caps: PipelineCapacities, raw: bool) -> ChannelPipeline {
        ChannelPipeline::new(
            cid,
            Box::new(DelimiterExtractor::new(vec![b'\n'], false)),
            caps,
            raw,
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
        let mut p = lf_pipeline(cid, PipelineCapacities::default(), false);
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
            false,
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
            false,
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
        // The invalid checksum produced exactly one decoder diagnostic.
        assert_eq!(p.diagnostics().len(), 1);
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
        let mut p = lf_pipeline(cid, caps, false);
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
        let mut p = lf_pipeline(cid, caps, false);
        p.ingest(bytes_chunk(cid, b"1\n2\n3\n4\n5\n"));
        let nums: Vec<u64> = p.retention().iter().map(|d| d.message.number).collect();
        // Oldest two evicted; survivors keep their original numbers 3,4,5.
        assert_eq!(nums, vec![3, 4, 5]);
        assert_eq!(p.next_message_number(), 6);
    }

    #[test]
    fn recording_fault_does_not_stop_reception() {
        // §152 / §56.1: a full recorder faults but reception continues.
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            raw_recording: 0, // force every enqueue to be rejected
            ..PipelineCapacities::default()
        };
        let mut p = lf_pipeline(cid, caps, true);
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"A\nB\n"));
        // Recorder faulted, but the two messages were still produced.
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Faulted));
        assert_eq!(p.retention().len(), 2);
        assert!(p
            .diagnostics()
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error));
    }

    #[tokio::test]
    async fn run_channel_drains_then_stops_when_sender_drops() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let p = lf_pipeline(cid, PipelineCapacities::default(), false);
        tx.send(bytes_chunk(cid, b"X\nY\n")).await.unwrap();
        drop(tx); // loop drains the buffered chunk, then sees the channel close
        let p = run_channel(rx, p, CancellationToken::new()).await;
        assert_eq!(p.retention().len(), 2);
    }

    #[tokio::test]
    async fn run_channel_stops_on_cancellation_with_live_sender() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<ReceivedData>(8);
        let p = lf_pipeline(cid, PipelineCapacities::default(), false);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_channel(rx, p, cancel.clone()));
        cancel.cancel(); // only cancellation can end the loop — tx is still alive
        let p = handle.await.unwrap();
        drop(tx);
        assert_eq!(p.next_message_number(), 1);
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
