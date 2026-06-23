//! The channel detail / message-view panel (`show_detail`), split out of `mod.rs`.
//! The decision logic it dispatches (Apply & Start/Restart sequencing, lifecycle
//! actions) lives — and is unit-tested — in [`super::widgets`]. The densest sub-panel,
//! the stream viewer, lives in [`stream_view`].

mod stream_view;

use std::time::SystemTime;

use crate::core::{ChannelId, RecordingState};
use crate::diagnostics::DiagnosticSeverity;

use super::bridge::{self, UiCommand};
use super::fonts::bold;
use super::state::ChannelStatus;
use super::theme;
use super::widgets::{
    config_needs_restart, edit_display_recording, edit_interface, edit_raw_recording, human_bytes,
    line_indicator, line_toggle, paint_glyph, recording_glyph_size, recording_indicator, short_id,
    start_button, status_color, status_glyph, status_label, stop_enabled,
};
use super::ListenerApp;

/// Uniform size for the lifecycle / recording control buttons. Text wider than the
/// min grows the button (so "Apply & Restart" doesn't clip).
const CONTROL_BUTTON_SIZE: egui::Vec2 = egui::vec2(96.0, 32.0);

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
            ui.colored_label(theme::FAULT_RED, format!("⚠ {err}"));
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
            egui::CollapsingHeader::new("Configure channel")
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
                    // Display (.disp) recording is part of display config, so it lives
                    // here under Configure — Raw recording is the separate panel above
                    // (ADR-013).
                    ui.separator();
                    ui.label(bold("Display record"));
                    edit_display_recording(ui, config);
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

    /// Diagnostics + match-firings for the selected channel (snapshot-driven): a
    /// color-coded headline that opens a filterable, ms-timestamped log, plus the
    /// cross-chunk match measurement. Split out of `show_detail`.
    fn show_diagnostics(&mut self, ui: &mut egui::Ui, id: ChannelId) {
        // Diagnostics / matches — only meaningful once there's a snapshot. Pull the
        // data into owned locals so the filter checkboxes can mutate `self` without a
        // live `self.state` borrow.
        struct DiagView {
            headline_level: &'static str,
            headline: String,
            headline_color: egui::Color32,
            counts: (usize, usize, usize),
            entries: Vec<(SystemTime, DiagnosticSeverity, String)>,
            matches: Vec<(Option<u64>, String)>,
            /// How many matches were recovered across a read-chunk boundary (§50.2):
            /// the cross-chunk-carry measurement (where/why land in the diag log).
            boundary_saves: u64,
        }
        let view = self.state.channel(id);
        // The diagnostics log comes *only* from the snapshot — the GUI never synthesizes
        // entries. The runtime is the single writer: live diagnostics arrive via the 5 Hz
        // poll; stop-time notes via the final snapshot taken at stop; and a start/bind
        // fault (which never ran a pipeline) is retained by the runtime and served via a
        // minimal snapshot for the faulted channel. The snapshot is kept across stop/start
        // so a previous run's messages persist.
        let mut entries: Vec<(SystemTime, DiagnosticSeverity, String)> = Vec::new();
        let (counts, matches, boundary_saves) = match view.and_then(|v| v.snapshot.as_ref()) {
            Some(s) => {
                let d = &s.diagnostics;
                for e in d.events.iter().chain(&d.warnings).chain(&d.errors) {
                    entries.push((e.timestamp, e.severity, e.message.clone()));
                }
                let matches = s
                    .matches
                    .iter()
                    .rev()
                    .take(20)
                    .map(|m| (m.byte_offset, short_id(&m.rule_id.to_string()).to_string()))
                    .collect();
                (
                    (d.events.len(), d.warnings.len(), d.errors.len()),
                    matches,
                    s.match_boundary_saves,
                )
            }
            None => ((0, 0, 0), Vec::new(), 0),
        };
        let diag_view = if entries.is_empty() {
            None
        } else {
            // Chronological (a single timeline across severities). The headline is the
            // latest entry, so a fresh INFO supersedes an older ERROR — and a just-merged
            // fault (newest) leads.
            entries.sort_by_key(|(t, _, _)| *t);
            let (headline_level, headline, headline_color) = match entries.last() {
                Some((_, sev, msg)) => {
                    let (level, color) = match sev {
                        DiagnosticSeverity::Event => ("INFO", theme::EVENT_GREY),
                        DiagnosticSeverity::Warning => ("WARN", theme::WARNING_AMBER),
                        DiagnosticSeverity::Error => ("ERROR", theme::FAULT_RED),
                    };
                    (level, msg.clone(), color)
                }
                None => ("", "no activity yet".to_string(), theme::IDLE_GREY),
            };
            Some(DiagView {
                headline_level,
                headline,
                headline_color,
                counts,
                entries,
                matches,
                boundary_saves,
            })
        };
        if let Some(dv) = diag_view {
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
                        for (t, sev, msg) in dv.entries.iter().rev() {
                            let (enabled, color, level) = match sev {
                                DiagnosticSeverity::Event => {
                                    (self.show_info, theme::INFO_GREY, "INFO ")
                                }
                                DiagnosticSeverity::Warning => {
                                    (self.show_warn, theme::WARNING_AMBER, "WARN ")
                                }
                                DiagnosticSeverity::Error => {
                                    (self.show_error, theme::FAULT_RED, "ERROR")
                                }
                            };
                            if !enabled {
                                continue;
                            }
                            let dt: chrono::DateTime<chrono::Local> = (*t).into();
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
            if !dv.matches.is_empty() || dv.boundary_saves > 0 {
                egui::CollapsingHeader::new(format!("Match firings ({})", dv.matches.len()))
                    .id_salt("matches")
                    .show(ui, |ui| {
                        // Cross-chunk measurement (§50.2): how often a pattern was
                        // recovered across a read boundary. The where/why per
                        // occurrence is in the Diagnostics log above.
                        if dv.boundary_saves > 0 {
                            ui.colored_label(
                                theme::WARNING_AMBER,
                                format!(
                                    "⮧ {} match{} spanned a read-chunk boundary \
                                     (recovered; see Diagnostics for where)",
                                    dv.boundary_saves,
                                    if dv.boundary_saves == 1 { "" } else { "es" },
                                ),
                            );
                        }
                        for (offset, rule) in &dv.matches {
                            let on = offset
                                .map(|n| format!("@{n}"))
                                .unwrap_or_else(|| "(idle)".to_string());
                            ui.monospace(format!("{on}  rule {rule}"));
                        }
                    });
            }
        }
    }

    /// The channel block (left column): name (live rename), status·details, byte
    /// stats, and the [Start / Apply & Restart / Retry] [Stop] lifecycle row. Both
    /// buttons always present; Stop disabled unless stoppable; the Start side's
    /// label/enabled is the pure `start_button` decision.
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
            // Painted into a fixed cell so the glyph never drives the row height (the
            // name line stayed put across status changes only once this stopped using a
            // sized label).
            let (glyph, scale) = status_glyph(status);
            paint_glyph(ui, glyph, scale, status_color(status));
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
                        .color(theme::WARNING_AMBER),
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
        ));
        // Bounded-queue occupancy (§99) — current/peak/capacity, for stress testing.
        // The peak is the value that matters; near capacity means backpressure is
        // imminent (a reception stall or a recording-queue-overflow fault).
        if let Some(view) = self.state.channel(id) {
            let iq = view.ingest_queue;
            let mut line = format!(
                "Ingest queue: {}/{} (peak {})",
                iq.current, iq.capacity, iq.peak
            );
            if let Some(rq) = view.raw_recording_queue {
                line.push_str(&format!(
                    "    Rec queue: {}/{} (peak {})",
                    rq.current, rq.capacity, rq.peak
                ));
            }
            // Amber once any queue's peak has reached half its capacity — an early
            // backpressure warning while stress testing.
            let pressured = iq.peak * 2 >= iq.capacity.max(1)
                || view
                    .raw_recording_queue
                    .is_some_and(|rq| rq.peak * 2 >= rq.capacity.max(1));
            let text = egui::RichText::new(line).weak();
            ui.label(if pressured {
                text.color(theme::WARNING_AMBER)
            } else {
                text
            });
        }
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

    /// Raw recording block: a header row ("Record Raw Data" + live state glyph + the
    /// Start/Stop recording button) over the always-shown setup fields. The live toggle
    /// reads the recording settings from the editor at click time (ADR-012).
    ///
    /// Kept in small pieces (the header row, `raw_record_button`, the setup) because
    /// this block is still evolving — add new recording controls as their own helpers
    /// rather than growing this method.
    fn show_recording_block(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
    ) {
        // Header row: title + live state indicator + the Start/Stop recording button on
        // the same line (no expander — the setup is always shown below).
        ui.horizontal(|ui| {
            ui.label(bold("Record Raw Data"));
            // Status glyph only (same symbol set/colors as channel status) — the word
            // ("recording"/"off"/"faulted") is dropped to keep the row compact; the glyph
            // ■/●/⚠ carries the state.
            let (glyph, color, _text) = recording_indicator(recording);
            paint_glyph(ui, glyph, recording_glyph_size(glyph), color);
            self.raw_record_button(ui, id, status, recording);
        });
        // Setup: a collapsible block (collapsed by default). Expanded shows the full
        // editor; collapsed shows a one-line summary (path · rotation · on-exists) so
        // the configured destination stays visible without the controls.
        let Some((_, config)) = &mut self.edit_draft else {
            ui.label(egui::RichText::new("(select the channel to edit)").weak());
            return;
        };
        let setup_id = ui.make_persistent_id(("raw_rec_setup", id));
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            setup_id,
            false,
        );
        let open = state.is_open();
        state
            .show_header(ui, |ui| {
                ui.label(bold("Setup"));
                // Show the summary on the (closed) header so it reads as one line; when
                // open, the full editor is in the body below, so keep the header terse.
                if !open {
                    ui.label(egui::RichText::new(raw_record_summary(&config.raw_recording)).weak());
                }
            })
            .body(|ui| edit_raw_recording(ui, config));

        self.persist_raw_recording(id);
    }

    /// Persist Raw recording edits (destination/rotation/overwrite/"record on start").
    /// Raw recording is applied live (no restart), so its edits never travel through
    /// the Apply & Restart path — without this, a profile save wouldn't capture them.
    /// When the draft's `raw_recording` differs from the channel's stored config, fold
    /// it into the stored config and sync the runtime (`SetRawRecordingConfig`).
    fn persist_raw_recording(&mut self, id: ChannelId) {
        let draft = self
            .edit_draft
            .as_ref()
            .filter(|(eid, _)| *eid == id)
            .map(|(_, cfg)| cfg.raw_recording.clone());
        let Some(draft) = draft else { return };
        if let Some(view) = self.state.channel_mut(id) {
            if view.config.raw_recording != draft {
                view.config.raw_recording = draft.clone();
                self.send(UiCommand::SetRawRecordingConfig(id, Box::new(draft)));
            }
        }
    }

    /// The Raw-recording controls on the header row: a "Record on start" toggle (the
    /// `raw_recording.enabled` flag — begins recording when the channel next starts,
    /// §53) and, for a running channel, the live Start/Stop Record button (ADR-012).
    /// The button reads the on-screen settings *at click time* (from the edit draft) and
    /// sends them with the command, so recording goes exactly where the controls say —
    /// no restart, no Apply.
    fn raw_record_button(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
    ) {
        // "Record on start" — the auto-start flag, editable whether or not the channel
        // is running (it governs the next start). Edits the draft's raw_recording.enabled.
        if let Some((_, config)) = &mut self.edit_draft {
            ui.checkbox(&mut config.raw_recording.enabled, "Record on start")
                .on_hover_text("Begin recording automatically when the channel starts (§53).");
        }
        if status != ChannelStatus::Running {
            return;
        }
        let draft_raw = self
            .edit_draft
            .as_ref()
            .filter(|(eid, _)| *eid == id)
            .map(|(_, cfg)| cfg.raw_recording.clone());
        let has_dest = draft_raw.as_ref().is_some_and(|r| r.destination.is_some());
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
            let raw = draft_raw.unwrap_or_default();
            self.send(UiCommand::SetRecording(id, !recording_now, Box::new(raw)));
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

/// A one-line summary of a Raw recording config for the collapsed Setup header:
/// `path · rotation · on-exists` (e.g. `C:\logs\gps.raw · Daily · Append`). The path
/// reads "(no destination)" when unset; rotation/on-exists use short words.
fn raw_record_summary(rec: &crate::config::RawRecordingConfig) -> String {
    use crate::record::{FileRotationPolicy, OverwritePolicy};
    let path = rec
        .destination
        .as_ref()
        .map(|p| shorten_path(p))
        .unwrap_or_else(|| "(no destination)".to_string());
    let rotation = match rec.file_rotation {
        FileRotationPolicy::None => "no rotation",
        FileRotationPolicy::Hourly => "Hourly",
        FileRotationPolicy::Daily => "Daily",
    };
    let on_exists = match rec.overwrite_policy {
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
    use super::headline_phrase;

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
}
