//! The detail pane: everything about the **selected** channel — header
//! (name, summary, actions), the Connection editor, the Messages
//! editor, and the Output display pane. One channel on screen at a time;
//! the channel list in [`super::channels`] picks which.

use egui::{Align, Layout};

use crate::core::{
    capacity::{
        measured_service_estimate, serial_line_estimate, service_sample_count, ChannelDemand,
        MessageDemand, MIN_SERVICE_SAMPLES,
    },
    channel::InterfaceConfig,
    message::NmeaChecksumMode,
    run_summary::RunSummary,
    telemetry::{recent_snapshot_state, RecentSnapshotState, SendTimingTelemetry},
    timing::{CadenceAlignment, TimerMode, TimerReason, TimerStatus, TimingMode},
};

use wiredata_ui::{
    diagnostics::{attention_callout, decision_card, signal_grid, signal_row, SignalTone},
    fonts::bold,
    format::{compact_duration, human_bytes},
    glyphs,
};

use super::draft::{ConnKind, PayloadKind, ScheduleDraft};
use super::widgets::{
    checksum_label, code_page_label, hex_valid, invalid_parse, lifecycle_indicator,
    marker_aware_text_edit, message_editor_max_height, plain_text_edit_with_cursor,
    preview_ascii_layout_job, red_bordered, show_display_pane, show_insert_byte_button,
    show_insert_unit_button, show_interface_summary, show_serial_fields, show_tcp_fields,
    show_udp_fields, start_button, UppercaseHex,
};
use super::{MessageAnalysisCache, MessageDraftAnalysis, MessagePreview, TalkerApp};
use wiredata_ui::palette::active as theme_palette;

const TIMING_WARMUP_SAMPLES: u64 = 20;

const LOCAL_ACCEPTANCE_TOOLTIP: &str = "Accepted means the configured-interface write returned \
success. It does not confirm that serial bits reached the wire, a network packet left the host, \
or a peer received the data.";

const THROUGHPUT_TOOLTIP: &str = "Rolling five-second average of messages and bytes whose \
configured-interface writes were accepted locally. Failed, retry-suppressed, and missed \
occurrences are excluded. The fixed five-second denominator makes the value ramp during startup \
and decay after traffic stops; it is not instantaneous line rate or proof of physical-wire or \
peer delivery.";

const TIMING_TOOLTIP: &str = "Each recent snapshot merges up to approximately ten seconds of \
fixed one-second segments ending when the runner captured its latest counter update. A slow or \
dormant schedule can therefore leave the displayed snapshot unchanged; its “as of” age is the \
snapshot compute time, not necessarily the newest sample time. While running, a snapshot is no \
longer used as recent evidence once that age reaches ten seconds. After a normal run end, the \
channel retains its exact final snapshot; an abnormal exit can leave the last non-final snapshot \
instead. Deadline lateness runs from a message's monotonic cadence deadline \
until the runner handles that occurrence; handled retry-suppressed occurrences are included, while \
cadence points counted as Missed are not sampled. Render covers payload and timestamp construction. \
Send call ends when the configured-interface write returns and includes failed attempts; it does \
not prove physical-wire or peer delivery. Each boundary has its own sample count and warm-up. \
Percentiles are histogram-bucket upper bounds.";

const TIMER_TOOLTIP: &str = "The shortest active interval selects the deadline-wait policy. On \
Windows, intervals below 32 ms hold the shared process-wide 1 ms timer-resolution request in \
either mode. At 32 ms or longer, Standard uses ordinary deadline waits; Precise requests 1 ms \
only for the final 32 ms before a waited deadline. An Immediate schedule's first occurrence has \
no preceding precision window. This channel releases a bounded-window request before rendering \
and writing, although another channel may keep the process-wide request active. Commands interrupt \
both wait stages. Other platforms use native deadline waits. This affects wake timing, not \
timestamp accuracy or physical-wire arrival.";

const ALIGNMENT_TOOLTIP: &str = "Immediate makes every active message due when the interface \
opens. UTC phase places each first application deadline on the strict next Unix-epoch multiple of \
its interval, then advances on monotonic deadlines. For example, 1000 ms aligns to whole UTC \
seconds, while 1500 ms alternates between whole- and half-second phases. When the runner loops, it \
compares wall clock with its elapsed-time projection no more often than once per second; a \
displacement of at least 250 ms rebuilds future deadlines without replaying bypassed points or \
adding scheduler misses. This aligns application deadlines, not completion of an interface write \
or physical-wire arrival.";

const DISPLAY_QUEUE_TOOLTIP: &str = "The current value was sampled immediately before the UI's \
last drain of the runner-to-UI diagnostic queue; it is not the post-drain depth. Peak is the \
largest such UI sample, not an exact queue high-water mark. A dropped update may be a payload \
sample, counter snapshot, timer change, or interface error/recovery notice. The runner never waits \
for this queue, so display pressure cannot delay sending. Live readouts can lag until a later \
cumulative update; the final run snapshot remains exact. Reliable command results use a separate \
queue.";

