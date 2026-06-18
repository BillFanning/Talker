//! Stateless presentation helpers for the GUI: small reusable widgets, the serial
//! option tables, the interface-config editor, and pure formatting/utility
//! functions. Nothing here holds application state — the `ListenerApp` panels in
//! the parent module call into these. Grouped by responsibility into submodules,
//! re-exported flat so callers keep using `widgets::<name>`.

mod format;
mod recording_editor;
mod status;

pub(super) use format::{human_bytes, short_id, truncate};
pub(super) use recording_editor::{edit_display_recording, edit_raw_recording};
pub(super) use status::{
    latest_diagnostic, line_indicator, line_toggle, paint_glyph, recording_glyph_size,
    recording_indicator, start_button, status_color, status_glyph, status_label, stop_enabled,
};

use crate::config::{
    templates, ChannelConfig, DataBits, FlowControl, InterfaceConfig, Parity, StopBits,
};
use crate::core::ChannelId;
use crate::transport::udp::UdpMode;

use super::fonts::bold;

/// Which interface a new channel uses, in the add-channel form.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AddKind {
    Udp,
    Tcp,
    Serial,
}

/// A simple preset color scheme for the message view (#6 — simpler than a picker).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorScheme {
    BlackOnWhite,
    GreenOnBlack,
    AmberOnBlack,
    WhiteOnBlack,
}

impl ColorScheme {
    pub(super) fn label(self) -> &'static str {
        match self {
            ColorScheme::BlackOnWhite => "Black on white",
            ColorScheme::GreenOnBlack => "Green on black",
            ColorScheme::AmberOnBlack => "Amber on black",
            ColorScheme::WhiteOnBlack => "White on black",
        }
    }
    pub(super) fn fg(self) -> egui::Color32 {
        match self {
            ColorScheme::BlackOnWhite => egui::Color32::from_gray(20),
            ColorScheme::GreenOnBlack => egui::Color32::from_rgb(60, 230, 60),
            ColorScheme::AmberOnBlack => egui::Color32::from_rgb(255, 190, 70),
            ColorScheme::WhiteOnBlack => egui::Color32::from_gray(235),
        }
    }
    pub(super) fn bg(self) -> egui::Color32 {
        match self {
            ColorScheme::BlackOnWhite => egui::Color32::from_gray(252),
            _ => egui::Color32::from_gray(16),
        }
    }

    /// A stable token for persisting the preset in a profile. The scheme bundles fg+bg,
    /// so it round-trips as one token (stored in `DisplayViewConfig.foreground_color`)
    /// rather than two free color strings. Pairs with [`from_name`](Self::from_name).
    pub(super) fn name(self) -> &'static str {
        match self {
            ColorScheme::BlackOnWhite => "black_on_white",
            ColorScheme::GreenOnBlack => "green_on_black",
            ColorScheme::AmberOnBlack => "amber_on_black",
            ColorScheme::WhiteOnBlack => "white_on_black",
        }
    }

    /// Parse a persisted token back to a preset; unknown/None falls back to the default
    /// (BlackOnWhite).
    pub(super) fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("green_on_black") => ColorScheme::GreenOnBlack,
            Some("amber_on_black") => ColorScheme::AmberOnBlack,
            Some("white_on_black") => ColorScheme::WhiteOnBlack,
            _ => ColorScheme::BlackOnWhite,
        }
    }
}

/// Font sizes offered in the message-view size dropdown (#5).
pub(super) const MSG_FONT_SIZES: &[f32] = &[
    8.0, 10.0, 11.0, 12.0, 13.0, 14.0, 16.0, 18.0, 20.0, 24.0, 28.0, 36.0, 48.0, 72.0,
];

/// Serial option tables (value, label) for the radio rows (§74).
const DATA_BITS: &[(DataBits, &str)] = &[
    (DataBits::Five, "5"),
    (DataBits::Six, "6"),
    (DataBits::Seven, "7"),
    (DataBits::Eight, "8"),
];
const PARITY: &[(Parity, &str)] = &[
    (Parity::None, "None"),
    (Parity::Even, "Even"),
    (Parity::Odd, "Odd"),
    (Parity::Mark, "Mark"),
    (Parity::Space, "Space"),
];
const STOP_BITS: &[(StopBits, &str)] = &[
    (StopBits::One, "1"),
    (StopBits::OnePointFive, "1.5"),
    (StopBits::Two, "2"),
];
/// Flow control: None first, RTS/CTS last (§74).
const FLOW_CONTROL: &[(FlowControl, &str)] = &[
    (FlowControl::None, "None"),
    (FlowControl::XonXoff, "Xon/Xoff"),
    (FlowControl::RtsCts, "RTS/CTS"),
];
/// Common serial baud rates for the baud radio row (§14.4).
const BAUD_RATES: &[u32] = &[4800, 9600, 19200, 38400, 57600, 115200, 230400];

