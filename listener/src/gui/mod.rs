//! GUI presentation layer (spec §3, listener ADR-008).
//!
//! A thin egui App over the runtime [`bridge`]: on startup it spawns the
//! background [`Driver`](bridge::Driver) (which owns the [`Listener`]) and then,
//! each frame, drains [`UiUpdate`](bridge::UiUpdate)s into its [`AppState`] and
//! lays out widgets that read that model and emit [`UiCommand`](bridge::UiCommand)s.
//! Per AGENTS §5 this layer never owns the runtime, never does I/O, and never
//! blocks — every runtime touch is a non-blocking channel send. The egui-free,
//! unit-tested pieces live in [`bridge`] and [`state`].

pub mod bridge;
pub mod state;

use anyhow::anyhow;

use crate::config::{templates, ChannelConfig, DecoderConfig, InterfaceConfig};
use crate::core::ChannelId;
use crate::decode::NmeaValidationMode;

use bridge::{BridgeHandle, UiCommand};
use state::{AppState, ChannelStatus};

/// Detach the inherited console when going graphical. The binary is a
/// console-subsystem app so the headless CLI works when launched from a terminal
/// (output, Ctrl-C, and shell-wait all behave); the cost is that a double-click
/// allocates a console window. Freeing it here removes that empty window for the
/// GUI. A brief console flash on double-click is unavoidable without breaking
/// terminal CLI output, so we accept it. No-op off Windows.
#[cfg(windows)]
fn detach_console() {
    // SAFETY: `FreeConsole` takes no arguments and is always safe to call; it simply
    // detaches the process from its console if it has one.
    unsafe {
        let _ = windows_sys::Win32::System::Console::FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console() {}

/// Launch the graphical interface (§3). Owns the eframe event loop on the calling
/// (main) thread; the runtime bridge runs on its own Tokio thread (ADR-008).
///
/// **Shared GUI-startup funnel (two-binary invariant — see `src/main.rs`).** Both
/// GUI entry points route through here: the flash-free `listener-gui.exe`
/// (windows-subsystem) binary, and `listener.exe`'s bare-launch / `--gui` path.
/// Put ALL GUI startup (console detach, logging, window options) in this function
/// so the two binaries stay identical — never in either `main`.
pub fn run() -> anyhow::Result<()> {
    detach_console(); // drop the double-click console before the window opens
    crate::diagnostics::init_logging(); // §114; non-fatal if already installed (§117)
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 740.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Listener",
        options,
        Box::new(|cc| {
            apply_style(&cc.egui_ctx);
            // The driver wakes the UI by requesting a repaint when it pushes an
            // update, so a streaming source refreshes without busy-polling.
            let ctx = cc.egui_ctx.clone();
            let bridge = bridge::spawn(move || ctx.request_repaint()).map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("failed to start the runtime bridge: {e}").into()
                },
            )?;
            Ok(Box::new(ListenerApp::new(bridge)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// Apply the app's base style: Noto Sans as the UI font (matching talker), slightly
/// larger text (≈5%), and a light theme with darker, higher-contrast text. Set once
/// at startup.
fn apply_style(ctx: &egui::Context) {
    install_unicode_fallback_fonts(ctx);
    install_control_pictures_fallback_font(ctx);
    ctx.set_pixels_per_point(1.05);
    let mut style = (*ctx.global_style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.override_text_color = Some(egui::Color32::from_gray(20));
    ctx.set_global_style(style);
}

/// Install Noto Sans as the primary proportional UI font, plus per-script Noto
/// files as lowest-priority fallbacks (mirrors talker's `gui` setup).
///
/// `NotoSans-Regular` (Latin / Greek / Cyrillic / Vietnamese) is registered at
/// **Highest** priority for the `Proportional` family, so it wins over egui's
/// default Ubuntu-Light for the whole UI — one consistent humanist sans. It is also
/// a *lowest*-priority `Monospace` fallback so the message-dump face stays monospace
/// but Noto fills any Latin gaps. The per-script files (Symbols2, Thai, Arabic,
/// Hebrew, Devanagari) are lowest-priority fallbacks for both families so non-Latin
/// codepoints render with real glyphs instead of tofu. CJK is not bundled. See
/// `assets/fonts/README.md`.
fn install_unicode_fallback_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    use egui::FontFamily::{Monospace, Proportional};

    ctx.add_font(FontInsert::new(
        "noto_sans",
        egui::FontData::from_static(include_bytes!("../../assets/fonts/NotoSans-Regular.ttf")),
        vec![
            InsertFontFamily {
                family: Proportional,
                priority: FontPriority::Highest,
            },
            InsertFontFamily {
                family: Monospace,
                priority: FontPriority::Lowest,
            },
        ],
    ));

    const FALLBACKS: &[(&str, &[u8])] = &[
        (
            "noto_sans_symbols2",
            include_bytes!("../../assets/fonts/NotoSansSymbols2-Regular.ttf"),
        ),
        (
            "noto_sans_thai",
            include_bytes!("../../assets/fonts/NotoSansThai-Regular.ttf"),
        ),
        (
            "noto_sans_arabic",
            include_bytes!("../../assets/fonts/NotoSansArabic-Regular.ttf"),
        ),
        (
            "noto_sans_hebrew",
            include_bytes!("../../assets/fonts/NotoSansHebrew-Regular.ttf"),
        ),
        (
            "noto_sans_devanagari",
            include_bytes!("../../assets/fonts/NotoSansDevanagari-Regular.ttf"),
        ),
    ];
    for (name, bytes) in FALLBACKS {
        ctx.add_font(FontInsert::new(
            name,
            egui::FontData::from_static(bytes),
            vec![
                InsertFontFamily {
                    family: Monospace,
                    priority: FontPriority::Lowest,
                },
                InsertFontFamily {
                    family: Proportional,
                    priority: FontPriority::Lowest,
                },
            ],
        ));
    }
}

/// Register a Unicode Control Pictures fallback (an ~18 KB Cascadia Mono subset
/// covering U+2400–U+2421) as a low-priority fallback for both families, so a Glyph
/// rendering of control bytes (`␊` `␍` …, §46) shows real pictures, not tofu.
fn install_control_pictures_fallback_font(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    const FONT: &[u8] = include_bytes!("../../assets/fonts/CascadiaMono-ControlPictures.ttf");
    ctx.add_font(FontInsert::new(
        "control_pictures",
        egui::FontData::from_static(FONT),
        vec![
            InsertFontFamily {
                family: egui::FontFamily::Monospace,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
        ],
    ));
}

/// Which interface a new channel uses, in the add-channel form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AddKind {
    Udp,
    Tcp,
    Serial,
}

impl AddKind {
    fn label(self) -> &'static str {
        match self {
            AddKind::Udp => "UDP",
            AddKind::Tcp => "TCP",
            AddKind::Serial => "Serial",
        }
    }
}

/// How the detail pane renders a Message's bytes (presentation-only, §42).
#[derive(Clone, Copy, PartialEq, Eq)]
enum MsgView {
    Text,
    Hex,
}

/// The eframe application root: the runtime bridge, the folded view-model, the
/// selected channel, and the add-channel form's draft state.
struct ListenerApp {
    bridge: BridgeHandle,
    state: AppState,
    selected: Option<ChannelId>,
    add_kind: AddKind,
    add_endpoint: String,
    add_baud: u32,
    add_nmea: bool,
    msg_view: MsgView,
    /// Edit buffer for the selected channel's port/baud (the "Configure" row), and
    /// which channel it was seeded from (re-seeded when the selection changes).
    edit_for: Option<ChannelId>,
    edit_endpoint: String,
    edit_baud: u32,
    status: String,
}

impl ListenerApp {
    fn new(bridge: BridgeHandle) -> Self {
        Self {
            bridge,
            state: AppState::default(),
            selected: None,
            add_kind: AddKind::Udp,
            add_endpoint: String::new(),
            add_baud: 9600,
            add_nmea: false,
            msg_view: MsgView::Text,
            edit_for: None,
            edit_endpoint: String::new(),
            edit_baud: 9600,
            status: String::new(),
        }
    }

    /// Drain every pending update into the view-model (non-blocking, §99). A newly
    /// added channel takes focus; a removed one that was selected clears it.
    fn drain_updates(&mut self) {
        while let Ok(update) = self.bridge.updates.try_recv() {
            match &update {
                bridge::UiUpdate::ChannelAdded(id, ..) => self.selected = Some(*id),
                bridge::UiUpdate::ChannelRemoved(id) if self.selected == Some(*id) => {
                    self.selected = None;
                }
                _ => {}
            }
            self.state.apply(update);
        }
    }

    /// Send a command to the driver. Non-blocking: a full command channel drops the
    /// command rather than stalling the UI thread (AGENTS §5).
    fn send(&self, command: UiCommand) {
        let _ = self.bridge.commands.try_send(command);
    }

    /// Build a `ChannelConfig` from the add-channel form, or `None` if the endpoint
    /// is not valid for the selected kind.
    fn draft_config(&self) -> Option<ChannelConfig> {
        let endpoint = self.add_endpoint.trim();
        let mut config = match self.add_kind {
            AddKind::Udp => {
                let port: u16 = endpoint.parse().ok()?;
                let mut config = templates::udp_template();
                if let InterfaceConfig::Udp(udp) = &mut config.interface {
                    udp.port = port;
                }
                config
            }
            AddKind::Tcp => {
                let port: u16 = endpoint.parse().ok()?;
                let mut config = templates::tcp_listener_template();
                if let InterfaceConfig::TcpListener(tcp) = &mut config.interface {
                    tcp.port = port;
                }
                config
            }
            AddKind::Serial => {
                if endpoint.is_empty() {
                    return None;
                }
                let mut config = templates::serial_template();
                if let InterfaceConfig::Serial(serial) = &mut config.interface {
                    serial.port = endpoint.to_string();
                    serial.baud_rate = self.add_baud;
                }
                config
            }
        };
        if self.add_nmea {
            config.decoder = DecoderConfig::Nmea0183 {
                validation_mode: NmeaValidationMode::Standard,
            };
        }
        Some(config)
    }

    fn add_channel(&mut self) {
        match self.draft_config() {
            Some(config) => {
                let kind = self.add_kind.label();
                self.send(UiCommand::AddChannel(Box::new(config)));
                self.status = format!("added a {kind} channel");
                self.add_endpoint.clear();
            }
            None => self.status = "enter a valid port (UDP/TCP) or port name (serial)".to_string(),
        }
    }

    /// Apply the edited port/baud to the selected channel (§13). Clones the stored
    /// config (so all other settings are preserved), patches the endpoint, and sends
    /// a `Reconfigure`. A Running channel restarts onto it; a Stopped/Faulted one
    /// swaps it in for the next Start/Retry.
    fn apply_reconfigure(&mut self, id: ChannelId) {
        let Some(mut config) = self.state.channel(id).map(|v| v.config.clone()) else {
            return;
        };
        let endpoint = self.edit_endpoint.trim();
        let ok = match &mut config.interface {
            InterfaceConfig::Udp(udp) => match endpoint.parse() {
                Ok(port) => {
                    udp.port = port;
                    true
                }
                Err(_) => false,
            },
            InterfaceConfig::TcpListener(tcp) => match endpoint.parse() {
                Ok(port) => {
                    tcp.port = port;
                    true
                }
                Err(_) => false,
            },
            InterfaceConfig::Serial(serial) => {
                if endpoint.is_empty() {
                    false
                } else {
                    serial.port = endpoint.to_string();
                    serial.baud_rate = self.edit_baud;
                    true
                }
            }
        };
        if !ok {
            self.status = "enter a valid port to apply".to_string();
            return;
        }
        self.send(UiCommand::Reconfigure(id, Box::new(config)));
        self.status = "reconfigured — Start/Retry to use it on a stopped channel".to_string();
    }

    // ── Panels ──────────────────────────────────────────────────────────────

    fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Add channel:");
            egui::ComboBox::from_id_salt("add_kind")
                .selected_text(self.add_kind.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.add_kind, AddKind::Udp, "UDP");
                    ui.selectable_value(&mut self.add_kind, AddKind::Tcp, "TCP");
                    ui.selectable_value(&mut self.add_kind, AddKind::Serial, "Serial");
                });
            let hint = match self.add_kind {
                AddKind::Serial => "port (e.g. COM3)",
                _ => "port",
            };
            ui.add(
                egui::TextEdit::singleline(&mut self.add_endpoint)
                    .hint_text(hint)
                    .desired_width(140.0),
            );
            if self.add_kind == AddKind::Serial {
                ui.label("baud");
                ui.add(egui::DragValue::new(&mut self.add_baud).range(50..=4_000_000));
            }
            ui.checkbox(&mut self.add_nmea, "NMEA");
            if ui.button("Add").clicked() {
                self.add_channel();
            }
        });
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        ui.add_space(4.0);
    }

    fn show_channel_list(&mut self, ui: &mut egui::Ui) {
        ui.heading("Channels");
        ui.separator();

        // Snapshot the rows first so the list isn't borrowing `state` while a click
        // mutates `selected` or sends a command.
        let rows: Vec<(ChannelId, String, ChannelStatus, u64, f64, usize)> = self
            .state
            .channels()
            .map(|v| {
                (
                    v.id,
                    v.name.clone(),
                    v.status,
                    v.messages,
                    v.bytes_per_sec,
                    v.warnings,
                )
            })
            .collect();

        if rows.is_empty() {
            ui.label("No channels yet — add one above.");
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (id, name, status, messages, bps, warnings) in rows {
                let selected = self.selected == Some(id);
                let warn = if warnings > 0 {
                    format!("  ·  {warnings}⚠")
                } else {
                    String::new()
                };
                // Build the row text as a LayoutJob so the status glyph can be 50%
                // larger and status-coloured (green = running, etc.) while the rest
                // stays the normal body size/colour.
                let base = egui::TextStyle::Body.resolve(ui.style()).size;
                let text_color = ui.visuals().text_color();
                let mut job = egui::text::LayoutJob::default();
                job.append(
                    status_glyph(status),
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(base * 1.5),
                        color: status_color(status),
                        valign: egui::Align::Center,
                        ..Default::default()
                    },
                );
                job.append(
                    &format!("  {name}  ·  {messages} msg  ·  {bps:.0} B/s{warn}"),
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(base),
                        color: text_color,
                        valign: egui::Align::Center,
                        ..Default::default()
                    },
                );
                // Full-width selectable: the whole row box is clickable and
                // highlighted, not just the text.
                let response = ui.add_sized(
                    [ui.available_width(), 0.0],
                    egui::Button::selectable(selected, job),
                );
                if response.clicked() {
                    self.selected = Some(id);
                }
                ui.horizontal(|ui| {
                    match status {
                        ChannelStatus::Running => {
                            if ui.small_button("Stop").clicked() {
                                self.send(UiCommand::Stop(id));
                            }
                        }
                        // A Faulted channel can't go straight to Starting (§8.5): it
                        // must pass through Stopped, so "Retry" sends Stop then Start.
                        ChannelStatus::Faulted => {
                            if ui.small_button("Retry").clicked() {
                                self.send(UiCommand::Stop(id));
                                self.send(UiCommand::Start(id));
                            }
                        }
                        _ => {
                            if ui.small_button("Start").clicked() {
                                self.send(UiCommand::Start(id));
                            }
                        }
                    }
                    if ui.small_button("Remove").clicked() {
                        self.send(UiCommand::RemoveChannel(id));
                    }
                });
                ui.separator();
            }
        });
    }

    fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected else {
            ui.label("Select a channel to see its details.");
            return;
        };
        // Header + the view-mode toggle mutate nothing heavy: take cheap copies of
        // the view so the toggle can borrow `self` mutably without conflicting with
        // a live `&self.state` borrow, then re-borrow for snapshot rendering below.
        let Some((name, details, status, messages, bps, warnings, last_error)) =
            self.state.channel(id).map(|v| {
                (
                    v.name.clone(),
                    v.details.clone(),
                    v.status,
                    v.messages,
                    v.bytes_per_sec,
                    v.warnings,
                    v.last_error.clone(),
                )
            })
        else {
            ui.label("That channel is no longer present.");
            return;
        };

        ui.heading(&name);
        ui.label(format!("Connection: {details}"));
        ui.label(format!("Status: {}", status_label(status)));
        ui.label(format!(
            "Messages: {messages}    Throughput: {bps:.0} B/s    Warnings: {warnings}"
        ));
        // Surface a failed command (e.g. a bind conflict) with the reason and a
        // way out, instead of a bare "faulted" with no explanation.
        if let Some(err) = &last_error {
            ui.colored_label(egui::Color32::from_rgb(170, 30, 30), format!("⚠ {err}"));
            ui.label(
                "Recourse: change the port below and Apply, free the resource (Stop \
                 the other channel on that port) then Retry, or Remove this channel.",
            );
        }

        // Configure: edit the port (and baud, for serial), then Apply (§13). Seed the
        // buffer from the channel's config whenever the selection changes.
        if self.edit_for != Some(id) {
            if let Some((endpoint, baud)) =
                self.state.channel(id).map(|v| config_endpoint(&v.config))
            {
                self.edit_endpoint = endpoint;
                self.edit_baud = baud;
            }
            self.edit_for = Some(id);
        }
        let is_serial = self
            .state
            .channel(id)
            .map(|v| matches!(v.config.interface, InterfaceConfig::Serial(_)))
            .unwrap_or(false);
        ui.horizontal(|ui| {
            ui.label("Port:");
            ui.add(egui::TextEdit::singleline(&mut self.edit_endpoint).desired_width(120.0));
            if is_serial {
                ui.label("baud");
                ui.add(egui::DragValue::new(&mut self.edit_baud).range(50..=4_000_000));
            }
            if ui.button("Apply").clicked() {
                self.apply_reconfigure(id);
            }
        });

        ui.horizontal(|ui| {
            ui.label("View:");
            ui.selectable_value(&mut self.msg_view, MsgView::Text, "Text");
            ui.selectable_value(&mut self.msg_view, MsgView::Hex, "Hex");
        });
        let msg_view = self.msg_view;
        ui.separator();

        let Some(view) = self.state.channel(id) else {
            return;
        };
        let Some(snapshot) = &view.snapshot else {
            ui.label("No snapshot yet — start the channel to see live data.");
            return;
        };

        // Diagnostics: counts, with the actual recent messages on demand (§91–§95).
        ui.label(format!(
            "Diagnostics — events {}, warnings {}, errors {}",
            snapshot.diagnostics.events.len(),
            snapshot.diagnostics.warnings.len(),
            snapshot.diagnostics.errors.len(),
        ));
        if !snapshot.diagnostics.warnings.is_empty() || !snapshot.diagnostics.errors.is_empty() {
            egui::CollapsingHeader::new("Diagnostic messages")
                .id_salt("diagnostics")
                .show(ui, |ui| {
                    for d in snapshot.diagnostics.errors.iter().rev().take(20) {
                        ui.colored_label(egui::Color32::from_rgb(170, 30, 30), &d.message);
                    }
                    for d in snapshot.diagnostics.warnings.iter().rev().take(20) {
                        ui.colored_label(egui::Color32::from_rgb(150, 100, 0), &d.message);
                    }
                });
        }

        // Match Rule firings (§50.2, §165): which rule fired, on which Message.
        if !snapshot.matches.is_empty() {
            egui::CollapsingHeader::new(format!("Match firings ({})", snapshot.matches.len()))
                .id_salt("matches")
                .show(ui, |ui| {
                    for m in snapshot.matches.iter().rev().take(20) {
                        let on = m
                            .message_number
                            .map(|n| format!("#{n}"))
                            .unwrap_or_else(|| "(idle)".to_string());
                        ui.monospace(format!("{on}  rule {}", short_id(&m.rule_id.to_string())));
                    }
                });
        }

        ui.separator();

        // Pause/Resume the primary Display View (§11, §50). Pause freezes this
        // view's on-screen history; reception, recording, and retention continue.
        // The message list below reads the view's history, so pause is visible here.
        if let Some(v0) = snapshot.display_views.first() {
            ui.horizontal(|ui| {
                if v0.paused {
                    if ui.button("Resume").clicked() {
                        self.send(UiCommand::ResumeDisplay(id, v0.id));
                    }
                    ui.label("view paused — reception continues");
                } else if ui.button("Pause").clicked() {
                    self.send(UiCommand::PauseDisplay(id, v0.id));
                }
            });
        }

        // The primary view's history (frozen while paused); fall back to retention
        // if a channel somehow has no view.
        let messages = snapshot
            .display_views
            .first()
            .map(|v| v.messages.as_slice())
            .unwrap_or(snapshot.retained.as_slice());
        ui.label(format!("Recent messages ({}):", messages.len()));
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for decoded in messages {
                    let number = decoded.message.number;
                    let kind = decoded
                        .protocol
                        .as_ref()
                        .and_then(|p| p.message_type.clone())
                        .map(|t| format!("[{t}] "))
                        .unwrap_or_default();
                    let body = match msg_view {
                        MsgView::Text => render_bytes(&decoded.message.bytes),
                        MsgView::Hex => render_hex(&decoded.message.bytes),
                    };
                    ui.monospace(format!("#{number}  {kind}{body}"));
                }
            });
    }
}

