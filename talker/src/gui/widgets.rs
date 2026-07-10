//! Shared field renderers and small pure helpers for the talker GUI, split
//! out of `mod.rs` in the master–detail restructure (spec §3.2). Everything
//! here is either a stateless `fn(ui, …)` widget or a pure validation /
//! formatting helper — app state stays in [`super::TalkerApp`].

use std::net::{Ipv4Addr, SocketAddr};

use egui::{Align, Layout};

use crate::core::message::{
    decode_codepage_byte, repair_after_edit, segments, ChecksumAlgorithm, CodePage, Segment,
};

use super::display::{ChannelDisplay, ControlStyle, DisplayMode};
use super::draft::{ConnDraft, ConnKind, PayloadKind, PortHold, ScheduleDraft, UdpModeDraft};

// ── Interface summary / start blockers ────────────────────────────────────────

/// Render [`interface_summary`] in a channel header, splitting on `?`
/// markers so unknown / unfilled fields show up as a **bold red** glyph
/// rather than blending into the rest of the weak-grey summary text.
pub(super) fn show_interface_summary(ui: &mut egui::Ui, conn: &ConnDraft) {
    let text = interface_summary(conn);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let mut buf = String::new();
        let flush = |ui: &mut egui::Ui, buf: &mut String| {
            if !buf.is_empty() {
                ui.weak(std::mem::take(buf));
            }
        };
        // Render the ? as inline text with a red background — RichText
        // sits on the same text baseline as the surrounding weak-grey
        // labels, so the badge looks the same size and alignment in
        // every channel (a Frame-wrapped version offsets vertically
        // and reads as a different chrome than the rest of the line).
        // Padded with thin spaces so the background extends past the
        // glyph instead of clinging to it.
        let red_bg = egui::Color32::from_rgb(200, 50, 50);
        for c in text.chars() {
            if c == '?' {
                flush(ui, &mut buf);
                ui.label(
                    egui::RichText::new("\u{2009}?\u{2009}")
                        .color(egui::Color32::WHITE)
                        .strong()
                        .background_color(red_bg),
                );
            } else {
                buf.push(c);
            }
        }
        flush(ui, &mut buf);
    });
}

/// One-line, human-readable summary of a channel's selected interface and
/// its parameters, shown in the channel list and detail header so the active
/// config is visible at a glance without expanding the editor. Uses the
/// draft's current strings — invalid or missing parts show as `?`.
pub(super) fn interface_summary(conn: &ConnDraft) -> String {
    /// Show `s` if it's non-empty AND `valid(s)` is true; otherwise
    /// the `?` placeholder — which [`show_interface_summary`] paints
    /// as a red pill. Drives the at-a-glance "this channel can't
    /// start yet" cue for both missing AND malformed values.
    fn or_q(s: &str, valid: impl Fn(&str) -> bool) -> &str {
        if s.is_empty() || !valid(s) {
            "?"
        } else {
            s
        }
    }
    // Validators line up with `channel_blockers` so the summary's `?`s
    // match the disabled-Start tooltip exactly.
    let ok_ipv4 = |s: &str| s.parse::<Ipv4Addr>().is_ok();
    let ok_port = |s: &str| s.parse::<u16>().is_ok();
    let ok_sock = |s: &str| s.parse::<SocketAddr>().is_ok();
    let ok_any = |_: &str| true;
    let lp = if conn.local_port.is_empty() {
        String::new()
    } else {
        format!(" (local {})", conn.local_port)
    };
    match conn.kind {
        ConnKind::Serial => {
            let data = conn.data_bits;
            let parity = match conn.parity {
                1 => "Odd",
                2 => "Even",
                _ => "None",
            };
            let stop = conn.stop_bits;
            let flow = match conn.flow_control {
                1 => "XON/XOFF",
                2 => "RTS/CTS",
                _ => "None",
            };
            format!(
                "Serial: {} {},{},{},{} flow:{}",
                or_q(&conn.serial_port, ok_any),
                conn.baud_rate,
                data,
                parity,
                stop,
                flow,
            )
        }
        ConnKind::Udp => {
            let (label, pair) = match conn.udp_mode {
                UdpModeDraft::Unicast => ("unicast", &conn.udp_unicast),
                UdpModeDraft::Broadcast => ("broadcast", &conn.udp_broadcast),
                UdpModeDraft::Multicast => ("multicast", &conn.udp_multicast),
            };
            format!(
                "UDP {label} {}:{}{lp}",
                or_q(&pair.addr, ok_ipv4),
                or_q(&pair.port, ok_port),
            )
        }
        ConnKind::Tcp => format!("TCP {}", or_q(&conn.tcp_addr, ok_sock)),
    }
}

/// Enumerate the specific reasons the Start button is disabled for a
/// channel — one human-readable line per problem. Returned in the same
/// order they appear in the editor (channel fields first, then per-message
/// issues from top to bottom).
pub(super) fn start_blockers(conn: &ConnDraft, messages: &[ScheduleDraft]) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(channel_blockers(conn));
    if messages.is_empty() {
        out.push("No messages defined — add at least one".to_string());
    } else {
        for (i, m) in messages.iter().enumerate() {
            out.extend(message_blockers(i, m));
        }
        if !messages.iter().any(|m| m.to_message_config().is_some()) {
            out.push("No message is fully filled in".to_string());
        }
    }
    out
}

fn channel_blockers(conn: &ConnDraft) -> Vec<String> {
    let mut out = Vec::new();
    match conn.kind {
        ConnKind::Serial => {
            if conn.serial_port.is_empty() {
                out.push("Channel: select a serial port".to_string());
            }
            if !conn.baud_custom.is_empty()
                && conn.baud_custom.parse::<u32>().map_or(true, |b| b == 0)
            {
                out.push("Channel: baud rate must be a positive number".to_string());
            }
        }
        ConnKind::Udp => {
            let (mode_label, pair, addr_label) = match conn.udp_mode {
                UdpModeDraft::Unicast => ("destination", &conn.udp_unicast, "address"),
                UdpModeDraft::Broadcast => ("broadcast", &conn.udp_broadcast, "address"),
                UdpModeDraft::Multicast => ("multicast", &conn.udp_multicast, "group"),
            };
            if pair.addr.is_empty() || pair.addr.parse::<Ipv4Addr>().is_err() {
                out.push(format!("Channel: {mode_label} {addr_label} must be IPv4"));
            }
            if pair.port.is_empty() || pair.port.parse::<u16>().is_err() {
                out.push(format!("Channel: {mode_label} port must be 1–65535"));
            }
            if invalid_parse::<u16>(&conn.local_port) {
                out.push("Channel: local port must be 1–65535".to_string());
            }
        }
        ConnKind::Tcp => {
            if conn.tcp_addr.is_empty() {
                out.push("Channel: address is empty".to_string());
            } else if conn.tcp_addr.parse::<SocketAddr>().is_err() {
                out.push("Channel: address must be host:port".to_string());
            }
        }
    }
    out
}

