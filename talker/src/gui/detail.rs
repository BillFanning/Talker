//! The detail pane: everything about the **selected** channel — header
//! (name, kind, summary, actions), the Connection editor, the Messages
//! editor, and the Output display pane. One channel on screen at a time;
//! the channel list in [`super::channels`] picks which.

use egui::{Align, Layout};

use crate::core::message::NmeaChecksumMode;
use crate::core::runner::TalkerCommand;

use super::draft::{ConnKind, PayloadKind, ScheduleDraft};
use super::widgets::{
    checksum_label, code_page_label, hex_valid, invalid_parse, marker_aware_text_edit,
    plain_text_edit_with_cursor, preview_ascii, preview_text, red_bordered, show_display_pane,
    show_insert_byte_button, show_insert_unit_button, show_interface_summary, show_serial_fields,
    show_tcp_fields, show_udp_fields, start_blockers, UppercaseHex,
};
use super::TalkerApp;

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
                ui.horizontal(|ui| {
                    self.show_channel_header(ui, i, running);
                });
                ui.separator();
                self.show_channel_body(ui, i, running);
                ui.separator();
                show_display_pane(ui, &mut self.displays[i]);
            });
        });
    }

    fn show_channel_header(&mut self, ui: &mut egui::Ui, i: usize, running: bool) {
        let error = self.conn_errors.get(i).and_then(|e| e.as_deref());
        let (dot_color, dot_tip): (egui::Color32, &str) = if !running {
            (egui::Color32::GRAY, "not running")
        } else if let Some(msg) = error {
            (egui::Color32::RED, msg)
        } else {
            (egui::Color32::from_rgb(80, 200, 80), "ok")
        };
        ui.colored_label(dot_color, "\u{2022}")
            .on_hover_text(dot_tip);

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
                        .color(egui::Color32::from_rgb(220, 180, 80))
                        .size(11.0),
                )
                .on_hover_text("Another channel has the same name — allowed, but confusing.");
            }
        });

        ui.separator();
        let before_kind = self.conn_drafts[i].kind;
        ui.radio_value(&mut self.conn_drafts[i].kind, ConnKind::Serial, "Serial");
        ui.radio_value(&mut self.conn_drafts[i].kind, ConnKind::Udp, "UDP");
        ui.radio_value(&mut self.conn_drafts[i].kind, ConnKind::Tcp, "TCP");
        if self.conn_drafts[i].kind != before_kind {
            self.deferred.apply.push(i);
        }
        ui.separator();
        // Stable id sources on every conditional widget that follows, so
        // the appearing / disappearing pending indicator can't shift the
        // sibling auto-ids between egui's two layout passes.
        ui.push_id("iface_summary", |ui| {
            show_interface_summary(ui, &self.conn_drafts[i]);
        });
        let (iface_drift, msg_drift) = self.detect_drift(i);
        ui.push_id("pending_indicator", |ui| {
            show_unapplied_badge(ui, running, iface_drift, msg_drift);
        });
        ui.push_id("channel_actions", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.show_channel_actions(ui, i, running, iface_drift, msg_drift);
            });
        });
    }

    /// Right-side action button cluster: × remove, then either ■ Stop (+ ↻
    /// Restart when drift exists) when running or ▶ Start when not. Called
    /// from inside a `right_to_left` layout, so the first widget rendered
    /// here appears rightmost.
    fn show_channel_actions(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        running: bool,
        iface_drift: bool,
        msg_drift: bool,
    ) {
        // Channel removal confirms through a modal (listener's pattern) —
        // it can't be missed, and Escape / backdrop-click cancels. The
        // per-message ✕ keeps its lighter inline confirm.
        if ui
            .button(egui::RichText::new("\u{00D7}").size(18.0).strong())
            .on_hover_text("Remove this channel")
            .clicked()
        {
            self.confirm_remove = Some(i);
        }
        if running {
            if ui.small_button("\u{25a0}").on_hover_text("Stop").clicked() {
                self.deferred.stop = Some(i);
            }
            // ↻ Restart, shown only when there's drift to apply.
            // start_connection internally calls stop_connection first, so
            // deferring `start` here gives us a clean stop+apply+start.
            if iface_drift || msg_drift {
                let restart = ui.add(
                    egui::Button::new(
                        egui::RichText::new("\u{21BA}")
                            .color(egui::Color32::from_rgb(220, 180, 60))
                            .strong()
                            .size(16.0),
                    )
                    .small(),
                );
                if restart
                    .on_hover_text(
                        "Restart channel — stops the current send loop, applies \
                         the current draft (interface + messages), and starts again.",
                    )
                    .clicked()
                {
                    self.deferred.start = Some(i);
                }
            }
        } else {
            let can = self.can_start_connection(i);
            // Compute the disabled-hover tip *before* the widget is added,
            // so we can chain `on_disabled_hover_text` directly on the
            // Response — egui only displays the tooltip when the call is
            // part of the same Response chain as the widget add.
            let tip = if !can {
                let t = start_blockers(&self.conn_drafts[i], &self.sched_drafts[i]).join("\n");
                if t.is_empty() {
                    "Add a valid message first".to_string()
                } else {
                    t
                }
            } else {
                String::new()
            };
            let mut btn = ui.add_enabled(can, egui::Button::new("\u{25b6}").small());
            if !can {
                btn = btn.on_disabled_hover_text(tip);
            }
            if btn.clicked() {
                self.deferred.start = Some(i);
            }
        }
    }

    fn show_channel_body(&mut self, ui: &mut egui::Ui, i: usize, running: bool) {
        // Connection stays a plain collapsing section — it does NOT
        // auto-collapse on run (you often want the interface params
        // visible while a channel is live). Default open; the user's
        // expand/collapse choice persists via the stable id_salt.
        let (changed, refresh) = egui::CollapsingHeader::new("Connection")
            .id_salt(("conn_section", i))
            .default_open(true)
            .show(ui, |ui| {
                match self.conn_drafts[i].kind {
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
                }
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
        let per_message_counts: &[u64] = self
            .message_sent_counts
            .get(i)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let status_drops = self.status_drops.get(i).copied().unwrap_or(0);
        let interval_changes = show_schedule_section(
            ui,
            &mut self.sched_drafts[i],
            &mut self.dirty,
            per_message_counts,
            running,
            status_drops,
        );
        for (msg_index, interval_ms) in interval_changes {
            if let Some(Some(handle)) = self.talkers.get(i) {
                let _ = handle.cmd_tx.try_send(TalkerCommand::SetInterval {
                    index: msg_index,
                    interval_ms,
                });
            }
        }
    }
}

// ── Inline message editor ─────────────────────────────────────────────────────

fn show_schedule_section(
    ui: &mut egui::Ui,
    entries: &mut Vec<ScheduleDraft>,
    dirty: &mut bool,
    per_message_counts: &[u64],
    channel_running: bool,
    status_drops: u64,
) -> Vec<(usize, u64)> {
    let mut to_remove: Option<usize> = None;
    let mut add_one = false;
    // Message indices whose interval was committed this frame, with the new value.
    let mut interval_changes: Vec<(usize, u64)> = Vec::new();

    // `id_salt` keeps the persistent open/closed state stable even
    // though the title text changes every frame as `total_sent`
    // ticks. Without it CollapsingHeader derives its id from the
    // label, so a user expand would be forgotten on the next send.
    // (The old stacked-card layout auto-collapsed this section on
    // Start; in the detail pane there's room, so the section just
    // honours whatever the user last chose.)
    let total_sent: u64 = per_message_counts.iter().sum();
    let n = entries.len();
    let header = if n == 0 {
        "Messages — (none)".to_string()
    } else if channel_running {
        let drops = if status_drops > 0 {
            // The runner sampled display updates because the GUI's status
            // queue was full — counts stay correct (they ride in each
            // status), but the Output pane missed some payloads.
            format!(" ({status_drops} display updates dropped)")
        } else {
            String::new()
        };
        format!(
            "Messages — {n} message{}, Sent: {total_sent}{drops}",
            if n == 1 { "" } else { "s" }
        )
    } else {
        format!("Messages — {n} message{}", if n == 1 { "" } else { "s" })
    };
    egui::CollapsingHeader::new(header)
        .id_salt("messages_section")
        .default_open(true)
        .show(ui, |ui| {
            for (i, entry) in entries.iter_mut().enumerate() {
                ui.push_id(i, |ui| {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(format!("Message {}", i + 1));
                            ui.separator();
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Nmea, "NMEA");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Ascii, "ASCII");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Utf8, "UTF-8");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Utf16, "UTF-16");
                            ui.radio_value(&mut entry.payload_kind, PayloadKind::Hex, "Hex");
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
                                show_payload_fields(ui, entry);

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
                                ui.end_row();
                                if interval_resp.lost_focus() {
                                    if let Ok(ms) = entry.interval_ms.parse::<u64>() {
                                        interval_changes.push((i, ms));
                                    }
                                }
                            });

                        ui.horizontal(|ui| {
                            show_timestamp_editor(ui, entry);
                            ui.separator();
                            show_checksum_editor(ui, entry);
                        });

                        show_message_preview(ui, entry);

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
        *dirty = true;
    }
    if add_one {
        entries.push(ScheduleDraft::default());
        *dirty = true;
    }

    interval_changes
}

