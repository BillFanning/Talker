//! The channel detail / message-view panel (`show_detail`), split out of `mod.rs`.
//! The decision logic it dispatches (Apply & Start/Restart sequencing, lifecycle
//! actions) lives — and is unit-tested — in [`super::widgets`]. The densest sub-panel,
//! the stream viewer, lives in [`stream_view`].

mod stream_view;

use crate::core::{ArrivalTimestampStatus, ChannelId, RecordingState};
use crate::diagnostics::DiagnosticSeverity;
use crate::runtime::{
    CounterAvailability, DurationHistogram, ListenerRunSummary, QueueDepth, TransportHealth,
};

use super::bridge::{self, UiCommand};
use super::state::{ChannelStatus, ChannelView};
use super::widgets::{
    config_needs_restart, edit_display_recording, edit_interface, edit_raw_recording, human_bytes,
    line_indicator, line_toggle, paint_glyph, recording_glyph_size, recording_indicator,
    start_button, status_color, status_glyph, status_label, stop_enabled,
};
use super::ListenerApp;
use wiredata_ui::diagnostics::{
    attention_callout, decision_card, signal_grid, signal_row, SignalTone,
};
use wiredata_ui::fonts::bold;
use wiredata_ui::format::compact_duration;
use wiredata_ui::palette::active as palette;

/// Uniform size for the lifecycle / recording control buttons. Text wider than the
/// min grows the button (so "Apply & Restart" doesn't clip).
const CONTROL_BUTTON_SIZE: egui::Vec2 = egui::vec2(96.0, 32.0);

/// Keep the existing display warm-up boundary. It affects whether a bounded
/// percentile is useful to show; it is not a latency-health threshold.
const TIMING_WARMUP_SAMPLES: u64 = 20;

// Diagnostic help follows the same technician-facing sequence throughout:
// locate the measurement in the receive path, explain how to read it, then
// state the operational implication and the boundary of what it can prove.
const TRANSPORT_TOOLTIP: &str = concat!(
    "Shows transport-specific evidence available from this channel run. ",
    "For Linux UDP, a reported drop is a datagram discarded because this socket's ",
    "kernel receive queue filled; the displayed cumulative count is a lower bound ",
    "because status updates are best-effort. For Serial, a stall means the receive ",
    "loop stopped issuing reads while a block waited for Ingest space. Serial ",
    "snapshots read the reader's authoritative state: completed totals cover finished ",
    "stalls, while Active is the elapsed time of the current unfinished stall. A ",
    "reported zero does not rule out loss in the network, device, adapter, driver, ",
    "or upstream buffer."
);
const PRESSURE_TOOLTIP: &str = concat!(
    "Ingest is received data moving through Listener's in-memory processing queue; ",
    "its summary shows the highest sampled in-flight count this run, not a live ",
    "current depth. Raw is the background recorder's in-memory queue; while recording, ",
    "its ",
    "latest status sample and highest observed occupancy are shown, and afterward ",
    "the highest observed occupancy is retained for the channel run. The writer may ",
    "be handling one block concurrently. These are application queues, not device or ",
    "driver buffers. Half capacity is an early-warning reference: active Raw pressure ",
    "at or above it raises Attention, while a historical peak remains visible but does ",
    "not escalate the card by itself. A brief burst can set a peak, and occupancy is ",
    "not proof of data loss or sustained pressure."
);
const PIPELINE_TOOLTIP: &str = concat!(
    "Handoff is the time from a completed operating-system read until Listener ",
    "starts processing that block. Processing is the time Listener spends handling ",
    "it synchronously; background recorder I/O may run concurrently and is outside this ",
    "boundary. Recent values use an approximate latest or final ten-second segmented ",
    "window. Before 20 samples the display shows the maximum; afterward, p99 ≤ X ",
    "means at least 99% of samples fell within X. Compare changes with queue pressure ",
    "and normal operation; there is no universal good/bad latency limit."
);

const UDP_DROP_CALLOUT_TOOLTIP: &str = concat!(
    "Linux confirms at least this many datagrams were discarded because this ",
    "socket's kernel receive queue filled. The displayed cumulative count is the ",
    "latest update that reached Listener's pipeline; the actual run total may be ",
    "higher. It excludes packets lost before reaching this socket. Check host load, ",
    "input rate, and receive-buffer capacity."
);
const SERIAL_BACKPRESSURE_TOOLTIP: &str = concat!(
    "Shows authoritative Serial backpressure state for this channel run. A stall ",
    "starts after a block is read when the Ingest queue is full; the reader retries ",
    "for space about every 5 ms and issues no new read meanwhile. Episodes is the ",
    "number of completed stalls; Total and Max also cover completed stalls only. ",
    "Active is measured separately from the reader's current stall start. Advisory ",
    "warnings may be dropped without changing these values. While the channel ",
    "continues, the pending block is retried, but it is not retained if the channel ",
    "stops or its processing path closes. Upstream adapter or driver loss is not ",
    "observable here."
);

const RECEIVE_DETAILS_TOOLTIP: &str = concat!(
    "Use these measurements to locate pressure between the input, Listener ",
    "processing, and storage. Recent or final values use an approximate ten-second ",
    "segmented window; run values cover the time since Start. p99 ≤ X is an ",
    "upper-bound estimate for at least 99% of samples."
);
const HANDOFF_TOOLTIP: &str = concat!(
    "Measures each received block from immediately after the operating-system read ",
    "returns until Listener begins processing it. This includes copying, waiting in ",
    "the Ingest queue, and thread scheduling. It excludes time spent in the device, ",
    "adapter, driver, or kernel before the read completed. Rising Handoff together ",
    "with Ingest pressure means arrivals were not drained promptly; compare Processing ",
    "to distinguish Listener workload from scheduling delay or input bursts."
);
const PROCESSING_TOOLTIP: &str = concat!(
    "Measures how long Listener handles each received block: accounting, scrollback, ",
    "match rules, display preparation, and recorder handoff and checks. It excludes ",
    "Handoff and background recorder I/O, which may run concurrently. Rising Processing ",
    "together with Ingest pressure points to a Listener workload bottleneck."
);
const CHUNK_SHAPE_TOOLTIP: &str = concat!(
    "Cumulative across the full channel run, not the recent ten-second window. One ",
    "chunk is the block returned by a single operating-system read; it is not ",
    "necessarily a complete line or protocol message. Size shows bytes per read. ",
    "Read gap is between consecutive post-read monotonic captures and can reflect ",
    "source cadence, buffering, and host scheduling—not per-byte wire timing. Because ",
    "these values accumulate, a long run can dilute a brief change."
);
const ARRIVAL_TIMESTAMP_TOOLTIP: &str = concat!(
    "Wall-clock source for timestamped Mark annotations and recording metadata. ",
    "Post-read is host time captured when the operating-system read returns; Linux ",
    "kernel software time is captured earlier in the socket receive path. When kernel ",
    "timing is active, a post-read fallback is a datagram whose ancillary metadata ",
    "did not contain a usable timestamp. SO_TIMESTAMPNS returns a software timestamp ",
    "with nanosecond fields; that representation does not imply nanosecond accuracy. ",
    "Handoff and Read gap always use post-read monotonic captures. Neither source is ",
    "device time, hardware time, or per-byte arrival time."
);
const UDP_NOT_APPLICABLE_TOOLTIP: &str = concat!(
    "This channel does not use an input with a per-socket UDP receive-overflow ",
    "counter. This is expected for this transport and says nothing about data loss."
);
const UDP_UNAVAILABLE_TOOLTIP: &str = concat!(
    "Listener does not have an active drop counter attributable to this UDP socket. ",
    "The operating system may not support it, or enabling it may have failed. Treat ",
    "unavailable as unknown, not zero. Use packet capture or operating-system and ",
    "network counters if an independent loss measurement is required."
);
const UDP_AVAILABLE_TOOLTIP: &str = concat!(
    "Latest cumulative drop total reported for this Linux UDP socket. ",
    "A nonzero value confirms at least that many datagrams were discarded because ",
    "the kernel receive queue filled; a later update can be missed, so the actual run ",
    "total may be higher. Zero means no nonzero update reached this display, not that ",
    "no packets were lost here or elsewhere."
);
const IDLE_RULE_TIMING_TOOLTIP: &str = concat!(
    "Measures how late Listener evaluates and fires each Idle rule after its ",
    "configured no-data deadline. For example, p99 ≤ 2 ms means at least 99% of ",
    "measured firings were no more than about 2 ms late. Lateness can include ",
    "operating-system wake delay, processor contention, and time Listener spent ",
    "servicing requests, processing input, or handling recording before evaluation. ",
    "It is not wall-clock or receive-time accuracy."
);
const IDLE_TIMER_TOOLTIP: &str = concat!(
    "Counts completed Idle-deadline wakes, not every wait that was started. On ",
    "Windows, Listener requests 1 ms timer resolution only during the final 32 ms; ",
    "1 ms unavailable counts completed wakes for which that request was ineffective, ",
    "not necessarily distinct Windows API calls. Linux and macOS use native deadline ",
    "waits. One completed wake can fire several rules. A wait restarted by ",
    "new input or other channel work is not counted. Compare unavailable wakes ",
    "with measured lateness; unavailable alone is not proof that a rule fired late."
);
const INGEST_QUEUE_TOOLTIP: &str = concat!(
    "Last sample is the number of blocks in flight when Listener most recently took ",
    "one for processing, including that block; it is not a live queue reading. Peak ",
    "is the highest such sample this run and capacity is the queue limit. A brief ",
    "burst can set the peak, so compare a high value with Handoff timing and ",
    "transport stalls."
);
const RAW_QUEUE_ACTIVE_TOOLTIP: &str = concat!(
    "Received blocks queued for the background Raw recorder. Latest sample is the queue ",
    "occupancy in the most recent status snapshot, highest observed is the largest ",
    "sample this run, and capacity is the queue limit. The writer may be handling one ",
    "block concurrently. A brief burst can set the observed high. If an enqueue is ",
    "attempted while the queue is full, Raw recording faults while reception continues."
);
const RAW_QUEUE_RETAINED_TOOLTIP: &str = concat!(
    "Raw recording is no longer active. This preserves the highest observed queue ",
    "occupancy from the current channel run so earlier writer pressure remains ",
    "visible. It does not describe current storage load."
);
const RAW_QUEUE_FAULTED_TOOLTIP: &str = concat!(
    "Raw recording is faulted. The highest observed queue occupancy remains visible ",
    "for this channel run. An enqueue attempted while the queue is full is terminal ",
    "for Raw recording, as are some recorder I/O failures; reception continues. Check ",
    "the recording fault and diagnostics for the specific cause."
);
const RAW_QUEUE_INACTIVE_TOOLTIP: &str =
    "Raw recording is not active, so there is no Raw recorder queue to measure.";
