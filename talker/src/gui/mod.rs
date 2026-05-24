mod display;
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
    message::{segments, ChecksumAlgorithm, CodePage, NmeaChecksumMode, Segment},
    profile::Profile,
    scheduler::Schedule,
};

use display::{ChannelDisplay, ControlStyle, DisplayMode};
use draft::{ConnDraft, ConnKind, PayloadKind, PortHold, ScheduleDraft, UdpModeDraft};
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
    displays: Vec<ChannelDisplay>,
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
        set_high_contrast_dark_visuals(ctx);
        install_control_pictures_fallback_font(ctx);
        bump_non_monospace_text_size(ctx, 0.5);
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
            displays: Vec::new(),
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
                self.displays = (0..n).map(|_| ChannelDisplay::default()).collect();
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
        self.displays.clear();
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
                    TalkerStatus::Sent { count, payload } => {
                        if i < self.sent_counts.len() {
                            self.sent_counts[i] = count;
                        }
                        if i < self.conn_errors.len() {
                            self.conn_errors[i] = None;
                        }
                        if let Some(d) = self.displays.get_mut(i) {
                            d.push(payload);
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
            .stroke(egui::Stroke::new(
                1.5,
                egui::Color32::from_rgb(110, 120, 145),
            ))
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
                            "Add at least one valid channel and one message",
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
                            "\u{2022} All running".to_string()
                        } else {
                            format!("\u{2022} {running}/{total} running")
                        },
                    )
                } else {
                    (egui::Color32::GRAY, "\u{2022} Stopped".to_string())
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
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(140, 160, 200));
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
                            ui.colored_label(dot_color, "\u{2022}")
                                .on_hover_text(dot_tip);

                            ui.strong(format!("Channel {}", i + 1));
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
                            ui.separator();
                            // Give each widget after the radios a stable
                            // string-source id, so the conditional `pending`
                            // indicator's appearance / disappearance can't
                            // shift sibling auto-ids between layout passes.
                            ui.push_id("iface_summary", |ui| {
                                ui.weak(interface_summary(&self.conn_drafts[i]));
                            });
                            let pending = i < self.profile.channels.len()
                                && self.conn_drafts[i]
                                    .to_config()
                                    .is_some_and(|cfg| cfg != self.profile.channels[i].interface);
                            ui.push_id("pending_indicator", |ui| {
                                if pending {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(220, 180, 60),
                                        "(unapplied — press Enter)",
                                    )
                                    .on_hover_text(
                                        "Interface parameters have been edited but not \
                                         yet applied to the running channel. Press Enter \
                                         in the edited field — or stop and start the \
                                         channel — to apply them.",
                                    );
                                }
                            });
                            ui.push_id("channel_actions", |ui| {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui
                                        .button(egui::RichText::new("\u{00D7}").size(18.0).strong())
                                        .on_hover_text("Remove this channel")
                                        .clicked()
                                    {
                                        to_remove = Some(i);
                                    }
                                    if running {
                                        if ui
                                            .small_button("\u{25a0}")
                                            .on_hover_text("Stop")
                                            .clicked()
                                        {
                                            to_stop = Some(i);
                                        }
                                    } else {
                                        let can = self.can_start_connection(i);
                                        // Compute the disabled-hover tip *before* the
                                        // widget is added, so we can chain
                                        // `on_disabled_hover_text` directly on the
                                        // Response — egui only displays the tooltip
                                        // when the call is part of the same Response
                                        // chain as the widget add.
                                        let tip = if !can {
                                            let t = start_blockers(
                                                &self.conn_drafts[i],
                                                &self.sched_drafts[i],
                                            )
                                            .join("\n");
                                            if t.is_empty() {
                                                "Add a valid message first".to_string()
                                            } else {
                                                t
                                            }
                                        } else {
                                            String::new()
                                        };
                                        let mut btn = ui.add_enabled(
                                            can,
                                            egui::Button::new("\u{25b6}").small(),
                                        );
                                        if !can {
                                            btn = btn.on_disabled_hover_text(tip);
                                        }
                                        if btn.clicked() {
                                            to_start = Some(i);
                                        }
                                    }
                                });
                            });
                        });
                        ui.separator();

                        let (changed, refresh) = match self.conn_drafts[i].kind {
                            // Each kind gets its own push_id namespace so
                            // the very different widget trees produced by
                            // Serial / UDP / TCP can't shift each other's
                            // auto-ids across egui's two layout passes.
                            ConnKind::Serial => {
                                ui.push_id("serial_body", |ui| {
                                    show_serial_fields(
                                        ui,
                                        &mut self.conn_drafts[i],
                                        &self.serial_ports,
                                    )
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
                        };
                        if changed {
                            to_apply.push(i);
                        }
                        if refresh {
                            do_refresh_ports = true;
                        }

                        ui.separator();
                        let interval_changes =
                            show_schedule_section(ui, &mut self.sched_drafts[i], &mut self.dirty);
                        for (msg_index, interval_ms) in interval_changes {
                            if let Some(Some(handle)) = self.talkers.get(i) {
                                let _ = handle.cmd_tx.try_send(TalkerCommand::SetInterval {
                                    index: msg_index,
                                    interval_ms,
                                });
                            }
                        }

                        ui.separator();
                        show_display_pane(ui, &mut self.displays[i]);
                    });
                });
                ui.add_space(6.0);
            }
            if ui.button("+ Add Channel").clicked() {
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
            self.displays.remove(i);
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
            self.displays.push(ChannelDisplay::default());
            self.dirty = true;
        }
        if do_refresh_ports {
            self.refresh_serial_ports();
        }
    }
}

