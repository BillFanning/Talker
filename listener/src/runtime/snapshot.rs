//! On-demand snapshots of a Channel's observable pipeline state (spec §10, §137;
//! listener ADR-006).
//!
//! The pipeline owns its retained Messages, per-view display history, and
//! diagnostics by value inside the [`run_channel`](super::pipeline::run_channel)
//! task, so they are not directly readable while a Channel runs. A
//! [`ChannelSnapshot`] is a point-in-time, owned copy the pipeline builds on
//! request and hands back through a oneshot reply.
//!
//! This is the *pull* half of the observability surface. The *push* half is the
//! [`RuntimeEvent`](crate::core::RuntimeEvent) stream, which stays authoritative
//! for presentation observers (ADR-006): a UI folds events for liveness and
//! requests a snapshot when it needs the actual retained content. Observers never
//! own or block the pipeline.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::core::{ChannelId, DisplayViewId, MatchRuleId, RecordingState};
use crate::diagnostics::Diagnostic;

use super::activity::ChannelActivity;

/// A query the pipeline task answers from its current state, replying on a
/// oneshot. Dropping the reply sender simply yields nothing.
///
/// Two granularities so observers pay only for what they show (listener ADR-006):
/// - [`Stats`](Self::Stats): O(1) counters — no Message cloning. A multi-channel
///   overview polls this for *every* Channel to keep per-tab health current.
/// - [`Snapshot`](Self::Snapshot): the full point-in-time state, including cloned
///   retained/display Messages. Polled only for the Channel actually on screen, so
///   a large retention buffer isn't deep-copied for every Channel each tick.
pub enum PipelineRequest {
    Snapshot(oneshot::Sender<ChannelSnapshot>),
    Stats(oneshot::Sender<ChannelStats>),
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
}

/// A point-in-time, owned copy of one Channel's observable pipeline state.
///
/// Built by the pipeline task in response to a snapshot request, so reading it
/// neither blocks reception nor shares mutable pipeline state. Messages are held
/// as `Arc`s, so a snapshot is cheap to build (reference-count bumps, not deep
/// copies).
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
    /// cross-references these against retained Messages to highlight/annotate.
    pub matches: Vec<TriggeredMatch>,
    /// The most recent verbatim **pre-extraction** bytes, oldest → newest, byte-
    /// capped (§88 count-retention doesn't apply — there are no Message boundaries).
    /// Feeds the Stream display source (ADR-009, §18/§41): rendering it reconstructs
    /// the wire regardless of framing or read-chunk boundaries.
    pub stream_tail: Arc<[u8]>,
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
/// content is the shared `stream_tail`, rendered per view.
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