const THROUGHPUT_TOOLTIP: &str = concat!(
    "Received is the cumulative byte count processed by Listener since Start and is ",
    "retained after Stop. Throughput uses bytes whose post-read arrival times fall in ",
    "the current and previous four one-second buckets, divided by five seconds and ",
    "shown in decimal kB/s. It is an approximate five-second application receive rate ",
    "from the latest status snapshot, not instantaneous line rate, link utilization, ",
    "or device-buffer occupancy. During the first five seconds it still uses the full ",
    "five-second denominator; after Stop the rate is zero while Received remains."
);
const FINAL_SNAPSHOT_TOOLTIP: &str = concat!(
    "When complete, completed-run values come from the channel's final snapshot. If ",
    "the final snapshot is incomplete, timing, queue, and transport values may be ",
    "missing or defaulted and must not be read as measured zero."
);

#[derive(Debug, PartialEq, Eq)]
struct DecisionSignal {
    value: String,
    tone: SignalTone,
}

fn queue_level_reaches_half(level: usize, capacity: usize) -> bool {
    // Keep the established >= 50% reference without multiplying `level`
    // (and therefore without a theoretical usize overflow).
    capacity > 0 && level >= capacity.div_ceil(2)
}

fn reported_udp_drops(dropped: u64) -> String {
    if dropped == 0 {
        "reported 0".to_owned()
    } else {
        format!("reported ≥{dropped}")
    }
}

fn transport_signal(health: TransportHealth) -> DecisionSignal {
    if let Some(stalls) = health.serial_stalls {
        let active = stalls
            .active_for
            .map(|elapsed| format!(" · active {}", compact_duration(elapsed)))
            .unwrap_or_default();
        return DecisionSignal {
            value: format!(
                "Serial: {} completed episodes · total {} · max {}{active}",
                stalls.episodes,
                compact_duration(stalls.total),
                compact_duration(stalls.max),
            ),
            tone: if stalls.episodes > 0 || stalls.active_for.is_some() {
                SignalTone::Warning
            } else {
                SignalTone::Neutral
            },
        };
    }

    match health.udp_kernel_drops {
        CounterAvailability::Available(dropped) => DecisionSignal {
            value: format!("Kernel drops {}", reported_udp_drops(dropped)),
            tone: if dropped > 0 {
                SignalTone::Fault
            } else {
                SignalTone::Neutral
            },
        },
        CounterAvailability::Unsupported => DecisionSignal {
            value: "Kernel drops unavailable".to_owned(),
            tone: SignalTone::Neutral,
        },
        CounterAvailability::NotApplicable => DecisionSignal {
            value: "Kernel drops not applicable".to_owned(),
            tone: SignalTone::Neutral,
        },
    }
}

fn pressure_signal(
    ingest: QueueDepth,
    raw: Option<QueueDepth>,
    raw_recording: Option<RecordingState>,
) -> DecisionSignal {
    let ingest_text = if ingest.capacity == 0 {
        "Ingest awaiting data".to_owned()
    } else {
        format!("Ingest highest sampled {}/{}", ingest.peak, ingest.capacity)
    };
    let raw_text = match (raw, raw_recording) {
        (Some(queue), Some(RecordingState::Faulted)) => format!(
            "Raw faulted · highest observed {}/{}",
            queue.peak, queue.capacity
        ),
        (None, Some(RecordingState::Faulted)) => "Raw faulted".to_owned(),
        (Some(queue), Some(RecordingState::Enabled)) => format!(
            "Raw latest sample {} · highest observed {}/{}",
            queue.current, queue.peak, queue.capacity
        ),
        (Some(queue), _) => format!(
            "Raw retained highest observed {}/{}",
            queue.peak, queue.capacity
        ),
        (None, _) => "Raw not recording".to_owned(),
    };
    let tone = match (raw, raw_recording) {
        (_, Some(RecordingState::Faulted)) => SignalTone::Fault,
        (Some(queue), Some(RecordingState::Enabled))
            if queue_level_reaches_half(queue.current, queue.capacity) =>
        {
            SignalTone::Warning
        }
        _ => SignalTone::Neutral,
    };
    DecisionSignal {
        value: format!("{ingest_text} · {raw_text}"),
        tone,
    }
}

fn recent_timing_signal(
    label: &str,
    cumulative: DurationHistogram,
    recent: DurationHistogram,
) -> String {
    if cumulative.sample_count() == 0 {
        return format!("{label} awaiting first chunk");
    }
    if recent.sample_count() == 0 {
        let run_max = cumulative
            .max()
            .map(compact_duration)
            .unwrap_or_else(|| "n/a".to_owned());
        return format!("{label} no recent chunks · run max {run_max}");
    }
    if recent.sample_count() < TIMING_WARMUP_SAMPLES {
        return format!(
            "{label} warm-up ({}) · max {}",
            recent.sample_count(),
            compact_duration(recent.max().unwrap_or_default())
        );
    }
    format!(
        "{label} p99 ≤ {}",
        compact_duration(recent.percentile_upper_bound(99).unwrap_or_default())
    )
}

fn pipeline_signal(view: &ChannelView, status: ChannelStatus) -> DecisionSignal {
    DecisionSignal {
        value: format!(
            "{} · {} · {}",
            recent_timing_signal("Handoff", view.ingest_delay, view.recent_ingest_delay),
            recent_timing_signal(
                "Processing",
                view.ingest_processing,
                view.recent_ingest_processing,
            ),
            timing_window(status),
        ),
        // No application latency budget exists, so these measurements are facts,
        // not a fabricated healthy/warning judgment.
        tone: SignalTone::Neutral,
    }
}

fn timing_window(status: ChannelStatus) -> &'static str {
    if matches!(status, ChannelStatus::Running | ChannelStatus::Reconnecting) {
        "~last 10 s"
    } else {
        "~final 10 s"
    }
}

fn card_tone(transport: SignalTone, pressure: SignalTone) -> SignalTone {
    if transport == SignalTone::Fault || pressure == SignalTone::Fault {
        SignalTone::Fault
    } else if transport == SignalTone::Warning || pressure == SignalTone::Warning {
        SignalTone::Warning
    } else {
        SignalTone::Neutral
    }
}

fn diagnostics_badge(
    tone: SignalTone,
    status: ChannelStatus,
    has_completed_run: bool,
) -> &'static str {
    match tone {
        SignalTone::Fault => "ISSUE",
        SignalTone::Warning => "ATTENTION",
        SignalTone::Neutral | SignalTone::Healthy
            if matches!(status, ChannelStatus::Running | ChannelStatus::Reconnecting) =>
        {
            "MONITORING"
        }
        SignalTone::Neutral | SignalTone::Healthy if has_completed_run => "LAST RUN",
        SignalTone::Neutral | SignalTone::Healthy => "AWAITING DATA",
    }
}

