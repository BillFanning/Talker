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
mod channels;
mod detail;
mod fonts;
pub mod state;
mod theme;
mod widgets;

use anyhow::anyhow;

use crate::config::ChannelConfig;
use crate::core::ChannelId;
use crate::display::{CharacterRendering, DisplayMode};

use bridge::{BridgeHandle, UiCommand};
use fonts::{install_fonts, MonoFont};
use state::{AppState, ChannelStatus};
use widgets::{
    config_incomplete, list_serial_ports, plan_config_commit, status_color, status_glyph,
    template_for, AddKind, ColorScheme, CommitStep,
};

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

/// The window size every launch opens at (window geometry isn't persisted — see
/// `persist_window: false` in [`run`]).
const DEFAULT_WINDOW_SIZE: [f32; 2] = [1100.0, 740.0];
/// Smallest size the window can be dragged to (a usability floor).
const MIN_WINDOW_SIZE: [f32; 2] = [640.0, 480.0];

/// Launch the graphical interface (§3). Owns the eframe event loop on the calling
/// (main) thread; the runtime bridge runs on its own Tokio thread (ADR-008).
///
/// **Shared GUI-startup funnel (two-binary invariant — see `src/main.rs`).** Both
/// GUI entry points route through here: the `listener-gui.exe` (windows-subsystem)
/// binary, which avoids the brief console-window flash on a double-click, and
/// `listener.exe`'s bare-launch / `--gui` path. Put ALL GUI startup (console detach,
/// logging, window options) in this function so the two binaries stay identical —
/// never in either `main`.
pub fn run() -> anyhow::Result<()> {
    detach_console(); // drop the double-click console before the window opens
    crate::diagnostics::init_logging(); // §114; non-fatal if already installed (§117)
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(DEFAULT_WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE),
        // Don't persist/restore window geometry. eframe restores the saved window state
        // (size, position, and — the one that bit us — `maximized`) *after* the window
        // is shown, so the window appeared at the default size and then jumped to its
        // saved geometry: the double frame/title-bar flash on launch. With this off the
        // window always opens at DEFAULT_WINDOW_SIZE with no post-show move, and a bad
        // tiny geometry can't be restored either. Trade-off: it no longer reopens where
        // it was last; the recent-profiles list is persisted separately via `save`, so
        // that still survives.
        persist_window: false,
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
            Ok(Box::new(ListenerApp::new(bridge, cc.storage)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// Apply the app's base style. This fork keeps visuals *per theme* and the active
/// theme separately, so a plain `set_global_style` is ignored — visuals must go
/// through `set_visuals_of` + `set_theme` and sizes through `all_styles_mut` (the
/// same path talker uses). Sets the Noto font, a light theme with the grey backdrop
/// and darker/heavier text, and a nudged text size — no custom UI scale (the OS DPI
/// drives scaling, see below), and no 3-D / outline / button-sizing overrides.
fn apply_style(ctx: &egui::Context) {
    install_fonts(ctx);
    // No custom UI scale: the OS DPI setting drives sizing. (We deliberately do not
    // call `set_pixels_per_point` / `set_zoom_factor` — overriding the scale here both
    // ignores the user's system setting and gets persisted into eframe storage, where
    // a stale value then sticks across launches. Text size is nudged in
    // `all_styles_mut` below instead.)

    // Clean slate: a light theme with the grey backdrop the user likes and darker
    // (heavier) text — no custom 3-D / outline / sizing overrides.
    let mut light = egui::Visuals::light();
    light.override_text_color = Some(egui::Color32::from_gray(20));
    light.panel_fill = egui::Color32::from_gray(220);
    light.window_fill = egui::Color32::from_gray(220);
    // More visible dividers (#6): `ui.separator()` draws with the noninteractive
    // bg_stroke, which defaults to a very faint grey — darken and thicken it.
    light.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.5, egui::Color32::from_gray(120));
    // Make buttons read as raised, interactive objects in every state (not flat
    // labels). egui's light defaults give buttons almost no fill or border; give the
    // resting (`inactive`), `hovered`, and `active` states a filled face + a visible
    // border + a little rounding, brightening on hover and darkening on press, so a
    // button looks clickable whether enabled or disabled. Disabled buttons use
    // `noninteractive` (flat/dim), so the enabled↔disabled distinction is preserved.
    let btn_border = egui::Stroke::new(1.0, egui::Color32::from_gray(150));
    let btn_round = egui::CornerRadius::same(4);
    light.widgets.inactive.weak_bg_fill = egui::Color32::from_gray(236);
    light.widgets.inactive.bg_fill = egui::Color32::from_gray(236);
    light.widgets.inactive.bg_stroke = btn_border;
    light.widgets.inactive.corner_radius = btn_round;
    light.widgets.hovered.weak_bg_fill = egui::Color32::from_gray(248);
    light.widgets.hovered.bg_fill = egui::Color32::from_gray(248);
    light.widgets.hovered.bg_stroke = egui::Stroke::new(1.2, egui::Color32::from_gray(110));
    light.widgets.hovered.corner_radius = btn_round;
    light.widgets.active.weak_bg_fill = egui::Color32::from_gray(214);
    light.widgets.active.bg_fill = egui::Color32::from_gray(214);
    light.widgets.active.bg_stroke = egui::Stroke::new(1.2, egui::Color32::from_gray(90));
    light.widgets.active.corner_radius = btn_round;
    ctx.set_visuals_of(egui::Theme::Light, light);
    ctx.set_theme(egui::ThemePreference::Light);

    // Match talker's text size (+0.5 to non-monospace; the stream view keeps its
    // monospace size). Button sizing stays at the egui default.
    ctx.all_styles_mut(|style| {
        for font in style.text_styles.values_mut() {
            if font.family != egui::FontFamily::Monospace {
                font.size += 0.5;
            }
        }
        // Open popups/menus instantly. egui fades areas in over `animation_time`
        // (~83 ms), which made the color/font dropdowns feel laggy to appear; for
        // a utility UI an instant snap reads as snappier (and stops hover/expand
        // transitions dragging too).
        style.animation_time = 0.0;
        // Enlarge the collapsing-section triangles ~25% so they're easier to hit
        // and read (#7). `icon_width` also sizes checkbox/radio glyphs, which scale
        // up consistently.
        style.spacing.icon_width *= 1.25;
        style.spacing.icon_width_inner *= 1.25;
    });
}

/// The eframe application root: the runtime bridge, the folded view-model, the
/// selected channel, and the add-channel form's draft state.
struct ListenerApp {
    bridge: BridgeHandle,
    state: AppState,
    selected: Option<ChannelId>,
    /// Last selection sent to the driver, so we only send `Select` on change. The
    /// driver full-snapshots only the selected channel (the rest get cheap stats).
    last_selected_sent: Option<ChannelId>,
    /// How the stream view renders bytes (§42): Hex / Rendered / Raw, plus the
    /// control-character style used in Raw mode (§46).
    msg_mode: DisplayMode,
    msg_chars: CharacterRendering,
    /// Stream-view font size and color scheme.
    msg_font_size: f32,
    /// Editable text backing the font-size combo, so a typed size persists across
    /// frames while it's being entered (#7).
    font_text: String,
    /// Selected monospace face for the stream view.
    msg_font: MonoFont,
    msg_colors: ColorScheme,
    /// A working copy of the selected channel's config, edited in the Configure
    /// section and sent on Apply. Re-seeded when the selection changes.
    edit_draft: Option<(ChannelId, ChannelConfig)>,
    /// A pending "remove this channel?" confirmation (#1); `Some` while the dialog
    /// is up.
    confirm_remove: Option<ChannelId>,
    /// One-shot: open the Configure section on the next frame because focus moved to
    /// an unconfigured/faulted channel that needs attention.
    force_config_open: bool,
    /// Whether the channel-list (tabs) column is collapsed to a thin strip (#1).
    channels_collapsed: bool,
    /// Per-severity filters for the diagnostics log.
    show_info: bool,
    show_warn: bool,
    show_error: bool,
    /// Available serial port names for the serial port dropdown (§14.4); refreshed
    /// on demand via the ⟳ button.
    serial_ports: Vec<String>,
    /// Memoized stream-view rows for the detail pane. The scrollback can reach the
    /// ~1 MB retention cap, so rendering + line-splitting it every frame (even at
    /// 5 Hz) is wasteful and, with a non-virtualized layout, was stalling the UI
    /// (regression after the stream-only split). We recompute only when the inputs
    /// change; `show_rows` then lays out just the visible rows.
    stream_cache: Option<StreamRenderCache>,
    /// The profile file the workspace is currently associated with (last saved or
    /// loaded). `Save` writes here silently; `Save As…` always re-prompts. `None`
    /// until the first save/load, so the first `Save` falls through to a picker.
    current_profile_path: Option<std::path::PathBuf>,
    /// Recently saved/loaded profile paths, most-recent-first (capped). Listed at the
    /// top of the Profile menu for one-click reload. Session-scoped for now.
    recent_profiles: Vec<std::path::PathBuf>,
}

/// How many recent profiles to keep in the Profile menu.
pub(super) const MAX_RECENT_PROFILES: usize = 8;

/// Cached, line-split render of a channel's accumulated stream bytes, reused
/// across frames until one of its inputs changes (see [`StreamRenderKey`]). The
/// key's `channel` also guards against showing one channel's rows after the
/// selection moves to another.
struct StreamRenderCache {
    key: StreamRenderKey,
    rows: Vec<String>,
}

/// The cheap signature that decides whether [`StreamRenderCache`] is still valid.
/// The stream `cursor` advances as deltas are folded and `len` captures front
/// eviction, so together they capture "the bytes changed" without hashing the
/// buffer; `channel` guards a selection change and the render settings a view
/// change.
#[derive(Clone, Copy, PartialEq)]
struct StreamRenderKey {
    channel: ChannelId,
    cursor: u64,
    len: usize,
    mode: DisplayMode,
    chars: CharacterRendering,
    /// Wrap width in monospace columns. The cache rows are pre-wrapped to this so each
    /// row is exactly one visual line (uniform height) — that lets the viewer both
    /// soft-wrap *and* virtualize with `show_rows`. Re-split when the width changes.
    wrap_cols: usize,
}

/// eframe storage key for the persisted recent-profiles list (newline-joined paths).
const RECENT_PROFILES_KEY: &str = "recent_profiles";

impl ListenerApp {
    fn new(bridge: BridgeHandle, storage: Option<&dyn eframe::Storage>) -> Self {
        // Restore the recent-profiles list from eframe storage (survives restarts).
        let recent_profiles = storage
            .and_then(|s| s.get_string(RECENT_PROFILES_KEY))
            .map(|s| {
                s.lines()
                    .filter(|l| !l.is_empty())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            bridge,
            state: AppState::default(),
            selected: None,
            last_selected_sent: None,
            msg_mode: DisplayMode::Rendered,
            msg_chars: CharacterRendering::Glyph,
            msg_font_size: 13.0,
            font_text: "13".to_string(),
            msg_font: MonoFont::Cascadia,
            msg_colors: ColorScheme::BlackOnWhite,
            edit_draft: None,
            confirm_remove: None,
            force_config_open: false,
            channels_collapsed: false,
            show_info: true,
            show_warn: true,
            show_error: true,
            serial_ports: list_serial_ports(),
            stream_cache: None,
            current_profile_path: None,
            recent_profiles,
        }
    }

    /// Record a profile path as recently used: move/insert it at the front, dedup, and
    /// cap the list. Called on every save/load so the Profile menu's recents are live.
    pub(super) fn remember_recent_profile(&mut self, path: std::path::PathBuf) {
        self.recent_profiles.retain(|p| p != &path);
        self.recent_profiles.insert(0, path);
        self.recent_profiles.truncate(MAX_RECENT_PROFILES);
    }

    pub(super) fn refresh_serial_ports(&mut self) {
        self.serial_ports = list_serial_ports();
    }

    /// Drain every pending update into the view-model (non-blocking, §99). A newly
    /// added channel takes focus; a removed one that was selected clears it.
    fn drain_updates(&mut self) {
        while let Ok(update) = self.bridge.updates.try_recv() {
            match &update {
                bridge::UiUpdate::ChannelAdded(id, ..) => self.selected = Some(*id),
                // When the selected channel goes away, fall back to the one above it
                // (or below, if it was the first) instead of clearing focus (#2).
                bridge::UiUpdate::ChannelRemoved(id) if self.selected == Some(*id) => {
                    self.selected = self.state.neighbor(*id);
                }
                _ => {}
            }
            self.state.apply(update);
        }
    }

    /// Send a command to the driver. Non-blocking: a full command channel drops the
    /// command rather than stalling the UI thread (AGENTS §5).
    pub(super) fn send(&self, command: UiCommand) {
        let _ = self.bridge.commands.try_send(command);
    }

    /// Add a fresh channel of `kind` (from the "Add" menu by the list heading). It
    /// is auto-selected, and configured in the Configure section above the view.
    pub(super) fn add_channel(&mut self, kind: AddKind) {
        self.send(UiCommand::AddChannel(Box::new(template_for(kind))));
    }

    /// Re-seed the edit draft from the selected channel's config whenever the
    /// selection changes, so the Configure section edits a fresh working copy.
    pub(super) fn sync_edit_draft(&mut self, id: ChannelId) {
        if self.edit_draft.as_ref().map(|(eid, _)| *eid) != Some(id) {
            if let Some(view) = self.state.channel(id) {
                // Pop the Configure section open when focus lands on a channel that
                // still needs setup (missing port) or is faulted (e.g. a bind
                // conflict) — so the fix is right there, not hidden behind a header.
                self.force_config_open =
                    config_incomplete(&view.config) || view.status == ChannelStatus::Faulted;
                self.edit_draft = Some((id, view.config.clone()));
            }
        }
    }

    /// Apply the edited config and bring the channel up on it in one action (§13) —
    /// the single "Apply & Restart" button, so there's no separate Apply-then-Start.
    /// Drives the channel to Stopped first (if it's Running/Faulted/Reconnecting),
    /// swaps in the new config, then Starts — so Start is always a legal
    /// Stopped→Running transition (§8.5). Refuses, with an inline complaint, if the
    /// new config can't start (e.g. no port), leaving the channel as-is.
    pub(super) fn apply_and_restart(&mut self, id: ChannelId) {
        let Some((_, config)) = self.edit_draft.clone() else {
            return;
        };
        let Some(status) = self.state.channel(id).map(|v| v.status) else {
            return;
        };
        match plan_config_commit(status, config_incomplete(&config)) {
            None => self.complain_unconfigured(id),
            Some(steps) => {
                for step in steps {
                    match step {
                        CommitStep::Stop => self.send(UiCommand::Stop(id)),
                        CommitStep::Reconfigure => {
                            self.send(UiCommand::Reconfigure(id, Box::new(config.clone())))
                        }
                        CommitStep::Start => self.send(UiCommand::Start(id)),
                    }
                }
            }
        }
    }

    /// Start a channel, applying any pending config edits first (the unified "go"
    /// action). Start always reflects what's in the editor: it commits the edit draft
    /// via the §13 Reconfigure path and then Starts, so there is no way to start on a
    /// stale config. Refuses (with an inline complaint) if the config is incomplete —
    /// e.g. a UDP channel with no port would otherwise bind an ephemeral port and
    /// silently "run" (#3, #6). Edits commit only here (or via Apply & Restart), never
    /// on every keystroke — a running channel keeps its config until you act.
    pub(super) fn try_start(&mut self, id: ChannelId) {
        let Some(config) = self.start_config(id) else {
            return;
        };
        // Reconfigure to the (possibly edited) config, then Start — so Start picks up
        // pending edits. Reconfiguring a Stopped channel just swaps the config in for
        // the upcoming Start (§13); no restart of a live channel happens here.
        self.send(UiCommand::Reconfigure(id, Box::new(config)));
        self.send(UiCommand::Start(id));
    }

    /// The config to start channel `id` with: the edit draft if one is loaded for this
    /// channel (what the editor shows), else its committed config. Returns `None` —
    /// raising an inline "unconfigured" complaint — if that config is incomplete (e.g.
    /// a UDP channel with no port, which would otherwise bind an ephemeral port and
    /// silently "run", #3/#6). Shared by `try_start` and `start_all` so both honor the
    /// same draft-preference and validation rule.
    fn start_config(&mut self, id: ChannelId) -> Option<ChannelConfig> {
        let config = match &self.edit_draft {
            Some((eid, cfg)) if *eid == id => cfg.clone(),
            _ => self.state.channel(id)?.config.clone(),
        };
        if config_incomplete(&config) {
            self.complain_unconfigured(id);
            return None;
        }
        Some(config)
    }

    /// "Start all": validate every Stopped channel the same way `try_start` does
    /// (inline complaint for an unconfigured one), and send the ready ones as a single
    /// `StartAll` batch. One command, not a 2N `Reconfigure`+`Start` burst — a burst
    /// could overflow the bounded command channel and silently drop starts.
    pub(super) fn start_all(&mut self) {
        let mut batch = Vec::new();
        for id in self.state.startable_channel_ids() {
            if let Some(config) = self.start_config(id) {
                batch.push((id, Box::new(config)));
            }
        }
        if !batch.is_empty() {
            self.send(UiCommand::StartAll(batch));
        }
    }

    /// Surface an "unconfigured" complaint for a channel inline (red ⚠ line) and open
    /// its Configure section if it's the one in view.
    fn complain_unconfigured(&mut self, id: ChannelId) {
        self.state.apply(bridge::UiUpdate::ChannelError(
            id,
            "Not configured — set a port before starting.".to_string(),
        ));
        if self.selected == Some(id) {
            self.force_config_open = true;
        }
    }

    /// Tab-style keyboard switching between channels (#3): Ctrl+Tab / Ctrl+Shift+Tab
    /// cycle forward / back (wrapping), like editor tabs. No-op with no selection.
    fn handle_tab_keys(&mut self, ctx: &egui::Context) {
        let (tab, shift, ctrl) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Tab),
                i.modifiers.shift,
                i.modifiers.ctrl,
            )
        });
        if tab && ctrl {
            let next = match self.selected {
                Some(id) => self.state.cycle(id, !shift),
                None => self.state.first(),
            };
            if next.is_some() {
                self.selected = next;
            }
        }
    }

    /// The "are you sure?" dialog for Remove (#1). A modal so it can't be ignored;
    /// confirming sends the removal (focus then falls to the neighbour, #2).
    fn show_remove_confirm(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_remove else {
            return;
        };
        let name = self
            .state
            .channel(id)
            .map(|v| v.name.clone())
            .unwrap_or_default();
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::new("remove_confirm")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading("Remove channel?");
            ui.add_space(4.0);
            ui.label(format!(
                "“{name}” will be stopped and removed. This can't be undone."
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
                        .fill(theme::FAULT_RED),
                    )
                    .clicked()
                {
                    self.send(UiCommand::RemoveChannel(id));
                    close = true;
                }
            });
        });
        // Clicking the dimmed backdrop or pressing Escape cancels.
        if close || resp.should_close() {
            self.confirm_remove = None;
        }
    }
}

