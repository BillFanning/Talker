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
    config_differs_ignoring_name, edit_display_recording, edit_interface, edit_raw_recording,
    human_bytes, latest_diagnostic, line_indicator, line_toggle, paint_glyph, recording_glyph_size,
    recording_indicator, short_id, start_button, status_color, status_glyph, status_label,
    stop_enabled, truncate,
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
        let Some((details, status, bytes_total, bps, last_error, recording, rec_dest)) =
            self.state.channel(id).map(|v| {
                (
                    v.details.clone(),
                    v.status,
                    v.bytes_total,
                    v.bytes_per_sec,
                    v.last_error.clone(),
                    v.recording,
                    v.config.raw_recording.destination.clone(),
                )
            })
        else {
            ui.label("That channel is no longer present.");
            return;
        };

        // Does the edit draft differ from the channel's committed config? (Name is
        // excluded — it renames live, not via restart.) Drives the Start button label.
        let config_changed = match (&self.edit_draft, self.state.channel(id)) {
            (Some((eid, draft)), Some(view)) if *eid == id => {
                config_differs_ignoring_name(draft, &view.config)
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
                self.show_recording_block(&mut cols[1], id, status, recording, &rec_dest);
            });
        });
        if let Some(err) = &last_error {
            ui.colored_label(theme::FAULT_RED, format!("⚠ {err}"));
            ui.label(
                "Recourse: change the port below and Apply, free the resource (Stop \
                 the other channel on that port) then Retry, or Remove this channel.",
            );
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
            headline: String,
            headline_color: egui::Color32,
            counts: (usize, usize, usize),
            entries: Vec<(SystemTime, DiagnosticSeverity, String)>,
            matches: Vec<(Option<u64>, String)>,
            /// How many matches were recovered across a read-chunk boundary (§50.2):
            /// the cross-chunk-carry measurement (where/why land in the diag log).
            boundary_saves: u64,
        }
        let diag_view = self
            .state
            .channel(id)
            .and_then(|v| v.snapshot.as_ref())
            .map(|s| {
                let d = &s.diagnostics;
                let mut entries: Vec<(SystemTime, DiagnosticSeverity, String)> = Vec::new();
                for e in d.events.iter().chain(&d.warnings).chain(&d.errors) {
                    entries.push((e.timestamp, e.severity, e.message.clone()));
                }
                // Chronological now that each entry carries a timestamp (a single
                // timeline across severities, not three separate buckets).
                entries.sort_by_key(|(t, _, _)| *t);
                let (headline, headline_color) = latest_diagnostic(d);
                DiagView {
                    headline,
                    headline_color,
                    counts: (d.events.len(), d.warnings.len(), d.errors.len()),
                    entries,
                    matches: s
                        .matches
                        .iter()
                        .rev()
                        .take(20)
                        .map(|m| (m.byte_offset, short_id(&m.rule_id.to_string()).to_string()))
                        .collect(),
                    boundary_saves: s.match_boundary_saves,
                }
            });
        if let Some(dv) = diag_view {
            // A real-time, color-coded status line (the channel's headline diagnostic)
            // that opens into a filterable, ms-timestamped log. The header updates
            // every snapshot (5 Hz); errors stay headlined over warnings over info.
            let header =
                egui::RichText::new(format!("Diagnostics — {}", truncate(&dv.headline, 70)))
                    .color(dv.headline_color);
            egui::CollapsingHeader::new(header)
                .id_salt("diagnostics")
                .show(ui, |ui| {
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
                                ui.colored_label(
                                    color,
                                    format!("{}  {level}  {msg}", dt.format("%H:%M:%S%.3f")),
                                );
                            }
                            if shown == 0 {
                                ui.label(
                                    egui::RichText::new("no diagnostics match the filter").weak(),
                                );
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
                // Tell the runtime AND fold locally — the echoed ChannelRenamed is
                // advisory/lossy (§99), so the optimistic local apply keeps the list row
                // and re-seeded draft authoritative (#5).
                self.send(UiCommand::Rename(
                    id,
                    crate::core::ChannelName::new(name.clone()),
                ));
                self.state.apply(bridge::UiUpdate::ChannelRenamed(id, name));
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

    /// Raw recording block: a "Record Raw Data  ●/■ state" header, the always-shown
    /// setup fields, and a Start/Stop recording button at the bottom. The live toggle
    /// reads the recording settings from the editor at click time (ADR-012).
    fn show_recording_block(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
        rec_dest: &Option<std::path::PathBuf>,
    ) {
        let size = CONTROL_BUTTON_SIZE;
        // Header: title + live state indicator (no expander — the setup is always shown).
        ui.horizontal(|ui| {
            ui.label(bold("Record Raw Data"));
            // Same painted-glyph technique + symbol set as channel status, then the text.
            let (glyph, color, text) = recording_indicator(recording);
            paint_glyph(ui, glyph, recording_glyph_size(glyph), color);
            ui.colored_label(color, text);
        });
        if let Some(RecordingState::Enabled) = recording {
            let dest = rec_dest
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(no destination)".to_string());
            ui.colored_label(
                theme::FAULT_RED,
                format!("\u{25CF} Recording \u{2192} {dest}"),
            );
        }
        // Setup fields, always visible.
        if let Some((_, config)) = &mut self.edit_draft {
            edit_raw_recording(ui, config);
        } else {
            ui.label(egui::RichText::new("(select the channel to edit)").weak());
        }
        // Start/Stop recording button at the bottom of the config. Live (no restart,
        // ADR-012): reads the on-screen settings at click time. Only for a running
        // channel; enabled once a destination is set.
        if status == ChannelStatus::Running {
            let draft_raw = self
                .edit_draft
                .as_ref()
                .filter(|(eid, _)| *eid == id)
                .map(|(_, cfg)| cfg.raw_recording.clone());
            let has_dest = draft_raw.as_ref().is_some_and(|r| r.destination.is_some());
            let recording_now = matches!(recording, Some(RecordingState::Enabled));
            let label = if recording_now { "Stop" } else { "Record" };
            // Match the start-channel button's *width* (96) but keep the default
            // height — a full CONTROL_BUTTON_SIZE min_size plus a long label made it
            // both too wide and too tall. Short labels fit the 96px width.
            let resp = ui.add_enabled(
                has_dest || recording_now,
                egui::Button::new(label).min_size(egui::vec2(size.x, 0.0)),
            );
            let resp = if !has_dest && !recording_now {
                resp.on_hover_text("Set a destination above first")
            } else {
                resp
            };
            if resp.clicked() {
                let raw = draft_raw.unwrap_or_default();
                self.send(UiCommand::SetRecording(id, !recording_now, Box::new(raw)));
            }
        }
    }
}
