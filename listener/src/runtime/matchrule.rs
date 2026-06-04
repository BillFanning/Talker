//! Match Rule evaluation (spec §50.2, §165).
//!
//! A [`MatchRuleSet`] is the runtime form of a Channel's configured
//! [`MatchRule`]s: each rule is compiled once (minting a [`MatchRuleId`] so events
//! and commands can name it) and then evaluated against received data. Evaluation
//! is **pure and side-effect-free** — it reports *which* rules fired and *what
//! actions* they carry; the pipeline owns applying those actions (recording,
//! display, diagnostics, events). Rules never modify Messages, bytes, recordings,
//! or metadata (§40, §103, §116).
//!
//! Two evaluation paths, per §50.2:
//! - **per-Message** ([`evaluate_message`](MatchRuleSet::evaluate_message)) for
//!   `BytePattern`, `DecodedField`, and `MessageSize`, run after decoding;
//! - **timer-based** ([`evaluate_idle`](MatchRuleSet::evaluate_idle)) for `Idle`,
//!   which fires once when the stream has been quiet for the timeout and re-arms
//!   when data resumes ([`note_activity`](MatchRuleSet::note_activity)).
//!
//! Evaluation is bounded (a linear scan of the rule list) and never stalls
//! reception (§100).

use std::time::Duration;

use crate::config::{DecodedMatch, MatchAction, MatchCondition, MatchRule};
use crate::core::{IntegrityStatus, MatchRuleId, ProtocolMetadata};

/// One configured rule in runtime form: its minted id, condition, actions, the
/// enabled flag (live-toggleable via `SetMatchRuleEnabled`, §136), and the small
/// bit of state the `Idle` condition needs to fire exactly once per quiet episode.
struct CompiledRule {
    id: MatchRuleId,
    condition: MatchCondition,
    actions: Vec<MatchAction>,
    enabled: bool,
    /// For an `Idle` rule: whether it has already fired during the current quiet
    /// episode (reset by `note_activity` when data resumes). Unused otherwise.
    idle_fired: bool,
}

/// A rule that fired, with the actions the pipeline should apply. Actions are
/// cloned (only for the rare matched rule) so the caller can apply them while
/// holding `&mut` to the rest of the pipeline.
#[derive(Clone, Debug, PartialEq)]
pub struct FiredRule {
    pub id: MatchRuleId,
    pub actions: Vec<MatchAction>,
}

/// A Channel's compiled Match Rules (§50.2). Owned by the pipeline.
pub struct MatchRuleSet {
    rules: Vec<CompiledRule>,
}

impl MatchRuleSet {
    /// Compile a Channel's configured rules, minting a fresh [`MatchRuleId`] for
    /// each (in config order).
    pub fn compile(rules: &[MatchRule]) -> Self {
        let rules = rules
            .iter()
            .map(|r| CompiledRule {
                id: MatchRuleId::new(),
                condition: r.condition.clone(),
                actions: r.actions.clone(),
                enabled: r.enabled,
                idle_fired: false,
            })
            .collect();
        Self { rules }
    }

    /// True when there are no rules at all (lets the pipeline skip evaluation).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// True when at least one enabled rule uses the `Idle` condition, so the
    /// pipeline knows whether to run an idle timer at all.
    pub fn has_idle_rule(&self) -> bool {
        self.rules
            .iter()
            .any(|r| r.enabled && matches!(r.condition, MatchCondition::Idle { .. }))
    }

    /// The (id, name-less) ids of every rule, in order, for command routing
    /// (`SetMatchRuleEnabled`). The name lives in config; the id is the runtime key.
    pub fn ids(&self) -> Vec<MatchRuleId> {
        self.rules.iter().map(|r| r.id).collect()
    }

    /// Enable or disable a rule by id (§136 `SetMatchRuleEnabled`). Returns whether
    /// a rule with that id existed.
    pub fn set_enabled(&mut self, id: MatchRuleId, enabled: bool) -> bool {
        if let Some(rule) = self.rules.iter_mut().find(|r| r.id == id) {
            rule.enabled = enabled;
            true
        } else {
            false
        }
    }