fn analyzed_channel_demand(
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

fn compact_rate(value: f64, unit: &str) -> String {
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

fn compact_factor(factor: f64) -> String {
    if factor >= 1_000.0 {
        ">999x".to_owned()
    } else if factor >= 10.0 {
        format!("{factor:.1}x")
    } else {
        format!("{factor:.2}x")
    }
}

fn recent_snapshot_label(state: RecentSnapshotState) -> String {
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
enum ServiceTimingSource {
    Recent,
    Run,
}

fn select_service_timing(
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

fn service_timing_source_label(source: ServiceTimingSource, state: RecentSnapshotState) -> String {
    match source {
        ServiceTimingSource::Recent => recent_snapshot_label(state),
        ServiceTimingSource::Run if matches!(state, RecentSnapshotState::Expired(_)) => {
            format!("run-wide fallback; {}", recent_snapshot_label(state))
        }
        ServiceTimingSource::Run => "run-wide".to_owned(),
    }
}

fn recent_timing_metric(
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

fn unavailable_line_capacity_label(kind: ConnKind) -> &'static str {
    match kind {
        ConnKind::Serial => "complete Serial setup",
        ConnKind::Udp | ConnKind::Tcp => "network line unmeasured",
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DecisionSignal {
    text: String,
    tone: SignalTone,
}

fn diagnostic_card_tone(
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

fn delivery_decision(sent: u64, failed: u64, suppressed: u64, missed: u64) -> DecisionSignal {
    let unsent = failed.saturating_add(suppressed).saturating_add(missed);
    let scheduled = sent.saturating_add(unsent);
    if scheduled == 0 {
        return DecisionSignal {
            text: "No scheduled sends yet".to_owned(),
            tone: SignalTone::Neutral,
        };
    }

    let unsent_pct = unsent as f64 / scheduled as f64 * 100.0;
    DecisionSignal {
        text: if unsent == 0 {
            format!("{sent} / {scheduled} accepted · none unsent")
        } else {
            format!("{sent} / {scheduled} accepted · {unsent} unsent ({unsent_pct:.1}%)")
        },
        tone: if failed > 0 {
            SignalTone::Fault
        } else if unsent > 0 {
            SignalTone::Warning
        } else {
            SignalTone::Healthy
        },
    }
}

fn relative_to_shortest(
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

fn cadence_decision(
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
            "No fires in {snapshot_label} · run max late {}{}",
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

fn title_case(mut text: String) -> String {
    if let Some(first) = text.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    text
}

fn timing_detail_text(
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

fn timer_status_detail(status: TimerStatus) -> (String, bool) {
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

fn show_last_run_summary(ui: &mut egui::Ui, summary: &RunSummary) {
    let unsent = summary.unsent_sends();
    let heading = format!(
        "Last completed run · {} · {} accepted locally · {unsent} unsent",
        compact_duration(summary.elapsed),
        summary.total_count,
    );
    egui::CollapsingHeader::new(heading)
        .id_salt("last_completed_run")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Copy summary").clicked() {
                    ui.ctx().copy_text(summary.to_report_text());
                }
                ui.weak(format!(
                    "Talker {} · {} · {}/{}",
                    env!("CARGO_PKG_VERSION"),
                    if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    },
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                ));
            });
            ui.weak(format!(
                "Started {} · finished {}",
                summary.started_utc(),
                summary.finished_utc(),
            ));
            ui.weak(format!(
                "Accepted locally: {} · {} msgs · Unsent: {} ({} failed · {} suppressed · {} missed)",
                human_bytes(summary.total_bytes),
                summary.total_count,
                unsent,
                summary.failed_sends,
                summary.suppressed_sends,
                summary.missed_sends,
            ))
            .on_hover_text(LOCAL_ACCEPTANCE_TOOLTIP);
            let (timer_detail, timer_hot) = timer_status_detail(summary.timer);
            let timer = egui::RichText::new(format!("Timer: {timer_detail}")).weak();
            ui.label(if timer_hot {
                timer.color(theme_palette(ui).warning_amber)
            } else {
                timer
            })
            .on_hover_text(TIMER_TOOLTIP);

            let timing = summary.timing.cumulative;
            let p99 = |histogram: crate::core::telemetry::DurationHistogram| {
                histogram
                    .percentile_upper_bound(99)
                    .map(compact_duration)
                    .unwrap_or_else(|| "n/a".to_owned())
            };
            let timing_text = if timing.deadline_lateness.sample_count() == 0 {
                "Timing: no deadline samples".to_owned()
            } else {
                format!(
                    "Timing (run): late p99 ≤ {} · render p99 ≤ {} · send call p99 ≤ {} · max late {}",
                    p99(timing.deadline_lateness),
                    p99(timing.render_duration),
                    p99(timing.send_duration),
                    timing
                        .deadline_lateness
                        .max()
                        .map(compact_duration)
                        .unwrap_or_else(|| "n/a".to_owned()),
                )
            };
            ui.weak(timing_text).on_hover_text(TIMING_TOOLTIP);
        });
}

impl TalkerApp {
    /// Render the central detail pane for the selected channel (or a hint
    /// when there is none).
    pub(super) fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(i) = self.selected.filter(|&i| i < self.conn_drafts.len()) else {
            ui.centered_and_justified(|ui| {
                ui.weak(if self.conn_drafts.is_empty() {
                    "No channels — use “+ Add” in the channel list to create one."
                } else {
                    "Select a channel on the left."
                });
            });
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.push_id(i, |ui| {
                let running = self.is_connection_running(i);
                self.show_channel_header(ui, i, running);
                ui.separator();
                self.show_channel_body(ui, i, running);
                ui.separator();
                // The accepted total proves whether Output payload updates
                // were omitted; retained Output history is a separate concern.
                let accepted_total = self
                    .sup
                    .telemetry_ref(i)
                    .map(|telemetry| telemetry.total_count)
                    .unwrap_or_default();
                show_display_pane(ui, &mut self.displays[i], accepted_total);
            });
        });
    }

    /// The detail header, laid out like listener's channel block so the two
    /// apps read as one product: name row (status glyph · name · label), a
    /// `status · interface` row, sent/unsent totals, throughput, the
    /// performance readouts for high-rate health, and the lifecycle button
    /// pair. Channel removal lives on the channel-list rows (the ✕ overlay),
    /// as in listener.
    fn show_channel_header(&mut self, ui: &mut egui::Ui, i: usize, running: bool) {
        let pal = theme_palette(ui);
        // Owned snapshot of the channel's telemetry (ADR-019): the readouts
        // are rendered across several `&mut self` widget closures.
        let telemetry = self.sup.telemetry(i);
        let recent_snapshot_state = recent_snapshot_state(
            telemetry.recent_timing_captured_at,
            std::time::Instant::now(),
            telemetry.recent_timing_is_final,
        );
        let error: Option<String> = telemetry.banner_error().map(str::to_owned);
        let (glyph, glyph_color, status_word) = lifecycle_indicator(running, error.is_some(), pal);
        let draft_kind = self.conn_drafts[i].kind();
        let draft_interface = self.conn_drafts.get(i).and_then(|draft| draft.to_config());
        let (iface_drift, run_drift) = self.detect_drift(i, draft_interface.as_ref());

        // Name row: status glyph (listener's symbol set/colors, painted into
        // a fixed cell so status changes never shift the row) + editable
        // name + label.
        ui.horizontal(|ui| {
            glyphs::paint_glyph(ui, glyph, glyphs::glyph_size(glyph), glyph_color);
            // Editable display name (cosmetic — channels are positional).
            // The hint shows the positional fallback the list uses when the
            // name is empty.
            let hint = format!("Channel {}", i + 1);
            let name_r = ui.add(
                egui::TextEdit::singleline(&mut self.conn_drafts[i].name)
                    .id_salt("channel_name")
                    .desired_width(140.0)
                    .hint_text(hint),
            );
            if name_r.changed() {
                self.dirty = true;
            }
            ui.label(bold("Name"))
                .on_hover_text("This channel's display name, shown in the channel list.");
            // Duplicate names are allowed (nothing is keyed by them) but
            // worth a nudge — two identical rows in the list are confusing.
            let name = &self.conn_drafts[i].name;
            let duplicate = !name.is_empty()
                && self
                    .conn_drafts
                    .iter()
                    .enumerate()
                    .any(|(j, d)| j != i && d.name == *name);
            ui.push_id("dup_name_hint", |ui| {
                if duplicate {
                    ui.label(
                        egui::RichText::new("duplicate name")
                            .color(pal.warning_amber)
                            .size(11.0),
                    )
                    .on_hover_text("Another channel has the same name — allowed, but confusing.");
                }
            });
        });

        // Status · interface row (listener's `running · details` line).
        // A stable id scopes the summary, whose red `?` pills may come and go
        // between egui's two layout passes.
        ui.horizontal(|ui| {
            ui.label(status_word);
            ui.label("·");
            ui.push_id("iface_summary", |ui| {
                show_interface_summary(ui, &self.conn_drafts[i]);
            });
        });

        // Decision-level diagnostics stay compact; the evidence and caveats
        // remain one click away in the card's details section.
        let detail_line = |ui: &mut egui::Ui, text: String, hot: bool, tip: &str| {
            let rt = egui::RichText::new(text).weak();
            ui.label(if hot { rt.color(pal.warning_amber) } else { rt })
                .on_hover_text(tip);
        };

        let msgs = telemetry.total_count;
        let bytes = telemetry.total_bytes;
        let (mps, bps) = self
            .rates
            .get(i)
            .map(|r| (r.per_sec, r.bytes_per_sec))
            .unwrap_or((0.0, 0.0));
        let missed = telemetry.missed_sends;
        let failed = telemetry.failed_sends;
        let suppressed = telemetry.suppressed_sends;
        let unsent = missed.saturating_add(failed).saturating_add(suppressed);
        let scheduled = msgs.saturating_add(unsent);
        let delivery = delivery_decision(msgs, failed, suppressed, missed);
        let unsent_tip = format!(
            "Run-to-date scheduler outcomes. Accepted means {msgs} configured-interface writes \
             returned success; it does not confirm physical-wire or peer delivery. Failed means \
             {failed} attempted writes returned an error. Suppressed means {suppressed} handled \
             occurrences were skipped during retry backoff. Missed means {missed} cadence-grid \
             points were skipped because the runner was more than one interval behind. Accepted, \
             failed, suppressed, and missed add up to the scheduled total."
        );

        // Capacity is computed from the current draft's memoized wire lengths,
        // never by re-rendering payloads in this per-frame header path.
        let demand = self
            .sched_drafts
            .get(i)
            .zip(self.message_analysis.get(i))
            .and_then(|(messages, analyses)| analyzed_channel_demand(messages.len(), analyses));
        let serial = demand.and_then(|demand| {
            if !demand.is_active() {
                return None;
            }
            let InterfaceConfig::Serial(config) = draft_interface.as_ref()? else {
                return None;
            };
            serial_line_estimate(demand, config)
        });
        let (service_source, service_timing) = select_service_timing(
            recent_snapshot_state,
            telemetry.recent_timing,
            telemetry.timing,
        );
        let service_source_label =
            service_timing_source_label(service_source, recent_snapshot_state);
        let service_estimate = demand
            .filter(|demand| demand.is_active())
            .and_then(|demand| measured_service_estimate(demand, service_timing));
        let service_samples = service_sample_count(service_timing);
        let draft_projection = running && (iface_drift || run_drift);
        let app_label = if draft_projection { "draft app" } else { "app" };

        let app_capacity = if let Some(estimate) = service_estimate {
            let mut text = format!(
                "{app_label} ~{} headroom",
                compact_factor(estimate.headroom_factor())
            );
            if service_source == ServiceTimingSource::Run
                && matches!(recent_snapshot_state, RecentSnapshotState::Expired(_))
            {
                text.push_str(" · run-wide timing (recent expired)");
            }
            text
        } else if service_samples == 0 {
            format!("{app_label} unmeasured")
        } else if service_samples < MIN_SERVICE_SAMPLES {
            format!("{app_label} warming {service_samples}/{MIN_SERVICE_SAMPLES}")
        } else {
            format!("{app_label} estimate unavailable")
        };
        let capacity = match demand {
            None => DecisionSignal {
                text: format!(
                    "Complete message setup to calculate · {} accepted (~5 s)",
                    compact_rate(f64::from(mps), "msg/s")
                ),
                tone: SignalTone::Neutral,
            },
            Some(demand) if !demand.is_active() => DecisionSignal {
                text: format!(
                    "No active messages · {} accepted (~5 s)",
                    compact_rate(f64::from(mps), "msg/s")
                ),
                tone: SignalTone::Neutral,
            },
            Some(demand) => {
                let line_capacity = if let Some(line) = serial {
                    format!("serial {:.1}%", line.utilization * 100.0)
                } else {
                    unavailable_line_capacity_label(draft_kind).to_owned()
                };
                DecisionSignal {
                    text: format!(
                        "{} requested / {} accepted (~5 s) · {line_capacity} · {app_capacity}",
                        compact_rate(demand.messages_per_second, "msg/s"),
                        compact_rate(f64::from(mps), "msg/s"),
                    ),
                    tone: if serial.is_some_and(|line| line.is_oversubscribed()) {
                        SignalTone::Fault
                    } else if serial.is_some_and(|line| line.utilization >= 0.8)
                        || service_estimate.is_some_and(|estimate| estimate.utilization >= 0.8)
                    {
                        SignalTone::Warning
                    } else {
                        SignalTone::Neutral
                    },
                }
            }
        };

        let cumulative_timing = telemetry.timing;
        let timing = telemetry.recent_timing;
        let timing_text = timing_detail_text(timing, cumulative_timing, recent_snapshot_state);
        let cadence_decision = cadence_decision(
            timing,
            cumulative_timing,
            telemetry
                .timer
                .shortest_active_interval
                .or_else(|| demand.and_then(|demand| demand.shortest_interval)),
            recent_snapshot_state,
        );

        let timer_prefix = if running {
            "Timer"
        } else if telemetry.timer.shortest_active_interval.is_some()
            || telemetry.timer.reason != TimerReason::None
        {
            "Timer (last run)"
        } else {
            "Timer"
        };
        let (timer_detail, timer_hot) = timer_status_detail(telemetry.timer);
        let cadence = match telemetry.timer.cadence_alignment {
            CadenceAlignment::Immediate => "immediate start".to_owned(),
            CadenceAlignment::UtcPhase if telemetry.timer.clock_realignments == 0 => {
                "UTC phase".to_owned()
            }
            CadenceAlignment::UtcPhase => format!(
                "UTC phase · {} wall-clock rebases",
                telemetry.timer.clock_realignments
            ),
        };
        let qlen = telemetry.queue_len;
        let qpeak = telemetry.queue_peak;
        let drops = telemetry.dropped_statuses;
        let card_tone =
            diagnostic_card_tone(delivery.tone, capacity.tone, timer_hot, drops, running);
        let card_status = match card_tone {
            SignalTone::Fault => "ISSUE",
            SignalTone::Warning => "ATTENTION",
            SignalTone::Healthy => "LIVE",
            SignalTone::Neutral => "IDLE",
        };

        decision_card(ui, "Send health", card_status, card_tone, |ui| {
            signal_grid(ui, "send_decisions", |ui| {
                signal_row(
                    ui,
                    "Interface outcomes",
                    &delivery.text,
                    delivery.tone,
                    &unsent_tip,
                );
                signal_row(
                        ui,
                        "Cadence",
                        &cadence_decision.text,
                        cadence_decision.tone,
                        "Deadline lateness is aggregated across all messages. Every handled due occurrence, including one suppressed during retry backoff, contributes a sample; cadence points counted as Missed do not. Faster messages therefore contribute more samples. p99 is a histogram-bucket upper bound. Any percentage compares the aggregate value with the displayed shortest active interval for schedule context; it is not a per-message percentage. The “as of” age tells you when the snapshot was computed. Once a running snapshot reaches ten seconds old, this row labels it expired instead of presenting its retained samples as current.",
                    );
                signal_row(
                        ui,
                        "Capacity",
                        &capacity.text,
                        capacity.tone,
                        "Requested load comes from the draft currently shown and may differ from the running configuration until Apply & Restart. Accepted rate is the rolling five-second average of configured-interface writes that returned success. Serial utilization is a theoretical UART line estimate. Application headroom compares the current draft with separate render and interface-write p99 bounds. A warmed recent snapshot is preferred while it is current; an expired snapshot is discarded and a clearly labelled run-wide fallback is used when available. This is an advisory projection, not a hard capacity promise.",
                    );
            });

            if unsent > 0 {
                ui.add_space(6.0);
                attention_callout(
                        ui,
                        "unsent_attention",
                        format!(
                            "{unsent} unsent · {failed} failed · {suppressed} suppressed · {missed} missed"
                        ),
                        if failed > 0 {
                            SignalTone::Fault
                        } else {
                            SignalTone::Warning
                        },
                        &unsent_tip,
                    );
            }
            if let Some(line) = serial.filter(|line| line.is_oversubscribed()) {
                ui.add_space(4.0);
                attention_callout(
                        ui,
                        "serial_capacity_attention",
                        format!(
                            "Serial demand is {:.1}% of line capacity · needs {} (baud {})",
                            line.utilization * 100.0,
                            compact_rate(line.required_bits_per_second, "bit/s"),
                            line.minimum_baud(),
                        ),
                        SignalTone::Fault,
                        "The sustained requested payload cannot physically fit at the configured baud. The warning remains advisory so deliberate overload tests are still possible.",
                    );
            }
            if timer_hot {
                ui.add_space(4.0);
                attention_callout(
                        ui,
                        "timer_request_attention",
                        "Windows 1 ms timer request failed; cadence waits are using the fallback",
                        SignalTone::Warning,
                        "The channel continues with ordinary deadline waits. The failed request does not prove that any occurrence was late; inspect measured deadline lateness and Missed cadence points for the observed effect.",
                    );
            }
            if drops > 0 {
                ui.add_space(4.0);
                attention_callout(
                    ui,
                    "display_drop_attention",
                    format!("{drops} diagnostic updates dropped; live readouts may lag"),
                    SignalTone::Warning,
                    DISPLAY_QUEUE_TOOLTIP,
                );
            }

            ui.add_space(5.0);
            egui::CollapsingHeader::new("Timing & runtime details")
                    .id_salt("timing_runtime_details")
                    .default_open(false)
                    .show(ui, |ui| {
                        detail_line(
                            ui,
                            format!(
                                "Interface outcomes: {} accepted locally · {msgs}/{scheduled} scheduled accepted · {unsent} unsent",
                                human_bytes(bytes)
                            ),
                            unsent > 0,
                            &unsent_tip,
                        );
                        detail_line(
                            ui,
                            format!(
                                "Accepted rate (~5 s): {:.1} kB/s · {:.1} msg/s",
                                bps / 1000.0,
                                mps
                            ),
                            false,
                            THROUGHPUT_TOOLTIP,
                        );

                        match demand {
                            None => detail_line(
                                ui,
                                "Capacity: complete all messages to calculate".to_owned(),
                                false,
                                "Capacity uses exact compiled wire lengths and active message intervals. Fix the message validation errors first.",
                            ),
                            Some(demand) if !demand.is_active() => detail_line(
                                ui,
                                "Capacity: no active messages".to_owned(),
                                false,
                                "Messages with interval 0 are dormant and create no scheduled wire demand.",
                            ),
                            Some(demand) => {
                                let requested = format!(
                                    "{} · {}",
                                    compact_rate(demand.messages_per_second, "msg/s"),
                                    compact_rate(demand.bytes_per_second, "B/s")
                                );
                                if let Some(line) = serial {
                                    let percentage = line.utilization * 100.0;
                                    let (text, hot) = if line.is_oversubscribed() {
                                        (
                                            format!(
                                                "Capacity: {requested} · serial OVER CAPACITY {percentage:.1}% · needs {} (baud {})",
                                                compact_rate(line.required_bits_per_second, "bit/s"),
                                                line.minimum_baud(),
                                            ),
                                            true,
                                        )
                                    } else if let Some(headroom) = line.headroom_factor() {
                                        (
                                            format!(
                                                "Capacity: {requested} · serial {percentage:.1}% · {} line headroom",
                                                compact_factor(headroom)
                                            ),
                                            line.utilization >= 0.8,
                                        )
                                    } else {
                                        (format!("Capacity: {requested} · serial idle"), false)
                                    };
                                    detail_line(
                                        ui,
                                        text,
                                        hot,
                                        "Static draft estimate. Each compiled wire byte uses one UART frame: 1 start bit plus the configured data, parity, and stop bits. Above 100%, the sustained requested payload cannot physically fit at the configured baud. At or below 100% is not a real-time guarantee: flow control, adapter/driver buffering, operating-system delays, and same-deadline message bursts can reduce effective headroom. This warning is advisory so deliberate overload tests remain possible.",
                                    );
                                } else if draft_kind == ConnKind::Serial {
                                    detail_line(
                                        ui,
                                        format!(
                                            "Capacity: requested {requested} · complete Serial setup"
                                        ),
                                        false,
                                        "Complete the Serial port and line settings before Talker can calculate UART utilization. The requested message and byte rates still come from exact compiled wire lengths and active intervals.",
                                    );
                                } else {
                                    detail_line(
                                        ui,
                                        format!("Capacity: requested {requested}"),
                                        false,
                                        "Static draft demand from exact compiled wire lengths and active intervals. Network link capacity is unknown, so Talker reports requested load without inventing a physical headroom figure.",
                                    );
                                }

                                if let Some(estimate) = service_estimate {
                                    let estimate_kind = if !running {
                                        "draft projection from retained timing"
                                    } else if draft_projection {
                                        "draft projection"
                                    } else {
                                        "running estimate"
                                    };
                                    detail_line(
                                        ui,
                                        format!(
                                            "Application headroom ({estimate_kind}; {service_source_label}): {} · summed p99 bounds ≤ {} · capacity ~{}",
                                            compact_factor(estimate.headroom_factor()),
                                            compact_duration(estimate.summed_p99_upper_bounds),
                                            compact_rate(estimate.capacity_messages_per_second, "msg/s"),
                                        ),
                                        estimate.utilization >= 0.8,
                                        "Advisory projection: the separate render and configured-interface write p99 histogram upper bounds are added, then compared with the current on-screen draft's aggregate requested message rate. A current recent snapshot is preferred after 20 paired observations. Otherwise the run-wide histogram is used after it warms up; a running recent snapshot is always discarded once its capture age reaches ten seconds. If unapplied edits exist, this deliberately projects the draft using timing retained from the running or previous configuration. It is not a joint p99 or hard capacity promise. A write may return after driver/kernel buffering, and coincident due messages still serialize.",
                                    );
                                } else {
                                    let text = if service_samples == 0 {
                                        "Estimated app headroom: run channel to measure".to_owned()
                                    } else if service_samples < MIN_SERVICE_SAMPLES {
                                        format!(
                                            "Estimated app headroom: warming up ({service_samples}/{MIN_SERVICE_SAMPLES} send attempts)"
                                        )
                                    } else {
                                        "Estimated app headroom: unavailable from the observed bounds".to_owned()
                                    };
                                    detail_line(
                                        ui,
                                        text,
                                        false,
                                        "At least 20 paired render/send-call observations are required before estimating application-service headroom. Slow schedules can use the cumulative run once enough samples exist.",
                                    );
                                }
                            }
                        }

                        detail_line(
                            ui,
                            timing_text,
                            false,
                            TIMING_TOOLTIP,
                        );
                        detail_line(
                            ui,
                            format!("{timer_prefix}: {timer_detail}"),
                            timer_hot,
                            TIMER_TOOLTIP,
                        );
                        detail_line(
                            ui,
                            format!("Cadence alignment: {cadence}"),
                            false,
                            ALIGNMENT_TOOLTIP,
                        );
                        detail_line(
                            ui,
                            format!(
                                "Display update queue before last drain: {qlen}/{} (sampled peak {qpeak}, {drops} dropped)",
                                super::STATUS_QUEUE_CAP
                            ),
                            qpeak * 2 >= super::STATUS_QUEUE_CAP || drops > 0,
                            DISPLAY_QUEUE_TOOLTIP,
                        );
                    });
        });

        if let Some(err) = &error {
            ui.colored_label(pal.fault_red, format!("\u{26A0} {err}"));
        }

        ui.add_space(12.0); // a blank line between the readouts and the buttons
        self.show_lifecycle_buttons(ui, i, running, iface_drift || run_drift, error.is_some());
        if let Some(summary) = self.sup.last_run_summary(i) {
            ui.add_space(4.0);
            show_last_run_summary(ui, summary);
        }
    }

    /// The lifecycle button pair (listener's control row): [Start Channel /
    /// Apply & Restart / Retry Channel] [Stop Channel], both always present
    /// at the shared control size; the Start side's label/enabled state is
    /// the pure [`start_button`] decision. Start, Retry, and Apply & Restart
    /// are all the same deferred action — `start_connection` stops any
    /// current runner, applies the drafts (interface + messages), and starts.
    fn show_lifecycle_buttons(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        running: bool,
        drift: bool,
        has_error: bool,
    ) {
        // Matches listener's CONTROL_BUTTON_SIZE so the two detail panes
        // read identically; text wider than the min grows the button.
        const SIZE: egui::Vec2 = egui::vec2(96.0, 32.0);
        // A running channel with drift needs the same full draft validation
        // as a stopped channel. Keep the reasons here so the button state and
        // its disabled tooltip are derived from one result.
        let blockers = if running && !drift {
            Vec::new()
        } else {
            super::widgets::start_blockers_analyzed(
                &self.conn_drafts[i],
                &self.sched_drafts[i],
                &self.message_analysis[i],
            )
        };
        let can_start = blockers.is_empty();
        let (label, enabled) = start_button(running, has_error, drift, can_start);
        ui.horizontal(|ui| {
            let mut btn = ui.add_enabled(enabled, egui::Button::new(label).min_size(SIZE));
            if !enabled && (!running || drift) {
                // The disabled hover must chain off the same Response as the
                // add, or egui won't show it.
                let tip = blockers.join("\n");
                btn = btn.on_disabled_hover_text(if tip.is_empty() {
                    "Add a valid message first".to_string()
                } else {
                    tip
                });
            }
            if label == "Apply & Restart" && enabled {
                btn = btn.on_hover_text(
                    "Stops the current send loop, applies the edited interface \
                     and messages, and starts again. Interface-only edits can \
                     also be applied live by pressing Enter in the edited field.",
                );
            }
            if btn.clicked() {
                self.deferred.start = Some(i);
            }
            if ui
                .add_enabled(running, egui::Button::new("Stop Channel").min_size(SIZE))
                .clicked()
            {
                self.deferred.stop = Some(i);
            }
        });
    }

    fn show_channel_body(&mut self, ui: &mut egui::Ui, i: usize, running: bool) {
        // "Configure connection" — the shared section title in both apps
        // (listener's Configure section uses the same words). Stays a plain
        // collapsing section — it does NOT auto-collapse on run (you often
        // want the interface params visible while a channel is live).
        // Default open; the user's expand/collapse choice persists via the
        // stable id_salt.
        let (changed, refresh) = egui::CollapsingHeader::new("Configure connection")
            .id_salt(("conn_section", i))
            .default_open(true)
            .show(ui, |ui| {
                let interface_result = match self.conn_drafts[i].kind() {
                    // Each kind gets its own push_id namespace so the very
                    // different widget trees produced by Serial / UDP / TCP can't
                    // shift each other's auto-ids across egui's two layout passes.
                    ConnKind::Serial => {
                        ui.push_id("serial_body", |ui| {
                            show_serial_fields(ui, &mut self.conn_drafts[i], &self.serial_ports)
                        })
                        .inner
                    }
                    ConnKind::Udp => {
                        ui.push_id("udp_body", |ui| {
                            (show_udp_fields(ui, &mut self.conn_drafts[i]), false)
                        })
                        .inner
                    }
                    ConnKind::Tcp => {
                        ui.push_id("tcp_body", |ui| {
                            (show_tcp_fields(ui, &mut self.conn_drafts[i]), false)
                        })
                        .inner
                    }
                };

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Timing");
                    let before = self.conn_drafts[i].timing_mode;
                    ui.radio_value(
                        &mut self.conn_drafts[i].timing_mode,
                        TimingMode::Standard,
                        "Standard",
                    )
                    .on_hover_text(
                        "Uses ordinary platform deadline waits at intervals of 32 ms or longer. On Windows, Talker still requests the shared process-wide 1 ms timer resolution continuously when the shortest active interval is below 32 ms. This affects deadline wake timing, not cadence phase, timestamp accuracy, or physical-wire arrival. Requires Apply & Restart.",
                    );
                    ui.radio_value(
                        &mut self.conn_drafts[i].timing_mode,
                        TimingMode::Precise,
                        "Precise",
                    )
                    .on_hover_text(format!("{TIMER_TOOLTIP} Requires Apply & Restart."));
                    if self.conn_drafts[i].timing_mode != before {
                        self.dirty = true;
                    }
                });
                let mut utc_aligned =
                    self.conn_drafts[i].cadence_alignment == CadenceAlignment::UtcPhase;
                if ui
                    .checkbox(&mut utc_aligned, "Align sends to UTC interval boundaries")
                    .on_hover_text(format!(
                        "{ALIGNMENT_TOOLTIP} Requires Apply & Restart."
                    ))
                    .changed()
                {
                    self.conn_drafts[i].cadence_alignment = if utc_aligned {
                        CadenceAlignment::UtcPhase
                    } else {
                        CadenceAlignment::Immediate
                    };
                    self.dirty = true;
                }
                interface_result
            })
            .body_returned
            .unwrap_or((false, false));
        if changed {
            self.deferred.apply.push(i);
        }
        if refresh {
            self.deferred.refresh_ports = true;
        }

        ui.separator();
        // Owned (the schedule section takes `&mut self` state alongside it),
        // but clone only the counts Vec, not the whole telemetry struct.
        let per_message_counts = self
            .sup
            .telemetry_ref(i)
            .map(|t| t.per_message_counts.clone())
            .unwrap_or_default();
        let interval_changes = show_schedule_section(
            ui,
            &mut self.sched_drafts[i],
            &mut self.message_analysis[i],
            &mut self.dirty,
            &per_message_counts,
            running,
        );
        for (msg_index, interval_ms) in interval_changes {
            if self.sup.is_running(i) {
                // Undeliverable changes surface in the channel telemetry.
                let _ = self.sup.set_interval(i, msg_index, interval_ms);
            }
        }
    }
}

// ── Inline message editor ─────────────────────────────────────────────────────

fn show_schedule_section(
    ui: &mut egui::Ui,
    entries: &mut Vec<ScheduleDraft>,
    analyses: &mut Vec<MessageAnalysisCache>,
    dirty: &mut bool,
    per_message_counts: &[u64],
    channel_running: bool,
) -> Vec<(usize, u64)> {
    let mut to_remove: Option<usize> = None;
    let mut add_one = false;
    // Message indices whose interval was committed this frame, with the new value.
    let mut interval_changes: Vec<(usize, u64)> = Vec::new();

    // Sent totals and drop counts live in the detail header now; this
    // header is just the section title. `id_salt` keeps the persistent
    // open/closed state stable when the message count changes the label.
    // (The old stacked-card layout auto-collapsed this section on
    // Start; in the detail pane there's room, so the section just
    // honours whatever the user last chose.)
    analyses.resize_with(entries.len(), MessageAnalysisCache::default);
    let n = entries.len();
    let header = if n == 0 {
        "Configure messages — (none)".to_string()
    } else {
        format!(
            "Configure messages — {n} message{}",
            if n == 1 { "" } else { "s" }
        )
    };
    egui::CollapsingHeader::new(header)
        .id_salt("messages_section")
        .default_open(true)
        .show(ui, |ui| {
            for (i, entry) in entries.iter_mut().enumerate() {
                let analysis_cache = &mut analyses[i];
                ui.push_id(i, |ui| {
                    ui.group(|ui| {
                        let mut content_changed = false;
                        ui.horizontal(|ui| {
                            ui.strong(format!("Message {}", i + 1));
                            ui.separator();
                            let before_kind = entry.payload_kind;
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Nmea, "NMEA");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Ascii, "ASCII");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Utf8, "UTF-8");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Utf16, "UTF-16");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Hex, "Hex");
                            content_changed |= entry.payload_kind != before_kind;
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if entry.pending_remove {
                                    // Confirm step: ✓ commits, ✕ cancels. The
                                    // confirm button is tinted red so the
                                    // destructive choice is the visually heavy
                                    // one rather than the bare-X-shape default.
                                    if ui
                                        .small_button("Cancel")
                                        .on_hover_text("Keep this message")
                                        .clicked()
                                    {
                                        entry.pending_remove = false;
                                    }
                                    let confirm = egui::Button::new(
                                        egui::RichText::new("Remove")
                                            .color(egui::Color32::WHITE)
                                            .strong(),
                                    )
                                    .fill(egui::Color32::from_rgb(180, 60, 60));
                                    if ui
                                        .add(confirm)
                                        .on_hover_text("Permanently remove this message")
                                        .clicked()
                                    {
                                        to_remove = Some(i);
                                    }
                                    ui.label(
                                        egui::RichText::new("Remove this message?")
                                            .color(egui::Color32::from_rgb(220, 180, 80)),
                                    );
                                } else if ui
                                    .button(egui::RichText::new("\u{00D7}").size(18.0).strong())
                                    .on_hover_text("Remove this message")
                                    .clicked()
                                {
                                    entry.pending_remove = true;
                                }
                            });
                        });

                        // Per-kind Grid id so each payload variant lives in its
                        // own egui id namespace. Without this, switching kinds
                        // makes the layout's widget set change shape inside the
                        // same Grid — and any auto-derived id whose position
                        // shifts triggers "id changed between passes" warnings
                        // on the next layout pass.
                        let grid_id = match entry.payload_kind {
                            PayloadKind::Hex => "message_grid_hex",
                            PayloadKind::Utf8 => "message_grid_utf8",
                            PayloadKind::Utf16 => "message_grid_utf16",
                            PayloadKind::Ascii => "message_grid_ascii",
                            PayloadKind::Nmea => "message_grid_nmea",
                        };
                        egui::Grid::new(grid_id)
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                content_changed |= show_payload_fields(
                                    ui,
                                    entry,
                                    analysis_cache.analysis.as_ref(),
                                );

                                let bad_interval = invalid_parse::<u64>(&entry.interval_ms);
                                ui.label("Interval (ms)");
                                let interval_resp = red_bordered(
                                    ui,
                                    bad_interval,
                                    "must be a whole number",
                                    |ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut entry.interval_ms)
                                                .id_salt("interval_ms")
                                                .desired_width(80.0),
                                        )
                                    },
                                );
                                content_changed |= interval_resp.changed();
                                ui.end_row();
                                if interval_resp.lost_focus() {
                                    if let Ok(ms) = entry.interval_ms.parse::<u64>() {
                                        interval_changes.push((i, ms));
                                    }
                                }
                            });

                        ui.horizontal(|ui| {
                            content_changed |= show_timestamp_editor(ui, entry);
                            ui.separator();
                            content_changed |= show_checksum_editor(ui, entry);
                        });

                        if content_changed {
                            entry.mark_changed();
                            *dirty = true;
                        }
                        let analysis = analysis_cache.refresh(entry);
                        show_message_preview(ui, analysis);

                        let sent = per_message_counts.get(i).copied().unwrap_or(0);
                        show_message_status(ui, channel_running, sent);
                    });
                });
                ui.add_space(4.0);
            }
            if ui.small_button("+ Add Message").clicked() {
                add_one = true;
            }
        });

    if let Some(i) = to_remove {
        entries.remove(i);
        analyses.remove(i);
        *dirty = true;
    }
    if add_one {
        entries.push(ScheduleDraft::default());
        analyses.push(MessageAnalysisCache::default());
        *dirty = true;
    }

    interval_changes
}

