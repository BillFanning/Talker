//! The channel-list panel (master side of the master–detail layout) and its
//! collapsed mini-strip. Rows are snapshotted before rendering so the list
//! isn't borrowing the app state while a click mutates the selection.

use wiredata_ui::{glyphs, palette::Palette};

use super::draft::ConnKind;
use super::widgets::{interface_summary, lifecycle_indicator, theme_palette};
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

        // Header: collapse, title, + Add and Profile menus (listener's header
        // shape — the Profile menu sits next to "+ Add").
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
            self.show_profile_menu(ui);
        });
        // Bulk actions on their own row so the header doesn't crowd. Plain
        // labels and an always-present Stop all, as in listener (stopping
        // with nothing running is a no-op).
        ui.horizontal(|ui| {
            let start_all = ui.add_enabled(self.can_start_any(), egui::Button::new("Start all"));
            if start_all.clicked() {
                self.deferred.start_all = true;
            }
            if !self.can_start_any() {
                start_all.on_disabled_hover_text("Add at least one valid channel and one message");
            }
            if ui.button("Stop all").clicked() {
                self.deferred.stop_all = true;
            }
        });
        // The current profile + dirty marker (listener's status line under
        // its header). Renaming happens via Save As…, as in listener.
        let name = if self.profile.name.is_empty() {
            "(unsaved profile)"
        } else {
            self.profile.name.as_str()
        };
        let marker = if self.dirty { " *" } else { "" };
        ui.label(egui::RichText::new(format!("{name}{marker}")).weak())
            .on_hover_text(
                "The loaded profile — * means unsaved changes (Ctrl+S saves). \
                 Rename by saving to a new file via Profile → Save As….",
            );
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
                ui.add_space(6.0);
            }
            if rows.is_empty() {
                ui.weak("No channels yet — “+ Add” above.");
            }
        });
    }

    /// The Profile menu (listener's structure): the Recent section, then
    /// Save / Save As… / Load…, plus talker's New at the bottom. Lives in the
    /// channel-list header next to "+ Add", as in listener.
    fn show_profile_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Profile", |ui| {
            // Recent profiles at the top: one click reloads. Most-recent-
            // first. The header always shows (with a placeholder when empty)
            // so the section is visibly present.
            ui.label(egui::RichText::new("Recent").weak());
            if self.recent_profiles.is_empty() {
                ui.add_enabled(false, egui::Button::new("(none yet)"));
            } else {
                let recents = self.recent_profiles.clone();
                for path in recents {
                    let label = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_else(|| path.to_str().unwrap_or("profile"));
                    if ui
                        .button(label)
                        .on_hover_text(path.display().to_string())
                        .clicked()
                    {
                        if self.confirm_discard() {
                            self.error_count = 0;
                            self.load_profile_from_path(&path);
                        }
                        ui.close();
                    }
                }
            }
            ui.separator();
            if ui.button("Save").on_hover_text("Ctrl+S").clicked() {
                self.save_profile();
                ui.close();
            }
            if ui
                .button("Save As\u{2026}")
                .on_hover_text("Ctrl+Shift+S — write to a new file")
                .clicked()
            {
                self.save_profile_as();
                ui.close();
            }
            if ui.button("Load\u{2026}").on_hover_text("Ctrl+O").clicked() {
                self.load_profile_dialog();
                ui.close();
            }
            ui.separator();
            if ui.button("New").on_hover_text("Ctrl+N").clicked() {
                self.new_profile();
                ui.close();
            }
        });
    }

    fn show_channel_row(&mut self, ui: &mut egui::Ui, i: usize, row: &ChannelRow, pal: &Palette) {
        let selected = self.selected == Some(i);
        // Listener's row chrome: rounded, padded, box_stroke border; the
        // selected row uses the theme's selection fill and stroke.
        let mut frame = egui::Frame::group(ui.style())
            .inner_margin(8.0)
            .corner_radius(egui::CornerRadius::same(6))
            .stroke(egui::Stroke::new(1.5_f32, pal.box_stroke));
        if selected {
            frame.fill = ui.visuals().selection.bg_fill;
            frame.stroke = egui::Stroke::new(1.5_f32, ui.visuals().selection.stroke.color);
        }
        let (box_resp, remove_clicked) = ui
            .push_id(i, |ui| {
                let inner = frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // Line 1: status glyph + name, as one LayoutJob so the
                    // enlarged glyph centers on the text line instead of
                    // stretching the row (listener's line-1 pattern; same
                    // shared symbol set as the detail header).
                    let base = egui::TextStyle::Body.resolve(ui.style()).size;
                    let (glyph, color, word) =
                        lifecycle_indicator(row.running, row.error.is_some(), pal);
                    let mut line1 = egui::text::LayoutJob::default();
                    line1.append(
                        glyph,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(base * glyphs::glyph_size(glyph)),
                            color,
                            valign: egui::Align::Center,
                            ..Default::default()
                        },
                    );
                    line1.append(
                        &format!("  {}", row.name),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(base * 1.1),
                            color: ui.visuals().text_color(),
                            valign: egui::Align::Center,
                            ..Default::default()
                        },
                    );
                    ui.label(line1)
                        .on_hover_text(row.error.as_deref().unwrap_or(word));
                    // Line 2: connection details.
                    ui.weak(&row.summary);
                    // Line 3: live stats (talker's message-count view),
                    // weak like listener's stats line.
                    if row.running {
                        let rate = if row.per_sec > 0.05 {
                            format!(" \u{00B7} {:.1} msg/s", row.per_sec)
                        } else {
                            String::new()
                        };
                        ui.label(egui::RichText::new(format!("Sent: {}{rate}", row.sent)).weak());
                    }
                    // Line 4: per-severity log counts (since the channel's
                    // last start) — listener's row line: always shown, in
                    // info · warn · err order with the shared colors.
                    ui.horizontal(|ui| {
                        let tip = "Log events attributed to this channel \
                                       since its last start";
                        ui.label(
                            egui::RichText::new(format!("{} info", row.info))
                                .weak()
                                .color(pal.count_info_grey),
                        )
                        .on_hover_text(tip);
                        ui.label(
                            egui::RichText::new(format!("{} warn", row.warnings))
                                .color(pal.warning_amber),
                        )
                        .on_hover_text(tip);
                        ui.label(
                            egui::RichText::new(format!("{} err", row.errors)).color(pal.fault_red),
                        )
                        .on_hover_text(tip);
                    });
                    // Line 5: the last error, wrapped like listener's so
                    // the full text reads on the row itself.
                    if let Some(err) = &row.error {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("\u{26A0} {err}"))
                                    .small()
                                    .color(pal.fault_red),
                            )
                            .wrap(),
                        );
                    }
                });
                // Box-select: sense a click on the whole box first.
                let box_resp = inner.response.interact(egui::Sense::click());
                // The ✕ remove button is placed ON TOP, in the box's
                // top-right corner, AFTER the box interaction — so it's last
                // in z-order and wins the click instead of the box stealing
                // it (listener's row pattern).
                let btn_size = egui::vec2(20.0, 20.0);
                let btn_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        box_resp.rect.right() - btn_size.x - 6.0,
                        box_resp.rect.top() + 6.0,
                    ),
                    btn_size,
                );
                let remove_clicked = ui
                    .put(btn_rect, egui::Button::new("\u{2715}").small())
                    .on_hover_text("Remove channel")
                    .clicked();
                (box_resp, remove_clicked)
            })
            .inner;
        // Removal confirms through the shared modal; a box click that
        // wasn't the ✕ selects the channel.
        if remove_clicked {
            self.confirm_remove = Some(i);
        } else if box_resp.clicked() {
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
        let base = egui::TextStyle::Body.resolve(ui.style()).size;
        for i in 0..self.conn_drafts.len() {
            let running = self.is_connection_running(i);
            let error = self.conn_errors.get(i).is_some_and(|e| e.is_some());
            let (glyph, color, _) = lifecycle_indicator(running, error, pal);
            let selected = self.selected == Some(i);
            let text = egui::RichText::new(glyph)
                .color(color)
                .size(base * glyphs::glyph_size(glyph));
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