// ── Inline message editor (one section per channel card) ──────────────────────

fn show_schedule_section(
    ui: &mut egui::Ui,
    entries: &mut Vec<ScheduleDraft>,
    dirty: &mut bool,
) -> Vec<(usize, u64)> {
    let mut to_remove: Option<usize> = None;
    let mut add_one = false;
    // Message indices whose interval was committed this frame, with the new value.
    let mut interval_changes: Vec<(usize, u64)> = Vec::new();

    ui.collapsing("Messages", |ui| {
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
                            if ui
                                .button(egui::RichText::new("\u{00D7}").size(18.0).strong())
                                .on_hover_text("Remove this message")
                                .clicked()
                            {
                                to_remove = Some(i);
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

                            let bad_interval = !entry.interval_ms.is_empty()
                                && entry.interval_ms.parse::<u64>().is_err();
                            ui.label("Interval (ms)");
                            let interval_resp =
                                red_bordered(ui, bad_interval, "must be a whole number", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut entry.interval_ms)
                                            .id_salt("interval_ms")
                                            .desired_width(80.0),
                                    )
                                });
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
fn show_payload_fields(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
    match entry.payload_kind {
        PayloadKind::Hex => {
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
        PayloadKind::Utf8 => {
            ui.label("Text");
            ui.horizontal(|ui| {
                let mut layouter = marker_layouter;
                ui.add(
                    egui::TextEdit::singleline(&mut entry.utf8_text)
                        .id_salt("payload_utf8")
                        .desired_width(300.0)
                        .hint_text("Unicode text")
                        .layouter(&mut layouter),
                );
                show_insert_byte_button(ui, &mut entry.utf8_text, &mut entry.insert_byte_hex);
            });
            ui.end_row();
        }
        PayloadKind::Utf16 => {
            ui.label("Text");
            ui.add(
                egui::TextEdit::singleline(&mut entry.utf16_text)
                    .id_salt("payload_utf16")
                    .desired_width(360.0)
                    .hint_text("Unicode text"),
            );
            ui.end_row();
            ui.label("Byte order");
            ui.horizontal(|ui| {
                ui.radio_value(&mut entry.utf16_big_endian, true, "Big-endian");
                ui.radio_value(&mut entry.utf16_big_endian, false, "Little-endian");
                ui.separator();
                ui.checkbox(&mut entry.utf16_bom, "BOM");
            });
            ui.end_row();
        }
        PayloadKind::Ascii => {
            ui.label("Text");
            ui.horizontal(|ui| {
                let mut layouter = marker_layouter;
                ui.add(
                    egui::TextEdit::singleline(&mut entry.ascii_text)
                        .id_salt("payload_ascii")
                        .desired_width(300.0)
                        .hint_text("text")
                        .layouter(&mut layouter),
                );
                show_insert_byte_button(ui, &mut entry.ascii_text, &mut entry.insert_byte_hex);
            });
            ui.end_row();
            ui.label("Code page");
            egui::ComboBox::from_id_salt("code_page")
                .selected_text(code_page_label(entry.ascii_code_page))
                .show_ui(ui, |ui| {
                    for cp in [
                        CodePage::Iso8859_1,
                        CodePage::Windows1252,
                        CodePage::Cp437,
                        CodePage::MacRoman,
                    ] {
                        ui.selectable_value(&mut entry.ascii_code_page, cp, code_page_label(cp));
                    }
                });
            ui.end_row();
        }
        PayloadKind::Nmea => {
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
    }
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
fn port_step(port_field: &mut String, direction: i8) -> bool {
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

/// Render `bytes` as a single-line preview string.
///
/// Every printable ASCII byte (`0x20..=0x7E`) is emitted as-is; **every
/// other byte** — control characters, CR/LF, anything ≥ 0x80, and the
/// individual bytes of any multi-byte UTF-8 sequence — becomes a `‹XX›`
/// marker. This guarantees that the bundled fonts (Hack / Ubuntu-Light
/// plus the Cascadia Control-Pictures subset) can render every glyph the
/// preview emits, so nothing tofus. The tradeoff: pretty Unicode display
/// is lost in the preview — `café` shows as `caf‹C3›‹A9›` — but the user
/// can see the exact bytes that will go on the wire, which matters more
/// for a tool like this.
///
/// Embedded `\r` and `\n` therefore appear as `‹0D›‹0A›` (visible, no
/// real line break), so the preview always renders on a single line and
/// no separate trim step is needed.
fn preview_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..=0x7E).contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("\u{2039}{b:02X}\u{203A}"));
        }
    }
    out
}

