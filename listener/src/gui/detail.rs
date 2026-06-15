//! The channel detail / message-view panel (`show_detail`), split out of `mod.rs`.
//! The decision logic it dispatches (Apply & Start/Restart sequencing, lifecycle
//! actions) lives — and is unit-tested — in [`super::widgets`].

use std::time::SystemTime;

use crate::core::{ChannelId, RecordingState};
use crate::diagnostics::DiagnosticSeverity;
use crate::display::{CharacterRendering, DisplayEncoding, DisplayMode, DisplayView, WrappingMode};

use super::bridge::{self, UiCommand};
use super::fonts::{bold, MonoFont};
use super::state::ChannelStatus;
use super::widgets::{
    config_differs_ignoring_name, edit_display_recording, edit_interface, edit_raw_recording,
    human_bytes, latest_diagnostic, line_indicator, line_toggle, recording_indicator, short_id,
    start_button, status_color, status_label, truncate, vsep, ColorScheme, MSG_FONT_SIZES,
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

        // Top line: status dot + an editable optional name (#6). The name is part of
        // the config draft; Apply commits it (and the list row mirrors it).
        ui.horizontal(|ui| {
            let base = egui::TextStyle::Body.resolve(ui.style()).size;
            ui.label(
                egui::RichText::new("\u{25CF}")
                    .size(base * 1.3)
                    .color(status_color(status)),
            );
            ui.label(bold("Name"));
            // Editing the name renames the channel in place — instant, no restart
            // (the name is a label only, §6). Keep the draft in step so a later
            // Apply of other edits doesn't carry a stale name.
            let mut renamed = None;
            if let Some((_, config)) = &mut self.edit_draft {
                let mut name = config.name.as_str().to_string();
                if ui
                    .add(egui::TextEdit::singleline(&mut name).desired_width(200.0))
                    .changed()
                {
                    config.name = crate::core::ChannelName::new(name.clone());
                    renamed = Some(name);
                }
            }
            if let Some(name) = renamed {
                // Tell the runtime, AND fold the change into the view-model locally.
                // The runtime→UI confirmation (`ChannelRenamed`) is an advisory,
                // lossy push (§99) and can be dropped under load — relying on it
                // left the list row and the re-seeded draft stale (#5). The optimistic
                // local apply makes the rename authoritative on the UI side; the
                // echoed update, if it survives, is idempotent.
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
            ui.label(egui::RichText::new(&details).weak());
        });
        // Byte-based liveness for the Stream viewer (§18: Stream Mode has no Message
        // count). Total received + rolling throughput; warnings live in Diagnostics.
        ui.label(format!(
            "Received: {}    Throughput: {bps:.0} B/s",
            human_bytes(bytes_total)
        ));
        // Does the edit draft differ from the channel's committed config? (Name is
        // excluded — it renames live, not via restart.) Drives the Start button label.
        let config_changed = match (&self.edit_draft, self.state.channel(id)) {
            (Some((eid, draft)), Some(view)) if *eid == id => {
                config_differs_ignoring_name(draft, &view.config)
            }
            _ => false,
        };
        // Stacked: channel lifecycle row, then the recording block. Each is its own
        // method (readability) — moving the layout is moving these calls. (Two-column
        // side-by-side was tried and reverted; it broke the panel. Keep it stacked.)
        self.show_channel_controls(ui, id, status, config_changed);
        self.show_recording_block(ui, id, status, recording, &rec_dest);
        if let Some(err) = &last_error {
            ui.colored_label(egui::Color32::from_rgb(170, 30, 30), format!("⚠ {err}"));
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
            egui::CollapsingHeader::new("Configure")
                // Per-channel id so each channel remembers its own open/closed state
                // — closing it on a running channel stays closed on return (#1).
                .id_salt(("configure", id))
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

        ui.horizontal(|ui| {
            let base = egui::TextStyle::Body.resolve(ui.style()).size;
            ui.label(bold("View"));
            ui.radio_value(&mut self.msg_mode, DisplayMode::Hex, "Hex");
            ui.radio_value(&mut self.msg_mode, DisplayMode::Rendered, "Rendered");
            ui.radio_value(&mut self.msg_mode, DisplayMode::Raw, "Raw");
            // Fixed-height dividers so the enlarged ␊ below doesn't stretch them.
            vsep(ui);
            // Control-character rendering (§46) — applies to Raw mode. Three styles,
            // matching talker (glyph / token / hex; LF shown as the example). The
            // control-picture glyph is a compact 2-letter design, so it's bumped up to
            // visually match the full-size [LF]/<0A> neighbours.
            ui.add_enabled_ui(self.msg_mode == DisplayMode::Raw, |ui| {
                ui.label(bold("ctrl-chars"));
                ui.radio_value(
                    &mut self.msg_chars,
                    CharacterRendering::Glyph,
                    egui::RichText::new("␊").size(base * 1.6),
                )
                .on_hover_text("Control pictures (␊ ␍ ␉ …)");
                ui.radio_value(&mut self.msg_chars, CharacterRendering::Token, "[LF]")
                    .on_hover_text("Bracketed names ([LF] [CR] [TAB] …)");
                ui.radio_value(&mut self.msg_chars, CharacterRendering::HexEscape, "<0A>")
                    .on_hover_text("Hex escapes (<0A> <0D> <09> …)");
            });
        });
        ui.horizontal(|ui| {
            ui.label(bold("Size"));
            // An editable "combo": type any size into the field, or pick a preset
            // from the ▾ menu (#7 — no separate entry box). The text is the source of
            // truth while editing; a valid parse updates the size (clamped).
            let resp = ui.add(egui::TextEdit::singleline(&mut self.font_text).desired_width(40.0));
            if resp.changed() {
                if let Ok(v) = self.font_text.trim().parse::<f32>() {
                    self.msg_font_size = v.clamp(6.0, 72.0);
                }
            }
            // U+25BC (full triangle), not U+25BE (the "small" one) — the small glyph
            // rendered noticeably tinier than the ComboBox / collapsing arrows.
            ui.menu_button("\u{25BC}", |ui| {
                for &size in MSG_FONT_SIZES {
                    if ui.button(format!("{size:.0}")).clicked() {
                        self.msg_font_size = size;
                        self.font_text = format!("{size:.0}");
                        ui.close();
                    }
                }
            });
            ui.separator();
            // Color scheme as a dropdown. It opens instantly now that the global
            // popup fade is off (see `apply_style`).
            ui.label(bold("Colors"));
            egui::ComboBox::from_id_salt("msg_colors")
                .selected_text(self.msg_colors.label())
                .show_ui(ui, |ui| {
                    for scheme in [
                        ColorScheme::BlackOnWhite,
                        ColorScheme::GreenOnBlack,
                        ColorScheme::AmberOnBlack,
                        ColorScheme::WhiteOnBlack,
                    ] {
                        ui.selectable_value(&mut self.msg_colors, scheme, scheme.label());
                    }
                });
            ui.separator();
            // Monospace face for the message dump.
            ui.label(bold("Mono"));
            egui::ComboBox::from_id_salt("msg_font")
                .selected_text(self.msg_font.label())
                .show_ui(ui, |ui| {
                    for &font in MonoFont::ALL {
                        ui.selectable_value(&mut self.msg_font, font, font.label());
                    }
                });
        });
        let msg_mode = self.msg_mode;
        let msg_chars = self.msg_chars;
        let font_size = self.msg_font_size;
        let mono_family = self.msg_font.family();
        let fg = self.msg_colors.fg();
        let bg = self.msg_colors.bg();
        ui.separator();

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
                                        (self.show_info, egui::Color32::from_gray(80), "INFO ")
                                    }
                                    DiagnosticSeverity::Warning => (
                                        self.show_warn,
                                        egui::Color32::from_rgb(150, 100, 0),
                                        "WARN ",
                                    ),
                                    DiagnosticSeverity::Error => (
                                        self.show_error,
                                        egui::Color32::from_rgb(170, 30, 30),
                                        "ERROR",
                                    ),
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
                                egui::Color32::from_rgb(150, 100, 0),
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

        ui.separator();
        // Pause/Resume the primary Display View, if one exists.
        let view0 = self
            .state
            .channel(id)
            .and_then(|v| v.snapshot.as_ref())
            .and_then(|s| s.display_views.first())
            .map(|v0| (v0.id, v0.paused));
        if let Some((view_id, is_paused)) = view0 {
            ui.horizontal(|ui| {
                if is_paused {
                    if ui.button("Resume").clicked() {
                        self.send(UiCommand::ResumeDisplay(id, view_id));
                    }
                    ui.label("view paused — reception continues");
                } else if ui.button("Pause").clicked() {
                    self.send(UiCommand::PauseDisplay(id, view_id));
                }
            });
        }

        // The message area is ALWAYS present (#1) — it shows a waiting note before
        // there's data, rather than popping into existence on first Start.
        let has_snapshot = self
            .state
            .channel(id)
            .map(|v| v.snapshot.is_some())
            .unwrap_or(false);
        // The stream viewer (§41): there is one source — the verbatim byte stream.
        let renderer = DisplayView {
            mode: msg_mode,
            encoding: DisplayEncoding::Utf8,
            character_rendering: msg_chars,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_line: 16,
        };
        let stream_len = self
            .state
            .channel(id)
            .map(|v| v.stream_bytes.len())
            .unwrap_or(0);
        ui.label(format!("Stream ({}):", human_bytes(stream_len as u64)));

        // Verbatim received bytes (§17–18, §41): line breaks come only from the
        // data — Rendered honors real CR/LF (§44), Raw shows control pictures, Hex
        // is a byte run. Serial and UDP render identically (no reframing).
        //
        // Performance (§100): the scrollback can reach the ~1 MB cap. The bytes
        // arrive incrementally (StreamDelta) so the driver never re-ships the whole
        // buffer; here we (a) memoize the split rows, re-rendering only when data
        // arrives or the view mode changes — keyed on the stream cursor — and (b)
        // virtualize the layout with `show_rows`, laying out only visible rows. Both
        // matter: a non-virtualized selectable Label over ~1 MB stalled the UI.
        let font = egui::FontId::new(font_size, mono_family.clone());
        // Monospace metrics: row height and the width of one glyph ('0' as a stand-in),
        // so we can convert the available pixel width into a column count for wrapping.
        let (row_h, char_w) =
            ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0').max(1.0)));
        // Columns that fit in the current viewer width (minus the frame margin + a
        // little slack for the scrollbar). Pre-wrapping to this keeps every cached row
        // exactly one visual line — uniform height — so we can soft-wrap *and*
        // virtualize with `show_rows`. Re-wrap only when this column count changes.
        let avail_w = (ui.available_width() - 24.0).max(char_w);
        let wrap_cols = (avail_w / char_w).floor().max(8.0) as usize;
        self.refresh_stream_rows(id, &renderer, wrap_cols);
        let rows: &[String] = self
            .stream_cache
            .as_ref()
            .filter(|c| c.key.channel == id)
            .map(|c| c.rows.as_slice())
            .unwrap_or(&[]);

        egui::Frame::new()
            .fill(bg)
            .inner_margin(4.0)
            .show(ui, |ui| {
                if stream_len == 0 {
                    let note = if has_snapshot {
                        "no data received yet"
                    } else {
                        "waiting for data — Start the channel"
                    };
                    ui.label(egui::RichText::new(note).weak());
                    return;
                }
                // Text selection across the (non-interactive) row labels.
                ui.style_mut().interaction.selectable_labels = true;
                // Suppress the per-widget hover/active visuals egui paints on selectable
                // labels (they drew a flickering box around the rows near the pointer).
                {
                    let w = &mut ui.visuals_mut().widgets;
                    for s in [
                        &mut w.hovered,
                        &mut w.active,
                        &mut w.inactive,
                        &mut w.noninteractive,
                    ] {
                        s.bg_stroke = egui::Stroke::NONE;
                        s.weak_bg_fill = egui::Color32::TRANSPARENT;
                        s.bg_fill = egui::Color32::TRANSPARENT;
                    }
                }
                // Soft-wrapped AND virtualized: rows are pre-wrapped to `wrap_cols`
                // (above) so each is one uniform-height visual line, which lets
                // `show_rows` lay out only the visible rows. This is what keeps the UI
                // responsive — the earlier "render everything" approaches (N labels or
                // one giant galley) re-laid-out the whole buffer every frame and made
                // resizing/the whole UI sluggish. Rows don't wrap again here (they're
                // already wrapped); they just extend if anything slipped through.
                egui::ScrollArea::vertical()
                    .id_salt("stream")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, rows.len().max(1), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for row in &rows[range] {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(row).font(font.clone()).color(fg),
                                )
                                .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        }
                    });
            });
    }

    /// Channel lifecycle row: [Start / Apply & Restart / Retry] [Stop]. Both buttons
    /// always present; Stop disabled unless Running; the Start side's label/enabled is
    /// the pure `start_button` decision. Layout is one `horizontal` row — extracted for
    /// readability, not changed.
    fn show_channel_controls(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        config_changed: bool,
    ) {
        let size = CONTROL_BUTTON_SIZE;
        ui.horizontal(|ui| {
            let (start_label, start_enabled) = start_button(status, config_changed);
            if ui
                .add_enabled(start_enabled, egui::Button::new(start_label).min_size(size))
                .clicked()
            {
                if status == ChannelStatus::Running {
                    self.apply_and_restart(id); // one coherent restart onto the new config
                } else {
                    if status == ChannelStatus::Faulted {
                        self.send(UiCommand::Stop(id)); // §8.5: Faulted -> Stop first
                    }
                    self.try_start(id); // applies pending edits, then starts
                }
            }
            if ui
                .add_enabled(
                    status == ChannelStatus::Running,
                    egui::Button::new("Stop Channel").min_size(size),
                )
                .clicked()
            {
                self.send(UiCommand::Stop(id));
            }
        });
    }

    /// Raw recording block: the "[▸] Record Raw Data  ●/■ state  [Start/Stop
    /// recording]" line and the expandable setup. The live toggle reads the recording
    /// settings from the editor at click time (ADR-012). Extracted for readability;
    /// the layout (a header row + an optional setup group) is unchanged.
    fn show_recording_block(
        &mut self,
        ui: &mut egui::Ui,
        id: ChannelId,
        status: ChannelStatus,
        recording: Option<RecordingState>,
        rec_dest: &Option<std::path::PathBuf>,
    ) {
        let size = CONTROL_BUTTON_SIZE;
        // Globally-stable id so the expander toggle and its body read one flag.
        let open_id = egui::Id::new(("raw_rec_open", id));
        ui.horizontal(|ui| {
            let mut open = ui
                .ctx()
                .data_mut(|d| d.get_temp::<bool>(open_id))
                .unwrap_or(false);
            let arrow = if open { "\u{25BE}" } else { "\u{25B8}" }; // ▾ / ▸
            if ui
                .button(arrow)
                .on_hover_text("Raw recording setup")
                .clicked()
            {
                open = !open;
                ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
            }
            ui.label(bold("Record Raw Data"));
            let (dot, color, text) = recording_indicator(recording);
            ui.colored_label(color, format!("{dot} {text}"));
            // Live recording toggle, for a running channel. Reads the recording settings
            // from the editor *at click time* and sends them with the command, so it
            // records to exactly what's on screen — no Apply, no restart (ADR-012).
            // Enabled when the on-screen settings have a destination.
            if status == ChannelStatus::Running {
                let draft_raw = self
                    .edit_draft
                    .as_ref()
                    .filter(|(eid, _)| *eid == id)
                    .map(|(_, cfg)| cfg.raw_recording.clone());
                let has_dest = draft_raw.as_ref().is_some_and(|r| r.destination.is_some());
                let recording_now = matches!(recording, Some(RecordingState::Enabled));
                let label = if recording_now {
                    "\u{25A0} Stop recording"
                } else {
                    "\u{25CF} Start recording"
                };
                let resp = ui.add_enabled(
                    has_dest || recording_now,
                    egui::Button::new(label).min_size(size),
                );
                let resp = if !has_dest && !recording_now {
                    resp.on_hover_text("Set a destination in the Raw recording setup (▸) first")
                } else {
                    resp
                };
                if resp.clicked() {
                    let raw = draft_raw.unwrap_or_default();
                    self.send(UiCommand::SetRecording(id, !recording_now, Box::new(raw)));
                }
            }
        });
        // Raw recording setup body, shown when expanded.
        let raw_open = ui
            .ctx()
            .data_mut(|d| d.get_temp::<bool>(open_id))
            .unwrap_or(false);
        if raw_open {
            ui.group(|ui| {
                if let Some(RecordingState::Enabled) = recording {
                    let dest = rec_dest
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(no destination)".to_string());
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 40, 40),
                        format!("\u{25CF} Recording \u{2192} {dest}"),
                    );
                }
                if let Some((_, config)) = &mut self.edit_draft {
                    edit_raw_recording(ui, config);
                } else {
                    ui.label(egui::RichText::new("(select the channel to edit)").weak());
                }
            });
        }
    }

    /// Refresh the memoized stream-view rows for `id` if the accumulated bytes or
    /// the render settings changed. Keyed on the view's stream cursor (advances as
    /// deltas are folded) plus the view mode/character rendering, so we re-render
    /// the scrollback only when something actually changed — not every frame.
    fn refresh_stream_rows(&mut self, id: ChannelId, renderer: &DisplayView, wrap_cols: usize) {
        let Some(view) = self.state.channel_mut(id) else {
            self.stream_cache = None;
            return;
        };
        let key = super::StreamRenderKey {
            channel: id,
            cursor: view.stream_cursor,
            len: view.stream_bytes.len(),
            mode: self.msg_mode,
            chars: self.msg_chars,
            wrap_cols,
        };
        if self.stream_cache.as_ref().is_some_and(|c| c.key == key) {
            return; // still valid — reuse the cached rows
        }
        let rows = split_stream_rows(&renderer.render_text(view.stream_contiguous()), wrap_cols);
        self.stream_cache = Some(super::StreamRenderCache { key, rows });
    }
}

/// Split rendered stream text into virtualization rows, soft-wrapping each data line
/// to `wrap_cols` monospace columns so every row is exactly one visual line (uniform
/// height — required by `show_rows`). A data line shorter than `wrap_cols` is one row;
/// a longer one (or a line with no LF, e.g. raw binary / UDP) is wrapped into several.
fn split_stream_rows(text: &str, wrap_cols: usize) -> Vec<String> {
    let cols = wrap_cols.max(8);
    let mut rows: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        // Wrap by character count (monospace, so columns == chars). Char-based, not
        // byte-based, so multi-byte UTF-8 isn't split mid-codepoint.
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(cols) {
            rows.push(chunk.iter().collect());
        }
    }
    rows
}