/// A labelled row of radio buttons bound to an enum value with a fixed option table.
fn radio_row<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
    options: &[(T, &str)],
) {
    ui.horizontal(|ui| {
        ui.label(bold(label));
        for (v, s) in options {
            ui.radio_value(value, *v, *s);
        }
    });
}

/// An incrementing port box (talker-style): `[−] [text] [+]`, no drag-to-increment
/// or resize cursor. The text field parses on change; the buttons step by one. A
/// port of 0 shows empty (a fresh channel has no port yet, #3).
fn port_field(ui: &mut egui::Ui, id: &str, port: &mut u16) {
    ui.horizontal(|ui| {
        if ui.small_button("\u{2212}").clicked() {
            *port = port.saturating_sub(1);
        }
        let mut text = if *port == 0 {
            String::new()
        } else {
            port.to_string()
        };
        if ui
            .add(
                egui::TextEdit::singleline(&mut text)
                    .id_salt(id)
                    .desired_width(64.0),
            )
            .changed()
        {
            if let Ok(value) = text.trim().parse::<u16>() {
                *port = value;
            }
        }
        if ui.small_button("+").clicked() {
            *port = port.saturating_add(1);
        }
    });
}

/// Edit a channel's interface config in place, laid out like talker (§74/§75).
/// Presentation only — applied via a §13 Reconfigure. Returns whether the serial
/// port list should be refreshed (the ⟳ button was clicked). `channel_id` keys the
/// per-channel custom-baud text buffer so it survives frames and resets on switch.
pub(super) fn edit_interface(
    ui: &mut egui::Ui,
    channel_id: ChannelId,
    config: &mut ChannelConfig,
    serial_ports: &[String],
) -> bool {
    let mut refresh = false;

    ui.separator();

    match &mut config.interface {
        InterfaceConfig::Udp(udp) => {
            // A 2-column grid (label | controls) keeps the address/port columns
            // aligned, so they don't shift left/right when the mode changes.
            egui::Grid::new("udp_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label(bold("Mode"));
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut udp.mode, UdpMode::Broadcast, "Broadcast");
                        ui.radio_value(&mut udp.mode, UdpMode::Unicast, "Unicast");
                        ui.radio_value(&mut udp.mode, UdpMode::Multicast, "Multicast");
                    });
                    ui.end_row();

                    // Binding address + port — always directly under Mode, so it
                    // never moves when switching modes (#2, #3). The multicast Group
                    // row appears *below* it.
                    ui.label(bold("Binding address"));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut udp.bind_address)
                                .id_salt("udp_bind")
                                .desired_width(130.0)
                                .hint_text("0.0.0.0"),
                        );
                        ui.label("port");
                        port_field(ui, "udp_port", &mut udp.port);
                    });
                    ui.end_row();

                    if udp.mode == UdpMode::Multicast {
                        ui.label(bold("Group"));
                        let mut group = udp.multicast_group.clone().unwrap_or_default();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut group)
                                    .id_salt("udp_group")
                                    .desired_width(130.0)
                                    .hint_text("239.0.0.1"),
                            )
                            .changed()
                        {
                            let g = group.trim();
                            udp.multicast_group = (!g.is_empty()).then(|| g.to_string());
                        }
                        ui.end_row();
                    }
                });
        }
        InterfaceConfig::TcpListener(tcp) => {
            egui::Grid::new("tcp_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label(bold("Binding address"));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut tcp.bind_address)
                                .id_salt("tcp_bind")
                                .desired_width(130.0)
                                .hint_text("0.0.0.0"),
                        );
                        ui.label("port");
                        port_field(ui, "tcp_port", &mut tcp.port);
                    });
                    ui.end_row();
                });
        }
        InterfaceConfig::Serial(serial) => {
            // Port dropdown + refresh.
            ui.horizontal(|ui| {
                ui.label(bold("Port"));
                let label = if serial.port.is_empty() {
                    "select port\u{2026}".to_string()
                } else {
                    serial.port.clone()
                };
                egui::ComboBox::from_id_salt("serial_port")
                    .selected_text(label)
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        if serial_ports.is_empty() {
                            ui.weak("No ports found");
                        } else {
                            for port in serial_ports {
                                ui.selectable_value(&mut serial.port, port.clone(), port);
                            }
                        }
                    });
                if ui
                    .small_button("\u{2B6E}")
                    .on_hover_text("Refresh port list")
                    .clicked()
                {
                    refresh = true;
                }
            });
            // Baud — radios for common rates plus a free-entry box for any custom
            // rate. The box keeps its own buffer (egui temp memory, keyed per
            // channel) so typing isn't clobbered each frame: the old
            // re-derive-from-state box cleared itself the instant a standard rate was
            // typed, and couldn't hold a custom one. A radio click resyncs the box;
            // otherwise the typed text wins.
            let buf_id = egui::Id::new(("baud_custom_text", channel_id));
            let mut text = ui
                .data(|d| d.get_temp::<String>(buf_id))
                .unwrap_or_else(|| serial.baud_rate.to_string());
            ui.horizontal(|ui| {
                ui.label(bold("Baud"));
                let before = serial.baud_rate;
                for &baud in BAUD_RATES {
                    ui.radio_value(&mut serial.baud_rate, baud, baud.to_string());
                }
                if serial.baud_rate != before {
                    // A radio set the rate — mirror it into the custom box.
                    text = serial.baud_rate.to_string();
                }
                ui.label("custom");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut text)
                            .id_salt("baud_custom")
                            .desired_width(80.0)
                            .hint_text("e.g. 250000"),
                    )
                    .changed()
                {
                    if let Ok(baud) = text.trim().parse::<u32>() {
                        if baud > 0 {
                            serial.baud_rate = baud;
                        }
                    }
                }
            });
            ui.data_mut(|d| d.insert_temp(buf_id, text));
            radio_row(ui, "Data bits", &mut serial.data_bits, DATA_BITS);
            radio_row(ui, "Parity", &mut serial.parity, PARITY);
            radio_row(ui, "Stop bits", &mut serial.stop_bits, STOP_BITS);
            radio_row(ui, "Flow", &mut serial.flow_control, FLOW_CONTROL);
        }
    }
    refresh
}

