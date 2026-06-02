//! Shared, typed error definitions used across the core data model and the
//! pipeline subsystems (spec §95, and the decoder/recorder contracts §140,
//! §142).
//!
//! Core is a pure library layer: it uses `thiserror`-style typed errors, not
//! application-level `anyhow` (which belongs to the runtime/CLI/GUI layers).
//! The runtime wraps these with `.context()` at the crate boundary.

/// Coarse classification of an error for diagnostics and reporting (§95).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCategory {
    Configuration,
    Resource,
    Communication,
    Recording,
    Decoder,
    Internal,
}

/// A decoder validation failure (§31, §140). Decoder failures are isolated and
/// never stop reception (§32): they are surfaced as metadata/diagnostics, not
/// propagated to halt the pipeline.
#[derive(Clone, Debug, thiserror::Error)]
pub enum DecodeError {
    /// The Message did not match the decoder's expected framing.
    #[error("malformed message for the selected decoder: {0}")]
    Malformed(String),
    /// An integrity field was present but could not be validated.
    #[error("integrity validation failed: {0}")]
    Integrity(String),
}

/// A recording subsystem failure (§56, §142). A recording fault is terminal for
/// the current artifact and never stalls reception (§56.1).
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// Underlying file I/O failed (create, write, flush, finalize).
    #[error("recording I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The bounded recorder queue could not accept new data; per §56.1 the
    /// recorder faults rather than blocking the producer.
    #[error("recorder queue overflow; recording faulted")]
    QueueOverflow,
}
