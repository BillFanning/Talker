//! Profile schema, configuration load/save, validation, and templates.
//!
//! This is `listener-config` (§128). It owns the persisted [`Profile`] schema
//! (§67–§80), load/save, validation (§71), and starting templates (§81–§85). It
//! stores configuration only — never runtime objects (§5.7, §69). Mapping a
//! validated config to live transports/extractors is the runtime's job (§128),
//! not config's.

pub mod schema;
pub mod templates;

pub use schema::*;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::ChannelKind;
use crate::record::{is_filesystem_safe, FileRotationPolicy, RecordingMode};
use crate::transport::udp::UdpMode;

/// The schema version this build understands (§72.1). Listener keeps its own
/// version series, starting at 1.
pub const CURRENT_VERSION: u32 = 1;

fn current_version() -> u32 {
    CURRENT_VERSION
}

/// A persisted Listener workspace (§67, §72). A complete set of configured
/// Channels plus profile-level defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default = "current_version")]
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub defaults: DefaultConfig,
}

impl Profile {
    /// A new, empty profile stamped with the current schema version.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: CURRENT_VERSION,
            name: name.into(),
            channels: Vec::new(),
            defaults: DefaultConfig::default(),
        }
    }

    /// Parse a profile from TOML text, enforcing schema-version compatibility
    /// (§72.1).
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let profile: Profile = toml::from_str(text)?;
        profile.check_version()?;
        Ok(profile)
    }

    /// Serialize this profile to TOML text.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Load a profile from a file (§70). Does not start channels or touch
    /// interfaces — it only reads and validates the schema version.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::from_toml(&std::fs::read_to_string(path)?)
    }

    /// Save this profile to a file.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }

    /// Schema-version compatibility (§72.1): equal loads; newer is refused;
    /// older is refused (no migration exists for the v1 series yet).
    fn check_version(&self) -> Result<(), ConfigError> {
        use std::cmp::Ordering::*;
        match self.schema_version.cmp(&CURRENT_VERSION) {
            Equal => Ok(()),
            Greater => Err(ConfigError::SchemaTooNew {
                found: self.schema_version,
                supported: CURRENT_VERSION,
            }),
            Less => Err(ConfigError::SchemaTooOld {
                found: self.schema_version,
                supported: CURRENT_VERSION,
            }),
        }
    }

    /// Validate every channel without starting anything (§71). A channel's
    /// validity is reported independently — one invalid channel does not
    /// invalidate the others. Returns `(channel name, result)` per channel.
    pub fn validate(&self) -> Vec<(String, Result<(), Vec<ChannelConfigError>>)> {
        self.channels
            .iter()
            .map(|channel| {
                (
                    channel.name.as_str().to_string(),
                    validate_channel(channel, &self.defaults),
                )
            })
            .collect()
    }
}

