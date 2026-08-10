//! Reporting for talker's own bookkeeping faults (ADR-054).
//!
//! An internal fault is talker's state machine disagreeing with itself: a
//! result arriving for an untracked request, a message index the schedule
//! cannot render. None of them is a statement about the channel's link, and
//! that distinction is what this module exists to keep.
//!
//! Two rules travel together here because they are easy to hold separately and
//! wrong separately:
//!
//! 1. **No `channel` field.** The GUI log layer tallies any event carrying that
//!    field onto the channel's row in the channel list, raising its warning
//!    badge. A bug in our own command tracking must not summon the reader to
//!    their serial link over a fault that is not the channel's, and that they
//!    can neither act on nor clear. The channel is named in the *text* instead,
//!    which keeps the identity in the record without making a false claim in
//!    the UI.
//! 2. **A decade cadence.** Several of these sit inside per-poll loops, so a
//!    wedged state machine could emit one every poll forever.
//!
//! Building the line through [`InternalFaultTally::report`] is what keeps the
//! two together: a caller cannot obtain the text without counting, and the
//! shape it returns has no field to attach a channel id to.

use std::fmt::Display;

/// Occurrences of one internal bookkeeping fault, and whether this one earns a
/// line.
///
/// Reports the 1st, 10th, 100th … so a fault that happens once is never missed
/// and one that repeats every poll cannot flood the log: a billion occurrences
/// produce ten lines. The count is reported with it, because *how often* is what
/// separates the two shapes these ever take — a single edge case we got wrong,
/// or a state machine wedged and repeating.
///
/// Counted per channel slot, never pooled: a wedged slot cannot be diagnosed
/// from a number that every channel contributed to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InternalFaultTally(u64);

impl InternalFaultTally {
    /// Count one occurrence; return the line to log when this one earns one.
    ///
    /// `did` states what talker's own state did, and `costs` what that means
    /// for the reader — the only two things they can use. Neither takes a
    /// trailing full stop; the shape supplies it, so every one of these reads
    /// the same way.
    ///
    /// Log the result at WARN or ERROR, never lower. A fault recorded only when
    /// someone had already raised the log level is one nobody hears about, and
    /// these are rare by construction, so their frequency is itself a signal.
    pub(crate) fn report(
        &mut self,
        channel_label: impl Display,
        did: impl Display,
        costs: impl Display,
    ) -> Option<String> {
        self.count().map(|seen| {
            format!("internal fault on channel {channel_label} ({seen}x): {did}. {costs}. Please report this.")
        })
    }

    /// The cadence itself, split out so it can be asserted without a message.
    fn count(&mut self) -> Option<u64> {
        self.0 += 1;
        let seen = self.0;
        std::iter::successors(Some(1_u64), |decade| decade.checked_mul(10))
            .take_while(|decade| *decade <= seen)
            .any(|decade| decade == seen)
            .then_some(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An internal fault must survive happening once and must not flood the
    /// log happening constantly — the two shapes these ever take. The decade
    /// cadence is what serves both, and the count is what tells them apart.
    #[test]
    fn an_internal_fault_reports_on_decades_so_a_wedged_state_cannot_flood() {
        let mut tally = InternalFaultTally::default();
        let reported: Vec<u64> = (0..10_000).filter_map(|_| tally.count()).collect();

        assert_eq!(
            reported,
            vec![1, 10, 100, 1_000, 10_000],
            "the first occurrence is never missed, and ten thousand cost five lines"
        );
    }

    /// The shape is fixed here rather than at each call site, so all six read
    /// alike: who, how often, what our state did, what it costs, what to do.
    #[test]
    fn a_reported_fault_names_the_channel_in_its_text() {
        let mut tally = InternalFaultTally::default();
        let line = tally
            .report(
                "GPS out",
                "a result arrived for an untracked request",
                "Sending is unaffected",
            )
            .expect("the first occurrence always reports");

        assert_eq!(
            line,
            "internal fault on channel GPS out (1x): a result arrived for an untracked request. \
             Sending is unaffected. Please report this."
        );
        assert_eq!(tally.report("GPS out", "x", "y"), None, "the 2nd is silent");
    }
}
