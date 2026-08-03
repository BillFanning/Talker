//! Decision logic behind Talker's channel diagnostics: the pure functions that
//! turn counters, capacity, and timing telemetry into the phrases the detail
//! panel shows, plus the technician-facing help that qualifies them.
//!
//! Kept separate from the rendering in [`super::detail`] so each signal's
//! wording and escalation rule is unit-testable without an egui context. Nothing
//! here draws; nothing here decides layout.
//!
//! Two boundaries run through the whole surface. Sent means the interface
//! write returned success, never that a peer received anything. And there is no
//! universal good/bad latency threshold, so timing is reported as measured fact
//! against the channel's own cadence rather than scored against an invented
//! budget.

use super::draft::ConnKind;
use crate::core::capacity::{
    service_sample_count, ChannelDemand, MessageDemand, ServiceEstimate, MIN_SERVICE_SAMPLES,
};
use crate::core::telemetry::{
    DurationHistogram, MessageTiming, RecentSnapshotState, SendTimingTelemetry,
};
use crate::core::timing::{ActiveCadence, TimerMode, TimerReason, TimerStatus};
use wiredata_ui::diagnostics::SignalTone;
use wiredata_ui::format::{compact_duration, interval as format_interval, percent, thousands};

use super::MessageAnalysisCache;

pub(super) const SENT_MEANING_TOOLTIP: &str =
    "Sent means the configured-interface write returned success. It does not confirm that serial \
bits reached the wire, a network packet left the host, or a peer received the data.";

pub(super) const THROUGHPUT_TOOLTIP: &str =
    "Total is every byte sent since Start and is retained after Stop. The two rates are a \
rolling five-second average of sent messages and bytes. Failed, retry-suppressed, and missed \
sends are excluded from all three. The fixed five-second denominator makes the rates ramp during \
startup and decay to zero after traffic stops, while the total stands still; they are not \
instantaneous line rate. Sent means the interface write returned success, not that any peer \
received the data.";

pub(super) const TIMING_TOOLTIP: &str = "Each recent snapshot merges up to approximately ten seconds of \
fixed one-second segments ending when the runner captured its latest counter update. A slow or \
dormant schedule can therefore leave the displayed snapshot unchanged; its “as of” age is the \
snapshot compute time, not necessarily the newest sample time. While running, a snapshot is no \
longer used as recent evidence once that age reaches ten seconds. After a normal run end, the \
channel retains its exact final snapshot; an abnormal exit can leave the last non-final snapshot \
instead. Deadline lateness runs from a message's monotonic cadence deadline \
until the runner handles that send; handled retry-suppressed sends are included, while \
cadence points counted as Missed are not sampled. Render covers payload and timestamp construction. \
Send call ends when the configured-interface write returns and includes failed attempts; it does \
not prove physical-wire or peer delivery. Each boundary reports its own worst value and its own \
sample count; a percentile is added only where it differs from that worst value, which below a \
hundred samples it never does. Percentiles are histogram-bucket upper bounds.";

pub(super) const TIMER_TOOLTIP: &str =
    "The shortest active interval selects the deadline-wait policy. On \
Windows, intervals below 32 ms hold the shared process-wide 1 ms timer-resolution request in \
either mode. At 32 ms or longer, Standard uses ordinary deadline waits; Precise requests 1 ms \
only for the final 32 ms before a waited deadline. An Immediate schedule's first send has \
no preceding precision window. This channel releases a bounded-window request before rendering \
and writing, although another channel may keep the process-wide request active. Commands interrupt \
both wait stages. Other platforms use native deadline waits. This affects wake timing, not \
timestamp accuracy or physical-wire arrival.";

pub(super) const ALIGNMENT_TOOLTIP: &str =
    "Immediate makes every active message due when the interface \
opens. UTC phase places each first application deadline on the strict next Unix-epoch multiple of \
its interval, then advances on monotonic deadlines. For example, 1000 ms aligns to whole UTC \
seconds, while 1500 ms alternates between whole- and half-second phases. When the runner loops, it \
compares wall clock with its elapsed-time projection no more often than once per second; a \
displacement of at least 250 ms rebuilds future deadlines without replaying bypassed points or \
adding scheduler misses. This aligns application deadlines, not completion of an interface write \
or physical-wire arrival.";

pub(super) const DISPLAY_QUEUE_TOOLTIP: &str =
    "The current value was sampled immediately before the UI's \
last drain of the runner-to-UI diagnostic queue; it is not the post-drain depth. Peak is the \
largest such UI sample, not an exact queue high-water mark. A dropped update may be a payload \
sample, counter snapshot, timer change, or interface error/recovery notice. The runner never waits \
for this queue, so display pressure cannot delay sending. Live readouts can lag until a later \
cumulative update; the final run snapshot remains exact. Reliable command results use a separate \
queue.";

pub(super) fn analyzed_channel_demand(
    message_count: usize,
    analyses: &[MessageAnalysisCache],
) -> Option<ChannelDemand> {
    if analyses.len() != message_count {
        return None;
    }
    let mut demand = ChannelDemand::default();
    for cache in analyses {
        let analysis = cache.analysis.as_ref()?;
        let config = analysis.config.as_ref()?;
        demand.include(MessageDemand::new(analysis.wire_len?, config.interval_ms));
    }
    Some(demand)
}

