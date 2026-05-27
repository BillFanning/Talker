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
    logging::{LogEvent, LogLevel, LogLevelHandle, LoggingConfig},
    message::{
        decode_codepage_byte, repair_after_edit, segments, ChecksumAlgorithm, CodePage,
        NmeaChecksumMode, Segment,
    },
    profile::Profile,
    scheduler::Schedule,
};

use display::{ChannelDisplay, ControlStyle, DisplayMode};
use draft::{ConnDraft, ConnKind, PayloadKind, PortHold, ScheduleDraft, UdpModeDraft};
use thread::{run_talker, TalkerCommand, TalkerHandle, TalkerStatus};

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(initial_profile: Option<PathBuf>) -> anyhow::Result<()> {
    let (log_tx, log_rx) = crossbeam_channel::bounded::<LogEvent>(512);
    // `logging` stays in scope until `run_native` returns so the
    // file-appender worker guards aren't dropped early. The reload
    // handle is cloned out for the GUI's log-level ComboBox.
    let logging = crate::core::logging::init(&LoggingConfig::default(), Some(log_tx))
        .context("initializing logging")?;
    let level_handle = logging.level_handle();

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
                level_handle,
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
    log_level: LogLevel,
    log_level_handle: LogLevelHandle,
    sent_counts: Vec<u64>,
    /// Per-message send counts. Outer index: channel. Inner index: message
    /// within the channel's compiled schedule. Grows on demand when a
    /// status arrives for an index past the current vector length.
    message_sent_counts: Vec<Vec<u64>>,
    displays: Vec<ChannelDisplay>,
    error_count: u64,
    last_title: String,
    serial_ports: Vec<String>,
    pixels_per_point: f32,
    zoom_held_timer: Option<f32>, // None = not held; Some(t) = held, t<0 in delay, t>=0 repeating
    /// Mutations that the channel-card render loop has requested. Drained at
    /// the END of each frame (after egui's layout passes complete) — never
    /// mid-frame — so the state changes can't cause widgets to appear,
    /// disappear, or change identity between egui's first and second layout
    /// passes (which trips the "Widget rect changed id between passes" warn).
    deferred: DeferredActions,
}

#[derive(Default)]
struct DeferredActions {
    apply: Vec<usize>,
    start: Option<usize>,
    stop: Option<usize>,
    remove: Option<usize>,
    add_channel: bool,
    refresh_ports: bool,
}

impl TalkerApp {
    fn new(
        log_rx: crossbeam_channel::Receiver<LogEvent>,
        initial_profile: Option<PathBuf>,
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
        log_level_handle: LogLevelHandle,
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
            log_level: LogLevel::default(),
            log_level_handle,
            sent_counts: Vec::new(),
            message_sent_counts: Vec::new(),
            displays: Vec::new(),
            error_count: 0,
            last_title: String::new(),
            serial_ports: Vec::new(),
            pixels_per_point: ppp,
            zoom_held_timer: None,
            deferred: DeferredActions::default(),
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

    /// Compare the draft for channel `i` against the applied config (what
    /// the talker thread is actually using). Returns `(interface_drift,
    /// message_drift)`:
    ///
    /// - `interface_drift`: the draft's interface params don't match the
    ///   applied interface. Can be applied live by pressing Enter (sends
    ///   `UpdateInterface` to the talker thread).
    /// - `message_drift`: the message list compiled from drafts differs
    ///   from the applied message list. Currently requires a stop+start
    ///   to apply — the scheduler is compiled at channel open time and
    ///   can't be hot-swapped today.
    fn detect_drift(&self, i: usize) -> (bool, bool) {
        let Some(applied) = self.profile.channels.get(i) else {
            return (false, false);
        };
        let iface_drift = self.conn_drafts[i]
            .to_config()
            .is_some_and(|cfg| cfg != applied.interface);
        let draft_messages: Vec<_> = self
            .sched_drafts
            .get(i)
            .map(|s| s.iter().filter_map(|d| d.to_message_config()).collect())
            .unwrap_or_default();
        let msg_drift = draft_messages != applied.messages;
        (iface_drift, msg_drift)
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
        // Profile name + dirty marker live next to the `Profile:`
        // text field in the top row — see `show_top_bar`. The title
        // bar is just the app identity.
        format!("Talker v{}", env!("CARGO_PKG_VERSION"))
    }

    // ── Profile actions ───────────────────────────────────────────────────────

    fn load_profile_from_path(&mut self, path: &Path) {
        self.stop_all();
        match Profile::load(path) {
            Ok(mut p) => {
                // The file root is the profile's name (`name` isn't
                // serialized — see `Profile::name`). Always overlay
                // from the path so renaming the file on disk is the
                // way to rename the profile.
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    p.name = stem.to_string();
                }
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
                self.message_sent_counts = (0..n)
                    .map(|j| vec![0u64; p.channels.get(j).map(|c| c.messages.len()).unwrap_or(0)])
                    .collect();
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
        self.message_sent_counts.clear();
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
            None => match self.pick_save_path() {
                Some(p) => p,
                None => return,
            },
        };
        self.write_profile_to(&path);
    }

    /// Always opens the native save dialog, so the user can fork the
    /// current profile to a new file. On success the new path becomes
    /// the bound `profile_path`, so subsequent plain Save writes there.
    fn save_profile_as(&mut self) {
        self.flush_drafts_to_profile();
        let Some(path) = self.pick_save_path() else {
            return;
        };
        self.write_profile_to(&path);
    }

    fn pick_save_path(&self) -> Option<PathBuf> {
        let stem = if self.profile.name.is_empty() {
            "profile"
        } else {
            &self.profile.name
        };
        let name = format!("{stem}.toml");
        let mut dialog = rfd::FileDialog::new()
            .add_filter("TOML Profile", &["toml"])
            .set_file_name(&name);
        // Seed the dialog at the current profile's directory so
        // Save As lands next to the original by default.
        if let Some(parent) = self.profile_path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(parent);
        }
        dialog.save_file()
    }

