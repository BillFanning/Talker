//! On-demand snapshots of a Channel's observable pipeline state (spec §10, §137;
//! listener ADR-006).
//!
//! The pipeline owns its stream scrollback, diagnostics, and recent match firings
//! by value inside the [`run_channel`](super::pipeline::run_channel) task, so they
//! are not directly readable while a Channel runs. A [`ChannelSnapshot`] is a
//! point-in-time, owned copy of the *small* observable state (diagnostics, matches,
//! liveness, view pause, the stream's end offset); the scrollback bytes themselves
//! are fetched separately and incrementally via [`StreamDelta`] so a high-throughput
//! viewer never re-ships the whole ~1 MB buffer (§87, ADR-009).
//!
//! This is the *pull* half of the observability surface. The *push* half is the
//! [`RuntimeEvent`](crate::core::RuntimeEvent) stream, which stays authoritative
//! for presentation observers (ADR-006): a UI folds events for liveness and
//! requests a snapshot/stream-delta when it needs the actual retained content.
//! Observers never own or block the pipeline.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::core::{ChannelId, DisplayViewId, MatchRuleId, RecordingState};
use crate::diagnostics::{Diagnostic, DiagnosticSeverity};

use super::activity::ChannelActivity;
use super::pipeline::RawRecordingSettings;

/// A query the pipeline task answers from its current state, replying on a
/// oneshot. Dropping the reply sender simply yields nothing.
///
/// Three granularities so observers pay only for what they show (listener ADR-006):
/// - [`Stats`](Self::Stats): O(1) counters for per-tab health. A multi-channel
///   overview polls this for *every* Channel.
/// - [`Snapshot`](Self::Snapshot): the small observable state (diagnostics, recent
///   matches, liveness, view pause, the stream end offset) — bounded, so it is cheap
///   even for the on-screen Channel. It carries **no** scrollback bytes.
/// - [`StreamDelta`](Self::StreamDelta): the scrollback bytes new since a cursor —
///   the only O(bytes) reply, and only of the *new* bytes, not the whole buffer.
pub enum PipelineRequest {
    Snapshot(oneshot::Sender<ChannelSnapshot>),
    Stats(oneshot::Sender<ChannelStats>),
    /// Incremental stream bytes since the requester's cursor (§87, ADR-009). The
    /// pipeline returns only what is new (or a reset window if the cursor fell
    /// behind eviction), so a live high-throughput viewer never re-ships or
    /// re-renders the whole ~1 MB scrollback each poll.
    StreamDelta {
        since: u64,
        reply: oneshot::Sender<StreamDelta>,
    },
    /// Begin or stop Raw recording on a running Channel without a restart (§50.2,
    /// ADR-012): the live counterpart of the match-rule `Record` action, driven by
    /// the same lazy begin/finalize path. `enabled = true` begins (idempotent if
    /// already recording); `false` stops and finalizes. `settings`, when present,
    /// apply the recording config the caller read at click time first — so a
    /// destination set *after* the channel started still records live, no restart
    /// needed. Fire-and-forget — the outcome surfaces through the next snapshot's
    /// recording state and, on a begin failure, a `RecordingFaulted` event (§55).
    SetRecording {
        enabled: bool,
        settings: Option<RawRecordingSettings>,
    },
}

/// An incremental slice of a Channel's stream scrollback (§87), answering "what
/// stream bytes exist at or after offset `since`?". Offsets are absolute stream
/// positions (bytes received since Start, modulo display pause).
#[derive(Clone, Debug)]
pub struct StreamDelta {
    /// Absolute stream offset of `bytes[0]`. Normally equals the requested `since`;
    /// it is **greater** when the requester's cursor had already been evicted from
    /// the front of the bounded scrollback — a signal to the consumer to reset its
    /// view to this window rather than append.
    pub base_offset: u64,
    /// The new (or reset-window) bytes, oldest → newest.
    pub bytes: Arc<[u8]>,
    /// Absolute offset just past the last retained byte (`base_offset + bytes.len()`
    /// for a fresh fetch). The consumer stores this as its next cursor.
    pub end_offset: u64,
}

/// Occupancy of one bounded pipeline queue (§99, §124) — for stress testing and
/// backpressure diagnosis. `current` is the depth at snapshot time; `peak` is the
/// high-water mark since Start (the value that matters — a transient spike a 5 Hz poll
/// would miss); `capacity` is the bound. A `peak` approaching `capacity` means the queue
/// is backing up: the recorder/disk (or the reader) can't keep up, the precursor to a
/// reception stall or a recording-queue-overflow fault.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueDepth {
    pub current: usize,
    pub peak: usize,
    pub capacity: usize,
}

/// Cheap, O(1) liveness counters for a Channel — everything a multi-channel
/// overview needs per tab without cloning the stream scrollback (the expensive
/// part of a full [`ChannelSnapshot`]).
#[derive(Clone, Debug)]
pub struct ChannelStats {
    /// Liveness facts: rolling throughput, total bytes + last-data time (§91.1, §166).
    pub activity: ChannelActivity,
    /// Retained-diagnostic counts by severity (§88) — for per-tab health.
    pub event_count: usize,
    pub warning_count: usize,
    pub error_count: usize,
    /// Raw-recording state, or `None` when raw recording isn't attached (§53).
    pub raw_recording: Option<RecordingState>,
    /// How many `BytePattern` matches were recovered only because a pattern spanned
    /// a read-chunk boundary (§50.2) — the cross-chunk-carry measurement. A nonzero,
    /// rising count tells an operator that read boundaries are routinely splitting
    /// the patterns they search for (the "how often").
    pub match_boundary_saves: u64,
    /// Depth of the Transport→Pipeline ingest queue (§99) — the edge that backpressures
    /// the reader. A rising `peak` is the first sign reception is outrunning processing.
    pub ingest_queue: QueueDepth,
    /// Depth of the Raw-recording queue (§56.1), or `None` when no recorder is attached.
    /// A `peak` near `capacity` precedes a `QueueOverflow` recording fault — i.e. the
    /// disk can't keep up with the inflow.
    pub raw_recording_queue: Option<QueueDepth>,
}

