//! Stateless presentation helpers for the GUI: small reusable widgets, the serial
//! option tables, the interface-config editor, and pure formatting/utility
//! functions. Nothing here holds application state — the `ListenerApp` panels in
//! the parent module call into these.

use std::path::PathBuf;

use crate::config::{
    templates, ChannelConfig, DataBits, FlowControl, InterfaceConfig, Parity, StopBits,
};
use crate::core::ChannelId;
use crate::record::{FileRotationPolicy, OverwritePolicy};
use crate::transport::udp::UdpMode;

use super::fonts::bold;
use super::state::ChannelStatus;
use super::theme;

/// Which interface a new channel uses, in the add-channel form.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AddKind {
    Udp,
    Tcp,
    Serial,
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

/// The shared destination / overwrite / rotation / timestamp controls for a recording
/// (§55–§59), used by both the Raw and Display editors (their config structs carry the
/// same fields; ADR-013). `ext` is the file extension shown in hints (".raw"/".disp").
#[allow(clippy::too_many_arguments)]
fn recording_file_fields(
    ui: &mut egui::Ui,
    ext: &str,
    destination: &mut Option<PathBuf>,
    overwrite_policy: &mut OverwritePolicy,
    file_rotation: &mut FileRotationPolicy,
    timestamp_enabled: &mut bool,
    // When `Some`, a "Record at start" checkbox is shown on the same line, left of the
    // "Record timestamps" checkbox (Raw uses this; Display has its own enable above).
    record_at_start: Option<&mut bool>,
) {
    // A single file when not rotating; a directory of <channel>_<period> files
    // otherwise (§59). `rotating` reflects this frame's start — a one-frame lag when
    // the user flips rotation below is harmless.
    let rotating = *file_rotation != FileRotationPolicy::None;
    ui.horizontal(|ui| {
        ui.label(if rotating { "Folder" } else { "File" });
        let mut path = destination
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if ui
            .add(egui::TextEdit::singleline(&mut path).desired_width(180.0))
            .changed()
        {
            let trimmed = path.trim();
            *destination = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        }
        if ui.button("Browse…").clicked() {
            let picked = if rotating {
                rfd::FileDialog::new().pick_folder()
            } else {
                rfd::FileDialog::new().save_file()
            };
            if let Some(p) = picked {
                *destination = Some(p);
            }
        }
    });
    if destination.is_none() {
        ui.label(
            egui::RichText::new("⚠ set a destination — recording won't start without one")
                .color(theme::WARNING_AMBER),
        );
    }
    ui.horizontal(|ui| {
        ui.label("On exists");
        ui.radio_value(overwrite_policy, OverwritePolicy::Refuse, "Refuse");
        ui.radio_value(overwrite_policy, OverwritePolicy::Overwrite, "Overwrite");
        ui.radio_value(overwrite_policy, OverwritePolicy::AppendIfExists, "Append");
    });
    ui.horizontal(|ui| {
        ui.label("Rotate");
        ui.radio_value(file_rotation, FileRotationPolicy::None, "None");
        ui.radio_value(file_rotation, FileRotationPolicy::Hourly, "Hourly");
        ui.radio_value(file_rotation, FileRotationPolicy::Daily, "Daily")
            .on_hover_text(
                "Rotating files are named <channel>_<period> — keep the channel name \
                 filesystem-safe (§59)",
            );
    });
    ui.horizontal(|ui| {
        if let Some(enabled) = record_at_start {
            ui.checkbox(enabled, "Record at start")
                .on_hover_text("Begin recording when the channel starts (§53)");
        }
        ui.checkbox(timestamp_enabled, format!("Record timestamps ({ext})"))
            .on_hover_text("Sidecar index for Raw; inline for Display (§57)");
    });
}

/// Edit the channel's **Raw** recording setup (§53): destination, overwrite,
/// rotation, timestamps, and the "record at start" flag. The byte-exact verbatim
/// stream (§53) — a separate pipeline tap from Display (ADR-013). Config-driven:
/// applied via a §13 Reconfigure. The live Record toggle (ADR-012) begins/stops it at
/// runtime without a restart, as long as a destination is set.
pub(super) fn edit_raw_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.raw_recording;
    recording_file_fields(
        ui,
        ".raw",
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
        &mut rec.timestamp_enabled,
        Some(&mut rec.enabled), // "Record at start", shown left of "Record timestamps"
    );
}

