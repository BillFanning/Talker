//! Where to look when scheduled sends are being skipped.
//!
//! Mostly a router. One branch may state an amount, because misses charged at
//! the moment they were skipped are evidence about the misses themselves
//! (ADR-051); the verbs carry which kind of claim is being made.

use super::*;

/// Hover text for the missed-send callout: what a miss is, and what to do.
///
/// Deliberately short. A technician hovering a fault wants the next action, not
/// the epistemology of the measurement; the limits that qualify the answer live
/// in [`MISSED_ROUTING_LIMITS`], under Timing & runtime details, where someone
/// who has followed the routing and wants to know how far to trust it will look.
pub(in crate::gui) const MISSED_ROUTING_TOOLTIP: &str =
    "A missed send is a scheduled send the channel never reached, having fallen more than one \
interval behind: nothing was attempted, so no timing exists for it. Work the line left to right \
— it is ordered by what would settle the question soonest. See Timing & runtime details for how \
far this evidence reaches.";

/// The measurement limits behind the routing line, shown in the details section.
///
/// Each sentence states a boundary the routing cannot cross, so the reader can
/// tell a strong signal from a weak one. This is the material the callout's
/// hover used to carry.
pub(in crate::gui) const MISSED_ROUTING_LIMITS: &str =
    "Where missed sends point, and how far: a miss charged to a message is counted as the \
cadence point is skipped, so that share is measured rather than inferred, and unlike delay it \
does not thin out as overload gets worse. A skipped run is split across every write it spans, \
so a stall containing several sends charges each with its own part. Its limits are narrower. A \
point is charged to whichever message was inside its interface write as the point passed, so a \
message that holds the channel some other way is not charged; and a message is never charged \
for its own skipped points, which show up instead as a send call longer than its interval. \
The retained write history is sized from the schedule — one window per message, which is as many \
writes as can ever separate a deadline from its handling — so a miss left uncharged means no \
retained interface write spanned that point. That is not the same as the channel having been \
idle: a hold that was not an interface write leaves exactly the same gap, which is why the line \
says \"not charged\" and never \"nothing was running\". The rest of the line is weaker evidence: \
the counts weighed are run totals, so a fault that has since recovered still appears, and the \
capacity finding is a projection rather than a measurement.";

/// Evidence available when scheduled sends are being skipped.
///
/// Grouped rather than passed loose because the honesty of the result depends
/// on which of these is *current* and which is a run total — a distinction the
/// caller has and a bare `u64` would lose.
pub(in crate::gui) struct MissedSendEvidence {
    /// Run-total skipped sends.
    pub missed: u64,
    /// An interface error is showing **now**, not merely somewhere in the run.
    pub interface_erroring: bool,
    /// Run-total failed writes, which may all predate the current state.
    pub failed: u64,
    pub serial_oversubscribed: bool,
    pub service: Option<ServiceEstimate>,
}