/// A point-in-time, owned copy of one Channel's *small* observable pipeline state
/// (diagnostics, recent matches, liveness, view pause, the stream end offset).
///
/// Built by the pipeline task in response to a snapshot request, so reading it
/// neither blocks reception nor shares mutable pipeline state. Every field is
/// bounded (§88, §124) — the unbounded scrollback bytes are **not** here; they are
/// fetched incrementally via [`StreamDelta`] — so a snapshot is always cheap to
/// build and ship, even at high throughput.
#[derive(Clone, Debug)]
pub struct ChannelSnapshot {
    pub channel_id: ChannelId,
    /// One entry per Display View (§48), in creation order (default view first).
    pub display_views: Vec<DisplayViewSnapshot>,
    /// Retained diagnostics, separated by severity (§88).
    pub diagnostics: DiagnosticsSnapshot,
    /// Raw-recording state, or `None` when raw recording isn't attached (§53).
    pub raw_recording: Option<RecordingState>,
    /// Liveness facts: rolling throughput + last-data time (§91.1, §166).
    pub activity: ChannelActivity,
    /// Recent Match Rule firings, oldest → newest, bounded (§50.2, §165). A GUI
    /// cross-references these (by `byte_offset`) against the accumulated stream to
    /// highlight/annotate.
    pub matches: Vec<TriggeredMatch>,
    /// How many `BytePattern` matches were recovered only because a pattern spanned
    /// a read-chunk boundary (§50.2). The aggregate "how often" of the cross-chunk
    /// measurement; per-occurrence detail (where/why) is in `diagnostics`.
    pub match_boundary_saves: u64,
    /// Absolute stream offset just past the last received byte (§87): the total
    /// bytes accepted into the scrollback since Start. The live viewer uses this as
    /// its cursor target and fetches the bytes themselves incrementally via
    /// [`PipelineRequest::StreamDelta`] — the big scrollback is **not** bundled into
    /// every snapshot (that was O(buffer) at 5 Hz).
    pub stream_end_offset: u64,
    /// Ingest queue occupancy (§99) — see [`ChannelStats::ingest_queue`].
    pub ingest_queue: QueueDepth,
    /// Raw-recording queue occupancy (§56.1), or `None` when no recorder is attached —
    /// see [`ChannelStats::raw_recording_queue`].
    pub raw_recording_queue: Option<QueueDepth>,
}

/// A single rule firing (§50.2). Records which rule fired and, for a data
/// condition, the **stream byte offset** it fired at (`None` for an `Idle`
/// firing, which is not tied to data). This is the observable record of a `Mark`
/// and of any rule's trigger; `Notify` also lands in diagnostics and every firing
/// emits a `MatchTriggered` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggeredMatch {
    pub rule_id: MatchRuleId,
    pub byte_offset: Option<u64>,
}

/// One Display View's snapshot: its identity and pause state (§50). The viewed
/// content is the shared stream scrollback (fetched via [`StreamDelta`]), rendered
/// per view.
#[derive(Clone, Debug)]
pub struct DisplayViewSnapshot {
    pub id: DisplayViewId,
    pub paused: bool,
}

/// Retained diagnostics by severity (§92–§95), oldest → newest within each.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticsSnapshot {
    pub events: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub errors: Vec<Diagnostic>,
}

impl DiagnosticsSnapshot {
    /// Group a flat list of diagnostics back into the per-severity buckets (the inverse
    /// of [`into_sorted_vec`](Self::into_sorted_vec)). Used to rebuild a snapshot from a
    /// retained `Vec<Diagnostic>`.
    pub fn from_diagnostics(diagnostics: impl IntoIterator<Item = Diagnostic>) -> Self {
        let mut snap = Self::default();
        for d in diagnostics {
            match d.severity {
                DiagnosticSeverity::Event => snap.events.push(d),
                DiagnosticSeverity::Warning => snap.warnings.push(d),
                DiagnosticSeverity::Error => snap.errors.push(d),
            }
        }
        snap
    }

    /// Flatten all severities into one chronological `Vec` (oldest → newest). The single
    /// timeline a consumer that doesn't care about severity buckets wants — the retained
    /// log and the GUI render order both use this.
    pub fn into_sorted_vec(self) -> Vec<Diagnostic> {
        let mut all: Vec<_> = self
            .events
            .into_iter()
            .chain(self.warnings)
            .chain(self.errors)
            .collect();
        all.sort_by_key(|d| d.timestamp);
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64, sev: DiagnosticSeverity, msg: &str) -> Diagnostic {
        Diagnostic::at(sev, msg, UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn diagnostics_round_trip_through_grouping_and_flattening() {
        use DiagnosticSeverity::*;
        // Out-of-order across severities; from_diagnostics groups, into_sorted_vec
        // flattens back into one chronological timeline.
        let input = vec![
            at(3, Error, "boom"),
            at(1, Event, "started"),
            at(2, Warning, "slow"),
        ];
        let grouped = DiagnosticsSnapshot::from_diagnostics(input);
        assert_eq!(grouped.events.len(), 1);
        assert_eq!(grouped.warnings.len(), 1);
        assert_eq!(grouped.errors.len(), 1);

        let sorted = grouped.into_sorted_vec();
        let flat: Vec<&str> = sorted.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(flat, vec!["started", "slow", "boom"], "chronological order");
    }
}