fn show_receive_diagnostics_card(
    ui: &mut egui::Ui,
    id: ChannelId,
    status: ChannelStatus,
    view: &ChannelView,
) {
    let transport = transport_signal(view.transport_health);
    let pressure = pressure_signal(view.ingest_queue, view.raw_recording_queue, view.recording);
    let pipeline = pipeline_signal(view, status);
    let tone = card_tone(transport.tone, pressure.tone);
    let has_completed_run = view
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.last_run_summary.is_some());
    let badge = diagnostics_badge(tone, status, has_completed_run);

    decision_card(ui, "Receive diagnostics", badge, tone, |ui| {
        signal_grid(ui, ("listener_receive_signals", id), |ui| {
            signal_row(
                ui,
                "Transport",
                transport.value,
                transport.tone,
                TRANSPORT_TOOLTIP,
            );
            signal_row(
                ui,
                "Pressure",
                pressure.value,
                pressure.tone,
                PRESSURE_TOOLTIP,
            );
            signal_row(
                ui,
                "Pipeline",
                pipeline.value,
                pipeline.tone,
                PIPELINE_TOOLTIP,
            );
        });

        if let CounterAvailability::Available(dropped) = view.transport_health.udp_kernel_drops {
            if dropped > 0 {
                ui.add_space(6.0);
                let _ = attention_callout(
                    ui,
                    ("listener_udp_kernel_drops", id),
                    format!("Kernel receive queue reported at least {dropped} dropped datagrams."),
                    SignalTone::Fault,
                    UDP_DROP_CALLOUT_TOOLTIP,
                );
            }
        }
        if let Some(stalls) = view.transport_health.serial_stalls {
            if stalls.episodes > 0 || stalls.active_for.is_some() {
                ui.add_space(6.0);
                let active = stalls
                    .active_for
                    .map(|elapsed| format!(", active for {}", compact_duration(elapsed)))
                    .unwrap_or_default();
                let _ = attention_callout(
                    ui,
                    ("listener_serial_stalls", id),
                    format!(
                        "Serial backpressure: {} completed episodes, {} completed total, {} completed maximum{active}.",
                        stalls.episodes,
                        compact_duration(stalls.total),
                        compact_duration(stalls.max),
                    ),
                    SignalTone::Warning,
                    SERIAL_BACKPRESSURE_TOOLTIP,
                );
            }
        }

        ui.add_space(5.0);
        let details = egui::CollapsingHeader::new("Receive & transport details")
            .id_salt(("listener_receive_transport_details", id))
            .default_open(false)
            .show(ui, |ui| show_receive_transport_details(ui, status, view));
        details
            .header_response
            .on_hover_text(RECEIVE_DETAILS_TOOLTIP);
    });
}

fn show_receive_transport_details(ui: &mut egui::Ui, status: ChannelStatus, view: &ChannelView) {
    let cumulative_timing = view.ingest_delay;
    let timing = view.recent_ingest_delay;
    let samples = timing.sample_count();
    let run_samples = cumulative_timing.sample_count();
    let run_max = cumulative_timing
        .max()
        .map(compact_duration)
        .unwrap_or_else(|| "n/a".to_owned());
    let window = timing_window(status);
    let timing_text = if run_samples == 0 {
        "Handoff timing: awaiting first chunk".to_owned()
    } else if samples == 0 {
        format!("Handoff timing ({window}): no chunks · run max {run_max}")
    } else if samples < TIMING_WARMUP_SAMPLES {
        format!(
            "Handoff timing ({window}): warming up ({samples} chunks) · max {} · run max {run_max}",
            compact_duration(timing.max().unwrap_or_default()),
        )
    } else {
        format!(
            "Handoff timing ({window}): post-read to pipeline p99 ≤ {} · run max {run_max}",
            compact_duration(timing.percentile_upper_bound(99).unwrap_or_default()),
        )
    };
    ui.label(egui::RichText::new(timing_text).weak())
        .on_hover_text(HANDOFF_TOOLTIP);

    let cumulative_processing = view.ingest_processing;
    let processing = view.recent_ingest_processing;
    let processing_samples = processing.sample_count();
    let processing_run_samples = cumulative_processing.sample_count();
    let processing_run_max = cumulative_processing
        .max()
        .map(compact_duration)
        .unwrap_or_else(|| "n/a".to_owned());
    let processing_text = if processing_run_samples == 0 {
        "Processing timing: awaiting first chunk".to_owned()
    } else if processing_samples == 0 {
        format!("Processing timing ({window}): no chunks · run max {processing_run_max}")
    } else if processing_samples < TIMING_WARMUP_SAMPLES {
        format!(
            "Processing timing ({window}): warming up ({processing_samples} chunks) · max {} · run max {processing_run_max}",
            compact_duration(processing.max().unwrap_or_default()),
        )
    } else {
        format!(
            "Processing timing ({window}): p99 ≤ {} · run max {processing_run_max}",
            compact_duration(processing.percentile_upper_bound(99).unwrap_or_default()),
        )
    };
    ui.label(egui::RichText::new(processing_text).weak())
        .on_hover_text(PROCESSING_TOOLTIP);

    let chunks = view.chunk_shape;
    let chunk_count = chunks.chunk_count();
    let chunk_text = if chunk_count == 0 {
        "Chunk shape: awaiting first chunk".to_owned()
    } else {
        let size_text = if chunk_count < TIMING_WARMUP_SAMPLES {
            format!(
                "size mean {} · max {}",
                human_bytes(chunks.sizes.mean().unwrap_or_default()),
                human_bytes(chunks.sizes.max().unwrap_or_default()),
            )
        } else {
            format!(
                "size p50 ≤ {} · p99 ≤ {} · max {}",
                human_bytes(chunks.sizes.percentile_upper_bound(50).unwrap_or_default()),
                human_bytes(chunks.sizes.percentile_upper_bound(99).unwrap_or_default()),
                human_bytes(chunks.sizes.max().unwrap_or_default()),
            )
        };
        let gap_samples = chunks.inter_read_gaps.sample_count();
        let gap_text = if gap_samples == 0 {
            "read gap awaiting second chunk".to_owned()
        } else if gap_samples < TIMING_WARMUP_SAMPLES {
            format!(
                "read gap max {}",
                compact_duration(chunks.inter_read_gaps.max().unwrap_or_default())
            )
        } else {
            format!(
                "read gap p99 ≤ {}",
                compact_duration(
                    chunks
                        .inter_read_gaps
                        .percentile_upper_bound(99)
                        .unwrap_or_default()
                )
            )
        };
        format!("Chunks (run): {chunk_count} · {size_text} · {gap_text}")
    };
    ui.label(egui::RichText::new(chunk_text).weak())
        .on_hover_text(CHUNK_SHAPE_TOOLTIP);

    if let Some(stalls) = view.transport_health.serial_stalls {
        let active = stalls
            .active_for
            .map(|elapsed| format!(" · active {}", compact_duration(elapsed)))
            .unwrap_or_default();
        ui.label(
            egui::RichText::new(format!(
                "Serial backpressure (run): {} completed episodes · completed total {} · completed max {}{active}",
                stalls.episodes,
                compact_duration(stalls.total),
                compact_duration(stalls.max),
            ))
            .weak(),
        )
        .on_hover_text(SERIAL_BACKPRESSURE_TOOLTIP);
    }

    let arrival = view.transport_health.arrival_timestamps;
    let arrival_text = match arrival.status {
        ArrivalTimestampStatus::PostRead => format!(
            "Arrival timestamps: post-read · {} chunks",
            arrival.post_read_samples
        ),
        ArrivalTimestampStatus::KernelSoftware => {
            let fallback = (arrival.post_read_samples > 0)
                .then(|| format!(" · {} post-read fallbacks", arrival.post_read_samples));
            format!(
                "Arrival timestamps: Linux kernel software · {} chunks{}",
                arrival.kernel_samples,
                fallback.as_deref().unwrap_or_default()
            )
        }
        ArrivalTimestampStatus::KernelRequestedUnavailable => format!(
            "Arrival timestamps: kernel unavailable · post-read fallback · {} chunks",
            arrival.post_read_samples
        ),
    };
    ui.label(egui::RichText::new(arrival_text).weak())
        .on_hover_text(ARRIVAL_TIMESTAMP_TOOLTIP);

    match view.transport_health.udp_kernel_drops {
        CounterAvailability::NotApplicable => {
            ui.label(egui::RichText::new("UDP kernel drops: not applicable").weak())
                .on_hover_text(UDP_NOT_APPLICABLE_TOOLTIP);
        }
        CounterAvailability::Unsupported => {
            ui.label(egui::RichText::new("UDP kernel drops: counter unavailable").weak())
                .on_hover_text(UDP_UNAVAILABLE_TOOLTIP);
        }
        CounterAvailability::Available(dropped) => {
            let reported = reported_udp_drops(dropped);
            let text = egui::RichText::new(format!("UDP kernel receive-queue drops: {reported}"));
            ui.label(if dropped > 0 {
                text.color(palette(ui).warning_amber)
            } else {
                text.weak()
            })
            .on_hover_text(UDP_AVAILABLE_TOOLTIP);
        }
    }

    let has_idle_rule = view.config.match_rules.iter().any(|rule| {
        rule.enabled && matches!(rule.condition, crate::config::MatchCondition::Idle { .. })
    });
    let cumulative_rule_timing = view.rule_timer_lateness;
    let recent_rule_timing = view.recent_rule_timer_lateness;
    if has_idle_rule || cumulative_rule_timing.sample_count() > 0 {
        let recent_samples = recent_rule_timing.sample_count();
        let run_samples = cumulative_rule_timing.sample_count();
        let run_max = cumulative_rule_timing
            .max()
            .map(compact_duration)
            .unwrap_or_else(|| "n/a".to_owned());
        let text = if run_samples == 0 {
            "Idle rule timing: awaiting first firing".to_owned()
        } else if recent_samples == 0 {
            format!("Idle rule timing ({window}): no firings · run max {run_max}")
        } else if recent_samples < TIMING_WARMUP_SAMPLES {
            format!(
                "Idle rule timing ({window}): {recent_samples} firings · max {} · run max {run_max}",
                compact_duration(recent_rule_timing.max().unwrap_or_default())
            )
        } else {
            format!(
                "Idle rule timing ({window}): lateness p99 ≤ {} · run max {run_max}",
                compact_duration(
                    recent_rule_timing
                        .percentile_upper_bound(99)
                        .unwrap_or_default()
                )
            )
        };
        ui.label(egui::RichText::new(text).weak())
            .on_hover_text(IDLE_RULE_TIMING_TOOLTIP);
        let timer = view.idle_deadline_timer;
        let timer_text = if cfg!(windows) {
            format!(
                "Idle timer: completed wakes {} · 1 ms effective {} · 1 ms unavailable {}",
                timer
                    .windows_one_millisecond_waits
                    .saturating_add(timer.windows_request_failures),
                timer.windows_one_millisecond_waits,
                timer.windows_request_failures,
            )
        } else {
            format!(
                "Idle timer: completed native deadline wakes {}",
                timer.native_waits
            )
        };
        ui.label(egui::RichText::new(timer_text).weak())
            .on_hover_text(IDLE_TIMER_TOOLTIP);
    }

    let ingest = view.ingest_queue;
    let ingest_text = if ingest.capacity == 0 {
        "Ingest pressure: awaiting first block".to_owned()
    } else {
        format!(
            "Ingest pressure (run): peak {}/{} · last sample {}",
            ingest.peak, ingest.capacity, ingest.current
        )
    };
    ui.label(egui::RichText::new(ingest_text).weak())
        .on_hover_text(INGEST_QUEUE_TOOLTIP);

    match view.raw_recording_queue {
        Some(queue) if view.recording == Some(RecordingState::Faulted) => {
            ui.label(
                egui::RichText::new(format!(
                    "Raw record queue (faulted): highest observed {}/{}",
                    queue.peak, queue.capacity
                ))
                .color(palette(ui).fault_red),
            )
            .on_hover_text(RAW_QUEUE_FAULTED_TOOLTIP);
        }
        Some(queue) if view.recording == Some(RecordingState::Enabled) => {
            let text = egui::RichText::new(format!(
                "Raw record queue: latest sample {}/{} · highest observed {}",
                queue.current, queue.capacity, queue.peak
            ))
            .weak();
            ui.label(if queue_level_reaches_half(queue.current, queue.capacity) {
                text.color(palette(ui).warning_amber)
            } else {
                text
            })
            .on_hover_text(RAW_QUEUE_ACTIVE_TOOLTIP);
        }
        Some(queue) => {
            ui.label(
                egui::RichText::new(format!(
                    "Raw record queue (retained): highest observed {}/{}",
                    queue.peak, queue.capacity
                ))
                .weak(),
            )
            .on_hover_text(RAW_QUEUE_RETAINED_TOOLTIP);
        }
        None => {
            ui.label(egui::RichText::new("Raw record queue: (not recording)").weak())
                .on_hover_text(RAW_QUEUE_INACTIVE_TOOLTIP);
        }
    }
}