impl eframe::App for ListenerApp {
    // This workspace's eframe surfaces a `Ui` directly (App::ui), like talker's GUI.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_updates();
        egui::Panel::top("toolbar").show_inside(ui, |ui| self.show_toolbar(ui));
        egui::Panel::left("channel_list")
            .resizable(true)
            .default_size(320.0)
            .show_inside(ui, |ui| self.show_channel_list(ui));
        egui::CentralPanel::default().show_inside(ui, |ui| self.show_detail(ui));
    }
}

/// The editable endpoint of a channel's config: the port string (UDP/TCP) or the
/// serial port name, plus the baud (0 for non-serial, which ignore it).
fn config_endpoint(config: &ChannelConfig) -> (String, u32) {
    match &config.interface {
        InterfaceConfig::Udp(udp) => (udp.port.to_string(), 0),
        InterfaceConfig::TcpListener(tcp) => (tcp.port.to_string(), 0),
        InterfaceConfig::Serial(serial) => (serial.port.clone(), serial.baud_rate),
    }
}

/// A short status word for the detail pane.
fn status_label(status: ChannelStatus) -> &'static str {
    match status {
        ChannelStatus::Stopped => "stopped",
        ChannelStatus::Running => "running",
        ChannelStatus::Faulted => "faulted",
        ChannelStatus::Reconnecting => "reconnecting",
    }
}

