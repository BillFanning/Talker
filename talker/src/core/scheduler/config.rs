use serde::{Deserialize, Serialize};

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ScheduleConfig {
    pub entries: Vec<ScheduleEntryConfig>,
}

impl ScheduleConfig {
    pub fn new(entries: Vec<ScheduleEntryConfig>) -> Self {
        Self { entries }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEntryConfig {
    pub payload: PayloadConfig,
    pub interval_ms: u64,
}

impl ScheduleEntryConfig {
    pub fn new(payload: PayloadConfig, interval_ms: u64) -> Self {
        Self {
            payload,
            interval_ms,
        }
    }
}

/// The payload source for one schedule entry.
///
/// `compile()` converts this to a wire-ready `Vec<u8>`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PayloadConfig {
    /// Raw bytes as a hex string (spaces and hyphens are stripped).
    /// Example: `"DE AD BE EF"` or `"DEADBEEF"`.
    RawHex { data: String },
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

    /// Compile this config to the raw bytes that will be put on the wire.
    pub fn compile(&self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::RawHex { data } => compile_hex(data),
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

    // ── PayloadConfig::compile — RawHex ──────────────────────────────────────

    #[test]
    fn compile_raw_hex_basic() {
        let p = PayloadConfig::raw_hex("DEADBEEF");
        assert_eq!(p.compile().unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn compile_raw_hex_with_spaces() {
        let p = PayloadConfig::raw_hex("DE AD BE EF");
        assert_eq!(p.compile().unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn compile_raw_hex_with_hyphens() {
        let p = PayloadConfig::raw_hex("DE-AD-BE-EF");
        assert_eq!(p.compile().unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn compile_raw_hex_single_byte() {
        assert_eq!(PayloadConfig::raw_hex("FF").compile().unwrap(), vec![0xFF]);
    }

    #[test]
    fn compile_raw_hex_empty() {
        assert_eq!(
            PayloadConfig::raw_hex("").compile().unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn compile_raw_hex_odd_length_returns_error() {
        let err = PayloadConfig::raw_hex("DEA").compile().unwrap_err();
        assert!(err.to_string().contains("odd length"));
    }

    #[test]
    fn compile_raw_hex_invalid_chars_returns_error() {
        let err = PayloadConfig::raw_hex("DEXZ").compile().unwrap_err();
        assert!(err.to_string().contains("invalid hex byte"));
    }

    // ── PayloadConfig::compile — Nmea ─────────────────────────────────────────

    #[test]
    fn compile_nmea_produces_valid_wire_format() {
        let p = PayloadConfig::nmea("GP", "GGA", vec!["123519".to_string()]);
        let bytes = p.compile().unwrap();
        let wire = std::str::from_utf8(&bytes).unwrap();
        assert!(wire.starts_with("$GPGGA,123519*"));
        assert!(wire.ends_with("\r\n"));
    }

    #[test]
    fn compile_nmea_custom_talker_and_type() {
        let p = PayloadConfig::nmea("GL", "RMC", vec!["field".to_string()]);
        let bytes = p.compile().unwrap();
        let wire = std::str::from_utf8(&bytes).unwrap();
        assert!(wire.starts_with("$GLRMC,field*"));
    }

    #[test]
    fn compile_nmea_no_fields() {
        let p = PayloadConfig::nmea("GN", "GLL", vec![]);
        let bytes = p.compile().unwrap();
        let wire = std::str::from_utf8(&bytes).unwrap();
        assert!(wire.starts_with("$GNGLL*"));
    }

    // ── serde round-trips ─────────────────────────────────────────────────────

    #[test]
    fn payload_raw_hex_round_trip() {
        let p = PayloadConfig::raw_hex("AABB");
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"type\":\"raw_hex\""));
        let back: PayloadConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn payload_nmea_round_trip() {
        let p = PayloadConfig::nmea("GP", "GGA", vec!["f1".to_string()]);
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"type\":\"nmea\""));
        let back: PayloadConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn schedule_config_round_trip() {
        let c = ScheduleConfig::new(vec![
            ScheduleEntryConfig::new(PayloadConfig::raw_hex("FF"), 500),
            ScheduleEntryConfig::new(PayloadConfig::nmea("GP", "RMC", vec![]), 1000),
        ]);
        let json = serde_json::to_string(&c).unwrap();
        let back: ScheduleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }
}
