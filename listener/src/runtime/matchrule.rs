//! Match Rule evaluation (spec §50.2, §165).
//!
//! A [`MatchRuleSet`] is the runtime form of a Channel's configured
//! [`MatchRule`]s: each rule is compiled once (minting a [`MatchRuleId`] so events
//! and commands can name it) and then evaluated against received data. Evaluation
//! is **pure and side-effect-free** — it reports *which* rules fired and *what
//! actions* they carry; the pipeline owns applying those actions (recording,
//! display, diagnostics, events). Rules never modify the received bytes or the
//! recordings — the stream stays verbatim (§40, §103, §116).
//!
//! Two evaluation paths, per §50.2:
//! - **stream** ([`evaluate_stream`](MatchRuleSet::evaluate_stream)) for
//!   `BytePattern`, run against each received chunk;
//! - **timer-based** ([`evaluate_idle`](MatchRuleSet::evaluate_idle)) for `Idle`,
//!   which fires once when the stream has been quiet for the timeout and re-arms
//!   when data resumes ([`note_activity`](MatchRuleSet::note_activity)).
//!
//! Evaluation is bounded (a linear scan of the rule list) and never stalls
//! reception (§100).
//!
//! ## Cross-chunk matching
//!
//! Received bytes arrive in chunks whose boundaries track OS buffering, not
//! content (ADR-009): a pattern can be split across two reads (`"GG"` ends one
//! chunk, `"A"` begins the next). A naive per-chunk scan would miss it. So the set
//! keeps a small **carry** — the last `max_pattern_len - 1` bytes of the prior
//! chunk — and scans `carry ++ chunk`, reporting only matches whose **end** falls
//! in the new chunk (the carry's own interior was already scanned last time). A
//! match that *starts* inside the carry is a **boundary split**: it would have
//! been missed without the carry. Those are counted and flagged so the pipeline
//! can measure where, why, and how often splitting actually occurs (§50.2).

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
    /// How many of this rule's `BytePattern` matches were **boundary splits** —
    /// found only because the cross-chunk carry was scanned (the pattern straddled
    /// a read boundary). The measurement of how often splitting matters (§50.2).
    boundary_saves: u64,
}

/// A rule that fired, with the actions the pipeline should apply. Actions are
/// cloned (only for the rare matched rule) so the caller can apply them while
/// holding `&mut` to the rest of the pipeline.
#[derive(Clone, Debug, PartialEq)]
pub struct FiredRule {
    pub id: MatchRuleId,
    pub actions: Vec<MatchAction>,
    /// Absolute stream offset of the **first byte** of the match that fired
    /// (§50.2: byte offsets, not Message Numbers). `None` for an `Idle` firing,
    /// which is not tied to a data position.
    pub match_offset: Option<u64>,
    /// Whether this match was a **boundary split** — its first byte fell in the
    /// previous chunk and it completed in this one, so a per-chunk scan would have
    /// missed it. Always `false` for `Idle`. Drives the where/why/how-often
    /// measurement in the pipeline.
    pub boundary_split: bool,
}

/// A Channel's compiled Match Rules (§50.2). Owned by the pipeline.
pub struct MatchRuleSet {
    rules: Vec<CompiledRule>,
    /// Tail of the previous chunk kept so a pattern split across the boundary
    /// still matches (`max_pattern_len - 1` bytes). Empty until the first chunk.
    carry: Vec<u8>,
    /// Absolute stream offset of `carry[0]` — the position of the carry's first
    /// byte, so a match starting in the carry reports its true offset.
    carry_offset: u64,
    /// The largest enabled `BytePattern` length, recomputed when rules change. The
    /// carry never needs more than `max_pattern_len - 1` bytes.
    max_pattern_len: usize,
}

impl MatchRuleSet {
    /// Compile a Channel's configured rules, minting a fresh [`MatchRuleId`] for
    /// each (in config order).
    pub fn compile(rules: &[MatchRule]) -> Self {
        let rules: Vec<CompiledRule> = rules
            .iter()
            .map(|r| CompiledRule {
                id: MatchRuleId::new(),
                condition: r.condition.clone(),
                actions: r.actions.clone(),
                enabled: r.enabled,
                idle_fired: false,
                boundary_saves: 0,
            })
            .collect();
        let mut set = Self {
            rules,
            carry: Vec::new(),
            carry_offset: 0,
            max_pattern_len: 0,
        };
        set.recompute_max_pattern_len();
        set
    }

