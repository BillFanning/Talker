//! The channel-list panel (master side of the master–detail layout) and its
//! collapsed mini-strip. Rows are snapshotted before rendering so the list
//! isn't borrowing the app state while a click mutates the selection.

use wiredata_ui::palette::{Palette, DARK, LIGHT};

use super::draft::ConnKind;
use super::widgets::interface_summary;
use super::TalkerApp;

/// One channel's row data, snapshotted before rendering.
struct ChannelRow {
    /// Display name, already resolved to the positional fallback.
    name: String,
    /// One-line interface summary (may contain `?` for unfilled fields).
    summary: String,
    running: bool,
    /// The channel's current error, if any — shown on the row so a fault
    /// on a non-selected channel is visible without opening it.
    error: Option<String>,
    sent: u64,
    /// Messages per second over the last sampling window (0 when idle).
    per_sec: f32,
    /// Log events attributed to this channel since its last start.
    info: u32,
    warnings: u32,
    errors: u32,
}

impl TalkerApp {
    /// Resolve channel `i`'s display name: the user's name, or the
    /// positional fallback.
    pub(super) fn channel_name(&self, i: usize) -> String {
        match self.conn_drafts.get(i) {
            Some(d) if !d.name.is_empty() => d.name.clone(),
            _ => format!("Channel {}", i + 1),
        }
    }

