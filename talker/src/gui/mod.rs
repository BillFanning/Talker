mod draft;
mod thread;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context as _;
use egui::{Align, Layout, ScrollArea};

use crate::core::{
    channel::ChannelConfig,
    logging::{LogEvent, LoggingConfig},
    profile::Profile,
    scheduler::Schedule,
};

use draft::{ConnDraft, ConnKind, PayloadKind, ScheduleDraft, UdpModeDraft};
use thread::{run_talker, TalkerCommand, TalkerHandle, TalkerStatus};

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(initial_profile: Option<PathBuf>) -> anyhow::Result<()> {
    let (log_tx, log_rx) = crossbeam_channel::bounded::<LogEvent>(512);
    let _logging = crate::core::logging::init(&LoggingConfig::default(), Some(log_tx))
        .context("initializing logging")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 740.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Talker",
        options,
        Box::new(move |cc| {
            Ok(Box::new(TalkerApp::new(
                log_rx,
                initial_profile,
                &cc.egui_ctx,
                cc.storage,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

// ── App ───────────────────────────────────────────────────────────────────────

struct TalkerApp {
    profile: Profile,
    profile_path: Option<PathBuf>,
    dirty: bool,
    conn_drafts: Vec<ConnDraft>,
    sched_drafts: Vec<Vec<ScheduleDraft>>,
    conn_errors: Vec<Option<String>>,
    talkers: Vec<Option<TalkerHandle>>,
    log_rx: crossbeam_channel::Receiver<LogEvent>,
    log_lines: Vec<(String, egui::Color32)>,
    sent_counts: Vec<u64>,
    error_count: u64,
    last_title: String,
    serial_ports: Vec<String>,
    pixels_per_point: f32,
    zoom_held_timer: Option<f32>, // None = not held; Some(t) = held, t<0 in delay, t>=0 repeating
}

impl TalkerApp {
    fn new(
        log_rx: crossbeam_channel::Receiver<LogEvent>,
        initial_profile: Option<PathBuf>,
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
    ) -> Self {
        let ppp = storage
            .and_then(|s| s.get_string("pixels_per_point"))
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|&v| v > 0.0)
            .unwrap_or(1.0);
        ctx.set_pixels_per_point(ppp);
        let mut app = Self {
            profile: Profile::default(),
            profile_path: None,
            dirty: false,
            conn_drafts: Vec::new(),
            sched_drafts: Vec::new(),
            conn_errors: Vec::new(),
            talkers: Vec::new(),
            log_rx,
            log_lines: Vec::new(),
            sent_counts: Vec::new(),
            error_count: 0,
            last_title: String::new(),
            serial_ports: Vec::new(),
            pixels_per_point: ppp,
            zoom_held_timer: None,
        };
        app.refresh_serial_ports();

        // CLI arg takes precedence; fall back to last path saved in storage.
        let path = initial_profile.or_else(|| {
            storage
                .and_then(|s| s.get_string("last_profile_path"))
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        });

        if let Some(p) = path {
            app.load_profile_from_path(&p);
        }

        app
    }

    fn is_any_running(&self) -> bool {
        self.talkers.iter().any(|t| t.is_some())
    }

    fn is_connection_running(&self, i: usize) -> bool {
        self.talkers.get(i).is_some_and(|t| t.is_some())
    }

    fn can_start_connection(&self, i: usize) -> bool {
        !self.is_connection_running(i)
            && self
                .conn_drafts
                .get(i)
                .is_some_and(|d| d.to_config().is_some())
            && self
                .sched_drafts
                .get(i)
                .is_some_and(|s| s.iter().any(|d| d.to_message_config().is_some()))
    }

    fn can_start_any(&self) -> bool {
        (0..self.conn_drafts.len()).any(|i| self.can_start_connection(i))
    }

    fn refresh_serial_ports(&mut self) {
        self.serial_ports = serialport::available_ports()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.port_name)
            .collect();
        self.serial_ports.sort();
    }

    fn window_title(&self) -> String {
        let name = if self.profile.name.is_empty() {
            "unnamed"
        } else {
            &self.profile.name
        };
        let star = if self.dirty { " *" } else { "" };
        if self.profile_path.is_none() && !self.dirty {
            "Talker".to_string()
        } else {
            format!("Talker \u{2014} {name}{star}")
        }
    }

    // ── Profile actions ───────────────────────────────────────────────────────

    fn load_profile_from_path(&mut self, path: &Path) {
        self.stop_all();
        match Profile::load(path) {
            Ok(p) => {
                let n = p.channels.len();
                self.conn_drafts = p
                    .channels
                    .iter()
                    .map(|ch| ConnDraft::from(&ch.interface))
                    .collect();
                self.sched_drafts = p
                    .channels
                    .iter()
                    .map(|ch| ch.messages.iter().map(ScheduleDraft::from).collect())
                    .collect();
                self.conn_errors = vec![None; n];
                self.talkers = (0..n).map(|_| None).collect();
                self.sent_counts = vec![0; n];
                self.profile = p;
                self.profile_path = Some(path.to_path_buf());
                self.dirty = false;
                tracing::info!("profile '{}' loaded", self.profile.name);
            }
            Err(e) => tracing::error!("load failed: {e:#}"),
        }
    }

    fn confirm_discard(&self) -> bool {
        !self.dirty
            || rfd::MessageDialog::new()
                .set_title("Unsaved Changes")
                .set_description("Discard unsaved changes?")
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show()
                == rfd::MessageDialogResult::Ok
    }

    fn new_profile(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        self.stop_all();
        self.profile = Profile::default();
        self.profile_path = None;
        self.dirty = true;
        self.conn_drafts.clear();
        self.sched_drafts.clear();
        self.conn_errors.clear();
        self.talkers.clear();
        self.sent_counts.clear();
        self.error_count = 0;
        tracing::info!("new profile");
    }

    fn load_profile_dialog(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("TOML Profile", &["toml"])
            .pick_file()
        else {
            return;
        };
        self.error_count = 0;
        self.load_profile_from_path(&path);
    }

    fn save_profile(&mut self) {
        self.flush_drafts_to_profile();
        let path = match &self.profile_path {
            Some(p) => p.clone(),
            None => {
                let stem = if self.profile.name.is_empty() {
                    "profile"
                } else {
                    &self.profile.name
                };
                let name = format!("{stem}.toml");
                let Some(p) = rfd::FileDialog::new()
                    .add_filter("TOML Profile", &["toml"])
                    .set_file_name(&name)
                    .save_file()
                else {
                    return;
                };
                p
            }
        };
        match self.profile.save(&path) {
            Ok(()) => {
                self.profile_path = Some(path);
                self.dirty = false;
                tracing::info!("profile saved");
            }
            Err(e) => tracing::error!("save failed: {e:#}"),
        }
    }

    // ── Talker thread lifecycle ────────────────────────────────────────────────

    fn start_connection(&mut self, i: usize) {
        self.stop_connection(i);
        self.flush_drafts_to_profile();

        let Some(cfg) = self.conn_drafts.get(i).and_then(|d| d.to_config()) else {
            tracing::warn!("channel {i} config invalid");
            return;
        };

        let interface = match cfg.open() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to open channel {i}: {e:#}");
                self.error_count += 1;
                if i < self.conn_errors.len() {
                    self.conn_errors[i] = Some(format!("{e:#}"));
                }
                return;
            }
        };

        let messages = self
            .profile
            .channels
            .get(i)
            .map(|c| c.messages.clone())
            .unwrap_or_default();
        let schedule = match Schedule::compile(&messages, Instant::now()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("channel {i} schedule error: {e:#}");
                return;
            }
        };

        if i < self.conn_errors.len() {
            self.conn_errors[i] = None;
        }
        if i < self.sent_counts.len() {
            self.sent_counts[i] = 0;
        }

        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(32);
        let (status_tx, status_rx) = crossbeam_channel::bounded(256);
        let thread = std::thread::spawn(move || run_talker(interface, schedule, cmd_rx, status_tx));

        if i < self.talkers.len() {
            self.talkers[i] = Some(TalkerHandle {
                cmd_tx,
                status_rx,
                thread,
            });
        }
        let message_count = messages.len();
        tracing::info!("channel {i} started ({message_count}-message schedule)");
    }

    fn stop_connection(&mut self, i: usize) {
        if i < self.talkers.len() {
            if let Some(h) = self.talkers[i].take() {
                let _ = h.cmd_tx.try_send(TalkerCommand::Stop);
                let _ = h.thread.join();
                tracing::info!("connection {i} stopped");
            }
        }
    }

    fn start_all(&mut self) {
        let n = self.conn_drafts.len();
        for i in 0..n {
            if self.can_start_connection(i) {
                self.start_connection(i);
            }
        }
    }

    fn stop_all(&mut self) {
        for i in 0..self.talkers.len() {
            if self.talkers[i].is_some() {
                self.stop_connection(i);
            }
        }
    }

    fn flush_drafts_to_profile(&mut self) {
        self.profile.channels = (0..self.conn_drafts.len())
            .filter_map(|i| {
                let interface = self.conn_drafts[i].to_config()?;
                let messages = self
                    .sched_drafts
                    .get(i)
                    .map(|drafts| {
                        drafts
                            .iter()
                            .filter_map(|d| d.to_message_config())
                            .collect()
                    })
                    .unwrap_or_default();
                Some(ChannelConfig::new(interface, messages))
            })
            .collect();
    }

    fn apply_connection(&mut self, i: usize) {
        let Some(cfg) = self.conn_drafts[i].to_config() else {
            return;
        };
        if i < self.profile.channels.len() {
            self.profile.channels[i].interface = cfg.clone();
        } else {
            self.profile
                .channels
                .push(ChannelConfig::new(cfg.clone(), Vec::new()));
        }
        if let Some(Some(h)) = self.talkers.get(i) {
            let _ = h.cmd_tx.try_send(TalkerCommand::UpdateInterface(cfg));
        }
        self.dirty = true;
    }

    // ── Channel polling ───────────────────────────────────────────────────────

    fn poll_channels(&mut self, ctx: &egui::Context) {
        // Keyboard shortcuts
        let (new, load, save) = ctx.input(|inp| {
            let ctrl = inp.modifiers.ctrl || inp.modifiers.mac_cmd;
            (
                ctrl && inp.key_pressed(egui::Key::N),
                ctrl && inp.key_pressed(egui::Key::O),
                ctrl && inp.key_pressed(egui::Key::S),
            )
        });
        if new {
            self.new_profile();
        }
        if load {
            self.load_profile_dialog();
        }
        if save {
            self.save_profile();
        }

        for event in self.log_rx.try_iter() {
            let ts = event.timestamp.format("%H:%M:%S%.3f");
            let color = level_color(event.level);
            let line = format!("[{ts}] [{:<5}] {}", event.level, event.message);
            self.log_lines.push((line, color));
        }
        const LOG_CAP: usize = 2000;
        if self.log_lines.len() > LOG_CAP {
            self.log_lines.drain(..self.log_lines.len() - LOG_CAP);
        }

        let mut any_running = false;
        for i in 0..self.talkers.len() {
            let (statuses, finished) = match &self.talkers[i] {
                Some(h) => {
                    let s: Vec<TalkerStatus> = h.status_rx.try_iter().collect();
                    let f = h.thread.is_finished();
                    (s, f)
                }
                None => continue,
            };
            any_running = true;
            for status in statuses {
                match status {
                    TalkerStatus::SendCount(n) => {
                        if i < self.sent_counts.len() {
                            self.sent_counts[i] = n;
                        }
                        if i < self.conn_errors.len() {
                            self.conn_errors[i] = None;
                        }
                    }
                    TalkerStatus::ConnectionError { message, .. } => {
                        self.error_count += 1;
                        if i < self.conn_errors.len() {
                            self.conn_errors[i] = Some(message);
                        }
                    }
                }
            }
            if finished {
                self.talkers[i] = None;
            }
        }
        if any_running {
            ctx.request_repaint();
        }

        // Update window title when it changes.
        let title = self.window_title();
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for TalkerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().set_pixels_per_point(self.pixels_per_point);
        self.poll_channels(ui.ctx());
        egui::Frame::new()
            .inner_margin(4.0)
            .stroke(egui::Stroke::new(1.5, egui::Color32::from_rgb(50, 60, 80)))
            .show(ui, |ui| {
                self.show_top_bar(ui);
                self.show_status_bar(ui);
                self.show_log_panel(ui);
                self.show_central(ui);
            });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let path_str = self
            .profile_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        storage.set_string("last_profile_path", path_str);
        storage.set_string("pixels_per_point", self.pixels_per_point.to_string());
    }
}

// ── Panel renderers ───────────────────────────────────────────────────────────

impl TalkerApp {
    fn show_top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Profile:");
                let r =
                    ui.add(egui::TextEdit::singleline(&mut self.profile.name).desired_width(160.0));
                if r.changed() {
                    self.dirty = true;
                }
                if ui.button("New").clicked() {
                    self.new_profile();
                }
                if ui.button("Load\u{2026}").clicked() {
                    self.load_profile_dialog();
                }
                if ui.button("Save").clicked() {
                    self.save_profile();
                }

                ui.separator();
                let r_minus = ui.small_button("−");
                ui.label(format!(
                    "{}%",
                    (self.pixels_per_point * 100.0).round() as u32,
                ));
                let r_plus = ui.small_button("+");

                let minus_down = r_minus.is_pointer_button_down_on();
                let plus_down = r_plus.is_pointer_button_down_on();
                let direction: f32 = if minus_down {
                    -1.0
                } else if plus_down {
                    1.0
                } else {
                    0.0
                };

                if direction != 0.0 {
                    let dt = ui.ctx().input(|i| i.stable_dt);
                    match self.zoom_held_timer {
                        None => {
                            // First frame pressed — fire immediately.
                            self.pixels_per_point =
                                (self.pixels_per_point + direction * 0.1).clamp(0.75, 2.5);
                            self.zoom_held_timer = Some(-0.4);
                        }
                        Some(ref mut t) => {
                            *t += dt;
                            if *t >= 0.0 {
                                *t -= 0.1; // repeat every 100 ms
                                self.pixels_per_point =
                                    (self.pixels_per_point + direction * 0.1).clamp(0.75, 2.5);
                            }
                        }
                    }
                    ui.ctx().request_repaint();
                } else {
                    // Fallback: handle a quick tap that releases before is_pointer_button_down_on fires.
                    if r_minus.clicked() && self.zoom_held_timer.is_none() {
                        self.pixels_per_point = (self.pixels_per_point - 0.1).max(0.75);
                    }
                    if r_plus.clicked() && self.zoom_held_timer.is_none() {
                        self.pixels_per_point = (self.pixels_per_point + 0.1).min(2.5);
                    }
                    self.zoom_held_timer = None;
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.is_any_running() && ui.button("\u{25a0}  Stop All").clicked() {
                        self.stop_all();
                    }
                    let btn = ui.add_enabled(
                        self.can_start_any(),
                        egui::Button::new("\u{25b6}  Start All"),
                    );
                    if btn.clicked() {
                        self.start_all();
                    }
                    if !self.can_start_any() {
                        btn.on_disabled_hover_text(
                            "Add at least one valid connection and one schedule entry",
                        );
                    }
                });
            });
            ui.add_space(4.0);
        });
    }

    fn show_status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let running = self.talkers.iter().filter(|t| t.is_some()).count();
                let total = self.talkers.len();
                let (color, label) = if running > 0 {
                    (
                        egui::Color32::from_rgb(80, 200, 80),
                        if running == total && total > 0 {
                            "\u{25cf} All running".to_string()
                        } else {
                            format!("\u{25cf} {running}/{total} running")
                        },
                    )
                } else {
                    (egui::Color32::GRAY, "\u{25cf} Stopped".to_string())
                };
                ui.colored_label(color, label);
                ui.separator();
                let total_sent: u64 = self.sent_counts.iter().sum();
                ui.label(format!("Sent: {total_sent}"));
                ui.separator();
                ui.label(format!("Errors: {}", self.error_count));
                if let Some(path) = &self.profile_path {
                    ui.separator();
                    let display = path.display().to_string();
                    ui.label(&display).on_hover_text(&display);
                }
            });
            ui.add_space(2.0);
        });
    }

    fn show_log_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log_panel")
            .resizable(true)
            .default_size(190.0)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Log");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.log_lines.clear();
                        }
                    });
                });
                ui.separator();
                ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                    for (line, color) in &self.log_lines {
                        ui.colored_label(*color, egui::RichText::new(line).monospace());
                    }
                });
            });
    }

    fn show_central(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.show_connections_tab(ui);
        });
    }

    fn show_connections_tab(&mut self, ui: &mut egui::Ui) {
        let mut to_apply: Vec<usize> = Vec::new();
        let mut to_remove: Option<usize> = None;
        let mut to_start: Option<usize> = None;
        let mut to_stop: Option<usize> = None;
        let mut add_one = false;
        let mut do_refresh_ports = false;

        ScrollArea::vertical().show(ui, |ui| {
            let n = self.conn_drafts.len();
            for i in 0..n {
                ui.push_id(i, |ui| {
                    let mut conn_frame = egui::Frame::group(ui.style());
                    conn_frame.stroke =
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(70, 85, 120));
                    conn_frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let error = self.conn_errors.get(i).and_then(|e| e.as_deref());
                            let running = self.is_connection_running(i);
                            let (dot_color, dot_tip): (egui::Color32, &str) = if !running {
                                (egui::Color32::GRAY, "not running")
                            } else if let Some(msg) = error {
                                (egui::Color32::RED, msg)
                            } else {
                                (egui::Color32::from_rgb(80, 200, 80), "ok")
                            };
                            ui.colored_label(dot_color, "\u{25cf}")
                                .on_hover_text(dot_tip);

                            ui.strong(format!("Connection {}", i + 1));
                            ui.separator();
                            let before_kind = self.conn_drafts[i].kind;
                            ui.radio_value(
                                &mut self.conn_drafts[i].kind,
                                ConnKind::Serial,
                                "Serial",
                            );
                            ui.radio_value(&mut self.conn_drafts[i].kind, ConnKind::Udp, "UDP");
                            ui.radio_value(&mut self.conn_drafts[i].kind, ConnKind::Tcp, "TCP");
                            if self.conn_drafts[i].kind != before_kind {
                                to_apply.push(i);
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("\u{2715}").clicked() {
                                    to_remove = Some(i);
                                }
                                if running {
                                    if ui.small_button("\u{25a0}").on_hover_text("Stop").clicked() {
                                        to_stop = Some(i);
                                    }
                                } else {
                                    let can = self.can_start_connection(i);
                                    let btn =
                                        ui.add_enabled(can, egui::Button::new("\u{25b6}").small());
                                    if btn.clicked() {
                                        to_start = Some(i);
                                    }
                                    if !can {
                                        btn.on_disabled_hover_text(
                                            "Add a valid schedule entry first",
                                        );
                                    }
                                }
                            });
                        });
                        ui.separator();

                        let (changed, refresh) = match self.conn_drafts[i].kind {
                            ConnKind::Serial => {
                                show_serial_fields(ui, &mut self.conn_drafts[i], &self.serial_ports)
                            }
                            ConnKind::Udp => (show_udp_fields(ui, &mut self.conn_drafts[i]), false),
                            ConnKind::Tcp => (show_tcp_fields(ui, &mut self.conn_drafts[i]), false),
                        };
                        if changed {
                            to_apply.push(i);
                        }
                        if refresh {
                            do_refresh_ports = true;
                        }

                        ui.separator();
                        show_schedule_section(ui, &mut self.sched_drafts[i], &mut self.dirty);
                    });
                });
                ui.add_space(6.0);
            }
            if ui.button("+ Add Connection").clicked() {
                add_one = true;
            }
        });

        for i in to_apply {
            self.apply_connection(i);
        }
        if let Some(i) = to_start {
            self.start_connection(i);
        }
        if let Some(i) = to_stop {
            self.stop_connection(i);
        }
        if let Some(i) = to_remove {
            self.stop_connection(i);
            self.conn_drafts.remove(i);
            self.sched_drafts.remove(i);
            self.conn_errors.remove(i);
            self.talkers.remove(i);
            self.sent_counts.remove(i);
            if i < self.profile.channels.len() {
                self.profile.channels.remove(i);
            }
            self.dirty = true;
        }
        if add_one {
            self.conn_drafts.push(ConnDraft::default());
            self.sched_drafts.push(Vec::new());
            self.conn_errors.push(None);
            self.talkers.push(None);
            self.sent_counts.push(0);
            self.dirty = true;
        }
        if do_refresh_ports {
            self.refresh_serial_ports();
        }
    }
}