/// Which recording tap a shared control targets (ADR-013): the byte-exact Raw
/// `.raw` or the rendered Display `.disp`. The two blocks are deliberately
/// symmetric — one enum keeps the header button and the live-persist fold a
/// single implementation each.
#[derive(Clone, Copy)]
enum RecTap {
    Raw,
    Display,
}

fn show_last_run_summary(ui: &mut egui::Ui, summary: &ListenerRunSummary) {
    let heading = format!(
        "Last completed run · {} · {} · {} chunks",
        compact_duration(summary.elapsed),
        human_bytes(summary.total_bytes),
        summary.chunk_shape.chunk_count(),
    );
    let summary_view = egui::CollapsingHeader::new(heading)
        .id_salt("listener_last_completed_run")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Copy summary").clicked() {
                    ui.ctx().copy_text(summary.to_report_text());
                }
                ui.weak(format!(
                    "Listener {} · {} · {}/{}",
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
                summary.finished_utc()
            ));
            ui.weak(format!(
                "Ingest queue peak {}/{} · {} warnings · {} errors{}",
                summary.ingest_queue.peak,
                summary.ingest_queue.capacity,
                summary.diagnostics_warnings,
                summary.diagnostics_errors,
                if summary.final_snapshot_complete {
                    ""
                } else {
                    " · final snapshot incomplete"
                }
            ));
        });
    summary_view
        .header_response
        .on_hover_text(FINAL_SNAPSHOT_TOOLTIP);
}

impl ListenerApp {
    pub(super) fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected else {
            ui.label("No channel selected. Use “+ Add” by the channel list.");
            return;
        };
        self.sync_edit_draft(id);
        let Some((details, status, bytes_total, bps, last_error, recording)) =
            self.state.channel(id).map(|v| {
                (
                    v.details.clone(),
                    v.status,
                    v.bytes_total,
                    v.bytes_per_sec,
                    v.last_error.clone(),
                    v.recording,
                )
            })
        else {
            ui.label("That channel is no longer present.");
            return;
        };

        // Does the edit draft differ from the committed config in a way that needs a
        // restart? Drives the Start button's "Apply & Restart" label. Live-applied
        // fields (name, raw recording, view settings, scroll buffer) are excluded — see
        // `config_needs_restart` — so editing them doesn't flip the lifecycle button.
        let config_changed = match (&self.edit_draft, self.state.channel(id)) {
            (Some((eid, draft)), Some(view)) if *eid == id => {
                config_needs_restart(draft, &view.config)
            }
            _ => false,
        };
        // Channel block (name, status, stats, lifecycle) on the LEFT, recording block on
        // the RIGHT, as two columns — but the whole columns area is capped to a FIXED
        // width (`set_max_width`), so each column has a constant width and the recording
        // block's left edge stays put when the window's right edge is resized (with a
        // free 50/50 `columns` the right column widened with the pane, dragging the block
        // sideways). The cap is scoped to just the columns; the View config + stream view
        // render after on the full-width `ui` — `columns` is the only side-by-side layout
        // that has reliably kept the stream view (horizontal_top variants collapsed it).
        const CONTROLS_WIDTH: f32 = 670.0;
        ui.scope(|ui| {
            ui.set_max_width(CONTROLS_WIDTH);
            ui.columns(2, |cols| {
                self.show_channel_controls(
                    &mut cols[0],
                    id,
                    status,
                    config_changed,
                    &details,
                    bytes_total,
                    bps,
                );
                self.show_recording_block(&mut cols[1], id, status, recording);
            });
        });
        if let Some(err) = &last_error {
            ui.colored_label(palette(ui).fault_red, format!("⚠ {err}"));
            // The port/bind recourse only applies to a *start* fault (channel Faulted) —
            // not a recording fault, which leaves the channel Running and whose error
            // already names its own recourse (check the destination / on-exists).
            if status == ChannelStatus::Faulted {
                ui.label(
                    "Recourse: change the port below and Apply, free the resource (Stop \
                     the other channel on that port) then Retry, or Remove this channel.",
                );
            }
        }
        if let Some(view) = self.state.channel(id) {
            ui.add_space(6.0);
            show_receive_diagnostics_card(ui, id, status, view);
        }
        if let Some(summary) = self
            .state
            .channel(id)
            .and_then(|view| view.snapshot.as_ref())
            .and_then(|snapshot| snapshot.last_run_summary.as_ref())
        {
            ui.add_space(4.0);
            show_last_run_summary(ui, summary);
        }