/// Render the payload-format fields for one message into the surrounding grid.
/// Each `PayloadKind` arm has its own renderer below.
fn show_payload_fields(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    match entry.payload_kind {
        PayloadKind::Hex => show_hex_payload(ui, entry),
        PayloadKind::Utf8 => show_utf8_payload(ui, entry),
        PayloadKind::Utf16 => show_utf16_payload(ui, entry),
        PayloadKind::Ascii => show_ascii_payload(ui, entry),
        PayloadKind::Nmea => show_nmea_payload(ui, entry),
    }
}

fn show_hex_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    let bad_hex = !entry.hex_data.is_empty() && !hex_valid(&entry.hex_data);
    ui.label("Data (hex)");
    let _ = red_bordered(
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
}

fn show_utf8_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    ui.label("Text");
    ui.horizontal(|ui| {
        marker_aware_text_edit(
            ui,
            &mut entry.utf8_text,
            "payload_utf8",
            300.0,
            "Unicode text",
        );
        show_insert_byte_button(
            ui,
            &mut entry.utf8_text,
            &mut entry.insert_byte_hex,
            "payload_utf8",
        );
    });
    ui.end_row();
}

fn show_utf16_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    ui.label("Text");
    ui.horizontal(|ui| {
        // Two editor modes, chosen by `Allow raw bytes`:
        //   off — plain Unicode editor (what you see is what gets
        //         encoded). Insert Code Unit inserts the decoded
        //         glyph (4 hex → one char).
        //   on  — marker-aware editor + Insert Byte button. Insert
        //         Code Unit inserts marker pairs with byte order
        //         applied.
        if entry.utf16_allow_raw_bytes {
            marker_aware_text_edit(
                ui,
                &mut entry.utf16_text,
                "payload_utf16",
                300.0,
                "Unicode text",
            );
            show_insert_byte_button(
                ui,
                &mut entry.utf16_text,
                &mut entry.insert_byte_hex,
                "payload_utf16",
            );
        } else {
            plain_text_edit_with_cursor(
                ui,
                &mut entry.utf16_text,
                "payload_utf16",
                300.0,
                "Unicode text",
            );
        }
        show_insert_unit_button(
            ui,
            &mut entry.utf16_text,
            &mut entry.insert_byte_hex,
            "payload_utf16",
            entry.utf16_big_endian,
            entry.utf16_allow_raw_bytes,
        );
    });
    ui.end_row();
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
}

