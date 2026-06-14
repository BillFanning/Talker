//! Per-Channel stream pipeline (spec §102, §99.1, §108).
//!
//! One stage: each received chunk fans out, non-blocking, to the raw-recording
//! tap (§53), the stream scrollback (§87), per-view display recording (§54),
//! find/trigger evaluation (§50.2), and diagnostics. There is no extraction,
//! metadata, or decoding stage (ADR-010) — the bytes are never reframed.
//!
//! Acquisition priority (§5.9, §100): every edge here is non-blocking — the
//! scrollback ring drops oldest, a recorder faults on overflow. The only edge
//! permitted to stall the reader is the Transport→Pipeline channel (§97.1),
//! the bounded `tokio::sync::mpsc` that feeds [`run_channel`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::matchrule::{FiredRule, MatchRuleSet};
use super::snapshot::TriggeredMatch;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use super::activity::ActivityMeter;
use super::snapshot::{
    ChannelSnapshot, ChannelStats, DiagnosticsSnapshot, DisplayViewSnapshot, PipelineRequest,
    StreamDelta,
};
use crate::config::{
    DiskGuard, DiskThreshold, LowDiskAction, MatchAction, MatchRule, RecordControl, RecordTarget,
};

use crate::core::{ChannelId, DisplayViewId, MatchRuleId, RecordingState, RuntimeEvent};
use crate::diagnostics::{Diagnostic, DiagnosticLog};
use crate::display::{DisplayView, RenderedOutput};
use crate::record::{
    start_raw_recording, FileRotationPolicy, OverwritePolicy, RawFileRecorder, Recording,
    RecordingStopReason, RotatingRawRecorder,
};
use crate::transport::{ReceivedData, TransportNotice};

use super::queue::DropOldestQueue;

/// Bounded capacities for a Channel's fan-out edges (§99, §124).
#[derive(Clone, Copy, Debug)]
pub struct PipelineCapacities {
    /// The bounded Transport→Pipeline queue — the only edge that may stall the
    /// reader (§97.1, §99).
    pub ingest: usize,
    /// Stream scrollback (§87): how many of the most recent received bytes to
    /// keep for display. Byte-capped — there are no Message boundaries (§18).
    pub stream_display: usize,
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
            stream_display: 128 * 1024,
            raw_recording: 1024,
            event_retention: None,
            warning_retention: None,
            error_retention: None,
            events: 256,
        }
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

    /// Pause this view: it stops accumulating new stream bytes. Reception,
    /// recording, and *other* views are unaffected (§50).
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

/// One Display View's runtime state: pause handle and an optional Display
/// Recording (§54). The displayed content itself is the shared stream
/// scrollback (§87) rendered per view; views hold no per-view history.
struct PipelineDisplayView {
    handle: DisplayViewHandle,
    recorder: Option<ViewRecorder>,
}

impl PipelineDisplayView {
    fn new() -> Self {
        Self {
            handle: DisplayViewHandle::new_active(),
            recorder: None,
        }
    }
}

/// One Channel's stream pipeline (§102). Driven synchronously via
/// [`ChannelPipeline::ingest`]; [`run_channel`] is the async loop around it.
pub struct ChannelPipeline {
    channel_id: ChannelId,
    /// Display Views (§48): a Channel may have several, each independently
    /// paused (§11). The first is the default view created by `new`.
    display_views: Vec<PipelineDisplayView>,
    /// Retained diagnostics, count-limited per type (§88).
    diagnostics: DiagnosticLog,
    /// Raw Recording handle (§53). `None` when recording is disabled or its
    /// enable failed (§55). Faults non-blockingly on overflow (§56.1).
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
    /// Whether the raw recorder's fault has already been reported.
    recording_fault_reported: bool,
    /// Disk-space guard for recording (§56.2, §168): the policy and the path whose
    /// filesystem free space is polled. `None` = no guard.
    disk_guard: Option<(DiskGuard, PathBuf)>,
    /// Whether the low-disk condition has already been reported (debounce).
    disk_low_reported: bool,
    /// Per-Channel liveness facts (§91.1): rolling throughput + last-data time.
    activity: ActivityMeter,
    events: Option<Sender<RuntimeEvent>>,
    /// Compiled find/trigger rules (§50.2, §165); empty when none are configured.
    match_rules: MatchRuleSet,
    /// Bounded log of recent rule firings, surfaced in the snapshot (§165).
    recent_matches: DropOldestQueue<TriggeredMatch>,
    /// `Record` actions queued by rule evaluation, applied asynchronously by
    /// [`apply_pending_records`](Self::apply_pending_records) (file I/O is async).
    pending_record_controls: Vec<PendingRecord>,
    /// The Raw recording settings used to build the recording on demand (§50.2,
    /// lazy-create: nothing on disk until a `Begin` fires). `None` = no destination
    /// set, so a `Begin` can't record.
    recording_settings: Option<RawRecordingSettings>,
    /// Anchor for the `Idle` condition before any data has arrived (§50.2): idle is
    /// measured from the last data, or from this instant when none has arrived yet.
    created_at: Instant,
    /// The stream scrollback (§87): the most recent received bytes, verbatim.
    /// Trimmed from the front to `stream_cap` bytes.
    stream_buf: VecDeque<u8>,
    stream_cap: usize,
    /// Total bytes evicted from the front of `stream_buf` since Start. The absolute
    /// stream offset of `stream_buf[0]` is exactly this value, so a consumer holding
    /// an absolute cursor can ask for "bytes since N" and we can locate N in the ring
    /// (or tell it its cursor was evicted). `stream_dropped + stream_buf.len()` is the
    /// absolute end offset.
    stream_dropped: u64,
}

/// A `Record` action queued for asynchronous application (§50.2).
struct PendingRecord {
    target: RecordTarget,
    control: RecordControl,
}

/// The Raw recording settings needed to build the recording when a `Record { Begin }`
/// fires (§50.2, §165) — the destination/overwrite/timestamps/rotation, packaged so
/// they can be sent to the pipeline task (which runs on its own async task and can't
/// read the config directly). Built from the channel's recording config, either at
/// channel start or — for a live toggle — read from the editor at click time (ADR-012).
#[derive(Clone, Debug)]
pub struct RawRecordingSettings {
    pub destination: PathBuf,
    pub channel_name: String,
    pub overwrite: OverwritePolicy,
    pub timestamps: bool,
    pub file_rotation: FileRotationPolicy,
    pub capacity: usize,
}