/// `egui::TextBuffer` wrapper around a `&mut String` that **uppercases every
/// character on insert**, so a hex field's value can never momentarily contain
/// a lowercase letter (no one-frame flash between keystroke and post-hoc
/// `to_ascii_uppercase`). Used for the message editor's Hex data field.
struct UppercaseHex<'a>(&'a mut String);

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

/// One-line, human-readable summary of a channel's selected interface and
/// its parameters, shown in the channel-card header so the active config
/// is visible at a glance without expanding the editor. Uses the draft's
/// current strings — invalid or missing parts show as `?`.
fn interface_summary(conn: &ConnDraft) -> String {
    fn or_q(s: &str) -> &str {
        if s.is_empty() {
            "?"
        } else {
            s
        }
    }
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
                or_q(&conn.serial_port),
                conn.baud_rate,
                data,
                parity,
                stop,
                flow,
            )
        }
        ConnKind::Udp => match conn.udp_mode {
            UdpModeDraft::Unicast => format!("UDP unicast {}{lp}", or_q(&conn.udp_dest)),
            UdpModeDraft::Broadcast => format!(
                "UDP broadcast {}:{}{lp}",
                or_q(&conn.udp_broadcast_addr),
                or_q(&conn.udp_broadcast_port),
            ),
            UdpModeDraft::Multicast => format!(
                "UDP multicast {}:{}{lp}",
                or_q(&conn.udp_group),
                or_q(&conn.udp_mc_port),
            ),
        },
        ConnKind::Tcp => format!("TCP {}", or_q(&conn.tcp_addr)),
    }
}