fn message_blockers(idx: usize, entry: &ScheduleDraft) -> Vec<String> {
    let mut out = Vec::new();
    let n = idx + 1;
    if entry.interval_ms.is_empty() {
        out.push(format!("Message {n}: interval is empty"));
    } else if entry.interval_ms.parse::<u64>().is_err() {
        out.push(format!("Message {n}: interval must be a whole number"));
    }
    match entry.payload_kind {
        PayloadKind::Hex if !hex_valid(&entry.hex_data) => {
            out.push(format!("Message {n}: hex is empty or invalid"));
        }
        PayloadKind::Nmea => {
            if entry.nmea_talker.is_empty() {
                out.push(format!("Message {n}: NMEA talker is empty"));
            }
            if entry.nmea_sentence_type.is_empty() {
                out.push(format!("Message {n}: NMEA sentence type is empty"));
            }
        }
        // UTF-8 / UTF-16 / ASCII payloads accept any string at this layer.
        _ => {}
    }
    out
}

// ── Interface field editors (Serial / UDP / TCP) ─────────────────────────────

pub(super) fn show_serial_fields(
    ui: &mut egui::Ui,
    conn: &mut ConnDraft,
    ports: &[String],
) -> (bool, bool) {
    let before = (
        conn.serial_port.clone(),
        conn.baud_rate,
        conn.data_bits,
        conn.parity,
        conn.stop_bits,
        conn.flow_control,
        conn.baud_custom.clone(),
    );
    let mut refresh = false;

    egui::Grid::new("serial_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Port");
            ui.horizontal(|ui| {
                let label = if conn.serial_port.is_empty() {
                    "select port\u{2026}".to_string()
                } else {
                    conn.serial_port.clone()
                };
                egui::ComboBox::from_label("")
                    .selected_text(label)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        if ports.is_empty() {
                            ui.weak("No ports found");
                        } else {
                            for port in ports {
                                ui.selectable_value(&mut conn.serial_port, port.clone(), port);
                            }
                        }
                    });
                if ui
                    .small_button("\u{21ba}")
                    .on_hover_text("Refresh port list")
                    .clicked()
                {
                    refresh = true;
                }
            });
            ui.end_row();

            ui.label("Baud");
            ui.horizontal(|ui| {
                for &baud in &[4800u32, 9600, 19200, 38400, 57600, 115200] {
                    if ui
                        .radio_value(&mut conn.baud_rate, baud, baud.to_string())
                        .clicked()
                    {
                        conn.baud_custom.clear();
                    }
                }
                ui.separator();
                let bad_baud = !conn.baud_custom.is_empty()
                    && conn.baud_custom.parse::<u32>().map_or(true, |b| b == 0);
                let r = red_bordered(
                    ui,
                    bad_baud,
                    "enter a positive baud rate — e.g. 230400",
                    |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut conn.baud_custom)
                                .id_salt("serial_baud_custom")
                                .desired_width(68.0)
                                .hint_text("custom"),
                        )
                    },
                );
                if enter_committed(&r, ui) {
                    if let Ok(b) = conn.baud_custom.parse::<u32>() {
                        if b > 0 {
                            conn.baud_rate = b;
                        }
                    }
                }
            });
            ui.end_row();

            ui.label("Data bits");
            ui.horizontal(|ui| {
                for &bits in &[5u8, 6, 7, 8] {
                    ui.radio_value(&mut conn.data_bits, bits, bits.to_string());
                }
            });
            ui.end_row();

            ui.label("Parity");
            ui.horizontal(|ui| {
                ui.radio_value(&mut conn.parity, 0u8, "None");
                ui.radio_value(&mut conn.parity, 1u8, "Odd");
                ui.radio_value(&mut conn.parity, 2u8, "Even");
            });
            ui.end_row();

            ui.label("Stop bits");
            ui.horizontal(|ui| {
                ui.radio_value(&mut conn.stop_bits, 1u8, "1");
                ui.radio_value(&mut conn.stop_bits, 2u8, "2");
            });
            ui.end_row();

            ui.label("Flow control");
            ui.horizontal(|ui| {
                ui.radio_value(&mut conn.flow_control, 0u8, "None");
                ui.radio_value(&mut conn.flow_control, 1u8, "Software");
                ui.radio_value(&mut conn.flow_control, 2u8, "Hardware");
            });
            ui.end_row();
        });

    let after = (
        conn.serial_port.clone(),
        conn.baud_rate,
        conn.data_bits,
        conn.parity,
        conn.stop_bits,
        conn.flow_control,
        conn.baud_custom.clone(),
    );
    (before != after, refresh)
}

pub(super) fn show_udp_fields(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let before_mode = conn.udp_mode;
    let mut apply = false;

    // Per-mode Grid id even though all three modes now render the same
    // [addr] [port] [local-port] shape — kept as defence-in-depth so any
    // auto-derived (un-salted) widget id inside the Grid lives in its
    // own namespace per mode, the same trick `message_grid_<kind>` uses
    // in the message editor.
    let grid_id = match conn.udp_mode {
        UdpModeDraft::Unicast => "udp_grid_unicast",
        UdpModeDraft::Broadcast => "udp_grid_broadcast",
        UdpModeDraft::Multicast => "udp_grid_multicast",
    };
    egui::Grid::new(grid_id)
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            apply |= show_udp_mode_row(ui, conn);
            apply |= show_udp_destination_row(ui, conn);
            apply |= show_udp_local_port_row(ui, conn);
        });

    apply || (conn.udp_mode != before_mode)
}

fn show_udp_mode_row(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    ui.label("Mode");
    ui.horizontal(|ui| {
        ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Broadcast, "Broadcast");
        ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Unicast, "Unicast");
        ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Multicast, "Multicast");
    });
    ui.end_row();
    false
}

