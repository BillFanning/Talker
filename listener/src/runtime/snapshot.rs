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
use crate::diagnostics::Diagnostic;

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
}

/// A single rule firing (§50.2). Records which rule fired and, for a data
/// condition, the **stream byte offset** it fired at (`None` for an `Idle`
/// firing, which is not tied to data). This is the observable record of
/// `Highlight`/`Mark` (whose visual styling is applied by the UI) and of any
/// rule's trigger; `Notify` also lands in diagnostics and every firing emits a
/// `MatchTriggered` event.
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
