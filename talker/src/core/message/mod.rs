//! Message definitions: payload format/encoding, timestamp, and checksum.
//!
//! A [`MessageConfig`] is compiled to a [`CompiledMessage`], which renders the
//! wire bytes — `[timestamp][payload][checksum]` — fresh on each send so the
//! timestamp is current.

mod checksum;
mod codepage;
mod timestamp;

pub use checksum::{ChecksumAlgorithm, ChecksumConfig};
pub use codepage::CodePage;
pub use timestamp::TimestampConfig;

use serde::{Deserialize, Serialize};

/// Byte order for multi-byte text encodings (UTF-16).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteOrder {
    /// Most-significant byte first (network order).
    #[default]
    BigEndian,
    /// Least-significant byte first.
    LittleEndian,
}

/// One message in a channel: a payload, a send interval, and optional
/// timestamp and checksum.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageConfig {
    pub payload: PayloadConfig,
    pub interval_ms: u64,
    #[serde(default)]
    pub timestamp: Option<TimestampConfig>,
    #[serde(default)]
    pub checksum: Option<ChecksumConfig>,
}

impl MessageConfig {
    /// Create a message with no timestamp and no checksum.
    pub fn new(payload: PayloadConfig, interval_ms: u64) -> Self {
        Self {
            payload,
            interval_ms,
            timestamp: None,
            checksum: None,
        }
    }

    /// Compile the static parts of this message. The payload is encoded once;
    /// the timestamp and checksum settings are kept for per-send rendering.
    pub fn compile(&self) -> anyhow::Result<CompiledMessage> {
        Ok(CompiledMessage {
            payload: self.payload.compile()?,
            timestamp: self.timestamp,
            checksum: self.checksum,
        })
    }
}

/// A message with its payload encoded, ready to render wire bytes per send.
#[derive(Debug, Clone)]
pub struct CompiledMessage {
    payload: Vec<u8>,
    timestamp: Option<TimestampConfig>,
    checksum: Option<ChecksumConfig>,
}

impl CompiledMessage {
    /// Produce the wire bytes for one send: `[timestamp][payload][checksum]`.
    ///
    /// The timestamp is generated at the current instant; the checksum is
    /// computed over the timestamp and payload together.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.len() + 16);
        if let Some(ts) = &self.timestamp {
            out.extend_from_slice(ts.format(chrono::Utc::now()).as_bytes());
        }
        out.extend_from_slice(&self.payload);
        if let Some(cs) = &self.checksum {
            let sum = cs.compute(&out);
            out.extend_from_slice(&sum);
        }
        out
    }
}

/// The payload source for one message.
///
/// `compile()` converts this to the static wire bytes for the payload.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PayloadConfig {
    /// Raw bytes as a hex string (spaces and hyphens are stripped).
    /// Example: `"DE AD BE EF"` or `"DEADBEEF"`.
    RawHex { data: String },
    /// Unicode text encoded as UTF-8.
    Utf8 {
        #[serde(default)]
        text: String,
    },
    /// Unicode text encoded as UTF-16, with a configurable byte order and an
    /// optional leading byte-order mark.
    Utf16 {
        #[serde(default)]
        text: String,
        #[serde(default)]
        byte_order: ByteOrder,
        #[serde(default)]
        bom: bool,
    },
    /// Text encoded with a single-byte code page.
    Ascii {
        #[serde(default)]
        text: String,
        #[serde(default)]
        code_page: CodePage,
    },
    /// A standard NMEA 0183 sentence. Fields are the payload values after the
    /// sentence type; the checksum is computed automatically.
    Nmea {
        talker: String,
        sentence_type: String,
        #[serde(default)]
        fields: Vec<String>,
    },
}

impl PayloadConfig {
    pub fn raw_hex(hex: impl Into<String>) -> Self {
        Self::RawHex { data: hex.into() }
    }

    pub fn nmea(
        talker: impl Into<String>,
        sentence_type: impl Into<String>,
        fields: Vec<String>,
    ) -> Self {
        Self::Nmea {
            talker: talker.into(),
            sentence_type: sentence_type.into(),
            fields,
        }
    }