    /// Evaluate the per-Message conditions against one decoded Message (§50.2):
    /// `BytePattern`, `DecodedField`, `MessageSize`. `Idle` rules are never matched
    /// here — they are timer-driven. Returns the rules that fired, in rule order.
    pub fn evaluate_message(
        &self,
        bytes: &[u8],
        protocol: Option<&ProtocolMetadata>,
    ) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        for rule in &self.rules {
            if rule.enabled && condition_matches_message(&rule.condition, bytes, protocol) {
                fired.push(FiredRule {
                    id: rule.id,
                    actions: rule.actions.clone(),
                });
            }
        }
        fired
    }

    /// Evaluate the `Idle` condition against the current quiet duration (§50.2). An
    /// idle rule fires once when `idle_for` reaches its timeout and is then latched
    /// until [`note_activity`](Self::note_activity) re-arms it. Returns the rules
    /// that fired this tick.
    pub fn evaluate_idle(&mut self, idle_for: Duration) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        for rule in &mut self.rules {
            let MatchCondition::Idle { timeout_ms } = rule.condition else {
                continue;
            };
            if !rule.enabled || rule.idle_fired {
                continue;
            }
            if idle_for >= Duration::from_millis(timeout_ms) {
                rule.idle_fired = true;
                fired.push(FiredRule {
                    id: rule.id,
                    actions: rule.actions.clone(),
                });
            }
        }
        fired
    }

    /// Data resumed: re-arm every `Idle` rule so it can fire again on the next
    /// quiet episode (§50.2).
    pub fn note_activity(&mut self) {
        for rule in &mut self.rules {
            rule.idle_fired = false;
        }
    }
}

/// Whether a per-Message condition matches (`Idle` is never matched here).
fn condition_matches_message(
    condition: &MatchCondition,
    bytes: &[u8],
    protocol: Option<&ProtocolMetadata>,
) -> bool {
    match condition {
        MatchCondition::BytePattern { pattern } => contains_subslice(bytes, pattern),
        MatchCondition::MessageSize { min, max } => message_size_matches(bytes.len(), *min, *max),
        MatchCondition::DecodedField { field } => {
            protocol.is_some_and(|meta| decoded_field_matches(field, meta))
        }
        MatchCondition::Idle { .. } => false,
    }
}

/// Substring search: is `needle` contained in `haystack`? An empty needle is
/// rejected at config time (§71), but treat it as "no match" defensively.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A Message of `len` bytes matches when its size is **outside** `[min, max]`
/// (§50.2). An unset bound does not constrain that side; both unset never matches.
fn message_size_matches(len: usize, min: Option<usize>, max: Option<usize>) -> bool {
    min.is_some_and(|m| len < m) || max.is_some_and(|m| len > m)
}

/// Whether a decoded-field predicate matches the Message's Protocol Metadata
/// (§134/§135). Limited to existing metadata — no field extraction (Appendix A).
fn decoded_field_matches(field: &DecodedMatch, meta: &ProtocolMetadata) -> bool {
    match field {
        DecodedMatch::MessageType { value } => meta.message_type.as_deref() == Some(value.as_str()),
        DecodedMatch::TalkerId { value } => {
            meta.attributes.get("talker_id").map(String::as_str) == Some(value.as_str())
        }
        DecodedMatch::Integrity { status } => integrity_matches(meta, *status),
    }
}

