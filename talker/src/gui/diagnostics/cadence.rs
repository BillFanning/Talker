//! The Cadence row: the schedule the channel is running, and how far behind it.
//!
//! Leads with the schedule — how many messages at which intervals — because the
//! lateness measurement pools every active message's deadlines and reads as one
//! message's behaviour without that count beside it.

use super::*;

/// Scale a delay against the channel's tightest cadence, for schedule context.
///
/// The interval is named but not restated: the schedule phrase leads the same
/// line and its groups are sorted shortest-first, so repeating the duration here
/// printed it twice. There is no `≤` either — since the warm-up gate went
/// (ADR-046) this only ever qualifies an exact maximum, never a bucket bound.
pub(in crate::gui) fn relative_to_shortest(
    duration: std::time::Duration,
    shortest: std::time::Duration,
) -> String {
    if shortest.is_zero() {
        return String::new();
    }
    let percentage = duration.as_secs_f64() / shortest.as_secs_f64() * 100.0;
    format!(" ({} of the shortest interval)", percent(percentage))
}

/// The most interval groups the schedule phrase will name before summarizing.
const MAX_CADENCE_GROUPS: usize = 3;

/// Distinct active intervals and how many messages run at each, shortest first.
/// Dormant messages (a zero interval) are excluded — they have no cadence.
///
/// Grouping rather than listing: real schedules cluster, and `2 at 50 ms, 1 at
/// 15 s` says something a span (`50 ms–15 s`) actively obscures, since a span
/// implies messages spread across the range. It also scales past the handful of
/// messages a flat list stays readable at.
pub(in crate::gui) fn cadence_groups(
    intervals: impl IntoIterator<Item = std::time::Duration>,
) -> Vec<(std::time::Duration, usize)> {
    let mut groups: Vec<(std::time::Duration, usize)> = Vec::new();
    for interval in intervals {
        if interval.is_zero() {
            continue;
        }
        match groups.binary_search_by_key(&interval, |(value, _)| *value) {
            Ok(at) => groups[at].1 += 1,
            Err(at) => groups.insert(at, (interval, 1)),
        }
    }
    groups
}

/// How the channel's active messages are scheduled, in the reader's terms.
///
/// A channel runs each message on its own interval, so this leads the Cadence
/// row: a number that pools several cadences is misread as one message's
/// behaviour unless the count is on the same line.
///
/// `groups` is the per-message detail when the channel has reported any;
/// `cadence` is the timer status' own summary, used only when it has not, so a
/// single render never mixes the two sources.
fn cadence_schedule_phrase(
    cadence: Option<ActiveCadence>,
    groups: &[(std::time::Duration, usize)],
    setup_incomplete: bool,
) -> String {
    if groups.is_empty() {
        // An unfinished edit is not the same as a channel with nothing to send,
        // and only one of them is actionable. Capacity already says this one
        // line above; saying "No messages sending" here contradicted it.
        if setup_incomplete && cadence.is_none() {
            return "Finish message setup to calculate cadence".to_owned();
        }
        // Only reachable in the gap between a channel's first TimerStatus and
        // its first Counters, so it states the count and the tightest cadence
        // and leaves the distribution to the per-message lane a moment later.
        let Some(cadence) = cadence else {
            return "No messages sending".to_owned();
        };
        let shortest = format_interval(cadence.shortest);
        return match cadence.messages {
            1 => format!("1 message every {shortest}"),
            count => format!("{count} messages, shortest {shortest}"),
        };
    }

    let total: usize = groups.iter().map(|(_, count)| count).sum();
    if groups.len() == 1 {
        let interval = format_interval(groups[0].0);
        return if total == 1 {
            format!("1 message every {interval}")
        } else {
            format!("{total} messages, each every {interval}")
        };
    }

    let named = groups
        .iter()
        .take(MAX_CADENCE_GROUPS)
        .map(|(interval, count)| format!("{count} at {}", format_interval(*interval)))
        .collect::<Vec<_>>()
        .join(", ");
    if groups.len() > MAX_CADENCE_GROUPS {
        let remaining: usize = groups
            .iter()
            .skip(MAX_CADENCE_GROUPS)
            .map(|(_, count)| count)
            .sum();
        format!("{total} messages: {named}, {remaining} slower")
    } else {
        format!("{total} messages: {named}")
    }
}