    pub(super) fn show_channel_list(&mut self, ui: &mut egui::Ui) {
        let pal = theme_palette(ui);

        // Header: collapse, title, + Add menu.
        ui.horizontal(|ui| {
            if ui
                .button("\u{25C0}")
                .on_hover_text("Collapse channel list")
                .clicked()
            {
                self.channels_collapsed = true;
            }
            ui.heading("Channels");
            ui.menu_button("+ Add", |ui| {
                for (kind, label) in [
                    (ConnKind::Serial, "Serial"),
                    (ConnKind::Udp, "UDP"),
                    (ConnKind::Tcp, "TCP"),
                ] {
                    if ui.button(label).clicked() {
                        self.deferred.add_channel = Some(kind);
                        ui.close();
                    }
                }
            });
        });
        // Bulk actions on their own row so the header doesn't crowd.
        ui.horizontal(|ui| {
            let start_all = ui.add_enabled(
                self.can_start_any(),
                egui::Button::new("\u{25b6} Start all"),
            );
            if start_all.clicked() {
                self.deferred.start_all = true;
            }
            if !self.can_start_any() {
                start_all.on_disabled_hover_text("Add at least one valid channel and one message");
            }
            if self.is_any_running() && ui.button("\u{25a0} Stop all").clicked() {
                self.deferred.stop_all = true;
            }
        });
        ui.separator();

        // Snapshot the rows, then render — the click handler mutates
        // `selected` (via deferred), which must not alias the borrow.
        let rows: Vec<ChannelRow> = (0..self.conn_drafts.len())
            .map(|i| ChannelRow {
                name: self.channel_name(i),
                summary: interface_summary(&self.conn_drafts[i]),
                running: self.is_connection_running(i),
                error: self.conn_errors.get(i).and_then(|e| e.clone()),
                sent: self.sent_counts.get(i).copied().unwrap_or(0),
                per_sec: self.rates.get(i).map(|r| r.per_sec).unwrap_or(0.0),
                info: self.log_counts.get(i).map(|c| c.info).unwrap_or(0),
                warnings: self.log_counts.get(i).map(|c| c.warn).unwrap_or(0),
                errors: self.log_counts.get(i).map(|c| c.error).unwrap_or(0),
            })
            .collect();

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, row) in rows.iter().enumerate() {
                self.show_channel_row(ui, i, row, pal);
                ui.add_space(2.0);
            }
            if rows.is_empty() {
                ui.weak("No channels yet — “+ Add” above.");
            }
        });
    }

    fn show_channel_row(&mut self, ui: &mut egui::Ui, i: usize, row: &ChannelRow, pal: &Palette) {
        let selected = self.selected == Some(i);
        let mut frame = egui::Frame::group(ui.style());
        frame.stroke = egui::Stroke::new(if selected { 1.5 } else { 1.0 }, pal.box_stroke);
        if selected {
            frame.fill = ui.visuals().widgets.active.weak_bg_fill;
        }
        let resp = ui
            .push_id(i, |ui| {
                frame
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (glyph, color, tip) = status_glyph(row, pal);
                            ui.colored_label(color, glyph).on_hover_text(tip);
                            ui.label(wiredata_ui::fonts::bold(&row.name));
                            // Per-channel log tallies (since the channel's
                            // last start), right-aligned like listener's tab
                            // counts. Only non-zero severities are shown.
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let tip = "Log events attributed to this channel \
                                               since its last start";
                                    if row.errors > 0 {
                                        ui.label(
                                            egui::RichText::new(format!("{} err", row.errors))
                                                .color(pal.fault_red)
                                                .size(11.0),
                                        )
                                        .on_hover_text(tip);
                                    }
                                    if row.warnings > 0 {
                                        ui.label(
                                            egui::RichText::new(format!("{} warn", row.warnings))
                                                .color(pal.warning_amber)
                                                .size(11.0),
                                        )
                                        .on_hover_text(tip);
                                    }
                                    if row.info > 0 {
                                        ui.label(
                                            egui::RichText::new(format!("{} info", row.info))
                                                .color(pal.count_info_grey)
                                                .size(11.0),
                                        )
                                        .on_hover_text(tip);
                                    }
                                },
                            );
                        });
                        ui.weak(&row.summary);
                        if row.running {
                            let rate = if row.per_sec > 0.05 {
                                format!(" \u{00B7} {:.1} msg/s", row.per_sec)
                            } else {
                                String::new()
                            };
                            ui.label(
                                egui::RichText::new(format!("Sent: {}{rate}", row.sent))
                                    .color(pal.running_green)
                                    .size(12.0),
                            );
                        }
                        if let Some(err) = &row.error {
                            // One-line, truncated; the full text is on the
                            // status glyph's hover and in the detail pane.
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(err).color(pal.fault_red).size(11.0),
                                )
                                .truncate(),
                            );
                        }
                    })
                    .response
            })
            .inner;
        if resp.interact(egui::Sense::click()).clicked() {
            self.deferred.select = Some(i);
        }
    }

    /// The collapsed variant: a thin strip — an expand button plus one
    /// status dot per channel (click to select, name on hover).
    pub(super) fn show_channel_strip(&mut self, ui: &mut egui::Ui) {
        let pal = theme_palette(ui);
        if ui
            .button("\u{25B6}")
            .on_hover_text("Show channels")
            .clicked()
        {
            self.channels_collapsed = false;
        }
        ui.separator();
        for i in 0..self.conn_drafts.len() {
            let running = self.is_connection_running(i);
            let error = self.conn_errors.get(i).is_some_and(|e| e.is_some());
            let color = if running && error {
                pal.fault_red
            } else if running {
                pal.running_green
            } else {
                pal.idle_grey
            };
            let glyph = if running { "\u{25CF}" } else { "\u{25CB}" };
            let selected = self.selected == Some(i);
            let text = egui::RichText::new(glyph).color(color).size(16.0);
            if ui
                .selectable_label(selected, text)
                .on_hover_text(self.channel_name(i))
                .clicked()
            {
                self.deferred.select = Some(i);
            }
        }
    }
}

/// The status glyph for a full-width row: (glyph, color, hover tip).
fn status_glyph<'a>(row: &'a ChannelRow, pal: &Palette) -> (&'static str, egui::Color32, &'a str) {
    if row.running {
        if let Some(err) = &row.error {
            ("\u{25CF}", pal.fault_red, err.as_str())
        } else {
            ("\u{25CF}", pal.running_green, "running")
        }
    } else if let Some(err) = &row.error {
        ("\u{25CB}", pal.fault_red, err.as_str())
    } else {
        ("\u{25CB}", pal.idle_grey, "stopped")
    }
}

/// The palette matching the active theme.
fn theme_palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}