// ── Inline schedule editor (one per connection card) ──────────────────────────

fn show_schedule_section(ui: &mut egui::Ui, entries: &mut Vec<ScheduleDraft>, dirty: &mut bool) {
    let mut to_remove: Option<usize> = None;
    let mut add_one = false;

    ui.collapsing("Schedule", |ui| {
        for (i, entry) in entries.iter_mut().enumerate() {
            ui.push_id(i, |ui| {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("Entry {}", i + 1));
                        ui.separator();
                        ui.radio_value(&mut entry.payload_kind, PayloadKind::RawHex, "Hex");
                        ui.radio_value(&mut entry.payload_kind, PayloadKind::Nmea, "NMEA");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("\u{2715}").clicked() {
                                to_remove = Some(i);
                            }
                        });
                    });

                    egui::Grid::new("sched_grid")
                        .num_columns(2)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            match entry.payload_kind {
                                PayloadKind::RawHex => {
                                    ui.label("Data (hex)");
                                    let r = ui.add(
                                        egui::TextEdit::singleline(&mut entry.hex_data)
                                            .desired_width(360.0)
                                            .hint_text("DE AD BE EF"),
                                    );
                                    if r.changed() {
                                        entry.hex_data = entry.hex_data.to_ascii_uppercase();
                                    }
                                    ui.end_row();
                                    if !entry.hex_data.is_empty() && !hex_valid(&entry.hex_data) {
                                        ui.label("");
                                        ui.label(err_text(
                                            "invalid hex — use byte pairs like DE AD BE EF",
                                        ));
                                        ui.end_row();
                                    }
                                }
                                PayloadKind::Nmea => {
                                    ui.label("Talker");
                                    ui.horizontal(|ui| {
                                        let r = ui.add(
                                            egui::TextEdit::singleline(&mut entry.nmea_talker)
                                                .desired_width(40.0)
                                                .hint_text("GP"),
                                        );
                                        if r.changed() {
                                            entry.nmea_talker =
                                                entry.nmea_talker.to_ascii_uppercase();
                                        }
                                        ui.menu_button("v", |ui| {
                                            for id in &["GP", "GN", "GL", "GA", "GB", "GQ", "P"] {
                                                if ui.button(*id).clicked() {
                                                    entry.nmea_talker = id.to_string();
                                                    ui.close();
                                                }
                                            }
                                        });
                                    });
                                    ui.end_row();

                                    ui.label("Sentence");
                                    ui.horizontal(|ui| {
                                        let r = ui.add(
                                            egui::TextEdit::singleline(
                                                &mut entry.nmea_sentence_type,
                                            )
                                            .desired_width(50.0)
                                            .hint_text("GGA"),
                                        );
                                        if r.changed() {
                                            entry.nmea_sentence_type =
                                                entry.nmea_sentence_type.to_ascii_uppercase();
                                        }
                                        ui.menu_button("v", |ui| {
                                            for st in &[
                                                "GGA", "RMC", "VTG", "GLL", "GSA", "GSV",
                                                "HDT", "HDM", "ZDA", "GNS", "VHW", "DBT",
                                                "DPT", "MTW", "MWV", "RSA", "ROT",
                                            ] {
                                                if ui.button(*st).clicked() {
                                                    entry.nmea_sentence_type = st.to_string();
                                                    ui.close();
                                                }
                                            }
                                        });
                                    });
                                    ui.end_row();

                                    ui.label("Fields");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut entry.nmea_fields)
                                            .desired_width(360.0)
                                            .hint_text(
                                                "comma-separated, e.g. 123519,4807.038,N,01131.000,E",
                                            ),
                                    );
                                    ui.end_row();
                                }
                                PayloadKind::Other => {
                                    ui.label("Format");
                                    ui.weak(
                                        "set via TOML \u{2014} GUI editor in a later phase",
                                    );
                                    ui.end_row();
                                }
                            }

                            ui.label("Interval (ms)");
                            ui.add(
                                egui::TextEdit::singleline(&mut entry.interval_ms)
                                    .desired_width(80.0),
                            );
                            ui.end_row();
                            if !entry.interval_ms.is_empty()
                                && entry.interval_ms.parse::<u64>().is_err()
                            {
                                ui.label("");
                                ui.label(err_text("must be a whole number greater than 0"));
                                ui.end_row();
                            }
                        });

                    if entry.timestamp.is_some() || entry.checksum.is_some() {
                        ui.weak(
                            "timestamp/checksum configured \u{2014} editor in a later phase",
                        );
                    }
                });
            });
            ui.add_space(4.0);
        }
        if ui.small_button("+ Add Entry").clicked() {
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
}

