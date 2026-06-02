//! Decoder trait and protocol decoders (spec §29–§39, §140).
//!
//! This is `listener-decode` (§128). A Decoder interprets a **completed**,
//! immutable Message and produces read-only annotations (Protocol Metadata) and
//! validation errors. It never modifies Messages, byte streams, recording,
//! display, or numbering, and never controls Channel lifecycle (§29, ADR-002).
//! Decoder selection is explicit per Channel (§30); there is no auto-detection.
//! v1 decoders identify and validate only — no semantic field extraction (§31).

pub mod nmea;

pub use nmea::{NmeaDecoder, NmeaValidationMode};

use crate::core::{DecodeError, Message, ProtocolMetadata};

/// Interprets a completed Message, producing Protocol Metadata and any
/// validation errors (§140). `&self`/`&Message` enforce that decoding is
/// read-only (§29).
pub trait Decoder {
    fn decode(&self, message: &Message) -> DecodeResult;
}

/// The output of a [`Decoder`] (§31, §140): optional Protocol Metadata plus any
/// validation errors. Both are logically separate from Message contents (§4.6).
pub struct DecodeResult {
    pub metadata: Option<ProtocolMetadata>,
    pub errors: Vec<DecodeError>,
}
