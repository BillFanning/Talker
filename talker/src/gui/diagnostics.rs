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
    service_sample_count, ChannelDemand, MessageDemand, MIN_SERVICE_SAMPLES,
};
use crate::core::telemetry::{RecentSnapshotState, SendTimingTelemetry};
use crate::core::timing::{TimerMode, TimerReason, TimerStatus, TimingMode};
use wiredata_ui::diagnostics::SignalTone;
use wiredata_ui::format::compact_duration;

use super::MessageAnalysisCache;

pub(super) const TIMING_WARMUP_SAMPLES: u64 = 20;

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
not prove physical-wire or peer delivery. Each boundary has its own sample count and warm-up. \
Percentiles are histogram-bucket upper bounds.";

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

pub(super) fn recent_timing_metric(
    label: &str,
    histogram: crate::core::telemetry::DurationHistogram,
) -> String {
    let samples = histogram.sample_count();
    if samples == 0 {
        return format!("{label} no samples");
    }
    if samples < TIMING_WARMUP_SAMPLES {
        return format!(
            "{label} warming {samples}/{TIMING_WARMUP_SAMPLES} (max {})",
            compact_duration(histogram.max().unwrap_or_default())
        );
    }
    format!(
        "{label} p99 ≤ {}",
        compact_duration(histogram.percentile_upper_bound(99).unwrap_or_default())
    )
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
            "Send outcomes: {scheduled} scheduled - {failed} failed - {suppressed} suppressed \
             - {missed} missed = {sent} sent"
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
        "{scheduled} scheduled counts every cadence point this run's schedule produced, less \
         the three ways a send does not go out. {failed} failed: the interface write was \
         attempted and returned an error. {suppressed} suppressed: after a failure, the send \
         was withheld during retry backoff and never attempted — these follow failures and \
         cannot occur without one. {missed} missed: the runner fell more than one interval \
         behind, so the cadence point was skipped before any send existed. The remaining \
         {sent} sent means the interface write returned success; it does not confirm that \
         bytes reached the wire or that any peer received them."
    )
}

pub(super) fn relative_to_shortest(
    duration: std::time::Duration,
    shortest: std::time::Duration,
    upper_bound: bool,
) -> String {
    if shortest.is_zero() {
        return String::new();
    }
    let percentage = duration.as_secs_f64() / shortest.as_secs_f64() * 100.0;
    let bound = if upper_bound { "≤" } else { "" };
    format!(
        " ({bound}{percentage:.1}% of shortest {})",
        compact_duration(shortest)
    )
}

pub(super) fn cadence_decision(
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
    shortest: Option<std::time::Duration>,
    snapshot_state: RecentSnapshotState,
) -> DecisionSignal {
    let recent_samples = recent.deadline_lateness.sample_count();
    let run_samples = cumulative.deadline_lateness.sample_count();
    let snapshot_label = recent_snapshot_label(snapshot_state);
    let shortest_text = shortest
        .map(|interval| format!(" · shortest {}", compact_duration(interval)))
        .unwrap_or_default();

    let text = if let RecentSnapshotState::Expired(_) = snapshot_state {
        let run_max = cumulative.deadline_lateness.max().unwrap_or_default();
        if run_samples == 0 {
            format!(
                "{} · no deadline samples in run",
                title_case(snapshot_label)
            )
        } else {
            format!(
                "{} · run max late {}{}",
                title_case(snapshot_label),
                compact_duration(run_max),
                shortest
                    .map(|interval| relative_to_shortest(run_max, interval, false))
                    .unwrap_or_default(),
            )
        }
    } else if run_samples == 0 {
        format!("Awaiting first deadline{shortest_text}")
    } else if recent_samples == 0 {
        let run_max = cumulative.deadline_lateness.max().unwrap_or_default();
        format!(
            "No sends in {snapshot_label} · run max late {}{}",
            compact_duration(run_max),
            shortest
                .map(|interval| relative_to_shortest(run_max, interval, false))
                .unwrap_or_default(),
        )
    } else if recent_samples < 20 {
        let recent_max = recent.deadline_lateness.max().unwrap_or_default();
        format!(
            "Warming up ({recent_samples} due) · max late {}{} · {snapshot_label}",
            compact_duration(recent_max),
            shortest
                .map(|interval| relative_to_shortest(recent_max, interval, false))
                .unwrap_or_default(),
        )
    } else {
        let recent_p99 = recent
            .deadline_lateness
            .percentile_upper_bound(99)
            .unwrap_or_default();
        let relative = shortest
            .map(|interval| relative_to_shortest(recent_p99, interval, true))
            .unwrap_or_default();
        format!(
            "Deadline lateness p99 ≤ {}{relative} · {snapshot_label}",
            compact_duration(recent_p99),
        )
    };

    DecisionSignal {
        text,
        // Lateness has no universal good/bad threshold. Keep it neutral and
        // let the exact value, normalized to the schedule, support the decision.
        tone: SignalTone::Neutral,
    }
}

pub(super) fn title_case(mut text: String) -> String {
    if let Some(first) = text.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    text
}

pub(super) fn timing_detail_text(
    recent: SendTimingTelemetry,
    cumulative: SendTimingTelemetry,
    snapshot_state: RecentSnapshotState,
) -> String {
    let run_samples = cumulative.deadline_lateness.sample_count();
    if run_samples == 0 {
        return "Timing: awaiting first deadline".to_owned();
    }

    let run_max = cumulative
        .deadline_lateness
        .max()
        .map(compact_duration)
        .unwrap_or_else(|| "n/a".to_owned());
    match snapshot_state {
        RecentSnapshotState::Expired(_) => format!(
            "Timing: {} · run max deadline lateness {run_max}",
            recent_snapshot_label(snapshot_state)
        ),
        RecentSnapshotState::Pending => {
            format!("Timing: recent snapshot pending · run max deadline lateness {run_max}")
        }
        RecentSnapshotState::Current(_) | RecentSnapshotState::Final => format!(
            "Timing ({}): {} · {} · {} · run max deadline lateness {run_max}",
            recent_snapshot_label(snapshot_state),
            recent_timing_metric("deadline", recent.deadline_lateness),
            recent_timing_metric("render", recent.render_duration),
            recent_timing_metric("send call", recent.send_duration),
        ),
    }
}