/// Edit the channel's **Display** recording setup (§54): records the rendered view
/// output (`.disp`) — a separate pipeline tap from Raw (ADR-013). Config-driven:
/// applied via a §13 Reconfigure.
pub(super) fn edit_display_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.display_recording;
    ui.checkbox(&mut rec.enabled, "Record display output (.disp)")
        .on_hover_text("Record the rendered view, not the raw bytes (§54)");
    if !rec.enabled {
        return;
    }
    recording_file_fields(
        ui,
        ".disp",
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
        &mut rec.timestamp_enabled,
        None, // Display has its own enable checkbox above
    );
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

/// The Start/Apply/Retry button's label and enabled state, from the channel status
/// and whether the edit draft has pending changes (§8.5). Pure decision, unit-tested;
/// the detail pane just renders the result and dispatches on click:
/// - Stopped → "Start Channel", enabled (Stopped→Starting is legal)
/// - Running + pending edits → "Apply & Restart", enabled (a coordinated restart)
/// - Running + no edits → "Start Channel", **disabled** (nothing to do)
/// - Faulted → "Retry Channel", enabled (Stop then Start, §8.5)
/// - Reconnecting → "Start Channel", **disabled** — a Start would be an illegal
///   Reconnecting→Starting transition; Stop is the only valid action mid-reconnect.
pub(super) fn start_button(status: ChannelStatus, config_changed: bool) -> (&'static str, bool) {
    match status {
        ChannelStatus::Stopped => ("Start Channel", true),
        ChannelStatus::Running if config_changed => ("Apply & Restart", true),
        ChannelStatus::Running => ("Start Channel", false),
        ChannelStatus::Faulted => ("Retry Channel", true),
        ChannelStatus::Reconnecting => ("Start Channel", false),
    }
}

/// Whether the Stop button is enabled: a Stop is legal from Running, Faulted, or
/// Reconnecting (it returns any of those to Stopped, §8.5/§10.2), and illegal from
/// Stopped. Pure, unit-tested.
pub(super) fn stop_enabled(status: ChannelStatus) -> bool {
    matches!(
        status,
        ChannelStatus::Running | ChannelStatus::Faulted | ChannelStatus::Reconnecting
    )
}

/// The recording-state indicator: glyph, color, and label for a channel's raw
/// recording state (§53). Uses the **same symbol set as channel status**
/// ([`status_glyph`]) — `■` off, `●` recording, `⚠` faulted — so the two read
/// consistently; only the colors differ (recording uses its own red). Pure,
/// unit-tested; the detail pane renders it as a colored label sized via
/// [`recording_glyph_size`].
pub(super) fn recording_indicator(
    recording: Option<crate::core::RecordingState>,
) -> (&'static str, egui::Color32, &'static str) {
    use crate::core::RecordingState;
    match recording {
        Some(RecordingState::Enabled) => ("\u{25CF}", theme::FAULT_RED, "recording"),
        Some(RecordingState::Faulted) => ("\u{26A0}", theme::FAULT_RED, "faulted"),
        Some(RecordingState::Disabled) | None => ("\u{25A0}", theme::IDLE_GREY, "off"),
    }
}

/// The headline diagnostic for the real-time status line: the most recent error,
/// else the most recent warning, else the most recent event, with its display color.
/// Errors win so a fault stays visible in the collapsed header while troubleshooting.
pub(super) fn latest_diagnostic(
    diag: &crate::runtime::snapshot::DiagnosticsSnapshot,
) -> (String, egui::Color32) {
    if let Some(d) = diag.errors.last() {
        (d.message.clone(), theme::FAULT_RED)
    } else if let Some(d) = diag.warnings.last() {
        (d.message.clone(), theme::WARNING_AMBER)
    } else if let Some(d) = diag.events.last() {
        (d.message.clone(), theme::EVENT_GREY)
    } else {
        ("no activity yet".to_string(), theme::IDLE_GREY)
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
        theme::LINE_HIGH_GREEN
    } else {
        theme::LINE_LOW_GREY
    };
    ui.colored_label(color, name)
        .on_hover_text(if high { "high" } else { "low" });
}

