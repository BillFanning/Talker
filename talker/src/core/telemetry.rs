//! Bounded cumulative timing measurements for the send hot path.
//!
//! Histograms retain fixed-size bucket counts, not individual samples. That
//! keeps recording allocation-free and makes a cumulative snapshot cheap to
//! copy through the existing observer lane.

use std::time::{Duration, Instant};

pub(crate) use wiredata_telemetry::RecentDurationHistogram;
pub use wiredata_telemetry::{DurationHistogram, RECENT_WINDOW};

/// Whether the observer's last collapsed recent-window snapshot is suitable
/// for live decisions.
///
/// A running snapshot expires once its capture age reaches the window it
/// summarized. A stopped channel retains its last exact-at-stop snapshot as
/// final evidence rather than aging it as though the runner were still live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecentSnapshotState {
    Pending,
    Current(Duration),
    Expired(Duration),
    Final,
}

/// Classify a collapsed recent-window snapshot from its exact compute instant.
///
/// `observed_at` can precede `captured_at` only through a caller race or a
/// synthetic test; saturating that age to zero keeps presentation robust.
/// `final_snapshot` is provenance carried by the runner's exact-at-rest
/// counter update; it is not inferred from thread or UI lifecycle.
pub fn recent_snapshot_state(
    captured_at: Option<Instant>,
    observed_at: Instant,
    final_snapshot: bool,
) -> RecentSnapshotState {
    let Some(captured_at) = captured_at else {
        return RecentSnapshotState::Pending;
    };
    if final_snapshot {
        return RecentSnapshotState::Final;
    }

    let age = observed_at
        .checked_duration_since(captured_at)
        .unwrap_or_default();
    if age >= RECENT_WINDOW {
        RecentSnapshotState::Expired(age)
    } else {
        RecentSnapshotState::Current(age)
    }
}

/// Cumulative timing boundaries observed by one channel runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendTimingTelemetry {
    /// Delay from a message's monotonic schedule deadline until it is handled.
    pub deadline_lateness: DurationHistogram,
    /// Time spent rendering a due payload immediately before its send attempt.
    pub render_duration: DurationHistogram,
    /// Time spent inside the interface's application-level `send` call.
    pub send_duration: DurationHistogram,
}

/// One counter-lane snapshot: exact run-to-date totals plus a bounded recent view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendTimingReport {
    pub cumulative: SendTimingTelemetry,
    pub recent: SendTimingTelemetry,
}