/// Where to look when scheduled sends are being skipped.
///
/// Mostly a router, occasionally a verdict, and the wording says which. One
/// input *is* measured at the instant a send was skipped —
/// [`MessageTiming::missed_others`], charged to whichever send held the thread
/// as each point passed (ADR-051) — and where that lands on a message the text
/// states an amount rather than a place to look. Everything else here is a run
/// total or evidence from deadlines that were reached, so those branches keep
/// the hedged verbs: "check", "start from", "may not".
///
/// Capacity findings use the running schedule, not the editable draft, so an
/// unapplied edit is never blamed for a run's misses.
///
/// Order is by decisiveness. A live interface fault comes first because retry
/// backoff withholds sends, which is a different failure wearing the same
/// symptom; a physically impossible schedule comes next because no amount of
/// tuning elsewhere changes it.
pub(in crate::gui) fn missed_send_routing(
    evidence: &MissedSendEvidence,
    per_message: &[MessageTiming],
) -> Option<DecisionSignal> {
    let MissedSendEvidence {
        missed,
        interface_erroring,
        failed,
        serial_oversubscribed,
        service,
    } = *evidence;
    if missed == 0 {
        return None;
    }

    let blocker = per_message
        .iter()
        .enumerate()
        .filter(|(_, message)| !message.blocked_others.is_zero())
        .max_by_key(|(_, message)| message.blocked_others);
    // Measured where the miss happened rather than inferred from lateness, so
    // this outranks `blocker` when both are present.
    let convicted = per_message
        .iter()
        .enumerate()
        .filter(|(_, message)| message.missed_others > 0)
        .max_by_key(|(_, message)| message.missed_others);

    let text = if interface_erroring {
        "Missed sends: the interface is failing right now, and retry backoff withholds sends while \
         it recovers — start from Send outcomes above."
            .to_owned()
    } else if failed > 0 {
        // Cumulative, so this fault may have recovered long ago. Say when it
        // happened rather than implying it is happening.
        format!(
            "Missed sends: {} sends failed earlier in this run. If the misses came from that \
             period they follow the retry backoff, not the schedule — check Send outcomes above.",
            thousands(failed)
        )
    } else if serial_oversubscribed {
        "Missed sends: the serial line cannot carry this schedule — see Capacity.".to_owned()
    } else if let Some((index, message)) = convicted {
        // The one branch entitled to state an amount rather than a lead: every
        // point counted here was charged while some message's send held the
        // thread.
        //
        // All three quantities appear. Naming only the largest culprit and the
        // shortfall drops every other charged message out of a sentence whose
        // numbers are supposed to add up — with 5 to #2, 4 to #3 and 3
        // uncharged, the old wording said 5 and 3 of 12.
        let attributed: u64 = per_message
            .iter()
            .map(|message| message.missed_others)
            .sum();
        let headline = if attributed >= missed {
            format!(
                "all {} are charged to sends that held the channel",
                thousands(missed)
            )
        } else {
            format!(
                "{} of {} are charged to sends that held the channel",
                thousands(attributed),
                thousands(missed)
            )
        };
        let largest = if message.missed_others == attributed {
            format!(", all to message #{}", index + 1)
        } else {
            format!(
                ", most to message #{} with {}",
                index + 1,
                thousands(message.missed_others)
            )
        };
        let hold = if message.longest_block.is_zero() {
            String::new()
        } else {
            format!(
                " Its longest send held the channel {}.",
                compact_duration(message.longest_block)
            )
        };
        // Uncharged is not idle, and nothing here can tell the causes apart.
        // What the record supports is one negative fact — no measured interface
        // write spanned the point — and a late wake, work outside the send
        // call, and a free thread all produce exactly that. Ageing out is *not*
        // among them: the history is sized from the schedule (see
        // `MISSED_ROUTING_LIMITS`), so a write that could have spanned the
        // point is still there to be found.
        let remainder = if attributed >= missed {
            String::new()
        } else {
            format!(
                " The other {} are not charged to any send: no measured interface write spanned \
                 those points, which a late deadline wake, work outside the send itself, and an \
                 idle thread all look alike.",
                thousands(missed - attributed)
            )
        };
        format!("Missed sends: {headline}{largest}.{hold}{remainder} See Per-message timing.")
    } else if let Some((index, message)) = blocker {
        // Two different quantities, and only one of them is an elapsed hold:
        // the longest blocking send is what the channel actually spent, while
        // the combined figure sums every delayed message's wait and can exceed
        // it. Stating the hold first keeps the larger number from reading as
        // one.
        format!(
            "Missed sends: check message #{} first — its longest send held the channel {}, causing \
             {} of combined waiting across other messages in {} sends. See Per-message timing.",
            index + 1,
            compact_duration(message.longest_block),
            compact_duration(message.blocked_others),
            thousands(message.blocking_sends),
        )
    } else if service.is_some_and(|estimate| estimate.headroom_factor() < 1.0) {
        // Still a projection even with the running schedule as its input: it
        // divides summed p99 bounds into a requested rate, so "may not" stays
        // the strongest honest verb.
        "Missed sends: rendering and the interface write together may not service the requested \
         rate — see Capacity."
            .to_owned()
    } else if per_message
        .iter()
        .filter(|message| !message.interval.is_zero())
        .count()
        <= 1
    {
        // With one active message there is nothing else to hold the channel, so
        // the blocking branch above can never fire. Saying "no single message
        // accounts for these" here would be true and useless — it describes the
        // absence of a cause that was never possible.
        //
        // Worded in the same terms as every other branch: a message *holds the
        // channel*. This one said "competing for its thread" and named the
        // internals of the wait — the reader is a technician with a serial
        // link, not someone who can act on a wake being late.
        "Missed sends: this channel has one active message, so nothing else can be holding it up \
         — either that message's own send takes longer than its interval, or the channel is being \
         woken late. Compare its send-call timing against its interval."
            .to_owned()
    } else {
        "Missed sends: no message delayed another and no capacity limit was reached — compare \
         render and send-call timing in Timing & runtime details."
            .to_owned()
    };

    Some(DecisionSignal {
        text,
        tone: SignalTone::Warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::telemetry::MessageTiming;

    use wiredata_ui::diagnostics::SignalTone;

    /// The routing exists because the message showing the misses is rarely the
    /// one causing them. Each branch names a place to look, in decisiveness
    /// order, and none of them fires when nothing was skipped.
    #[test]
    fn missed_send_routing_names_a_cause_in_decisiveness_order() {
        let blocker = MessageTiming {
            blocked_others: Duration::from_millis(430),
            blocking_sends: 4,
            longest_block: Duration::from_millis(120),
            ..MessageTiming::default()
        };
        let per_message = [MessageTiming::default(), blocker];
        let evidence = |missed, interface_erroring, failed, oversubscribed| MissedSendEvidence {
            missed,
            interface_erroring,
            failed,
            serial_oversubscribed: oversubscribed,
            service: None,
        };

        // Nothing skipped: no line at all.
        assert!(missed_send_routing(&evidence(0, false, 0, false), &per_message).is_none());

        // A *live* interface fault outranks everything: backoff withholding
        // sends is a different failure wearing the same symptom.
        let failing = missed_send_routing(&evidence(12, true, 3, true), &per_message).unwrap();
        assert!(failing.text.contains("failing right now"), "{failing:?}");
        assert_eq!(failing.tone, SignalTone::Warning);

        // The same failure count with no current error is a run total that may
        // long since have recovered, and must not be stated in the present.
        let recovered = missed_send_routing(&evidence(12, false, 3, false), &per_message).unwrap();
        assert!(
            recovered.text.contains("earlier in this run"),
            "a recovered fault must not be reported as current: {recovered:?}"
        );

        // A schedule the wire cannot carry. No settings-vs-running qualifier is
        // needed any more: capacity is calculated from the running schedule, so
        // an unapplied edit cannot reach this line.
        let oversubscribed =
            missed_send_routing(&evidence(12, false, 0, true), &per_message).unwrap();
        assert!(oversubscribed.text.contains("cannot carry this schedule"));

        // The blocking message: routed to, not convicted, and the elapsed hold
        // is stated separately from the combined waiting it caused — the latter
        // sums across victims and can exceed the send itself.
        let blocked = missed_send_routing(&evidence(12, false, 0, false), &per_message).unwrap();
        assert_eq!(
            blocked.text,
            "Missed sends: check message #2 first — its longest send held the channel 120 ms, \
             causing 430 ms of combined waiting across other messages in 4 sends. See Per-message \
             timing."
        );

        // One active message cannot block another, so the blocking branch is
        // structurally unreachable. Reporting its absence as a finding is the
        // non-sequitur this branch exists to avoid.
        let active = MessageTiming {
            interval: Duration::from_millis(50),
            ..MessageTiming::default()
        };
        let alone = missed_send_routing(
            &evidence(12, false, 0, false),
            &[active, MessageTiming::default()],
        )
        .unwrap();
        assert!(
            alone.text.contains("one active message"),
            "a single-message channel must not be told no message stands out: {alone:?}"
        );

        // Two active messages, neither blocking: now the absence really is the
        // finding, and the line says so without naming a message.
        let second = MessageTiming {
            interval: Duration::from_millis(80),
            ..MessageTiming::default()
        };
        let unexplained =
            missed_send_routing(&evidence(12, false, 0, false), &[active, second]).unwrap();
        assert!(unexplained.text.contains("no message delayed another"));
    }

    /// The one branch entitled to state an amount rather than a lead, and the
    /// two things that keep it honest: every charged miss is accounted for,
    /// and the uncharged remainder is described as uncharged rather than as
    /// idle time the measurement never observed.
    #[test]
    fn measured_misses_state_an_amount_without_overclaiming_the_remainder() {
        let culprit = MessageTiming {
            blocked_others: Duration::from_millis(430),
            blocking_sends: 4,
            longest_block: Duration::from_millis(120),
            missed_others: 9,
            ..MessageTiming::default()
        };
        let per_message = [MessageTiming::default(), culprit];
        let evidence = |missed| MissedSendEvidence {
            missed,
            interface_erroring: false,
            failed: 0,
            serial_oversubscribed: false,
            service: None,
        };

        // Every miss accounted for. Note this same input routes to the
        // delay-based lead when `missed_others` is zero, above.
        let all = missed_send_routing(&evidence(9), &per_message).unwrap();
        assert_eq!(
            all.text,
            "Missed sends: all 9 are charged to sends that held the channel, all to message #2. \
             Its longest send held the channel 120 ms. See Per-message timing."
        );

        // Three the record cannot place. Uncharged is not idle — a late wake
        // and work outside the send call leave the same gap as a free thread,
        // and this line must not pick one of those.
        let partial = missed_send_routing(&evidence(12), &per_message).unwrap();
        assert!(
            partial
                .text
                .contains("The other 3 are not charged to any send"),
            "the shortfall must be stated as uncharged: {partial:?}"
        );
        assert!(
            !partial.text.contains("thread free"),
            "the measurement cannot see an idle thread, so it may not claim one: {partial:?}"
        );
    }

    /// Every charged message has to survive into the sentence. Naming only the
    /// largest culprit and the shortfall silently dropped the rest: with five
    /// misses on #2, four on #3 and three uncharged, the line accounted for
    /// eight of twelve and read as though it had covered all of them.
    #[test]
    fn the_routing_line_accounts_for_every_charged_miss() {
        let hold = Duration::from_millis(120);
        let per_message = [
            MessageTiming::default(),
            MessageTiming {
                missed_others: 5,
                longest_block: hold,
                ..MessageTiming::default()
            },
            MessageTiming {
                missed_others: 4,
                longest_block: hold,
                ..MessageTiming::default()
            },
        ];
        let routing = missed_send_routing(
            &MissedSendEvidence {
                missed: 12,
                interface_erroring: false,
                failed: 0,
                serial_oversubscribed: false,
                service: None,
            },
            &per_message,
        )
        .unwrap();

        // The charged total, the run total, the largest single share, and the
        // part that could not be placed — all four, or the arithmetic does not
        // close for the reader.
        assert!(routing.text.contains("9 of 12 are charged"), "{routing:?}");
        assert!(
            routing.text.contains("most to message #2 with 5"),
            "{routing:?}"
        );
        assert!(routing.text.contains("The other 3"), "{routing:?}");
    }
}