/// Render the payload-format fields for one message into the surrounding grid.
/// Each `PayloadKind` arm has its own renderer below.
fn show_payload_fields(
    ui: &mut egui::Ui,
    entry: &mut ScheduleDraft,
    analysis: Option<&MessageDraftAnalysis>,
) -> bool {
    match entry.payload_kind {
        PayloadKind::Hex => show_hex_payload(ui, entry),
        PayloadKind::Utf8 => show_utf8_payload(ui, entry),
        PayloadKind::Utf16 => show_utf16_payload(ui, entry),
        PayloadKind::Ascii => show_ascii_payload(ui, entry, analysis),
        PayloadKind::Nmea => show_nmea_payload(ui, entry),
    }
}

/// Fill a grid cell's known row height while placing its contents at the top.
/// egui grids otherwise center every cell vertically, which is undesirable for
/// the multiline text rows.
fn top_aligned_grid_cell<R>(
    ui: &mut egui::Ui,
    height: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.scope_builder(
        egui::UiBuilder::new().layout(egui::Layout::left_to_right(egui::Align::Min)),
        |ui| {
            ui.set_min_height(height);
            add_contents(ui)
        },
    )
    .inner
}

fn show_hex_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let bad_hex = !entry.hex_data.is_empty() && !hex_valid(&entry.hex_data);
    ui.label("Data (hex)");
    let response = red_bordered(
        ui,
        bad_hex,
        "invalid hex — use byte pairs like DE AD BE EF",
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut UppercaseHex(&mut entry.hex_data))
                    .id_salt("payload_hex")
                    .desired_width(360.0)
                    .hint_text("DE AD BE EF"),
            )
        },
    );
    ui.end_row();
    response.changed()
}