        // Configure: edit the full interface config on a working copy, then commit
        // with one click — "Apply & Restart" installs it and brings the channel up
        // (no separate Apply-then-Start step).
        let ports = self.serial_ports.clone();
        // Force the section open for one frame when focus moved to a needy channel
        // (task 1); `None` afterwards so the user can still collapse it.
        let force_open = self.force_config_open.then_some(true);
        let mut refresh = false;
        // Configure is edit-only: there's no Apply button here. Edits commit via the
        // Start / Apply & Restart button at the top, which applies the pending draft.
        if let Some((_, config)) = &mut self.edit_draft {
            // "Configure connection" — the shared section title in both apps
            // (talker's Connection editor uses the same words).
            egui::CollapsingHeader::new(bold("Configure connection"))
                // A STABLE id (not per-channel) so switching channels doesn't create a
                // "new" header each time — that re-triggered a focus/animation highlight
                // that flashed a rectangle around the label on every channel switch. The
                // open/closed state is now shared across channels (consistent), with a
                // one-frame force-open when a needy channel needs attention.
                .id_salt("configure")
                .open(force_open)
                .default_open(true)
                .show(ui, |ui| {
                    refresh = ui
                        .push_id("edit_iface", |ui| edit_interface(ui, id, config, &ports))
                        .inner;
                    // Display (.disp) recording lives in the recording block on the
                    // right, under Record Raw Data (ADR-013); the inline Mark
                    // timestamp editor lives under Configure display (it shapes what
                    // the view and .disp show).
                });
        }
        self.force_config_open = false;
        if refresh {
            self.refresh_serial_ports();
        }

        // Live serial control/status lines (§161): green = high, grey = low.
        if let Some(lines) = self.state.channel(id).and_then(|v| v.control_lines) {
            ui.horizontal(|ui| {
                // Outputs are clickable toggles (§161): clicking sends Set{Rts,Dtr};
                // the shown state still comes from the live poll, so it reflects what
                // the port actually did, not just what we asked for.
                ui.label(bold("Out:"));
                if line_toggle(ui, "RTS", lines.rts).clicked() {
                    self.send(UiCommand::SetRts(id, !lines.rts));
                }
                if line_toggle(ui, "DTR", lines.dtr).clicked() {
                    self.send(UiCommand::SetDtr(id, !lines.dtr));
                }
                ui.separator();
                // Inputs are read-only indicators.
                ui.label(bold("In:"));
                line_indicator(ui, "CTS", lines.cts);
                line_indicator(ui, "DSR", lines.dsr);
                line_indicator(ui, "DCD", lines.dcd);
                line_indicator(ui, "RI", lines.ri);
            });
        }

