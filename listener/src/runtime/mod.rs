//! Channel lifecycle orchestration, queue wiring, shutdown, fan-out, and backpressure policy.
//!
//! This is `listener-runtime` (spec §128): it owns orchestration — Channel
//! lifecycle, task spawning, queue wiring, fan-out, backpressure policy, and
//! shutdown. Per ADR-001 it is a Tokio hybrid: async tasks for orchestration
//! and network I/O, dedicated OS threads for blocking serial reads.
//!
//! Layers:
//! - [`queue`] — the §99 bounded-queue backpressure policies.
//! - [`pipeline`] — the §102 per-Channel stream pipeline + async ingest loop
//!   (raw recorder, scrollback, display recording, find/triggers, diagnostics).
//! - [`activity`] — the §166 per-Channel liveness/throughput meter.
//! - [`matchrule`] — §50.2 Find & Triggers evaluation (BytePattern / Idle).
//! - [`channel`]/[`tcp`] — per-Channel and TCP-listener task orchestration.
//! - [`build`] — maps validated config to live transports.
//! - [`listener`] — the [`Listener`] orchestrator: registry, §9 state machine,
//!   start/stop/apply-pending exposed as async methods (the command surface; ADR-012).
//! - [`snapshot`] — on-demand, pull-side readout: the small [`snapshot::ChannelSnapshot`]
//!   plus incremental [`snapshot::StreamDelta`] scrollback reads (ADR-011).

pub mod activity;
pub mod build;
pub mod channel;
pub mod listener;
pub mod matchrule;
pub mod pipeline;
pub mod queue;
pub mod snapshot;
pub mod tcp;

pub use activity::{ActivityMeter, ChannelActivity};
pub use build::BuildError;
pub use channel::{start_data_channel, RunningChannel};
pub use listener::{Listener, OrchestratorError};
pub use matchrule::{FiredRule, MatchRuleSet};
pub use pipeline::{run_channel, ChannelPipeline, PipelineCapacities};
pub use queue::{
    Diagnostic, DiagnosticSeverity, DiagnosticsQueue, DropOldestQueue, FaultOnFullQueue, QueueFull,
};
pub use snapshot::{
    ChannelSnapshot, ChannelStats, DiagnosticsSnapshot, DisplayViewSnapshot, PipelineRequest,
    QueueDepth, StreamDelta, TriggeredMatch,
};
pub use tcp::{start_tcp_listener, TcpListenerHandle};