/// Enumerate the specific reasons the Start button is disabled for a
/// channel — one human-readable line per problem. Returned in the same
/// order they appear in the editor (channel fields first, then per-message
/// issues from top to bottom).
fn start_blockers(conn: &ConnDraft, messages: &[ScheduleDraft]) -> Vec<String> {
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
            match conn.udp_mode {
                UdpModeDraft::Unicast => {
                    if conn.udp_dest.is_empty() {
                        out.push("Channel: destination is empty".to_string());
                    } else if conn.udp_dest.parse::<SocketAddr>().is_err() {
                        out.push("Channel: destination must be host:port".to_string());
                    }
                }
                UdpModeDraft::Broadcast => {
                    if conn.udp_broadcast_addr.is_empty()
                        || conn.udp_broadcast_addr.parse::<Ipv4Addr>().is_err()
                    {
                        out.push("Channel: broadcast address must be IPv4".to_string());
                    }
                    if conn.udp_broadcast_port.is_empty()
                        || conn.udp_broadcast_port.parse::<u16>().is_err()
                    {
                        out.push("Channel: broadcast port must be 1–65535".to_string());
                    }
                }
                UdpModeDraft::Multicast => {
                    if conn.udp_group.is_empty() || conn.udp_group.parse::<Ipv4Addr>().is_err() {
                        out.push("Channel: multicast group must be IPv4".to_string());
                    }
                    if conn.udp_mc_port.is_empty() || conn.udp_mc_port.parse::<u16>().is_err() {
                        out.push("Channel: multicast port must be 1–65535".to_string());
                    }
                }
            }
            if !conn.local_port.is_empty() && conn.local_port.parse::<u16>().is_err() {
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
/// get for non-UTF-8 bytes — Hack and Ubuntu-Light don't include
/// U+FFFD glyphs.
fn show_message_preview(ui: &mut egui::Ui, entry: &ScheduleDraft) {
    // 2024-01-01T12:00:00.000Z — a fixed, recognisable sample instant.
    let reference = chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_110_400, 0).unwrap();
    ui.horizontal(|ui| {
        ui.label("Preview:").on_hover_text(
            "Sample of the wire bytes that would be sent. \
                 Timestamps use a fixed reference instant so the value \
                 doesn't tick — the actual send uses the wall clock.",
        );
        let text = match entry.to_message_config().and_then(|m| m.compile().ok()) {
            Some(compiled) => {
                let bytes = compiled.render_at(reference);
                match entry.payload_kind {
                    PayloadKind::Utf8 | PayloadKind::Ascii | PayloadKind::Nmea => {
                        preview_text(&bytes)
                    }
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
            ui.checkbox(&mut entry.ts_timezone, "Timezone");
        }
    });
}

/// Render the per-message checksum controls. See [`show_timestamp_editor`]
/// for the separator hierarchy rationale.
fn show_checksum_editor(ui: &mut egui::Ui, entry: &mut ScheduleDraft) {
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

fn code_page_label(code_page: CodePage) -> &'static str {
    match code_page {
        CodePage::Iso8859_1 => "ISO-8859-1",
        CodePage::Windows1252 => "Windows-1252",
        CodePage::Cp437 => "CP437",
        CodePage::MacRoman => "Mac OS Roman",
    }
}

fn checksum_label(algorithm: ChecksumAlgorithm) -> &'static str {
    match algorithm {
        ChecksumAlgorithm::Xor => "XOR",
        ChecksumAlgorithm::Crc8 => "CRC-8",
        ChecksumAlgorithm::Crc16Ccitt => "CRC-16/CCITT",
        ChecksumAlgorithm::Crc16Modbus => "CRC-16/MODBUS",
        ChecksumAlgorithm::Crc32 => "CRC-32",
    }
}

// ── Theme ─────────────────────────────────────────────────────────────────────

/// Force dark mode and lift the default text + separator contrast so every
/// element reads clearly against the dark background.
///
/// The custom RGB borders elsewhere in this file (outer panel, channel cards)
/// are tuned for this dark theme, so the theme is set explicitly rather than
/// left to inherit from the OS. The visuals are written into *both* the Dark
/// and Light theme slots so `eframe`'s persistence cannot resurrect an older
/// style on the next launch.
fn set_high_contrast_dark_visuals(ctx: &egui::Context) {
    let body = egui::Color32::from_gray(230);
    let separator = egui::Stroke::new(1.0, egui::Color32::from_gray(90));

    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(body);
    v.widgets.noninteractive.fg_stroke.color = body;
    v.widgets.inactive.fg_stroke.color = body;
    v.widgets.noninteractive.bg_stroke = separator;

    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);
}

/// Add `delta` points to every text style in both themes *except* the
/// Monospace family, so the display pane's output data (rendered with
/// `RichText::monospace()`) keeps its original size while the rest of the
/// UI (labels, buttons, headings, byte markers) is slightly larger.
fn bump_non_monospace_text_size(ctx: &egui::Context, delta: f32) {
    ctx.all_styles_mut(|style| {
        for font_id in style.text_styles.values_mut() {
            if font_id.family != egui::FontFamily::Monospace {
                font_id.size += delta;
            }
        }
    });
}

/// Register a Unicode Control Pictures fallback font.
///
/// The default `egui` fonts (Hack, Ubuntu-Light, NotoEmoji, emoji-icon) cover
/// zero glyphs in U+2400–U+243F, so the display pane's `Pictures` style would
/// otherwise render every control byte as a tofu box. We ship an ~19 KB
/// subset of Cascadia Mono containing exactly U+2400–U+2421 and register it
/// as a low-priority fallback for both the Monospace family (display pane)
/// and the Proportional family (the `␊` radio label in the controls bar).
fn install_control_pictures_fallback_font(ctx: &egui::Context) {
    const FONT: &[u8] = include_bytes!("../../assets/fonts/CascadiaMono-ControlPictures.ttf");
    ctx.add_font(egui::epaint::text::FontInsert::new(
        "control_pictures",
        egui::FontData::from_static(FONT),
        vec![
            egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Monospace,
                priority: egui::epaint::text::FontPriority::Lowest,
            },
            egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: egui::epaint::text::FontPriority::Lowest,
            },
        ],
    ));
}