/// Cumulative timing for one message, plus what its own sends cost the others.
///
/// A channel runs every message on one thread, so a send holds that thread
/// against every other message's deadline. Lateness alone therefore identifies
/// only the *victim*: the message that suffers most is usually the one with the
/// tightest interval, not the one responsible. [`MessageTiming::blocked_others`]
/// is the other half of that pair.
///
/// **What that pair does and does not establish.** Delay blame
/// ([`blocked_others`](MessageTiming::blocked_others)) is measured against
/// deadlines the channel *reached*, so it explains observed **lateness**
/// directly (ADR-045). It could never explain a **miss**, because a skipped
/// cadence point is passed over before any of it runs — and it failed worst
/// exactly under overload, where fewer deadlines are reached and the evidence
/// thins out as the problem grows.
///
/// [`missed_others`](MessageTiming::missed_others) closes that gap by counting
/// at the skip instead of at the deadline (ADR-051). The two are separate
/// columns on purpose: one is time other messages spent waiting, the other is
/// sends they never made, and neither is convertible into the other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageTiming {
    /// The cadence this message was running at when the snapshot was taken.
    /// Zero means dormant. Carried with the measurement so a reader never has
    /// to pair run-time timing against a draft interval that may have moved.
    pub interval: Duration,
    /// Wire bytes one send of this message produces. With `interval`, this is
    /// the running schedule's own demand, so capacity can be calculated from
    /// what is sending rather than from the settings on screen.
    pub wire_bytes: usize,
    /// Whether this message's interval changed after it had started being
    /// measured. The histograms below are cumulative for the whole run, so when
    /// this is set they span more than one cadence and `interval` is only the
    /// latest — a distinction a reader cannot recover from the numbers alone.
    pub interval_changed: bool,
    /// Delay from this message's own deadlines until it was handled.
    pub deadline_lateness: DurationHistogram,
    /// Time spent rendering this message's payloads.
    pub render_duration: DurationHistogram,
    /// Time spent inside the interface `send` call for this message.
    pub send_duration: DurationHistogram,
    /// Total delay other messages incurred because of this message's sends,
    /// **summed across every message delayed**.
    ///
    /// Not an elapsed time, and deliberately not presentable as one: a single
    /// 412 ms send that leaves four messages waiting contributes each of their
    /// waits, so this can exceed the duration of the send that caused it. The
    /// figure that *is* an elapsed hold is [`Self::longest_block`].
    pub blocked_others: Duration,
    /// How many of this message's sends delayed at least one other message.
    /// Counts sends, not victims: one long send that displaces four deadlines
    /// increments this once.
    pub blocking_sends: u64,
    /// The longest single send of this message that delayed another — the real
    /// elapsed time the channel was held, which `blocked_others` is not.
    pub longest_block: Duration,
    /// Cadence points of **other** messages that were passed over while this
    /// message's sends held the channel thread, and so will never fire.
    ///
    /// The counterpart to `blocked_others` for the sends that never happened
    /// rather than the ones that happened late — and the one that survives
    /// overload. `blocked_others` can only grow when a deadline is *reached*,
    /// so the worse the stall, the fewer deadlines there are to observe and
    /// the thinner the evidence gets. A skipped point is counted where it
    /// occurred, so this grows with the problem instead (ADR-051).
    ///
    /// Summing this across every message gives the share of the channel's
    /// `missed_sends` that some send is answerable for. The remainder was
    /// skipped with the thread idle — a late wake, machine sleep — and is
    /// deliberately charged to nobody.
    pub missed_others: u64,
}

impl MessageTiming {
    /// This message's interval in whole milliseconds, the unit
    /// [`MessageDemand`](crate::core::capacity::MessageDemand) is built from.
    pub fn interval_ms(&self) -> u64 {
        self.interval.as_millis() as u64
    }
}

/// How many of `count` points, starting at `first` and spaced `interval`
/// apart, fall strictly before `limit`.
///
/// Closed form, not a loop: a 1 ms cadence stalled for a minute skips sixty
/// thousand points, and a machine suspended overnight skips tens of millions.
/// This runs on the channel thread between sends, so it has to cost the same
/// whether one point was lost or a million.
fn points_before(first: Instant, interval: Duration, count: u64, limit: Instant) -> u64 {
    let Some(span) = limit.checked_duration_since(first) else {
        return 0;
    };
    let interval = interval.as_nanos();
    if span.is_zero() || interval == 0 {
        return 0;
    }
    // The points before `limit` are those with k * interval < span, for
    // k = 0, 1, …; there are ceil(span / interval) of them.
    let fitting = span.as_nanos().div_ceil(interval);
    u64::try_from(fitting).unwrap_or(u64::MAX).min(count)
}

/// One completed send's occupancy of the channel thread.
#[derive(Clone, Copy, Debug)]
struct SendWindow {
    index: usize,
    started: Instant,
    ended: Instant,
    /// Whether this send has already been counted in `blocking_sends`.
    counted: bool,
}

/// Runner-owned per-message timing, including deadline-delay attribution.
///
/// **Attribution rule.** A message is credited with another's delay only for
/// the portion of that delay which elapsed while its own send held the thread.
/// A deadline that passed during a send is charged to that send; once the
/// runner starts working off a late backlog, the whole burst stays charged to
/// the send that opened it, rather than to the quick catch-up sends that
/// happen to precede each later victim. Lateness with no send spanning the
/// deadline — an OS wake delay on an idle thread — is charged to nobody.
///
/// Cadence points that were skipped outright follow the same rule at the same
/// resolution ([`record_skips`](Self::record_skips)): each point is charged to
/// whichever send held the thread when it passed, and to nobody when the
/// thread was free. One rule, two consequences — time lost and sends lost.
#[derive(Debug, Default)]
pub(crate) struct MessageTimingRecorder {
    messages: Vec<MessageTiming>,
    /// The most recent completed send.
    last_send: Option<SendWindow>,
    /// While a late backlog is being worked off, the send that opened it.
    burst: Option<SendWindow>,
}