/// Validate one channel's configuration (§71). Resource existence (e.g. whether
/// a COM port is present) is deferred to Start; this checks only structural
/// validity.
pub fn validate_channel(
    channel: &ChannelConfig,
    defaults: &DefaultConfig,
) -> Result<(), Vec<ChannelConfigError>> {
    let mut errors = Vec::new();

    // The Channel Kind must match its interface, and a TCP Connection is
    // runtime-only — it can never appear in a persisted profile (§16.3).
    let kind_matches = matches!(
        (channel.kind, &channel.interface),
        (ChannelKind::Serial, InterfaceConfig::Serial(_))
            | (ChannelKind::Udp, InterfaceConfig::Udp(_))
            | (ChannelKind::TcpListener, InterfaceConfig::TcpListener(_))
    );
    if channel.kind == ChannelKind::TcpConnection {
        errors.push(ChannelConfigError::TcpConnectionNotPersistable);
    } else if !kind_matches {
        errors.push(ChannelConfigError::KindInterfaceMismatch);
    }

    if let InterfaceConfig::Udp(udp) = &channel.interface {
        if udp.mode == UdpMode::Multicast
            && udp
                .multicast_group
                .as_deref()
                .is_none_or(|g| g.trim().is_empty())
        {
            errors.push(ChannelConfigError::MissingMulticastGroup);
        }
    }

    match &channel.extraction {
        ExtractionConfig::Delimiter { delimiter, .. } if delimiter.is_empty() => {
            errors.push(ChannelConfigError::EmptyDelimiter);
        }
        ExtractionConfig::FixedLength { length, .. } if *length == 0 => {
            errors.push(ChannelConfigError::ZeroFixedLength);
        }
        _ => {}
    }

    // When recording rotates, the channel name becomes part of generated filenames
    // (§59), so it must be filesystem-safe (§71). Non-rotating recordings use a
    // fixed destination path and do not constrain the name.
    if channel.recording.mode != RecordingMode::Disabled
        && channel.recording.file_rotation != FileRotationPolicy::None
        && !is_filesystem_safe(channel.name.as_str())
    {
        errors.push(ChannelConfigError::InvalidChannelName);
    }

    // A count-based subsample must pass at least one of every N (§50.1) — on any
    // sink: a Display View's history or the message-framed (`.ssdat`) recording.
    let display_subsample_invalid = channel
        .display
        .views
        .iter()
        .any(|v| matches!(v.subsample, Subsample::EveryNth { n: 0 }));
    let recording_subsample_invalid =
        matches!(channel.recording.subsample, Subsample::EveryNth { n: 0 });
    if display_subsample_invalid || recording_subsample_invalid {
        errors.push(ChannelConfigError::InvalidSubsample);
    }

    // Match Rules (§50.2, §165). A `BytePattern` with an empty pattern would match
    // every Message (an empty needle is always found) — reject it like an empty
    // delimiter. Decoded-field rules without a decoder are a non-fatal *warning*
    // (see `channel_warnings`): the rule simply never matches, it is not invalid.
    for rule in &channel.match_rules {
        if let MatchCondition::BytePattern { pattern } = &rule.condition {
            if pattern.is_empty() {
                errors.push(ChannelConfigError::EmptyMatchPattern);
            }
        }
    }

    // Retention must be bounded (§80): use the channel's own limits, or the
    // profile default if the channel sets none.
    let effective = if channel.retention.is_unbounded() {
        defaults
            .retention
            .clone()
            .unwrap_or_else(|| channel.retention.clone())
    } else {
        channel.retention.clone()
    };
    if effective.is_unbounded() {
        errors.push(ChannelConfigError::UnboundedRetention);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Non-fatal configuration warnings (§71): the channel is still valid and will
/// run, but something is likely a mistake. Surfaced to the user (e.g. printed at
/// profile load) without skipping the channel.
pub fn channel_warnings(channel: &ChannelConfig) -> Vec<ChannelConfigWarning> {
    let mut warnings = Vec::new();

    // A decoded-field Match Rule needs a decoder to have anything to match (§50.2):
    // with `DecoderConfig::None` the rule can never fire. Warn rather than reject.
    let has_decoder = !matches!(channel.decoder, DecoderConfig::None);
    if !has_decoder {
        for rule in &channel.match_rules {
            if matches!(rule.condition, MatchCondition::DecodedField { .. }) {
                warnings.push(ChannelConfigWarning::DecodedMatchWithoutDecoder {
                    rule: rule.name.clone(),
                });
            }
        }
    }

    warnings
}

/// Errors from loading or saving a profile (§72.1).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("profile parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("profile serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("profile schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("profile schema version {found} is older than {supported} and has no migration")]
    SchemaTooOld { found: u32, supported: u32 },
}

/// A structural problem in one channel's configuration (§71).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelConfigError {
    #[error("channel kind does not match its interface configuration")]
    KindInterfaceMismatch,
    #[error("TCP Connection channels are runtime-only and cannot be persisted")]
    TcpConnectionNotPersistable,
    #[error("multicast UDP requires a multicast group address")]
    MissingMulticastGroup,
    #[error("delimiter extraction has an empty delimiter")]
    EmptyDelimiter,
    #[error("fixed-length extraction has a length of zero")]
    ZeroFixedLength,
    #[error("retention is unbounded: set a limit on the channel or in defaults")]
    UnboundedRetention,
    #[error(
        "channel name is not filesystem-safe but recording file rotation uses it in filenames (§59)"
    )]
    InvalidChannelName,
    #[error("count-based subsampling requires n >= 1 (§50.1)")]
    InvalidSubsample,
    #[error("a match rule's byte pattern is empty (it would match every message, §50.2)")]
    EmptyMatchPattern,
}

