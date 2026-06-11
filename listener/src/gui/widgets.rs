//! Stateless presentation helpers for the GUI: small reusable widgets, the serial
//! option tables, the interface-config editor, and pure formatting/utility
//! functions. Nothing here holds application state — the `ListenerApp` panels in
//! the parent module call into these.

use std::path::PathBuf;

use crate::config::{
    templates, ChannelConfig, DataBits, ExtractionConfig, FlowControl, InterfaceConfig, Parity,
    StopBits,
};
use crate::core::ChannelId;
use crate::record::{FileRotationPolicy, OverwritePolicy, RecordingMode};
use crate::transport::udp::UdpMode;

use super::fonts::bold;
use super::state::ChannelStatus;

/// The stroke for a channel box border — talker's connection-card color.
pub(super) const BOX_STROKE: egui::Color32 = egui::Color32::from_rgb(140, 160, 200);

/// Which interface a new channel uses, in the add-channel form.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AddKind {
    Udp,
    Tcp,
    Serial,
}

/// Which data the viewer renders (spec §41 `DisplaySource`): the verbatim
/// pre-extraction wire stream, or the extracted/decoded Messages (ADR-009).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum DisplaySource {
    /// The verbatim byte stream as received (no Message boundaries, §18). Default.
    #[default]
    Stream,
    /// Completed Messages: numbered, optionally timestamped and protocol-tagged.
    Messages,
}

/// A simple preset color scheme for the message view (#6 — simpler than a picker).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ColorScheme {
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

/// The framing method shown in the Configure selector — the GUI face of the core
/// [`ExtractionConfig`] (§20). `Protocol` is presented as `Delimiter`, since it
/// realizes as CRLF-delimiter framing in v1 (§23).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FramingMethod {
    Stream,
    Delimiter,
    Fixed,
}

impl FramingMethod {
    fn of(config: &ExtractionConfig) -> Self {
        match config {
            ExtractionConfig::Stream => FramingMethod::Stream,
            ExtractionConfig::FixedLength { .. } => FramingMethod::Fixed,
            ExtractionConfig::Delimiter { .. } => FramingMethod::Delimiter,
        }
    }

    /// A sensible default config when switching *to* this method.
    fn default_config(self) -> ExtractionConfig {
        match self {
            FramingMethod::Stream => ExtractionConfig::Stream,
            // CRLF, delimiter stripped from the payload — the NMEA convention (§24).
            FramingMethod::Delimiter => ExtractionConfig::Delimiter {
                delimiter: vec![b'\r', b'\n'],
                include_delimiter: false,
            },
            FramingMethod::Fixed => ExtractionConfig::FixedLength {
                length: 256,
                sync_marker: None,
            },
        }
    }
}

/// Common delimiter presets (label, bytes) for the framing selector.
const DELIMITERS: &[(&str, &[u8])] = &[
    ("CRLF (\\r\\n)", b"\r\n"),
    ("LF (\\n)", b"\n"),
    ("CR (\\r)", b"\r"),
    ("NUL (\\0)", b"\0"),
];