impl MessageTimingRecorder {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            messages: vec![MessageTiming::default(); len],
            last_send: None,
            burst: None,
        }
    }

    fn entry(&mut self, index: usize) -> &mut MessageTiming {
        if index >= self.messages.len() {
            self.messages.resize(index + 1, MessageTiming::default());
        }
        &mut self.messages[index]
    }

    /// Which send, if any, held the channel thread when `deadline` passed.
    ///
    /// The single definition of "who was in the way", shared by the two things
    /// that need it: a deadline that was reached late, and a cadence point that
    /// was never reached at all. They must agree — a run where the same stall
    /// named one message for the lateness and another for the misses would be
    /// reporting an artefact of two rules, not a finding.
    fn blocker_for(&self, deadline: Instant) -> Option<SendWindow> {
        // Still inside a backlog opened by an earlier send: that send keeps the
        // charge, so a quick catch-up send is not blamed for a delay it
        // inherited.
        match self.burst {
            Some(burst) if deadline < burst.ended => Some(burst),
            _ => match self.last_send {
                Some(send) if deadline >= send.started && deadline < send.ended => Some(send),
                _ => None,
            },
        }
    }

    /// [`Self::blocker_for`], and advance the backlog cursor to match.
    ///
    /// Only the deadline actually being handled may move the cursor. A skipped
    /// point lies *after* that deadline, so letting it advance the cursor would
    /// step over deadlines the runner has not processed yet — messages are
    /// handled earliest-first, so a later query must not decide for an earlier
    /// one.
    fn blocker_at(&mut self, deadline: Instant) -> Option<SendWindow> {
        let blocker = self.blocker_for(deadline);
        let continuing = self.burst.is_some_and(|burst| deadline < burst.ended);
        if !continuing {
            self.burst = blocker;
        }
        blocker
    }

    /// Record one due message's lateness and charge it to whichever send was
    /// holding the thread when its deadline passed.
    pub(crate) fn record_due(&mut self, index: usize, scheduled_for: Instant, handled_at: Instant) {
        let lateness = handled_at.saturating_duration_since(scheduled_for);
        self.entry(index).deadline_lateness.record(lateness);

        let Some(blocker) = self.blocker_at(scheduled_for) else {
            return;
        };
        // A send that overran its own next deadline is visible in its own
        // send_duration; this column is what a message cost *others*.
        if blocker.index == index {
            return;
        }
        let attributable = lateness.min(blocker.ended.saturating_duration_since(scheduled_for));
        if attributable.is_zero() {
            return;
        }
        let first_victim = !self.burst.is_some_and(|burst| burst.counted);
        if let Some(burst) = self.burst.as_mut() {
            burst.counted = true;
        }
        let held = blocker.ended.saturating_duration_since(blocker.started);
        let entry = self.entry(blocker.index);
        entry.blocked_others = entry.blocked_others.saturating_add(attributable);
        entry.longest_block = entry.longest_block.max(held);
        if first_victim {
            entry.blocking_sends = entry.blocking_sends.saturating_add(1);
        }
    }

    /// Charge the cadence points this poll skipped to whichever send was
    /// holding the thread as each one passed.
    ///
    /// `scheduled_for` is the deadline that *did* fire; the `skipped` points
    /// follow it at `interval` spacing. Call this after
    /// [`record_due`](Self::record_due) for the same tick, so the backlog
    /// cursor already reflects the deadline these skips belong to.
    ///
    /// Points that passed after the blocking send had returned are charged to
    /// nobody: with the thread free, nothing here caused them. That is the
    /// whole reason the split is worth measuring — it separates a channel that
    /// is over-subscribed from one that is being starved of wake-ups, and the
    /// two want opposite fixes.
    pub(crate) fn record_skips(
        &mut self,
        index: usize,
        scheduled_for: Instant,
        interval: Duration,
        skipped: u64,
    ) {
        if skipped == 0 || interval.is_zero() {
            return;
        }
        let Some(first) = scheduled_for.checked_add(interval) else {
            return;
        };
        let Some(blocker) = self.blocker_for(first) else {
            return;
        };
        // A message that outruns its own cadence says so in its send_duration;
        // this column is what a message cost *others*, same as blocked_others.
        if blocker.index == index {
            return;
        }
        let charged = points_before(first, interval, skipped, blocker.ended);
        if charged == 0 {
            return;
        }
        let entry = self.entry(blocker.index);
        entry.missed_others = entry.missed_others.saturating_add(charged);
    }

    pub(crate) fn record_render(&mut self, index: usize, duration: Duration) {
        self.entry(index).render_duration.record(duration);
    }

    /// Record a completed send and the thread occupancy it represents.
    pub(crate) fn record_send(&mut self, index: usize, started: Instant, ended: Instant) {
        self.entry(index)
            .send_duration
            .record(ended.saturating_duration_since(started));
        self.last_send = Some(SendWindow {
            index,
            started,
            ended,
            counted: false,
        });
    }

    /// Record each message's current wire size and cadence, so the snapshot
    /// describes the schedule the timing was actually measured against — and
    /// carries enough for a reader to recompute its demand.
    pub(crate) fn set_schedule(&mut self, schedule: impl IntoIterator<Item = (usize, Duration)>) {
        for (index, (wire_bytes, interval)) in schedule.into_iter().enumerate() {
            let entry = self.entry(index);
            entry.wire_bytes = wire_bytes;
            // Only a change *away from* a cadence already in effect matters. The
            // first stamp moves from the zero default, and waking a dormant
            // message collected nothing to be misread.
            if !entry.interval.is_zero() && entry.interval != interval {
                entry.interval_changed = true;
            }
            entry.interval = interval;
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<MessageTiming> {
        self.messages.clone()
    }
}

/// Runner-owned timing state. Only fixed-size recent segments are retained.
#[derive(Debug, Default)]
pub(crate) struct SendTimingRecorder {
    cumulative: SendTimingTelemetry,
    recent_deadline_lateness: RecentDurationHistogram,
    recent_render_duration: RecentDurationHistogram,
    recent_send_duration: RecentDurationHistogram,
}

impl SendTimingRecorder {
    pub(crate) fn record_deadline_lateness(&mut self, at: std::time::Instant, duration: Duration) {
        self.cumulative.deadline_lateness.record(duration);
        self.recent_deadline_lateness.record_at(at, duration);
    }

    pub(crate) fn record_render_duration(&mut self, at: std::time::Instant, duration: Duration) {
        self.cumulative.render_duration.record(duration);
        self.recent_render_duration.record_at(at, duration);
    }

    pub(crate) fn record_send_duration(&mut self, at: std::time::Instant, duration: Duration) {
        self.cumulative.send_duration.record(duration);
        self.recent_send_duration.record_at(at, duration);
    }

    pub(crate) fn snapshot_at(&self, now: std::time::Instant) -> SendTimingReport {
        SendTimingReport {
            cumulative: self.cumulative,
            recent: SendTimingTelemetry {
                deadline_lateness: self.recent_deadline_lateness.snapshot_at(now),
                render_duration: self.recent_render_duration.snapshot_at(now),
                send_duration: self.recent_send_duration.snapshot_at(now),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// The core inversion this measurement exists to prevent: the message that
    /// records the lateness is not the message that caused it.
    #[test]
    fn a_long_send_is_charged_to_the_blocker_not_the_late_message() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(4);

        // #3 holds the thread for 412 ms; #1's 100 ms deadline passes inside it.
        recorder.record_send(3, t0, t0 + ms(412));
        recorder.record_due(1, t0 + ms(100), t0 + ms(412));

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot[3].blocked_others, ms(312));
        assert_eq!(snapshot[3].blocking_sends, 1);
        // The victim records its own lateness and blames nobody.
        assert_eq!(snapshot[1].deadline_lateness.max(), Some(ms(312)));
        assert_eq!(snapshot[1].blocked_others, Duration::ZERO);
        assert_eq!(snapshot[1].blocking_sends, 0);
    }

    /// Working off a backlog runs quick sends back to back. Each one precedes
    /// the next victim, but none of them caused the delay they inherited.
    #[test]
    fn a_backlog_stays_charged_to_the_send_that_opened_it() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(4);

        recorder.record_send(3, t0, t0 + ms(412));
        recorder.record_due(1, t0 + ms(100), t0 + ms(412));
        // #1's own send is fast, and immediately precedes #2 being handled.
        let catch_up_end = t0 + ms(412) + Duration::from_micros(400);
        recorder.record_send(1, t0 + ms(412), catch_up_end);
        recorder.record_due(2, t0 + ms(150), catch_up_end);

        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot[1].blocked_others,
            Duration::ZERO,
            "a catch-up send must not inherit blame for the backlog it is clearing"
        );
        // #3 is charged for both victims, but counted as one blocking send.
        assert_eq!(snapshot[3].blocked_others, ms(312) + ms(262));
        assert_eq!(snapshot[3].blocking_sends, 1);
        // The reason these are two different fields: summing across victims
        // exceeds the send that caused it, so only `longest_block` is an
        // elapsed hold time and only it may be presented as one.
        assert_eq!(snapshot[3].longest_block, ms(412));
        assert!(
            snapshot[3].blocked_others > snapshot[3].longest_block,
            "combined victim waiting is expected to exceed the blocking send"
        );
    }

    /// Sends that never happened, charged the same way as sends that happened
    /// late: to whoever was writing when the point went by.
    #[test]
    fn a_skipped_point_is_charged_to_the_send_that_was_writing() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(4);

        // #3 holds the thread for 412 ms. #1 runs at 100 ms, so its points at
        // 200, 300 and 400 pass unreachable inside that write, while the one
        // at 100 fires late as soon as the thread comes free.
        recorder.record_send(3, t0, t0 + ms(412));
        recorder.record_due(1, t0 + ms(100), t0 + ms(412));
        recorder.record_skips(1, t0 + ms(100), ms(100), 3);

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot[3].missed_others, 3);
        assert_eq!(
            snapshot[1].missed_others, 0,
            "the message that lost the points is not the one that caused it"
        );
    }

    /// The split that makes the column worth having: an over-subscribed
    /// channel and a starved one both miss sends, and only the position of
    /// the skipped points tells them apart.
    #[test]
    fn points_skipped_after_the_write_returned_are_charged_to_nobody() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(4);

        // #3 writes for 250 ms, then the thread is free — but the next wake
        // does not arrive until 550 ms, by which time #1 has lost the points
        // at 200, 300, 400 and 500.
        recorder.record_send(3, t0, t0 + ms(250));
        recorder.record_due(1, t0 + ms(100), t0 + ms(550));
        recorder.record_skips(1, t0 + ms(100), ms(100), 4);

        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot[3].missed_others, 1,
            "only the point at 200 ms passed while the write held the thread"
        );
    }

    /// A message that outruns its own cadence is a real fault, but it is not
    /// a cost to anyone else — and it is already visible as a send call longer
    /// than the interval.
    #[test]
    fn a_message_is_never_charged_for_its_own_skipped_points() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(2);

        recorder.record_send(1, t0, t0 + ms(412));
        recorder.record_due(1, t0 + ms(100), t0 + ms(412));
        recorder.record_skips(1, t0 + ms(100), ms(100), 3);

        assert_eq!(recorder.snapshot()[1].missed_others, 0);
    }

    /// The defect this measurement exists to fix. Delay blame can only be
    /// collected at a deadline the channel *reached*, so a block long enough
    /// to swallow a thousand cadence points still yields exactly one lateness
    /// sample: the evidence thins out as the fault gets worse. Counting at the
    /// skip scales with the damage instead.
    #[test]
    fn miss_blame_scales_with_overload_where_delay_blame_thins_out() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(2);

        // One ten-second write against a 10 ms cadence.
        recorder.record_send(0, t0, t0 + ms(10_005));
        recorder.record_due(1, t0 + ms(10), t0 + ms(10_005));
        recorder.record_skips(1, t0 + ms(10), ms(10), 999);

        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot[1].deadline_lateness.sample_count(),
            1,
            "a longer block cannot produce more lateness samples — that is the point"
        );
        assert_eq!(snapshot[0].missed_others, 999);
    }

    /// A suspended machine skips more cadence points than any loop could
    /// visit. Recording the charge is arithmetic, so it costs the same whether
    /// one point was lost or billions.
    #[test]
    fn an_enormous_stall_is_charged_without_visiting_each_point() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(2);
        // The write outlasts the hour by a millisecond, so the last skipped
        // point falls inside it and the count is not a boundary case.
        let block_end = t0 + Duration::from_secs(3600) + Duration::from_millis(1);

        recorder.record_send(0, t0, block_end);
        recorder.record_due(1, t0, block_end);
        // A 1 µs cadence across an hour: 3.6 billion points.
        recorder.record_skips(1, t0, Duration::from_micros(1), 3_600_000_000);

        assert_eq!(recorder.snapshot()[0].missed_others, 3_600_000_000);
    }

    /// Messages are handled earliest-first, so a tick's skipped points reach
    /// further forward in time than deadlines still queued behind it. Looking
    /// those points up must not advance the backlog cursor past the deadlines
    /// still to come, or the message handled next finds no blocker and a real
    /// block goes unattributed.
    #[test]
    fn charging_skips_does_not_disinherit_the_deadline_handled_next() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(3);

        // #2 holds the thread from 0 to 350 ms.
        recorder.record_send(2, t0, t0 + ms(350));
        // #0 runs at 300 ms and is handled very late; its skipped points start
        // at 400 ms, after that block ended.
        recorder.record_due(0, t0 + ms(100), t0 + ms(900));
        recorder.record_skips(0, t0 + ms(100), ms(300), 2);
        // #0's own catch-up send becomes the most recent one…
        recorder.record_send(0, t0 + ms(900), t0 + ms(901));
        // …and only now is #1's deadline handled, which fell inside #2's block.
        recorder.record_due(1, t0 + ms(150), t0 + ms(901));

        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot[2].missed_others, 0,
            "points skipped after the block ended belong to nobody"
        );
        assert_eq!(
            snapshot[2].blocked_others,
            ms(250) + ms(200),
            "the deadline handled after the skip lookup lost its blocker"
        );
    }

    #[test]
    fn lateness_with_no_send_spanning_the_deadline_is_charged_to_nobody() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(4);

        // The thread was idle when this deadline passed: an OS wake delay.
        recorder.record_send(3, t0, t0 + ms(10));
        recorder.record_due(1, t0 + ms(500), t0 + ms(520));

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot[1].deadline_lateness.max(), Some(ms(20)));
        assert!(snapshot
            .iter()
            .all(|message| message.blocked_others.is_zero()));
        assert!(snapshot.iter().all(|message| message.blocking_sends == 0));
    }

    #[test]
    fn a_send_that_overruns_its_own_next_deadline_does_not_blame_others() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(2);

        // #1 sends for 200 ms on a 50 ms interval: it delays only itself.
        recorder.record_send(1, t0, t0 + ms(200));
        recorder.record_due(1, t0 + ms(50), t0 + ms(200));

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot[1].deadline_lateness.max(), Some(ms(150)));
        assert_eq!(snapshot[1].blocked_others, Duration::ZERO);
        assert_eq!(snapshot[1].blocking_sends, 0);
    }

    #[test]
    fn separate_blocking_sends_are_counted_separately() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(4);

        recorder.record_send(3, t0, t0 + ms(400));
        recorder.record_due(1, t0 + ms(100), t0 + ms(400));
        // A second, unrelated block a long while later.
        recorder.record_send(3, t0 + ms(2_000), t0 + ms(2_400));
        recorder.record_due(1, t0 + ms(2_100), t0 + ms(2_400));

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot[3].blocking_sends, 2);
        assert_eq!(snapshot[3].blocked_others, ms(300) + ms(300));
    }

    #[test]
    fn an_interval_changed_mid_run_is_flagged_against_its_cumulative_timing() {
        let t0 = Instant::now();
        let mut recorder = MessageTimingRecorder::new(2);

        // First stamp: moving off the zero default is not a change.
        recorder.set_schedule([(80, ms(50)), (0, Duration::ZERO)]);
        assert!(!recorder.snapshot()[0].interval_changed);

        recorder.record_send(0, t0, t0 + ms(1));
        recorder.set_schedule([(80, ms(50)), (0, Duration::ZERO)]);
        assert!(
            !recorder.snapshot()[0].interval_changed,
            "no change, no flag"
        );

        // Waking a dormant message collected nothing that could be misread.
        recorder.set_schedule([(80, ms(50)), (40, ms(200))]);
        assert!(!recorder.snapshot()[1].interval_changed);

        // A real retune: the histograms now span two cadences and say so.
        recorder.set_schedule([(80, ms(1_000)), (40, ms(200))]);
        let snapshot = recorder.snapshot();
        assert!(snapshot[0].interval_changed);
        assert_eq!(snapshot[0].interval, ms(1_000));
        assert!(!snapshot[1].interval_changed);
        // Wire size rides the same stamp, so a running channel's demand is
        // recomputable from the snapshot alone.
        assert_eq!(snapshot[0].wire_bytes, 80);
        assert_eq!(snapshot[1].wire_bytes, 40);
    }

    #[test]
    fn recent_snapshot_state_has_an_exact_window_boundary() {
        let captured_at = std::time::Instant::now();

        assert_eq!(
            recent_snapshot_state(None, captured_at, false),
            RecentSnapshotState::Pending
        );
        assert_eq!(
            recent_snapshot_state(
                Some(captured_at),
                captured_at + RECENT_WINDOW - Duration::from_nanos(1),
                false,
            ),
            RecentSnapshotState::Current(RECENT_WINDOW - Duration::from_nanos(1))
        );
        assert_eq!(
            recent_snapshot_state(Some(captured_at), captured_at + RECENT_WINDOW, false,),
            RecentSnapshotState::Expired(RECENT_WINDOW)
        );
    }

    #[test]
    fn final_snapshot_does_not_expire_and_future_capture_saturates() {
        let captured_at = std::time::Instant::now();

        assert_eq!(
            recent_snapshot_state(
                Some(captured_at),
                captured_at + RECENT_WINDOW + Duration::from_secs(1),
                true,
            ),
            RecentSnapshotState::Final
        );
        assert_eq!(
            recent_snapshot_state(
                Some(captured_at + Duration::from_secs(1)),
                captured_at,
                false,
            ),
            RecentSnapshotState::Current(Duration::ZERO)
        );
    }

    #[test]
    fn recorder_keeps_cumulative_truth_after_recent_samples_age_out() {
        let t0 = std::time::Instant::now();
        let mut recorder = SendTimingRecorder::default();
        recorder.record_deadline_lateness(t0, Duration::from_micros(100));
        recorder.record_render_duration(t0, Duration::from_micros(200));
        recorder.record_send_duration(t0, Duration::from_micros(300));

        let live = recorder.snapshot_at(t0 + Duration::from_secs(9));
        assert_eq!(live.cumulative.deadline_lateness.sample_count(), 1);
        assert_eq!(live.cumulative.render_duration.sample_count(), 1);
        assert_eq!(live.cumulative.send_duration.sample_count(), 1);
        assert_eq!(live.recent.deadline_lateness.sample_count(), 1);
        assert_eq!(live.recent.render_duration.sample_count(), 1);
        assert_eq!(live.recent.send_duration.sample_count(), 1);

        let aged = recorder.snapshot_at(t0 + RECENT_WINDOW);
        assert_eq!(aged.cumulative.deadline_lateness.sample_count(), 1);
        assert_eq!(aged.cumulative.render_duration.sample_count(), 1);
        assert_eq!(aged.cumulative.send_duration.sample_count(), 1);
        assert_eq!(aged.recent, SendTimingTelemetry::default());
    }
}
