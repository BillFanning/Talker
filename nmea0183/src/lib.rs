pub mod checksum;

mod error;
mod talker_id;
mod sentence_type;
mod sentence;
mod proprietary;

pub use error::NmeaError;
pub use talker_id::TalkerId;
pub use sentence_type::SentenceType;
pub use sentence::NmeaSentence;
pub use proprietary::{PashrData, PrdidData, ProprietarySentence};

/// The result of parsing any NMEA sentence.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub enum AnyNmeaSentence {
    Standard(NmeaSentence),
    Proprietary(ProprietarySentence),
}

/// Parse any NMEA sentence, dispatching to [`NmeaSentence`] or [`ProprietarySentence`]
/// based on whether the sentence begins with `$P`.
pub fn parse(line: &str) -> Result<AnyNmeaSentence, NmeaError> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let rest = trimmed.strip_prefix('$').ok_or(NmeaError::MissingLeadingDollar)?;

    if rest.starts_with('P') {
        ProprietarySentence::parse(line).map(AnyNmeaSentence::Proprietary)
    } else {
        NmeaSentence::parse(line).map(AnyNmeaSentence::Standard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_standard() {
        let wire = NmeaSentence::new(TalkerId::GP, SentenceType::GGA, vec![]).to_wire();
        assert!(matches!(parse(&wire).unwrap(), AnyNmeaSentence::Standard(_)));
    }

    #[test]
    fn dispatch_proprietary_prdid() {
        let wire = ProprietarySentence::Prdid(PrdidData { pitch: 1.0, roll: 2.0, heave: 3.0 }).to_wire();
        assert!(matches!(parse(&wire).unwrap(), AnyNmeaSentence::Proprietary(_)));
    }

    #[test]
    fn dispatch_proprietary_raw() {
        let wire = ProprietarySentence::Raw {
            identifier: "FOO".to_string(),
            fields: vec![],
        }.to_wire();
        assert!(matches!(parse(&wire).unwrap(), AnyNmeaSentence::Proprietary(_)));
    }

    #[test]
    fn dispatch_missing_dollar() {
        let err = parse("GPGGA*47").unwrap_err();
        assert!(matches!(err, NmeaError::MissingLeadingDollar));
    }
}