    fn write_profile_to(&mut self, path: &Path) {
        match self.profile.save(path) {
            Ok(()) => {
                self.profile_path = Some(path.to_path_buf());
                // Keep the in-memory display name in sync with the
                // file root — see [`Profile::name`]. Especially
                // matters after Save As to a new path.
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    self.profile.name = stem.to_string();
                }
                self.dirty = false;
                tracing::info!("profile '{}' saved", self.profile.name);
            }
            Err(e) => tracing::error!("save failed: {e:#}"),
        }
    }

    // ── Talker thread lifecycle ────────────────────────────────────────────────

    fn start_connection(&mut self, i: usize) {
        self.stop_connection(i);
        self.flush_drafts_to_profile();

        // Starting (or attempting to start) is an explicit commit —
        // flip the active UDP destination into strict validation so
        // missing / malformed fields surface as red immediately.
        if let Some(draft) = self.conn_drafts.get_mut(i) {
            if matches!(draft.kind, ConnKind::Udp) {
                let pair = match draft.udp_mode {
                    UdpModeDraft::Unicast => &mut draft.udp_unicast,
                    UdpModeDraft::Broadcast => &mut draft.udp_broadcast,
                    UdpModeDraft::Multicast => &mut draft.udp_multicast,
                };
                pair.submitted = true;
            }
        }

        // 1-based for log strings — matches the UI label "Channel N".
        let n = i + 1;

        let Some(cfg) = self.conn_drafts.get(i).and_then(|d| d.to_config()) else {
            tracing::warn!("channel {n} config invalid");
            return;
        };

        let interface = match cfg.open() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to open channel {n}: {e:#}");
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
                tracing::error!("channel {n} schedule error: {e:#}");
                return;
            }
        };

        if i < self.conn_errors.len() {
            self.conn_errors[i] = None;
        }
        if i < self.sent_counts.len() {
            self.sent_counts[i] = 0;
        }
        // Zero per-message counts, sized to the now-active schedule.
        let message_count = messages.len();
        if i < self.message_sent_counts.len() {
            self.message_sent_counts[i] = vec![0u64; message_count];
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
        tracing::info!("channel {n} started ({message_count}-message schedule)");
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
        let (new, load, save, save_as) = ctx.input(|inp| {
            let ctrl = inp.modifiers.ctrl || inp.modifiers.mac_cmd;
            let shift = inp.modifiers.shift;
            (
                ctrl && !shift && inp.key_pressed(egui::Key::N),
                ctrl && !shift && inp.key_pressed(egui::Key::O),
                ctrl && !shift && inp.key_pressed(egui::Key::S),
                ctrl && shift && inp.key_pressed(egui::Key::S),
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
        if save_as {
            self.save_profile_as();
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
                    TalkerStatus::Sent {
                        message_index,
                        message_count,
                        total_count,
                        payload,
                    } => {
                        if i < self.sent_counts.len() {
                            self.sent_counts[i] = total_count;
                        }
                        if let Some(per_msg) = self.message_sent_counts.get_mut(i) {
                            if message_index >= per_msg.len() {
                                per_msg.resize(message_index + 1, 0);
                            }
                            per_msg[message_index] = message_count;
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
        // Apply user-requested mutations AFTER the layout closes — never
        // inside it — so egui's two-pass layout sees one consistent state.
        self.process_deferred();
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
                // Dirty marker: a small amber `*` painted at the
                // upper-left corner of the field. Uses the painter
                // (not a label) so it doesn't push surrounding
                // widgets around when it appears/disappears.
                if self.dirty {
                    let pos = r.rect.left_top() + egui::vec2(3.0, 1.0);
                    ui.painter().text(
                        pos,
                        egui::Align2::LEFT_TOP,
                        "*",
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_rgb(220, 180, 60),
                    );
                }
                if ui.button("New").clicked() {
                    self.new_profile();
                }
                if ui.button("Load\u{2026}").clicked() {
                    self.load_profile_dialog();
                }
                if ui.button("Save").on_hover_text("Ctrl+S").clicked() {
                    self.save_profile();
                }
                if ui
                    .button("Save As\u{2026}")
                    .on_hover_text("Ctrl+Shift+S — write to a new file")
                    .clicked()
                {
                    self.save_profile_as();
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
                    ui.separator();
                    ui.label("Level:");
                    let before = self.log_level;
                    egui::ComboBox::from_id_salt("log_level")
                        .selected_text(self.log_level.as_str())
                        .show_ui(ui, |ui| {
                            for lvl in [
                                LogLevel::Trace,
                                LogLevel::Debug,
                                LogLevel::Info,
                                LogLevel::Warn,
                                LogLevel::Error,
                            ] {
                                ui.selectable_value(&mut self.log_level, lvl, lvl.as_str());
                            }
                        });
                    if self.log_level != before {
                        // Don't log on success — the new filter may hide
                        // an info-level confirmation, and the ComboBox
                        // itself shows the active level.
                        if let Err(e) = self.log_level_handle.set(self.log_level) {
                            tracing::error!("log level change failed: {e:#}");
                        }
                    }
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
        ScrollArea::vertical().show(ui, |ui| {
            let n = self.conn_drafts.len();
            for i in 0..n {
                ui.push_id(i, |ui| {
                    self.show_channel_card(ui, i);
                });
                ui.add_space(6.0);
            }
            if ui.button("+ Add Channel").clicked() {
                self.deferred.add_channel = true;
            }
        });
    }

    fn show_channel_card(&mut self, ui: &mut egui::Ui, i: usize) {
        let mut conn_frame = egui::Frame::group(ui.style());
        conn_frame.stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(140, 160, 200));
        conn_frame.show(ui, |ui| {
            let running = self.is_connection_running(i);
            ui.horizontal(|ui| {
                self.show_channel_header(ui, i, running);
            });
            ui.separator();
            self.show_channel_body(ui, i, running);
            ui.separator();
            show_display_pane(ui, &mut self.displays[i]);
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

        ui.strong(format!("Channel {}", i + 1));
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
        // Confirm step (only relevant when the profile is dirty —
        // removing a clean profile's channel is reversible by Load).
        // Mirrors the per-message ✕ confirm UI: Cancel restores the
        // ✕, red "Remove" commits.
        if self.conn_drafts[i].pending_remove {
            if ui
                .small_button("Cancel")
                .on_hover_text("Keep this channel")
                .clicked()
            {
                self.conn_drafts[i].pending_remove = false;
            }
            let confirm = egui::Button::new(
                egui::RichText::new("Remove")
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(egui::Color32::from_rgb(180, 60, 60));
            if ui
                .add(confirm)
                .on_hover_text("Remove this channel; unsaved profile changes will be lost")
                .clicked()
            {
                self.deferred.remove = Some(i);
            }
            ui.label(
                egui::RichText::new("Discard channel?")
                    .color(egui::Color32::from_rgb(220, 180, 80)),
            );
        } else if ui
            .button(egui::RichText::new("\u{00D7}").size(18.0).strong())
            .on_hover_text("Remove this channel")
            .clicked()
        {
            if self.dirty {
                self.conn_drafts[i].pending_remove = true;
            } else {
                self.deferred.remove = Some(i);
            }
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
        // Auto-collapse the connection editor on Start, auto-expand on
        // Stop. The transition is detected by comparing this frame's
        // `running` against the previous frame's value stashed in egui
        // memory. Between transitions, the persistent CollapsingHeader
        // state honours whatever the user clicks — so mid-run edits are
        // still possible by manually expanding, and the choice sticks
        // until the next start/stop.
        let prev_id = ui.id().with(("conn_section_prev_running", i));
        let prev_running = ui
            .memory(|m| m.data.get_temp::<bool>(prev_id))
            .unwrap_or(running);
        let force_open = if running && !prev_running {
            Some(false)
        } else if !running && prev_running {
            Some(true)
        } else {
            None
        };
        ui.memory_mut(|m| m.data.insert_temp(prev_id, running));

        let (changed, refresh) = egui::CollapsingHeader::new(if running {
            "Connection (running — expand to edit)"
        } else {
            "Connection"
        })
        .id_salt(("conn_section", i))
        .default_open(!running)
        .open(force_open)
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
        let interval_changes = show_schedule_section(
            ui,
            &mut self.sched_drafts[i],
            &mut self.dirty,
            per_message_counts,
            running,
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

    /// Apply every mutation queued on `self.deferred` during the just-
    /// completed egui layout. Called from the end of `ui()`, OUTSIDE any
    /// egui `show()` closure, so the state changes can't interleave with
    /// egui's two-pass layout.
    fn process_deferred(&mut self) {
        let mut d = std::mem::take(&mut self.deferred);
        // Dedup applies — multiple radio-clicks in one frame on the same
        // channel are pointless to apply twice.
        d.apply.sort_unstable();
        d.apply.dedup();
        for i in d.apply {
            self.apply_connection(i);
        }
        if let Some(i) = d.start {
            self.start_connection(i);
        }
        if let Some(i) = d.stop {
            self.stop_connection(i);
        }
        if let Some(i) = d.remove {
            self.stop_connection(i);
            self.conn_drafts.remove(i);
            self.sched_drafts.remove(i);
            self.conn_errors.remove(i);
            self.talkers.remove(i);
            self.sent_counts.remove(i);
            if i < self.message_sent_counts.len() {
                self.message_sent_counts.remove(i);
            }
            self.displays.remove(i);
            if i < self.profile.channels.len() {
                self.profile.channels.remove(i);
            }
            self.dirty = true;
        }
        if d.add_channel {
            self.conn_drafts.push(ConnDraft::default());
            self.sched_drafts.push(Vec::new());
            self.conn_errors.push(None);
            self.talkers.push(None);
            self.message_sent_counts.push(Vec::new());
            self.sent_counts.push(0);
            self.displays.push(ChannelDisplay::default());
            self.dirty = true;
        }
        if d.refresh_ports {
            self.refresh_serial_ports();
        }
    }
}

// ── Inline message editor (one section per channel card) ──────────────────────

fn show_schedule_section(
    ui: &mut egui::Ui,
    entries: &mut Vec<ScheduleDraft>,
    dirty: &mut bool,
    per_message_counts: &[u64],
    channel_running: bool,
) -> Vec<(usize, u64)> {
    let mut to_remove: Option<usize> = None;
    let mut add_one = false;
    // Message indices whose interval was committed this frame, with the new value.
    let mut interval_changes: Vec<(usize, u64)> = Vec::new();

    // Collapsed by default — the editors for many channels eat a lot of
    // vertical real estate. The header summary keeps the at-a-glance
    // info (count, total sent) visible without expanding.
    //
    // `id_salt` keeps the persistent open/closed state stable even
    // though the title text changes every frame as `total_sent` ticks.
    // Without it CollapsingHeader derives its id from the label, so a
    // user expand would be forgotten on the next send.
    let total_sent: u64 = per_message_counts.iter().sum();
    let n = entries.len();
    let header = if n == 0 {
        "Messages — (none)".to_string()
    } else if channel_running {
        format!(
            "Messages — {n} message{}, Sent: {total_sent}",
            if n == 1 { "" } else { "s" }
        )
    } else {
        format!("Messages — {n} message{}", if n == 1 { "" } else { "s" })
    };
    egui::CollapsingHeader::new(header)
        .id_salt("messages_section")
        .default_open(false)
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
    // Printable ASCII passes through; anything else (control bytes
    // and high bytes alike) is opaque to this preview, so render it
    // as the familiar `‹XX›` byte marker.
    preview_with(bytes, |b| (0x20..=0x7E).contains(&b).then_some(b as char))
}

/// Preview text for an `Ascii` payload, decoding high bytes through
/// `code_page` so the user sees what a code-page-aware receiver would
/// render. Control bytes (0x00–0x1F and 0x7F) still show as `‹XX›`
/// byte markers so they're never invisible.
fn preview_ascii(bytes: &[u8], code_page: CodePage) -> String {
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

/// Amber pill in the channel-card header when the user has edited fields
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

/// Render [`interface_summary`] in the channel-card header, splitting on `?`
/// markers so unknown / unfilled fields show up as a **bold red** glyph
/// rather than blending into the rest of the weak-grey summary text.
fn show_interface_summary(ui: &mut egui::Ui, conn: &ConnDraft) {
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
/// its parameters, shown in the channel-card header so the active config
/// is visible at a glance without expanding the editor. Uses the draft's
/// current strings — invalid or missing parts show as `?`.
fn interface_summary(conn: &ConnDraft) -> String {
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
    let (dot_color, state) = if channel_running {
        (egui::Color32::from_rgb(80, 200, 80), "Active")
    } else {
        (egui::Color32::from_gray(140), "Idle")
    };
    let bg = if channel_running {
        egui::Color32::from_rgb(28, 52, 28)
    } else {
        egui::Color32::from_gray(40)
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
fn marker_aware_text_edit(
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
fn plain_text_edit_with_cursor(
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
fn show_insert_byte_button(
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
fn show_insert_unit_button(
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
            // Hint deliberately uses BMP codepoints the default
            // egui font (Latin-1 + Control Pictures) can render —
            // `00E9` is `é`, `00B5` is `µ`. Asian codepoints like
            // `0E16` (`ฃ`) show as tofu unless a wider fallback
            // font is bundled.
            button_label: "Insert Code Unit",
            field_label: "Code unit(s) (4 hex):",
            hint: "00E9  or  00E9 00B5",
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
/// [`marker_aware_text_edit`] (or `marker_aware_text_edit_opts`) for
/// the field this drives — that's how the popup (rendered under a
/// different ui parent) finds the target's cursor and widget id in
/// egui memory.
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
                // Render all messages as ONE Label so consecutive sends
                // flow into each other instead of stacking on separate
                // rows. The per-message labels we used to render each
                // gained a row-of-padding inter-message gap that looked
                // like a stray newline.
                //
                // Separator between messages:
                //  - Hex: a single space, so byte groups stay readable
                //    ("34 0D 0A 34 0D 0A", not "34 0D 0A34 0D 0A").
                //  - Raw / Rendered: empty, so the wire-byte stream is
                //    shown verbatim. Any line breaks the user sees here
                //    come from the bytes themselves (a 0x0A in Rendered
                //    mode, etc.) — not synthesized by the display.
                let sep = if display.mode == DisplayMode::Hex {
                    " "
                } else {
                    ""
                };
                let combined: String = display.lines().collect::<Vec<_>>().join(sep);
                ui.add(
                    egui::Label::new(egui::RichText::new(combined).monospace())
                        .wrap()
                        .selectable(true),
                );
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

fn show_udp_fields(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
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

fn level_color(level: tracing::Level) -> egui::Color32 {
    match level {
        tracing::Level::ERROR => egui::Color32::from_rgb(220, 80, 80),
        tracing::Level::WARN => egui::Color32::from_rgb(220, 180, 60),
        tracing::Level::DEBUG => egui::Color32::from_gray(175),
        tracing::Level::TRACE => egui::Color32::from_gray(150),
        _ => egui::Color32::from_gray(235),
    }
}

/// "Broken value" red-box for any text-input field. Calls `add` to
/// render the field, and when `invalid` is true tints the fill pink,
/// paints a 2-px red outline just outside the widget rect, and
/// attaches `msg` as the hover tooltip. Returns the field's
/// [`Response`] unchanged so callers can still chain `lost_focus()`,
/// `changed()`, etc.
///
/// Replaces an earlier "red error row below the field" pattern that
/// pushed surrounding controls around as the user typed.
fn red_bordered<F>(ui: &mut egui::Ui, invalid: bool, msg: &str, add: F) -> egui::Response
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
fn enter_committed(r: &egui::Response, ui: &egui::Ui) -> bool {
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// True when `s` is a non-empty string that fails to parse as `T`.
/// Used to drive the red-border / start-blocker validation: empty means
/// "user hasn't typed anything here yet" (not an error to surface),
/// whereas non-empty + parse fail means the user typed something wrong.
fn invalid_parse<T>(s: &str) -> bool
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