        ui.separator();
        self.show_diagnostics(ui, id);
        ui.separator();
        self.show_stream_view(ui, id);
    }

    /// Diagnostics for the selected channel (snapshot-driven): a color-coded
    /// headline that opens a filterable, ms-timestamped log. Split out of
    /// `show_detail`. (Match-rule activity surfaces through its effects — inline
    /// Mark timestamps, recording state, the diagnostics entries Notify and
    /// boundary-split recoveries write — not a firing list of its own.)
    fn show_diagnostics(&mut self, ui: &mut egui::Ui, id: ChannelId) {
        // Diagnostics — only meaningful once there's a snapshot. Pull the
        // data into owned locals so the filter checkboxes can mutate `self` without a
        // live `self.state` borrow.
        struct DiagView {
            headline_level: &'static str,
            headline: String,
            headline_color: egui::Color32,
            counts: (usize, usize, usize),
            /// The full diagnostics log, chronological (oldest → newest) — the
            /// per-snapshot cache from the view-model (an O(1) `Rc` clone per
            /// frame; the flatten+sort happens once per poll, not per repaint).
            entries: std::rc::Rc<Vec<crate::diagnostics::Diagnostic>>,
        }
        let view = self.state.channel(id);
        // The diagnostics log comes *only* from the snapshot — the GUI never synthesizes
        // entries. The runtime is the single writer: live diagnostics arrive via the 5 Hz
        // poll; stop-time notes via the final snapshot taken at stop; and a start/bind
        // fault (which never ran a pipeline) is retained by the runtime and served via a
        // minimal snapshot for the faulted channel. The snapshot is kept across stop/start
        // so a previous run's messages persist.
        let (entries, counts) = match view {
            Some(v) => {
                let counts = v
                    .snapshot
                    .as_ref()
                    .map(|s| {
                        let d = &s.diagnostics;
                        (d.events.len(), d.warnings.len(), d.errors.len())
                    })
                    .unwrap_or((0, 0, 0));
                (v.sorted_diagnostics.clone(), counts)
            }
            None => (std::rc::Rc::new(Vec::new()), (0, 0, 0)),
        };
        // Always shown — an empty log renders a neutral "no diagnostics yet"
        // headline so the section doesn't pop into existence on the first entry.
        // The headline is the latest entry, so the newest diagnostic leads — a fresh
        // INFO supersedes an older ERROR, and a just-recorded fault (newest) leads.
        let (headline_level, headline, headline_color) = match entries.last() {
            Some(d) => {
                let (level, color) = match d.severity {
                    DiagnosticSeverity::Event => ("INFO", palette(ui).event_grey),
                    DiagnosticSeverity::Warning => ("WARN", palette(ui).warning_amber),
                    DiagnosticSeverity::Error => ("ERROR", palette(ui).fault_red),
                };
                (level, d.message.clone(), color)
            }
            None => ("", "no diagnostics yet".to_string(), palette(ui).idle_grey),
        };
        let dv = DiagView {
            headline_level,
            headline,
            headline_color,
            counts,
            entries,
        };
        {
            // The diagnostics header is a single line: "Diagnostics (counts)  LEVEL phrase"
            // — the live headline (chronologically latest diagnostic) sits to the right of
            // the title and score, shortened to a clean phrase (headline_phrase) and
            // truncated by egui if it still overflows the row. Single-line and a fixed
            // height, so the collapsing header's layout stays stable across egui's two
            // passes (a wrapping/variable-height header caused a repaint spin). The full
            // untruncated text is in the expanded log below.
            let headline = if dv.headline_level.is_empty() {
                headline_phrase(&dv.headline)
            } else {
                format!("{}  {}", dv.headline_level, headline_phrase(&dv.headline))
            };
            let headline_color = dv.headline_color;
            let diag_id = ui.make_persistent_id(("diagnostics", id));
            egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                diag_id,
                false,
            )
            .show_header(ui, |ui| {
                let (e, w, x) = dv.counts;
                ui.label(bold("Diagnostics"));
                ui.label(egui::RichText::new(format!("({e} info · {w} warn · {x} err)")).weak());
                ui.add(
                    egui::Label::new(egui::RichText::new(headline).color(headline_color))
                        .truncate(),
                );
            })
            .body(|ui| {
                ui.horizontal(|ui| {
                    let (e, w, x) = dv.counts;
                    ui.label(bold("Show"));
                    ui.checkbox(&mut self.show_info, format!("Info ({e})"));
                    ui.checkbox(&mut self.show_warn, format!("Warn ({w})"));
                    ui.checkbox(&mut self.show_error, format!("Error ({x})"));
                });
                egui::ScrollArea::vertical()
                    .id_salt("diag_log")
                    .max_height(200.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Inside the scroll area (fixed height, auto_shrink off), the
                        // available width is the stable wrap target — don't derive it from
                        // `clip_rect`/`cursor`, which vary between egui's two layout passes
                        // and were destabilizing the layout.
                        let log_w = ui.available_width();
                        ui.set_max_width(log_w);
                        let mut shown = 0usize;
                        for d in dv.entries.iter().rev() {
                            let (enabled, color, level) = match d.severity {
                                DiagnosticSeverity::Event => {
                                    (self.show_info, palette(ui).info_grey, "INFO ")
                                }
                                DiagnosticSeverity::Warning => {
                                    (self.show_warn, palette(ui).warning_amber, "WARN ")
                                }
                                DiagnosticSeverity::Error => {
                                    (self.show_error, palette(ui).fault_red, "ERROR")
                                }
                            };
                            if !enabled {
                                continue;
                            }
                            let dt: chrono::DateTime<chrono::Local> = d.timestamp.into();
                            let msg = &d.message;
                            shown += 1;
                            // Wrap long entries so the full message stays readable
                            // (a plain colored_label was clipped at the pane edge).
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "{}  {level}  {msg}",
                                        dt.format("%H:%M:%S%.3f")
                                    ))
                                    .color(color),
                                )
                                .wrap(),
                            );
                        }
                        if shown == 0 {
                            ui.label(egui::RichText::new("no diagnostics match the filter").weak());
                        }
                    });
            });
        }
    }

    /// The left channel/control column: status, live rename, byte stats, and
    /// the [Start / Apply & Restart / Retry] [Stop] lifecycle row. Both
    /// buttons are always present; Stop is disabled unless stoppable; the
    /// Start side's label/enabled state comes from the pure `start_button`
    /// decision.
    #[allow(clippy::too_many_arguments)]
    fn show_channel_controls(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        config_changed: bool,
        details: &str,
        bytes_total: u64,
        bps: f64,
    ) {
        // Name row: status glyph + editable name (renames live, §6).
        ui.horizontal(|ui| {
            // Painted into a fixed cell so the glyph never drives the row height.
            let (glyph, scale) = status_glyph(status);
            paint_glyph(ui, glyph, scale, status_color(status, palette(ui)));
            const NAME_HINT: &str = "This channel's display name. When file rotation is \
                on, it's also the base name of the rotated files (<channel>_<time \
                period>), so keep it filesystem-safe.";
            ui.label(bold("Name")).on_hover_text(NAME_HINT);
            let mut renamed = None;
            if let Some((_, config)) = &mut self.edit_draft {
                let mut name = config.name.as_str().to_string();
                if ui
                    .add(egui::TextEdit::singleline(&mut name).desired_width(180.0))
                    .on_hover_text(NAME_HINT)
                    .changed()
                {
                    config.name = crate::core::ChannelName::new(name.clone());
                    renamed = Some(name);
                }
            }
            if let Some(name) = renamed {
                // Names must be unique (§6, ADR-014). Commit only a name not already used
                // by another channel (case-insensitively); a duplicate is kept in the
                // draft (so the user can keep editing toward a unique name) but not sent
                // to the runtime, and an inline warning shows why. This channel itself is
                // excluded (`v.id != id`), so re-typing its own current name is fine.
                let is_duplicate = self
                    .state
                    .channels()
                    .any(|v| v.id != id && v.name.eq_ignore_ascii_case(&name));
                self.name_duplicate = is_duplicate;
                if !is_duplicate {
                    // Tell the runtime AND fold locally — the echoed ChannelRenamed is
                    // advisory/lossy (§99), so the optimistic local apply keeps the list
                    // row and re-seeded draft authoritative (#5).
                    self.send(UiCommand::Rename(
                        id,
                        crate::core::ChannelName::new(name.clone()),
                    ));
                    self.state.apply(bridge::UiUpdate::ChannelRenamed(id, name));
                }
            }
            if self.name_duplicate {
                ui.label(
                    egui::RichText::new("⚠ name already in use — names must be unique")
                        .color(palette(ui).warning_amber),
                );
            }
        });
        ui.horizontal(|ui| {
            ui.label(status_label(status));
            ui.label("·");
            ui.label(egui::RichText::new(details).weak());
        });
        // Byte-based liveness (§18): total received + rolling throughput.
        ui.label(format!(
            "Received: {}    Throughput: {:.1} kB/s",
            human_bytes(bytes_total),
            bps / 1000.0
        ))
        .on_hover_text(THROUGHPUT_TOOLTIP);
        ui.add_space(12.0); // a blank line between the readouts and the buttons
        let size = CONTROL_BUTTON_SIZE;
        ui.horizontal(|ui| {
            let (start_label, start_enabled) = start_button(status, config_changed);
            if ui
                .add_enabled(start_enabled, egui::Button::new(start_label).min_size(size))
                .clicked()
            {
                // Start / Apply & Restart / Retry are all the same action: commit the
                // edited config and bring the channel up. `try_start` sends one
                // CommitAndStart; the runtime handles the Running-restart and the
                // Faulted→Stopped→Starting recovery (§8.5) — no per-state client steps.
                self.try_start(id);
            }
            if ui
                .add_enabled(
                    stop_enabled(status),
                    egui::Button::new("Stop Channel").min_size(size),
                )
                .clicked()
            {
                self.send(UiCommand::Stop(id));
            }
        });
    }

    /// Recording block (right column): the **Raw** section (header row with live
    /// state glyph + Start/Stop button, over a collapsible setup) and, right under
    /// it, the **Display** section in the same shape (ADR-013 — two independent
    /// recordings, same options). The Raw live toggle reads the settings from the
    /// editor at click time (ADR-012); both taps' setup edits apply live
    /// (`persist_recording`), never via Apply & Restart.
    ///
    /// Kept in small pieces (the header rows, `record_button`,
    /// `recording_setup_section`) because this block is still evolving — add new
    /// recording controls as their own helpers rather than growing this method.
    fn show_recording_block(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
    ) {
        // Raw header row: title + live state indicator + the Start/Stop recording
        // button on the same line (no expander — the setup follows below).
        ui.horizontal(|ui| {
            ui.label(bold("Record Raw Data"));
            // Status glyph only (same symbol set/colors as channel status) — the word
            // ("recording"/"off"/"faulted") is dropped to keep the row compact; the glyph
            // ■/●/⚠ carries the state.
            let (glyph, color, _text) = recording_indicator(recording, palette(ui));
            paint_glyph(ui, glyph, recording_glyph_size(glyph), color);
            self.record_button(ui, id, status, recording, RecTap::Raw);
        });
        if self.edit_draft.as_ref().map(|(eid, _)| *eid) != Some(id) {
            ui.label(egui::RichText::new("(select the channel to edit)").weak());
            return;
        }
        if let Some((_, config)) = &mut self.edit_draft {
            let summary = {
                let rec = &config.raw_recording;
                record_summary(&rec.destination, rec.file_rotation, rec.overwrite_policy)
            };
            recording_setup_section(ui, ("raw_rec_setup", id), &summary, |ui| {
                edit_raw_recording(ui, config)
            });
        }
        self.persist_recording(id, RecTap::Raw);

        // Display recording (§54): the sibling tap, same options, same layout, same
        // live toggle (ADR-012/-013) — begin/stop mid-run from click-time settings.
        ui.separator();
        let display_recording = self.state.channel(id).and_then(|v| v.display_recording);
        ui.horizontal(|ui| {
            ui.label(bold("Record Display"));
            let (glyph, color, _text) = recording_indicator(display_recording, palette(ui));
            paint_glyph(ui, glyph, recording_glyph_size(glyph), color);
            self.record_button(ui, id, status, display_recording, RecTap::Display);
        });
        if let Some((_, config)) = &mut self.edit_draft {
            let summary = {
                let rec = &config.display_recording;
                record_summary(&rec.destination, rec.file_rotation, rec.overwrite_policy)
            };
            recording_setup_section(ui, ("disp_rec_setup", id), &summary, |ui| {
                edit_display_recording(ui, config)
            });
        }
        self.persist_recording(id, RecTap::Display);
    }

    /// The recording controls on a tap's header row: a "Record on start" toggle
    /// (begins recording when the channel next starts, §53/§54) and, for a running
    /// channel, the live Record/Stop button (ADR-012). The button reads the
    /// on-screen settings *at click time* (from the edit draft) and sends them with
    /// the command, so recording goes exactly where the controls say — no restart,
    /// no Apply. One implementation for both taps (ADR-013 symmetry).
    fn record_button(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
        tap: RecTap,
    ) {
        // "Record on start" — the auto-start flag, editable whether or not the
        // channel is running (it governs the next start).
        if let Some((_, config)) = &mut self.edit_draft {
            let (flag, hover) = match tap {
                RecTap::Raw => (
                    &mut config.raw_recording.enabled,
                    "Begin recording automatically when the channel starts (§53).",
                ),
                RecTap::Display => (
                    &mut config.display_recording.enabled,
                    "Record the rendered view output (.disp) — what the display shows,                      not the raw bytes (§54) — automatically when the channel starts.",
                ),
            };
            ui.checkbox(flag, "Record on start").on_hover_text(hover);
        }
        if status != ChannelStatus::Running {
            return;
        }
        let draft = self
            .edit_draft
            .as_ref()
            .filter(|(eid, _)| *eid == id)
            .map(|(_, cfg)| cfg);
        let has_dest = draft.is_some_and(|cfg| match tap {
            RecTap::Raw => cfg.raw_recording.destination.is_some(),
            RecTap::Display => cfg.display_recording.destination.is_some(),
        });
        let recording_now = matches!(recording, Some(RecordingState::Enabled));
        let label = if recording_now { "Stop" } else { "Record" };
        // Match the start-channel button's *width* (96) but keep the default height —
        // a full CONTROL_BUTTON_SIZE min_size plus a long label made it both too wide
        // and too tall. Short labels fit the 96px width.
        let resp = ui.add_enabled(
            has_dest || recording_now,
            egui::Button::new(label).min_size(egui::vec2(CONTROL_BUTTON_SIZE.x, 0.0)),
        );
        let resp = if !has_dest && !recording_now {
            resp.on_hover_text("Set a destination below first")
        } else {
            resp
        };
        if resp.clicked() {
            let begin = !recording_now;
            let cmd = match tap {
                RecTap::Raw => UiCommand::SetRecording(
                    id,
                    begin,
                    Box::new(draft.map(|c| c.raw_recording.clone()).unwrap_or_default()),
                ),
                RecTap::Display => UiCommand::SetDisplayRecording(
                    id,
                    begin,
                    Box::new(
                        draft
                            .map(|c| c.display_recording.clone())
                            .unwrap_or_default(),
                    ),
                ),
            };
            self.send(cmd);
        }
    }

    /// Persist a tap's recording edits into the stored config and runtime. Both
    /// recordings are live fields (ADR-012), so their edits never travel through
    /// the Apply & Restart path — without this, a profile save wouldn't capture
    /// them. When the draft differs from the channel's stored config, fold it in
    /// and sync the runtime.
    fn persist_recording(&mut self, id: ChannelId, tap: RecTap) {
        let Some((eid, cfg)) = self.edit_draft.as_ref() else {
            return;
        };
        if *eid != id {
            return;
        }
        match tap {
            RecTap::Raw => {
                let draft = cfg.raw_recording.clone();
                if let Some(view) = self.state.channel_mut(id) {
                    if view.config.raw_recording != draft {
                        view.config.raw_recording = draft.clone();
                        self.send(UiCommand::SetRawRecordingConfig(id, Box::new(draft)));
                    }
                }
            }
            RecTap::Display => {
                let draft = cfg.display_recording.clone();
                if let Some(view) = self.state.channel_mut(id) {
                    if view.config.display_recording != draft {
                        view.config.display_recording = draft.clone();
                        self.send(UiCommand::SetDisplayRecordingConfig(id, Box::new(draft)));
                    }
                }
            }
        }
    }
}