/// All three UDP modes have the same shape — an IPv4 address plus a port
/// — so they share one row helper. The per-mode differences (label, hint
/// text, validation message, tooltip, id salts, and which pair of
/// `udp_unicast` / `udp_broadcast` / `udp_multicast` strings to point
/// at) are looked up from a single match.
fn show_udp_destination_row(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let mode = conn.udp_mode;
    // Compute validation flags up front while we still hold an immutable
    // borrow — the mutable destructure below precludes re-reading.
    let pair_ref = match mode {
        UdpModeDraft::Unicast => &conn.udp_unicast,
        UdpModeDraft::Broadcast => &conn.udp_broadcast,
        UdpModeDraft::Multicast => &conn.udp_multicast,
    };
    // Two-mode validation. *Lenient* before the user has submitted
    // (no red on empty, no red on partial IPv4 like `192.168.1.`),
    // *strict* after — empty AND any malformed value go red. The
    // submit flag flips on the first Enter, Start, or profile load.
    let (bad_addr, bad_port) = if pair_ref.submitted {
        (
            pair_ref.addr.parse::<Ipv4Addr>().is_err(),
            pair_ref.port.parse::<u16>().is_err(),
        )
    } else {
        (
            invalid_ipv4(&pair_ref.addr),
            invalid_parse::<u16>(&pair_ref.port),
        )
    };

    let (label, label_tip, hint, invalid_msg, addr_salt, port_salt) = match mode {
        UdpModeDraft::Unicast => (
            "Destination",
            None,
            "192.168.1.100",
            "enter an IPv4 address — e.g. 192.168.1.100",
            "udp_unicast_addr",
            "udp_unicast_port",
        ),
        UdpModeDraft::Broadcast => (
            "Destination",
            None,
            "255.255.255.255",
            "enter an IPv4 address — e.g. 255.255.255.255",
            "udp_broadcast_addr",
            "udp_broadcast_port",
        ),
        UdpModeDraft::Multicast => (
            "Multicast group",
            Some(
                "IPv4 multicast group address (must be in the 224.0.0.0 – \
                 239.255.255.255 range). Receivers must subscribe to the same \
                 group + port to see these packets. Common admin-local picks \
                 live in 239.x.x.x.",
            ),
            "239.0.0.1",
            "enter IPv4 multicast address — e.g. 239.0.0.1",
            "udp_multicast_addr",
            "udp_multicast_port",
        ),
    };
    let label_resp = ui.label(label);
    if let Some(t) = label_tip {
        let _ = label_resp.on_hover_text(t);
    }

    let pair = match mode {
        UdpModeDraft::Unicast => &mut conn.udp_unicast,
        UdpModeDraft::Broadcast => &mut conn.udp_broadcast,
        UdpModeDraft::Multicast => &mut conn.udp_multicast,
    };
    let apply = show_addr_port_row(
        ui,
        AddrPortRow {
            addr_field: &mut pair.addr,
            addr_id_salt: addr_salt,
            addr_hint: hint,
            addr_invalid_msg: invalid_msg,
            bad_addr,
            port_field: &mut pair.port,
            port_id_salt: port_salt,
            bad_port,
            port_hold: &mut conn.udp_port_hold,
        },
    );
    // First explicit commit (Enter on either field, or a ± port
    // click that changes the value) flips the pair into strict
    // validation. Stays flipped for the life of the channel.
    if apply {
        pair.submitted = true;
    }
    ui.end_row();
    apply
}

fn show_udp_local_port_row(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let bad_local = invalid_parse::<u16>(&conn.local_port);
    ui.label("Local port");
    let r = red_bordered(ui, bad_local, "enter a port number 1–65535", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut conn.local_port)
                .id_salt("udp_local_port")
                .desired_width(80.0)
                .hint_text("auto"),
        )
    });
    let apply = enter_committed(&r, ui);
    ui.end_row();
    apply
}

/// Parameters for [`show_addr_port_row`] — shared by all three UDP-mode
/// editors. Renders the `[addr] Port: [-] [port] [+]` strip with the same
/// hold-to-repeat ± behaviour, against per-mode fields / ids / hints /
/// validation messages.
struct AddrPortRow<'a> {
    addr_field: &'a mut String,
    addr_id_salt: &'a str,
    addr_hint: &'a str,
    addr_invalid_msg: &'a str,
    bad_addr: bool,
    port_field: &'a mut String,
    port_id_salt: &'a str,
    bad_port: bool,
    port_hold: &'a mut Option<PortHold>,
}

/// Render the right-hand side of a UDP destination row: address TextEdit,
/// "Port:" label, hold-to-repeat ± buttons around the port TextEdit.
/// Returns `true` if the user committed an edit (Enter on either field,
/// or a ± click that changed the port).
fn show_addr_port_row(ui: &mut egui::Ui, p: AddrPortRow) -> bool {
    let mut apply = false;
    ui.horizontal(|ui| {
        let addr_r = red_bordered(ui, p.bad_addr, p.addr_invalid_msg, |ui| {
            ui.add(
                egui::TextEdit::singleline(p.addr_field)
                    .id_salt(p.addr_id_salt)
                    .desired_width(140.0)
                    .hint_text(p.addr_hint),
            )
        });
        if enter_committed(&addr_r, ui) {
            apply = true;
        }
        ui.label("Port:");
        let r_minus = ui
            .small_button("\u{2212}")
            .on_hover_text("Decrement port (hold to accelerate)");
        let port_r = red_bordered(ui, p.bad_port, "enter a port number 1–65535", |ui| {
            ui.add(
                egui::TextEdit::singleline(p.port_field)
                    .id_salt(p.port_id_salt)
                    .desired_width(60.0),
            )
        });
        if enter_committed(&port_r, ui) {
            apply = true;
        }
        let r_plus = ui
            .small_button("+")
            .on_hover_text("Increment port (hold to accelerate)");
        if drive_port_hold(ui, p.port_hold, p.port_field, &r_minus, &r_plus) {
            apply = true;
        }
    });
    apply
}

pub(super) fn show_tcp_fields(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let mut apply = false;

    egui::Grid::new("tcp_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            let bad_tcp = invalid_parse::<SocketAddr>(&conn.tcp_addr);
            ui.label("Address");
            let r = red_bordered(
                ui,
                bad_tcp,
                "enter host:port — e.g. 192.168.1.100:4000",
                |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut conn.tcp_addr)
                            .id_salt("tcp_addr")
                            .desired_width(220.0)
                            .hint_text("host:port  (Enter to apply)"),
                    )
                },
            );
            if enter_committed(&r, ui) {
                apply = true;
            }
            ui.end_row();
        });

    apply
}

// ── Hold-to-repeat port ± buttons ─────────────────────────────────────────────

