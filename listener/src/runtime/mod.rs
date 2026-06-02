//! Channel lifecycle orchestration, queue wiring, shutdown, fan-out, and backpressure policy.
//!
//! This is `listener-runtime` (spec §128): it owns orchestration — Channel
//! lifecycle, task spawning, queue wiring, fan-out, backpressure policy, and
//! shutdown. Per ADR-001 it is a Tokio hybrid: async tasks for orchestration
//! and network I/O, dedicated OS threads for blocking serial reads.
//!
//! Implemented so far (skeleton):
//! - [`queue`] — the §99 bounded-queue backpressure policies.
//! - [`metadata`] — the §106 Message Numbering / metadata stage.
//! - [`pipeline`] — the §102 per-Channel processing pipeline and its async
//!   ingest loop ([`pipeline::run_channel`]).
//!
//! Still to come (wired with the transports, steps 6–8): the top-level command
//! loop that owns the channel registry, mints `ChannelId`s for accepted TCP
//! connections (§97.1), spawns transport runners, and drives the §110 stop
//! sequence in response to [`crate::core::RuntimeCommand`]s.

pub mod channel;
pub mod metadata;
pub mod pipeline;
pub mod queue;
pub mod tcp;

pub use channel::{start_data_channel, RunningChannel};
pub use metadata::MessageNumbering;
pub use pipeline::{run_channel, ChannelPipeline, DecodedMessage, PipelineCapacities};
pub use queue::{
    Diagnostic, DiagnosticSeverity, DiagnosticsQueue, DropOldestQueue, FaultOnFullQueue, QueueFull,
};
pub use tcp::{start_tcp_listener, TcpListenerHandle};