    /// Encode this payload to its static wire bytes.
    pub fn compile(&self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::RawHex { data } => compile_hex(data),
            Self::Utf8 { text } => Ok(text.clone().into_bytes()),
            Self::Utf16 {
                text,
                byte_order,
                bom,
            } => Ok(encode_utf16(text, *byte_order, *bom)),
            Self::Ascii { text, code_page } => codepage::encode(text, *code_page),
            Self::Nmea {
                talker,
                sentence_type,
                fields,
            } => compile_nmea(talker, sentence_type, fields),
        }
    }
}

fn compile_hex(data: &str) -> anyhow::Result<Vec<u8>> {
    let clean: String = data.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    anyhow::ensure!(
        clean.len().is_multiple_of(2),
        "hex string has odd length after stripping whitespace: {data:?}"
    );
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16)
                .map_err(|_| anyhow::anyhow!("invalid hex byte {:?} in {data:?}", &clean[i..i + 2]))
        })
        .collect()
}

fn encode_utf16(text: &str, byte_order: ByteOrder, bom: bool) -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    if bom {
        units.push(0xFEFF);
    }
    units.extend(text.encode_utf16());
    let mut out = Vec::with_capacity(units.len() * 2);
    for u in units {
        match byte_order {
            ByteOrder::BigEndian => out.extend_from_slice(&u.to_be_bytes()),
            ByteOrder::LittleEndian => out.extend_from_slice(&u.to_le_bytes()),
        }
    }
    out
}