/// Drive one frame of hold-to-repeat for the broadcast port's ± buttons.
///
/// - A simple click changes the port by exactly 1.
/// - Holding either button fires once immediately, then waits ~250 ms, then
///   auto-repeats at a rate that *accelerates* the longer the button is
///   held (see [`port_repeat_interval`]).
/// - Switching from one button to the other while held resets the state.
///
/// Uses absolute `Instant` deadlines (no per-frame `dt` accumulation), so the
/// cadence stays correct even when the framerate is jittery. Schedules the
/// next egui repaint precisely at the next fire instant via
/// `request_repaint_after`, so the loop keeps running without depending on
/// other input events.
///
/// Returns `true` if the port value changed this frame.
fn drive_port_hold(
    ui: &egui::Ui,
    hold: &mut Option<PortHold>,
    port_field: &mut String,
    r_minus: &egui::Response,
    r_plus: &egui::Response,
) -> bool {
    use std::time::{Duration, Instant};

    let mut changed = false;
    let now = Instant::now();

    // Use *global* pointer state, not Response::is_pointer_button_down_on,
    // because that per-widget flag depends on the widget's egui id being
    // present every frame — a single frame where it isn't tracked drops
    // the flag and ends the hold. Global primary_down stays true while the
    // mouse button is physically down regardless of what egui can or can't
    // see about the widget.
    let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
    let primary_down = ui.input(|i| i.pointer.primary_down());

    // Initial press: pointer was just pressed AND was hovering one of our
    // buttons. Fire once and start the hold.
    if primary_pressed {
        let direction: i8 = if r_minus.hovered() {
            -1
        } else if r_plus.hovered() {
            1
        } else {
            0
        };
        if direction != 0 {
            changed |= port_step(port_field, direction);
            *hold = Some(PortHold {
                direction,
                started: now,
                next_fire_at: now + Duration::from_millis(250),
            });
        }
    }

    // Ongoing hold.
    if let Some(mut h) = *hold {
        if !primary_down {
            *hold = None;
        } else {
            // Catch up any deadlines that have already passed in a single
            // frame (handles slow frames cleanly).
            while now >= h.next_fire_at {
                let interval = port_repeat_interval(now.saturating_duration_since(h.started));
                h.next_fire_at += interval;
                changed |= port_step(port_field, h.direction);
            }
            *hold = Some(h);
            // Wake egui up exactly when the next fire is due, so the loop
            // keeps running without depending on any other input event.
            ui.ctx()
                .request_repaint_after(h.next_fire_at.saturating_duration_since(now));
        }
    }

    changed
}

/// Step a port-number string by `direction` (±1), clamped to 1..=65535.
/// Returns `true` if the value actually changed.
///
/// An empty field bootstraps to `1` on either button — otherwise the
/// buttons would silently do nothing until the user typed a starting
/// number. A non-empty but unparseable value (e.g. `444444444`) is left
/// alone so the user's typo isn't trashed.
fn port_step(port_field: &mut String, direction: i8) -> bool {
    if port_field.is_empty() {
        *port_field = "1".to_string();
        return true;
    }
    let Ok(p) = port_field.parse::<u16>() else {
        return false;
    };
    let new = match direction {
        -1 if p > 1 => p - 1,
        1 if p < u16::MAX => p + 1,
        _ => return false,
    };
    *port_field = new.to_string();
    true
}

/// Acceleration curve for the ± port hold-to-repeat.
/// Time-elapsed-since-press → delay until the next repeat.
///
/// Tiered (not exponential) so the cadence is predictable when the user is
/// targeting a specific port number. The initial 250 ms delay before the
/// first auto-repeat is handled separately in [`drive_port_hold`].
fn port_repeat_interval(elapsed: std::time::Duration) -> std::time::Duration {
    use std::time::Duration;
    match elapsed.as_secs_f32() {
        t if t < 1.0 => Duration::from_millis(100), // 10 / s for the first second
        t if t < 3.0 => Duration::from_millis(50),  // 20 / s next two seconds
        t if t < 6.0 => Duration::from_millis(25),  // 40 / s next three seconds
        _ => Duration::from_millis(10),             // 100 / s after that
    }
}

// ── Byte previews ─────────────────────────────────────────────────────────────

/// Render `bytes` as a single-line preview string.
///
/// Every printable ASCII byte (`0x20..=0x7E`) is emitted as-is; **every
/// other byte** — control characters, CR/LF, anything ≥ 0x80, and the
/// individual bytes of any multi-byte UTF-8 sequence — becomes a `‹XX›`
/// marker. This guarantees that the bundled fonts can render every glyph the
/// preview emits, so nothing tofus. The tradeoff: pretty Unicode display
/// is lost in the preview — `café` shows as `caf‹C3›‹A9›` — but the user
/// can see the exact bytes that will go on the wire, which matters more
/// for a tool like this.
///
/// Embedded `\r` and `\n` therefore appear as `‹0D›‹0A›` (visible, no
/// real line break), so the preview always renders on a single line and
/// no separate trim step is needed.
pub(super) fn preview_text(bytes: &[u8]) -> String {
    // Printable ASCII passes through; anything else (control bytes
    // and high bytes alike) is opaque to this preview, so render it
    // as the familiar `‹XX›` byte marker.
    preview_with(bytes, |b| (0x20..=0x7E).contains(&b).then_some(b as char))
}

/// Preview text for an `Ascii` payload, decoding high bytes through
/// `code_page` so the user sees what a code-page-aware receiver would
/// render. Control bytes (0x00–0x1F and 0x7F) still show as `‹XX›`
/// byte markers so they're never invisible.
pub(super) fn preview_ascii(bytes: &[u8], code_page: CodePage) -> String {
    preview_with(bytes, |b| match b {
        0x00..=0x1F | 0x7F => None,
        _ => Some(decode_codepage_byte(b, code_page)),
    })
}

/// Shared body of [`preview_text`] / [`preview_ascii`]: walk `bytes`
/// and produce one output character per input byte — either the
/// caller-supplied glyph or, when the caller returns `None`, the
/// `‹XX›` byte marker. Keeping the marker format and the loop in one
/// place means new preview variants only need a closure.
fn preview_with<F: Fn(u8) -> Option<char>>(bytes: &[u8], decode: F) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match decode(b) {
            Some(c) => out.push(c),
            None => out.push_str(&format!("\u{2039}{b:02X}\u{203A}")),
        }
    }
    out
}

// ── Marker-aware text editing ─────────────────────────────────────────────────

/// `egui::TextBuffer` wrapper around a `&mut String` that **uppercases every
/// character on insert**, so a hex field's value can never momentarily contain
/// a lowercase letter (no one-frame flash between keystroke and post-hoc
/// `to_ascii_uppercase`). Used for the message editor's Hex data field.
pub(super) struct UppercaseHex<'a>(pub(super) &'a mut String);

impl egui::TextBuffer for UppercaseHex<'_> {
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        let upper = text.to_ascii_uppercase();
        let byte_idx = self
            .0
            .char_indices()
            .nth(char_index)
            .map_or(self.0.len(), |(i, _)| i);
        self.0.insert_str(byte_idx, &upper);
        upper.chars().count()
    }
    fn delete_char_range(&mut self, char_range: std::ops::Range<usize>) {
        let start = self
            .0
            .char_indices()
            .nth(char_range.start)
            .map_or(self.0.len(), |(i, _)| i);
        let end = self
            .0
            .char_indices()
            .nth(char_range.end)
            .map_or(self.0.len(), |(i, _)| i);
        self.0.replace_range(start..end, "");
    }
    fn type_id(&self) -> std::any::TypeId {
        // `UppercaseHex<'a>` isn't `'static`, so we can't use `TypeId::of::<Self>()`.
        // Use a `'static` marker — egui only needs *some* stable TypeId.
        struct UppercaseHexMarker;
        std::any::TypeId::of::<UppercaseHexMarker>()
    }
}