/// Whether any integrity field on the Message reports `status` (§28, §135).
fn integrity_matches(meta: &ProtocolMetadata, status: IntegrityStatus) -> bool {
    meta.integrity.iter().any(|i| i.status == status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HighlightStyle, MatchRule};
    use crate::core::metadata::{IntegrityMetadata, IntegrityScope, ProtocolId};
    use std::collections::BTreeMap;

    fn rule(name: &str, condition: MatchCondition) -> MatchRule {
        MatchRule {
            name: name.to_string(),
            condition,
            actions: vec![MatchAction::Mark],
            enabled: true,
        }
    }

    fn nmea_meta(
        message_type: Option<&str>,
        talker: Option<&str>,
        status: IntegrityStatus,
    ) -> ProtocolMetadata {
        let mut attributes = BTreeMap::new();
        if let Some(t) = talker {
            attributes.insert("talker_id".to_string(), t.to_string());
        }
        ProtocolMetadata {
            protocol: ProtocolId::Nmea0183,
            message_type: message_type.map(str::to_string),
            integrity: vec![IntegrityMetadata {
                scope: IntegrityScope::Protocol,
                status,
                algorithm: Some("NMEA XOR".to_string()),
            }],
            attributes,
        }
    }

    #[test]
    fn byte_pattern_matches_a_substring_within_a_message() {
        let set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        assert_eq!(set.evaluate_message(b"$GPGGA,...", None).len(), 1);
        assert!(set.evaluate_message(b"$GPGLL,...", None).is_empty());
    }

    #[test]
    fn empty_or_oversized_byte_patterns_never_match() {
        assert!(!contains_subslice(b"abc", b""));
        assert!(!contains_subslice(b"ab", b"abc"));
        assert!(contains_subslice(b"abc", b"abc"));
    }

    #[test]
    fn message_size_matches_outside_the_band() {
        // Outside [10, 80]: too short or too long.
        assert!(message_size_matches(9, Some(10), Some(80)));
        assert!(message_size_matches(81, Some(10), Some(80)));
        assert!(!message_size_matches(10, Some(10), Some(80)));
        assert!(!message_size_matches(80, Some(10), Some(80)));
        // One-sided bounds constrain only their side.
        assert!(message_size_matches(5, Some(10), None));
        assert!(!message_size_matches(500, Some(10), None));
        assert!(message_size_matches(500, None, Some(80)));
        // Both unset: nothing is outside an unbounded range.
        assert!(!message_size_matches(0, None, None));
    }

    #[test]
    fn decoded_field_matches_type_talker_and_integrity() {
        let meta = nmea_meta(Some("GLL"), Some("GP"), IntegrityStatus::Invalid);

        let by_type = MatchRuleSet::compile(&[rule(
            "t",
            MatchCondition::DecodedField {
                field: DecodedMatch::MessageType {
                    value: "GLL".to_string(),
                },
            },
        )]);
        assert_eq!(by_type.evaluate_message(b"x", Some(&meta)).len(), 1);

        let by_talker = MatchRuleSet::compile(&[rule(
            "tk",
            MatchCondition::DecodedField {
                field: DecodedMatch::TalkerId {
                    value: "GP".to_string(),
                },
            },
        )]);
        assert_eq!(by_talker.evaluate_message(b"x", Some(&meta)).len(), 1);

        let bad_checksum = MatchRuleSet::compile(&[rule(
            "bad",
            MatchCondition::DecodedField {
                field: DecodedMatch::Integrity {
                    status: IntegrityStatus::Invalid,
                },
            },
        )]);
        assert_eq!(bad_checksum.evaluate_message(b"x", Some(&meta)).len(), 1);
    }

    #[test]
    fn decoded_field_never_matches_without_metadata() {
        let set = MatchRuleSet::compile(&[rule(
            "t",
            MatchCondition::DecodedField {
                field: DecodedMatch::MessageType {
                    value: "GLL".to_string(),
                },
            },
        )]);
        // No decoder → no Protocol Metadata → the rule cannot fire (§50.2).
        assert!(set.evaluate_message(b"$GPGLL", None).is_empty());
    }

    #[test]
    fn disabled_rules_do_not_fire() {
        let mut config = rule(
            "p",
            MatchCondition::BytePattern {
                pattern: b"X".to_vec(),
            },
        );
        config.enabled = false;
        let mut set = MatchRuleSet::compile(&[config]);
        assert!(set.evaluate_message(b"X", None).is_empty());

        // Re-enable via the command path and it fires.
        let id = set.ids()[0];
        assert!(set.set_enabled(id, true));
        assert_eq!(set.evaluate_message(b"X", None).len(), 1);
    }

    #[test]
    fn idle_fires_once_per_quiet_episode_and_rearms_on_activity() {
        let mut set =
            MatchRuleSet::compile(&[rule("idle", MatchCondition::Idle { timeout_ms: 500 })]);
        assert!(set.has_idle_rule());

        // Below the timeout: nothing fires.
        assert!(set.evaluate_idle(Duration::from_millis(300)).is_empty());
        // At/over the timeout: fires once.
        assert_eq!(set.evaluate_idle(Duration::from_millis(600)).len(), 1);
        // Still quiet, already fired: latched, no repeat.
        assert!(set.evaluate_idle(Duration::from_millis(900)).is_empty());
        // Data resumes, then quiet again: it can fire again.
        set.note_activity();
        assert_eq!(set.evaluate_idle(Duration::from_millis(600)).len(), 1);
    }

    #[test]
    fn idle_is_never_matched_on_the_per_message_path() {
        let set = MatchRuleSet::compile(&[rule("idle", MatchCondition::Idle { timeout_ms: 1 })]);
        assert!(set.evaluate_message(b"anything", None).is_empty());
    }

    #[test]
    fn fired_rule_carries_its_actions() {
        let mut config = rule(
            "p",
            MatchCondition::BytePattern {
                pattern: b"hit".to_vec(),
            },
        );
        config.actions = vec![
            MatchAction::Highlight {
                style: HighlightStyle::default(),
            },
            MatchAction::Mark,
        ];
        let set = MatchRuleSet::compile(&[config]);
        let fired = set.evaluate_message(b"a hit here", None);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].actions.len(), 2);
    }

    #[test]
    fn multiple_rules_each_evaluate_independently() {
        let set = MatchRuleSet::compile(&[
            rule(
                "a",
                MatchCondition::BytePattern {
                    pattern: b"AA".to_vec(),
                },
            ),
            rule(
                "b",
                MatchCondition::BytePattern {
                    pattern: b"ZZ".to_vec(),
                },
            ),
        ]);
        // Only rule "a" matches a message containing "AA" but not "ZZ".
        let fired = set.evaluate_message(b"--AA--", None);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, set.ids()[0]);
        // Both fire when both patterns are present.
        assert_eq!(set.evaluate_message(b"AA..ZZ", None).len(), 2);
        // Neither matches when absent.
        assert!(set.evaluate_message(b"BBBB", None).is_empty());
    }
}