/// Bound on the retained recent-match log (§165) — generous but constant (§124).
const RECENT_MATCHES_CAP: usize = 256;

impl ChannelPipeline {
    pub fn new(channel_id: ChannelId, caps: PipelineCapacities) -> Self {
        Self {
            channel_id,
            display_views: vec![PipelineDisplayView::new()],
            diagnostics: DiagnosticLog::new(
                caps.event_retention,
                caps.warning_retention,
                caps.error_retention,
            ),
            raw_recorder: None,
            recording_fault_reported: false,
            disk_guard: None,
            disk_low_reported: false,
            activity: ActivityMeter::new(),
            events: None,
            match_rules: MatchRuleSet::compile(&[]),
            recent_matches: DropOldestQueue::with_capacity(RECENT_MATCHES_CAP),
            pending_record_controls: Vec::new(),
            recording_settings: None,
            created_at: Instant::now(),
            stream_buf: VecDeque::new(),
            stream_cap: caps.stream_display,
            stream_dropped: 0,
        }
    }

    /// Attach compiled find/trigger rules (§50.2, §165). `BytePattern` rules are
    /// evaluated per chunk; `Idle` via a timer. Actions are presentation/control only.
    pub fn with_match_rules(mut self, rules: &[MatchRule]) -> Self {
        self.match_rules = MatchRuleSet::compile(rules);
        self
    }

    /// Provide the Raw recording settings (§50.2): the recorder is created only when a
    /// `Record { Begin }` action fires, so nothing is written before then.
    pub fn with_recording_settings(mut self, settings: RawRecordingSettings) -> Self {
        self.recording_settings = Some(settings);
        self
    }

    /// Attach a Raw Recording handle (§53). The orchestrator creates it at Start
    /// (the file open is async and may fail per §55); the pipeline only feeds and
    /// finalizes it.
    pub fn with_raw_recorder(mut self, recorder: Recording<Arc<ReceivedData>>) -> Self {
        self.raw_recorder = Some(recorder);
        self
    }

    /// Attach a disk-space guard (§56.2, §168): `path`'s filesystem free space is
    /// polled, and on a low condition the guard warns and, per its policy, stops
    /// recording.
    pub fn with_disk_guard(mut self, guard: DiskGuard, path: PathBuf) -> Self {
        self.disk_guard = Some((guard, path));
        self
    }

    /// Attach a runtime event sender (§137). Events are advisory, so emission is
    /// non-blocking and drops on a full event channel — the authoritative record
    /// lives in recording/diagnostics, not the event stream.
    pub fn with_event_sender(mut self, events: Sender<RuntimeEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// Process one received chunk through the pipeline (§102).
    ///
    /// Distribution order follows §99.1: the raw recorder is offered the chunk
    /// first with a non-blocking enqueue, then the stream scrollback, display
    /// recording, and find/trigger evaluation. Every edge is non-blocking.
    pub fn ingest(&mut self, data: ReceivedData) {
        let data = Arc::new(data);
        // The chunk's start offset in the stream (total bytes before it) — the
        // anchor for find/trigger firings (§50.2: byte offsets, not numbers).
        let chunk_offset = self.activity.total_bytes();

        // Liveness (§91.1): count received bytes at the chunk's arrival time.
        self.activity
            .record_chunk(data.received_at.monotonic, data.payload.bytes().len());
        // Data arrived: re-arm any `Idle` rule so it can fire again on the next
        // quiet episode (§50.2). Cheap no-op when there are no idle rules.
        self.match_rules.note_activity();

        // 1. Raw recorder tap (§53). Non-blocking: a full recorder queue faults
        // the recording rather than stalling reception (§56.1). `try_record`
        // updates the handle's state internally.
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

        // 2. Stream scrollback (§87): keep the most recent bytes exactly as
        // received — the wire, regardless of read-chunk boundaries. A byte ring,
        // trimmed from the front. Honors the default view's pause (§50): a paused
        // view freezes its display while reception and recording keep going.
        let bytes = data.payload.bytes();
        if !self
            .display_views
            .first()
            .is_some_and(|v| v.handle.is_paused())
        {
            if bytes.len() >= self.stream_cap {
                // A single chunk already exceeds the cap: keep only its tail. Every
                // currently-buffered byte plus the dropped prefix of this chunk is
                // evicted; account for all of it in the absolute offset.
                let dropped = self.stream_buf.len() as u64 + (bytes.len() - self.stream_cap) as u64;
                self.stream_dropped += dropped;
                self.stream_buf.clear();
                self.stream_buf
                    .extend(bytes[bytes.len() - self.stream_cap..].iter().copied());
            } else {
                self.stream_buf.extend(bytes.iter().copied());
                let overflow = self.stream_buf.len().saturating_sub(self.stream_cap);
                if overflow > 0 {
                    self.stream_buf.drain(..overflow);
                    self.stream_dropped += overflow as u64;
                }
            }
        }

        // 3. Display Recording (§54, §58): render this chunk per recording view
        // and record it — regardless of pause (pausing presentation never pauses
        // recording). Non-blocking; a full queue faults that recording only.
        let channel_id = self.channel_id;
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.as_mut() {
                let rendered = rec.renderer.render_stream(channel_id, bytes);
                rec.recording.try_record(rendered);
            }
        }

        // 4. Find/triggers (§50.2): evaluate `BytePattern` rules against this chunk,
        // matching **across the previous chunk's boundary** via the rule set's
        // carry. Each firing carries its true match start offset (which may fall in
        // the prior chunk for a boundary split) and a flag the measurement uses.
        if !self.match_rules.is_empty() {
            let fired = self.match_rules.evaluate_stream(bytes, chunk_offset);
            if !fired.is_empty() {
                self.apply_fired_rules(fired);
            }
        }
    }