/// Shorten a full diagnostic message into a headline phrase by cutting at the first
/// natural boundary, so the title-row headline reads as a clean phrase rather than a
/// mid-word truncation. Drops the *detail* tail:
/// - `→` separates a subject from its target — keep the subject ("Raw recording
///   started → C:\…" → "Raw recording started").
/// - ` — ` / `: ` introduce an explanation — keep up to and including the first
///   `<name>:` segment but drop a following explanatory clause ("UDP_Channel3: failed
///   to bind interface: Only one usage… (os error 10048)" → "UDP_Channel3: failed to
///   bind interface").
///
/// Falls back to the whole (trimmed) message when there is no such boundary; the egui
/// label still ellipsizes if even the phrase overflows the row.
fn headline_phrase(message: &str) -> String {
    // First, drop a `→ target` tail (recording destinations etc.).
    let head = message.split('→').next().unwrap_or(message).trim();
    // Then drop an explanatory clause after the *second* `: ` (the first `: ` is the
    // "<name>: <kind>" separator we want to keep) or after a ` — ` dash.
    let mut cut = head.len();
    if let Some(dash) = head.find(" — ") {
        cut = cut.min(dash);
    }
    // Keep the first "<name>: <kind>" but trim a second ": <detail>".
    if let Some(first_colon) = head.find(": ") {
        if let Some(rel) = head[first_colon + 2..].find(": ") {
            cut = cut.min(first_colon + 2 + rel);
        }
    }
    head[..cut].trim_end().to_string()
}

/// A collapsible "Setup" section for one recording (collapsed by default): the
/// closed header carries a one-line `path · rotation · on-exists` summary so the
/// configured destination stays visible without the controls; open shows the full
/// editor. Shared by the Raw and Display blocks (ADR-013 — same options).
fn recording_setup_section(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash,
    summary: &str,
    body: impl FnOnce(&mut egui::Ui),
) {
    let setup_id = ui.make_persistent_id(salt);
    let state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), setup_id, false);
    let open = state.is_open();
    state
        .show_header(ui, |ui| {
            ui.label(bold("Setup"));
            // The summary rides the (closed) header so it reads as one line; when
            // open, the full editor is in the body below, so keep the header terse.
            if !open {
                ui.label(egui::RichText::new(summary).weak());
            }
        })
        .body(body);
}

/// A one-line summary of a recording setup for the collapsed Setup header:
/// `path · rotation · on-exists` (e.g. `C:\logs\gps.raw · Daily · Append`). The path
/// reads "(no destination)" when unset; rotation/on-exists use short words. Shared
/// by the Raw and Display blocks — their configs carry the same fields (ADR-013).
fn record_summary(
    destination: &Option<std::path::PathBuf>,
    file_rotation: crate::record::FileRotationPolicy,
    overwrite_policy: crate::record::OverwritePolicy,
) -> String {
    use crate::record::{FileRotationPolicy, OverwritePolicy};
    let path = destination
        .as_ref()
        .map(|p| shorten_path(p))
        .unwrap_or_else(|| "(no destination)".to_string());
    let rotation = match file_rotation {
        FileRotationPolicy::None => "no rotation",
        FileRotationPolicy::Hourly => "Hourly",
        FileRotationPolicy::Daily => "Daily",
    };
    let on_exists = match overwrite_policy {
        OverwritePolicy::Refuse => "Refuse",
        OverwritePolicy::Overwrite => "Overwrite",
        OverwritePolicy::AppendIfExists => "Append",
    };
    format!("{path} · {rotation} · {on_exists}")
}