/// Edit how the byte stream is split into Messages (§20): the framing method and
/// its parameters. This is what *defines a Message* for the channel — without it
/// (Stream), the Messages display source and Message-framed recording produce
/// nothing. Applied via a §13 Reconfigure, like the rest of the config.
fn edit_extraction(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    // UDP self-frames: each datagram is one Message and bypasses extraction (§15),
    // so the method here would have no effect — show the fact instead of a control.
    if matches!(config.interface, InterfaceConfig::Udp(_)) {
        ui.horizontal(|ui| {
            ui.label(bold("Framing"));
            ui.label(egui::RichText::new("one Message per datagram (UDP)").weak())
                .on_hover_text(
                    "UDP preserves datagram boundaries, so each datagram is a Message (§15)",
                );
        });
        return;
    }
    ui.horizontal(|ui| {
        ui.label(bold("Framing"));
        let mut method = FramingMethod::of(&config.extraction);
        let before = method;
        ui.radio_value(&mut method, FramingMethod::Stream, "Stream")
            .on_hover_text("No Message boundaries — bytes only (Stream source, §18)");
        ui.radio_value(&mut method, FramingMethod::Delimiter, "Delimiter")
            .on_hover_text("Split on a byte sequence (CRLF for NMEA, §23)");
        ui.radio_value(&mut method, FramingMethod::Fixed, "Fixed length")
            .on_hover_text("Every N bytes is one Message");
        // Only rewrite the config when the method actually changes, so editing a
        // method's parameters below isn't clobbered each frame.
        if method != before {
            config.extraction = method.default_config();
        }
    });
    match &mut config.extraction {
        ExtractionConfig::Delimiter {
            delimiter,
            include_delimiter,
        } => {
            ui.horizontal(|ui| {
                ui.label("Delimiter");
                let selected = DELIMITERS
                    .iter()
                    .find(|(_, b)| *b == delimiter.as_slice())
                    .map_or("Custom", |(name, _)| name);
                egui::ComboBox::from_id_salt("delimiter")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for (name, bytes) in DELIMITERS {
                            ui.selectable_value(delimiter, bytes.to_vec(), *name);
                        }
                    });
                ui.checkbox(include_delimiter, "keep delimiter")
                    .on_hover_text("Include the delimiter bytes in the Message payload");
            });
        }
        ExtractionConfig::FixedLength { length, .. } => {
            ui.horizontal(|ui| {
                ui.label("Length");
                ui.add(
                    egui::DragValue::new(length)
                        .range(1..=65_536)
                        .suffix(" bytes"),
                );
            });
        }
        _ => {}
    }
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

    // Framing first (§20): how the byte stream is split into Messages — it *defines*
    // what a Message is for this channel, so the Messages display source and any
    // Message-framed recording have something to produce.
    edit_extraction(ui, config);

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
    ui.separator();
    edit_recording(ui, config);
    refresh
}

/// Edit the channel's recording (§51–§59): write received data to a file. Mode
/// picks the system — byte-exact Raw `.dat` (§53) or rendered Display `.disp`
/// (§54). The rest sets the destination, overwrite handling, and time rotation.
/// Config-driven: applied via a §13 Reconfigure, so Apply & Restart begins it.
fn edit_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.recording;
    ui.horizontal(|ui| {
        ui.label(bold("Recording"));
        ui.radio_value(&mut rec.mode, RecordingMode::Disabled, "Off");
        ui.radio_value(&mut rec.mode, RecordingMode::Raw, "Raw (.dat)")
            .on_hover_text("Byte-exact, exactly as received — pre-extraction (§53)");
        ui.radio_value(&mut rec.mode, RecordingMode::Display, "Display (.disp)")
            .on_hover_text("The rendered view output (§54)");
    });
    if rec.mode == RecordingMode::Disabled {
        return;
    }
    // A single file when not rotating; a directory of <channel>_<period> files
    // otherwise (§59). `rotating` reflects this frame's start — a one-frame lag
    // when the user flips rotation below is harmless.
    let rotating = rec.file_rotation != FileRotationPolicy::None;
    ui.horizontal(|ui| {
        ui.label(if rotating { "Folder" } else { "File" });
        let mut path = rec
            .destination
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if ui
            .add(egui::TextEdit::singleline(&mut path).desired_width(260.0))
            .changed()
        {
            let trimmed = path.trim();
            rec.destination = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        }
        if ui.button("Browse…").clicked() {
            let picked = if rotating {
                rfd::FileDialog::new().pick_folder()
            } else {
                rfd::FileDialog::new().save_file()
            };
            if let Some(p) = picked {
                rec.destination = Some(p);
            }
        }
    });
    if rec.destination.is_none() {
        ui.label(
            egui::RichText::new("⚠ set a destination — recording won't start without one")
                .color(egui::Color32::from_rgb(150, 100, 0)),
        );
    }
    ui.horizontal(|ui| {
        ui.label("On exists");
        ui.radio_value(&mut rec.overwrite_policy, OverwritePolicy::Refuse, "Refuse");
        ui.radio_value(
            &mut rec.overwrite_policy,
            OverwritePolicy::Overwrite,
            "Overwrite",
        );
        ui.radio_value(
            &mut rec.overwrite_policy,
            OverwritePolicy::AppendIfExists,
            "Append",
        );
    });
    ui.horizontal(|ui| {
        ui.label("Rotate");
        ui.radio_value(&mut rec.file_rotation, FileRotationPolicy::None, "None");
        ui.radio_value(&mut rec.file_rotation, FileRotationPolicy::Hourly, "Hourly");
        ui.radio_value(&mut rec.file_rotation, FileRotationPolicy::Daily, "Daily")
            .on_hover_text(
                "Rotating files are named <channel>_<period> — keep the channel name \
                 filesystem-safe (§59)",
            );
    });
    ui.checkbox(&mut rec.timestamp_enabled, "Record timestamps")
        .on_hover_text("Sidecar index for Raw; inline for Display (§57)");
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