// ── Field renderers ───────────────────────────────────────────────────────────

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

/// An "Insert Byte" button whose popup appends a `‹XX›` marker to `text`.
fn show_insert_byte_button(ui: &mut egui::Ui, text: &mut String, hex: &mut String) {
    // Default menu close behavior is `CloseOnClick`, which closes the menu
    // the moment the user clicks anywhere inside — including the TextEdit
    // (which has to be clicked to gain focus). Switch to `CloseOnClickOutside`
    // so the popup stays open while the user types the hex value.
    egui::containers::menu::MenuButton::new("Insert Byte")
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            ui.label("Byte value (hex):");
            let resp = ui.add(
                egui::TextEdit::singleline(hex)
                    .desired_width(48.0)
                    .hint_text("1B"),
            );
            let value = u8::from_str_radix(hex.trim(), 16).ok();
            let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let insert = ui.add_enabled(value.is_some(), egui::Button::new("Insert"));
            if let Some(b) = value {
                if insert.clicked() || entered {
                    text.push_str(&format!("\u{2039}{b:02X}\u{203A}"));
                    hex.clear();
                    ui.close();
                }
            }
        });
}

/// Render a channel's real-time outbound display pane (spec §5.7).
fn show_display_pane(ui: &mut egui::Ui, display: &mut ChannelDisplay) {
    ui.collapsing("Output", |ui| {
        ui.horizontal(|ui| {
            ui.label("View:").on_hover_text(
                "These are display modes — the bytes on the wire are the \
                 same regardless of which view is selected. The view only \
                 changes how the buffered bytes are rendered here.",
            );
            ui.radio_value(&mut display.mode, DisplayMode::Hex, "Hex");
            ui.radio_value(&mut display.mode, DisplayMode::Ascii, "ASCII");
            ui.radio_value(&mut display.mode, DisplayMode::Decoded, "Decoded");
            // Wrap the conditional ctrl-chars block in a stable id scope so
            // its appearance / disappearance can't shift the auto-derived
            // ids of the surrounding widgets (Clear button, etc.) and trip
            // egui's "duplicate widget id" warnings on view-mode changes.
            ui.push_id("ctrl_chars_block", |ui| {
                if display.mode == DisplayMode::Ascii {
                    ui.separator();
                    ui.label("ctrl-chars:");
                    ui.radio_value(
                        &mut display.control_style,
                        ControlStyle::Pictures,
                        "\u{240A}",
                    );
                    ui.radio_value(&mut display.control_style, ControlStyle::Brackets, "[LF]");
                    ui.radio_value(
                        &mut display.control_style,
                        ControlStyle::HexEscapes,
                        "<0x0A>",
                    );
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    display.clear();
                }
            });
        });
        ui.separator();
        ScrollArea::vertical()
            .max_height(150.0)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for line in display.lines() {
                    ui.label(egui::RichText::new(line).monospace());
                }
            });
    });
}

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
                if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
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