/// Each drafted message's interval, for a channel that has not reported any
/// runtime timing yet.
///
/// `None` when any message fails to parse, matching [`analyzed_channel_demand`]:
/// a partial schedule would understate the count. That is distinct from
/// `Some(empty)`, which means the schedule is valid and every message dormant —
/// the caller must not collapse the two, because one is an unfinished edit the
/// user can act on and the other is a deliberate state.
pub(super) fn draft_intervals(
    analyses: &[MessageAnalysisCache],
) -> Option<Vec<std::time::Duration>> {
    analyses
        .iter()
        .map(|cache| {
            let config = cache.analysis.as_ref()?.config.as_ref()?;
            Some(std::time::Duration::from_millis(config.interval_ms))
        })
        .collect()
}

pub(super) fn compact_rate(value: f64, unit: &str) -> String {
    let (value, prefix) = if value >= 1_000_000.0 {
        (value / 1_000_000.0, "M")
    } else if value >= 1_000.0 {
        (value / 1_000.0, "k")
    } else {
        (value, "")
    };
    let precision = if value < 10.0 { 2 } else { 1 };
    format!("{value:.precision$} {prefix}{unit}")
}

pub(super) fn compact_factor(factor: f64) -> String {
    if factor >= 1_000.0 {
        ">999x".to_owned()
    } else if factor >= 10.0 {
        format!("{factor:.1}x")
    } else {
        format!("{factor:.2}x")
    }
}