/// A clickable serial output-line toggle (RTS/DTR, §161): a selectable chip,
/// highlighted and green when the line is asserted (high). Returns the click
/// response so the caller can send the matching Set command.
pub(super) fn line_toggle(ui: &mut egui::Ui, name: &str, high: bool) -> egui::Response {
    let color = if high {
        theme::LINE_HIGH_GREEN
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

/// The color for a status glyph. Running is a bright blue-green and Reconnecting a
/// yellower amber, both chosen to read distinctly from the red fault for red-green
/// color blindness (the distinct glyphs ●/■/⚠ are the primary signal; color reinforces).
/// All values live in [`super::theme`].
pub(super) fn status_color(status: ChannelStatus) -> egui::Color32 {
    match status {
        ChannelStatus::Running => theme::RUNNING_GREEN,
        ChannelStatus::Stopped => theme::IDLE_GREY,
        ChannelStatus::Faulted => theme::FAULT_RED,
        ChannelStatus::Reconnecting => theme::RECONNECTING_AMBER,
    }
}

/// The base size multiplier for status indicator glyphs (relative to body size). The
/// square (`■`) is the reference at this size; the dot and triangle are enlarged by
/// [`glyph_scale`] to match the square's apparent size.
pub(super) const STATUS_GLYPH_SCALE: f32 = 1.5;

/// Per-glyph optical correction: `●`/`■`/`⚠` have different bounding boxes, so at one
/// font size they look different sizes. The square is the reference (1.0); the dot and
/// triangle are enlarged so all three read the same size. Multiply into
/// [`STATUS_GLYPH_SCALE`].
fn glyph_scale(glyph: &str) -> f32 {
    match glyph {
        "\u{25A0}" => 1.0,  // ■ square — the reference
        "\u{25CF}" => 1.34, // ● dot — enlarge up to the square
        "\u{26A0}" => 1.30, // ⚠ triangle — enlarge up to the square
        _ => 1.0,
    }
}

/// The shared status symbol set + its size, used for BOTH channel-lifecycle and raw-
/// recording state so the two read consistently:
/// - Stopped / recording-off → `■`
/// - Running / recording-on → `●`
/// - Faulted (channel or recording) → `⚠`
/// - Reconnecting → `●`
///
/// Returns the glyph and the body-relative size (base scale × optical correction), so
/// every call site renders the same symbol at the same apparent size. Pair with
/// [`status_color`] (channel) or the recording color from [`recording_indicator`].
pub(super) fn status_glyph(status: ChannelStatus) -> (&'static str, f32) {
    let glyph = match status {
        ChannelStatus::Stopped => "\u{25A0}", // ■ square
        ChannelStatus::Faulted => "\u{26A0}", // ⚠ triangle
        ChannelStatus::Running | ChannelStatus::Reconnecting => "\u{25CF}", // ● dot
    };
    (glyph, STATUS_GLYPH_SCALE * glyph_scale(glyph))
}

/// The size for a recording-indicator glyph, matching [`status_glyph`]'s optical
/// sizing for the same symbol.
pub(super) fn recording_glyph_size(glyph: &str) -> f32 {
    STATUS_GLYPH_SCALE * glyph_scale(glyph)
}

/// Paint a status/recording `glyph` into a **fixed-size, non-interactive cell**,
/// centered. Painting (rather than adding a sized label) keeps the glyph from driving
/// the row height — a taller glyph otherwise shifts the line beside it. `allocate_space`
/// reserves only layout space with no widget id, so there's no stray hover/focus
/// rectangle. `scale` is the body-relative glyph size (from [`status_glyph`] /
/// [`recording_glyph_size`]); the cell is sized to the largest glyph.
pub(super) fn paint_glyph(ui: &mut egui::Ui, glyph: &str, scale: f32, color: egui::Color32) {
    let base = egui::TextStyle::Body.resolve(ui.style()).size;
    let (_id, rect) = ui.allocate_space(egui::vec2(base * 1.5, base));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(base * scale),
        color,
    );
}

/// Shorten a UUID string to its first segment, enough to disambiguate at a glance.
pub(super) fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
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