    /// Apply the actions of every rule that fired (§50.2). Synchronous actions —
    /// `Notify`, `Mark`, `PauseDisplay`, `Highlight` — take effect immediately;
    /// `Record` actions are queued for asynchronous application (file I/O). Every
    /// firing is observable: it is logged for the snapshot and emits a
    /// `MatchTriggered` event (§137). Each firing carries its own byte offset
    /// (`None` for an idle firing); a boundary-split firing also records the
    /// where/why measurement diagnostic.
    fn apply_fired_rules(&mut self, fired: Vec<FiredRule>) {
        for rule in fired {
            let byte_offset = rule.match_offset;
            self.recent_matches.push(TriggeredMatch {
                rule_id: rule.id,
                byte_offset,
            });
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::MatchTriggered(self.channel_id, rule.id));
            }
            // Measurement (§50.2): when a match was a cross-chunk boundary split,
            // record *where* (the stream offset) and *why* (the split) so an
            // operator can see that read-chunk boundaries are splitting patterns —
            // and, via `boundary_saves` in the snapshot, *how often*. An info-level
            // diagnostic: it is a recovered match, not a fault.
            if rule.boundary_split {
                let at = byte_offset
                    .map(|n| format!(" at stream offset {n}"))
                    .unwrap_or_default();
                self.diagnostics.record(Diagnostic::event(format!(
                    "match rule {} on channel {} spanned a read-chunk boundary{at} \
                     (recovered by cross-chunk carry)",
                    rule.id, self.channel_id
                )));
            }
            for action in &rule.actions {
                match action {
                    MatchAction::Notify { severity } => {
                        let where_ = byte_offset
                            .map(|n| format!(" (stream offset {n})"))
                            .unwrap_or_default();
                        self.diagnostics.record(Diagnostic::new(
                            *severity,
                            format!("match rule fired on channel {}{where_}", self.channel_id),
                        ));
                    }
                    MatchAction::Mark => self.write_mark(rule.id, byte_offset),
                    MatchAction::PauseDisplay { view } => self.pause_views(*view),
                    // Highlight is presentation-only; the firing is recorded above
                    // (`recent_matches`) and the UI applies the style. Nothing to do
                    // headless, and it never touches the bytes (§50.2).
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
    /// Display Recording (`.disp`) — **never** the raw `.raw` stream, which stays
    /// byte-exact (§5.6/§49). The `MatchTriggered` event and `recent_matches` log
    /// (written by the caller) are the marker's display/event surfaces.
    fn write_mark(&mut self, rule_id: MatchRuleId, byte_offset: Option<u64>) {
        let suffix = byte_offset
            .map(|n| format!(" offset={n}"))
            .unwrap_or_default();
        let text = format!("\u{2039}MARK rule={rule_id}{suffix}\u{203a}");
        let channel_id = self.channel_id;
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.as_mut() {
                rec.recording.try_record(RenderedOutput {
                    channel_id,
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

    /// Idle rules (§50.2): fire any whose quiet-time has reached its timeout.
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
            self.apply_fired_rules(fired);
        }
    }

    /// Apply queued `Record` actions (§50.2). Called from the async ingest loop,
    /// since recorder creation/finalization is async. Raw/`Both` targets are
    /// honoured by the byte-exact recorder; the display portion of `Display`/`Both`
    /// is deferred (it needs per-view display-recorder wiring).
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
                RecordControl::Begin => self.begin_recording().await,
                RecordControl::Stop => self.stop_recording().await,
            }
        }
    }

    /// Begin or stop Raw recording live, without a restart (§50.2, ADR-012). The
    /// manual counterpart of the match-rule `Record` action: it drives the same lazy
    /// begin / clean finalize path, so a user-toggled recording and a rule-triggered
    /// one are byte-identical and share the idempotency rules.
    ///
    /// `settings`, when present, replace the pipeline's recording settings first — so
    /// the live toggle records to whatever the editor showed at click time, with no
    /// restart. They're only swapped in while not actively recording, so a begin can't
    /// change the destination out from under an open file.
    pub async fn set_recording(&mut self, enabled: bool, settings: Option<RawRecordingSettings>) {
        if let Some(settings) = settings {
            if self.raw_recorder.is_none() {
                self.recording_settings = Some(settings);
            }
        }
        if enabled {
            self.begin_recording().await;
        } else {
            self.stop_recording().await;
        }
    }

    /// Lazily create the Raw recording on a `Begin` (§50.2): nothing is on disk until
    /// now. A no-op if a recording is already active; if no destination is set it
    /// reports a fault rather than silently doing nothing; an open failure (e.g. an
    /// existing file under a Refuse policy) reports the reason without faulting the
    /// Channel (§55).
    async fn begin_recording(&mut self) {
        if self.raw_recorder.is_some() {
            return; // already recording — `Begin` is idempotent
        }
        let Some(settings) = self.recording_settings.clone() else {
            // No destination set — a Begin can't record. Surface it instead of a
            // silent no-op (the live toggle would otherwise appear to do nothing).
            self.diagnostics.record(Diagnostic::error(
                "can't begin Raw recording: no destination is set — set one in the Raw \
                 recording setup, then press Record again",
            ));
            if let Some(events) = &self.events {
                let _ = events.try_send(RuntimeEvent::RecordingFaulted(self.channel_id));
            }
            return;
        };
        let created = if settings.file_rotation == FileRotationPolicy::None {
            RawFileRecorder::create(
                &settings.destination,
                settings.overwrite,
                settings.timestamps,
            )
            .await
            .map(|r| start_raw_recording(r, settings.capacity))
        } else {
            RotatingRawRecorder::create(
                &settings.destination,
                &settings.channel_name,
                ".raw",
                settings.overwrite,
                settings.timestamps,
                settings.file_rotation,
            )
            .await
            .map(|r| start_raw_recording(r, settings.capacity))
        };
        match created {
            Ok(rec) => {
                self.raw_recorder = Some(rec);
                self.recording_fault_reported = false;
            }
            Err(err) => {
                // Record *why* the begin failed (e.g. Refuse over an existing file) in
                // the diagnostic log, and signal it as a recording fault so the GUI can
                // surface it — a silent no-op left the user clicking "Record now" with
                // no feedback (§55).
                self.diagnostics.record(Diagnostic::error(format!(
                    "could not begin Raw recording to {}: {err} (check the destination \
                     and the on-exists policy — Refuse will not overwrite)",
                    settings.destination.display(),
                )));
                if let Some(events) = &self.events {
                    let _ = events.try_send(RuntimeEvent::RecordingFaulted(self.channel_id));
                }
            }
        }
    }

    /// Stop and finalize the Raw recording on a `Record { Stop }` (§50.2, §56): a
    /// clean finalize, reception continues. A no-op if none is active.
    async fn stop_recording(&mut self) {
        if let Some(recorder) = self.raw_recorder.take() {
            recorder.finalize(RecordingStopReason::Disabled).await;
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
                // Dedicated `ReceptionStalled` (v1.2) so observers can tell a stall
                // apart from any other warning (ADR-007).
                if let Some(events) = &self.events {
                    let _ =
                        events.try_send(RuntimeEvent::ReceptionStalled(channel_id, stalled_for));
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
        if self.raw_recorder.is_none() && !self.display_views.iter().any(|v| v.recorder.is_some()) {
            return;
        }
        let (Ok(free), Ok(total)) = (fs4::available_space(&path), fs4::total_space(&path)) else {
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
    /// display, and the scrollback are unaffected (§96).
    async fn stop_all_recording(&mut self) {
        if let Some(recorder) = self.raw_recorder.take() {
            recorder.finalize(RecordingStopReason::Disabled).await;
        }
        for view in &mut self.display_views {
            if let Some(rec) = view.recorder.take() {
                rec.recording.finalize(RecordingStopReason::Disabled).await;
            }
        }
    }

    /// Called at Channel stop (§110, §112): finalize the recorders — flush and
    /// close (§56). In-flight bytes already accepted by a recorder are written.
    pub async fn finish(&mut self) {
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

    /// Add a Display View and return its pause handle (§48). The caller keeps
    /// the handle to pause/resume the view across the pipeline-task boundary.
    pub fn add_display_view(&mut self) -> DisplayViewHandle {
        let view = PipelineDisplayView::new();
        let handle = view.handle.clone();
        self.display_views.push(view);
        handle
    }

    /// Attach a Display Recording to the primary (first) Display View (§54),
    /// rendering each chunk with `renderer`. v1 records the primary view;
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

    pub fn diagnostics(&self) -> &DiagnosticLog {
        &self.diagnostics
    }

    /// Current raw-recording state (§53), or `None` if no recorder is attached.
    pub fn raw_recording_state(&self) -> Option<RecordingState> {
        self.raw_recorder.as_ref().map(|r| r.state())
    }

    /// Cheap O(1) counters for a multi-channel overview (§91.1): liveness plus
    /// per-severity diagnostic counts and the boundary-save total. Polled
    /// per-Channel each tick; [`snapshot`](Self::snapshot) (the full diagnostic/match
    /// detail) is reserved for the Channel actually on screen, and the scrollback
    /// bytes come from [`stream_delta`](Self::stream_delta) — neither call clones the
    /// scrollback.
    pub fn stats(&self) -> ChannelStats {
        ChannelStats {
            activity: self.activity.snapshot(Instant::now()),
            event_count: self.diagnostics.events().count(),
            warning_count: self.diagnostics.warnings().count(),
            error_count: self.diagnostics.errors().count(),
            raw_recording: self.raw_recording_state(),
            match_boundary_saves: self.match_rules.boundary_saves(),
        }
    }

    /// Build an owned, point-in-time snapshot of the *small* observable state (§137,
    /// ADR-006): per-view pause state, diagnostics, recent match firings, recording
    /// state, liveness, and the stream's end offset. The scrollback bytes are **not**
    /// included — they are fetched incrementally via [`stream_delta`](Self::stream_delta)
    /// so this stays cheap at high throughput.
    pub fn snapshot(&self) -> ChannelSnapshot {
        ChannelSnapshot {
            channel_id: self.channel_id,
            display_views: self
                .display_views
                .iter()
                .map(|v| DisplayViewSnapshot {
                    id: v.handle.id,
                    paused: v.handle.is_paused(),
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
            match_boundary_saves: self.match_rules.boundary_saves(),
            // The scrollback bytes are fetched incrementally (StreamDelta), not
            // bundled here — only the cursor target travels in the snapshot.
            stream_end_offset: self.stream_dropped + self.stream_buf.len() as u64,
        }
    }

    /// Absolute stream offset just past the last retained byte (§87): total bytes
    /// accepted into the scrollback since Start.
    pub fn stream_end_offset(&self) -> u64 {
        self.stream_dropped + self.stream_buf.len() as u64
    }

    /// Incremental scrollback read (§87): the bytes at or after absolute offset
    /// `since`. Returns only what is new since the consumer's cursor — O(returned
    /// bytes), not O(buffer) — so the runtime ships just the delta and the consumer
    /// renders just the delta.
    ///
    /// If `since` is at/after the end, the delta is empty. If `since` is behind the
    /// retained window (its bytes were evicted), the whole window is returned with
    /// `base_offset > since`, signalling the consumer to reset rather than append.
    pub fn stream_delta(&self, since: u64) -> StreamDelta {
        let start = self.stream_dropped; // absolute offset of stream_buf[0]
        let end = start + self.stream_buf.len() as u64;
        // Clamp the requested cursor into the retained window.
        let from = since.clamp(start, end);
        let skip = (from - start) as usize;
        let bytes: Arc<[u8]> = self.stream_buf.iter().skip(skip).copied().collect();
        StreamDelta {
            base_offset: from,
            bytes,
            end_offset: end,
        }
    }

    /// Test/diagnostic accessor: the full retained scrollback, verbatim (§87). Live
    /// consumers use [`stream_delta`](Self::stream_delta) instead.
    #[cfg(test)]
    fn stream_tail(&self) -> Vec<u8> {
        self.stream_buf.iter().copied().collect()
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
/// Drains the bounded Transport→Pipeline channel until cancelled or the sender
/// is dropped, then returns the pipeline so the caller can finalize/inspect it.
/// Cancellation is cooperative and checked first (`biased`) so shutdown does not
/// depend on draining the queue (§111).
///
/// Between reads it also serves snapshot/stats/stream-delta requests (§137, ADR-006,
/// ADR-011) and records transport notices (§95, §101, ADR-007): a requester sends a
/// oneshot reply on `requests` and the loop answers (the small snapshot, cheap stats,
/// or an incremental stream delta) from current state; a transport sends a
/// `TransportNotice` on `notices` and the loop records it as a diagnostic. Both are checked ahead of reads (they are rare and cheap) so
/// they are serviced promptly; a closed `requests`/`notices` channel simply stops
/// being polled.
pub async fn run_channel(
    mut ingest: Receiver<ReceivedData>,
    mut requests: Receiver<PipelineRequest>,
    mut notices: Receiver<TransportNotice>,
    mut pipeline: ChannelPipeline,
    cancel: CancellationToken,
) -> ChannelPipeline {
    let mut requests_open = true;
    let mut notices_open = true;
    // Periodic disk-space guard poll (§56.2, §168) — not per write. Cheap when no
    // guard is configured (the check returns immediately).
    let mut disk_check = tokio::time::interval(Duration::from_secs(5));
    disk_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Idle rule timer (§50.2): a sub-second tick so an `Idle` condition fires
    // promptly once the stream goes quiet. Cheap no-op when no idle rule exists.
    let mut idle_check = tokio::time::interval(Duration::from_millis(250));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            req = requests.recv(), if requests_open => match req {
                Some(PipelineRequest::Snapshot(tx)) => {
                    let _ = tx.send(pipeline.snapshot());
                }
                Some(PipelineRequest::Stats(tx)) => {
                    let _ = tx.send(pipeline.stats());
                }
                Some(PipelineRequest::StreamDelta { since, reply }) => {
                    let _ = reply.send(pipeline.stream_delta(since));
                }
                Some(PipelineRequest::SetRecording { enabled, settings }) => {
                    pipeline.set_recording(enabled, settings).await;
                }
                None => requests_open = false, // all requesters gone; keep running
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
                    // A `Record` action may have been queued by a rule (§50.2).
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
    use crate::transport::ReceivedPayload;

    fn pipeline(cid: ChannelId, caps: PipelineCapacities) -> ChannelPipeline {
        ChannelPipeline::new(cid, caps)
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
    fn datagram_boundaries_do_not_segment_the_stream() {
        // v2 invariant (§15, §18): UDP datagram boundaries are a reception/recording
        // detail only. The stream concatenates datagram payloads verbatim — nothing
        // is inserted between them and nothing is reframed.
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.ingest(datagram(cid, b"$GPGGA,1*00\r\n"));
        p.ingest(datagram(cid, b"$GPRMC,2*00\r\n"));
        assert_eq!(&p.stream_tail()[..], &b"$GPGGA,1*00\r\n$GPRMC,2*00\r\n"[..]);
        // Mixed chunk kinds (a serial-style Bytes chunk after datagrams) still
        // append verbatim: one stream, regardless of transport read shape.
        p.ingest(bytes_chunk(cid, b"tail"));
        assert_eq!(
            &p.stream_tail()[..],
            &b"$GPGGA,1*00\r\n$GPRMC,2*00\r\ntail"[..]
        );
    }

    #[test]
    fn stream_tail_is_verbatim_across_chunks_and_byte_capped() {
        let cid = ChannelId::new();
        // Small cap so trimming is observable.
        let caps = PipelineCapacities {
            stream_display: 8,
            ..PipelineCapacities::default()
        };
        let mut p = pipeline(cid, caps);
        // Reconstructs the wire across read-chunk boundaries.
        p.ingest(bytes_chunk(cid, b"$ABC"));
        p.ingest(bytes_chunk(cid, b"\r\n"));
        assert_eq!(&p.stream_tail()[..], &b"$ABC\r\n"[..]);
        // Over the cap: keep only the most recent `stream_display` bytes.
        p.ingest(bytes_chunk(cid, b"123456")); // "$ABC\r\n123456" (12) → drop front 4
        assert_eq!(&p.stream_tail()[..], &b"\r\n123456"[..]);
        // A single chunk larger than the cap keeps just its tail.
        p.ingest(bytes_chunk(cid, b"0123456789"));
        assert_eq!(&p.stream_tail()[..], &b"23456789"[..]);
    }

    #[test]
    fn stream_delta_serves_only_new_bytes_since_a_cursor() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.ingest(bytes_chunk(cid, b"hello"));
        // From the start: the whole window.
        let d0 = p.stream_delta(0);
        assert_eq!(d0.base_offset, 0);
        assert_eq!(&*d0.bytes, &b"hello"[..]);
        assert_eq!(d0.end_offset, 5);

        // From the prior end cursor: only the new bytes (no re-ship of "hello").
        p.ingest(bytes_chunk(cid, b"world"));
        let d1 = p.stream_delta(d0.end_offset);
        assert_eq!(d1.base_offset, 5);
        assert_eq!(&*d1.bytes, &b"world"[..]);
        assert_eq!(d1.end_offset, 10);

        // Caught up: an at-end cursor yields nothing.
        let d2 = p.stream_delta(d1.end_offset);
        assert!(d2.bytes.is_empty());
        assert_eq!(d2.base_offset, 10);
        assert_eq!(d2.end_offset, 10);
    }

    #[test]
    fn stream_delta_resets_when_the_cursor_was_evicted() {
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            stream_display: 4,
            ..PipelineCapacities::default()
        };
        let mut p = pipeline(cid, caps);
        p.ingest(bytes_chunk(cid, b"AB")); // offsets 0..2
        let d0 = p.stream_delta(0); // cursor now 2
        assert_eq!(&*d0.bytes, &b"AB"[..]);

        // Push past the cap so offsets 0..2 are evicted (cap 4): buffer holds 4..8.
        p.ingest(bytes_chunk(cid, b"CDEF")); // "ABCDEF" → keep "CDEF", dropped 2
                                             // A stale cursor (2) is behind the window start (2 dropped → start=2);
                                             // here start==2 so it's still valid. Drop more to force a reset.
        p.ingest(bytes_chunk(cid, b"GH")); // "CDEFGH" → keep "EFGH", dropped total 4

        // Cursor 2 is now behind the window start (4): the delta resets to the
        // window with base_offset > since, signalling the consumer to re-seed.
        let d1 = p.stream_delta(d0.end_offset); // since = 2
        assert_eq!(d1.base_offset, 4, "cursor was evicted; base jumps forward");
        assert_eq!(&*d1.bytes, &b"EFGH"[..]);
        assert_eq!(d1.end_offset, 8);
    }

    #[test]
    fn paused_view_freezes_the_stream_tail() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default());
        let view = p.display_view_handles()[0].clone();
        p.ingest(bytes_chunk(cid, b"AB"));
        assert_eq!(&p.stream_tail()[..], &b"AB"[..]);
        // Pause: the displayed stream freezes (§50), but reception still counts
        // the bytes — the liveness counter keeps moving.
        view.pause();
        p.ingest(bytes_chunk(cid, b"CD"));
        assert_eq!(&p.stream_tail()[..], &b"AB"[..]);
        assert_eq!(p.snapshot().activity.total_bytes, 4);
        // Resume: the stream continues from live data (no backfill of the gap).
        view.resume();
        p.ingest(bytes_chunk(cid, b"EF"));
        assert_eq!(&p.stream_tail()[..], &b"ABEF"[..]);
    }

    #[test]
    fn diagnostic_warning_retention_limit_is_applied() {
        // §88: the per-type diagnostic limit bounds retained warnings.
        let cid = ChannelId::new();
        let caps = PipelineCapacities {
            warning_retention: Some(2),
            ..PipelineCapacities::default()
        };
        let mut p = pipeline(cid, caps);
        // Four warnings recorded, capped at two retained.
        for i in 0..4 {
            p.diagnostics.record(Diagnostic::warning(format!("w{i}")));
        }
        assert_eq!(p.diagnostics().warnings().count(), 2);
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "listener-pipeline-{tag}-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[tokio::test]
    async fn raw_recording_captures_received_bytes() {
        // §53: the raw tap records every received chunk byte-exact, in order.
        let cid = ChannelId::new();
        let path = temp_path("raw");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let recording = start_raw_recording(recorder, 64);
        let mut p = pipeline(cid, PipelineCapacities::default()).with_raw_recorder(recording);

        p.ingest(bytes_chunk(cid, b"$GPGLL,1*00\r\n"));
        p.ingest(datagram(cid, b"$GPGLL,2*00\r\n"));
        p.finish().await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"$GPGLL,1*00\r\n$GPGLL,2*00\r\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn display_recording_captures_rendered_output() {
        // §54: the display recorder writes each chunk's *rendered* output.
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let path = temp_path("disp");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let recording = start_display_recording(recorder, 64);
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.set_display_recorder(DisplayView::default(), recording);

        p.ingest(bytes_chunk(cid, b"hello"));
        p.finish().await;

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("hello"));
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn display_recording_continues_while_the_view_is_paused() {
        // §58: pausing presentation never pauses recording.
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let path = temp_path("disp-paused");
        let recorder = DisplayFileRecorder::create(&path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let recording = start_display_recording(recorder, 64);
        let mut p = pipeline(cid, PipelineCapacities::default());
        p.set_display_recorder(DisplayView::default(), recording);
        let view = p.display_view_handles()[0].clone();

        view.pause();
        p.ingest(bytes_chunk(cid, b"while-paused"));
        p.finish().await;

        // The paused view's scrollback stayed empty, but the recording captured it.
        assert!(p.stream_tail().is_empty());
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("while-paused"));
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn recording_overflow_faults_emits_event_and_reception_continues() {
        // §56.1: a recorder that cannot keep up faults; reception continues.
        use crate::record::RawRecorder;
        struct StallingRecorder(tokio::sync::oneshot::Receiver<()>);
        #[async_trait::async_trait]
        impl RawRecorder for StallingRecorder {
            async fn write_chunk(
                &mut self,
                _chunk: &ReceivedData,
            ) -> Result<(), crate::core::RecordError> {
                // Park forever: the queue backs up and overflows.
                let _ = (&mut self.0).await;
                Ok(())
            }
            async fn flush(&mut self) -> Result<(), crate::core::RecordError> {
                Ok(())
            }
            async fn finalize(
                &mut self,
                _reason: RecordingStopReason,
            ) -> Result<(), crate::core::RecordError> {
                Ok(())
            }
        }

        let cid = ChannelId::new();
        let (_hold_tx, hold_rx) = tokio::sync::oneshot::channel();
        let recording = start_raw_recording(StallingRecorder(hold_rx), 1);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(recording)
            .with_event_sender(event_tx);

        // Flood: the 1-slot queue fills while the writer is parked; the recording
        // faults, reception continues, and the stream keeps accumulating.
        for _ in 0..8 {
            p.ingest(bytes_chunk(cid, b"x"));
            tokio::task::yield_now().await;
        }
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Faulted));
        assert_eq!(p.snapshot().activity.total_bytes, 8);
        let mut saw_fault = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, RuntimeEvent::RecordingFaulted(id) if id == cid) {
                saw_fault = true;
            }
        }
        assert!(saw_fault);
    }

    #[tokio::test]
    async fn run_channel_drains_then_stops_when_sender_drops() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (_req_tx, req_rx) = tokio::sync::mpsc::channel(1);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel));

        tx.send(bytes_chunk(cid, b"abc")).await.unwrap();
        drop(tx);
        let p = task.await.unwrap();
        assert_eq!(&p.stream_tail()[..], &b"abc"[..]);
    }

    #[tokio::test]
    async fn run_channel_stops_on_cancellation_with_live_sender() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (_req_tx, req_rx) = tokio::sync::mpsc::channel(1);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        cancel.cancel();
        let _p = task.await.unwrap();
        drop(tx); // sender stayed alive the whole time
    }

    #[tokio::test]
    async fn run_channel_serves_snapshots_while_running() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        tx.send(bytes_chunk(cid, b"live")).await.unwrap();
        // Snapshot reports the channel and the stream end offset (cursor target);
        // the bytes themselves come via the incremental StreamDelta request.
        let snapshot = loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            req_tx
                .send(PipelineRequest::Snapshot(reply_tx))
                .await
                .unwrap();
            let s = reply_rx.await.unwrap();
            if s.stream_end_offset > 0 {
                break s;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(snapshot.channel_id, cid);
        assert_eq!(snapshot.stream_end_offset, 4);

        // Fetch the new bytes from offset 0 via the incremental path.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        req_tx
            .send(PipelineRequest::StreamDelta {
                since: 0,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let delta = reply_rx.await.unwrap();
        assert_eq!(delta.base_offset, 0);
        assert_eq!(&*delta.bytes, &b"live"[..]);
        assert_eq!(delta.end_offset, 4);

        cancel.cancel();
        let _ = task.await.unwrap();
    }

    #[tokio::test]
    async fn run_channel_serves_cheap_stats_while_running() {
        let cid = ChannelId::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (_notice_tx, notice_rx) = tokio::sync::mpsc::channel(1);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        tx.send(bytes_chunk(cid, b"12345")).await.unwrap();
        let stats = loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            req_tx.send(PipelineRequest::Stats(reply_tx)).await.unwrap();
            let s = reply_rx.await.unwrap();
            if s.activity.total_bytes > 0 {
                break s;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(stats.activity.total_bytes, 5);

        cancel.cancel();
        let _ = task.await.unwrap();
    }

    #[tokio::test]
    async fn run_channel_records_a_transport_notice_as_a_diagnostic() {
        let cid = ChannelId::new();
        let (_tx, rx) = tokio::sync::mpsc::channel(8);
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel(4);
        let p = pipeline(cid, PipelineCapacities::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_channel(rx, req_rx, notice_rx, p, cancel.clone()));

        notice_tx
            .send(TransportNotice::ReceptionStalled {
                channel_id: cid,
                stalled_for: Duration::from_millis(750),
            })
            .await
            .unwrap();
        let snapshot = loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            req_tx
                .send(PipelineRequest::Snapshot(reply_tx))
                .await
                .unwrap();
            let s = reply_rx.await.unwrap();
            if !s.diagnostics.warnings.is_empty() {
                break s;
            }
            tokio::task::yield_now().await;
        };
        assert!(snapshot.diagnostics.warnings[0]
            .message
            .contains("reception stalled 750 ms"));

        cancel.cancel();
        let _ = task.await.unwrap();
    }

    #[test]
    fn disk_is_low_compares_bytes_and_percent_thresholds() {
        assert!(disk_is_low(9, 100, DiskThreshold::Bytes { bytes: 10 }));
        assert!(!disk_is_low(10, 100, DiskThreshold::Bytes { bytes: 10 }));
        assert!(disk_is_low(4, 100, DiskThreshold::Percent { percent: 5 }));
        assert!(!disk_is_low(5, 100, DiskThreshold::Percent { percent: 5 }));
        // A zero-total filesystem is never "low" (avoids division weirdness).
        assert!(!disk_is_low(0, 0, DiskThreshold::Percent { percent: 5 }));
    }

    #[tokio::test]
    async fn disk_guard_stops_recording_once_and_emits_events() {
        // §56.2/§168: an impossible byte threshold (u64::MAX) is always "low", so
        // the guard warns, stops the recording cleanly, and debounces the report.
        let cid = ChannelId::new();
        let path = temp_path("guard");
        let recorder = RawFileRecorder::create(&path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let recording = start_raw_recording(recorder, 64);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(recording)
            .with_disk_guard(
                DiskGuard {
                    min_free: DiskThreshold::Bytes { bytes: u64::MAX },
                    on_low: LowDiskAction::StopRecording,
                },
                std::env::temp_dir(),
            )
            .with_event_sender(event_tx);

        p.ingest(bytes_chunk(cid, b"data"));
        p.check_disk_guard().await;
        // The recording was stopped and finalized; reception continues.
        assert!(p.raw_recording_state().is_none());
        let mut low = 0;
        let mut stopped = 0;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                RuntimeEvent::DiskSpaceLow(id) if id == cid => low += 1,
                RuntimeEvent::RecordingStoppedLowDisk(id) if id == cid => stopped += 1,
                _ => {}
            }
        }
        assert_eq!((low, stopped), (1, 1));
        // Debounced: a second poll in the same low episode does not re-report.
        p.check_disk_guard().await;
        assert!(event_rx.try_recv().is_err());
        let _ = tokio::fs::remove_file(&path).await;
    }

    // --- Find & Triggers (§50.2, §165) ---

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
        let cid = ChannelId::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[byte_rule(
                "gga",
                b"GGA",
                vec![MatchAction::Notify {
                    severity: crate::diagnostics::DiagnosticSeverity::Warning,
                }],
            )])
            .with_event_sender(event_tx);

        p.ingest(bytes_chunk(cid, b"$GPGLL,...")); // no match (10 bytes, offsets 0..9)
        p.ingest(bytes_chunk(cid, b"$GPGGA,...")); // "GGA" at chunk index 3 → offset 13
        assert_eq!(p.diagnostics().warnings().count(), 1);
        let snapshot = p.snapshot();
        assert_eq!(snapshot.matches.len(), 1);
        // The firing is anchored at the match's exact stream offset (§50.2): chunk 2
        // starts at offset 10 and "GGA" begins 3 bytes into it.
        assert_eq!(snapshot.matches[0].byte_offset, Some(13));
        // A within-chunk match is not a boundary split.
        assert_eq!(snapshot.match_boundary_saves, 0);
        let mut saw_match = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, RuntimeEvent::MatchTriggered(id, _) if id == cid) {
                saw_match = true;
            }
        }
        assert!(saw_match);
    }

    #[test]
    fn byte_pattern_spanning_two_chunks_fires_and_is_measured() {
        // §50.2 cross-chunk carry: a pattern split across two received chunks still
        // matches, anchors on its true start offset, and is counted + diagnosed as a
        // boundary save (where / why / how often).
        let cid = ChannelId::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[byte_rule(
                "gga",
                b"GGA",
                vec![MatchAction::Notify {
                    severity: crate::diagnostics::DiagnosticSeverity::Warning,
                }],
            )])
            .with_event_sender(event_tx);

        // "GG" ends chunk 1 (offsets 0..4); "A" begins chunk 2 (offset 5). A per-
        // chunk scan would miss "GGA"; the carry recovers it.
        p.ingest(bytes_chunk(cid, b"$GPGG"));
        assert_eq!(
            p.snapshot().matches.len(),
            0,
            "nothing fires within chunk 1"
        );
        p.ingest(bytes_chunk(cid, b"A,123"));

        let snap = p.snapshot();
        assert_eq!(snap.matches.len(), 1, "the split pattern fires on chunk 2");
        // "GGA" starts at stream offset 3 (inside chunk 1).
        assert_eq!(snap.matches[0].byte_offset, Some(3));
        // How often: exactly one boundary save measured.
        assert_eq!(snap.match_boundary_saves, 1);
        // Where / why: an event diagnostic records the offset and the cause.
        let boundary_note = p
            .diagnostics()
            .events()
            .any(|d| d.message.contains("read-chunk boundary") && d.message.contains("offset 3"));
        assert!(boundary_note, "a where/why diagnostic is recorded");
        // The recovered match still emits a normal MatchTriggered event.
        let mut saw_match = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, RuntimeEvent::MatchTriggered(id, _) if id == cid) {
                saw_match = true;
            }
        }
        assert!(saw_match);
    }

    #[test]
    fn pause_display_rule_freezes_the_targeted_view() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[byte_rule(
            "freeze",
            b"STOP",
            vec![MatchAction::PauseDisplay { view: Some(0) }],
        )]);
        assert!(!p.snapshot().display_views[0].paused);
        p.ingest(bytes_chunk(cid, b"...STOP..."));
        assert!(p.snapshot().display_views[0].paused);
    }

    #[test]
    fn idle_rule_fires_once_then_rearms_after_data() {
        let cid = ChannelId::new();
        let mut p = pipeline(cid, PipelineCapacities::default()).with_match_rules(&[MatchRule {
            name: "quiet".to_string(),
            condition: MatchCondition::Idle { timeout_ms: 0 },
            actions: vec![MatchAction::Notify {
                severity: crate::diagnostics::DiagnosticSeverity::Event,
            }],
            enabled: true,
        }]);
        // Quiet from creation: the idle rule fires once (offset is None).
        p.evaluate_idle_rules(Instant::now());
        p.evaluate_idle_rules(Instant::now());
        assert_eq!(p.snapshot().matches.len(), 1);
        assert_eq!(p.snapshot().matches[0].byte_offset, None);
        // Data re-arms it; quiet again → a second firing.
        p.ingest(bytes_chunk(cid, b"x"));
        p.evaluate_idle_rules(Instant::now() + Duration::from_secs(1));
        assert_eq!(p.snapshot().matches.len(), 2);
    }

    #[tokio::test]
    async fn record_action_begins_and_stops_from_the_match_forward() {
        // §50.2/§158: Record{Begin} creates the recording lazily and captures only
        // data from the match forward; Record{Stop} finalizes it.
        let cid = ChannelId::new();
        let path = temp_path("armed");
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_match_rules(&[
                byte_rule(
                    "begin",
                    b"BEGIN",
                    vec![MatchAction::Record {
                        target: RecordTarget::Raw,
                        control: RecordControl::Begin,
                    }],
                ),
                byte_rule(
                    "stop",
                    b"STOP",
                    vec![MatchAction::Record {
                        target: RecordTarget::Raw,
                        control: RecordControl::Stop,
                    }],
                ),
            ])
            .with_recording_settings(RawRecordingSettings {
                destination: path.clone(),
                channel_name: "armed".to_string(),
                overwrite: OverwritePolicy::Refuse,
                timestamps: false,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
            });

        // Before the match: nothing on disk, nothing recorded.
        p.ingest(bytes_chunk(cid, b"before "));
        p.apply_pending_records().await;
        assert!(p.raw_recording_state().is_none());

        // The BEGIN chunk fires the rule; recording starts *from the match forward*
        // (the BEGIN chunk itself is not backfilled, §158).
        p.ingest(bytes_chunk(cid, b"BEGIN"));
        p.apply_pending_records().await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"captured"));

        // STOP finalizes; later data is not written.
        p.ingest(bytes_chunk(cid, b"STOP"));
        p.apply_pending_records().await;
        assert!(p.raw_recording_state().is_none());
        p.ingest(bytes_chunk(cid, b"after"));
        p.finish().await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"capturedSTOP");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn set_recording_begins_and_stops_raw_recording_live() {
        // ADR-012: live Record begin/stop without a restart, driven by set_recording
        // (no match rules). Shares the lazy begin / clean finalize path with the
        // match-rule Record action, so capture starts from the toggle forward and a
        // Stop finalizes byte-exactly.
        let cid = ChannelId::new();
        let path = temp_path("live");
        let mut p = pipeline(cid, PipelineCapacities::default()).with_recording_settings(
            RawRecordingSettings {
                destination: path.clone(),
                channel_name: "live".to_string(),
                overwrite: OverwritePolicy::Refuse,
                timestamps: false,
                file_rotation: FileRotationPolicy::None,
                capacity: 64,
            },
        );

        // Before enabling: nothing on disk, nothing recorded.
        p.ingest(bytes_chunk(cid, b"before "));
        assert!(p.raw_recording_state().is_none());

        // Live Begin: recording starts from here forward.
        p.set_recording(true, None).await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"captured"));

        // Begin is idempotent — a second enable while recording is a no-op.
        p.set_recording(true, None).await;
        assert_eq!(p.raw_recording_state(), Some(RecordingState::Enabled));
        p.ingest(bytes_chunk(cid, b"more"));

        // Live Stop finalizes; later data is not written.
        p.set_recording(false, None).await;
        assert!(p.raw_recording_state().is_none());
        p.ingest(bytes_chunk(cid, b"after"));
        p.finish().await;

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"capturedmore");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn mark_action_annotates_the_display_recording_not_the_raw_stream() {
        // §50.2: Mark writes a marker into the display recording (`.disp`) but
        // never the raw byte stream, which stays byte-exact.
        use crate::record::{start_display_recording, DisplayFileRecorder};
        let cid = ChannelId::new();
        let raw_path = temp_path("mark-raw");
        let disp_path = temp_path("mark-disp");
        let raw = RawFileRecorder::create(&raw_path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let disp = DisplayFileRecorder::create(&disp_path, OverwritePolicy::Refuse, false)
            .await
            .unwrap();
        let mut p = pipeline(cid, PipelineCapacities::default())
            .with_raw_recorder(start_raw_recording(raw, 64))
            .with_match_rules(&[byte_rule("mark", b"HERE", vec![MatchAction::Mark])]);
        p.set_display_recorder(DisplayView::default(), start_display_recording(disp, 64));

        p.ingest(bytes_chunk(cid, b"data HERE data"));
        p.finish().await;

        let raw_written = tokio::fs::read(&raw_path).await.unwrap();
        assert_eq!(raw_written, b"data HERE data"); // byte-exact, no marker
        let disp_written = tokio::fs::read_to_string(&disp_path).await.unwrap();
        assert!(disp_written.contains("MARK rule="));
        let _ = tokio::fs::remove_file(&raw_path).await;
        let _ = tokio::fs::remove_file(&disp_path).await;
    }

    #[tokio::test]
    async fn transport_to_pipeline_queue_is_bounded() {
        // §99: the ingest queue is the bounded backpressure edge; try_send on a
        // full queue is refused rather than growing without bound.
        let (tx, _rx) = tokio::sync::mpsc::channel::<ReceivedData>(2);
        let cid = ChannelId::new();
        assert!(tx.try_send(bytes_chunk(cid, b"1")).is_ok());
        assert!(tx.try_send(bytes_chunk(cid, b"2")).is_ok());
        assert!(tx.try_send(bytes_chunk(cid, b"3")).is_err());
    }
}