    /// The longest enabled `BytePattern` determines how much carry we must keep.
    /// Recomputed whenever the enabled set changes.
    fn recompute_max_pattern_len(&mut self) {
        self.max_pattern_len = self
            .rules
            .iter()
            .filter(|r| r.enabled)
            .filter_map(|r| match &r.condition {
                MatchCondition::BytePattern { pattern } => Some(pattern.len()),
                MatchCondition::Idle { .. } => None,
            })
            .max()
            .unwrap_or(0);
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
            self.recompute_max_pattern_len();
            true
        } else {
            false
        }
    }

    /// Total boundary-split saves across all rules — `BytePattern` matches found
    /// only because the cross-chunk carry was scanned. The "how often" of the
    /// where/why/how-often measurement (§50.2). Monotonic since Start.
    pub fn boundary_saves(&self) -> u64 {
        self.rules.iter().map(|r| r.boundary_saves).sum()
    }

    /// Evaluate the `BytePattern` conditions against one received chunk (§50.2),
    /// matching **across the previous chunk's boundary** via the retained carry.
    /// `chunk_offset` is the absolute stream offset of `chunk[0]`. `Idle` rules are
    /// never matched here — they are timer-driven. Returns the rules that fired, in
    /// rule order, each carrying its match's absolute start offset and whether it
    /// was a boundary split. The carry is updated for the next chunk.
    pub fn evaluate_stream(&mut self, chunk: &[u8], chunk_offset: u64) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        if chunk.is_empty() {
            return fired;
        }

        // Join the carry and this chunk so a pattern straddling the boundary is
        // visible as one contiguous slice. The carry holds bytes immediately
        // preceding this chunk, so the joined slice starts at `carry_offset`.
        let carry_len = self.carry.len();
        let joined: Vec<u8> = if carry_len == 0 {
            chunk.to_vec()
        } else {
            let mut j = Vec::with_capacity(carry_len + chunk.len());
            j.extend_from_slice(&self.carry);
            j.extend_from_slice(chunk);
            j
        };
        // Absolute offset of `joined[0]`: the carry's first byte if we have carry,
        // else this chunk's first byte.
        let joined_offset = if carry_len == 0 {
            chunk_offset
        } else {
            self.carry_offset
        };

        for rule in &mut self.rules {
            let MatchCondition::BytePattern { pattern } = &rule.condition else {
                continue;
            };
            if !rule.enabled || pattern.is_empty() || pattern.len() > joined.len() {
                continue;
            }
            // Find the first match whose **end** lands in the new chunk — i.e. the
            // match was not already fully contained in (and reported from) the
            // previous chunk. `end_in_joined > carry_len` means at least the last
            // pattern byte is in `chunk`.
            let mut start = 0;
            while let Some(rel) = find_subslice(&joined[start..], pattern) {
                let match_start = start + rel;
                let match_end = match_start + pattern.len(); // exclusive
                if match_end > carry_len {
                    let abs_start = joined_offset + match_start as u64;
                    // A boundary split: the match began inside the carry (the prior
                    // chunk) and only completes now. Per-chunk scanning would miss it.
                    let boundary_split = match_start < carry_len;
                    if boundary_split {
                        rule.boundary_saves += 1;
                    }
                    fired.push(FiredRule {
                        id: rule.id,
                        actions: rule.actions.clone(),
                        match_offset: Some(abs_start),
                        boundary_split,
                    });
                    break; // one firing per rule per chunk (mirrors prior behavior)
                }
                // This match ended within the carry (already reported last chunk);
                // keep scanning past its start for a later, in-chunk occurrence.
                start = match_start + 1;
            }
        }

        // Retain the tail of `chunk` as the next carry: enough for the longest
        // enabled pattern to straddle (`max_pattern_len - 1`). Computed from the
        // chunk's true end offset so `carry_offset` stays absolute.
        let want = self.max_pattern_len.saturating_sub(1);
        let chunk_end_offset = chunk_offset + chunk.len() as u64;
        if want == 0 || chunk.is_empty() {
            self.carry.clear();
            self.carry_offset = chunk_end_offset;
        } else {
            let keep = want.min(chunk.len());
            self.carry.clear();
            self.carry.extend_from_slice(&chunk[chunk.len() - keep..]);
            self.carry_offset = chunk_end_offset - keep as u64;
        }
        fired
    }

    /// Reset the cross-chunk carry (e.g. on Stop/Start). The next chunk starts a
    /// fresh stream with no straddle from before.
    pub fn reset_stream(&mut self) {
        self.carry.clear();
        self.carry_offset = 0;
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
                    match_offset: None,
                    boundary_split: false,
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

/// Substring search: the index of the first occurrence of `needle` in `haystack`,
/// or `None`. An empty needle is rejected at config time (§71), but treat it as
/// "no match" defensively.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
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
    fn byte_pattern_matches_a_substring_within_a_chunk() {
        let mut set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        let fired = set.evaluate_stream(b"$GPGGA,...", 0);
        assert_eq!(fired.len(), 1);
        // "GGA" begins at index 3 of the chunk; no carry → absolute offset 3.
        assert_eq!(fired[0].match_offset, Some(3));
        assert!(!fired[0].boundary_split);
        assert!(set.evaluate_stream(b"$GPGLL,...", 10).is_empty());
    }

    #[test]
    fn empty_or_oversized_byte_patterns_never_match() {
        assert_eq!(find_subslice(b"abc", b""), None);
        assert_eq!(find_subslice(b"ab", b"abc"), None);
        assert_eq!(find_subslice(b"abc", b"abc"), Some(0));
        assert_eq!(find_subslice(b"xxabc", b"abc"), Some(2));
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
        assert!(set.evaluate_stream(b"X", 0).is_empty());

        // Re-enable via the command path and it fires.
        let id = set.ids()[0];
        assert!(set.set_enabled(id, true));
        assert_eq!(set.evaluate_stream(b"X", 1).len(), 1);
    }

    #[test]
    fn byte_pattern_matches_across_a_chunk_boundary_and_is_counted() {
        let mut set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        // "GG" ends the first chunk, "A" begins the second: a per-chunk scan misses
        // it, but the carry catches it.
        assert!(set.evaluate_stream(b"$GPGG", 0).is_empty());
        let fired = set.evaluate_stream(b"A,123", 5);
        assert_eq!(fired.len(), 1);
        // The match starts at stream offset 3 ("GGA" begins inside chunk 1).
        assert_eq!(fired[0].match_offset, Some(3));
        assert!(fired[0].boundary_split, "match straddled the boundary");
        // Measurement: exactly one boundary save recorded.
        assert_eq!(set.boundary_saves(), 1);
    }

    #[test]
    fn a_match_is_not_double_counted_across_the_boundary() {
        // A pattern fully inside chunk 1 ending exactly at the boundary is reported
        // from chunk 1, and must not be re-reported when it reappears in the carry.
        let mut set = MatchRuleSet::compile(&[rule(
            "ab",
            MatchCondition::BytePattern {
                pattern: b"AB".to_vec(),
            },
        )]);
        // chunk 1 ends with "AB"; reported now at offset 2.
        let f1 = set.evaluate_stream(b"xxAB", 0);
        assert_eq!(f1.len(), 1);
        assert_eq!(f1[0].match_offset, Some(2));
        assert!(!f1[0].boundary_split);
        // chunk 2 has no new occurrence ending in it; the carried "B" must not
        // re-fire the earlier "AB".
        let f2 = set.evaluate_stream(b"cd", 4);
        assert!(f2.is_empty());
        assert_eq!(set.boundary_saves(), 0);
    }

    #[test]
    fn an_in_chunk_match_after_the_carry_is_not_a_boundary_split() {
        let mut set = MatchRuleSet::compile(&[rule(
            "zz",
            MatchCondition::BytePattern {
                pattern: b"ZZ".to_vec(),
            },
        )]);
        set.evaluate_stream(b"aaa", 0); // carry = "a"
        let fired = set.evaluate_stream(b"bZZc", 3);
        assert_eq!(fired.len(), 1);
        // "ZZ" begins at chunk index 1 → absolute offset 4; not a split.
        assert_eq!(fired[0].match_offset, Some(4));
        assert!(!fired[0].boundary_split);
        assert_eq!(set.boundary_saves(), 0);
    }

    #[test]
    fn reset_stream_clears_the_carry() {
        let mut set = MatchRuleSet::compile(&[rule(
            "gga",
            MatchCondition::BytePattern {
                pattern: b"GGA".to_vec(),
            },
        )]);
        assert!(set.evaluate_stream(b"$GPGG", 0).is_empty());
        set.reset_stream(); // Stop/Start: the straddle does not carry over.
        assert!(set.evaluate_stream(b"A,123", 0).is_empty());
        assert_eq!(set.boundary_saves(), 0);
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
    fn idle_is_never_matched_on_the_stream_path() {
        let mut set =
            MatchRuleSet::compile(&[rule("idle", MatchCondition::Idle { timeout_ms: 1 })]);
        assert!(set.evaluate_stream(b"anything", 0).is_empty());
        // An idle-only set keeps no carry (max_pattern_len is 0).
        assert_eq!(set.boundary_saves(), 0);
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
        let mut set = MatchRuleSet::compile(&[config]);
        let fired = set.evaluate_stream(b"a hit here", 0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].actions.len(), 2);
        assert_eq!(fired[0].match_offset, Some(2));
    }

    #[test]
    fn multiple_rules_each_evaluate_independently() {
        let mut set = MatchRuleSet::compile(&[
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
        // Only rule "a" matches a chunk containing "AA" but not "ZZ".
        let fired = set.evaluate_stream(b"--AA--", 0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, set.ids()[0]);
        // Both fire when both patterns are present (offset 6 keeps the streams apart).
        assert_eq!(set.evaluate_stream(b"AA..ZZ", 6).len(), 2);
        // Neither matches when absent.
        assert!(set.evaluate_stream(b"BBBB", 12).is_empty());
    }
}
