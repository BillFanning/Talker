mod channels;
mod detail;
mod display;
mod draft;
mod widgets;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context as _;
use egui::{Align, Layout, ScrollArea};

use crate::core::{
    channel::ChannelConfig,
    logging::{LogEvent, LogLevel, LogLevelHandle, LoggingConfig},
    profile::Profile,
    runner,
    scheduler::Schedule,
    supervisor::TalkerSupervisor,
};

use display::ChannelDisplay;
use draft::{ConnDraft, ConnKind, ScheduleDraft, UdpModeDraft};

// ── Entry point ───────────────────────────────────────────────────────────────

/// Detach the inherited console when going graphical. The `talker` binary is a
/// console-subsystem app so the headless CLI works when launched from a
/// terminal; the cost is that a double-click into the GUI leaves a console
/// window behind. Freeing it here removes that window. `talker-gui.exe` never
/// allocates one in the first place. No-op off Windows.
#[cfg(windows)]
fn detach_console() {
    // SAFETY: `FreeConsole` takes no arguments and is always safe to call; it
    // simply detaches the process from its console if it has one.
    unsafe {
        let _ = windows_sys::Win32::System::Console::FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console() {}

/// Launch the graphical interface.
///
/// **Shared GUI-startup funnel (two-binary invariant — see
/// `src/bin/talker-gui.rs`).** Both GUI entry points route through here: the
/// `talker-gui` (windows-subsystem) binary and `talker.exe`'s `--gui` path.
/// Put ALL GUI startup (console detach, logging, window options) in this
/// function so the two binaries stay identical — never in either `main`.
pub fn run(initial_profile: Option<PathBuf>) -> anyhow::Result<()> {
    detach_console(); // drop the double-click console before the window opens

    // Windows 11 would otherwise ignore the high-rate timer request while
    // the window is minimized — the usual state of a long soak (ADR-017).
    crate::core::timing::keep_timer_resolution_when_minimized();
    let (log_tx, log_rx) = crossbeam_channel::bounded::<LogEvent>(512);
    // `logging` stays in scope until `run_native` returns so the
    // file-appender worker guards aren't dropped early. The reload
    // handle is cloned out for the GUI's log-level ComboBox.
    let logging = crate::core::logging::init(&LoggingConfig::default(), Some(log_tx))
        .context("initializing logging")?;
    let level_handle = logging.level_handle();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 740.0])
            .with_min_inner_size([640.0, 480.0]),
        // Don't persist/restore window geometry (listener's lesson, ADR-016):
        // eframe restores the saved window state *after* the window is shown,
        // which produced a double frame/title-bar flash on launch — and a bad
        // tiny geometry could be restored too. The window always opens at the
        // default size; zoom and the last profile are persisted separately.
        persist_window: false,
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

/// The `pixels_per_point` the zoom widget treats as **100%**. This was
/// the comfortable default on the app's target displays — what the
/// widget used to label "115%". The widget now shows and steps zoom
/// relative to this baseline, so a fresh install opens at 100%.
const ZOOM_BASE_PPP: f32 = 1.15;
/// One zoom click = ±10 percentage points of [`ZOOM_BASE_PPP`].
const ZOOM_STEP_PPP: f32 = ZOOM_BASE_PPP * 0.10;
/// Zoom clamp range, as `pixels_per_point` (50%..=200% of the base).
const ZOOM_MIN_PPP: f32 = ZOOM_BASE_PPP * 0.5;
const ZOOM_MAX_PPP: f32 = ZOOM_BASE_PPP * 2.0;

// The status-queue bound moved into core with the supervisor (ADR-019); the
// detail header's performance readouts still reference it via this path.
pub(crate) use crate::core::supervisor::STATUS_QUEUE_CAP;

/// eframe-storage key for the newline-joined recent-profiles list.
const RECENT_PROFILES_KEY: &str = "recent_profiles";
/// How many entries the Profile menu's Recent section keeps.
const MAX_RECENT_PROFILES: usize = 8;

/// Convert a `pixels_per_point` value to the widget's displayed
/// percentage (relative to [`ZOOM_BASE_PPP`]), rounded to a whole number.
fn zoom_percent(ppp: f32) -> u32 {
    (ppp / ZOOM_BASE_PPP * 100.0).round() as u32
}

struct TalkerApp {
    /// Repaint-on-status coalescer shared with every runner thread: a status
    /// wakes the UI instantly, but N statuses between frames cost **one**
    /// winit wake (see `wiredata_ui::repaint`). Re-armed at the top of each
    /// frame, before the status drain.
    repaint: std::sync::Arc<wiredata_ui::repaint::RepaintCoalescer>,
    profile: Profile,
    profile_path: Option<PathBuf>,
    dirty: bool,
    conn_drafts: Vec<ConnDraft>,
    sched_drafts: Vec<Vec<ScheduleDraft>>,
    /// The channel collection (ADR-019): runner threads, command/status
    /// channels, draining buckets, and per-channel telemetry all live in
    /// core's supervisor — the GUI keeps only view-state and reads
    /// [`TalkerSupervisor::telemetry`] when rendering.
    sup: TalkerSupervisor,
    log_rx: crossbeam_channel::Receiver<LogEvent>,
    /// Buffered log lines paired with their level. The level (not a
    /// baked colour) is stored so the log re-colours live when the
    /// theme is toggled — see [`level_color`].
    log_lines: Vec<(String, tracing::Level)>,
    log_level: LogLevel,
    log_level_handle: LogLevelHandle,
    displays: Vec<ChannelDisplay>,
    last_title: String,
    serial_ports: Vec<String>,
    pixels_per_point: f32,
    zoom_held_timer: Option<f32>, // None = not held; Some(t) = held, t<0 in delay, t>=0 repeating
    /// `true` = dark theme, `false` = light. Persisted; toggled from
    /// the top-bar sun/moon button next to the zoom control.
    dark_mode: bool,
    /// Index of the channel shown in the detail pane. `None` only when
    /// there are no channels.
    selected: Option<usize>,
    /// Whether the channel list is collapsed to the thin status strip.
    channels_collapsed: bool,
    /// Per-channel msgs/s estimators for the channel-list rows.
    rates: Vec<RateTracker>,
    /// Per-channel log-event tallies for the channel-list rows.
    log_counts: Vec<LogCounts>,
    /// Per-severity display filters for the log panel (capture level is a
    /// separate concern — the Level ComboBox).
    show_info: bool,
    show_warn: bool,
    show_error: bool,
    /// Recently loaded/saved profile paths, most recent first (max
    /// [`MAX_RECENT_PROFILES`]); persisted via eframe storage.
    recent_profiles: Vec<PathBuf>,
    /// A pending "remove this channel?" confirmation; `Some(index)` while
    /// the modal is up.
    confirm_remove: Option<usize>,
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
    /// Switch the detail pane to this channel (from a list-row click).
    select: Option<usize>,
    start: Option<usize>,
    stop: Option<usize>,
    start_all: bool,
    stop_all: bool,
    remove: Option<usize>,
    /// Add a channel of this kind (from the list header's `+ Add` menu).
    add_channel: Option<ConnKind>,
    refresh_ports: bool,
}

/// Per-channel tallies of log events attributed via the structured
/// `channel` tracing field (see `LogEvent::channel`), shown on the
/// channel-list rows. Reset when the channel starts, like the send counts.
#[derive(Clone, Copy, Default)]
struct LogCounts {
    info: u32,
    warn: u32,
    error: u32,
}

/// Lightweight throughput estimator for a channel: samples the cumulative
/// sent count and byte total over a ~1 s window and reports the delta rates
/// (msgs/s for the channel-list row, both for the detail header).
#[derive(Clone, Copy)]
struct RateTracker {
    last_sample: Instant,
    last_total: u64,
    last_bytes: u64,
    per_sec: f32,
    bytes_per_sec: f32,
}

impl RateTracker {
    fn new() -> Self {
        Self {
            last_sample: Instant::now(),
            last_total: 0,
            last_bytes: 0,
            per_sec: 0.0,
            bytes_per_sec: 0.0,
        }
    }

    fn sample(&mut self, now: Instant, total: u64, bytes: u64, running: bool) {
        if !running {
            self.per_sec = 0.0;
            self.bytes_per_sec = 0.0;
            self.last_total = total;
            self.last_bytes = bytes;
            self.last_sample = now;
            return;
        }
        let dt = now.duration_since(self.last_sample).as_secs_f32();
        if dt >= 1.0 {
            self.per_sec = total.saturating_sub(self.last_total) as f32 / dt;
            self.bytes_per_sec = bytes.saturating_sub(self.last_bytes) as f32 / dt;
            self.last_total = total;
            self.last_bytes = bytes;
            self.last_sample = now;
        }
    }
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
            // First run opens at 100% on the new scale (= ZOOM_BASE_PPP).
            .unwrap_or(ZOOM_BASE_PPP);
        ctx.set_pixels_per_point(ppp);
        // Default to dark; persisted across runs. Stored as the string
        // "false" only when the user has switched to light.
        let dark_mode = storage
            .and_then(|s| s.get_string("dark_mode"))
            .map(|s| s != "false")
            .unwrap_or(true);
        // The shared wiredata look (ADR-016): font stack, both themes'
        // visuals, and the style tweaks come from `wiredata-ui`, so talker
        // and listener read as one product. Talker keeps its dark/light
        // toggle; `apply_theme` just picks which installed theme is active.
        wiredata_ui::fonts::install_fonts(ctx);
        wiredata_ui::style::install_visuals(ctx);
        wiredata_ui::style::apply_style_tweaks(ctx);
        apply_theme(ctx, dark_mode);
        let repaint = wiredata_ui::repaint::RepaintCoalescer::for_ctx(ctx.clone());
        // Sampled lanes (ADR-018): the GUI's display cost stays constant
        // regardless of send rate; statuses wake the UI via the coalescer.
        let mut sup = TalkerSupervisor::new(runner::ObserverPolicy::sampled());
        {
            let r = std::sync::Arc::clone(&repaint);
            sup.set_notify(std::sync::Arc::new(move || r.notify()));
        }
        let mut app = Self {
            repaint,
            profile: Profile::default(),
            profile_path: None,
            dirty: false,
            conn_drafts: Vec::new(),
            sched_drafts: Vec::new(),
            sup,
            log_rx,
            log_lines: Vec::new(),
            log_level: LogLevel::default(),
            log_level_handle,
            displays: Vec::new(),
            last_title: String::new(),
            serial_ports: Vec::new(),
            pixels_per_point: ppp,
            zoom_held_timer: None,
            dark_mode,
            selected: None,
            channels_collapsed: false,
            rates: Vec::new(),
            log_counts: Vec::new(),
            show_info: true,
            show_warn: true,
            show_error: true,
            recent_profiles: storage
                .and_then(|s| s.get_string(RECENT_PROFILES_KEY))
                .map(|joined| {
                    joined
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default(),
            confirm_remove: None,
            deferred: DeferredActions::default(),
        };
        app.refresh_serial_ports();

        // CLI arg takes precedence; fall back to last path saved in
        // storage. The storage path is also filtered through
        // `Path::exists()` — when the file is gone (renamed, on a
        // disconnected drive, etc.) we skip the load and start
        // empty rather than logging the same "file not found"
        // error on every launch. `profile_path` stays `None`, so
        // the next `save()` overwrites the stale storage entry
        // with an empty string and the loop self-clears.
        let path = initial_profile.or_else(|| {
            storage
                .and_then(|s| s.get_string("last_profile_path"))
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .and_then(|p| {
                    if p.exists() {
                        Some(p)
                    } else {
                        tracing::warn!("last-used profile {p:?} is gone — opening empty");
                        None
                    }
                })
        });

        if let Some(p) = path {
            app.load_profile_from_path(&p);
        }

        app
    }

    fn is_connection_running(&self, i: usize) -> bool {
        self.sup.is_running(i)
    }

    fn can_start_connection(&self, i: usize) -> bool {
        !self.is_connection_running(i)
            && self
                .conn_drafts
                .get(i)
                .is_some_and(|d| d.to_config().is_some())
            && self.sched_drafts.get(i).is_some_and(|s| {
                // At least one message must convert, and everything that
                // converts must also compile (`validate` is the shared
                // core surface — e.g. bad hex, NMEA framing characters).
                let msgs: Vec<_> = s.iter().filter_map(|d| d.to_message_config()).collect();
                !msgs.is_empty() && msgs.iter().all(|m| m.validate().is_ok())
            })
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
                    .map(|ch| {
                        let mut d = ConnDraft::from(&ch.interface);
                        d.name = ch.name.clone();
                        d
                    })
                    .collect();
                self.sched_drafts = p
                    .channels
                    .iter()
                    .map(|ch| ch.messages.iter().map(ScheduleDraft::from).collect())
                    .collect();
                self.sup.resize_slots(0); // orphan any old runners, then size fresh
                self.sup.resize_slots(n);
                self.displays = (0..n).map(|_| ChannelDisplay::default()).collect();
                self.rates = vec![RateTracker::new(); n];
                self.log_counts = vec![LogCounts::default(); n];
                self.selected = if n > 0 { Some(0) } else { None };
                // Preflight the whole profile so any payload that won't
                // compile is surfaced now, not silently at the first Start.
                if let Err(e) = p.validate() {
                    tracing::warn!("profile '{}' has an invalid message: {e:#}", p.name);
                }
                self.profile = p;
                self.profile_path = Some(path.to_path_buf());
                self.push_recent(path);
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
        self.sup.resize_slots(0);
        self.displays.clear();
        self.rates.clear();
        self.log_counts.clear();
        self.selected = None;
        tracing::info!("new profile");
    }

    /// Move `path` to the front of the recent-profiles list (deduplicated,
    /// capped at [`MAX_RECENT_PROFILES`]).
    fn push_recent(&mut self, path: &Path) {
        self.recent_profiles.retain(|p| p != path);
        self.recent_profiles.insert(0, path.to_path_buf());
        self.recent_profiles.truncate(MAX_RECENT_PROFILES);
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
                self.push_recent(path);
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
        // Park any currently-running runner as a predecessor; the interface
        // is opened on the new runner thread, never on the UI thread.
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
            tracing::warn!(channel = n, "channel {n} config invalid");
            return;
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
                tracing::error!(channel = n, "channel {n} schedule error: {e:#}");
                return;
            }
        };

        // Lifecycle, telemetry reset, predecessor joining, and the runner
        // spawn all live in the supervisor (ADR-019); the GUI resets only
        // its own view-state.
        if i < self.log_counts.len() {
            self.log_counts[i] = LogCounts::default();
        }
        self.sup.start(i, cfg, schedule);
    }

    /// Stop channel `i` without blocking the UI (the supervisor parks the
    /// runner to drain in the background; undeliverable commands surface in
    /// the channel's telemetry).
    fn stop_connection(&mut self, i: usize) {
        let _ = self.sup.stop(i);
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
        self.sup.stop_all();
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
                let mut cfg = ChannelConfig::new(interface, messages);
                cfg.name = self.conn_drafts[i].name.clone();
                Some(cfg)
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
            self.profile.channels.push(ChannelConfig::named(
                self.conn_drafts[i].name.clone(),
                cfg.clone(),
                Vec::new(),
            ));
        }
        if self.sup.is_running(i) {
            // Undeliverable updates surface in the channel telemetry.
            let _ = self.sup.update_interface(i, cfg);
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
            // Tally channel-attributed events (structured `channel` field,
            // 1-based) for the channel-list rows.
            if let Some(idx) = event.channel.and_then(|n| n.checked_sub(1)) {
                if let Some(c) = self.log_counts.get_mut(idx) {
                    match event.level {
                        tracing::Level::ERROR => c.error += 1,
                        tracing::Level::WARN => c.warn += 1,
                        _ => c.info += 1,
                    }
                }
            }
            let ts = event.timestamp.format("%H:%M:%S%.3f");
            let line = format!("[{ts}] [{:<5}] {}", event.level, event.message);
            self.log_lines.push((line, event.level));
        }
        const LOG_CAP: usize = 2000;
        if self.log_lines.len() > LOG_CAP {
            self.log_lines.drain(..self.log_lines.len() - LOG_CAP);
        }

        // Drain runner telemetry (ADR-019: the supervisor owns the statuses;
        // the GUI gets back only the display samples) and route the sampled
        // payloads into the Output panes.
        for sample in self.sup.poll() {
            if let Some(d) = self.displays.get_mut(sample.channel) {
                d.push(sample.payload);
            }
        }

        // Refresh the per-channel send-rate samples (~1 s window).
        let now = Instant::now();
        for i in 0..self.rates.len() {
            let t = self.sup.telemetry(i);
            self.rates[i].sample(now, t.total_count, t.total_bytes, self.sup.is_running(i));
        }

        if self.sup.any_running() || self.sup.any_draining() {
            // Sends wake the UI instantly via the runners' notify callbacks
            // (ADR-016); this slower heartbeat only covers what has no
            // callback — log lines arriving over `log_rx`, the window title,
            // and reaping drained threads.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
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
        // Re-arm the repaint coalescer BEFORE draining statuses, so a status
        // arriving mid-drain either lands in this frame's batch or triggers a
        // fresh wake — never lost.
        self.repaint.frame_started();
        self.poll_channels(ui.ctx());
        self.handle_tab_keys(ui.ctx());
        egui::Frame::new()
            .inner_margin(4.0)
            .stroke(egui::Stroke::new(
                1.5_f32,
                egui::Color32::from_rgb(110, 120, 145),
            ))
            .show(ui, |ui| {
                self.show_top_bar(ui);
                self.show_status_bar(ui);
                self.show_log_panel(ui);
                // Master–detail (spec §3.2): the channel list on the left
                // (or its collapsed status strip), the selected channel's
                // detail pane in the centre.
                if self.channels_collapsed {
                    egui::Panel::left("channel_strip")
                        .resizable(false)
                        .show_inside(ui, |ui| self.show_channel_strip(ui));
                } else {
                    egui::Panel::left("channel_list")
                        .resizable(true)
                        .default_size(280.0)
                        .show_inside(ui, |ui| self.show_channel_list(ui));
                }
                egui::CentralPanel::default().show_inside(ui, |ui| self.show_detail(ui));
            });
        self.show_remove_confirm(ui.ctx());
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
        storage.set_string("dark_mode", self.dark_mode.to_string());
        let recents = self
            .recent_profiles
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        storage.set_string(RECENT_PROFILES_KEY, recents);
    }

    /// On window close (the X button) or any app exit: orderly shutdown.
    /// Every runner gets Stop and is then joined (bounded by the interface
    /// send timeouts), so serial ports and sockets close cleanly before the
    /// process dies instead of being killed mid-write.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all();
        self.sup.join_all();
    }
}

