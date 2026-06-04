//! Protocol and integrity metadata produced by decoders (spec §134, §135),
//! and the shared `ProtocolId` (§80.1).
//!
//! Protocol Metadata is logically separate from Message contents (§5.5): a
//! decoder annotates a Message, it never mutates one (ADR-002, §29).

use std::collections::BTreeMap;

/// Identifies a decoder protocol (§80.1). Non-exhaustive: additional protocols
/// may be added without a breaking change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum ProtocolId {
    Nmea0183,
}

/// Whether an integrity field covers protocol framing or payload contents (§4.9,
/// §28). E.g. the NMEA XOR checksum is protocol integrity; a future payload CRC
/// is payload integrity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityScope {
    Protocol,
    Payload,
}

/// Result of validating an integrity field (§28).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum IntegrityStatus {
    NotPresent,
    Valid,
    Invalid,
    NotChecked,
    DecoderError,
}

/// One integrity check's scope, outcome, and algorithm name (§28, §135).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityMetadata {
    pub scope: IntegrityScope,
    pub status: IntegrityStatus,
    /// e.g. `"NMEA XOR"` (§35); `None` when no integrity field is present.
    pub algorithm: Option<String>,
}

/// Protocol Metadata produced by a decoder (§4.8, §134).
///
/// For NMEA: `protocol = Nmea0183`, `message_type = Some("GLL")`, with
/// `attributes` carrying `talker_id` and an optional `proprietary_id`.
#[derive(Clone, Debug)]
pub struct ProtocolMetadata {
    pub protocol: ProtocolId,
    pub message_type: Option<String>,
    pub integrity: Vec<IntegrityMetadata>,
    pub attributes: BTreeMap<String, String>,
}
