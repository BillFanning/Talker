//! Channel lifecycle orchestration, queue wiring, shutdown, fan-out, and backpressure policy.
//!
//! This is `listener-runtime` (spec §128): it owns orchestration — Channel
//! lifecycle, task spawning, queue wiring, fan-out, backpressure policy, and
//! shutdown. Per ADR-001 it is a Tokio hybrid: async tasks for orchestration
//! and network I/O, dedicated OS threads for blocking serial reads.
//!
//! Layers:
//! - [`queue`] — the §99 bounded-queue backpressure policies.
//! - [`metadata`] — the §106 Message Numbering / metadata stage.
//! - [`pipeline`] — the §102 per-Channel processing pipeline + async ingest loop.
//! - [`channel`]/[`tcp`] — per-Channel and TCP-listener task orchestration.
//! - [`build`] — maps validated config to live transports/extractors/decoders.
//! - [`listener`] — the [`Listener`] orchestrator: registry, §9 state machine,
//!   start/stop/apply-pending in the [`crate::core::RuntimeCommand`] vocabulary.
//! - [`snapshot`] — on-demand, pull-side readout of a running Channel's state.

pub mod activity;
pub mod build;
pub mod channel;
pub mod listener;
pub mod metadata;
pub mod pipeline;
pub mod queue;
pub mod snapshot;
pub mod tcp;

pub use activity::{ActivityMeter, ChannelActivity};
pub use build::BuildError;
pub use channel::{start_data_channel, RunningChannel};
pub use listener::{Listener, OrchestratorError};
pub use metadata::MessageNumbering;
pub use pipeline::{run_channel, ChannelPipeline, DecodedMessage, PipelineCapacities};
pub use queue::{
    Diagnostic, DiagnosticSeverity, DiagnosticsQueue, DropOldestQueue, FaultOnFullQueue, QueueFull,
};
pub use snapshot::{ChannelSnapshot, DiagnosticsSnapshot, DisplayViewSnapshot, SnapshotRequest};
pub use tcp::{start_tcp_listener, TcpListenerHandle};