/// The available serial port names, sorted (§14.4). Empty if enumeration fails.
pub(super) fn list_serial_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|list| list.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    ports.sort();
    ports
}

/// A fresh template config for the given interface kind. New UDP channels default
/// to Broadcast (the first/most-common option for this tool).
pub(super) fn template_for(kind: AddKind) -> ChannelConfig {
    match kind {
        AddKind::Udp => {
            let mut config = templates::udp_template();
            if let InterfaceConfig::Udp(udp) = &mut config.interface {
                udp.mode = UdpMode::Broadcast;
            }
            config
        }
        AddKind::Tcp => templates::tcp_listener_template(),
        AddKind::Serial => templates::serial_template(),
    }
}

/// Whether a channel's config is missing the minimum needed to start: no serial
/// port, or a 0 bind port for UDP/TCP. Drives auto-opening the Configure section
/// when such a channel takes focus (task 1).
pub(super) fn config_incomplete(config: &ChannelConfig) -> bool {
    match &config.interface {
        InterfaceConfig::Udp(u) => u.port == 0,
        InterfaceConfig::TcpListener(t) => t.port == 0,
        InterfaceConfig::Serial(s) => s.port.trim().is_empty(),
    }
}

/// Whether an edited config draft differs from the channel's committed config in any
/// way that needs an Apply & Restart — i.e. everything **except** the name, which
/// renames live without a restart (§6). Used to switch the Start button to
/// "Apply & Restart" while a running channel has pending edits. Pure / unit-tested.
pub(super) fn config_differs_ignoring_name(
    draft: &ChannelConfig,
    committed: &ChannelConfig,
) -> bool {
    let mut a = draft.clone();
    a.name = committed.name.clone(); // neutralize the name before comparing
    &a != committed
}

#[cfg(test)]
mod tests {
    use super::super::state::ChannelStatus;
    use super::*;
    use crate::diagnostics::Diagnostic;
    use crate::runtime::snapshot::DiagnosticsSnapshot;

