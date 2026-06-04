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

use tokio::sync::oneshot;

use crate::core::{ChannelId, DisplayViewId, MatchRuleId, RecordingState};
use crate::diagnostics::Diagnostic;

use super::activity::ChannelActivity;
use super::pipeline::DecodedMessage;

/// A request for a [`ChannelSnapshot`]: the reply half of a oneshot the pipeline
/// fulfils from its current state. Dropping the sender simply yields no snapshot.
pub type SnapshotRequest = oneshot::Sender<ChannelSnapshot>;

/// A point-in-time, owned copy of one Channel's observable pipeline state.
///
/// Built by the pipeline task in response to a snapshot request, so reading it
/// neither blocks reception nor shares mutable pipeline state. Messages are held
/// as `Arc`s, so a snapshot is cheap to build (reference-count bumps, not deep
/// copies).
#[derive(Clone, Debug)]
pub struct ChannelSnapshot {
    pub channel_id: ChannelId,
    /// The next Message Number to be assigned (== Messages produced so far + 1).
    pub next_message_number: u64,
    /// Retained decoded Messages, oldest → newest, bounded by §88 retention.
    pub retained: Vec<DecodedMessage>,
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
}

/// A single Match Rule firing (§50.2). Records which rule fired and, for a
/// per-Message condition, the Message Number it fired on (`None` for an `Idle`
/// firing, which is not tied to a Message). This is the observable record of
/// `Highlight`/`Mark` (whose visual styling is applied by the UI) and of any
/// rule's trigger; `Notify` also lands in diagnostics and every firing emits a
/// `MatchTriggered` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggeredMatch {
    pub rule_id: MatchRuleId,
    pub message_number: Option<u64>,
}

/// One Display View's snapshot: its identity, pause state, and accumulated
/// presentation history (§50, §87).
#[derive(Clone, Debug)]
pub struct DisplayViewSnapshot {
    pub id: DisplayViewId,
    pub paused: bool,
    /// The view's display history, oldest → newest. Empty or frozen while paused.
    pub messages: Vec<DecodedMessage>,
}

/// Retained diagnostics by severity (§92–§95), oldest → newest within each.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticsSnapshot {
    pub events: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    pub errors: Vec<Diagnostic>,
}
