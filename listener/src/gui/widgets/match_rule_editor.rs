//! A minimal in-app editor for **Mark timestamp** rules (§50.2): create a
//! `BytePattern → Mark(+timestamp)` rule so a matched pattern gets an inline local
//! arrival timestamp in the display and Display Recording. This is deliberately small
//! — it edits only single-pattern Mark rules; the general match-rule editor
//! (Idle/Record/Notify/PauseDisplay, compound conditions) is a separate TODO. Rules
//! it doesn't understand are listed read-only and never modified.
//!
//! Edits go straight into `ChannelConfig.match_rules`; because `config_needs_restart`
//! counts `match_rules`, committing flips the lifecycle button to **Apply & Restart**,
//! which rebuilds the pipeline with the new rules (no dedicated runtime command).

use crate::config::{
    ChannelConfig, MarkPosition, MarkTimestamp, MatchAction, MatchCondition, MatchRule,
};
use crate::core::TimestampConfig;

use super::super::fonts::bold;

/// Is this rule one the Mark editor owns — a single `BytePattern` condition whose only
/// action is a timestamped `Mark`? Other rules (Idle, Record, bare Mark, multi-action)
/// are shown read-only so the editor never silently drops profile-authored rules.
fn is_mark_timestamp_rule(rule: &MatchRule) -> bool {
    matches!(rule.condition, MatchCondition::BytePattern { .. })
        && matches!(
            rule.actions.as_slice(),
            [MatchAction::Mark { timestamp: Some(_) }]
        )
}

/// Edit the channel's inline **Mark timestamp** rules (§50.2). Lists the editor-owned
/// rules with pattern / position / format toggles and a delete button, plus an
/// add-row. Returns nothing — edits land in `config.match_rules`; the caller's
/// Apply & Restart path picks them up.
pub(crate) fn edit_mark_rules(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    ui.label(bold("Timestamp marks")).on_hover_text(
        "Insert a local arrival timestamp before/after a byte pattern, inline in \
             the view and the .disp recording (never .raw). Applying restarts the channel.",
    );

    // Edit existing editor-owned rules in place; collect indices to delete afterwards
    // so we don't mutate the vec while iterating it.
    let mut delete: Option<usize> = None;
    for (i, rule) in config.match_rules.iter_mut().enumerate() {
        if !is_mark_timestamp_rule(rule) {
            continue;
        }
        // Both matches! guards above guarantee these destructures succeed.
        let MatchCondition::BytePattern { pattern } = &mut rule.condition else {
            continue;
        };
        let [MatchAction::Mark {
            timestamp: Some(ts),
        }] = rule.actions.as_mut_slice()
        else {
            continue;
        };
        ui.horizontal(|ui| {
            ui.add_enabled_ui(false, |ui| ui.checkbox(&mut rule.enabled, ""));
            mark_pattern_field(ui, pattern);
            mark_position_selector(ui, &mut ts.position);
            mark_format_toggles(ui, &mut ts.format);
            if ui
                .button("🗑")
                .on_hover_text("Delete this mark rule")
                .clicked()
            {
                delete = Some(i);
            }
        });
    }
    if let Some(i) = delete {
        config.match_rules.remove(i);
    }

    // Add-row: an "Add" button appends a new Before/HH:MM:SS mark on an empty pattern.
    // The empty pattern is invalid (validation rejects it) until the user types one, so
    // it can't fire; the user fills it in on the row above after adding.
    if ui
        .button("+ Add timestamp mark")
        .on_hover_text("Add a new byte-pattern → timestamp rule")
        .clicked()
    {
        let n = config
            .match_rules
            .iter()
            .filter(|r| is_mark_timestamp_rule(r))
            .count();
        config.match_rules.push(MatchRule {
            name: format!("mark {}", n + 1),
            condition: MatchCondition::BytePattern {
                pattern: Vec::new(),
            },
            actions: vec![MatchAction::Mark {
                timestamp: Some(MarkTimestamp {
                    position: MarkPosition::Before,
                    format: TimestampConfig::default(),
                }),
            }],
            enabled: true,
        });
    }
}

/// The pattern field: edit the byte pattern as text (UTF-8), the common case for NMEA
/// (`$GPGGA`). Non-UTF-8 patterns from a profile are shown as a lossy string and left
/// unchanged unless the user edits (then they become the typed UTF-8).
fn mark_pattern_field(ui: &mut egui::Ui, pattern: &mut Vec<u8>) {
    let mut text = String::from_utf8_lossy(pattern).into_owned();
    let resp = ui.add(
        egui::TextEdit::singleline(&mut text)
            .desired_width(120.0)
            .hint_text("pattern e.g. $GPGGA"),
    );
    if resp.changed() {
        *pattern = text.into_bytes();
    }
    if pattern.is_empty() {
        ui.label(egui::RichText::new("⚠ empty").weak());
    }
}

/// Before/After selector for where the timestamp is spliced relative to the match.
fn mark_position_selector(ui: &mut egui::Ui, position: &mut MarkPosition) {
    egui::ComboBox::from_id_salt(("mark_pos", ui.next_auto_id()))
        .selected_text(match position {
            MarkPosition::Before => "before",
            MarkPosition::After => "after",
        })
        .width(64.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(position, MarkPosition::Before, "before");
            ui.selectable_value(position, MarkPosition::After, "after");
        });
}

/// The three independent format toggles (date / millis / timezone). Time-of-day is
/// always present, so there's no toggle for it — matches talker's `TimestampConfig`.
fn mark_format_toggles(ui: &mut egui::Ui, format: &mut TimestampConfig) {
    ui.checkbox(&mut format.include_date, "date")
        .on_hover_text("Prefix the calendar date (YYYY-MM-DD).");
    ui.checkbox(&mut format.include_millis, "ms")
        .on_hover_text("Include milliseconds (.123).");
    ui.checkbox(&mut format.include_timezone, "tz")
        .on_hover_text("Append the local UTC offset (e.g. -07:00).");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::templates;

    #[test]
    fn recognizes_only_single_timestamped_mark_rules() {
        let mut rule = MatchRule {
            name: "m".into(),
            condition: MatchCondition::BytePattern {
                pattern: b"$".to_vec(),
            },
            actions: vec![MatchAction::Mark {
                timestamp: Some(MarkTimestamp {
                    position: MarkPosition::Before,
                    format: TimestampConfig::default(),
                }),
            }],
            enabled: true,
        };
        assert!(is_mark_timestamp_rule(&rule));

        // A bare Mark (no timestamp) is not owned by this editor.
        rule.actions = vec![MatchAction::Mark { timestamp: None }];
        assert!(!is_mark_timestamp_rule(&rule));

        // An Idle condition is not owned either.
        rule.condition = MatchCondition::Idle { timeout_ms: 1000 };
        assert!(!is_mark_timestamp_rule(&rule));
    }

    #[test]
    fn ignores_channels_with_unrelated_rules() {
        // A channel template has no match rules; the editor treats it as empty.
        let channel = templates::udp_template();
        assert!(!channel.match_rules.iter().any(is_mark_timestamp_rule));
    }
}