/// Shorten a path for a compact display: collapse the user's home directory to `~`
/// (the OS-idiomatic shorthand — `USERPROFILE` on Windows, `HOME` elsewhere), then, if
/// still long, middle-ellipsize so the start and the filename stay visible
/// (`C:\logs\…\gps.raw`). Display-only — never used for the actual path.
fn shorten_path(path: &std::path::Path) -> String {
    const MAX: usize = 28; // characters before middle-ellipsizing (aggressive)

    // Collapse $HOME / %USERPROFILE% to ~.
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from);
    let s = match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    };

    if s.chars().count() <= MAX {
        return s;
    }
    // Keep the filename whole; ellipsize the directory prefix in the middle.
    let sep = std::path::MAIN_SEPARATOR;
    let (dir, file) = match s.rfind(sep) {
        Some(i) => (&s[..i], &s[i + sep.len_utf8()..]),
        None => return s, // single component longer than MAX — leave it
    };
    // Budget for the directory part after reserving the filename + "…\" markers.
    let keep = MAX.saturating_sub(file.chars().count() + 3);
    let head: String = dir.chars().take(keep).collect();
    format!("{head}…{sep}{file}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        card_tone, diagnostics_badge, headline_phrase, pressure_signal, queue_level_reaches_half,
        recent_timing_signal, timing_window, transport_signal, ChannelStatus,
        ARRIVAL_TIMESTAMP_TOOLTIP, CHUNK_SHAPE_TOOLTIP, FINAL_SNAPSHOT_TOOLTIP,
        IDLE_RULE_TIMING_TOOLTIP, IDLE_TIMER_TOOLTIP, PIPELINE_TOOLTIP, PRESSURE_TOOLTIP,
        RAW_QUEUE_ACTIVE_TOOLTIP, RAW_QUEUE_FAULTED_TOOLTIP, SERIAL_BACKPRESSURE_TOOLTIP,
        THROUGHPUT_TOOLTIP, TRANSPORT_TOOLTIP, UDP_AVAILABLE_TOOLTIP, UDP_UNAVAILABLE_TOOLTIP,
    };
    use crate::core::RecordingState;
    use crate::runtime::{
        CounterAvailability, DurationHistogram, QueueDepth, SerialStallSummary, TransportHealth,
    };
    use wiredata_ui::diagnostics::SignalTone;
    use wiredata_ui::format::compact_duration;

    #[test]
    fn headline_phrase_cuts_at_natural_boundaries() {
        // `→ target` is dropped (recording destination).
        assert_eq!(
            headline_phrase("Raw recording started → C:\\Users\\me\\Desktop\\poop"),
            "Raw recording started"
        );
        // A second `: detail` (the OS reason) is dropped; the "<name>: <kind>" is kept.
        assert_eq!(
            headline_phrase(
                "UDP_Channel3: failed to bind interface: Only one usage of each socket \
                 address (os error 10048)"
            ),
            "UDP_Channel3: failed to bind interface"
        );
        // A ` — ` explanatory clause is dropped.
        assert_eq!(
            headline_phrase("recording could not start — check the destination"),
            "recording could not start"
        );
        // No boundary → the whole (trimmed) message is kept (egui ellipsizes if needed).
        assert_eq!(headline_phrase("connected"), "connected");
        assert_eq!(headline_phrase("  spaced  "), "spaced");
    }

    #[test]
    fn active_queue_pressure_preserves_the_existing_half_capacity_reference() {
        assert!(!queue_level_reaches_half(4, 10));
        assert!(queue_level_reaches_half(5, 10));
        assert!(!queue_level_reaches_half(1, 3));
        assert!(queue_level_reaches_half(2, 3));
        assert!(!queue_level_reaches_half(0, 0));
    }

    #[test]
    fn transport_signal_names_kernel_counter_scope_without_claiming_no_loss() {
        let cases = [
            (
                CounterAvailability::Available(0),
                "Kernel drops reported 0",
                SignalTone::Neutral,
            ),
            (
                CounterAvailability::Unsupported,
                "Kernel drops unavailable",
                SignalTone::Neutral,
            ),
            (
                CounterAvailability::NotApplicable,
                "Kernel drops not applicable",
                SignalTone::Neutral,
            ),
            (
                CounterAvailability::Available(3),
                "Kernel drops reported ≥3",
                SignalTone::Fault,
            ),
        ];

        for (availability, expected, tone) in cases {
            let signal = transport_signal(TransportHealth {
                udp_kernel_drops: availability,
                ..TransportHealth::default()
            });
            assert_eq!(signal.value, expected);
            assert_eq!(signal.tone, tone);
            assert!(!signal.value.to_ascii_lowercase().contains("no loss"));
        }
    }

    #[test]
    fn serial_stalls_separate_completed_and_active_measurements() {
        let signal = transport_signal(TransportHealth {
            serial_stalls: Some(SerialStallSummary {
                episodes: 2,
                total: Duration::from_millis(7),
                max: Duration::from_millis(5),
                active_for: Some(Duration::from_millis(3)),
            }),
            ..TransportHealth::default()
        });

        assert_eq!(signal.tone, SignalTone::Warning);
        assert!(signal.value.contains("2 completed episodes"));
        assert!(signal.value.contains(&format!(
            "active {}",
            compact_duration(Duration::from_millis(3))
        )));
        assert!(!signal.value.contains("estimated"));
    }

    #[test]
    fn pressure_signal_escalates_current_raw_pressure_but_not_historical_peaks() {
        let ingest = QueueDepth {
            current: 1,
            peak: 9,
            capacity: 10,
        };
        let raw = QueueDepth {
            current: 2,
            peak: 7,
            capacity: 8,
        };
        let quiet = pressure_signal(ingest, Some(raw), Some(RecordingState::Enabled));
        assert_eq!(quiet.tone, SignalTone::Neutral);
        assert_eq!(
            quiet.value,
            "Ingest highest sampled 9/10 · Raw latest sample 2 · highest observed 7/8"
        );

        let pressured = pressure_signal(
            ingest,
            Some(QueueDepth { current: 4, ..raw }),
            Some(RecordingState::Enabled),
        );
        assert_eq!(pressured.tone, SignalTone::Warning);

        let retained = pressure_signal(ingest, Some(raw), Some(RecordingState::Disabled));
        assert_eq!(retained.tone, SignalTone::Neutral);
        assert_eq!(
            retained.value,
            "Ingest highest sampled 9/10 · Raw retained highest observed 7/8"
        );

        let faulted = pressure_signal(ingest, Some(raw), Some(RecordingState::Faulted));
        assert_eq!(faulted.tone, SignalTone::Fault);
        assert!(faulted.value.contains("Raw faulted"));

        let awaiting = pressure_signal(QueueDepth::default(), None, None);
        assert_eq!(awaiting.value, "Ingest awaiting data · Raw not recording");
    }

    #[test]
    fn pressure_help_locates_both_queues_inside_listener() {
        assert!(PRESSURE_TOOLTIP.contains("Listener's in-memory processing queue"));
        assert!(PRESSURE_TOOLTIP.contains("recorder's in-memory queue"));
        assert!(PRESSURE_TOOLTIP.contains("not device or driver buffers"));
        assert!(PRESSURE_TOOLTIP.contains("not a live current depth"));
        assert!(PRESSURE_TOOLTIP.contains("does not escalate the card by itself"));
        assert!(PRESSURE_TOOLTIP.contains("not proof of data loss"));
    }

    #[test]
    fn technician_help_preserves_measurement_boundaries() {
        assert!(TRANSPORT_TOOLTIP.contains("reader's authoritative state"));
        assert!(TRANSPORT_TOOLTIP.contains("current unfinished stall"));
        assert!(UDP_UNAVAILABLE_TOOLTIP.contains("unknown, not zero"));
        assert!(UDP_AVAILABLE_TOOLTIP.contains("actual run total may be higher"));
        assert!(UDP_AVAILABLE_TOOLTIP.contains("not that no packets were lost"));
        assert!(PIPELINE_TOOLTIP.contains("approximate"));
        assert!(PIPELINE_TOOLTIP.contains("background recorder I/O may run concurrently"));
        assert!(PIPELINE_TOOLTIP.contains("no universal good/bad latency limit"));
    }

    #[test]
    fn technician_help_states_timing_timestamp_and_retention_limits() {
        assert!(SERIAL_BACKPRESSURE_TOOLTIP.contains("authoritative"));
        assert!(SERIAL_BACKPRESSURE_TOOLTIP.contains("completed stalls only"));
        assert!(SERIAL_BACKPRESSURE_TOOLTIP.contains("warnings may be dropped"));
        assert!(IDLE_RULE_TIMING_TOOLTIP.contains("processor contention"));
        assert!(IDLE_RULE_TIMING_TOOLTIP.contains("handling recording before evaluation"));
        assert!(IDLE_TIMER_TOOLTIP.contains("completed Idle-deadline wakes"));
        assert!(IDLE_TIMER_TOOLTIP.contains("One completed wake can fire several rules"));
        assert!(CHUNK_SHAPE_TOOLTIP.contains("full channel run"));
        assert!(CHUNK_SHAPE_TOOLTIP.contains("not the recent ten-second window"));
        assert!(ARRIVAL_TIMESTAMP_TOOLTIP.contains("does not imply nanosecond accuracy"));
        assert!(ARRIVAL_TIMESTAMP_TOOLTIP.contains("Handoff and Read gap always use post-read"));
        assert!(RAW_QUEUE_ACTIVE_TOOLTIP.contains("most recent status snapshot"));
        assert!(RAW_QUEUE_ACTIVE_TOOLTIP.contains("reception continues"));
        assert!(RAW_QUEUE_FAULTED_TOOLTIP.contains("terminal for Raw recording"));
    }

    #[test]
    fn throughput_and_final_snapshot_help_prevent_false_precision() {
        assert!(THROUGHPUT_TOOLTIP.contains("divided by five seconds"));
        assert!(THROUGHPUT_TOOLTIP.contains("full five-second denominator"));
        assert!(THROUGHPUT_TOOLTIP.contains("after Stop the rate is zero"));
        assert!(FINAL_SNAPSHOT_TOOLTIP.contains("channel's final snapshot"));
        assert!(FINAL_SNAPSHOT_TOOLTIP.contains("must not be read as measured zero"));
        assert_eq!(timing_window(ChannelStatus::Running), "~last 10 s");
        assert_eq!(timing_window(ChannelStatus::Stopped), "~final 10 s");
    }

    #[test]
    fn card_badge_escalates_only_from_actionable_transport_or_pressure_evidence() {
        assert_eq!(
            card_tone(SignalTone::Fault, SignalTone::Neutral),
            SignalTone::Fault
        );
        assert_eq!(
            card_tone(SignalTone::Neutral, SignalTone::Warning),
            SignalTone::Warning
        );
        assert_eq!(
            card_tone(SignalTone::Neutral, SignalTone::Neutral),
            SignalTone::Neutral
        );
    }

    #[test]
    fn neutral_badge_distinguishes_live_completed_and_empty_states() {
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Running, false),
            "MONITORING"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Reconnecting, true),
            "MONITORING"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Stopped, true),
            "LAST RUN"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Neutral, ChannelStatus::Stopped, false),
            "AWAITING DATA"
        );
        assert_eq!(
            diagnostics_badge(SignalTone::Warning, ChannelStatus::Stopped, true),
            "ATTENTION"
        );
    }

    #[test]
    fn recent_timing_uses_warmup_max_then_bounded_p99() {
        let mut cumulative = DurationHistogram::default();
        let mut recent = DurationHistogram::default();
        for _ in 0..19 {
            cumulative.record(Duration::from_millis(2));
            recent.record(Duration::from_millis(2));
        }
        let warmup = recent_timing_signal("Handoff", cumulative, recent);
        assert!(warmup.contains("warm-up (19)"));
        assert!(warmup.contains("max"));
        assert!(!warmup.contains("p99"));

        cumulative.record(Duration::from_millis(2));
        recent.record(Duration::from_millis(2));
        let mature = recent_timing_signal("Handoff", cumulative, recent);
        assert!(mature.contains("p99 ≤"));
    }
}