impl eframe::App for ListenerApp {
    /// Persist the recent-profiles list (eframe calls this periodically and on exit),
    /// so the Profile menu's recents survive a restart. Paths are newline-joined.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let joined = self
            .recent_profiles
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        storage.set_string(RECENT_PROFILES_KEY, joined);
    }

    // This workspace's eframe surfaces a `Ui` directly (App::ui), like talker's GUI.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_updates();
        self.handle_tab_keys(ui.ctx());
        if self.channels_collapsed {
            // Collapsed: a thin strip — an expand button plus mini tabs (a status dot
            // per channel, click to select, name on hover) (#1).
            egui::Panel::left("channel_list_collapsed")
                .resizable(false)
                .show_inside(ui, |ui| {
                    if ui
                        .button("\u{25B6}")
                        .on_hover_text("Show channels")
                        .clicked()
                    {
                        self.channels_collapsed = false;
                    }
                    ui.separator();
                    let mini: Vec<(ChannelId, ChannelStatus, String)> = self
                        .state
                        .channels()
                        .map(|v| (v.id, v.status, v.name.clone()))
                        .collect();
                    let base = egui::TextStyle::Body.resolve(ui.style()).size;
                    for (cid, status, name) in mini {
                        let selected = self.selected == Some(cid);
                        let (glyph, scale) = status_glyph(status);
                        let dot = egui::RichText::new(glyph)
                            .size(base * scale)
                            .color(status_color(status));
                        if ui
                            .selectable_label(selected, dot)
                            .on_hover_text(name)
                            .clicked()
                        {
                            self.selected = Some(cid);
                        }
                    }
                });
        } else {
            egui::Panel::left("channel_list")
                .resizable(true)
                .default_size(320.0)
                .show_inside(ui, |ui| self.show_channel_list(ui));
        }
        egui::CentralPanel::default().show_inside(ui, |ui| self.show_detail(ui));
        self.show_remove_confirm(ui.ctx());
        // Tell the driver which channel is on screen so it full-snapshots only that
        // one (others get cheap stats). Sent only on change.
        if self.selected != self.last_selected_sent {
            self.last_selected_sent = self.selected;
            self.send(UiCommand::Select(self.selected));
        }
    }
}
