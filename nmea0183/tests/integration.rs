use nmea0183::{
    parse, AnyNmeaSentence, NmeaError, NmeaSentence, PashrData, PrdidData, ProprietarySentence,
    SentenceType, TalkerId,
};

// ── Checksum ──────────────────────────────────────────────────────────────────

#[test]
fn checksum_known_gga() {
    let s = NmeaSentence::parse(
        "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n",
    )
    .unwrap();
    assert_eq!(s.checksum(), 0x47);
}

#[test]
fn checksum_known_rmc() {
    let s = NmeaSentence::parse(
        "$GPRMC,220516,A,5133.82,N,00042.24,W,173.8,231.8,130694,004.2,W*70\r\n",
    )
    .unwrap();
    assert_eq!(s.checksum(), 0x70);
}

// ── Standard sentence construction ───────────────────────────────────────────

#[test]
fn construct_and_parse_all_gnss_talkers() {
    let talkers = [
        TalkerId::GP,
        TalkerId::GL,
        TalkerId::GN,
        TalkerId::GA,
        TalkerId::GB,
        TalkerId::GI,
        TalkerId::GQ,
    ];
    for talker in talkers {
        let s = NmeaSentence::new(
            talker.clone(),
            SentenceType::GGA,
            vec!["123519".to_string(), "4807.038".to_string()],
        );
        let wire = s.to_wire();
        assert!(wire.starts_with(&format!("${}GGA,", talker.as_str())));
        let parsed = NmeaSentence::parse(&wire).unwrap();
        assert_eq!(parsed.talker_id, talker);
        assert_eq!(parsed.sentence_type, SentenceType::GGA);
    }
}

#[test]
fn construct_and_parse_representative_sentence_types() {
    let types = [
        SentenceType::GGA,
        SentenceType::GLL,
        SentenceType::GSA,
        SentenceType::GSV,
        SentenceType::RMC,
        SentenceType::VTG,
        SentenceType::ZDA,
        SentenceType::HDT,
        SentenceType::HDG,
        SentenceType::VHW,
        SentenceType::DBT,
        SentenceType::DPT,
        SentenceType::MTW,
        SentenceType::MWV,
        SentenceType::XDR,
        SentenceType::APB,
        SentenceType::BOD,
        SentenceType::BWC,
        SentenceType::RMB,
        SentenceType::RTE,
        SentenceType::WPL,
        SentenceType::XTE,
        SentenceType::TTM,
        SentenceType::VDM,
        SentenceType::VDO,
        SentenceType::TXT,
        SentenceType::ROT,
        SentenceType::VBW,
        SentenceType::VLW,
        SentenceType::GNS,
        SentenceType::GBS,
        SentenceType::GST,
        SentenceType::DTM,
        SentenceType::THS,
    ];
    for sentence_type in types {
        let s = NmeaSentence::new(
            TalkerId::GP,
            sentence_type.clone(),
            vec!["field1".to_string(), "field2".to_string()],
        );
        let wire = s.to_wire();
        let parsed = NmeaSentence::parse(&wire).unwrap();
        assert_eq!(parsed.sentence_type, sentence_type);
        assert_eq!(parsed.field(0), Some("field1"));
        assert_eq!(parsed.field(1), Some("field2"));
    }
}

#[test]
fn empty_field_list_round_trip() {
    let s = NmeaSentence::new(TalkerId::GP, SentenceType::GLL, vec![]);
    let wire = s.to_wire();
    let parsed = NmeaSentence::parse(&wire).unwrap();
    assert!(parsed.fields.is_empty());
}

#[test]
fn empty_fields_within_sentence_preserved() {
    let fields: Vec<String> = "123519,,N,,,E,,,,,M,,,"
        .split(',')
        .map(str::to_string)
        .collect();
    let s = NmeaSentence::new(TalkerId::GP, SentenceType::GGA, fields);
    let wire = s.to_wire();
    let parsed = NmeaSentence::parse(&wire).unwrap();
    assert_eq!(parsed.field(1), Some(""));
    assert_eq!(parsed.field(2), Some("N"));
}

#[test]
fn custom_talker_and_sentence_type_round_trip() {
    let s = NmeaSentence::new(
        TalkerId::Custom("HQ".to_string()),
        SentenceType::Custom("XYZ".to_string()),
        vec!["abc".to_string()],
    );
    let wire = s.to_wire();
    assert!(wire.starts_with("$HQXYZ,abc*"));
    let parsed = NmeaSentence::parse(&wire).unwrap();
    assert_eq!(parsed.talker_id, TalkerId::Custom("HQ".to_string()));
    assert_eq!(
        parsed.sentence_type,
        SentenceType::Custom("XYZ".to_string())
    );
}

// ── Parse error cases ─────────────────────────────────────────────────────────