    #[test]
    fn truncate_keeps_short_strings_and_clips_long_ones_on_char_boundaries() {
        assert_eq!(truncate("short", 70), "short");
        assert_eq!(truncate("abcdef", 4), "abc\u{2026}");
        // Multibyte input must not panic or split a char.
        let s = "ééééééé"; // 7 two-byte chars
        let out = truncate(s, 4);
        assert_eq!(out.chars().count(), 4); // 3 kept + ellipsis
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn config_incomplete_flags_missing_port_or_serial_device() {
        // A real port → complete; a 0 port (the fresh template default) → incomplete.
        let mut udp = templates::udp_template();
        if let InterfaceConfig::Udp(u) = &mut udp.interface {
            u.port = 18180;
        }
        assert!(!config_incomplete(&udp));
        if let InterfaceConfig::Udp(u) = &mut udp.interface {
            u.port = 0;
        }
        assert!(config_incomplete(&udp));

        // A fresh serial template has no port selected → incomplete.
        let serial = templates::serial_template();
        assert!(config_incomplete(&serial));
    }

    #[test]
    fn start_button_reflects_state_and_pending_edits() {
        // Stopped → plain Start, enabled.
        assert_eq!(
            start_button(ChannelStatus::Stopped, false),
            ("Start Channel", true)
        );
        // Running with no edits → disabled (nothing to apply).
        assert_eq!(
            start_button(ChannelStatus::Running, false),
            ("Start Channel", false)
        );
        // Running with pending edits → Apply & Restart, enabled.
        assert_eq!(
            start_button(ChannelStatus::Running, true),
            ("Apply & Restart", true)
        );
        // Faulted → Retry, enabled (config_changed irrelevant).
        assert_eq!(
            start_button(ChannelStatus::Faulted, false),
            ("Retry Channel", true)
        );
        assert_eq!(
            start_button(ChannelStatus::Faulted, true),
            ("Retry Channel", true)
        );
        // Reconnecting → disabled: a Start would be an illegal Reconnecting→Starting.
        assert_eq!(
            start_button(ChannelStatus::Reconnecting, false),
            ("Start Channel", false)
        );
    }

    #[test]
    fn stop_enabled_only_for_stoppable_states() {
        // Stop is legal from Running/Faulted/Reconnecting, illegal from Stopped.
        assert!(stop_enabled(ChannelStatus::Running));
        assert!(stop_enabled(ChannelStatus::Faulted));
        assert!(stop_enabled(ChannelStatus::Reconnecting));
        assert!(!stop_enabled(ChannelStatus::Stopped));
    }

    #[test]
    fn recording_indicator_maps_state_to_label() {
        use crate::core::RecordingState;
        assert_eq!(
            recording_indicator(Some(RecordingState::Enabled)).2,
            "recording"
        );
        assert_eq!(
            recording_indicator(Some(RecordingState::Faulted)).2,
            "faulted"
        );
        assert_eq!(recording_indicator(Some(RecordingState::Disabled)).2, "off");
        assert_eq!(recording_indicator(None).2, "off");
    }

    #[test]
    fn config_change_detection_ignores_the_name() {
        let base = templates::udp_template();
        // Identical config = no change.
        assert!(!config_differs_ignoring_name(&base, &base));
        // A name-only difference is NOT a restart-worthy change (renames live, §6).
        let mut renamed = base.clone();
        renamed.name = crate::core::ChannelName::new("different");
        assert!(!config_differs_ignoring_name(&renamed, &base));
        // A real config edit (recording destination) IS a change.
        let mut edited = base.clone();
        edited.raw_recording.destination = Some(std::path::PathBuf::from("/tmp/x.raw"));
        assert!(config_differs_ignoring_name(&edited, &base));
    }

    #[test]
    fn latest_diagnostic_headlines_errors_then_warnings_then_events() {
        let mut diag = DiagnosticsSnapshot::default();
        assert_eq!(latest_diagnostic(&diag).0, "no activity yet");

        diag.events.push(Diagnostic::event("connected"));
        assert_eq!(latest_diagnostic(&diag).0, "connected");

        diag.warnings.push(Diagnostic::warning("checksum"));
        assert_eq!(latest_diagnostic(&diag).0, "checksum");

        diag.errors.push(Diagnostic::error("bind failed"));
        assert_eq!(latest_diagnostic(&diag).0, "bind failed");
        // The most recent error wins over earlier ones.
        diag.errors.push(Diagnostic::error("port lost"));
        assert_eq!(latest_diagnostic(&diag).0, "port lost");
    }
}
