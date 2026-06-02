//! Stream, delimiter, fixed-length, and protocol message extraction.
//!
//! This is `listener-extract` (spec §128). Extraction answers *where a Message
//! begins and ends* (§4.4) — it never interprets protocol meaning (§5.2). The
//! extractor owns all partial-message state (§105), including the `ChunkTime`
//! (§138) of the chunk that supplied the first byte of the Message currently
//! being assembled, so the metadata stage can derive Arrival Timestamp (§26)
//! and Reception Duration (§27).
//!
//! Only one extraction method is active per Channel (§20). The mapping from the
//! persisted `ExtractionConfig` (§20, §72) to a concrete extractor — including
//! `Protocol { Nmea0183 }`, which v1 may realize as CRLF delimiter extraction
//! (§23, §34) — is wired in the config/runtime layer; this module exposes the
//! extractors with direct constructors.

mod delimiter;
mod fixed_length;
mod stream;

pub use delimiter::DelimiterExtractor;
pub use fixed_length::FixedLengthExtractor;
pub use stream::StreamExtractor;

use crate::core::{ChunkTime, MessageBytes};

/// Splits a received byte stream into completed Messages (spec §139).
///
/// Implementations are stateful and Channel-local. `push_chunk` is fed each
/// transport chunk in receive order with that chunk's `ChunkTime`; it returns
/// every Message completed by those bytes (possibly several, possibly none).
/// `finish` is called once at Channel stop: incomplete trailing data is
/// discarded and not emitted (§112).
pub trait MessageExtractor {
    /// Feed one received chunk. `at` is the chunk's `ChunkTime` (§138); the
    /// extractor attaches the first/last contributing chunk times to each
    /// completed Message for §26/§27.
    fn push_chunk(&mut self, bytes: &[u8], at: ChunkTime) -> Vec<MessageBytes>;

    /// Called at Channel stop. Any partial, un-terminated Message is discarded
    /// per §112; this returns the (currently always empty) set of Messages that
    /// could still be completed without further input.
    fn finish(&mut self) -> Vec<MessageBytes>;
}
