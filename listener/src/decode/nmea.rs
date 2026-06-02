//! NMEA0183 decoder (spec §33–§39, acceptance §160).
//!
//! Validates and identifies NMEA0183 sentences; it does **not** extract semantic
//! fields (§33). It uses the local `nmea0183` crate for NMEA functionality —
//! specifically the canonical XOR checksum and `*XX` parsing (§36).
//!
//! The decoder deliberately does *not* delegate identity to `nmea0183::parse`,
//! because that returns an error (no identity) when the checksum is bad or
//! missing. A forensic decoder must still report the talker/sentence of a
//! checksum-failing sentence (§35, §37), so identity is extracted directly while
//! the checksum is validated separately.

use std::collections::BTreeMap;

use nmea0183::checksum;

use crate::core::{
    DecodeError, IntegrityMetadata, IntegrityScope, IntegrityStatus, Message, ProtocolId,
    ProtocolMetadata,
};

use super::{DecodeResult, Decoder};

/// The integrity algorithm this decoder reports (§35).
const ALGORITHM: &str = "NMEA XOR";

/// NMEA checksum validation mode (§37). Affects only the `IntegrityStatus`
/// assigned and the diagnostics raised — never whether a sentence is surfaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NmeaValidationMode {
    /// Missing checksum is accepted (`NotPresent`); present checksums are
    /// validated (§37.1).
    Standard,
    /// A checksum is required: missing is `Invalid` (§37.2).
    Strict,
}

/// Validates and identifies NMEA0183 sentences (§33).
#[derive(Clone, Copy, Debug)]
pub struct NmeaDecoder {
    mode: NmeaValidationMode,
}

impl NmeaDecoder {
    pub fn new(mode: NmeaValidationMode) -> Self {
        Self { mode }
    }

    pub fn standard() -> Self {
        Self::new(NmeaValidationMode::Standard)
    }

    pub fn strict() -> Self {
        Self::new(NmeaValidationMode::Strict)
    }
}

impl Decoder for NmeaDecoder {
    fn decode(&self, message: &Message) -> DecodeResult {
        decode_nmea(&message.bytes, self.mode)
    }
}

/// A message that is not a recognizable NMEA sentence: no metadata, one error.
fn not_recognized(reason: &str) -> DecodeResult {
    DecodeResult {
        metadata: None,
        errors: vec![DecodeError::Malformed(reason.to_string())],
    }
}

