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
            status: String::new(),
        }
    }

    /// Drain every pending update into the view-model (non-blocking, §99).
    fn drain_updates(&mut self) {
        while let Ok(update) = self.bridge.updates.try_recv() {
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
                let label = format!(
                    "{}  {name}  ·  {messages} msg  ·  {bps:.0} B/s{warn}",
                    status_glyph(status)
                );
                if ui.selectable_label(selected, label).clicked() {
                    self.selected = Some(id);
                }
                ui.horizontal(|ui| {
                    if status == ChannelStatus::Running {
                        if ui.small_button("Stop").clicked() {
                            self.send(UiCommand::Stop(id));
                        }
                    } else if ui.small_button("Start").clicked() {
                        self.send(UiCommand::Start(id));
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
        let Some(view) = self.state.channel(id) else {
            ui.label("That channel is no longer present.");
            return;
        };

        ui.heading(&view.name);
        ui.label(format!("Status: {}", status_label(view.status)));
        ui.label(format!(
            "Messages: {}    Throughput: {:.0} B/s    Warnings: {}",
            view.messages, view.bytes_per_sec, view.warnings
        ));
        ui.separator();

        let Some(snapshot) = &view.snapshot else {
            ui.label("No snapshot yet — start the channel to see live data.");
            return;
        };

        ui.label(format!(
            "Diagnostics — events {}, warnings {}, errors {}",
            snapshot.diagnostics.events.len(),
            snapshot.diagnostics.warnings.len(),
            snapshot.diagnostics.errors.len(),
        ));
        if !snapshot.matches.is_empty() {
            ui.label(format!("Match rule firings: {}", snapshot.matches.len()));
        }
        ui.separator();
        ui.label(format!(
            "Recent messages ({} retained):",
            snapshot.retained.len()
        ));

        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for decoded in &snapshot.retained {
                    let number = decoded.message.number;
                    let kind = decoded
                        .protocol
                        .as_ref()
                        .and_then(|p| p.message_type.clone())
                        .map(|t| format!("[{t}] "))
                        .unwrap_or_default();
                    ui.monospace(format!(
                        "#{number}  {kind}{}",
                        render_bytes(&decoded.message.bytes)
                    ));
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

/// Render Message bytes for display: printable ASCII as-is, other bytes as a
/// `⟨HH⟩` marker (matching the spec's inline-marker convention, §5.3). This is a
/// presentation-only rendering; the bytes themselves are never modified (§40).
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