/// How late a send started, in the reader's terms: a send is "late" by the time
/// between the clock reaching its scheduled moment and the runner starting it.
fn lateness_phrase(
    label: &str,
    lateness: std::time::Duration,
    shortest: Option<std::time::Duration>,
) -> String {
    format!(
        "{label} {}{}",
        compact_duration(lateness),
        shortest
            .map(|interval| relative_to_shortest(lateness, interval))
            .unwrap_or_default()
    )
}

pub(in crate::gui) fn cadence_decision(
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
    cadence: Option<ActiveCadence>,
    groups: &[(std::time::Duration, usize)],
    setup_incomplete: bool,
    snapshot_state: RecentSnapshotState,
) -> DecisionSignal {
    let recent_samples = recent.deadline_lateness.sample_count();
    let run_samples = cumulative.deadline_lateness.sample_count();
    let snapshot_label = recent_snapshot_label(snapshot_state);
    let schedule = cadence_schedule_phrase(cadence, groups, setup_incomplete);
    // The percentage denominator comes from whichever source drew the schedule
    // above it, so the percentage always scales against an interval the reader
    // can see in the schedule phrase on the same line.
    let shortest = groups
        .first()
        .map(|(interval, _)| *interval)
        .or_else(|| cadence.map(|cadence| cadence.shortest));
    let nothing_scheduled = shortest.is_none();
    let run_max = cumulative.deadline_lateness.max().unwrap_or_default();

    // Every branch leads with the schedule, so the reader learns how many
    // cadences the measurement pools before reading the measurement.
    let text = if let RecentSnapshotState::Expired(_) = snapshot_state {
        if run_samples == 0 {
            format!("{schedule} · {snapshot_label} · nothing sent yet this run")
        } else {
            format!(
                "{schedule} · {snapshot_label} · {}",
                lateness_phrase("worst this run", run_max, shortest)
            )
        }
    } else if run_samples == 0 {
        // Nothing is scheduled and nothing ever ran: promising a first send
        // would describe a channel that has not been asked to send at all.
        if nothing_scheduled {
            schedule.clone()
        } else {
            format!("{schedule} · awaiting the first scheduled send")
        }
    } else if recent_samples == 0 {
        format!(
            "{schedule} · nothing sent in {snapshot_label} · {}",
            lateness_phrase("worst this run", run_max, shortest)
        )
    } else {
        // One measured state at every sample count. The percentage is anchored
        // to whichever figure the sentence ends on, so it always qualifies the
        // number immediately before it.
        let recent_max = recent.deadline_lateness.max().unwrap_or_default();
        let recent_p99 = recent
            .deadline_lateness
            .percentile_upper_bound(99)
            .unwrap_or_default();
        // "Sends" would be wrong here. Lateness is sampled when the channel
        // reaches a scheduled point, which happens before retry backoff decides
        // whether to attempt anything — so this population includes sends that
        // were withheld and never transmitted, and excludes skipped points the
        // channel never reached at all. "Reached" is exactly that set.
        let measured = if recent_p99 < recent_max {
            format!(
                "99% within {} of schedule, worst {} behind{}",
                compact_duration(recent_p99),
                compact_duration(recent_max),
                shortest
                    .map(|interval| relative_to_shortest(recent_max, interval))
                    .unwrap_or_default(),
            )
        } else {
            format!(
                "worst was {} behind schedule{}",
                compact_duration(recent_max),
                shortest
                    .map(|interval| relative_to_shortest(recent_max, interval))
                    .unwrap_or_default(),
            )
        };
        format!(
            "{schedule} · {measured} · {} scheduled sends reached · {snapshot_label}",
            thousands(recent_samples)
        )
    };

    DecisionSignal {
        text,
        // Lateness has no universal good/bad threshold. Keep it neutral and
        // let the exact value, normalized to the schedule, support the decision.
        tone: SignalTone::Neutral,
    }
}

