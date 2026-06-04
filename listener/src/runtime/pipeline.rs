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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::matchrule::{FiredRule, MatchRuleSet};
use super::snapshot::TriggeredMatch;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use super::activity::ActivityMeter;
use super::snapshot::{ChannelSnapshot, DiagnosticsSnapshot, DisplayViewSnapshot, SnapshotRequest};
use super::subsample::Subsampler;
use crate::config::{
    DiskGuard, DiskThreshold, LowDiskAction, MatchAction, MatchRule, RecordControl, RecordTarget,
    Subsample,
};

use crate::core::{
    ChannelId, ChunkTime, DisplayViewId, MatchRuleId, Message, MessageBytes, ProtocolMetadata,
    RecordingState, RuntimeEvent,
};
use crate::decode::Decoder;
use crate::diagnostics::{Diagnostic, DiagnosticLog};
use crate::display::{DisplayView, RenderedOutput, Renderer};
use crate::extract::MessageExtractor;
use crate::record::{
    start_raw_recording, FileRotationPolicy, OverwritePolicy, RawFileRecorder, Recording,
    RecordingStopReason, RotatingRawRecorder,
};
use crate::retention::{ByteSized, MessageRetention, RetentionStore};
use crate::transport::{ReceivedData, ReceivedPayload, TransportNotice};

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

/// A subsampled, message-framed data recording (`.ssdat`, §50.1/§53): each passed
/// Message's bytes are written, decimated by `subsampler`. It reuses the raw
/// recording task by feeding it a synthetic per-Message chunk, so no new recorder
/// type is needed — but it taps the *post-extraction* Message fan-out, not the raw
/// chunk stream (raw `.dat` byte data is never subsampled).
struct MessageRecorder {
    recording: Recording<Arc<ReceivedData>>,
    subsampler: Subsampler,
    fault_reported: bool,
}

/// One Display View's runtime state: pause handle, bounded history (§87), an
/// optional Display Recording (§54), and an optional subsampler for its history
/// (§50.1).
struct PipelineDisplayView {
    handle: DisplayViewHandle,
    history: DropOldestQueue<DecodedMessage>,
    recorder: Option<ViewRecorder>,
    subsampler: Subsampler,
}