/// Lay out a UTF-8/ASCII text field, drawing `‹XX›` byte markers in a
/// distinct colour from surrounding text (spec §5.3).
fn marker_layouter(
    ui: &egui::Ui,
    buf: &dyn egui::TextBuffer,
    wrap_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let text = buf.as_str();
    let font = egui::TextStyle::Body.resolve(ui.style());
    let normal = ui.visuals().text_color();
    let marker = egui::Color32::from_rgb(110, 170, 255);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for (range, segment) in segments(text) {
        let color = match segment {
            Segment::Byte(_) => marker,
            Segment::Text => normal,
        };
        job.append(
            &text[range],
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color,
                ..Default::default()
            },
        );
    }
    ui.fonts_mut(|f| f.layout_job(job))
}

/// Single-line TextEdit for text that may contain `‹XX›` byte markers.
///
/// Three marker-aware behaviours layered on a plain `TextEdit`:
///
///  1. Coloured-marker highlighting via [`marker_layouter`].
///  2. *Atomic* marker deletion via [`repair_after_edit`]: a single
///     keystroke that disturbs a complete marker removes the whole
///     4-character unit rather than leaving an orphan `‹` / `›`.
///  3. *Cursor jump*: if the caret lands strictly inside a marker
///     (typed-through, clicked-into, etc.), it snaps to the marker's
///     near edge — direction of movement when known, closer edge on a
///     fresh click. Markers behave as single atoms for navigation.
///
/// The pre-edit text, previous cursor position, and the widget id are
/// stashed in `egui::Memory` so [`show_insert_byte_button`] (rendered
/// from a different ui parent — the popup) can read the target's
/// cursor and write a new one after inserting markers.
pub(super) fn marker_aware_text_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    width: f32,
    hint: &str,
) -> egui::Response {
    let stash_prev_text = ui.id().with("marker_prev").with(salt);
    let stash_prev_cursor = ui.id().with("marker_prev_cursor").with(salt);
    // Shared (parent-independent) ids the insert-byte popup uses.
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(salt);

    let prev_text: String = ui
        .memory(|m| m.data.get_temp::<String>(stash_prev_text))
        .unwrap_or_else(|| text.clone());
    let prev_cursor: Option<usize> = ui.memory(|m| m.data.get_temp(stash_prev_cursor));

    let mut layouter = marker_layouter;
    let output = egui::TextEdit::singleline(text)
        .id_salt(salt)
        .desired_width(width)
        .hint_text(hint)
        .layouter(&mut layouter)
        .show(ui);

    // TextEdit::show returns AtomLayoutResponse wrapping the actual
    // Response — unwrap once here so the rest reads naturally.
    let resp = output.response.response;
    if resp.changed() {
        repair_after_edit(&prev_text, text);
    }
    ui.memory_mut(|m| m.data.insert_temp(stash_prev_text, text.clone()));

    let widget_id = resp.id;
    ui.memory_mut(|m| m.data.insert_temp(shared_widget_id, widget_id));

    if let Some(range) = output.cursor_range {
        // Only snap when there's no active selection — otherwise we'd
        // yank the user's shift-arrow selection sideways.
        let has_selection = range.primary != range.secondary;
        let cursor_char = range.primary.index;
        let cursor_byte = char_to_byte(text, cursor_char);
        let mut effective_cursor_char = cursor_char;
        if !has_selection {
            for (mrange, seg) in segments(text) {
                if !matches!(seg, Segment::Byte(_)) {
                    continue;
                }
                if mrange.start < cursor_byte && cursor_byte < mrange.end {
                    let target_byte = match prev_cursor.map(|p| char_to_byte(text, p)) {
                        Some(p) if p < cursor_byte => mrange.end,
                        Some(p) if p > cursor_byte => mrange.start,
                        // Fresh click or stationary — closer edge, end on tie.
                        _ => {
                            if cursor_byte - mrange.start < mrange.end - cursor_byte {
                                mrange.start
                            } else {
                                mrange.end
                            }
                        }
                    };
                    let target_char = byte_to_char(text, target_byte);
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), widget_id) {
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(target_char),
                            )));
                        state.store(ui.ctx(), widget_id);
                    }
                    effective_cursor_char = target_char;
                    break;
                }
            }
        }
        ui.memory_mut(|m| {
            m.data.insert_temp(stash_prev_cursor, effective_cursor_char);
            m.data.insert_temp(shared_cursor_id, effective_cursor_char);
        });
    }

    resp
}

/// Plain single-line TextEdit that stashes its cursor + widget id
/// under the same shared ids [`marker_aware_text_edit`] uses, so the
/// matching `Insert …` popup can find them. Use this for fields
/// that don't recognise `‹XX›` markers (UTF-16 in its default
/// Unicode mode).
pub(super) fn plain_text_edit_with_cursor(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    width: f32,
    hint: &str,
) -> egui::Response {
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(salt);
    let output = egui::TextEdit::singleline(text)
        .id_salt(salt)
        .desired_width(width)
        .hint_text(hint)
        .show(ui);
    let resp = output.response.response;
    ui.memory_mut(|m| m.data.insert_temp(shared_widget_id, resp.id));
    if let Some(range) = output.cursor_range {
        ui.memory_mut(|m| m.data.insert_temp(shared_cursor_id, range.primary.index));
    }
    resp
}

/// Byte position of the character at `char_idx` in `text`. Saturates to
/// `text.len()` for indices past the end (treat as the after-last position).
fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Character position of the byte at `byte_idx`. Clamps `byte_idx` to
/// `text.len()` first so callers don't have to.
fn byte_to_char(text: &str, byte_idx: usize) -> usize {
    let byte_idx = byte_idx.min(text.len());
    text[..byte_idx].chars().count()
}

// ── Insert Byte / Insert Code Unit popups ─────────────────────────────────────

/// Parse the Insert Byte popup's hex input — single byte (`1B`) or a
/// space- and/or comma-separated list (`1B 0D 0A`, `1B,0D,0A`,
/// `1B, 0D 0A`). On failure the `Err` is the disabled-button hover text.
fn parse_hex_bytes(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "Type 1–2 hex digits — single byte (1B) or several separated by \
             spaces / commas (1B 0D 0A)"
                .to_string(),
        );
    }
    let pieces: Vec<&str> = trimmed
        .split([' ', ',', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.is_empty() {
        // All separators, no values — e.g. " , , ".
        return Err("No hex digits found between separators".to_string());
    }
    let mut out = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        if piece.len() > 2 {
            return Err(format!(
                "`{piece}` is more than 2 hex digits — split bytes with a \
                 space or comma (1B 0D 0A)"
            ));
        }
        match u8::from_str_radix(piece, 16) {
            Ok(b) => out.push(b),
            Err(_) => {
                return Err(format!(
                    "`{piece}` is not a valid hex byte — use 1–2 digits 0–9 / A–F"
                ))
            }
        }
    }
    Ok(out)
}