/// One step of an "Apply & Start/Restart" config commit, in dispatch order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommitStep {
    Stop,
    Reconfigure,
    Start,
}

/// Plan an "Apply & Start/Restart": the ordered commands to install the edited
/// config and bring the channel up, given its current state. `None` means the
/// config can't start (e.g. no port) — the caller complains and changes nothing.
///
/// Pure (no egui, no bridge) so the state-machine sequencing is unit-tested. Start
/// must be a legal `Stopped → Running` transition (§8.5), so anything currently up
/// (Running) or not cleanly stoppable (Faulted/Reconnecting) is Stopped first.
pub(super) fn plan_config_commit(
    status: ChannelStatus,
    config_incomplete: bool,
) -> Option<Vec<CommitStep>> {
    if config_incomplete {
        return None;
    }
    let mut steps = Vec::new();
    if matches!(
        status,
        ChannelStatus::Running | ChannelStatus::Faulted | ChannelStatus::Reconnecting
    ) {
        steps.push(CommitStep::Stop);
    }
    steps.push(CommitStep::Reconfigure);
    steps.push(CommitStep::Start);
    Some(steps)
}

/// The primary lifecycle action offered for a channel in its current state — the
/// label and meaning of the big action button. `Retry` is Stop-then-Start (a
/// Faulted channel can't Start directly, §8.5). Pure, so it's unit-tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LifecycleAction {
    Stop,
    Retry,
    Start,
}

impl LifecycleAction {
    pub(super) fn from_status(status: ChannelStatus) -> Self {
        match status {
            ChannelStatus::Running => LifecycleAction::Stop,
            ChannelStatus::Faulted => LifecycleAction::Retry,
            _ => LifecycleAction::Start,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            LifecycleAction::Stop => "Stop",
            LifecycleAction::Retry => "Retry",
            LifecycleAction::Start => "Start",
        }
    }
}

/// The headline diagnostic for the real-time status line: the most recent error,
/// else the most recent warning, else the most recent event, with its display color.
/// Errors win so a fault stays visible in the collapsed header while troubleshooting.
pub(super) fn latest_diagnostic(
    diag: &crate::runtime::snapshot::DiagnosticsSnapshot,
) -> (String, egui::Color32) {
    if let Some(d) = diag.errors.last() {
        (d.message.clone(), egui::Color32::from_rgb(170, 30, 30))
    } else if let Some(d) = diag.warnings.last() {
        (d.message.clone(), egui::Color32::from_rgb(150, 100, 0))
    } else if let Some(d) = diag.events.last() {
        (d.message.clone(), egui::Color32::from_gray(60))
    } else {
        ("no activity yet".to_string(), egui::Color32::from_gray(120))
    }
}

/// Truncate a one-line status to `max` characters (on a char boundary), adding an
/// ellipsis when shortened — keeps the collapsed diagnostics header tidy.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}\u{2026}")
    }
}

/// Format a byte count compactly in SI units (kB = 1000 B, MB = 1000 kB, …) for the
/// stream liveness readouts (ADR-009): the Channel list rows and the detail header.
pub(super) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.3} {}", UNITS[u])
    }
}

/// A short status word for the detail pane.
pub(super) fn status_label(status: ChannelStatus) -> &'static str {
    match status {
        ChannelStatus::Stopped => "stopped",
        ChannelStatus::Running => "running",
        ChannelStatus::Faulted => "faulted",
        ChannelStatus::Reconnecting => "reconnecting",
    }
}