fn decode_nmea(bytes: &[u8], mode: NmeaValidationMode) -> DecodeResult {
    // NMEA is ASCII; non-text input is not a sentence.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return not_recognized("message is not valid UTF-8/ASCII text");
    };
    // CRLF termination is part of extraction and may or may not remain (§34).
    let line = text.trim_end_matches(['\r', '\n']);

    let Some(&delimiter) = line.as_bytes().first() else {
        return not_recognized("empty message");
    };
    if delimiter != b'$' && delimiter != b'!' {
        return not_recognized("missing '$' or '!' start delimiter");
    }
    // Everything after the (single-byte) start delimiter.
    let after = &line[1..];

    let mut errors = Vec::new();

    // Checksum (§36): the XOR covers the bytes between the start delimiter and
    // '*', excluding both. `body` is that span.
    let (body, status) = match after.rfind('*') {
        Some(idx) => {
            let body = &after[..idx];
            let suffix = &after[idx..]; // "*XX"
            match checksum::from_hex(suffix) {
                Some(expected) => {
                    if checksum::xor(body.as_bytes()) == expected {
                        (body, IntegrityStatus::Valid)
                    } else {
                        errors.push(DecodeError::Integrity(format!(
                            "checksum mismatch (declared {expected:#04X})"
                        )));
                        (body, IntegrityStatus::Invalid)
                    }
                }
                None => {
                    errors.push(DecodeError::Integrity(format!(
                        "malformed checksum field {suffix:?}"
                    )));
                    (body, IntegrityStatus::Invalid)
                }
            }
        }
        None => match mode {
            NmeaValidationMode::Standard => (after, IntegrityStatus::NotPresent),
            NmeaValidationMode::Strict => {
                errors.push(DecodeError::Integrity(
                    "missing checksum (strict mode requires one)".to_string(),
                ));
                (after, IntegrityStatus::Invalid)
            }
        },
    };

    // Identity (§35): the address field is the part before the first comma.
    let addr = body.split(',').next().unwrap_or(body);
    let mut attributes = BTreeMap::new();
    let message_type;

    if delimiter == b'$' && addr.starts_with('P') {
        // Proprietary sentence (§38): the identifier is everything after "$P".
        attributes.insert("proprietary_id".to_string(), addr[1..].to_string());
        message_type = None;
    } else if addr.len() >= 5 {
        // Standard ($) and AIS (!) share talker(2)+type(3) addressing; AIS gives
        // talker "AI" and sentence "VDM"/"VDO" (§39).
        let (talker, sentence_type) = addr.split_at(addr.len() - 3);
        attributes.insert("talker_id".to_string(), talker.to_string());
        message_type = Some(sentence_type.to_string());
    } else {
        errors.push(DecodeError::Malformed(format!(
            "address field {addr:?} too short to identify talker/sentence"
        )));
        message_type = None;
    }

    let integrity = vec![IntegrityMetadata {
        scope: IntegrityScope::Protocol,
        status,
        algorithm: Some(ALGORITHM.to_string()),
    }];

    DecodeResult {
        metadata: Some(ProtocolMetadata {
            protocol: ProtocolId::Nmea0183,
            message_type,
            integrity,
            attributes,
        }),
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime, MessageMetadata, MessageTimestamp};
    use nmea0183::{
        AisSentence, NmeaChecksumMode, NmeaSentence, ProprietarySentence, SentenceType, TalkerId,
    };
    use std::sync::Arc;

    fn decode(s: &str, mode: NmeaValidationMode) -> DecodeResult {
        let message = Message {
            channel_id: ChannelId::new(),
            number: 1,
            bytes: Arc::from(s.as_bytes()),
            metadata: MessageMetadata {
                total_byte_count: s.len(),
                arrival_timestamp: MessageTimestamp::from(ChunkTime::now()),
                reception_duration: None,
            },
        };
        NmeaDecoder::new(mode).decode(&message)
    }

    fn status(result: &DecodeResult) -> IntegrityStatus {
        result.metadata.as_ref().unwrap().integrity[0].status
    }

    #[test]
    fn valid_standard_sentence_is_identified() {
        // GLL with empty fields → "$GPGLL*<cs>\r\n" (§134 example shape).
        let wire = NmeaSentence::new(TalkerId::GP, SentenceType::GLL, vec![]).to_wire();
        let r = decode(&wire, NmeaValidationMode::Standard);
        let meta = r.metadata.as_ref().unwrap();
        assert_eq!(meta.protocol, ProtocolId::Nmea0183);
        assert_eq!(meta.message_type.as_deref(), Some("GLL"));
        assert_eq!(
            meta.attributes.get("talker_id").map(String::as_str),
            Some("GP")
        );
        assert_eq!(status(&r), IntegrityStatus::Valid);
        assert_eq!(meta.integrity[0].algorithm.as_deref(), Some("NMEA XOR"));
        assert!(r.errors.is_empty());
    }

    #[test]
    fn sentence_without_crlf_still_decodes() {
        // Extraction may strip CRLF (§34); the decoder works with or without it.
        let wire = NmeaSentence::new(TalkerId::GP, SentenceType::GLL, vec![]).to_wire();
        let r = decode(wire.trim_end(), NmeaValidationMode::Standard);
        assert_eq!(status(&r), IntegrityStatus::Valid);
    }

    #[test]
    fn invalid_checksum_is_marked_in_both_modes() {
        let wire = NmeaSentence::new(TalkerId::GP, SentenceType::HDT, vec!["123.4".into()])
            .to_wire_with(NmeaChecksumMode::Wrong);
        for mode in [NmeaValidationMode::Standard, NmeaValidationMode::Strict] {
            let r = decode(&wire, mode);
            assert_eq!(status(&r), IntegrityStatus::Invalid);
            // Still identified despite the bad checksum (§37).
            assert_eq!(r.metadata.unwrap().message_type.as_deref(), Some("HDT"));
            assert!(!r.errors.is_empty());
        }
    }

    #[test]
    fn missing_checksum_differs_by_mode() {
        // "$GPHDT,123.4,T\r\n" — no "*XX".
        let wire = NmeaSentence::new(
            TalkerId::GP,
            SentenceType::HDT,
            vec!["123.4".into(), "T".into()],
        )
        .to_wire_with(NmeaChecksumMode::Omit);

        let standard = decode(&wire, NmeaValidationMode::Standard);
        assert_eq!(status(&standard), IntegrityStatus::NotPresent);
        assert!(standard.errors.is_empty());

        let strict = decode(&wire, NmeaValidationMode::Strict);
        assert_eq!(status(&strict), IntegrityStatus::Invalid);
        assert!(!strict.errors.is_empty());

        // Validation mode affects only status — identity is identical (§37).
        assert_eq!(
            standard.metadata.unwrap().message_type,
            strict.metadata.unwrap().message_type
        );
    }

    #[test]
    fn malformed_checksum_field_is_invalid() {
        let r = decode("$GPGLL*ZZ\r\n", NmeaValidationMode::Standard);
        assert_eq!(status(&r), IntegrityStatus::Invalid);
        assert!(r
            .errors
            .iter()
            .any(|e| matches!(e, DecodeError::Integrity(_))));
    }

    #[test]
    fn proprietary_identifier_is_exposed() {
        let wire = ProprietarySentence::Raw {
            identifier: "GRMZ".to_string(),
            fields: vec!["93".into(), "f".into(), "3".into()],
        }
        .to_wire();
        let r = decode(&wire, NmeaValidationMode::Standard);
        let meta = r.metadata.as_ref().unwrap();
        assert_eq!(
            meta.attributes.get("proprietary_id").map(String::as_str),
            Some("GRMZ")
        );
        assert_eq!(meta.message_type, None);
        assert_eq!(status(&r), IntegrityStatus::Valid);
    }

    #[test]
    fn ais_fragment_is_identified_as_ai_vdm() {
        let received = AisSentence::new(false, "A", &[0x01, 0x02, 0x03]).to_wire();
        let r = decode(&received, NmeaValidationMode::Standard);
        let meta = r.metadata.as_ref().unwrap();
        assert_eq!(
            meta.attributes.get("talker_id").map(String::as_str),
            Some("AI")
        );
        assert_eq!(meta.message_type.as_deref(), Some("VDM"));
        assert_eq!(status(&r), IntegrityStatus::Valid);

        let own = AisSentence::new(true, "B", &[0xAA]).to_wire();
        let r = decode(&own, NmeaValidationMode::Standard);
        assert_eq!(r.metadata.unwrap().message_type.as_deref(), Some("VDO"));
    }

    #[test]
    fn missing_start_delimiter_is_not_recognized() {
        let r = decode("GPGGA,1,2*00\r\n", NmeaValidationMode::Standard);
        assert!(r.metadata.is_none());
        assert!(matches!(r.errors[0], DecodeError::Malformed(_)));
    }

    #[test]
    fn short_address_is_flagged_independently_of_checksum() {
        // Valid checksum over "GP", but the address is too short to identify.
        let body = "GP";
        let cs = checksum::xor(body.as_bytes());
        let wire = format!("${body}*{cs:02X}");
        let r = decode(&wire, NmeaValidationMode::Standard);
        // Checksum is fine...
        assert_eq!(status(&r), IntegrityStatus::Valid);
        // ...but the sentence could not be identified.
        assert_eq!(r.metadata.unwrap().message_type, None);
        assert!(r
            .errors
            .iter()
            .any(|e| matches!(e, DecodeError::Malformed(_))));
    }
}