fn show_utf8_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let row_height = message_editor_max_height(ui, &entry.utf8_text);
    top_aligned_grid_cell(ui, row_height, |ui| ui.label("Text"));
    let changed = top_aligned_grid_cell(ui, row_height, |ui| {
        let edited = marker_aware_text_edit(
            ui,
            &mut entry.utf8_text,
            "payload_utf8",
            None,
            300.0,
            "Unicode text",
        )
        .changed();
        let inserted = show_insert_byte_button(
            ui,
            &mut entry.utf8_text,
            &mut entry.insert_byte_hex,
            "payload_utf8",
        );
        edited || inserted
    });
    ui.end_row();
    changed
}

fn show_utf16_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let row_height = message_editor_max_height(ui, &entry.utf16_text);
    top_aligned_grid_cell(ui, row_height, |ui| ui.label("Text"));
    let text_changed = top_aligned_grid_cell(ui, row_height, |ui| {
        // Two editor modes, chosen by `Allow raw bytes`:
        //   off — plain Unicode editor (what you see is what gets
        //         encoded). Insert Code Unit inserts the decoded
        //         glyph (4 hex → one char).
        //   on  — marker-aware editor + Insert Byte button. Insert
        //         Code Unit inserts marker pairs with byte order
        //         applied.
        let mut changed = if entry.utf16_allow_raw_bytes {
            let edited = marker_aware_text_edit(
                ui,
                &mut entry.utf16_text,
                "payload_utf16",
                None,
                300.0,
                "Unicode text",
            )
            .changed();
            let inserted = show_insert_byte_button(
                ui,
                &mut entry.utf16_text,
                &mut entry.insert_byte_hex,
                "payload_utf16",
            );
            edited || inserted
        } else {
            plain_text_edit_with_cursor(
                ui,
                &mut entry.utf16_text,
                "payload_utf16",
                300.0,
                "Unicode text",
            )
            .changed()
        };
        changed |= show_insert_unit_button(
            ui,
            &mut entry.utf16_text,
            &mut entry.insert_byte_hex,
            "payload_utf16",
            entry.utf16_big_endian,
            entry.utf16_allow_raw_bytes,
        );
        changed
    });
    ui.end_row();
    let before_options = (
        entry.utf16_big_endian,
        entry.utf16_bom,
        entry.utf16_allow_raw_bytes,
    );
    ui.label("Byte order");
    ui.horizontal(|ui| {
        ui.radio_value(&mut entry.utf16_big_endian, true, "Big-endian");
        ui.radio_value(&mut entry.utf16_big_endian, false, "Little-endian");
        ui.separator();
        ui.checkbox(&mut entry.utf16_bom, "BOM");
        ui.separator();
        ui.checkbox(&mut entry.utf16_allow_raw_bytes, "Allow raw bytes")
            .on_hover_text(
                "Treat ‹XX› in the text as raw bytes (fuzzing escape \
                 hatch). When off, ‹ and › are literal Unicode chars.",
            );
    });
    ui.end_row();
    text_changed
        || before_options
            != (
                entry.utf16_big_endian,
                entry.utf16_bom,
                entry.utf16_allow_raw_bytes,
            )
}