impl PipelineDisplayView {
    fn new(capacity: usize) -> Self {
        Self {
            handle: DisplayViewHandle::new_active(),
            history: DropOldestQueue::with_capacity(capacity),
            recorder: None,
            subsampler: Subsampler::new(Subsample::None),
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
    /// Subsampled message-framed data recording (`.ssdat`, §50.1); mutually
    /// exclusive with `raw_recorder` (a Raw recording is one or the other).
    message_recorder: Option<MessageRecorder>,
    /// Disk-space guard for recording (§56.2, §168): the policy and the path whose
    /// filesystem free space is polled. `None` = no guard.
    disk_guard: Option<(DiskGuard, PathBuf)>,
    /// Whether the low-disk condition has already been reported (debounce).
    disk_low_reported: bool,
    /// Per-Channel liveness facts (§91.1): rolling throughput + last-data time.
    activity: ActivityMeter,
    events: Option<Sender<RuntimeEvent>>,
    /// Compiled Match Rules (§50.2, §165); empty when none are configured.
    match_rules: MatchRuleSet,
    /// Bounded log of recent rule firings, surfaced in the snapshot (§165).
    recent_matches: DropOldestQueue<TriggeredMatch>,
    /// `Record` actions queued by rule evaluation, applied asynchronously by
    /// [`apply_pending_records`](Self::apply_pending_records) (file I/O is async).
    pending_record_controls: Vec<PendingRecord>,
    /// What a match-armed `Record` recording needs to be built on demand (§50.2,
    /// lazy-create: nothing on disk until a `Begin` fires). `None` = no arming.
    record_arming: Option<RawRecordArming>,
    /// Anchor for the `Idle` condition before any data has arrived (§50.2): idle is
    /// measured from the last data, or from this instant when none has arrived yet.
    created_at: Instant,
}

/// A `Record` action queued for asynchronous application (§50.2).
struct PendingRecord {
    target: RecordTarget,
    control: RecordControl,
}

/// Everything needed to lazily build a match-armed Raw/`.ssdat` recording the
/// first time a `Record { Begin }` fires (§50.2, §165). Mirrors the orchestrator's
/// raw-recorder construction so a match-triggered recording uses the Channel's
/// configured destination, overwrite policy, timestamps, rotation, and subsample.
#[derive(Clone, Debug)]
pub struct RawRecordArming {
    pub destination: PathBuf,
    pub channel_name: String,
    pub overwrite: OverwritePolicy,
    pub timestamps: bool,
    pub file_rotation: FileRotationPolicy,
    pub subsample: Subsample,
    pub capacity: usize,
}

/// Bound on the retained recent-match log (§165) — generous but constant (§124).
const RECENT_MATCHES_CAP: usize = 256;

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
            message_recorder: None,
            disk_guard: None,
            disk_low_reported: false,
            activity: ActivityMeter::new(),
            events: None,
            match_rules: MatchRuleSet::compile(&[]),
            recent_matches: DropOldestQueue::with_capacity(RECENT_MATCHES_CAP),
            pending_record_controls: Vec::new(),
            record_arming: None,
            created_at: Instant::now(),
        }
    }

    /// Attach compiled Match Rules (§50.2, §165). Evaluated per-Message after
    /// decoding and via an idle timer; actions are presentation/control only.
    pub fn with_match_rules(mut self, rules: &[MatchRule]) -> Self {
        self.match_rules = MatchRuleSet::compile(rules);
        self
    }

    /// Arm a match-triggered Raw recording (§50.2): the recorder is created only
    /// when a `Record { Begin }` action fires, so nothing is written before a match.
    pub fn with_record_arming(mut self, arming: RawRecordArming) -> Self {
        self.record_arming = Some(arming);
        self
    }

    /// Attach a Raw Recording handle (§53). The orchestrator creates it at Start
    /// (the file open is async and may fail per §55); the pipeline only feeds and
    /// finalizes it.
    pub fn with_raw_recorder(mut self, recorder: Recording<Arc<ReceivedData>>) -> Self {
        self.raw_recorder = Some(recorder);
        self
    }

    /// Attach a subsampled message-framed data recording (`.ssdat`, §50.1): each
    /// passed Message's bytes are recorded, decimated by `subsample`.
    pub fn with_message_recorder(
        mut self,
        recorder: Recording<Arc<ReceivedData>>,
        subsample: Subsample,
    ) -> Self {
        self.message_recorder = Some(MessageRecorder {
            recording: recorder,
            subsampler: Subsampler::new(subsample),
            fault_reported: false,
        });
        self
    }

    /// Attach a disk-space guard (§56.2, §168): `path`'s filesystem free space is
    /// polled, and on a low condition the guard warns and, per its policy, stops
    /// recording.
    pub fn with_disk_guard(mut self, guard: DiskGuard, path: PathBuf) -> Self {
        self.disk_guard = Some((guard, path));
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

        // Liveness (§91.1): count received bytes at the chunk's arrival time.
        self.activity
            .record_chunk(data.received_at.monotonic, data.payload.bytes().len());
        // Data arrived: re-arm any `Idle` Match Rule so it can fire again on the
        // next quiet episode (§50.2). Cheap no-op when there are no idle rules.
        self.match_rules.note_activity();

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
        // Liveness (§91.1): count completed Messages at their arrival time.
        self.activity
            .record_message(msg.metadata.arrival_timestamp.monotonic);

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

        // Match Rules (§50.2, §165): evaluate the per-Message conditions after
        // decoding and before fan-out, then apply each fired rule's actions
        // (presentation/control only — never touching the Message or its bytes).
        if !self.match_rules.is_empty() {
            let fired = self
                .match_rules
                .evaluate_message(&decoded.message.bytes, decoded.protocol.as_ref());
            if !fired.is_empty() {
                self.apply_fired_rules(fired, Some(number));
            }
        }

        // Display fan-out (§48, §108). For each view:
        //  - Display Recording renders + records the Message regardless of pause
        //    (§58): pausing presentation never pauses recording.
        //  - Presentation history accumulates only while Active (§50) and only for
        //    Messages that pass the view's subsampler (§50.1), dropping oldest on
        //    overflow (§99), without affecting reception/retention/numbering or
        //    other views. The subsampler advances over the full stream, so pausing
        //    does not change which Messages it would pass.
        let at = decoded.message.metadata.arrival_timestamp.monotonic;
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.as_mut() {
                let rendered = rec.renderer.render(&decoded.message);
                rec.recording.try_record(rendered);
            }
            if view.subsampler.should_pass(at) && !view.handle.is_paused() {
                let _ = view.history.push(decoded.clone());
            }
        }
        // Keep the Message (an `Arc`) for the data recorder before retention takes
        // ownership of `decoded`.
        let message = Arc::clone(&decoded.message);
        // Retention: bounded by count and bytes (§88), evicting oldest (§89).
        // Message Numbers are never rewritten, so survivors keep their numbers.
        self.retention.push(decoded);
        // Subsampled data recording (`.ssdat`, §50.1/§53): write passed Messages'
        // bytes, decimated, by feeding the raw recording task a synthetic
        // per-Message chunk. Non-blocking and fault-on-overflow like raw (§56.1).
        let mut message_recording_faulted = false;
        if let Some(mr) = self.message_recorder.as_mut() {
            if mr.recording.state() == RecordingState::Enabled && mr.subsampler.should_pass(at) {
                let chunk = Arc::new(ReceivedData {
                    channel_id: self.channel_id,
                    payload: ReceivedPayload::Bytes(message.bytes.to_vec()),
                    received_at: ChunkTime {
                        monotonic: message.metadata.arrival_timestamp.monotonic,
                        wall_clock: message.metadata.arrival_timestamp.wall_clock,
                    },
                });
                mr.recording.try_record(chunk);
                if mr.recording.state() == RecordingState::Faulted && !mr.fault_reported {
                    mr.fault_reported = true;
                    message_recording_faulted = true;
                }
            }
        }
        if message_recording_faulted {
            self.diagnostics.record(Diagnostic::error(format!(
                "subsampled data recording faulted on channel {}",
                self.channel_id
            )));
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingFaulted(self.channel_id));
            }
        }

        // Event stream (§137): advisory, non-blocking, drop on full.
        if let Some(events) = &self.events {
            let _ = events.try_send(RuntimeEvent::MessageReceived(self.channel_id, number));
        }
    }

    /// Apply the actions of every rule that fired (§50.2). Synchronous actions —
    /// `Notify`, `Mark`, `PauseDisplay`, `Highlight` — take effect immediately;
    /// `Record` actions are queued for asynchronous application (file I/O). Every
    /// firing is observable: it is logged for the snapshot and emits a
    /// `MatchTriggered` event (§137). `message_number` is `None` for an idle firing.
    fn apply_fired_rules(&mut self, fired: Vec<FiredRule>, message_number: Option<u64>) {
        for rule in fired {
            self.recent_matches.push(TriggeredMatch {
                rule_id: rule.id,
                message_number,
            });
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::MatchTriggered(self.channel_id, rule.id));
            }
            for action in &rule.actions {
                match action {
                    MatchAction::Notify { severity } => {
                        let where_ = message_number
                            .map(|n| format!(" (message {n})"))
                            .unwrap_or_default();
                        self.diagnostics.record(Diagnostic::new(
                            *severity,
                            format!("match rule fired on channel {}{where_}", self.channel_id),
                        ));
                    }
                    MatchAction::Mark => self.write_mark(rule.id, message_number),
                    MatchAction::PauseDisplay { view } => self.pause_views(*view),
                    // Highlight is presentation-only; the firing is recorded above
                    // (`recent_matches`) and the UI applies the style. Nothing to do
                    // headless, and it never touches the Message (§50.2).
                    MatchAction::Highlight { .. } => {}
                    MatchAction::Record { target, control } => {
                        self.pending_record_controls.push(PendingRecord {
                            target: *target,
                            control: *control,
                        });
                    }
                }
            }
        }
    }

    /// `Mark` action (§50.2): drop a correlation marker into every Display View's
    /// Display Recording (`.disp`) — **never** the raw `.dat` stream, which stays
    /// byte-exact (§5.6/§49). The `MatchTriggered` event and `recent_matches` log
    /// (written by the caller) are the marker's display/event surfaces.
    fn write_mark(&mut self, rule_id: MatchRuleId, message_number: Option<u64>) {
        let suffix = message_number
            .map(|n| format!(" msg={n}"))
            .unwrap_or_default();
        let text = format!("\u{2039}MARK rule={rule_id}{suffix}\u{203a}");
        let channel_id = self.channel_id;
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.as_mut() {
                rec.recording.try_record(RenderedOutput {
                    channel_id,
                    message_number,
                    text: text.clone(),
                    timestamp: None,
                });
            }
        }
    }

    /// `PauseDisplay` action (§50.2, §50): freeze one Display View by index, or all
    /// views when `None`. Reception and recording continue (§58).
    fn pause_views(&mut self, view: Option<usize>) {
        match view {
            Some(idx) => {
                if let Some(v) = self.display_views.get(idx) {
                    v.handle.pause();
                }
            }
            None => {
                for v in &self.display_views {
                    v.handle.pause();
                }
            }
        }
    }

    /// Idle Match Rules (§50.2): fire any whose quiet-time has reached its timeout.
    /// `now` anchors "time since last data" (or since channel start before any data
    /// arrives). A no-op when no idle rule is configured.
    pub fn evaluate_idle_rules(&mut self, now: Instant) {
        if !self.match_rules.has_idle_rule() {
            return;
        }
        let last = self
            .activity
            .snapshot(now)
            .last_data_at
            .unwrap_or(self.created_at);
        let idle_for = now.saturating_duration_since(last);
        let fired = self.match_rules.evaluate_idle(idle_for);
        if !fired.is_empty() {
            self.apply_fired_rules(fired, None);
        }
    }

    /// Apply queued `Record` actions (§50.2). Called from the async ingest loop,
    /// since recorder creation/finalization is async. Raw/`Both` targets are
    /// honoured by the byte-exact (or `.ssdat`) recorder; the display portion of
    /// `Display`/`Both` is deferred (it needs per-view display-recorder arming).
    pub async fn apply_pending_records(&mut self) {
        if self.pending_record_controls.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_record_controls);
        for req in pending {
            let raw_targeted = matches!(req.target, RecordTarget::Raw | RecordTarget::Both);
            if !raw_targeted {
                continue; // Display-only target: deferred (no raw recorder to drive)
            }
            match req.control {
                RecordControl::Begin => self.begin_armed_recording().await,
                RecordControl::Stop => self.stop_armed_recording().await,
            }
        }
    }

    /// Lazily create the match-armed Raw/`.ssdat` recording on the first `Begin`
    /// (§50.2): nothing is on disk until now. A no-op if a recording is already
    /// active or no arming is configured; an open failure warns without faulting
    /// the Channel (§55).
    async fn begin_armed_recording(&mut self) {
        if self.raw_recorder.is_some() || self.message_recorder.is_some() {
            return; // already recording — `Begin` is idempotent
        }
        let Some(arm) = self.record_arming.clone() else {
            return; // no destination armed — cannot record
        };
        let subsampled = arm.subsample != Subsample::None;
        let ext = if subsampled { ".ssdat" } else { ".dat" };
        let created = if arm.file_rotation == FileRotationPolicy::None {
            RawFileRecorder::create(&arm.destination, arm.overwrite, arm.timestamps)
                .await
                .map(|r| start_raw_recording(r, arm.capacity))
        } else {
            RotatingRawRecorder::create(
                &arm.destination,
                &arm.channel_name,
                ext,
                arm.overwrite,
                arm.timestamps,
                arm.file_rotation,
            )
            .await
            .map(|r| start_raw_recording(r, arm.capacity))
        };
        match created {
            Ok(rec) if subsampled => {
                self.message_recorder = Some(MessageRecorder {
                    recording: rec,
                    subsampler: Subsampler::new(arm.subsample),
                    fault_reported: false,
                });
            }
            Ok(rec) => {
                self.raw_recorder = Some(rec);
                self.recording_fault_reported = false;
            }
            Err(_err) => {
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::WarningRaised(self.channel_id));
                }
            }
        }
    }

    /// Stop and finalize the match-armed recording on a `Record { Stop }` (§50.2,
    /// §56): a clean finalize, reception continues. A no-op if none is active.
    async fn stop_armed_recording(&mut self) {
        if let Some(recorder) = self.raw_recorder.take() {
            recorder.finalize(RecordingStopReason::Disabled).await;
        }
        if let Some(mr) = self.message_recorder.take() {
            mr.recording.finalize(RecordingStopReason::Disabled).await;
        }
    }

    /// Record a non-terminal transport notice (§95, §101; ADR-007). The transport
    /// states *what happened*; the pipeline — the channel's `DiagnosticLog` owner —
    /// decides how it is recorded and reported, keeping the §95 diagnostic and the
    /// §137 event paired in one place.
    pub fn record_notice(&mut self, notice: TransportNotice) {
        match notice {
            TransportNotice::ReceptionStalled {
                channel_id,
                stalled_for,
            } => {
                self.diagnostics.record(Diagnostic::warning(format!(
                    "reception stalled {} ms on channel {channel_id}; possible transport-specific \
                     loss (UART/driver overrun) — lost byte count is not observable (§101)",
                    stalled_for.as_millis(),
                )));
                // The matching event (§137); advisory, non-blocking like the rest.
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::WarningRaised(channel_id));
                }
            }
        }
    }

    /// Poll the recording filesystem's free space and act on a low condition
    /// (§56.2, §168). Called periodically (not per write). Warns once per low
    /// episode and, if the policy is `StopRecording`, finalizes and stops the
    /// recordings while reception continues (§96). A failed space query is ignored.
    pub async fn check_disk_guard(&mut self) {
        let Some((guard, path)) = self.disk_guard.clone() else {
            return;
        };
        // Only meaningful while a recording is active.
        if self.raw_recorder.is_none()
            && self.message_recorder.is_none()
            && !self.display_views.iter().any(|v| v.recorder.is_some())
        {
            return;
        }
        let (Ok(free), Ok(total)) = (fs2::available_space(&path), fs2::total_space(&path)) else {
            return; // cannot determine free space; do not act
        };
        if !disk_is_low(free, total, guard.min_free) {
            self.disk_low_reported = false;
            return;
        }
        if self.disk_low_reported {
            return; // already reported this episode
        }
        self.disk_low_reported = true;
        self.diagnostics.record(Diagnostic::warning(format!(
            "low disk for recording on channel {}: {free} bytes free",
            self.channel_id
        )));
        if let Some(events) = &self.events {
            let _ = events.try_send(RuntimeEvent::DiskSpaceLow(self.channel_id));
        }
        if guard.on_low == LowDiskAction::StopRecording {
            self.stop_all_recording().await;
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingStoppedLowDisk(self.channel_id));
            }
        }
    }

    /// Finalize and drop every recording on this Channel (§56). Reception,
    /// extraction, display, and retention are unaffected (§96).
    async fn stop_all_recording(&mut self) {
        if let Some(recorder) = self.raw_recorder.take() {
            recorder.finalize(RecordingStopReason::Disabled).await;
        }
        if let Some(mr) = self.message_recorder.take() {
            mr.recording.finalize(RecordingStopReason::Disabled).await;
        }
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.take() {
                rec.recording.finalize(RecordingStopReason::Disabled).await;
            }
        }
    }

    /// Called at Channel stop (§110, §112): discard any partial Message (§112)
    /// and finalize the recorder — flush and close (§56).
    pub async fn finish(&mut self) {
        let _ = self.extractor.finish();
        if let Some(recorder) = self.raw_recorder.take() {
            recorder.finalize(RecordingStopReason::ChannelStopped).await;
        }
        if let Some(mr) = self.message_recorder.take() {
            mr.recording
                .finalize(RecordingStopReason::ChannelStopped)
                .await;
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

    /// Set each Display View's subsampling policy (§50.1), in creation order.
    /// Views without a matching policy keep `Subsample::None`.
    pub fn set_view_subsamples(&mut self, policies: &[Subsample]) {
        for (view, &policy) in self.display_views.iter_mut().zip(policies) {
            view.subsampler = Subsampler::new(policy);
        }
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

    /// Current data-recording state — raw `.dat` or subsampled `.ssdat` (the two
    /// are mutually exclusive) — or `None` if neither is attached.
    pub fn raw_recording_state(&self) -> Option<RecordingState> {
        self.raw_recorder
            .as_ref()
            .map(|r| r.state())
            .or_else(|| self.message_recorder.as_ref().map(|m| m.recording.state()))
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
            raw_recording: self.raw_recording_state(),
            activity: self.activity.snapshot(Instant::now()),
            matches: self.recent_matches.iter().copied().collect(),
        }
    }
}

/// Whether `free` bytes is below the disk-guard threshold (§168).
fn disk_is_low(free: u64, total: u64, threshold: DiskThreshold) -> bool {
    match threshold {
        DiskThreshold::Bytes { bytes } => free < bytes,
        DiskThreshold::Percent { percent } => {
            total > 0 && (free as u128) * 100 < (total as u128) * (percent as u128)
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
/// Between reads it also serves snapshot requests (§137, ADR-006) and records
/// transport notices (§95, §101, ADR-007): a requester sends a oneshot reply on
/// `snapshots` and the loop answers from current state; a transport sends a
/// `TransportNotice` on `notices` and the loop records it as a diagnostic. Both
/// are checked ahead of reads (they are rare and cheap) so they are serviced
/// promptly; a closed `snapshots`/`notices` channel simply stops being polled.
pub async fn run_channel(
    mut ingest: Receiver<ReceivedData>,
    mut snapshots: Receiver<SnapshotRequest>,
    mut notices: Receiver<TransportNotice>,
    mut pipeline: ChannelPipeline,
    cancel: CancellationToken,
) -> ChannelPipeline {
    let mut snapshots_open = true;
    let mut notices_open = true;
    // Periodic disk-space guard poll (§56.2, §168) — not per write. Cheap when no
    // guard is configured (the check returns immediately).
    let mut disk_check = tokio::time::interval(Duration::from_secs(5));
    disk_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Idle Match Rule timer (§50.2): a sub-second tick so an `Idle` condition fires
    // promptly once the stream goes quiet. Cheap no-op when no idle rule exists.
    let mut idle_check = tokio::time::interval(Duration::from_millis(250));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
            notice = notices.recv(), if notices_open => match notice {
                Some(notice) => pipeline.record_notice(notice),
                None => notices_open = false, // transport gone; keep running
            },
            _ = disk_check.tick() => pipeline.check_disk_guard().await,
            _ = idle_check.tick() => {
                pipeline.evaluate_idle_rules(Instant::now());
                pipeline.apply_pending_records().await;
            }
            maybe = ingest.recv() => match maybe {
                Some(data) => {
                    pipeline.ingest(data);
                    // A per-Message `Record` action may have been queued (§50.2).
                    pipeline.apply_pending_records().await;
                }
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
    use crate::config::MatchCondition;
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
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(4);
        tx.send(bytes_chunk(cid, b"X\nY\n")).await.unwrap();
        drop(tx); // loop drains the buffered chunk, then sees the channel close
        let p = run_channel(rx, snap_rx, notice_rx, p, CancellationToken::new()).await;
        assert_eq!(p.retention().len(), 2);
    }

    #[tokio::test]
    async fn run_channel_stops_on_cancellation_with_live_sender() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<ReceivedData>(8);
        let p = lf_pipeline(cid, PipelineCapacities::default());
        let (_snap_tx, snap_rx) = tokio::sync::mpsc::channel(4);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(4);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_channel(rx, snap_rx, notice_rx, p, cancel.clone()));
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
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(4);
        let p = lf_pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_channel(rx, snap_rx, notice_rx, p, cancel.clone()));

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
    async fn run_channel_records_a_transport_notice_as_a_diagnostic() {
        // ADR-007 seam: a transport notice becomes a retained warning diagnostic
        // plus a WarningRaised event, both observable without stopping the channel.
        let cid = ChannelId::new();
        let (_tx, rx) = tokio::sync::mpsc::channel(8);
        let (snap_tx, snap_rx) = tokio::sync::mpsc::channel(4);
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel(4);
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel(8);
        let p = lf_pipeline(cid, PipelineCapacities::default()).with_event_sender(ev_tx);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_channel(rx, snap_rx, notice_rx, p, cancel.clone()));

        notice_tx
            .send(TransportNotice::ReceptionStalled {
                channel_id: cid,
                stalled_for: std::time::Duration::from_millis(500),
            })
            .await
            .unwrap();

        // The matching event surfaces (waiting on it also means record_notice ran).
        assert_eq!(
            ev_rx.recv().await.unwrap(),
            RuntimeEvent::WarningRaised(cid)
        );

        // A live snapshot shows the retained warning naming the stall duration.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        snap_tx.send(reply_tx).await.unwrap();
        let snap = reply_rx.await.unwrap();
        assert_eq!(snap.diagnostics.warnings.len(), 1);
        assert!(snap.diagnostics.warnings[0]
            .message
            .contains("reception stalled 500 ms"));

        cancel.cancel();
        let _ = handle.await.unwrap();
    }

    #[test]
    fn disk_is_low_compares_bytes_and_percent_thresholds() {
        // Bytes: low strictly below the floor.
        let bytes = DiskThreshold::Bytes { bytes: 1_000 };
        assert!(disk_is_low(999, 10_000, bytes));
        assert!(!disk_is_low(1_000, 10_000, bytes));
        // Percent: low when free is below percent% of total.
        let pct = DiskThreshold::Percent { percent: 10 };
        assert!(disk_is_low(999, 10_000, pct)); // 9.99% < 10%
        assert!(!disk_is_low(1_000, 10_000, pct)); // exactly 10% is not low
                                                   // A zero/unknown total never reads as low (avoids divide-by-zero panics).
        assert!(!disk_is_low(0, 0, pct));
    }

    #[tokio::test]
    async fn disk_guard_stops_recording_once_and_emits_events() {
        use crate::record::{start_raw_recording, RawFileRecorder};
        use std::sync::atomic::{AtomicU64, Ordering};

        // Unique temp file (coarse Windows clock → use a process-static counter).
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "listener-diskguard-{}-{}.dat",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));

        let cid = ChannelId::new();
        let recorder =
            RawFileRecorder::create(&path, crate::record::OverwritePolicy::Overwrite, false)
                .await
                .unwrap();
        let recording = start_raw_recording(recorder, 16);

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel(8);
        // min_free = u64::MAX forces a permanent low-disk condition; StopRecording.
        let guard = DiskGuard {
            min_free: DiskThreshold::Bytes { bytes: u64::MAX },
            on_low: LowDiskAction::StopRecording,
        };
        let mut p = lf_pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(recording)
            .with_event_sender(ev_tx)
            .with_disk_guard(guard, std::env::temp_dir());
        assert!(p.raw_recorder.is_some());

        p.check_disk_guard().await;

        // The guard warned, stopped the recording, and announced both (§168).
        assert!(p.raw_recorder.is_none(), "recording stopped on low disk");
        assert_eq!(ev_rx.recv().await.unwrap(), RuntimeEvent::DiskSpaceLow(cid));
        assert_eq!(
            ev_rx.recv().await.unwrap(),
            RuntimeEvent::RecordingStoppedLowDisk(cid)
        );

        // Idempotent within one low episode: a second poll re-emits nothing.
        p.check_disk_guard().await;
        assert!(ev_rx.try_recv().is_err());

        let _ = std::fs::remove_file(&path);
    }

    // --- Match Rules & Triggers (§50.2, §165) ---

    fn byte_rule(name: &str, pattern: &[u8], actions: Vec<MatchAction>) -> MatchRule {
        MatchRule {
            name: name.to_string(),
            condition: MatchCondition::BytePattern {
                pattern: pattern.to_vec(),
            },
            actions,
            enabled: true,
        }
    }

    #[test]
    fn notify_rule_records_a_diagnostic_logs_the_firing_and_emits_an_event() {
        use crate::diagnostics::DiagnosticSeverity;
        let cid = ChannelId::new();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel(8);
        let rule = byte_rule(
            "alarm",
            b"ALARM",
            vec![MatchAction::Notify {
                severity: DiagnosticSeverity::Warning,
            }],
        );
        let mut p = lf_pipeline(cid, PipelineCapacities::default())
            .with_event_sender(ev_tx)
            .with_match_rules(&[rule]);

        // A non-matching Message fires nothing.
        p.ingest(bytes_chunk(cid, b"normal\n"));
        assert_eq!(p.snapshot().matches.len(), 0);

        // A matching Message: diagnostic recorded, firing logged, event emitted.
        p.ingest(bytes_chunk(cid, b"ALARM\n"));
        assert_eq!(p.diagnostics().warnings().count(), 1);
        let snap = p.snapshot();
        assert_eq!(snap.matches.len(), 1);
        assert_eq!(snap.matches[0].message_number, Some(2));

        // The MessageReceived (#1, #2) and a MatchTriggered are on the stream.
        let mut saw_match = false;
        while let Ok(ev) = ev_rx.try_recv() {
            if let RuntimeEvent::MatchTriggered(c, r) = ev {
                assert_eq!(c, cid);
                assert_eq!(r, snap.matches[0].rule_id);
                saw_match = true;
            }
        }
        assert!(saw_match, "a MatchTriggered event should have been emitted");
    }

    #[test]
    fn pause_display_rule_freezes_the_targeted_view() {
        let cid = ChannelId::new();
        let rule = byte_rule(
            "freeze",
            b"STOP",
            vec![MatchAction::PauseDisplay { view: Some(0) }],
        );
        let mut p = lf_pipeline(cid, PipelineCapacities::default()).with_match_rules(&[rule]);
        assert!(!p.snapshot().display_views[0].paused);

        p.ingest(bytes_chunk(cid, b"STOP\n"));
        assert!(
            p.snapshot().display_views[0].paused,
            "the matching Message should pause the view (§50.2)"
        );
    }

    #[test]
    fn idle_rule_fires_once_then_rearms_after_data() {
        let cid = ChannelId::new();
        let rule = MatchRule {
            name: "quiet".to_string(),
            condition: MatchCondition::Idle { timeout_ms: 0 },
            actions: vec![MatchAction::Mark],
            enabled: true,
        };
        let mut p = lf_pipeline(cid, PipelineCapacities::default()).with_match_rules(&[rule]);

        // Quiet since start (timeout 0): fires once, then latches.
        p.evaluate_idle_rules(Instant::now());
        assert_eq!(p.snapshot().matches.len(), 1);
        p.evaluate_idle_rules(Instant::now());
        assert_eq!(p.snapshot().matches.len(), 1, "idle latches — fires once");

        // Data arrives (re-arms), then quiet again: it can fire a second time.
        p.ingest(bytes_chunk(cid, b"x\n"));
        p.evaluate_idle_rules(Instant::now());
        assert_eq!(p.snapshot().matches.len(), 2);
    }

    #[tokio::test]
    async fn record_action_begins_and_stops_from_the_match_forward() {
        let path = temp_path("match-record");
        let cid = ChannelId::new();
        let arming = RawRecordArming {
            destination: path.clone(),
            channel_name: "t".to_string(),
            overwrite: OverwritePolicy::Overwrite,
            timestamps: false,
            file_rotation: FileRotationPolicy::None,
            subsample: Subsample::None,
            capacity: 16,
        };
        let begin = byte_rule(
            "go",
            b"GO",
            vec![MatchAction::Record {
                target: RecordTarget::Raw,
                control: RecordControl::Begin,
            }],
        );
        let stop = byte_rule(
            "stop",
            b"STOP",
            vec![MatchAction::Record {
                target: RecordTarget::Raw,
                control: RecordControl::Stop,
            }],
        );
        let mut p = lf_pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[begin, stop])
            .with_record_arming(arming);

        // Lazy-create: nothing on disk before any match (§50.2 — no pre-match data).
        assert!(!path.exists());

        // Pre-match data is not recorded and creates no file.
        p.ingest(bytes_chunk(cid, b"before\n"));
        p.apply_pending_records().await;
        assert!(!path.exists());

        // The match itself arms recording; its own chunk was tapped before the
        // recorder existed, so capture starts from the next chunk forward.
        p.ingest(bytes_chunk(cid, b"GO\n"));
        p.apply_pending_records().await;
        p.ingest(bytes_chunk(cid, b"DATA\n"));
        p.apply_pending_records().await;

        // The stop match finalizes; data after it is not recorded.
        p.ingest(bytes_chunk(cid, b"STOP\n"));
        p.apply_pending_records().await;
        p.ingest(bytes_chunk(cid, b"after\n"));
        p.apply_pending_records().await;
        p.finish().await;

        let contents = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        assert!(contents.contains("DATA"), "data after Begin is captured");
        assert!(!contents.contains("before"), "no pre-match backfill (§158)");
        assert!(
            !contents.contains("GO"),
            "the arming Message itself is not retro-captured"
        );
        assert!(
            !contents.contains("after"),
            "nothing after Stop is captured"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mark_action_annotates_the_display_recording_not_the_raw_stream() {
        use crate::display::DisplayView;
        use crate::record::{
            start_display_recording, start_raw_recording, DisplayFileRecorder, RawFileRecorder,
        };

        let disp_path = temp_path("mark-disp");
        let raw_path = temp_path("mark-raw");
        let cid = ChannelId::new();

        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();
        let raw = RawFileRecorder::create(&raw_path, OverwritePolicy::Overwrite, false)
            .await
            .unwrap();

        let rule = byte_rule("mark", b"HIT", vec![MatchAction::Mark]);
        let mut p = lf_pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(start_raw_recording(raw, 16))
            .with_match_rules(&[rule]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 16));

        p.ingest(bytes_chunk(cid, b"HIT\n"));
        p.finish().await;

        // The marker lands in the Display Recording (.disp)…
        let disp_text = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert!(
            disp_text.contains("MARK"),
            "mark annotates the .disp stream"
        );
        // …but the raw .dat stays byte-exact — no marker bytes injected (§5.6/§49).
        let raw_bytes = tokio::fs::read(&raw_path).await.unwrap();
        assert_eq!(raw_bytes, b"HIT\n", "raw .dat is untouched by Mark");

        let _ = tokio::fs::remove_file(&disp_path).await;
        let _ = tokio::fs::remove_file(&raw_path).await;
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