fn show_udp_fields(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let before_mode = conn.udp_mode;
    let mut apply = false;

    // Per-mode Grid id so the very different widget trees produced by
    // Unicast / Broadcast / Multicast each get their own id namespace —
    // see the equivalent message_grid_<kind> trick in the message editor.
    let grid_id = match conn.udp_mode {
        UdpModeDraft::Unicast => "udp_grid_unicast",
        UdpModeDraft::Broadcast => "udp_grid_broadcast",
        UdpModeDraft::Multicast => "udp_grid_multicast",
    };
    egui::Grid::new(grid_id)
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Mode");
            ui.horizontal(|ui| {
                ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Broadcast, "Broadcast");
                ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Unicast, "Unicast");
                ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Multicast, "Multicast");
            });
            ui.end_row();

            match conn.udp_mode {
                UdpModeDraft::Unicast => {
                    let bad =
                        !conn.udp_dest.is_empty() && conn.udp_dest.parse::<SocketAddr>().is_err();
                    ui.label("Destination");
                    let r = red_bordered(
                        ui,
                        bad,
                        "enter host:port — e.g. 192.168.1.100:4000",
                        |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut conn.udp_dest)
                                    .id_salt("udp_unicast_dest")
                                    .desired_width(220.0)
                                    .hint_text("host:port  (Enter to apply)"),
                            )
                        },
                    );
                    if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                        apply = true;
                    }
                    ui.end_row();
                }
                UdpModeDraft::Broadcast => {
                    let bad_addr = !conn.udp_broadcast_addr.is_empty()
                        && conn.udp_broadcast_addr.parse::<Ipv4Addr>().is_err();
                    let bad_port = !conn.udp_broadcast_port.is_empty()
                        && conn.udp_broadcast_port.parse::<u16>().is_err();
                    ui.label("Destination");
                    ui.horizontal(|ui| {
                        let addr_r = red_bordered(
                            ui,
                            bad_addr,
                            "enter an IPv4 address — e.g. 255.255.255.255",
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut conn.udp_broadcast_addr)
                                        .id_salt("udp_broadcast_addr")
                                        .desired_width(140.0)
                                        .hint_text("255.255.255.255"),
                                )
                            },
                        );
                        if addr_r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter))
                        {
                            apply = true;
                        }
                        ui.label("Port:");
                        let r_minus = ui
                            .small_button("\u{2212}")
                            .on_hover_text("Decrement port (hold to accelerate)");
                        let port_r =
                            red_bordered(ui, bad_port, "enter a port number 1–65535", |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut conn.udp_broadcast_port)
                                        .id_salt("udp_broadcast_port")
                                        .desired_width(60.0),
                                )
                            });
                        if port_r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter))
                        {
                            apply = true;
                        }
                        let r_plus = ui
                            .small_button("+")
                            .on_hover_text("Increment port (hold to accelerate)");
                        if drive_port_hold(
                            ui,
                            &mut conn.udp_port_hold,
                            &mut conn.udp_broadcast_port,
                            &r_minus,
                            &r_plus,
                        ) {
                            apply = true;
                        }
                    });
                    ui.end_row();
                }
                UdpModeDraft::Multicast => {
                    let bad_group =
                        !conn.udp_group.is_empty() && conn.udp_group.parse::<Ipv4Addr>().is_err();
                    let bad_port =
                        !conn.udp_mc_port.is_empty() && conn.udp_mc_port.parse::<u16>().is_err();
                    ui.label("Multicast group").on_hover_text(
                        "IPv4 multicast group address (must be in the 224.0.0.0 – \
                         239.255.255.255 range). Receivers must subscribe to the same \
                         group + port to see these packets. Common admin-local picks \
                         live in 239.x.x.x.",
                    );
                    ui.horizontal(|ui| {
                        let addr_r = red_bordered(
                            ui,
                            bad_group,
                            "enter IPv4 multicast address — e.g. 239.0.0.1",
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut conn.udp_group)
                                        .id_salt("udp_multicast_group")
                                        .desired_width(140.0)
                                        .hint_text("239.0.0.1"),
                                )
                            },
                        );
                        if addr_r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter))
                        {
                            apply = true;
                        }
                        ui.label("Port:");
                        let r_minus = ui
                            .small_button("\u{2212}")
                            .on_hover_text("Decrement port (hold to accelerate)");
                        let port_r =
                            red_bordered(ui, bad_port, "enter a port number 1–65535", |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut conn.udp_mc_port)
                                        .id_salt("udp_multicast_port")
                                        .desired_width(60.0),
                                )
                            });
                        if port_r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter))
                        {
                            apply = true;
                        }
                        let r_plus = ui
                            .small_button("+")
                            .on_hover_text("Increment port (hold to accelerate)");
                        if drive_port_hold(
                            ui,
                            &mut conn.udp_port_hold,
                            &mut conn.udp_mc_port,
                            &r_minus,
                            &r_plus,
                        ) {
                            apply = true;
                        }
                    });
                    ui.end_row();
                }
            }

            let bad_local = !conn.local_port.is_empty() && conn.local_port.parse::<u16>().is_err();
            ui.label("Local port");
            let r = red_bordered(ui, bad_local, "enter a port number 1–65535", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut conn.local_port)
                        .id_salt("udp_local_port")
                        .desired_width(80.0)
                        .hint_text("auto"),
                )
            });
            if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                apply = true;
            }
            ui.end_row();
        });

    apply || (conn.udp_mode != before_mode)
}