#[test]
fn parse_rejects_bad_checksum() {
    let err = NmeaSentence::parse(
        "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*48\r\n",
    )
    .unwrap_err();
    assert!(matches!(
        err,
        NmeaError::InvalidChecksum {
            expected: 0x48,
            computed: 0x47
        }
    ));
}

#[test]
fn parse_rejects_missing_checksum() {
    let err = NmeaSentence::parse("$GPGGA,123519").unwrap_err();
    assert!(matches!(err, NmeaError::MissingChecksum));
}

#[test]
fn parse_rejects_missing_dollar() {
    let err = NmeaSentence::parse("GPGGA,123519*47").unwrap_err();
    assert!(matches!(err, NmeaError::MissingLeadingDollar));
}

#[test]
fn parse_rejects_proprietary_prefix() {
    let err = NmeaSentence::parse("$PASHR,045.67,T*4F").unwrap_err();
    assert!(matches!(err, NmeaError::Parse(_)));
}

// ── Proprietary sentences ─────────────────────────────────────────────────────

#[test]
fn prdid_no_checksum_in_wire() {
    let wire = ProprietarySentence::Prdid(PrdidData::new(0.0, 0.0, 0.0)).to_wire();
    assert!(!wire.contains('*'));
}

#[test]
fn prdid_round_trip() {
    let d = PrdidData::new(12.34, -5.67, 0.01);
    let wire = ProprietarySentence::Prdid(d.clone()).to_wire();
    let parsed = ProprietarySentence::parse(&wire).unwrap();
    assert_eq!(parsed, ProprietarySentence::Prdid(d));
}

#[test]
fn pashr_round_trip_no_utc() {
    let d = PashrData::new(None, 359.99, -10.00, 5.00, -0.50, 0.100, 0.100, 0.500, 2);
    let wire = ProprietarySentence::Pashr(d.clone()).to_wire();
    let parsed = ProprietarySentence::parse(&wire).unwrap();
    assert_eq!(parsed, ProprietarySentence::Pashr(d));
}

#[test]
fn pashr_round_trip_with_utc() {
    let d = PashrData::new(
        Some("235959.99".to_string()),
        0.00,
        0.00,
        0.00,
        0.00,
        0.001,
        0.001,
        0.010,
        0,
    );
    let wire = ProprietarySentence::Pashr(d.clone()).to_wire();
    let parsed = ProprietarySentence::parse(&wire).unwrap();
    assert_eq!(parsed, ProprietarySentence::Pashr(d));
}

#[test]
fn raw_proprietary_round_trip() {
    let s = ProprietarySentence::Raw {
        identifier: "GRMZ".to_string(),
        fields: vec!["93".to_string(), "f".to_string(), "3".to_string()],
    };
    let wire = s.to_wire();
    assert!(wire.starts_with("$PGRMZ,93,f,3*"));
    assert_eq!(ProprietarySentence::parse(&wire).unwrap(), s);
}

#[test]
fn raw_proprietary_no_fields() {
    let s = ProprietarySentence::Raw {
        identifier: "PING".to_string(),
        fields: vec![],
    };
    let wire = s.to_wire();
    assert!(wire.starts_with("$PPING*"));
    assert_eq!(ProprietarySentence::parse(&wire).unwrap(), s);
}

// ── Top-level parse dispatch ──────────────────────────────────────────────────

#[test]
fn top_level_parse_routes_standard() {
    let wire = NmeaSentence::new(TalkerId::GP, SentenceType::RMC, vec![]).to_wire();
    assert!(matches!(
        parse(&wire).unwrap(),
        AnyNmeaSentence::Standard(_)
    ));
}

#[test]
fn top_level_parse_routes_prdid() {
    let wire = ProprietarySentence::Prdid(PrdidData::new(1.0, 2.0, 3.0)).to_wire();
    assert!(matches!(
        parse(&wire).unwrap(),
        AnyNmeaSentence::Proprietary(ProprietarySentence::Prdid(_))
    ));
}

#[test]
fn top_level_parse_routes_pashr() {
    let d = PashrData::new(None, 45.0, 1.0, 2.0, 0.0, 0.01, 0.01, 0.1, 1);
    let wire = ProprietarySentence::Pashr(d).to_wire();
    assert!(matches!(
        parse(&wire).unwrap(),
        AnyNmeaSentence::Proprietary(ProprietarySentence::Pashr(_))
    ));
}

#[test]
fn top_level_parse_routes_raw_proprietary() {
    let wire = ProprietarySentence::Raw {
        identifier: "FOO".to_string(),
        fields: vec!["bar".to_string()],
    }
    .to_wire();
    assert!(matches!(
        parse(&wire).unwrap(),
        AnyNmeaSentence::Proprietary(ProprietarySentence::Raw { .. })
    ));
}

#[test]
fn top_level_parse_missing_dollar() {
    assert!(matches!(
        parse("GPGGA*47").unwrap_err(),
        NmeaError::MissingLeadingDollar
    ));
}