fn show_ascii_payload(
    ui: &mut egui::Ui,
    entry: &mut ScheduleDraft,
    analysis: Option<&MessageDraftAnalysis>,
) -> bool {
    let row_height = message_editor_max_height(ui, &entry.ascii_text);
    top_aligned_grid_cell(ui, row_height, |ui| ui.label("Text"));
    let text_changed = top_aligned_grid_cell(ui, row_height, |ui| {
        let edited = marker_aware_text_edit(
            ui,
            &mut entry.ascii_text,
            "payload_ascii",
            Some(entry.ascii_code_page),
            300.0,
            "text",
        )
        .changed();
        let inserted = show_insert_byte_button(
            ui,
            &mut entry.ascii_text,
            &mut entry.insert_byte_hex,
            "payload_ascii",
        );
        edited || inserted
    });
    ui.end_row();
    let code_page_before = entry.ascii_code_page;
    ui.label("Code page");
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("code_page")
            .selected_text(code_page_label(entry.ascii_code_page))
            .show_ui(ui, |ui| {
                for cp in [
                    crate::core::message::CodePage::Iso8859_1,
                    crate::core::message::CodePage::Windows1252,
                    crate::core::message::CodePage::Cp437,
                    crate::core::message::CodePage::MacRoman,
                ] {
                    ui.selectable_value(&mut entry.ascii_code_page, cp, code_page_label(cp));
                }
            });
        if let Some(summary) = analysis.and_then(|analysis| analysis.replacements.as_ref()) {
            let characters = summary
                .characters
                .iter()
                .map(|c| format!("'{c}' (U+{:04X})", *c as u32))
                .collect::<Vec<_>>()
                .join(", ");
            ui.colored_label(
                theme_palette(ui).warning_amber,
                format!("{} replaced with ?; use UTF-8", summary.count),
            )
            .on_hover_text(format!(
                "{} cannot represent: {characters}. Each occurrence will be sent as '?' \
                 (0x3F). Switch the message Format to UTF-8 (recommended) or UTF-16, \
                 or insert exact bytes when substitution is not appropriate.",
                code_page_label(entry.ascii_code_page)
            ));
        }
    });
    ui.end_row();
    text_changed || entry.ascii_code_page != code_page_before
}

