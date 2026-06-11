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
//!   `BytePattern`, run against received bytes;
//! - **timer-based** ([`evaluate_idle`](MatchRuleSet::evaluate_idle)) for `Idle`,
//!   which fires once when the stream has been quiet for the timeout and re-arms
//!   when data resumes ([`note_activity`](MatchRuleSet::note_activity)).
//!
//! Evaluation is bounded (a linear scan of the rule list) and never stalls
//! reception (§100).

use std::time::Duration;

use crate::config::{MatchAction, MatchCondition, MatchRule};
use crate::core::MatchRuleId;

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

    /// Evaluate the `BytePattern` conditions against received bytes (§50.2).
    /// `Idle` rules are never matched here — they are timer-driven. Returns the
    /// rules that fired, in rule order.
    pub fn evaluate_message(&self, bytes: &[u8]) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        for rule in &self.rules {
            if rule.enabled && condition_matches_message(&rule.condition, bytes) {
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

/// Whether a data condition matches (`Idle` is never matched here).
fn condition_matches_message(condition: &MatchCondition, bytes: &[u8]) -> bool {
    match condition {
        MatchCondition::BytePattern { pattern } => contains_subslice(bytes, pattern),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HighlightStyle, MatchRule};

    fn rule(name: &str, condition: MatchCondition) -> MatchRule {
        MatchRule {
            name: name.to_string(),
            condition,
            actions: vec![MatchAction::Mark],
            enabled: true,
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
        assert_eq!(set.evaluate_message(b"$GPGGA,...").len(), 1);
        assert!(set.evaluate_message(b"$GPGLL,...").is_empty());
    }

    #[test]
    fn empty_or_oversized_byte_patterns_never_match() {
        assert!(!contains_subslice(b"abc", b""));
        assert!(!contains_subslice(b"ab", b"abc"));
        assert!(contains_subslice(b"abc", b"abc"));
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
        assert!(set.evaluate_message(b"X").is_empty());

        // Re-enable via the command path and it fires.
        let id = set.ids()[0];
        assert!(set.set_enabled(id, true));
        assert_eq!(set.evaluate_message(b"X").len(), 1);
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
        assert!(set.evaluate_message(b"anything").is_empty());
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
        let fired = set.evaluate_message(b"a hit here");
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
        let fired = set.evaluate_message(b"--AA--");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, set.ids()[0]);
        // Both fire when both patterns are present.
        assert_eq!(set.evaluate_message(b"AA..ZZ").len(), 2);
        // Neither matches when absent.
        assert!(set.evaluate_message(b"BBBB").is_empty());
    }
}