// ── Panel renderers ───────────────────────────────────────────────────────────

impl TalkerApp {
    fn show_top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // Profile UI (menu + name/dirty status) lives in the channel-
                // list header next to "+ Add", as in listener — see
                // `show_profile_menu` in `channels.rs`. The top bar keeps the
                // app-wide controls: zoom and theme.
                let r_minus = ui.small_button("−");
                ui.label(format!("{}%", zoom_percent(self.pixels_per_point)));
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
                            self.pixels_per_point = (self.pixels_per_point
                                + direction * ZOOM_STEP_PPP)
                                .clamp(ZOOM_MIN_PPP, ZOOM_MAX_PPP);
                            self.zoom_held_timer = Some(-0.4);
                        }
                        Some(ref mut t) => {
                            *t += dt;
                            if *t >= 0.0 {
                                *t -= 0.1; // repeat every 100 ms
                                self.pixels_per_point = (self.pixels_per_point
                                    + direction * ZOOM_STEP_PPP)
                                    .clamp(ZOOM_MIN_PPP, ZOOM_MAX_PPP);
                            }
                        }
                    }
                    ui.ctx().request_repaint();
                } else {
                    // Fallback: handle a quick tap that releases before is_pointer_button_down_on fires.
                    if r_minus.clicked() && self.zoom_held_timer.is_none() {
                        self.pixels_per_point =
                            (self.pixels_per_point - ZOOM_STEP_PPP).max(ZOOM_MIN_PPP);
                    }
                    if r_plus.clicked() && self.zoom_held_timer.is_none() {
                        self.pixels_per_point =
                            (self.pixels_per_point + ZOOM_STEP_PPP).min(ZOOM_MAX_PPP);
                    }
                    self.zoom_held_timer = None;
                }

                ui.separator();
                // Theme toggle. Uses half-circle glyphs from the
                // Geometric Shapes block (U+25D0/U+25D1) — the same
                // block as the ■ ▶ • glyphs the app already renders,
                // so coverage is guaranteed in the base font (the
                // Misc-Symbols ☀/☾ dingbats are not). The half-lit
                // circle reads as a light/dark duality icon; the
                // tooltip states the action.
                let (glyph, tip) = if self.dark_mode {
                    ("\u{25D1}", "Switch to light theme") // ◑
                } else {
                    ("\u{25D0}", "Switch to dark theme") // ◐
                };
                if ui.small_button(glyph).on_hover_text(tip).clicked() {
                    self.dark_mode = !self.dark_mode;
                    apply_theme(ui.ctx(), self.dark_mode);
                }
            });
            ui.add_space(4.0);
        });
    }

    fn show_status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let total = self.sup.len();
                let running = (0..total).filter(|&i| self.sup.is_running(i)).count();
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
                let (total_sent, errors) = (0..self.sup.len())
                    .map(|i| {
                        let t = self.sup.telemetry(i);
                        (t.total_count, t.errors_total)
                    })
                    .fold((0u64, 0u64), |(s, e), (ts, te)| (s + ts, e + te));
                ui.label(format!("Sent: {total_sent}"));
                ui.separator();
                // Per-run errors: each channel's tally resets when it starts,
                // like the send counts and log tallies.
                ui.label(format!("Errors: {errors}"));
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
                    ui.separator();
                    // Display filters — what's *shown*, independent of the
                    // capture level above. Info covers DEBUG/TRACE too.
                    ui.checkbox(&mut self.show_info, "Info");
                    ui.checkbox(&mut self.show_warn, "Warn");
                    ui.checkbox(&mut self.show_error, "Error");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.log_lines.clear();
                        }
                    });
                });
                ui.separator();
                let dark = ui.visuals().dark_mode;
                // Filter first, then virtualize: `show_rows` lays out only the
                // visible rows instead of all (up to 2,000) lines every
                // repaint. Rows must be uniform height for virtualization, so
                // long lines truncate (hover shows the full text) rather than
                // wrap.
                let visible: Vec<(&str, tracing::Level)> = self
                    .log_lines
                    .iter()
                    .filter(|(_, level)| match *level {
                        tracing::Level::ERROR => self.show_error,
                        tracing::Level::WARN => self.show_warn,
                        _ => self.show_info,
                    })
                    .map(|(line, level)| (line.as_str(), *level))
                    .collect();
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                ui.spacing_mut().item_spacing.y = 0.0;
                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, visible.len(), |ui, range| {
                        for &(line, level) in &visible[range] {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(line)
                                        .monospace()
                                        .color(level_color(level, dark)),
                                )
                                .truncate(),
                            )
                            .on_hover_text(line);
                        }
                    });
            });
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab cycles the channel selection (same keys as
    /// listener). Plain Tab is left to egui's widget-focus traversal.
    fn handle_tab_keys(&mut self, ctx: &egui::Context) {
        let (tab, shift, ctrl) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Tab),
                i.modifiers.shift,
                i.modifiers.ctrl,
            )
        });
        if tab && ctrl {
            let n = self.conn_drafts.len();
            if n > 0 {
                self.selected = Some(match self.selected {
                    Some(s) if !shift => (s + 1) % n,
                    Some(s) => (s + n - 1) % n,
                    None => 0,
                });
            }
        }
    }

    /// The "are you sure?" dialog for channel Remove. A modal so it can't be
    /// ignored; confirming queues the removal (selection then falls to the
    /// neighbour). Clicking the dimmed backdrop or pressing Escape cancels.
    fn show_remove_confirm(&mut self, ctx: &egui::Context) {
        let Some(i) = self.confirm_remove else {
            return;
        };
        let name = self.channel_name(i);
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::new("remove_confirm")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading("Remove channel?");
            ui.add_space(4.0);
            ui.label(format!(
                "“{name}” will be stopped and removed. Unsaved profile changes to it are lost."
            ));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Remove").color(egui::Color32::WHITE),
                        )
                        .fill(wiredata_ui::palette::LIGHT.fault_red),
                    )
                    .clicked()
                {
                    self.deferred.remove = Some(i);
                    close = true;
                }
            });
        });
        if close || resp.should_close() {
            self.confirm_remove = None;
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
        if let Some(i) = d.select {
            if i < self.conn_drafts.len() {
                self.selected = Some(i);
            }
        }
        if d.start_all {
            self.start_all();
        }
        if d.stop_all {
            self.stop_all();
        }
        if let Some(i) = d.start {
            self.start_connection(i);
        }
        if let Some(i) = d.stop {
            self.stop_connection(i);
        }
        if let Some(i) = d.remove {
            // The supervisor stops the runner and parks it in its orphan
            // bucket (reaped by poll — never joined on the UI thread).
            self.sup.remove_slot(i);
            self.conn_drafts.remove(i);
            self.sched_drafts.remove(i);
            self.displays.remove(i);
            if i < self.rates.len() {
                self.rates.remove(i);
            }
            if i < self.log_counts.len() {
                self.log_counts.remove(i);
            }
            if i < self.profile.channels.len() {
                self.profile.channels.remove(i);
            }
            // Keep the selection on the same visual position: the row that
            // slid into the removed slot, else the new last row, else none.
            self.selected = match self.selected {
                Some(s) if s == i => {
                    let n = self.conn_drafts.len();
                    if n == 0 {
                        None
                    } else {
                        Some(i.min(n - 1))
                    }
                }
                Some(s) if s > i => Some(s - 1),
                other => other,
            };
            self.dirty = true;
        }
        if let Some(kind) = d.add_channel {
            self.conn_drafts.push(ConnDraft {
                kind,
                ..ConnDraft::default()
            });
            self.sched_drafts.push(Vec::new());
            self.sup.push_slot();
            self.displays.push(ChannelDisplay::default());
            self.rates.push(RateTracker::new());
            self.log_counts.push(LogCounts::default());
            // Jump straight to the new channel for editing.
            self.selected = Some(self.conn_drafts.len() - 1);
            self.dirty = true;
        }
        if d.refresh_ports {
            self.refresh_serial_ports();
        }
    }
}