/// The one tooltip for the Cadence row.
///
/// Written for a technician who has never read the source: it defines the
/// channel-of-messages model first, then what "late" measures, then why the
/// number is a pool rather than one message's behaviour.
pub(in crate::gui) fn cadence_tooltip(
    cadence: Option<ActiveCadence>,
    groups: &[(std::time::Duration, usize)],
) -> String {
    // Same source rule as the row itself: per-message detail when the channel
    // has reported any, the timer status' own count otherwise.
    let active = if groups.is_empty() {
        cadence.map_or(0, |cadence| cadence.messages)
    } else {
        groups.iter().map(|(_, count)| count).sum()
    };
    let pooling = if active > 1 {
        format!(
            "This channel is sending {active} messages, each on its own repeating interval, and \
             all of them go out through one interface, one at a time. The figure above pools \
             every active message's sends together — it is the channel's behaviour, not any \
             single message's. Because a \
             message that repeats more often contributes more sends, the fastest messages weigh \
             most heavily in it."
        )
    } else {
        "A channel sends each of its messages on its own repeating interval; this one currently \
         has a single message sending, so the figure above describes that message."
            .to_owned()
    };

    format!(
        "{pooling} This measures one thing: the gap between the moment a send was scheduled for \
         and the moment the channel actually got to it. It does not include how long the send \
         itself took, and it never means the data arrived late at the far end. \"Reached\" is the \
         exact population behind it — every scheduled send the channel got to, which includes any \
         that were then withheld by retry backoff without being transmitted, and excludes points \
         skipped entirely, which are counted as Missed on the Send outcomes line and never appear \
         here. The worst delay is always shown with that count beside it, so a figure from four \
         cannot be mistaken for one from four thousand. \"99% within X of schedule\" appears \
         alongside only when that is a different figure from the worst — below a hundred it never \
         is — and means at most one in a hundred waited longer than X, rounded up to a histogram \
         bucket edge, which is what ≤ marks elsewhere. Any percentage compares the delay with the \
         shortest interval on the channel, to show whether it is a rounding error against the \
         tightest schedule or a real part of it; it is not a per-message figure."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::telemetry::{RecentSnapshotState, SendTimingTelemetry};
    use crate::core::timing::ActiveCadence;

    use crate::gui::diagnostics::tests::{mixed_cadence, mixed_groups};
    use wiredata_ui::diagnostics::SignalTone;

    #[test]
    fn cadence_has_one_measured_state_at_every_sample_count() {
        let mut recent = SendTimingTelemetry::default();
        for _ in 0..4 {
            recent.deadline_lateness.record(Duration::from_millis(1));
        }

        // Four samples: the p99 bucket is the maximum's bucket, so a percentile
        // would restate the same number under a stronger name.
        let few = cadence_decision(
            recent,
            recent,
            mixed_cadence(),
            &mixed_groups(),
            false,
            RecentSnapshotState::Current(Duration::ZERO),
        );
        assert_eq!(
            few.text,
            "3 messages: 2 at 50 ms, 1 at 1,000 ms · worst was 1.00 ms behind schedule \
             (2% of the shortest interval) · 4 scheduled sends reached · recent snapshot"
        );
        assert_eq!(few.tone, SignalTone::Neutral);

        // Crossing twenty changes nothing: the old gate fired here, but the two
        // statistics are still identical.
        for _ in 4..40 {
            recent.deadline_lateness.record(Duration::from_millis(1));
        }
        let more = cadence_decision(
            recent,
            recent,
            mixed_cadence(),
            &mixed_groups(),
            false,
            RecentSnapshotState::Current(Duration::ZERO),
        );
        assert!(
            more.text.contains("worst was 1.00 ms behind schedule") && !more.text.contains("99%"),
            "twenty samples must not promote the same figure to a percentile: {}",
            more.text
        );

        // With enough samples and real spread, the percentile is a different
        // figure and both are worth showing.
        for _ in 40..200 {
            recent.deadline_lateness.record(Duration::from_millis(1));
        }
        recent.deadline_lateness.record(Duration::from_millis(40));
        let spread = cadence_decision(
            recent,
            recent,
            mixed_cadence(),
            &mixed_groups(),
            false,
            RecentSnapshotState::Current(Duration::ZERO),
        );
        assert_eq!(
            spread.text,
            "3 messages: 2 at 50 ms, 1 at 1,000 ms · 99% within 1.00 ms of schedule, worst 40 ms \
             behind (80% of the shortest interval) · 201 scheduled sends reached · recent snapshot"
        );
    }

    /// The schedule leads every branch: a reader who sees a lateness figure
    /// must always be able to see, on the same line, how many independently
    /// scheduled messages were pooled to produce it.
    #[test]
    fn every_cadence_state_states_how_many_messages_are_scheduled() {
        let mut warmed = SendTimingTelemetry::default();
        for _ in 0..20 {
            warmed.deadline_lateness.record(Duration::from_millis(1));
        }
        let empty = SendTimingTelemetry::default();
        let current = RecentSnapshotState::Current(Duration::ZERO);

        for (state, recent, cumulative) in [
            (current, empty, empty),
            (current, empty, warmed),
            (current, warmed, warmed),
            (
                RecentSnapshotState::Expired(Duration::from_secs(11)),
                warmed,
                warmed,
            ),
        ] {
            let text = cadence_decision(
                recent,
                cumulative,
                mixed_cadence(),
                &mixed_groups(),
                false,
                state,
            )
            .text;
            assert!(
                text.starts_with("3 messages: 2 at 50 ms, 1 at 1,000 ms"),
                "state left the message count off the line: {text}"
            );
        }
    }

    /// A span implies messages spread across a range. Real schedules cluster,
    /// and the grouping is what shows the cluster plus its outlier.
    #[test]
    fn clustered_intervals_group_rather_than_reading_as_a_spread() {
        let empty = SendTimingTelemetry::default();
        let state = RecentSnapshotState::Current(Duration::ZERO);

        assert_eq!(
            cadence_decision(empty, empty, mixed_cadence(), &mixed_groups(), false, state).text,
            "3 messages: 2 at 50 ms, 1 at 1,000 ms · awaiting the first scheduled send"
        );
    }

    #[test]
    fn cadence_groups_exclude_dormant_messages_and_sort_shortest_first() {
        let groups = cadence_groups([
            Duration::from_secs(1),
            Duration::ZERO, // dormant: no cadence to report
            Duration::from_millis(50),
            Duration::from_secs(1),
        ]);
        assert_eq!(
            groups,
            vec![(Duration::from_millis(50), 1), (Duration::from_secs(1), 2),]
        );
        assert_eq!(cadence_groups([Duration::ZERO, Duration::ZERO]), vec![]);
    }

    /// A schedule with many distinct intervals must not turn the row into a
    /// list; past three groups the remainder is summarized.
    #[test]
    fn many_distinct_intervals_are_summarized_after_three_groups() {
        let empty = SendTimingTelemetry::default();
        let state = RecentSnapshotState::Current(Duration::ZERO);
        let groups = cadence_groups([
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(50),
            Duration::from_secs(1),
            Duration::from_secs(5),
        ]);

        assert_eq!(
            cadence_decision(empty, empty, mixed_cadence(), &groups, false, state).text,
            "5 messages: 1 at 10 ms, 1 at 20 ms, 1 at 50 ms, 2 slower · awaiting the \
             first scheduled send"
        );
    }

    #[test]
    fn uniform_and_single_message_schedules_do_not_claim_a_span() {
        let empty = SendTimingTelemetry::default();
        let state = RecentSnapshotState::Current(Duration::ZERO);
        let uniform = |messages| {
            Some(ActiveCadence {
                messages,
                shortest: Duration::from_millis(250),
            })
        };

        let same = |count| cadence_groups(std::iter::repeat_n(Duration::from_millis(250), count));
        assert_eq!(
            cadence_decision(empty, empty, uniform(1), &same(1), false, state).text,
            "1 message every 250 ms · awaiting the first scheduled send"
        );
        assert_eq!(
            cadence_decision(empty, empty, uniform(4), &same(4), false, state).text,
            "4 messages, each every 250 ms · awaiting the first scheduled send"
        );
        // Every message dormant and nothing ever sent: the row says only that,
        // with no promise of a send that nothing is scheduled to make.
        assert_eq!(
            cadence_decision(empty, empty, None, &[], false, state).text,
            "No messages sending"
        );
        // But an unfinished edit is a different, actionable state, and must not
        // be reported as a channel that has nothing to send — Capacity says
        // the same thing one line above.
        assert_eq!(
            cadence_decision(empty, empty, None, &[], true, state).text,
            "Finish message setup to calculate cadence"
        );
    }

    /// The tooltip has to teach the channel-of-messages model, since a
    /// technician reading the row has no other source for it.
    #[test]
    fn cadence_tooltip_explains_pooling_only_when_several_messages_send() {
        let many = cadence_tooltip(mixed_cadence(), &mixed_groups());
        assert!(many.contains("sending 3 messages, each on its own repeating interval"));
        assert!(many.contains("pools every active message's sends together"));

        let one = cadence_tooltip(
            Some(ActiveCadence {
                messages: 1,
                shortest: Duration::from_millis(250),
            }),
            &cadence_groups([Duration::from_millis(250)]),
        );
        assert!(
            !one.contains("pools every active message's sends together"),
            "a single-message channel must not be told its figure is a pool"
        );
        assert!(one.contains("describes that message"));

        // Both forms must define what "late" measures and rule out delivery.
        for tooltip in [many, one] {
            assert!(tooltip.contains("scheduled for"));
            assert!(tooltip.contains("never means the data arrived late at the far end"));
        }
    }
}