fn show_ascii_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    ui.label("Text");
    ui.horizontal(|ui| {
        marker_aware_text_edit(ui, &mut entry.ascii_text, "payload_ascii", 300.0, "text");
        show_insert_byte_button(
            ui,
            &mut entry.ascii_text,
            &mut entry.insert_byte_hex,
            "payload_ascii",
        );
    });
    ui.end_row();
    ui.label("Code page");
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
    ui.end_row();
}

fn show_nmea_payload(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
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
        }
        ui.menu_button("v", |ui| {
            show_filtered_picker(
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
        }
        let sentence_before = entry.nmea_sentence_type.clone();
        ui.menu_button("v", |ui| {
            show_filtered_picker(
                ui,
                "filter by code or description",
                &mut entry.nmea_sentence_filter,
                nmea0183::sentence_type::ALL_WITH_DESC,
                &mut entry.nmea_sentence_type,
            );
        });
        if entry.nmea_sentence_type != sentence_before {
            prefill_nmea_fields(entry);
        }
        ui.separator();
        ui.label("NMEA checksum:").on_hover_text(
            "The protocol-internal `*XX` byte at the end of an NMEA \
             sentence. Distinct from the `Message checksum` row below, \
             which is an outer checksum wrapped around the complete \
             rendered message (timestamp + payload + NMEA `*XX`).",
        );
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
    }
    ui.end_row();
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
) {
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
                ui.close();
            }
            for (code, desc) in options {
                let matches = needle.is_empty()
                    || code.to_ascii_lowercase().contains(&needle)
                    || desc.to_ascii_lowercase().contains(&needle);
                if matches && ui.button(format!("{code}  —  {desc}")).clicked() {
                    *selected = (*code).to_string();
                    filter.clear();
                    ui.close();
                }
            }
        });
}