// ── Theme ─────────────────────────────────────────────────────────────────────

/// Apply `dark`/light to `ctx` via [`egui::ThemePreference`]. The visuals for
/// both themes are installed by `wiredata_ui::style::install_visuals` (ADR-016).
fn apply_theme(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
}

/// Log-line colour for `level`, adapted to the active theme.
///
/// ERROR / WARN keep saturated reds/ambers that read on either
/// background. INFO follows the theme's body text. DEBUG / TRACE are
/// muted greys, lightened on dark and darkened on light so they
/// stay legible and still read as "less important than INFO".
fn level_color(level: tracing::Level, dark: bool) -> egui::Color32 {
    match level {
        tracing::Level::ERROR => egui::Color32::from_rgb(220, 80, 80),
        tracing::Level::WARN if dark => egui::Color32::from_rgb(220, 180, 60),
        tracing::Level::WARN => egui::Color32::from_rgb(150, 110, 0),
        tracing::Level::DEBUG => egui::Color32::from_gray(if dark { 175 } else { 95 }),
        tracing::Level::TRACE => egui::Color32::from_gray(if dark { 150 } else { 120 }),
        // INFO: the theme's body text colour.
        _ => egui::Color32::from_gray(if dark { 235 } else { 20 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── zoom widget ───────────────────────────────────────────────────────────

    #[test]
    fn zoom_base_reads_as_100_percent() {
        // The old "115%" ppp is the new 100% baseline.
        assert_eq!(zoom_percent(ZOOM_BASE_PPP), 100);
    }

    #[test]
    fn zoom_steps_are_ten_percent() {
        assert_eq!(zoom_percent(ZOOM_BASE_PPP + ZOOM_STEP_PPP), 110);
        assert_eq!(zoom_percent(ZOOM_BASE_PPP - ZOOM_STEP_PPP), 90);
        assert_eq!(zoom_percent(ZOOM_BASE_PPP + 2.0 * ZOOM_STEP_PPP), 120);
    }

    #[test]
    fn zoom_clamp_bounds_are_50_and_200() {
        assert_eq!(zoom_percent(ZOOM_MIN_PPP), 50);
        assert_eq!(zoom_percent(ZOOM_MAX_PPP), 200);
    }
}