/// A serial control-line indicator (§161): the line name colored green when the
/// line is high (asserted), grey when low, with a hover tooltip.
pub(super) fn line_indicator(ui: &mut egui::Ui, name: &str, high: bool) {
    let color = if high {
        egui::Color32::from_rgb(30, 150, 30)
    } else {
        egui::Color32::from_gray(150)
    };
    ui.colored_label(color, name)
        .on_hover_text(if high { "high" } else { "low" });
}

/// A clickable serial output-line toggle (RTS/DTR, §161): a selectable chip,
/// highlighted and green when the line is asserted (high). Returns the click
/// response so the caller can send the matching Set command.
pub(super) fn line_toggle(ui: &mut egui::Ui, name: &str, high: bool) -> egui::Response {
    let color = if high {
        egui::Color32::from_rgb(30, 150, 30)
    } else {
        ui.visuals().weak_text_color()
    };
    ui.selectable_label(high, egui::RichText::new(name).color(color))
        .on_hover_text(format!(
            "{name} output is {} — click to set it {}",
            if high { "high" } else { "low" },
            if high { "low" } else { "high" },
        ))
}

/// The color for a status glyph: green running, grey stopped, red faulted, amber
/// reconnecting.
pub(super) fn status_color(status: ChannelStatus) -> egui::Color32 {
    match status {
        ChannelStatus::Running => egui::Color32::from_rgb(30, 150, 30),
        ChannelStatus::Stopped => egui::Color32::from_gray(120),
        ChannelStatus::Faulted => egui::Color32::from_rgb(190, 40, 40),
        ChannelStatus::Reconnecting => egui::Color32::from_rgb(200, 140, 0),
    }
}

/// Shorten a UUID string to its first segment, enough to disambiguate at a glance.
pub(super) fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// A vertical divider at the standard control height. Unlike `ui.separator()` (which
/// stretches to the row height), this stays a fixed length, so a row containing an
/// over-tall element — e.g. the enlarged `␊` glyph — doesn't get a taller divider
/// than every other row.
pub(super) fn vsep(ui: &mut egui::Ui) {
    let h = ui.spacing().interact_size.y;
    let width = ui.spacing().item_spacing.x.max(6.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    ui.painter().vline(rect.center().x, rect.y_range(), stroke);
}

#[cfg(test)]
mod tests {
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
    fn config_commit_sequences_by_state() {
        use CommitStep::*;
        // Running = a real restart: Stop → Reconfigure → Start.
        assert_eq!(
            plan_config_commit(ChannelStatus::Running, false),
            Some(vec![Stop, Reconfigure, Start])
        );
        // Stopped just installs + starts (nothing to stop).
        assert_eq!(
            plan_config_commit(ChannelStatus::Stopped, false),
            Some(vec![Reconfigure, Start])
        );
        // Faulted/Reconnecting must be Stopped first so Start is legal (§8.5).
        for s in [ChannelStatus::Faulted, ChannelStatus::Reconnecting] {
            assert_eq!(
                plan_config_commit(s, false),
                Some(vec![Stop, Reconfigure, Start]),
                "{s:?}"
            );
        }
        // An incomplete config can't start in any state → refuse (None).
        for s in [
            ChannelStatus::Running,
            ChannelStatus::Stopped,
            ChannelStatus::Faulted,
            ChannelStatus::Reconnecting,
        ] {
            assert_eq!(plan_config_commit(s, true), None, "{s:?}");
        }
    }

    #[test]
    fn lifecycle_action_maps_state_to_button() {
        assert_eq!(
            LifecycleAction::from_status(ChannelStatus::Running),
            LifecycleAction::Stop
        );
        assert_eq!(
            LifecycleAction::from_status(ChannelStatus::Faulted),
            LifecycleAction::Retry
        );
        assert_eq!(
            LifecycleAction::from_status(ChannelStatus::Stopped),
            LifecycleAction::Start
        );
        assert_eq!(
            LifecycleAction::from_status(ChannelStatus::Reconnecting),
            LifecycleAction::Start
        );
        assert_eq!(LifecycleAction::Stop.label(), "Stop");
        assert_eq!(LifecycleAction::Retry.label(), "Retry");
        assert_eq!(LifecycleAction::Start.label(), "Start");
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