/// Render the read-only "this is what would be sent" preview row.
///
/// Compiles the draft each frame and renders the wire bytes with a fixed
/// reference timestamp — never `chrono::Utc::now()` — so the value does
/// not change between repaints (which can be triggered by mouse motion,
/// not just edits). The actual send still uses the wall clock; this
/// preview shows the format and structure, not a live tick.
///
/// Bytes are shown as text (lossy UTF-8) for payload types that are text
/// at heart (Utf8 / Ascii / NMEA) and as space-separated hex for the
/// binary types (Hex / Utf16), to avoid the U+FFFD-tofu we'd otherwise
/// get for non-UTF-8 bytes.
fn show_message_preview(ui: &mut egui::Ui, entry: &ScheduleDraft) {
    // 2024-01-01T12:00:00.000Z — a fixed, recognisable sample instant.
    let reference = chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_110_400, 0).unwrap();
    ui.horizontal(|ui| {
        ui.label("Wire bytes:").on_hover_text(
            "Literal bytes that would be sent on the wire, rendered \
                 in a payload-appropriate view. Timestamps use a fixed \
                 reference instant so the value doesn't tick — the \
                 actual send uses the wall clock.",
        );
        let text = match entry.to_message_config().and_then(|m| m.compile().ok()) {
            Some(compiled) => {
                let bytes = compiled.render_at(reference);
                match entry.payload_kind {
                    // ASCII previews through the message's code page,
                    // so the user sees what a receiver decoding via
                    // the same code page would render: byte `0xE9` is
                    // `é` in ISO-8859-1, `Θ` in CP437, `È` in Mac
                    // Roman, etc.
                    PayloadKind::Ascii => preview_ascii(&bytes, entry.ascii_code_page),
                    PayloadKind::Utf8 | PayloadKind::Nmea => preview_text(&bytes),
                    PayloadKind::Hex | PayloadKind::Utf16 => bytes
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                }
            }
            None => "(message is incomplete)".to_string(),
        };
        ui.label(egui::RichText::new(text).monospace());
    });
}

/// Per-message status line at the bottom of each message group:
/// a coloured state dot plus the message's running send count.
///
/// State follows the channel — messages aren't independently scheduled
/// from the user's perspective. "Active" = channel is running and this
/// message will fire on its interval. "Idle" = channel is stopped, so
/// the count is the last value seen.
fn show_message_status(ui: &mut egui::Ui, channel_running: bool, sent: u64) {
    // Footer bar: separator above to split it from the message body, then
    // a tinted Frame so the "Active / Sent: N" line reads as a status
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
                    egui::RichText::new(format!("Sent: {sent}"))
                        .strong()
                        .monospace(),
                );
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
fn show_timestamp_editor(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut entry.timestamp_enabled, "Timestamp");
        if entry.timestamp_enabled {
            ui.checkbox(&mut entry.ts_date, "Date");
            ui.checkbox(&mut entry.ts_millis, "Milliseconds");
            ui.checkbox(&mut entry.ts_timezone, "Z (UTC)");
        }
    });
}

/// Render the per-message checksum controls. See [`show_timestamp_editor`]
/// for the separator hierarchy rationale.
fn show_checksum_editor(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    use crate::core::message::ChecksumAlgorithm;
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
}

/// Amber pill in the detail header when the user has edited fields
/// that haven't reached the running talker thread yet. Hides itself when
/// there's no drift (or when the channel isn't running and the user is
/// just composing — they'll apply by pressing Start anyway).
///
/// - **Interface drift** can be applied live (press Enter in the edited
///   field; the talker thread reopens the interface).
/// - **Message drift** currently needs a stop+start — the schedule is
///   compiled at channel open and can't be hot-swapped today.
fn show_unapplied_badge(ui: &mut egui::Ui, running: bool, iface_drift: bool, msg_drift: bool) {
    if !running || !(iface_drift || msg_drift) {
        return;
    }
    let (label, tip) = match (iface_drift, msg_drift) {
        (true, true) => (
            "RESTART NEEDED",
            "Both interface parameters and the message list have been edited.\n\
             Press Stop then Start to apply both — message changes can't be \
             applied without a restart.",
        ),
        (false, true) => (
            "RESTART NEEDED",
            "Message edits (payload, interval rules, timestamp/checksum toggles, \
             added/removed messages) only take effect when the channel restarts. \
             Press Stop then Start.",
        ),
        (true, false) => (
            "APPLY NEEDED",
            "Interface parameters have been edited but not yet applied to the \
             running channel. Press Enter in the edited field — or stop and \
             start the channel — to apply them.",
        ),
        (false, false) => unreachable!(),
    };
    let bg = egui::Color32::from_rgb(220, 180, 60);
    let fg = egui::Color32::BLACK;
    egui::Frame::default()
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).color(fg).strong().size(12.0));
        })
        .response
        .on_hover_text(tip);
}