fn level_color(level: tracing::Level) -> egui::Color32 {
    match level {
        tracing::Level::ERROR => egui::Color32::from_rgb(220, 80, 80),
        tracing::Level::WARN => egui::Color32::from_rgb(220, 180, 60),
        tracing::Level::DEBUG => egui::Color32::from_gray(175),
        tracing::Level::TRACE => egui::Color32::from_gray(150),
        _ => egui::Color32::from_gray(235),
    }
}

/// Add a single text-input field through `add`, and when `invalid` is true
/// draw a red border around it and attach a hover tooltip with `msg`.
/// Returns the field's [`egui::Response`] so callers can still inspect
/// `.lost_focus()`, `.changed()`, etc.
///
/// This replaces the older "extra row of red text below the field" pattern
/// so the controls beneath an invalid field do not shift while the user
/// types.
fn red_bordered<F>(ui: &mut egui::Ui, invalid: bool, msg: &str, add: F) -> egui::Response
where
    F: FnOnce(&mut egui::Ui) -> egui::Response,
{
    // Always wrap in a `ui.scope` regardless of `invalid` — egui derives a
    // widget's id from its position in the ui tree, so a TextEdit that is
    // sometimes inside a scope and sometimes not gets a new id whenever
    // validity flips. The old code did that, which dropped keyboard focus
    // the moment a keystroke made the field invalid.
    let red = egui::Color32::from_rgb(220, 80, 80);
    let inner = ui.scope(|ui| {
        if invalid {
            let v = ui.visuals_mut();
            v.widgets.inactive.bg_stroke.color = red;
            v.widgets.inactive.bg_stroke.width = v.widgets.inactive.bg_stroke.width.max(1.0);
            v.widgets.hovered.bg_stroke.color = red;
            v.widgets.active.bg_stroke.color = red;
            v.selection.stroke.color = red;
        }
        add(ui)
    });
    if invalid {
        inner.inner.on_hover_text(msg)
    } else {
        inner.inner
    }
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
            let bad_tcp = !conn.tcp_addr.is_empty() && conn.tcp_addr.parse::<SocketAddr>().is_err();
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
            if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                apply = true;
            }
            ui.end_row();
        });

    apply
}