fn show_nmea_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let mut changed = false;
    ui.label("Talker / Sentence");
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut entry.nmea_talker)
                .id_salt("payload_nmea_talker")
                .desired_width(40.0)
                .hint_text("GP"),
        );
        if r.changed() {
            entry.nmea_talker = entry.nmea_talker.to_ascii_uppercase();
            changed = true;
        }
        ui.menu_button("v", |ui| {
            changed |= show_filtered_picker(
                ui,
                "filter by code or description",
                &mut entry.nmea_talker_filter,
                nmea0183::talker_id::ALL_WITH_DESC,
                &mut entry.nmea_talker,
            );
        });
        ui.separator();
        let r = ui.add(
            egui::TextEdit::singleline(&mut entry.nmea_sentence_type)
                .id_salt("payload_nmea_sentence")
                .desired_width(50.0)
                .hint_text("GGA"),
        );
        if r.changed() {
            entry.nmea_sentence_type = entry.nmea_sentence_type.to_ascii_uppercase();
            prefill_nmea_fields(entry);
            changed = true;
        }
        ui.menu_button("v", |ui| {
            if show_filtered_picker(
                ui,
                "filter by code or description",
                &mut entry.nmea_sentence_filter,
                nmea0183::sentence_type::ALL_WITH_DESC,
                &mut entry.nmea_sentence_type,
            ) {
                prefill_nmea_fields(entry);
                changed = true;
            }
        });
        ui.separator();
        ui.label("NMEA checksum:").on_hover_text(
            "The protocol-internal `*XX` byte at the end of an NMEA \
             sentence. Distinct from the `Message checksum` row below, \
             which is an outer checksum wrapped around the complete \
             rendered message (timestamp + payload + NMEA `*XX`).",
        );
        let checksum_before = entry.nmea_checksum_mode;
        ui.radio_value(
            &mut entry.nmea_checksum_mode,
            NmeaChecksumMode::Correct,
            "include",
        );
        ui.radio_value(
            &mut entry.nmea_checksum_mode,
            NmeaChecksumMode::Omit,
            "omit",
        );
        ui.radio_value(
            &mut entry.nmea_checksum_mode,
            NmeaChecksumMode::Wrong,
            "wrong",
        );
        changed |= entry.nmea_checksum_mode != checksum_before;
    });
    ui.end_row();

    let sentence_type: nmea0183::SentenceType = entry
        .nmea_sentence_type
        .parse()
        .expect("SentenceType parse is infallible");
    let time_fields = sentence_type.time_fields();
    let supports_live_time = !time_fields.is_empty();
    ui.label("Time fields");
    ui.horizontal(|ui| {
        let live_before = entry.nmea_live_time;
        let live_response = ui.add_enabled(
            supports_live_time || entry.nmea_live_time,
            egui::Checkbox::new(&mut entry.nmea_live_time, "Live time (UTC)"),
        );
        if supports_live_time {
            let fields = time_fields
                .iter()
                .map(|(index, kind)| format!("field {}: {}", index + 1, kind.label()))
                .collect::<Vec<_>>()
                .join(", ");
            live_response.on_hover_text(format!(
                "Refreshed at every send: {fields}. Typed values are retained in the profile but \
                 overridden on the wire. Missing fields are added through the last live field."
            ));
        } else {
            live_response
                .on_hover_text("No live UTC field positions are defined for this sentence type.");
        }
        changed |= entry.nmea_live_time != live_before;

        let millis_before = entry.nmea_live_millis;
        ui.add_enabled(
            supports_live_time && entry.nmea_live_time,
            egui::Checkbox::new(&mut entry.nmea_live_millis, "Milliseconds"),
        )
        .on_hover_text("Use hhmmss.sss instead of hhmmss for live UTC time fields.");
        changed |= entry.nmea_live_millis != millis_before;

        if entry.nmea_live_time && !supports_live_time {
            ui.colored_label(theme_palette(ui).fault_red, "Unsupported sentence type")
                .on_hover_text("Turn Live time off or choose a sentence with defined UTC fields.");
        }
    });
    ui.end_row();

    ui.label("Fields");
    let fields_r = ui.add(
        egui::TextEdit::singleline(&mut entry.nmea_fields)
            .id_salt("payload_nmea_fields")
            .desired_width(360.0)
            .hint_text("comma-separated, e.g. 123519,4807.038,N,01131.000,E"),
    );
    if fields_r.changed() {
        // User edited by hand — protect Fields from being overwritten
        // by future auto-fills on sentence-type changes.
        entry.nmea_fields_autofilled = false;
        changed = true;
    }
    ui.end_row();
    changed
}

