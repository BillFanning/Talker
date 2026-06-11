//! The channel detail / message-view panel (`show_detail`), split out of `mod.rs`.
//! The decision logic it dispatches (Apply & Start/Restart sequencing, lifecycle
//! actions) lives — and is unit-tested — in [`super::widgets`].

use std::time::SystemTime;

use crate::core::RecordingState;
use crate::diagnostics::DiagnosticSeverity;
use crate::display::{CharacterRendering, DisplayEncoding, DisplayMode, DisplayView, WrappingMode};

use super::bridge::{self, UiCommand};
use super::fonts::{bold, MonoFont};
use super::state::ChannelStatus;
use super::widgets::{
    edit_interface, human_bytes, latest_diagnostic, line_indicator, line_toggle, short_id,
    status_color, status_label, truncate, vsep, ColorScheme, DisplaySource, LifecycleAction,
    MSG_FONT_SIZES,
};
use super::ListenerApp;

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
                    v.config.recording.destination.clone(),
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
        // Recording indicator (§53): live state from the snapshot, destination from
        // the config. Off/None shows nothing — only Configure surfaces the setting.
        match recording {
            Some(RecordingState::Enabled) => {
                let dest = rec_dest
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(no destination)".to_string());
                ui.colored_label(
                    egui::Color32::from_rgb(200, 40, 40),
                    format!("\u{25CF} Recording \u{2192} {dest}"),
                );
            }
            Some(RecordingState::Faulted) => {
                ui.colored_label(
                    egui::Color32::from_rgb(170, 30, 30),
                    "\u{26A0} Recording faulted — see Diagnostics",
                );
            }
            Some(RecordingState::Disabled) | None => {}
        }
        // Lifecycle actions, below the stats line (#6); bigger so Stop/Remove stand out.
        ui.horizontal(|ui| {
            let size = egui::vec2(86.0, 30.0);
            // Primary action + label both derived from state (tested in widgets).
            let action = LifecycleAction::from_status(status);
            if ui
                .add_sized(size, egui::Button::new(action.label()))
                .clicked()
            {
                match action {
                    LifecycleAction::Stop => self.send(UiCommand::Stop(id)),
                    // Faulted can't go straight to Starting (§8.5): Retry = Stop + Start.
                    LifecycleAction::Retry => {
                        self.send(UiCommand::Stop(id));
                        self.try_start(id);
                    }
                    LifecycleAction::Start => self.try_start(id),
                }
            }
            if ui.add_sized(size, egui::Button::new("Remove")).clicked() {
                // Confirm first — removal is destructive and can't be undone (#1).
                self.confirm_remove = Some(id);
            }
        });
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
        let mut apply = false;
        let mut refresh = false;
        // "Restart" only when it's actually up; otherwise it's coming up = "Start".
        let apply_label = if status == ChannelStatus::Running {
            "Apply & Restart"
        } else {
            "Apply & Start"
        };
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
                    apply = ui.button(apply_label).clicked();
                });
        }
        self.force_config_open = false;
        if refresh {
            self.refresh_serial_ports();
        }
        if apply {
            self.apply_and_restart(id);
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
            // Display source (§41): the verbatim wire, or decoded Messages.
            ui.label(bold("Source"));
            ui.radio_value(&mut self.msg_source, DisplaySource::Stream, "Stream")
                .on_hover_text("Verbatim bytes as received (no Message boundaries, §18)");
            ui.radio_value(&mut self.msg_source, DisplaySource::Messages, "Messages")
                .on_hover_text("Extracted, numbered Messages");
            vsep(ui);
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
            // Per-message prefix — Messages source only (§18: Stream has no Message #).
            if self.msg_source == DisplaySource::Messages {
                vsep(ui);
                ui.label(bold("Add"));
                ui.checkbox(&mut self.show_msg_number, "msg #");
                ui.checkbox(&mut self.show_timestamp, "timestamp");
            }
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
        let msg_source = self.msg_source;
        let show_number = self.show_msg_number;
        let show_timestamp = self.show_timestamp;
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
                        .map(|m| {
                            (
                                m.message_number,
                                short_id(&m.rule_id.to_string()).to_string(),
                            )
                        })
                        .collect(),
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
            if !dv.matches.is_empty() {
                egui::CollapsingHeader::new(format!("Match firings ({})", dv.matches.len()))
                    .id_salt("matches")
                    .show(ui, |ui| {
                        for (number, rule) in &dv.matches {
                            let on = number
                                .map(|n| format!("#{n}"))
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
        // Shared renderer: same Hex/Rendered/Raw semantics for whichever source.
        let renderer = DisplayView {
            mode: msg_mode,
            encoding: DisplayEncoding::Utf8,
            character_rendering: msg_chars,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_line: 16,
        };
        match msg_source {
            DisplaySource::Stream => {
                let stream_bytes = self
                    .state
                    .channel(id)
                    .and_then(|v| v.snapshot.as_ref())
                    .map(|s| s.stream_tail.len())
                    .unwrap_or(0);
                ui.label(format!("Stream ({}):", human_bytes(stream_bytes as u64)));
                egui::Frame::new()
                    .fill(bg)
                    .inner_margin(4.0)
                    .show(ui, |ui| {
                        if stream_bytes == 0 {
                            let note = if has_snapshot {
                                "no data received yet"
                            } else {
                                "waiting for data — Start the channel"
                            };
                            ui.label(egui::RichText::new(note).weak());
                            return;
                        }
                        // Verbatim pre-extraction bytes (ADR-009, §18/§41): line breaks come
                        // only from the data — Rendered honors real CR/LF (§44), Raw shows
                        // control pictures, Hex is a byte run. The Label soft-wraps so the
                        // stream reflows; serial and UDP render identically (no reframing).
                        let text = match self.state.channel(id).and_then(|v| v.snapshot.as_ref()) {
                            None => String::new(),
                            Some(snapshot) => renderer.render_text(&snapshot.stream_tail),
                        };
                        egui::ScrollArea::vertical()
                            .id_salt("stream")
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(text)
                                            .font(egui::FontId::new(font_size, mono_family.clone()))
                                            .color(fg),
                                    )
                                    .wrap()
                                    .selectable(true),
                                );
                            });
                    });
            }
            DisplaySource::Messages => {
                let count = self
                    .state
                    .channel(id)
                    .and_then(|v| v.snapshot.as_ref())
                    .map(|s| {
                        s.display_views
                            .first()
                            .map(|v| v.messages.len())
                            .unwrap_or(s.retained.len())
                    })
                    .unwrap_or(0);
                ui.label(format!("Messages ({count}):"));
                egui::Frame::new()
                    .fill(bg)
                    .inner_margin(4.0)
                    .show(ui, |ui| {
                        if count == 0 {
                            let note = if has_snapshot {
                                "no messages — this channel isn't framed (set NMEA / a delimiter)"
                            } else {
                                "waiting for data — Start the channel"
                            };
                            ui.label(egui::RichText::new(note).weak());
                            return;
                        }
                        // Decoded Messages (§41). Compose rows up front — the display buffer
                        // is bounded (§88, ~1024) so this is cheap, and it lets us virtualize
                        // by *line*: a datagram can carry several CRLF-terminated sentences,
                        // so Rendered (§44) shows one row per sentence; Hex/Raw stay one
                        // non-wrapping row per message (long lines extend, horizontal scroll).
                        let lines: Vec<String> = match self
                            .state
                            .channel(id)
                            .and_then(|v| v.snapshot.as_ref())
                        {
                            None => Vec::new(),
                            Some(snapshot) => {
                                let messages = snapshot
                                    .display_views
                                    .first()
                                    .map(|v| v.messages.as_slice())
                                    .unwrap_or(snapshot.retained.as_slice());
                                let mut lines = Vec::with_capacity(messages.len());
                                for decoded in messages {
                                    let mut head = String::new();
                                    if show_timestamp {
                                        let dt: chrono::DateTime<chrono::Local> = decoded
                                            .message
                                            .metadata
                                            .arrival_timestamp
                                            .wall_clock
                                            .into();
                                        head.push_str(&format!("{} ", dt.format("%H:%M:%S%.3f")));
                                    }
                                    if show_number {
                                        head.push_str(&format!("#{} ", decoded.message.number));
                                    }
                                    let text = renderer.render_text(&decoded.message.bytes);
                                    match msg_mode {
                                        // Honor CRLF: one row per sentence. Drop a single
                                        // trailing newline so a CRLF-terminated datagram
                                        // doesn't add a blank row after it.
                                        DisplayMode::Rendered => {
                                            let body = text.strip_suffix('\n').unwrap_or(&text);
                                            let mut segs = body.split('\n');
                                            match segs.next() {
                                                None => lines.push(head),
                                                Some(first) => {
                                                    lines.push(format!("{head}{first}"));
                                                    // Continuation sentences carry no prefix.
                                                    for seg in segs {
                                                        lines.push(seg.to_string());
                                                    }
                                                }
                                            }
                                        }
                                        // Hex / Raw: one non-wrapping row per message.
                                        _ => {
                                            let body = text.replace(['\n', '\r'], " ");
                                            lines.push(format!("{head}{body}"));
                                        }
                                    }
                                }
                                lines
                            }
                        };
                        let row_h = ui.fonts_mut(|f| {
                            f.row_height(&egui::FontId::new(font_size, mono_family.clone()))
                        });
                        egui::ScrollArea::both()
                            .id_salt("messages")
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .show_rows(ui, row_h, lines.len(), |ui, range| {
                                let end = range.end.min(lines.len());
                                let start = range.start.min(end);
                                for line in &lines[start..end] {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(line.clone())
                                                .font(egui::FontId::new(
                                                    font_size,
                                                    mono_family.clone(),
                                                ))
                                                .color(fg),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                    );
                                }
                            });
                    });
            }
        }
    }
}