/// A non-fatal configuration warning (§71): the channel runs, but this is likely
/// not what the user intended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelConfigWarning {
    #[error(
        "match rule \"{rule}\" tests a decoded field but the channel has no decoder; \
         it will never match (§50.2)"
    )]
    DecodedMatchWithoutDecoder { rule: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_round_trip_through_toml() {
        let mut profile = Profile::new("test workspace");
        profile.channels = vec![
            templates::serial_template(),
            templates::nmea_serial_template(),
            templates::udp_template(),
            templates::tcp_listener_template(),
        ];
        let toml = profile.to_toml().expect("serialize");
        let parsed = Profile::from_toml(&toml).expect("round trip");
        assert_eq!(parsed, profile);
    }

    #[test]
    fn rotation_requires_a_filesystem_safe_channel_name() {
        use crate::core::ChannelName;

        let mut channel = templates::udp_template();
        channel.recording.mode = RecordingMode::Raw;
        channel.recording.file_rotation = FileRotationPolicy::Hourly;
        channel.recording.destination = Some(std::path::PathBuf::from("."));
        let defaults = DefaultConfig::default();

        // Rotation + unsafe name → rejected at config time (§59/§71).
        channel.name = ChannelName::new("GPS/AIS");
        let errs = validate_channel(&channel, &defaults).unwrap_err();
        assert!(errs.contains(&ChannelConfigError::InvalidChannelName));

        // Rotation + safe name → ok.
        channel.name = ChannelName::new("GPS");
        assert!(validate_channel(&channel, &defaults).is_ok());

        // No rotation → the name is not constrained (it is not used in filenames).
        channel.name = ChannelName::new("GPS/AIS");
        channel.recording.file_rotation = FileRotationPolicy::None;
        match validate_channel(&channel, &defaults) {
            Ok(()) => {}
            Err(e) => assert!(!e.contains(&ChannelConfigError::InvalidChannelName)),
        }
    }

    #[test]
    fn count_subsampling_requires_positive_n() {
        let mut channel = templates::udp_template();
        let defaults = DefaultConfig::default();

        // A Display View sink with n = 0 is rejected.
        channel.display.views[0].subsample = Subsample::EveryNth { n: 0 };
        assert!(validate_channel(&channel, &defaults)
            .unwrap_err()
            .contains(&ChannelConfigError::InvalidSubsample));

        channel.display.views[0].subsample = Subsample::EveryNth { n: 5 };
        assert!(validate_channel(&channel, &defaults).is_ok());

        // The recording (`.ssdat`) sink is validated too (§50.1) — not just views.
        channel.recording.subsample = Subsample::EveryNth { n: 0 };
        assert!(validate_channel(&channel, &defaults)
            .unwrap_err()
            .contains(&ChannelConfigError::InvalidSubsample));

        channel.recording.subsample = Subsample::EveryNth { n: 3 };
        assert!(validate_channel(&channel, &defaults).is_ok());
    }

    #[test]
    fn empty_byte_pattern_match_rule_is_rejected() {
        let mut channel = templates::udp_template();
        channel.match_rules = vec![MatchRule {
            name: "everything".to_string(),
            condition: MatchCondition::BytePattern { pattern: vec![] },
            actions: vec![MatchAction::Mark],
            enabled: true,
        }];
        let errs = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(errs.contains(&ChannelConfigError::EmptyMatchPattern));

        // A non-empty pattern validates.
        channel.match_rules[0].condition = MatchCondition::BytePattern {
            pattern: b"GGA".to_vec(),
        };
        assert!(validate_channel(&channel, &DefaultConfig::default()).is_ok());
    }

    #[test]
    fn decoded_field_rule_without_a_decoder_warns_but_stays_valid() {
        let mut channel = templates::udp_template(); // DecoderConfig::None
        channel.match_rules = vec![MatchRule {
            name: "bad checksums".to_string(),
            condition: MatchCondition::DecodedField {
                field: DecodedMatch::Integrity {
                    status: crate::core::IntegrityStatus::Invalid,
                },
            },
            actions: vec![MatchAction::Notify {
                severity: crate::diagnostics::DiagnosticSeverity::Warning,
            }],
            enabled: true,
        }];

        // Non-fatal: the channel is still valid (it runs; the rule just never fires).
        assert!(validate_channel(&channel, &DefaultConfig::default()).is_ok());
        let warnings = channel_warnings(&channel);
        assert_eq!(
            warnings,
            vec![ChannelConfigWarning::DecodedMatchWithoutDecoder {
                rule: "bad checksums".to_string()
            }]
        );

        // With a decoder configured, there is no warning.
        channel.decoder = DecoderConfig::Nmea0183 {
            validation_mode: crate::decode::NmeaValidationMode::Standard,
        };
        assert!(channel_warnings(&channel).is_empty());
    }

    #[test]
    fn match_rules_round_trip_through_toml() {
        let mut profile = Profile::new("rules");
        let mut channel = templates::nmea_serial_template();
        channel.match_rules = vec![
            MatchRule {
                name: "GGA highlight".to_string(),
                condition: MatchCondition::DecodedField {
                    field: DecodedMatch::MessageType {
                        value: "GGA".to_string(),
                    },
                },
                actions: vec![MatchAction::Highlight {
                    style: HighlightStyle {
                        background: Some("yellow".to_string()),
                        ..HighlightStyle::default()
                    },
                }],
                enabled: true,
            },
            MatchRule {
                name: "go quiet".to_string(),
                condition: MatchCondition::Idle { timeout_ms: 5_000 },
                actions: vec![
                    MatchAction::Notify {
                        severity: crate::diagnostics::DiagnosticSeverity::Warning,
                    },
                    MatchAction::Record {
                        target: RecordTarget::Both,
                        control: RecordControl::Begin,
                    },
                    MatchAction::PauseDisplay { view: Some(0) },
                ],
                enabled: false,
            },
        ];
        profile.channels = vec![channel];
        let toml = profile.to_toml().expect("serialize");
        let parsed = Profile::from_toml(&toml).expect("round trip");
        assert_eq!(parsed, profile);
    }

    #[test]
    fn templates_are_valid() {
        let mut profile = Profile::new("templates");
        profile.channels = vec![
            templates::serial_template(),
            templates::nmea_serial_template(),
            templates::udp_template(),
            templates::tcp_listener_template(),
        ];
        for (name, result) in profile.validate() {
            assert!(result.is_ok(), "{name} should be valid: {result:?}");
        }
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let toml = format!("schema_version = {}\nname = \"x\"\n", CURRENT_VERSION + 1);
        assert!(matches!(
            Profile::from_toml(&toml),
            Err(ConfigError::SchemaTooNew { .. })
        ));
    }

    #[test]
    fn older_schema_version_is_refused() {
        let toml = "schema_version = 0\nname = \"x\"\n";
        assert!(matches!(
            Profile::from_toml(toml),
            Err(ConfigError::SchemaTooOld { .. })
        ));
    }

    #[test]
    fn missing_additive_fields_default_in() {
        // Only the required fields are present; everything else defaults (§72.1).
        let toml = "name = \"minimal\"\n";
        let profile = Profile::from_toml(toml).expect("defaults fill in");
        assert_eq!(profile.schema_version, CURRENT_VERSION);
        assert!(profile.channels.is_empty());
        assert_eq!(profile.defaults, DefaultConfig::default());
    }

    #[test]
    fn unbounded_retention_is_rejected() {
        let mut channel = templates::udp_template();
        channel.retention = RetentionConfig::default(); // all None
        let err = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(err.contains(&ChannelConfigError::UnboundedRetention));
    }

    #[test]
    fn profile_default_retention_satisfies_a_channel_without_one() {
        let mut channel = templates::udp_template();
        channel.retention = RetentionConfig::default(); // all None
        let defaults = DefaultConfig {
            retention: Some(RetentionConfig::with_message_limit(500)),
            ..DefaultConfig::default()
        };
        assert!(validate_channel(&channel, &defaults).is_ok());
    }

    #[test]
    fn kind_interface_mismatch_is_caught() {
        let mut channel = templates::udp_template();
        channel.kind = ChannelKind::Serial; // interface is still UDP
        let err = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(err.contains(&ChannelConfigError::KindInterfaceMismatch));
    }

    #[test]
    fn profiles_never_carry_tcp_connection_channels() {
        let mut channel = templates::tcp_listener_template();
        channel.kind = ChannelKind::TcpConnection;
        let err = validate_channel(&channel, &DefaultConfig::default()).unwrap_err();
        assert!(err.contains(&ChannelConfigError::TcpConnectionNotPersistable));
    }
}