/// Parse the UTF-16 Insert popup's input — one or more 4-hex-digit
/// code units, optionally space/comma separated (`0E16`,
/// `0E16 1F62`, `0E16, 1F62`). Each piece must be exactly 4 hex
/// digits; the active byte order is applied by the caller, not here.
fn parse_hex_units(input: &str) -> Result<Vec<u16>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "Type 4 hex digits — single code unit (0E16) or several separated \
             by spaces / commas (0E16 1F62)"
                .to_string(),
        );
    }
    let pieces: Vec<&str> = trimmed
        .split([' ', ',', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.is_empty() {
        return Err("No hex digits found between separators".to_string());
    }
    let mut out = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        if piece.len() != 4 {
            return Err(format!(
                "`{piece}` must be exactly 4 hex digits (one UTF-16 code unit) \
                 — use spaces or commas to separate units (0E16 1F62)"
            ));
        }
        match u16::from_str_radix(piece, 16) {
            Ok(u) => out.push(u),
            Err(_) => {
                return Err(format!(
                    "`{piece}` is not a valid hex code unit — use digits 0–9 / A–F"
                ))
            }
        }
    }
    Ok(out)
}

/// Chrome strings for the insert popup — bundled so
/// [`show_insert_popup`] doesn't take a parade of `&'static str`s.
struct InsertChrome {
    button_label: &'static str,
    field_label: &'static str,
    hint: &'static str,
}

/// "Insert Byte" button — popup inserts one or more raw bytes (each
/// wrapped in a `‹XX›` marker) at the target field's cursor. Used by
/// UTF-8, ASCII, and UTF-16 (with raw-bytes mode on) payloads.
pub(super) fn show_insert_byte_button(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
) {
    show_insert_popup(
        ui,
        text,
        hex,
        target_salt,
        InsertChrome {
            button_label: "Insert Byte",
            field_label: "Byte value(s) (hex):",
            hint: "1B  or  1B 0D 0A",
        },
        |s| Ok(bytes_to_markers(&parse_hex_bytes(s)?)),
    );
}

/// "Insert Code Unit" button — UTF-16 variant. Each unit is 4 hex
/// digits (one `u16`). What gets inserted depends on `allow_raw_bytes`:
///
///  - `false` (default): the units decode as UTF-16 to actual
///    Unicode characters and are inserted verbatim. `0E16` inserts
///    `ฃ`, surrogate pairs are recognised, lone surrogates error.
///  - `true`: each unit splits into two raw bytes per `big_endian`
///    and is inserted as a pair of `‹XX›` markers.
pub(super) fn show_insert_unit_button(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
    big_endian: bool,
    allow_raw_bytes: bool,
) {
    show_insert_popup(
        ui,
        text,
        hex,
        target_salt,
        InsertChrome {
            // `0E16` is Thai `ฃ` — renders because the shared font stack
            // bundles Noto Sans Thai as a fallback. CJK is *not* bundled,
            // so codepoints in U+4E00–9FFF still show as tofu.
            button_label: "Insert Code Unit",
            field_label: "Code unit(s) (4 hex):",
            hint: "0E16  or  0E16 1F62",
        },
        move |s| {
            let units = parse_hex_units(s)?;
            if allow_raw_bytes {
                let mut bytes = Vec::with_capacity(units.len() * 2);
                for u in &units {
                    if big_endian {
                        bytes.extend_from_slice(&u.to_be_bytes());
                    } else {
                        bytes.extend_from_slice(&u.to_le_bytes());
                    }
                }
                Ok(bytes_to_markers(&bytes))
            } else {
                String::from_utf16(&units).map_err(|_| {
                    "lone surrogate — pair high (D800–DBFF) and low (DC00–DFFF) \
                     surrogates together (e.g. D83D DE00 for 😀)"
                        .to_string()
                })
            }
        },
    );
}

/// Wrap each byte in a `‹XX›` marker (uppercase hex). The string is
/// what gets inserted into a marker-aware text field.
fn bytes_to_markers(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("\u{2039}{b:02X}\u{203A}"))
        .collect()
}

/// Shared MenuButton + popup chrome that the byte / code-unit insert
/// buttons hang off. `parse` turns the popup's text into the literal
/// string to splice into the target field at the cursor — bytes get
/// marker-wrapped by the caller's parser, glyph mode produces real
/// Unicode characters. Labels / hint live in [`InsertChrome`]; the
/// cursor bookkeeping is common to every caller.
///
/// `target_salt` must match the salt passed to
/// [`marker_aware_text_edit`] for the field this drives — that's how the
/// popup (rendered under a different ui parent) finds the target's
/// cursor and widget id in egui memory.
fn show_insert_popup<F>(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
    chrome: InsertChrome,
    parse: F,
) where
    F: Fn(&str) -> Result<String, String>,
{
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(target_salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(target_salt);

    // Default menu close behavior is `CloseOnClick`, which closes the
    // menu the moment the user clicks anywhere inside — including the
    // TextEdit (which has to be clicked to gain focus). Switch to
    // `CloseOnClickOutside` so the popup stays open while the user
    // types the hex value.
    egui::containers::menu::MenuButton::new(chrome.button_label)
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            // Three rows — label, hex entry, Insert button — in a fixed
            // 200-px wide child UI with a non-justified top-down layout.
            // The fixed width keeps the popup compact enough to fit
            // below the trigger button (egui's auto-placement flips
            // popups above when they'd be too wide for the space below).
            // A bare `ui.vertical` would inherit the menu's
            // `top_down_justified` layout, which stretches each row to
            // the full layout width — re-introducing the same flip.
            ui.allocate_ui_with_layout(
                egui::vec2(200.0, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.label(chrome.field_label);
                    let resp = ui.add(
                        egui::TextEdit::singleline(hex)
                            .desired_width(180.0)
                            .hint_text(chrome.hint),
                    );
                    // Auto-focus the hex field so the user can start typing
                    // right away. Idempotent — egui doesn't keep resetting
                    // the caret if the field already has focus.
                    resp.request_focus();
                    let parse_result = parse(hex);
                    let ok = parse_result.is_ok();
                    // Enter while the popup is open commits — gated on the
                    // input parsing, not on `resp.lost_focus()` (which
                    // doesn't always fire for popup-hosted TextEdits, so
                    // Enter would otherwise feel dead). Only consume Enter
                    // when the value parses.
                    let entered =
                        resp.has_focus() && ok && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let mut insert = ui.add_enabled(ok, egui::Button::new("Insert"));
                    if let Err(why) = &parse_result {
                        insert = insert.on_disabled_hover_text(why.clone());
                    }
                    if let Ok(insertion) = &parse_result {
                        if insert.clicked() || entered {
                            // Cursor byte position from memory; fall back
                            // to end-of-text if the field was never focused.
                            let cursor_char: Option<usize> =
                                ui.memory(|m| m.data.get_temp(shared_cursor_id));
                            let insert_byte = cursor_char
                                .map(|c| char_to_byte(text, c))
                                .unwrap_or(text.len());
                            text.insert_str(insert_byte, insertion);
                            // New cursor sits right after the inserted
                            // text. Update both the shared stash (so a
                            // subsequent Insert lands in the right place
                            // even if the field isn't re-focused first)
                            // and the actual TextEditState.
                            let new_cursor_char = byte_to_char(text, insert_byte + insertion.len());
                            ui.memory_mut(|m| {
                                m.data.insert_temp(shared_cursor_id, new_cursor_char);
                            });
                            let widget_id: Option<egui::Id> =
                                ui.memory(|m| m.data.get_temp(shared_widget_id));
                            if let Some(wid) = widget_id {
                                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), wid) {
                                    state.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::one(egui::text::CCursor::new(
                                            new_cursor_char,
                                        )),
                                    ));
                                    state.store(ui.ctx(), wid);
                                }
                            }
                            hex.clear();
                            ui.close();
                        }
                    }
                },
            );
        });
}