/// Example comma-separated field values for common NMEA sentence types.
/// Returned with no trailing `*XX` (the checksum is added downstream).
/// Used to auto-fill the Fields box when the user picks a sentence type
/// and the Fields box is currently empty — so brand-new messages start
/// from a realistic sample rather than a blank.
fn nmea_example_fields(sentence: &str) -> Option<&'static str> {
    match sentence {
        "GGA" => Some("123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,"),
        "RMC" => Some("220516,A,5133.82,N,00042.24,W,173.8,231.8,130694,004.2,W"),
        "VTG" => Some("054.7,T,034.4,M,005.5,N,010.2,K"),
        "GLL" => Some("4916.45,N,12311.12,W,225444,A"),
        "GSA" => Some("A,3,19,28,14,18,27,22,31,39,,,,,1.7,1.0,1.3"),
        "GSV" => Some("2,1,08,01,40,083,46,02,17,308,41,12,07,344,39,14,22,228,45"),
        "GNS" => Some("122310.2,3722.425671,N,12258.856215,W,DA,15,0.9,1005.543,6.5,5.2,23"),
        "HDT" => Some("123.4,T"),
        "HDM" => Some("123.4,M"),
        "HDG" => Some("123.4,1.2,E,2.0,W"),
        "THS" => Some("123.4,A"),
        "ROT" => Some("35.6,A"),
        "ZDA" => Some("201530.00,04,07,2002,00,00"),
        "VHW" => Some("123.4,T,123.4,M,1.0,N,1.852,K"),
        "VBW" => Some("11.0,01.0,A,12.0,02.0,A"),
        "VLW" => Some("12345.6,N,123.4,N"),
        "DBT" => Some("5.0,f,1.5,M,0.8,F"),
        "DBK" => Some("5.0,f,1.5,M,0.8,F"),
        "DBS" => Some("5.0,f,1.5,M,0.8,F"),
        "DPT" => Some("3.4,0.5"),
        "MTW" => Some("17.9,C"),
        "MWV" => Some("019.0,R,15.5,N,A"),
        "MWD" => Some("019.0,T,021.0,M,015.5,N,007.97,M"),
        "MDA" => Some("30.12,I,1.02,B,17.9,C,,,53,,,,019.0,T,021.0,M,15.5,N,007.97,M"),
        "XDR" => Some("C,17.9,C,TEMP1"),
        "RSA" => Some("0.5,A,,V"),
        "RPM" => Some("S,1,1000.0,5.0,A"),
        "APB" => Some("A,A,0.10,R,N,V,V,011.0,T,DEST,011.0,T,011.0,T"),
        "BOD" => Some("097.0,T,103.2,M,POINTB,POINTA"),
        "XTE" => Some("A,A,0.10,R,N"),
        "GBS" => Some("125027,1.2,1.3,3.2,12,0.04,-0.3,7.5"),
        "GST" => Some("172814.0,0.006,0.023,0.020,273.6,0.023,0.020,0.031"),
        // Proprietary — pair with talker P. PASHR (Ashtech attitude):
        // hhmmss.ss,heading,T,roll,pitch,heave,roll_acc,pitch_acc,heading_acc,quality
        "ASHR" => Some("123519.00,123.45,T,1.23,-0.50,0.10,0.020,0.020,0.025,1"),
        // PRDID (Teledyne RDI): pitch,roll,heading — has no checksum.
        "RDID" => Some("-1.23,2.34,123.45"),
        _ => None,
    }
}

/// Pre-fill `entry.nmea_fields` with a sample for the current sentence
/// type when it's safe to do so:
///
/// - The Fields box is empty, OR
/// - The Fields box was previously auto-filled and the user hasn't edited
///   it since (`nmea_fields_autofilled == true`).
///
/// Anything the user has typed by hand is left alone.
fn prefill_nmea_fields(entry: &mut ScheduleDraft) {
    let safe_to_overwrite = entry.nmea_fields.is_empty() || entry.nmea_fields_autofilled;
    if !safe_to_overwrite {
        return;
    }
    if let Some(example) = nmea_example_fields(&entry.nmea_sentence_type) {
        entry.nmea_fields = example.to_string();
        entry.nmea_fields_autofilled = true;
    } else if entry.nmea_fields_autofilled {
        // No example for this new sentence type. Clear any stale auto-fill
        // from the previous sentence type — keeping it would confuse the
        // user. (Leave user-typed content alone, which is why we only do
        // this when the autofilled flag is set.)
        entry.nmea_fields.clear();
        entry.nmea_fields_autofilled = false;
    }
}

/// Filterable, scrollable popup body used for the NMEA Talker and Sentence
/// pickers. Renders a small TextEdit at the top, then a scrollable list of
/// `(code, description)` rows. The filter is case-insensitive and matches
/// against BOTH the code and the description, so typing "depth" narrows the
/// sentence list to DBK/DBS/DBT/DPT etc. Clicking a row commits the code
/// into `selected` and closes the popup.
fn show_filtered_picker(
    ui: &mut egui::Ui,
    hint: &str,
    filter: &mut String,
    options: &[(&'static str, &'static str)],
    selected: &mut String,
) -> bool {
    // Pin the popup so the Talker and Sentence pickers look the same and
    // so the (often long) descriptions don't keep widening it.
    ui.set_min_width(360.0);
    let r = ui.add(
        egui::TextEdit::singleline(filter)
            .desired_width(340.0)
            .hint_text(hint),
    );
    r.request_focus();
    let needle = filter.to_ascii_lowercase();
    let mut changed = false;
    egui::ScrollArea::vertical()
        .min_scrolled_height(300.0)
        .max_height(300.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Empty-selection row, always at the top — lets the user
            // clear a previously-picked value without retyping or
            // closing the popup. Skipped when the filter is active so
            // it doesn't visually compete with real matches.
            if needle.is_empty()
                && ui
                    .button(egui::RichText::new("(empty — clear selection)").italics())
                    .clicked()
            {
                selected.clear();
                filter.clear();
                changed = true;
                ui.close();
            }
            for (code, desc) in options {
                let matches = needle.is_empty()
                    || code.to_ascii_lowercase().contains(&needle)
                    || desc.to_ascii_lowercase().contains(&needle);
                if matches && ui.button(format!("{code}  —  {desc}")).clicked() {
                    *selected = (*code).to_string();
                    filter.clear();
                    changed = true;
                    ui.close();
                }
            }
        });
    changed
}