/// A leading glyph for a channel row, so status reads at a glance.
fn status_glyph(status: ChannelStatus) -> &'static str {
    match status {
        ChannelStatus::Stopped => "○",
        ChannelStatus::Running => "●",
        ChannelStatus::Faulted => "✖",
        ChannelStatus::Reconnecting => "↻",
    }
}

/// The colour for a status glyph: green running, grey stopped, red faulted, amber
/// reconnecting.
fn status_color(status: ChannelStatus) -> egui::Color32 {
    match status {
        ChannelStatus::Running => egui::Color32::from_rgb(30, 150, 30),
        ChannelStatus::Stopped => egui::Color32::from_gray(120),
        ChannelStatus::Faulted => egui::Color32::from_rgb(190, 40, 40),
        ChannelStatus::Reconnecting => egui::Color32::from_rgb(200, 140, 0),
    }
}

/// Render Message bytes as text: printable ASCII as-is, other bytes as a `⟨HH⟩`
/// marker (matching the spec's inline-marker convention, §5.3). Presentation-only;
/// the bytes themselves are never modified (§40).
fn render_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                (b as char).to_string()
            } else {
                format!("⟨{b:02X}⟩")
            }
        })
        .collect()
}

/// Render Message bytes as space-separated uppercase hex (§45). Presentation-only.
fn render_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Shorten a UUID string to its first segment, enough to disambiguate at a glance.
fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}