fn compile_nmea(talker: &str, sentence_type: &str, fields: &[String]) -> anyhow::Result<Vec<u8>> {
    use nmea0183::{NmeaSentence, SentenceType, TalkerId};
    let talker_id: TalkerId = talker.parse().unwrap();
    let st: SentenceType = sentence_type.parse().unwrap();
    let sentence = NmeaSentence::new(talker_id, st, fields.to_vec());
    Ok(sentence.to_wire().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── RawHex ────────────────────────────────────────────────────────────────

    #[test]
    fn compile_raw_hex_basic() {
        assert_eq!(
            PayloadConfig::raw_hex("DEADBEEF").compile().unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn compile_raw_hex_with_separators() {
        assert_eq!(
            PayloadConfig::raw_hex("DE AD-BE EF").compile().unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn compile_raw_hex_odd_length_errors() {
        assert!(PayloadConfig::raw_hex("DEA").compile().is_err());
    }

    #[test]
    fn compile_raw_hex_invalid_byte_errors() {
        assert!(PayloadConfig::raw_hex("DEXZ").compile().is_err());
    }

    // ── UTF-8 ─────────────────────────────────────────────────────────────────

    #[test]
    fn compile_utf8() {
        let p = PayloadConfig::Utf8 {
            text: "héllo".to_string(),
        };
        assert_eq!(p.compile().unwrap(), "héllo".as_bytes());
    }

    // ── UTF-16 ────────────────────────────────────────────────────────────────

    #[test]
    fn compile_utf16_big_endian_no_bom() {
        let p = PayloadConfig::Utf16 {
            text: "AB".to_string(),
            byte_order: ByteOrder::BigEndian,
            bom: false,
        };
        assert_eq!(p.compile().unwrap(), vec![0x00, 0x41, 0x00, 0x42]);
    }

    #[test]
    fn compile_utf16_little_endian_with_bom() {
        let p = PayloadConfig::Utf16 {
            text: "A".to_string(),
            byte_order: ByteOrder::LittleEndian,
            bom: true,
        };
        // BOM U+FEFF then 'A' U+0041, little-endian
        assert_eq!(p.compile().unwrap(), vec![0xFF, 0xFE, 0x41, 0x00]);
    }

    #[test]
    fn compile_utf16_surrogate_pair() {
        // U+1F600 encodes as a surrogate pair: D83D DE00
        let p = PayloadConfig::Utf16 {
            text: "\u{1F600}".to_string(),
            byte_order: ByteOrder::BigEndian,
            bom: false,
        };
        assert_eq!(p.compile().unwrap(), vec![0xD8, 0x3D, 0xDE, 0x00]);
    }

    // ── ASCII / code pages ────────────────────────────────────────────────────

    #[test]
    fn compile_ascii_iso8859_1() {
        let p = PayloadConfig::Ascii {
            text: "café".to_string(),
            code_page: CodePage::Iso8859_1,
        };
        assert_eq!(p.compile().unwrap(), vec![b'c', b'a', b'f', 0xE9]);
    }

    #[test]
    fn compile_ascii_unrepresentable_errors() {
        let p = PayloadConfig::Ascii {
            text: "€".to_string(),
            code_page: CodePage::Iso8859_1,
        };
        assert!(p.compile().is_err());
    }

    // ── NMEA ──────────────────────────────────────────────────────────────────

    #[test]
    fn compile_nmea_wire_format() {
        let p = PayloadConfig::nmea("GP", "GGA", vec!["123519".to_string()]);
        let wire = String::from_utf8(p.compile().unwrap()).unwrap();
        assert!(wire.starts_with("$GPGGA,123519*"));
        assert!(wire.ends_with("\r\n"));
    }

    // ── MessageConfig / CompiledMessage ───────────────────────────────────────

    #[test]
    fn render_plain_payload_is_just_the_payload() {
        let m = MessageConfig::new(PayloadConfig::raw_hex("AABB"), 1000);
        assert_eq!(m.compile().unwrap().render(), vec![0xAA, 0xBB]);
    }

    #[test]
    fn render_appends_checksum_over_payload() {
        let m = MessageConfig {
            payload: PayloadConfig::raw_hex("01 02 03"),
            interval_ms: 1000,
            timestamp: None,
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Xor,
                intentionally_wrong: false,
            }),
        };
        // payload 01 02 03, XOR = 00, appended
        assert_eq!(m.compile().unwrap().render(), vec![0x01, 0x02, 0x03, 0x00]);
    }

    #[test]
    fn render_prepends_timestamp_then_payload() {
        let m = MessageConfig {
            payload: PayloadConfig::Utf8 {
                text: "X".to_string(),
            },
            interval_ms: 1000,
            timestamp: Some(TimestampConfig::default()),
            checksum: None,
        };
        let out = m.compile().unwrap().render();
        // "HH:MM:SS" (8 bytes) followed by the payload 'X'
        assert_eq!(out.len(), 9);
        assert_eq!(out[8], b'X');
        assert_eq!(out[2], b':');
    }

    #[test]
    fn render_checksum_covers_timestamp_and_payload() {
        let m = MessageConfig {
            payload: PayloadConfig::raw_hex("FF"),
            interval_ms: 1000,
            timestamp: Some(TimestampConfig::default()),
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Xor,
                intentionally_wrong: false,
            }),
        };
        let out = m.compile().unwrap().render();
        // last byte is the XOR of everything before it
        let body_xor = out[..out.len() - 1].iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(*out.last().unwrap(), body_xor);
    }

    #[test]
    fn message_config_round_trip() {
        let m = MessageConfig {
            payload: PayloadConfig::nmea("GP", "RMC", vec![]),
            interval_ms: 500,
            timestamp: Some(TimestampConfig {
                include_date: true,
                include_millis: true,
                include_timezone: true,
            }),
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Crc16Ccitt,
                intentionally_wrong: false,
            }),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MessageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn message_config_round_trip_defaults_timestamp_checksum() {
        let json = r#"{"payload":{"type":"raw_hex","data":"AB"},"interval_ms":100}"#;
        let m: MessageConfig = serde_json::from_str(json).unwrap();
        assert!(m.timestamp.is_none());
        assert!(m.checksum.is_none());
    }

    #[test]
    fn payload_round_trips() {
        for p in [
            PayloadConfig::raw_hex("AABB"),
            PayloadConfig::Utf8 {
                text: "hello".to_string(),
            },
            PayloadConfig::Utf16 {
                text: "hi".to_string(),
                byte_order: ByteOrder::LittleEndian,
                bom: true,
            },
            PayloadConfig::Ascii {
                text: "x".to_string(),
                code_page: CodePage::Windows1252,
            },
            PayloadConfig::nmea("GP", "GGA", vec!["f".to_string()]),
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: PayloadConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back);
        }
    }
}