/// Render the read-only "this is what would be sent" preview row.
///
/// The revision-keyed [`MessageDraftAnalysis`] owns conversion, compilation,
/// and fixed-time rendering. This widget only applies theme-dependent styling,
/// so unchanged long messages do no wire-format work during repaints.
///
/// Bytes are shown as text (lossy UTF-8) for payload types that are text
/// at heart (Utf8 / Ascii / NMEA) and as space-separated hex for the
/// binary types (Hex / Utf16), to avoid the U+FFFD-tofu we'd otherwise
/// get for non-UTF-8 bytes.
fn show_message_preview(ui: &mut egui::Ui, analysis: &MessageDraftAnalysis) {
    ui.horizontal(|ui| {
        ui.label("Wire bytes:").on_hover_text(
            "Literal bytes that would be sent on the wire, rendered \
                 in a payload-appropriate view. Timestamps use a fixed \
                 reference instant so the value doesn't tick — the \
                 actual send uses the wall clock.",
        );
        let preview: egui::WidgetText = match &analysis.preview {
            MessagePreview::Ascii {
                bytes,
                code_page,
                replacement_wire_offsets,
            } => preview_ascii_layout_job(ui, bytes, *code_page, replacement_wire_offsets).into(),
            MessagePreview::Text(text) | MessagePreview::Hex(text) => {
                egui::RichText::new(text).monospace().into()
            }
            MessagePreview::Invalid(error) => egui::RichText::new(format!("Invalid: {error}"))
                .color(theme_palette(ui).fault_red)
                .monospace()
                .into(),
            MessagePreview::Incomplete => egui::RichText::new("(message is incomplete)")
                .monospace()
                .into(),
        };
        ui.label(preview);
    });
}

/// Per-message status line at the bottom of each message group:
/// a coloured state dot plus the message's running local-acceptance count.
///
/// State follows the channel — messages aren't independently scheduled
/// from the user's perspective. "Active" = channel is running and this
/// message will fire on its interval. "Idle" = channel is stopped, so
/// the count is the last value seen.
fn show_message_status(ui: &mut egui::Ui, channel_running: bool, sent: u64) {
    // Footer bar: separator above to split it from the message body, then
    // a tinted Frame so the "Active / Accepted: N" line reads as a status
    // strip rather than just another row of widgets. Inner margin
    // matches the channel-summary chrome so all the framed bits in the
    // GUI feel like the same component.
    ui.add_space(2.0);
    ui.separator();
    let dark = ui.visuals().dark_mode;
    let (dot_color, state) = if channel_running {
        (egui::Color32::from_rgb(80, 200, 80), "Active")
    } else {
        (
            egui::Color32::from_gray(if dark { 140 } else { 120 }),
            "Idle",
        )
    };
    // Tinted strip behind the status line, keyed to the theme so the
    // label text (which follows the theme's body colour) stays
    // legible on it: a deep green / dim grey on dark, a pale green /
    // light grey on light.
    let bg = match (channel_running, dark) {
        (true, true) => egui::Color32::from_rgb(28, 52, 28),
        (true, false) => egui::Color32::from_rgb(205, 232, 205),
        (false, true) => egui::Color32::from_gray(40),
        (false, false) => egui::Color32::from_gray(222),
    };
    egui::Frame::default()
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(dot_color, egui::RichText::new("\u{2022}").size(16.0));
                ui.label(egui::RichText::new(state).strong());
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("Accepted: {sent}"))
                        .strong()
                        .monospace(),
                )
                .on_hover_text(LOCAL_ACCEPTANCE_TOOLTIP);
            });
        });
}

/// Render the per-message timestamp toggles.
///
/// No inner separator between the `Timestamp` checkbox and its
/// sub-toggles — visual grouping comes from the parent horizontal. The
/// only `ui.separator()` at this nesting level is the one *between* the
/// timestamp group and the message-checksum group, so the hierarchy reads
/// "groups are separated; within a group is just spacing".
fn show_timestamp_editor(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    let before = (
        entry.timestamp_enabled,
        entry.ts_date,
        entry.ts_millis,
        entry.ts_timezone,
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut entry.timestamp_enabled, "Timestamp");
        if entry.timestamp_enabled {
            ui.checkbox(&mut entry.ts_date, "Date");
            ui.checkbox(&mut entry.ts_millis, "Milliseconds");
            ui.checkbox(&mut entry.ts_timezone, "Z (UTC)");
        }
    });
    before
        != (
            entry.timestamp_enabled,
            entry.ts_date,
            entry.ts_millis,
            entry.ts_timezone,
        )
}

/// Render the per-message checksum controls. See [`show_timestamp_editor`]
/// for the separator hierarchy rationale.
fn show_checksum_editor(ui: &mut egui::Ui, entry: &mut ScheduleDraft) -> bool {
    use crate::core::message::ChecksumAlgorithm;
    let before = (
        entry.checksum_enabled,
        entry.checksum_algorithm,
        entry.checksum_wrong,
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut entry.checksum_enabled, "Message checksum")
            .on_hover_text(
                "Outer checksum appended to the complete rendered message \
                 (timestamp + payload). Independent of any protocol-internal \
                 checksum like NMEA's `*XX` — that one is still emitted.",
            );
        if entry.checksum_enabled {
            egui::ComboBox::from_id_salt("checksum_algorithm")
                .selected_text(checksum_label(entry.checksum_algorithm))
                .show_ui(ui, |ui| {
                    for algo in [
                        ChecksumAlgorithm::Xor,
                        ChecksumAlgorithm::Crc8,
                        ChecksumAlgorithm::Crc16Ccitt,
                        ChecksumAlgorithm::Crc16Modbus,
                        ChecksumAlgorithm::Crc32,
                    ] {
                        ui.selectable_value(
                            &mut entry.checksum_algorithm,
                            algo,
                            checksum_label(algo),
                        );
                    }
                });
            ui.checkbox(&mut entry.checksum_wrong, "Intentionally wrong");
        }
    });
    before
        != (
            entry.checksum_enabled,
            entry.checksum_algorithm,
            entry.checksum_wrong,
        )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        analyzed_channel_demand, cadence_decision, delivery_decision, diagnostic_card_tone,
        recent_timing_metric, select_service_timing, timer_status_detail, timing_detail_text,
        top_aligned_grid_cell, unavailable_line_capacity_label, ServiceTimingSource,
    };
    use crate::core::telemetry::{RecentSnapshotState, SendTimingTelemetry};
    use crate::core::timing::{CadenceAlignment, TimerMode, TimerReason, TimerStatus, TimingMode};
    use crate::gui::{
        draft::{ConnKind, PayloadKind, ScheduleDraft},
        MessageAnalysisCache,
    };
    use wiredata_ui::diagnostics::SignalTone;

    #[test]
    fn delivery_decision_distinguishes_clean_shortfall_and_failure() {
        let clean = delivery_decision(100, 0, 0, 0);
        assert_eq!(clean.text, "100 / 100 accepted · none unsent");
        assert_eq!(clean.tone, SignalTone::Healthy);

        let missed = delivery_decision(98, 0, 1, 1);
        assert_eq!(missed.text, "98 / 100 accepted · 2 unsent (2.0%)");
        assert_eq!(missed.tone, SignalTone::Warning);

        let failed = delivery_decision(98, 1, 0, 1);
        assert_eq!(failed.text, "98 / 100 accepted · 2 unsent (2.0%)");
        assert_eq!(failed.tone, SignalTone::Fault);
    }

    #[test]
    fn delivery_decision_is_neutral_before_any_schedule_fires() {
        let decision = delivery_decision(0, 0, 0, 0);
        assert_eq!(decision.text, "No scheduled sends yet");
        assert_eq!(decision.tone, SignalTone::Neutral);
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

    #[test]
    fn multiline_grid_cells_share_the_same_top_edge() {
        let mut measured_tops = None;
        egui::__run_test_ui(|ui| {
            egui::Grid::new("top_aligned_grid_test")
                .num_columns(2)
                .show(ui, |ui| {
                    let label = top_aligned_grid_cell(ui, 80.0, |ui| ui.label("Text"));
                    let editor = top_aligned_grid_cell(ui, 80.0, |ui| {
                        ui.allocate_response(egui::vec2(300.0, 60.0), egui::Sense::hover())
                    });
                    ui.end_row();
                    measured_tops = Some((label.rect.top(), editor.rect.top()));
                });
        });

        let (label_top, editor_top) = measured_tops.expect("grid contents should be measured");
        assert!(
            (label_top - editor_top).abs() <= 0.5,
            "label top {label_top} did not align with editor top {editor_top}"
        );
    }
}