// ── Field renderers ───────────────────────────────────────────────────────────

fn show_serial_fields(ui: &mut egui::Ui, conn: &mut ConnDraft, ports: &[String]) -> (bool, bool) {
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
                let r = ui.add(
                    egui::TextEdit::singleline(&mut conn.baud_custom)
                        .desired_width(68.0)
                        .hint_text("custom"),
                );
                if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                    if let Ok(b) = conn.baud_custom.parse::<u32>() {
                        if b > 0 {
                            conn.baud_rate = b;
                        }
                    }
                }
            });
            ui.end_row();
            if !conn.baud_custom.is_empty()
                && conn.baud_custom.parse::<u32>().map_or(true, |b| b == 0)
            {
                ui.label("");
                ui.label(err_text("enter a positive baud rate — e.g. 230400"));
                ui.end_row();
            }

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

fn show_udp_fields(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let before_mode = conn.udp_mode;
    let mut apply = false;

    egui::Grid::new("udp_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Mode");
            ui.horizontal(|ui| {
                ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Unicast, "Unicast");
                ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Broadcast, "Broadcast");
                ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Multicast, "Multicast");
            });
            ui.end_row();

            match conn.udp_mode {
                UdpModeDraft::Unicast | UdpModeDraft::Broadcast => {
                    ui.label("Destination");
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut conn.udp_dest)
                            .desired_width(220.0)
                            .hint_text("host:port  (Enter to apply)"),
                    );
                    if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                        apply = true;
                    }
                    ui.end_row();
                    if !conn.udp_dest.is_empty() && conn.udp_dest.parse::<SocketAddr>().is_err() {
                        ui.label("");
                        ui.label(err_text("enter host:port — e.g. 192.168.1.100:4000"));
                        ui.end_row();
                    }
                }
                UdpModeDraft::Multicast => {
                    ui.label("Group");
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut conn.udp_group)
                            .desired_width(140.0)
                            .hint_text("239.x.x.x  (Enter to apply)"),
                    );
                    if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                        apply = true;
                    }
                    ui.end_row();
                    if !conn.udp_group.is_empty() && conn.udp_group.parse::<Ipv4Addr>().is_err() {
                        ui.label("");
                        ui.label(err_text("enter IPv4 multicast address — e.g. 239.0.0.1"));
                        ui.end_row();
                    }

                    ui.label("Port");
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut conn.udp_mc_port)
                            .desired_width(80.0)
                            .hint_text("port"),
                    );
                    if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                        apply = true;
                    }
                    ui.end_row();
                    if !conn.udp_mc_port.is_empty() && conn.udp_mc_port.parse::<u16>().is_err() {
                        ui.label("");
                        ui.label(err_text("enter a port number 1–65535"));
                        ui.end_row();
                    }
                }
            }

            ui.label("Local port");
            let r = ui.add(
                egui::TextEdit::singleline(&mut conn.local_port)
                    .desired_width(80.0)
                    .hint_text("auto"),
            );
            if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                apply = true;
            }
            ui.end_row();
            if !conn.local_port.is_empty() && conn.local_port.parse::<u16>().is_err() {
                ui.label("");
                ui.label(err_text("enter a port number 1–65535"));
                ui.end_row();
            }
        });

    apply || (conn.udp_mode != before_mode)
}