// ── Display pane ──────────────────────────────────────────────────────────────

/// Render a channel's real-time outbound display pane (spec §5.7).
pub(super) fn show_display_pane(ui: &mut egui::Ui, display: &mut ChannelDisplay) {
    ui.collapsing("Output", |ui| {
        ui.horizontal(|ui| {
            ui.label("View:").on_hover_text(
                "These are display modes — the bytes on the wire are the \
                 same regardless of which view is selected. The view only \
                 changes how the buffered bytes are rendered here.",
            );
            ui.radio_value(&mut display.mode, DisplayMode::Hex, "Hex");
            ui.radio_value(&mut display.mode, DisplayMode::Rendered, "Rendered");
            // Raw comes last so the ctrl-chars sub-options below sit
            // immediately next to the radio they modify.
            ui.radio_value(&mut display.mode, DisplayMode::Raw, "Raw");
            // Wrap the conditional ctrl-chars block in a stable id scope so
            // its appearance / disappearance can't shift the auto-derived
            // ids of the surrounding widgets (Clear button, etc.) and trip
            // egui's "duplicate widget id" warnings on view-mode changes.
            ui.push_id("ctrl_chars_block", |ui| {
                if display.mode == DisplayMode::Raw {
                    ui.separator();
                    ui.label("ctrl-chars:");
                    ui.radio_value(
                        &mut display.control_style,
                        ControlStyle::Pictures,
                        "\u{240A}",
                    );
                    ui.radio_value(&mut display.control_style, ControlStyle::Brackets, "[LF]");
                    ui.radio_value(&mut display.control_style, ControlStyle::HexEscapes, "<0A>");
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    display.clear();
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(150.0)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                // One Label for the whole pane so consecutive sends flow
                // into each other (per-message labels gained a padding gap
                // that read as a stray newline). The text is memoized in
                // ChannelDisplay — re-rendered only when the buffer or the
                // view settings change, not per repaint.
                ui.add(
                    egui::Label::new(egui::RichText::new(display.rendered()).monospace())
                        .wrap()
                        .selectable(true),
                );
            });
    });
}

// ── Validation / chrome helpers ───────────────────────────────────────────────

/// "Broken value" red-box for any text-input field. Calls `add` to
/// render the field, and when `invalid` is true tints the fill pink,
/// paints a 2-px red outline just outside the widget rect, and
/// attaches `msg` as the hover tooltip. Returns the field's
/// [`egui::Response`] unchanged so callers can still chain `lost_focus()`,
/// `changed()`, etc.
///
/// Replaces an earlier "red error row below the field" pattern that
/// pushed surrounding controls around as the user typed.
pub(super) fn red_bordered<F>(ui: &mut egui::Ui, invalid: bool, msg: &str, add: F) -> egui::Response
where
    F: FnOnce(&mut egui::Ui) -> egui::Response,
{
    /// `220,80,80` — the rest of the GUI's "warning red" (status dot,
    /// invalid-field outline, profile-summary `?` badge).
    const RED: egui::Color32 = egui::Color32::from_rgb(220, 80, 80);
    /// Translucent red — low alpha keeps text legible while making
    /// the whole field obviously broken at a glance.
    const TINT: egui::Color32 = egui::Color32::from_rgba_premultiplied(31, 12, 12, 36);

    // Always `ui.scope`, even when valid, so the field's id derives
    // from a stable position in the ui tree — flipping in and out of
    // a scope on every keystroke would drop keyboard focus.
    let inner = ui.scope(|ui| {
        if invalid {
            // Pink fill via the two fields TextEdit might read:
            //  - `text_edit_bg_color` is the explicit override
            //  - `extreme_bg_color` is the fallback when the former is `None`
            let v = ui.visuals_mut();
            v.text_edit_bg_color = Some(TINT);
            v.extreme_bg_color = TINT;
        }
        add(ui)
    });
    let resp = inner.inner;
    if invalid {
        // Explicit outline outside the rect — guarantees a visible
        // 2-px red box regardless of which `Visuals` field a given
        // egui version's TextEdit uses for its border.
        ui.painter().rect_stroke(
            resp.rect,
            egui::CornerRadius::same(2),
            egui::Stroke::new(2.0, RED),
            egui::StrokeKind::Outside,
        );
        resp.on_hover_text(msg)
    } else {
        resp
    }
}

/// True when the user "committed" the contents of a TextEdit by pressing
/// Enter on the way out — the pattern we use to apply interface-field
/// changes to the running talker thread.
pub(super) fn enter_committed(r: &egui::Response, ui: &egui::Ui) -> bool {
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// True when `s` is a non-empty string that fails to parse as `T`.
/// Used to drive the red-border / start-blocker validation: empty means
/// "user hasn't typed anything here yet" (not an error to surface),
/// whereas non-empty + parse fail means the user typed something wrong.
pub(super) fn invalid_parse<T>(s: &str) -> bool
where
    T: std::str::FromStr,
{
    !s.is_empty() && s.parse::<T>().is_err()
}

/// "Broken value" check for an IPv4-address text field, tolerant of
/// partial typing.
///
/// Empty and not-yet-complete inputs are considered OK so the field
/// doesn't flash red while the user is mid-type. Only flags red once
/// the string is unambiguously garbage:
///
///  - any character that isn't a digit or `.`
///  - more than four dot-separated parts
///  - exactly four parts with none empty, but the whole string still
///    fails to parse as [`Ipv4Addr`] (e.g. `192.168.1.999`)
///
/// In particular the LAN-prefix default `192.168.1.` (4 parts, last
/// empty) is considered "still being typed" — no red.
fn invalid_ipv4(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.chars().any(|c| !c.is_ascii_digit() && c != '.') {
        return true;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 4 {
        return true;
    }
    parts.len() == 4
        && parts.iter().all(|p| !p.is_empty())
        && s.parse::<std::net::Ipv4Addr>().is_err()
}

pub(super) fn hex_valid(s: &str) -> bool {
    let stripped: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    !stripped.is_empty()
        && stripped.len().is_multiple_of(2)
        && stripped.chars().all(|c| c.is_ascii_hexdigit())
}

pub(super) fn code_page_label(code_page: CodePage) -> &'static str {
    match code_page {
        CodePage::Iso8859_1 => "ISO-8859-1",
        CodePage::Windows1252 => "Windows-1252",
        CodePage::Cp437 => "CP437",
        CodePage::MacRoman => "Mac OS Roman",
    }
}

pub(super) fn checksum_label(algorithm: ChecksumAlgorithm) -> &'static str {
    match algorithm {
        ChecksumAlgorithm::Xor => "XOR",
        ChecksumAlgorithm::Crc8 => "CRC-8",
        ChecksumAlgorithm::Crc16Ccitt => "CRC-16/CCITT",
        ChecksumAlgorithm::Crc16Modbus => "CRC-16/MODBUS",
        ChecksumAlgorithm::Crc32 => "CRC-32",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_hex_bytes ───────────────────────────────────────────────────────

    #[test]
    fn parse_single_byte() {
        assert_eq!(parse_hex_bytes("1B").unwrap(), vec![0x1B]);
        assert_eq!(parse_hex_bytes("ff").unwrap(), vec![0xFF]);
        assert_eq!(parse_hex_bytes("0").unwrap(), vec![0x00]);
    }

    #[test]
    fn parse_space_separated() {
        assert_eq!(parse_hex_bytes("1B 0D 0A").unwrap(), vec![0x1B, 0x0D, 0x0A]);
    }

    #[test]
    fn parse_comma_separated() {
        assert_eq!(parse_hex_bytes("1B,0D,0A").unwrap(), vec![0x1B, 0x0D, 0x0A]);
    }

    #[test]
    fn parse_mixed_separators_and_extra_whitespace() {
        assert_eq!(
            parse_hex_bytes("  1B,  0D 0A,   FF  ").unwrap(),
            vec![0x1B, 0x0D, 0x0A, 0xFF]
        );
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_hex_bytes("").is_err());
        assert!(parse_hex_bytes("   ").is_err());
        assert!(parse_hex_bytes(" , , ").is_err());
    }

    #[test]
    fn parse_rejects_non_hex() {
        let err = parse_hex_bytes("1B XY 0D").unwrap_err();
        assert!(err.contains("XY"), "{err}");
    }

    #[test]
    fn parse_rejects_too_long_piece() {
        let err = parse_hex_bytes("1B0D").unwrap_err();
        assert!(err.contains("more than 2"), "{err}");
    }

    // ── parse_hex_units ───────────────────────────────────────────────────────

    #[test]
    fn parse_units_single_and_multiple() {
        assert_eq!(parse_hex_units("0E16").unwrap(), vec![0x0E16]);
        assert_eq!(parse_hex_units("0E16 1F62").unwrap(), vec![0x0E16, 0x1F62]);
        assert_eq!(parse_hex_units("0E16,1F62").unwrap(), vec![0x0E16, 0x1F62]);
        assert_eq!(
            parse_hex_units("  0E16,  1F62  ").unwrap(),
            vec![0x0E16, 0x1F62]
        );
    }

    #[test]
    fn parse_units_rejects_wrong_length() {
        // 3 digits, 5 digits, and a missing space between two units.
        for input in ["E16", "01F62", "0E161F62"] {
            let err = parse_hex_units(input).unwrap_err();
            assert!(err.contains("exactly 4 hex digits"), "{input}: {err}");
        }
    }

    #[test]
    fn parse_units_rejects_non_hex() {
        let err = parse_hex_units("XYZW").unwrap_err();
        assert!(err.contains("XYZW"), "{err}");
    }

    #[test]
    fn parse_units_rejects_empty() {
        assert!(parse_hex_units("").is_err());
        assert!(parse_hex_units("   ").is_err());
        assert!(parse_hex_units(", ,").is_err());
    }

    // ── char_to_byte / byte_to_char ───────────────────────────────────────────

    #[test]
    fn char_byte_round_trip_ascii() {
        let s = "hello";
        for i in 0..=s.len() {
            assert_eq!(byte_to_char(s, char_to_byte(s, i)), i.min(5));
        }
    }

    #[test]
    fn char_byte_handles_multibyte() {
        // 'A' (1 byte/char) + '‹' (3 bytes/1 char) + 'B' (1 byte/char)
        let s = "A\u{2039}B";
        assert_eq!(char_to_byte(s, 0), 0);
        assert_eq!(char_to_byte(s, 1), 1);
        assert_eq!(char_to_byte(s, 2), 4);
        assert_eq!(char_to_byte(s, 3), 5); // saturates to len
        assert_eq!(byte_to_char(s, 0), 0);
        assert_eq!(byte_to_char(s, 1), 1);
        assert_eq!(byte_to_char(s, 4), 2);
        assert_eq!(byte_to_char(s, 5), 3);
        assert_eq!(byte_to_char(s, 99), 3); // clamps past end
    }

    // ── invalid_ipv4 ──────────────────────────────────────────────────────────

    #[test]
    fn ipv4_empty_is_not_invalid() {
        assert!(!invalid_ipv4(""));
    }

    #[test]
    fn ipv4_partial_typing_is_not_invalid() {
        // The user is mid-typing; don't flash red yet.
        for s in [
            "1",
            "19",
            "192",
            "192.",
            "192.168",
            "192.168.1",
            "192.168.1.",
        ] {
            assert!(!invalid_ipv4(s), "{s:?} should be treated as partial");
        }
    }

    #[test]
    fn ipv4_complete_valid_is_not_invalid() {
        for s in ["0.0.0.0", "192.168.1.5", "255.255.255.255"] {
            assert!(!invalid_ipv4(s), "{s:?} parses as Ipv4Addr");
        }
    }

    #[test]
    fn ipv4_garbage_chars_are_invalid() {
        for s in ["abc", "192.168.1.a", "192-168-1-5", "192.168.1.5 "] {
            assert!(invalid_ipv4(s), "{s:?} contains non-IPv4 characters");
        }
    }

    #[test]
    fn ipv4_too_many_parts_or_out_of_range_is_invalid() {
        for s in ["192.168.1.5.6", "192.168.1.300", "1..2.3.4"] {
            assert!(invalid_ipv4(s), "{s:?} can never be a valid Ipv4Addr");
        }
    }
}