pub(super) fn timer_status_detail(status: TimerStatus) -> (String, bool) {
    let shortest = status
        .shortest_active_interval
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
            format!("Precise · Windows 1 ms deadline windows · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::WindowsRequestFailed) => (
            format!("Precise · Windows 1 ms request failed · shortest {shortest}"),
            true,
        ),
        (TimerReason::PrecisionWindow, TimerMode::NativeDeadlineWaits) => (
            format!("Precise · native deadline waits · shortest {shortest}"),
            false,
        ),
        (TimerReason::PrecisionWindow, TimerMode::Standard) => (
            format!(
                "Precise selected · first waited deadline window pending · shortest {shortest}"
            ),
            false,
        ),
        (TimerReason::None, _)
            if status.timing_mode == TimingMode::Precise
                && status.shortest_active_interval.is_none() =>
        {
            (
                "idle · Precise selected; no timer request".to_owned(),
                false,
            )
        }
        (TimerReason::None, _) if status.shortest_active_interval.is_some() => {
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
        analyzed_channel_demand, cadence_decision, diagnostic_card_tone, recent_timing_metric,
        select_service_timing, send_outcomes, send_outcomes_tooltip, timer_status_detail,
        timing_detail_text, unavailable_line_capacity_label, ServiceTimingSource,
    };
    use crate::core::telemetry::{RecentSnapshotState, SendTimingTelemetry};
    use crate::core::timing::{CadenceAlignment, TimerMode, TimerReason, TimerStatus, TimingMode};
    use crate::gui::{
        draft::{ConnKind, PayloadKind, ScheduleDraft},
        MessageAnalysisCache,
    };
    use wiredata_ui::diagnostics::SignalTone;
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

    #[test]
    fn cadence_decision_switches_from_warmup_to_normalized_p99_at_twenty_samples() {
        let mut recent = SendTimingTelemetry::default();
        for _ in 0..19 {
            recent.deadline_lateness.record(Duration::from_millis(1));
        }

        let warming = cadence_decision(
            recent,
            recent,
            Some(Duration::from_millis(50)),
            RecentSnapshotState::Current(Duration::ZERO),
        );
        assert_eq!(
            warming.text,
            "Warming up (19 due) · max late 1.00 ms (2.0% of shortest 50.0 ms) · recent snapshot"
        );
        assert_eq!(warming.tone, SignalTone::Neutral);

        recent.deadline_lateness.record(Duration::from_millis(1));
        let ready = cadence_decision(
            recent,
            recent,
            Some(Duration::from_millis(50)),
            RecentSnapshotState::Current(Duration::ZERO),
        );
        assert_eq!(
            ready.text,
            "Deadline lateness p99 ≤ 1.00 ms (≤2.0% of shortest 50.0 ms) · recent snapshot"
        );
        assert_eq!(ready.tone, SignalTone::Neutral);
    }

    #[test]
    fn expired_snapshot_is_not_presented_as_current_cadence_or_timing() {
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..20 {
            timing.deadline_lateness.record(Duration::from_millis(1));
        }
        let state = RecentSnapshotState::Expired(Duration::from_secs(11));

        let cadence = cadence_decision(timing, timing, Some(Duration::from_millis(50)), state);
        assert_eq!(
            cadence.text,
            "Recent snapshot expired · as of 11.0 s ago · run max late 1.00 ms (2.0% of shortest 50.0 ms)"
        );
        assert_eq!(
            timing_detail_text(timing, timing, state),
            "Timing: recent snapshot expired · as of 11.0 s ago · run max deadline lateness 1.00 ms"
        );
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
    fn timing_metrics_warm_up_independently() {
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..20 {
            timing.deadline_lateness.record(Duration::from_millis(1));
        }
        timing.render_duration.record(Duration::from_micros(100));
        timing.send_duration.record(Duration::from_micros(200));

        assert_eq!(
            recent_timing_metric("deadline", timing.deadline_lateness),
            "deadline p99 ≤ 1.00 ms"
        );
        assert_eq!(
            recent_timing_metric("render", timing.render_duration),
            "render warming 1/20 (max 100 us)"
        );
        assert_eq!(
            recent_timing_metric("send call", timing.send_duration),
            "send call warming 1/20 (max 200 us)"
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
            timing_mode: TimingMode::Precise,
            reason: TimerReason::PrecisionWindow,
            shortest_active_interval: Some(Duration::from_millis(50)),
            cadence_alignment: CadenceAlignment::Immediate,
            clock_realignments: 0,
        });

        assert!(detail.contains("deadline windows"), "{detail}");
        assert!(!detail.contains("continuous"), "{detail}");
        assert!(!hot);
    }

    #[test]
    fn dormant_timer_status_names_idle_state_and_no_request() {
        let (precise, hot) = timer_status_detail(TimerStatus {
            timing_mode: TimingMode::Precise,
            ..TimerStatus::default()
        });
        assert_eq!(precise, "idle · Precise selected; no timer request");
        assert!(!hot);

        let (standard, hot) = timer_status_detail(TimerStatus::default());
        assert_eq!(standard, "idle · no active messages; no timer request");
        assert!(!hot);
    }
}