fn level_color(level: tracing::Level) -> egui::Color32 {
    match level {
        tracing::Level::ERROR => egui::Color32::from_rgb(220, 80, 80),
        tracing::Level::WARN => egui::Color32::from_rgb(220, 180, 60),
        tracing::Level::DEBUG => egui::Color32::from_rgb(130, 130, 130),
        tracing::Level::TRACE => egui::Color32::from_rgb(100, 100, 100),
        _ => egui::Color32::from_rgb(200, 200, 200),
    }
}

fn err_text(msg: &str) -> egui::RichText {
    egui::RichText::new(msg).color(egui::Color32::RED).small()
}

fn hex_valid(s: &str) -> bool {
    let stripped: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    !stripped.is_empty()
        && stripped.len().is_multiple_of(2)
        && stripped.chars().all(|c| c.is_ascii_hexdigit())
}

fn show_tcp_fields(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let mut apply = false;

    egui::Grid::new("tcp_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Address");
            let r = ui.add(
                egui::TextEdit::singleline(&mut conn.tcp_addr)
                    .desired_width(220.0)
                    .hint_text("host:port  (Enter to apply)"),
            );
            if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                apply = true;
            }
            ui.end_row();
            if !conn.tcp_addr.is_empty() && conn.tcp_addr.parse::<SocketAddr>().is_err() {
                ui.label("");
                ui.label(err_text("enter host:port — e.g. 192.168.1.100:4000"));
                ui.end_row();
            }
        });

    apply
}