pub(super) fn recent_snapshot_label(state: RecentSnapshotState) -> String {
    match state {
        RecentSnapshotState::Pending => "recent snapshot pending".to_owned(),
        RecentSnapshotState::Current(age) if age < std::time::Duration::from_secs(1) => {
            "recent snapshot".to_owned()
        }
        RecentSnapshotState::Current(age) => {
            format!("recent snapshot · as of {} ago", compact_duration(age))
        }
        RecentSnapshotState::Expired(age) => {
            format!(
                "recent snapshot expired · as of {} ago",
                compact_duration(age)
            )
        }
        RecentSnapshotState::Final => "final recent snapshot · at run end".to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServiceTimingSource {
    Recent,
    Run,
}

pub(super) fn select_service_timing(
    state: RecentSnapshotState,
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
) -> (ServiceTimingSource, SendTimingTelemetry) {
    let recent_is_live = matches!(
        state,
        RecentSnapshotState::Current(_) | RecentSnapshotState::Final
    );
    if recent_is_live && service_sample_count(recent) >= MIN_SERVICE_SAMPLES {
        (ServiceTimingSource::Recent, recent)
    } else {
        (ServiceTimingSource::Run, cumulative)
    }
}

pub(super) fn service_timing_source_label(
    source: ServiceTimingSource,
    state: RecentSnapshotState,
) -> String {
    match source {
        ServiceTimingSource::Recent => recent_snapshot_label(state),
        ServiceTimingSource::Run if matches!(state, RecentSnapshotState::Expired(_)) => {
            format!("run-wide fallback; {}", recent_snapshot_label(state))
        }
        ServiceTimingSource::Run => "run-wide".to_owned(),
    }
}

/// Summarize a histogram without a sample-count gate.
///
/// There was one — below twenty samples the readout showed the maximum and
/// called it a warm-up. It never guarded a bad computation: `rank` is
/// `ceil(samples × 99 / 100)`, which equals `samples` for any count up to 99,
/// so below a hundred samples the p99 bucket *is* the maximum's bucket. The
/// gate only relabelled the same number, and it did so at 20 while the two
/// statistics actually separate at 100.
///
/// So state the maximum, which is exact and true at one sample, and add the
/// percentile only where it is genuinely a different figure. `p99 < max` is
/// exactly that test: within one bucket the bound is `>=` the maximum, so the
/// comparison is false; it becomes true only when the p99 bucket sits strictly
/// below the maximum's. The sample count carries the weight the label used to
/// imply, so nothing needs a warm-up disclaimer at any count.
pub(super) fn timing_figures(histogram: DurationHistogram) -> Option<String> {
    let max = histogram.max()?;
    let p99 = histogram.percentile_upper_bound(99)?;
    Some(if p99 < max {
        format!(
            "99% ≤ {}, worst {}",
            compact_duration(p99),
            compact_duration(max)
        )
    } else {
        format!("worst {}", compact_duration(max))
    })
}

/// One boundary's figures for a line that already states a sample count.
///
/// `line_samples` is that count, so this appends its own only when the two
/// populations differ — the same rule the per-message cells use. Render and the
/// send call are recorded in lockstep, so on a line carrying both, one shared
/// count is exact; lateness is sampled for sends that retry backoff then
/// withheld, so it states its own whenever that gap opens.
pub(super) fn timing_metric(
    label: &str,
    histogram: DurationHistogram,
    line_samples: u64,
) -> String {
    let Some(figures) = timing_figures(histogram) else {
        return format!("{label} no samples");
    };
    let samples = histogram.sample_count();
    if samples == line_samples {
        format!("{label} {figures}")
    } else {
        format!("{label} {figures} of {}", thousands(samples))
    }
}

pub(super) fn unavailable_line_capacity_label(kind: ConnKind) -> &'static str {
    match kind {
        ConnKind::Serial => "complete Serial setup",
        ConnKind::Udp | ConnKind::Tcp => "network line unmeasured",
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DecisionSignal {
    pub(super) text: String,
    pub(super) tone: SignalTone,
}

pub(super) fn diagnostic_card_tone(
    delivery: SignalTone,
    capacity: SignalTone,
    timer_request_failed: bool,
    dropped_updates: u64,
    running: bool,
) -> SignalTone {
    if delivery == SignalTone::Fault || capacity == SignalTone::Fault {
        SignalTone::Fault
    } else if delivery == SignalTone::Warning
        || capacity == SignalTone::Warning
        || timer_request_failed
        || dropped_updates > 0
    {
        SignalTone::Warning
    } else if running {
        SignalTone::Healthy
    } else {
        SignalTone::Neutral
    }
}

/// The run's counted send outcomes as one always-visible line.
///
/// Written as visible arithmetic — the schedule's own cadence points, less each
/// way a send can fail to go out — because every alternative required naming the
/// successful remainder in isolation, and no such name survived scrutiny:
/// *accepted* never says accepted by what, *sent* alone reads as delivery, and
/// *unaccepted* is false for the two categories the interface never saw. Stating
/// the subtraction removes the need: the line defines its own final term.
pub(super) fn send_outcomes(
    sent: u64,
    failed: u64,
    suppressed: u64,
    missed: u64,
) -> DecisionSignal {
    let unsent = failed.saturating_add(suppressed).saturating_add(missed);
    let scheduled = sent.saturating_add(unsent);
    if scheduled == 0 {
        return DecisionSignal {
            text: "Send outcomes: no scheduled sends yet".to_owned(),
            tone: SignalTone::Neutral,
        };
    }

    DecisionSignal {
        text: format!(
            "Send outcomes: {} scheduled - {} failed - {} suppressed - {} missed = {} sent",
            thousands(scheduled),
            thousands(failed),
            thousands(suppressed),
            thousands(missed),
            thousands(sent),
        ),
        tone: if failed > 0 {
            SignalTone::Fault
        } else if unsent > 0 {
            SignalTone::Warning
        } else {
            SignalTone::Healthy
        },
    }
}

/// The one tooltip for the send-outcomes line: what each term counts, and where
/// in the send path each deduction happened.
pub(super) fn send_outcomes_tooltip(
    sent: u64,
    failed: u64,
    suppressed: u64,
    missed: u64,
) -> String {
    let scheduled = sent
        .saturating_add(failed)
        .saturating_add(suppressed)
        .saturating_add(missed);
    format!(
        "{} scheduled counts every cadence point this run's schedule produced, less \
         the three ways a send does not go out. {} failed: the interface write was \
         attempted and returned an error. {} suppressed: after a failure, the send \
         was withheld during retry backoff and never attempted — these follow failures and \
         cannot occur without one. {} missed: the runner fell more than one interval \
         behind, so the cadence point was skipped before any send existed. The remaining \
         {} sent means the interface write returned success; it does not confirm that \
         bytes reached the wire or that any peer received them.",
        thousands(scheduled),
        thousands(failed),
        thousands(suppressed),
        thousands(missed),
        thousands(sent),
    )
}

/// Scale a delay against the channel's tightest cadence, for schedule context.
///
/// The interval is named but not restated: the schedule phrase leads the same
/// line and its groups are sorted shortest-first, so repeating the duration here
/// printed it twice. There is no `≤` either — since the warm-up gate went
/// (ADR-046) this only ever qualifies an exact maximum, never a bucket bound.
pub(super) fn relative_to_shortest(
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
pub(super) fn cadence_groups(
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

pub(super) fn cadence_decision(
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

/// One message's row in the per-message breakdown.
///
/// The point of the row is reading across it: `late_p99` is what this message
/// suffered, `blocked_others` is what it cost the rest. A channel where the
/// two land on different rows is the normal case, not an anomaly — the message
/// with the tightest interval absorbs the delay, and a slow infrequent one
/// causes it.
pub(super) struct MessageRow {
    pub label: String,
    pub interval: String,
    pub sends: String,
    pub late: String,
    pub send_call: String,
    /// Longest single send of this message that delayed another: an elapsed
    /// hold time, unlike [`Self::delay_caused`].
    pub longest_block: String,
    /// Combined waiting this message imposed, summed across every message it
    /// delayed. Can exceed `longest_block`; never an elapsed time.
    pub delay_caused: String,
    /// This message delayed others, so its row carries the attention tone.
    pub blocks_others: bool,
}

const NO_MEASUREMENT: &str = "—";

/// Per-message cells use the same model as every other timing readout
/// (ADR-046) — but the row already carries a Sends column, so repeating an
/// identical count in every cell just crowds out the figures.
///
/// The count appears only when this boundary's population differs from that
/// column, which is exactly when it is worth reading: lateness is sampled for
/// sends withheld by retry backoff, and the send call is timed for writes that
/// failed, so a gap here is evidence rather than noise.
fn cell_summary(histogram: DurationHistogram, sends: u64) -> String {
    let Some(figures) = timing_figures(histogram) else {
        return NO_MEASUREMENT.to_owned();
    };
    let samples = histogram.sample_count();
    if samples == sends {
        figures
    } else {
        format!("{figures} of {}", thousands(samples))
    }
}

pub(super) fn per_message_rows(counts: &[u64], timing: &[MessageTiming]) -> Vec<MessageRow> {
    timing
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let sends = counts.get(index).copied().unwrap_or(0);
            MessageRow {
                // Messages are identified by their position in the Messages
                // editor, one-based to match what the editor shows.
                label: format!("#{}", index + 1),
                interval: if message.interval.is_zero() {
                    "dormant".to_owned()
                } else if message.interval_changed {
                    // The histograms beside this are cumulative, so they span the
                    // cadence this message used to run at as well as this one.
                    format!("{} (changed)", format_interval(message.interval))
                } else {
                    format_interval(message.interval)
                },
                sends: thousands(sends),
                late: cell_summary(message.deadline_lateness, sends),
                // No render column: payload construction is a clock read, an
                // allocation and a memcpy — typically 1-3 us, below the 50 us
                // first bucket of the histogram that would report it. It stays
                // in the clipboard report and in the channel-wide work line,
                // where it costs nothing and still catches a pathological
                // payload, rather than taking a column that reads the same
                // forever.
                send_call: cell_summary(message.send_duration, sends),
                longest_block: if message.longest_block.is_zero() {
                    NO_MEASUREMENT.to_owned()
                } else {
                    format!(
                        "{} in {} sends",
                        compact_duration(message.longest_block),
                        thousands(message.blocking_sends)
                    )
                },
                delay_caused: if message.blocked_others.is_zero() {
                    NO_MEASUREMENT.to_owned()
                } else {
                    compact_duration(message.blocked_others)
                },
                blocks_others: !message.blocked_others.is_zero(),
            }
        })
        .collect()
}

pub(super) const PER_MESSAGE_TOOLTIP: &str =
    "One row per message, numbered as in the Messages editor. Late is what that message \
suffered: how long after its own scheduled moment the channel got to it. Longest block and Delay \
caused are what it cost everything else — the longest single send of this message that held the \
channel against another, and the waiting that imposed added up across every message delayed. \
Those two are different quantities: the second sums several messages' waits, so it can exceed the \
send that caused it and is not an elapsed time. Suffering and causing usually land on different \
rows, and that is the normal shape of a cadence problem: a channel handles its messages one at a \
time, so a slow infrequent message can delay a fast one badly while recording almost no lateness \
itself. Read across a row, not down a column. Sends counts writes that succeeded; a timing figure \
repeats a count only where its own differs — lateness is also sampled for sends that retry \
backoff then withheld, and the send call is timed for writes that failed. Figures cover the whole \
run, and a percentile appears only where it differs from the worst value; the rolling ten-second \
view is channel-wide and appears in the Cadence row instead.";

pub(super) const MISSED_ROUTING_TOOLTIP: &str =
    "A missed send is a scheduled send the channel never reached, because it had fallen more than \
one interval behind. Nothing was attempted and no timing exists for it, so nothing here is \
measured at the moment a send was skipped. This line suggests where to look, in the order worth \
checking; it does not prove a cause. Two limits are worth knowing. The counts it weighs are run \
totals, so a fault that has since recovered still appears, and a finding drawn from the settings \
on screen describes those settings rather than whatever is running. And the blocking evidence is \
measured against scheduled sends the channel did reach, not against the ones it skipped — the two \
usually share a cause, but they are different populations, and under heavy overload fewer sends \
are reached, so blocking is measured least well exactly when it matters most. What is reliable is \
the direction: misses concentrate on whichever message has the tightest interval, because one \
grid point is skipped per interval of lateness, so the message showing the misses is rarely the \
one causing them.";

/// Evidence available when scheduled sends are being skipped.
///
/// Grouped rather than passed loose because the honesty of the result depends
/// on which of these is *current* and which is a run total — a distinction the
/// caller has and a bare `u64` would lose.
pub(super) struct MissedSendEvidence {
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
/// A router, not a verdict. Nothing here is measured at the instant a send was
/// skipped: the counts are run totals and the blocking evidence comes from
/// sends that *were* reached, so the strongest honest claim is where to start.
/// The verbs carry that — "check", "start from".
///
/// Capacity findings now derive from the running schedule rather than the
/// settings on screen, so an unapplied edit can no longer be blamed for a run's
/// misses and no longer needs qualifying here.
///
/// Order is by decisiveness. A live interface fault comes first because retry
/// backoff withholds sends, which is a different failure wearing the same
/// symptom; a physically impossible schedule comes next because no amount of
/// tuning elsewhere changes it.
pub(super) fn missed_send_routing(
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
        // With one active message there is nothing else to hold the thread, so
        // the blocking branch above can never fire. Saying "no single message
        // accounts for these" here would be true and useless — it describes the
        // absence of a cause that was never possible.
        "Missed sends: this channel has one active message, so nothing else is competing for its \
         thread — either its own render and send overrun its interval, or deadline wakes are \
         arriving late. Compare its send-call timing against its interval."
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

/// The one tooltip for the Cadence row.
///
/// Written for a technician who has never read the source: it defines the
/// channel-of-messages model first, then what "late" measures, then why the
/// number is a pool rather than one message's behaviour.
pub(super) fn cadence_tooltip(
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

pub(super) fn timing_detail_text(
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
    snapshot_state: RecentSnapshotState,
) -> String {
    let run_samples = cumulative.deadline_lateness.sample_count();
    if run_samples == 0 {
        return "Work per send: awaiting first send".to_owned();
    }

    let run_max = cumulative
        .deadline_lateness
        .max()
        .map(compact_duration)
        .unwrap_or_else(|| "n/a".to_owned());
    match snapshot_state {
        RecentSnapshotState::Expired(_) => format!(
            "Work per send: {} · run max late {run_max}",
            recent_snapshot_label(snapshot_state)
        ),
        RecentSnapshotState::Pending => {
            format!("Work per send: recent snapshot pending · run max late {run_max}")
        }
        // Deadline lateness is deliberately absent: the Cadence row renders the
        // same recent histogram with the schedule context that makes it
        // readable, so repeating it here was one fact in two places. What is
        // left is what nothing else shows — how long the two stages of the work
        // itself took, channel-wide and recently, since the per-message table is
        // cumulative — plus the run-wide lateness ceiling the Cadence row omits
        // while a current window is available.
        RecentSnapshotState::Current(_) | RecentSnapshotState::Final => {
            // Both boundaries share one population, so the count belongs to the
            // line rather than to each figure on it.
            let samples = recent.send_duration.sample_count();
            format!(
                "Work per send ({}, {} sends): {} · {} · run max late {run_max}",
                recent_snapshot_label(snapshot_state),
                thousands(samples),
                timing_metric("render", recent.render_duration, samples),
                timing_metric("send call", recent.send_duration, samples),
            )
        }
    }
}

pub(super) fn timer_status_detail(status: TimerStatus) -> (String, bool) {
    let shortest = status
        .shortest_active_interval()
        .map(compact_duration)
        .unwrap_or_else(|| "no active messages".to_owned());
    match (status.reason, status.mode) {
        (TimerReason::HighRate, TimerMode::WindowsOneMillisecond) => (
            format!("Windows 1 ms continuous (high rate) · shortest {shortest}"),
            false,
        ),
        (TimerReason::HighRate, TimerMode::WindowsRequestFailed) => (
            format!("Windows 1 ms request failed (high rate) · shortest {shortest}"),
            true,
        ),
        (TimerReason::HighRate, TimerMode::NativeDeadlineWaits) => (
            format!("native deadline waits (high rate) · shortest {shortest}"),
            false,
        ),
        (TimerReason::HighRate, TimerMode::Standard) => (
            format!("high-rate timer request pending · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::WindowsOneMillisecond) => (
            format!("Windows 1 ms deadline windows · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::WindowsRequestFailed) => (
            format!("Windows 1 ms request failed · shortest {shortest}"),
            true,
        ),
        (TimerReason::PrecisionWindow, TimerMode::NativeDeadlineWaits) => (
            format!("native deadline waits · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::Standard) => (
            format!("first waited deadline window pending · shortest {shortest}"),
            false,
        ),
        (TimerReason::None, _) if status.active_cadence.is_some() => {
            (format!("standard deadline waits · {shortest}"), false)
        }
        (TimerReason::None, _) => (
            "idle · no active messages; no timer request".to_owned(),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        analyzed_channel_demand, cadence_decision, cadence_groups, cadence_tooltip,
        diagnostic_card_tone, missed_send_routing, per_message_rows, select_service_timing,
        send_outcomes, send_outcomes_tooltip, timer_status_detail, timing_detail_text,
        timing_figures, timing_metric, unavailable_line_capacity_label, MissedSendEvidence,
        ServiceTimingSource,
    };
    use crate::core::telemetry::{
        DurationHistogram, MessageTiming, RecentSnapshotState, SendTimingTelemetry,
    };
    use crate::core::timing::{
        ActiveCadence, CadenceAlignment, TimerMode, TimerReason, TimerStatus,
    };
    use crate::gui::{
        draft::{ConnKind, PayloadKind, ScheduleDraft},
        MessageAnalysisCache,
    };
    use wiredata_ui::diagnostics::SignalTone;
    /// Every count on this line and in its tooltip is grouped. A long run's
    /// scheduled total is the widest number in the pane, and it is the one a
    /// reader has to compare against the sent total by eye.
    #[test]
    fn send_outcomes_group_every_count_in_the_line_and_its_tooltip() {
        let long_run = send_outcomes(1_234_567, 0, 0, 2_400);
        assert_eq!(
            long_run.text,
            "Send outcomes: 1,236,967 scheduled - 0 failed - 0 suppressed - 2,400 missed \
             = 1,234,567 sent"
        );

        let tip = send_outcomes_tooltip(1_234_567, 0, 0, 2_400);
        assert!(tip.contains("1,236,967 scheduled"), "{tip}");
        assert!(tip.contains("2,400 missed"), "{tip}");
        assert!(tip.contains("1,234,567 sent"), "{tip}");
    }

    #[test]
    fn send_outcomes_distinguish_clean_shortfall_and_failure() {
        let clean = send_outcomes(100, 0, 0, 0);
        assert_eq!(
            clean.text,
            "Send outcomes: 100 scheduled - 0 failed - 0 suppressed - 0 missed = 100 sent"
        );
        assert_eq!(clean.tone, SignalTone::Healthy);

        // Stated as arithmetic: the schedule's own cadence points, less each
        // way a send fails to go out. Nothing has to name the remainder.
        let shortfall = send_outcomes(98, 0, 1, 1);
        assert_eq!(
            shortfall.text,
            "Send outcomes: 100 scheduled - 0 failed - 1 suppressed - 1 missed = 98 sent"
        );
        assert_eq!(shortfall.tone, SignalTone::Warning);

        // Same totals, but an attempted write returned an error: that is the
        // only component that escalates to a fault.
        let failed = send_outcomes(98, 1, 0, 1);
        assert_eq!(
            failed.text,
            "Send outcomes: 100 scheduled - 1 failed - 0 suppressed - 1 missed = 98 sent"
        );
        assert_eq!(failed.tone, SignalTone::Fault);
    }

    #[test]
    fn send_outcomes_are_neutral_before_any_schedule_fires() {
        let decision = send_outcomes(0, 0, 0, 0);
        assert_eq!(decision.text, "Send outcomes: no scheduled sends yet");
        assert_eq!(decision.tone, SignalTone::Neutral);
    }

    /// The send-outcome counts moved out of the card and above it, but their
    /// tone still has to reach the badge — otherwise a failing interface reads
    /// as a calm card. This pins the coupling that survived that move.
    #[test]
    fn send_outcome_tone_still_escalates_the_card_badge() {
        let failing = send_outcomes(98, 1, 0, 1);
        assert_eq!(
            diagnostic_card_tone(failing.tone, SignalTone::Neutral, false, 0, true),
            SignalTone::Fault
        );

        let shortfall = send_outcomes(98, 0, 1, 1);
        assert_eq!(
            diagnostic_card_tone(shortfall.tone, SignalTone::Neutral, false, 0, true),
            SignalTone::Warning
        );

        let clean = send_outcomes(100, 0, 0, 0);
        assert_eq!(
            diagnostic_card_tone(clean.tone, SignalTone::Neutral, false, 0, true),
            SignalTone::Healthy
        );
    }

    #[test]
    fn send_outcomes_tooltip_defines_each_term_and_its_limit() {
        let tip = send_outcomes_tooltip(98, 1, 0, 1);
        assert!(tip.contains("100 scheduled counts every cadence point"));
        // Suppressions are downstream of a failure, never an independent
        // fault — the tooltip has to say so or the two read as peers.
        assert!(tip.contains("cannot occur without one"));
        // The remainder is defined by the equation, but its limit still needs
        // stating: a successful write is not proof of delivery.
        assert!(tip.contains("does not confirm that bytes reached the wire"));
    }

    #[test]
    fn card_tone_surfaces_faults_warnings_and_clean_live_state() {
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Fault, false, 0, true),
            SignalTone::Fault
        );
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Neutral, true, 0, true),
            SignalTone::Warning
        );
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Neutral, false, 0, true),
            SignalTone::Healthy
        );
        assert_eq!(
            diagnostic_card_tone(SignalTone::Healthy, SignalTone::Neutral, false, 0, false),
            SignalTone::Neutral
        );
    }

    /// A clustered schedule — two fast messages and one slow one — which is the
    /// shape a span renders misleadingly and a grouping renders honestly.
    fn mixed_cadence() -> Option<ActiveCadence> {
        Some(ActiveCadence {
            messages: 3,
            shortest: Duration::from_millis(50),
        })
    }

    fn mixed_groups() -> Vec<(Duration, usize)> {
        cadence_groups([
            Duration::from_millis(50),
            Duration::from_millis(50),
            Duration::from_secs(1),
        ])
    }

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

    /// The rule that replaced the warm-up gate, stated directly: the percentile
    /// earns its place only when it differs from the maximum.
    #[test]
    fn timing_figures_add_a_percentile_only_when_it_differs_from_the_maximum() {
        assert_eq!(timing_figures(DurationHistogram::default()), None);

        let mut single = DurationHistogram::default();
        single.record(Duration::from_millis(3));
        assert_eq!(
            timing_figures(single).unwrap(),
            "worst 3 ms",
            "one sample is reportable without a warm-up disclaimer"
        );

        // 99 identical samples: rank == samples, so p99 lands in the maximum's
        // own bucket and adds nothing.
        let mut identical = DurationHistogram::default();
        for _ in 0..99 {
            identical.record(Duration::from_millis(1));
        }
        assert_eq!(timing_figures(identical).unwrap(), "worst 1.00 ms");

        let mut spread = identical;
        for _ in 99..200 {
            spread.record(Duration::from_millis(1));
        }
        spread.record(Duration::from_millis(40));
        assert_eq!(
            timing_figures(spread).unwrap(),
            "99% ≤ 1.00 ms, worst 40 ms"
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

    /// The row exists so a reader can see the victim and the culprit are
    /// different messages. This pins that shape.
    #[test]
    fn per_message_rows_separate_what_a_message_suffered_from_what_it_caused() {
        let mut victim = MessageTiming {
            interval: Duration::from_millis(50),
            ..MessageTiming::default()
        };
        victim.deadline_lateness.record(Duration::from_millis(9));
        victim.send_duration.record(Duration::from_micros(300));

        let mut culprit = MessageTiming {
            interval: Duration::from_secs(2),
            blocked_others: Duration::from_millis(430),
            blocking_sends: 4,
            longest_block: Duration::from_millis(120),
            ..MessageTiming::default()
        };
        culprit.deadline_lateness.record(Duration::from_micros(80));
        culprit.send_duration.record(Duration::from_millis(120));

        let rows = per_message_rows(&[600, 4], &[victim, culprit]);
        assert_eq!(rows.len(), 2);

        // #1 is late and blames nobody.
        assert_eq!(rows[0].label, "#1");
        assert_eq!(rows[0].interval, "50 ms");
        assert_eq!(rows[0].sends, "600");
        assert_eq!(rows[0].late, "worst 9 ms of 1");
        assert_eq!(rows[0].longest_block, "—");
        assert_eq!(rows[0].delay_caused, "—");
        assert!(!rows[0].blocks_others);

        // #2 is barely late itself, and is charged for the delay it caused.
        assert_eq!(rows[1].label, "#2");
        assert_eq!(rows[1].late, "worst 80.0 us of 1");
        assert_eq!(rows[1].send_call, "worst 120 ms of 1");
        // The hold and the combined waiting are separate cells because the
        // second is a sum across victims and is not an elapsed time.
        assert_eq!(rows[1].longest_block, "120 ms in 4 sends");
        assert_eq!(rows[1].delay_caused, "430 ms");
        assert!(rows[1].blocks_others);
    }

    /// The Sends column already carries the count, so repeating it in all three
    /// timing cells said the same number four times per row. It survives only
    /// where it differs — which is the case that carries information.
    #[test]
    fn per_message_cells_repeat_the_send_count_only_when_it_differs() {
        let mut matching = MessageTiming::default();
        for _ in 0..3 {
            matching.deadline_lateness.record(Duration::from_millis(1));
            matching.send_duration.record(Duration::from_micros(200));
        }
        let rows = per_message_rows(&[3], &[matching]);
        assert_eq!(rows[0].sends, "3");
        assert_eq!(rows[0].late, "worst 1.00 ms", "count is already a column");
        assert_eq!(rows[0].send_call, "worst 200 us");

        // Retry backoff samples lateness for sends it then withholds, so this
        // population outruns the successful-send count — and that gap is the
        // evidence, so it is stated.
        let mut withheld = matching;
        for _ in 0..9 {
            withheld.deadline_lateness.record(Duration::from_millis(1));
        }
        let rows = per_message_rows(&[3], &[withheld]);
        assert_eq!(rows[0].late, "worst 1.00 ms of 12");
        assert_eq!(rows[0].send_call, "worst 200 us");
    }

    #[test]
    fn per_message_rows_mark_dormant_messages_and_missing_measurements() {
        let dormant = MessageTiming::default();
        let rows = per_message_rows(&[], &[dormant]);
        assert_eq!(rows[0].interval, "dormant");
        assert_eq!(rows[0].sends, "0");
        // No samples is not the same as a measured zero.
        assert_eq!(rows[0].late, "—");
        assert_eq!(rows[0].send_call, "—");
        assert_eq!(rows[0].longest_block, "—");
        assert_eq!(rows[0].delay_caused, "—");
    }

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

    #[test]
    fn expired_snapshot_is_not_presented_as_current_cadence_or_timing() {
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..20 {
            timing.deadline_lateness.record(Duration::from_millis(1));
        }
        let state = RecentSnapshotState::Expired(Duration::from_secs(11));

        let cadence = cadence_decision(
            timing,
            timing,
            mixed_cadence(),
            &mixed_groups(),
            false,
            state,
        );
        assert_eq!(
            cadence.text,
            "3 messages: 2 at 50 ms, 1 at 1,000 ms · recent snapshot expired · as \
             of 11.0 s ago · worst this run 1.00 ms (2% of the shortest interval)"
        );
        assert_eq!(
            timing_detail_text(timing, timing, state),
            "Work per send: recent snapshot expired · as of 11.0 s ago · run max late 1.00 ms"
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

    #[test]
    fn service_timing_uses_recent_only_while_current_or_final_and_warmed() {
        let mut recent = SendTimingTelemetry::default();
        for _ in 0..20 {
            recent.render_duration.record(Duration::from_micros(100));
            recent.send_duration.record(Duration::from_micros(200));
        }
        let mut cumulative = recent;
        cumulative.render_duration.record(Duration::from_millis(10));
        cumulative.send_duration.record(Duration::from_millis(20));

        assert_eq!(
            select_service_timing(
                RecentSnapshotState::Current(Duration::from_secs(9)),
                recent,
                cumulative,
            )
            .0,
            ServiceTimingSource::Recent
        );
        assert_eq!(
            select_service_timing(
                RecentSnapshotState::Expired(Duration::from_secs(10)),
                recent,
                cumulative,
            )
            .0,
            ServiceTimingSource::Run
        );
        assert_eq!(
            select_service_timing(RecentSnapshotState::Final, recent, cumulative).0,
            ServiceTimingSource::Recent
        );
    }

    #[test]
    fn timing_metrics_report_each_boundary_at_its_own_sample_count() {
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..20 {
            timing.deadline_lateness.record(Duration::from_millis(1));
        }
        timing.render_duration.record(Duration::from_micros(100));
        timing.send_duration.record(Duration::from_micros(200));

        // The line states one count; a boundary repeats it only where its own
        // population differs. Render and the send call are recorded in
        // lockstep, so they stay silent; lateness carries the suppressed sends
        // that never reached a write, so it says so.
        let line_samples = timing.send_duration.sample_count();
        assert_eq!(
            timing_metric("deadline", timing.deadline_lateness, line_samples),
            "deadline worst 1.00 ms of 20"
        );
        assert_eq!(
            timing_metric("render", timing.render_duration, line_samples),
            "render worst 100 us"
        );
        assert_eq!(
            timing_metric("send call", timing.send_duration, line_samples),
            "send call worst 200 us"
        );
        assert_eq!(
            timing_metric("render", DurationHistogram::default(), line_samples),
            "render no samples"
        );
    }

    #[test]
    fn incomplete_serial_setup_is_not_described_as_network_capacity() {
        assert_eq!(
            unavailable_line_capacity_label(ConnKind::Serial),
            "complete Serial setup"
        );
        assert_eq!(
            unavailable_line_capacity_label(ConnKind::Udp),
            "network line unmeasured"
        );
    }

    #[test]
    fn capacity_demand_reuses_exact_memoized_wire_lengths() {
        let drafts = [
            ScheduleDraft {
                payload_kind: PayloadKind::Utf8,
                utf8_text: "hello".to_owned(),
                interval_ms: "1000".to_owned(),
                ..ScheduleDraft::default()
            },
            ScheduleDraft {
                payload_kind: PayloadKind::Hex,
                hex_data: "DE AD".to_owned(),
                interval_ms: "500".to_owned(),
                ..ScheduleDraft::default()
            },
        ];
        let analyses: Vec<_> = drafts
            .iter()
            .map(|draft| {
                let mut cache = MessageAnalysisCache::default();
                cache.refresh(draft);
                cache
            })
            .collect();

        assert_eq!(analyses[0].analysis.as_ref().unwrap().wire_len, Some(5));
        assert_eq!(analyses[1].analysis.as_ref().unwrap().wire_len, Some(2));
        let demand = analyzed_channel_demand(drafts.len(), &analyses).unwrap();
        assert_eq!(demand.messages_per_second, 3.0);
        assert_eq!(demand.bytes_per_second, 9.0);
    }

    #[test]
    fn incomplete_message_withholds_capacity_instead_of_understating_it() {
        let drafts = [ScheduleDraft {
            payload_kind: PayloadKind::Utf8,
            utf8_text: "hello".to_owned(),
            interval_ms: "not-a-number".to_owned(),
            ..ScheduleDraft::default()
        }];
        let mut cache = MessageAnalysisCache::default();
        cache.refresh(&drafts[0]);

        assert_eq!(analyzed_channel_demand(drafts.len(), &[cache]), None);
    }

    #[test]
    fn precise_near_threshold_status_names_the_deadline_window_policy() {
        let (detail, hot) = timer_status_detail(TimerStatus {
            mode: TimerMode::WindowsOneMillisecond,
            reason: TimerReason::PrecisionWindow,
            active_cadence: Some(ActiveCadence {
                messages: 1,
                shortest: Duration::from_millis(50),
            }),
            cadence_alignment: CadenceAlignment::Immediate,
            clock_realignments: 0,
        });

        assert!(detail.contains("deadline windows"), "{detail}");
        assert!(!detail.contains("continuous"), "{detail}");
        assert!(!hot);
    }

    #[test]
    fn dormant_timer_status_names_idle_state_and_no_request() {
        // One idle state now, not two: with no configured mode there is no
        // "Precise selected but idle" to distinguish from plain idle.
        let (standard, hot) = timer_status_detail(TimerStatus::default());
        assert_eq!(standard, "idle · no active messages; no timer request");
        assert!(!hot);
    }
}
