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

use std::time::SystemTime;

use anyhow::anyhow;

use crate::config::{
    templates, ChannelConfig, DataBits, DecoderConfig, FlowControl, InterfaceConfig, Parity,
    StopBits,
};
use crate::core::ChannelId;
use crate::decode::NmeaValidationMode;
use crate::diagnostics::DiagnosticSeverity;
use crate::display::{CharacterRendering, DisplayEncoding, DisplayMode, DisplayView, WrappingMode};
use crate::transport::udp::UdpMode;

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

/// Apply the app's base style. This fork keeps visuals *per theme* and the active
/// theme separately, so a plain `set_global_style` is ignored — visuals must go
/// through `set_visuals_of` + `set_theme` and sizes through `all_styles_mut` (the
/// same path talker uses). Sets the Noto font at talker's 1.15 zoom, a light theme
/// with the grey backdrop and darker/heavier text, and a text size matching talker
/// (+0.5) — no 3-D, outline, or button-sizing overrides (clean slate).
fn apply_style(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_pixels_per_point(1.15); // talker's baseline zoom

    // Clean slate: a light theme with the grey backdrop the user likes and darker
    // (heavier) text — no custom 3-D / outline / sizing overrides.
    let mut light = egui::Visuals::light();
    light.override_text_color = Some(egui::Color32::from_gray(20));
    light.panel_fill = egui::Color32::from_gray(220);
    light.window_fill = egui::Color32::from_gray(220);
    // More visible dividers (#6): `ui.separator()` draws with the noninteractive
    // bg_stroke, which defaults to a very faint grey — darken and thicken it.
    light.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.5, egui::Color32::from_gray(120));
    ctx.set_visuals_of(egui::Theme::Light, light);
    ctx.set_theme(egui::ThemePreference::Light);

    // Match talker's text size (+0.5 to non-monospace; the message dump keeps its
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

/// The stroke for a channel box border — talker's connection-card color.
const BOX_STROKE: egui::Color32 = egui::Color32::from_rgb(140, 160, 200);

/// Selectable monospace faces for the message view: (display label, family key,
/// font bytes). Each becomes an egui `FontFamily::Name` so the view can switch faces
/// per render. `Hack` (egui's built-in monospace) is offered separately, no entry
/// here. `mono_dejavu` doubles as the wide-coverage fallback for the others.
const MONO_FONTS: &[(&str, &str, &[u8])] = &[
    (
        "Cascadia Mono",
        "mono_cascadia",
        include_bytes!("../../assets/fonts/CascadiaMono.ttf"),
    ),
    (
        "JetBrains Mono",
        "mono_jetbrains",
        include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"),
    ),
    (
        "DejaVu Sans Mono",
        "mono_dejavu",
        include_bytes!("../../assets/fonts/DejaVuSansMono.ttf"),
    ),
];

/// Build the whole font stack in one place (clearer and less error-prone than
/// incremental `add_font` with cross-family append ordering). Each family's fallback
/// chain is spelled out explicitly:
/// - **Proportional** (the UI): Noto Sans, then script + symbol + control fallbacks.
/// - **Monospace** (built-in Hack) and every selectable mono face: the face → DejaVu
///   (widest mono, so missing glyphs stay *monospace* and columns stay aligned) →
///   control pictures → Noto Sans (last resort; proportional, may misalign) → symbols.
/// - **`ui_bold`**: Noto Sans Bold for genuinely bold titles, with regular fallbacks.
///
/// CJK is not bundled. See `assets/fonts/README.md`.
fn install_fonts(ctx: &egui::Context) {
    use egui::FontFamily::{Monospace, Name, Proportional};
    use std::sync::Arc;

    let mut f = egui::FontDefinitions::default();
    fn reg(f: &mut egui::FontDefinitions, name: &str, bytes: &'static [u8]) {
        f.font_data.insert(
            name.to_owned(),
            Arc::new(egui::FontData::from_static(bytes)),
        );
    }
    reg(
        &mut f,
        "noto_sans",
        include_bytes!("../../assets/fonts/NotoSans-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_bold",
        include_bytes!("../../assets/fonts/NotoSans-Bold.ttf"),
    );
    reg(
        &mut f,
        "control_pictures",
        include_bytes!("../../assets/fonts/CascadiaMono-ControlPictures.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_symbols2",
        include_bytes!("../../assets/fonts/NotoSansSymbols2-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_thai",
        include_bytes!("../../assets/fonts/NotoSansThai-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_arabic",
        include_bytes!("../../assets/fonts/NotoSansArabic-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_hebrew",
        include_bytes!("../../assets/fonts/NotoSansHebrew-Regular.ttf"),
    );
    reg(
        &mut f,
        "noto_sans_devanagari",
        include_bytes!("../../assets/fonts/NotoSansDevanagari-Regular.ttf"),
    );
    for &(_label, key, bytes) in MONO_FONTS {
        reg(&mut f, key, bytes);
    }

    let scripts = [
        "noto_sans_symbols2",
        "noto_sans_thai",
        "noto_sans_arabic",
        "noto_sans_hebrew",
        "noto_sans_devanagari",
    ];

    // Proportional: Noto Sans wins, then control pictures + scripts as fallbacks.
    let prop = f.families.entry(Proportional).or_default();
    prop.insert(0, "noto_sans".to_owned());
    prop.push("control_pictures".to_owned());
    prop.extend(scripts.iter().map(|s| s.to_string()));

    // Mono fallback tail shared by the built-in Monospace and the named faces.
    let mono_tail = |face_self: Option<&str>| {
        let mut v: Vec<String> = Vec::new();
        // DejaVu is the wide-coverage mono fallback — but don't list it under itself.
        if face_self != Some("mono_dejavu") {
            v.push("mono_dejavu".to_owned());
        }
        v.push("control_pictures".to_owned());
        v.push("noto_sans".to_owned());
        v.extend(scripts.iter().map(|s| s.to_string()));
        v
    };

    // Built-in Monospace (Hack) keeps its defaults, then the shared tail.
    let mono = f.families.entry(Monospace).or_default();
    mono.extend(mono_tail(None));

    // Each selectable mono face: face first, then the shared tail.
    for &(_label, key, _bytes) in MONO_FONTS {
        let mut chain = vec![key.to_owned()];
        chain.extend(mono_tail(Some(key)));
        f.families.insert(Name(key.into()), chain);
    }

    // Bold UI titles: real bold weight, with regular + symbol fallbacks so a stray
    // glyph never tofus.
    f.families.insert(
        Name("ui_bold".into()),
        vec![
            "noto_sans_bold".to_owned(),
            "noto_sans".to_owned(),
            "noto_sans_symbols2".to_owned(),
            "control_pictures".to_owned(),
        ],
    );

    ctx.set_fonts(f);
}

/// A label in a genuine bold weight (the `ui_bold` family). egui `.strong()` only
/// recolors, so titles use this for real boldface.
fn bold(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(egui::FontFamily::Name("ui_bold".into()))
}

/// A selectable message-view monospace face. `Hack` is egui's built-in monospace;
/// the rest are bundled (see [`MONO_FONTS`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum MonoFont {
    Hack,
    Cascadia,
    JetBrains,
    DejaVu,
}

impl MonoFont {
    const ALL: &'static [MonoFont] = &[
        MonoFont::Hack,
        MonoFont::Cascadia,
        MonoFont::JetBrains,
        MonoFont::DejaVu,
    ];

    fn label(self) -> &'static str {
        match self {
            MonoFont::Hack => "Hack",
            MonoFont::Cascadia => "Cascadia Mono",
            MonoFont::JetBrains => "JetBrains Mono",
            MonoFont::DejaVu => "DejaVu Sans Mono",
        }
    }

    fn family(self) -> egui::FontFamily {
        match self {
            MonoFont::Hack => egui::FontFamily::Monospace,
            MonoFont::Cascadia => egui::FontFamily::Name("mono_cascadia".into()),
            MonoFont::JetBrains => egui::FontFamily::Name("mono_jetbrains".into()),
            MonoFont::DejaVu => egui::FontFamily::Name("mono_dejavu".into()),
        }
    }
}

/// Which interface a new channel uses, in the add-channel form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AddKind {
    Udp,
    Tcp,
    Serial,
}

/// A simple preset color scheme for the message view (#6 — simpler than a picker).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColorScheme {
    BlackOnWhite,
    GreenOnBlack,
    AmberOnBlack,
    WhiteOnBlack,
}

impl ColorScheme {
    fn label(self) -> &'static str {
        match self {
            ColorScheme::BlackOnWhite => "Black on white",
            ColorScheme::GreenOnBlack => "Green on black",
            ColorScheme::AmberOnBlack => "Amber on black",
            ColorScheme::WhiteOnBlack => "White on black",
        }
    }
    fn fg(self) -> egui::Color32 {
        match self {
            ColorScheme::BlackOnWhite => egui::Color32::from_gray(20),
            ColorScheme::GreenOnBlack => egui::Color32::from_rgb(60, 230, 60),
            ColorScheme::AmberOnBlack => egui::Color32::from_rgb(255, 190, 70),
            ColorScheme::WhiteOnBlack => egui::Color32::from_gray(235),
        }
    }
    fn bg(self) -> egui::Color32 {
        match self {
            ColorScheme::BlackOnWhite => egui::Color32::from_gray(252),
            _ => egui::Color32::from_gray(16),
        }
    }
}

/// Font sizes offered in the message-view size dropdown (#5).
const MSG_FONT_SIZES: &[f32] = &[
    8.0, 10.0, 11.0, 12.0, 13.0, 14.0, 16.0, 18.0, 20.0, 24.0, 28.0, 36.0, 48.0, 72.0,
];

/// One channel's row data, snapshotted before rendering so the list isn't borrowing
/// the view-model while a click mutates the selection.
struct ChannelRow {
    id: ChannelId,
    name: String,
    details: String,
    status: ChannelStatus,
    messages: u64,
    bytes_per_sec: f64,
    info: usize,
    warnings: usize,
    errors: usize,
}

/// The eframe application root: the runtime bridge, the folded view-model, the
/// selected channel, and the add-channel form's draft state.
struct ListenerApp {
    bridge: BridgeHandle,
    state: AppState,
    selected: Option<ChannelId>,
    /// How the message view renders bytes (§42): Hex / Rendered / Raw, plus the
    /// control-character style used in Raw mode (§46).
    msg_mode: DisplayMode,
    msg_chars: CharacterRendering,
    /// View options for the message list: prepend the Message Number / timestamp.
    show_msg_number: bool,
    show_timestamp: bool,
    /// Message-view font size and color scheme.
    msg_font_size: f32,
    /// Editable text backing the font-size combo, so a typed size persists across
    /// frames while it's being entered (#7).
    font_text: String,
    /// Selected monospace face for the message view.
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
}

impl ListenerApp {
    fn new(bridge: BridgeHandle) -> Self {
        Self {
            bridge,
            state: AppState::default(),
            selected: None,
            msg_mode: DisplayMode::Rendered,
            msg_chars: CharacterRendering::Glyph,
            show_msg_number: true,
            show_timestamp: false,
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
        }
    }

    fn refresh_serial_ports(&mut self) {
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
    fn send(&self, command: UiCommand) {
        let _ = self.bridge.commands.try_send(command);
    }

    /// Add a fresh channel of `kind` (from the "Add" menu by the list heading). It
    /// is auto-selected, and configured in the Configure section above the view.
    fn add_channel(&mut self, kind: AddKind) {
        self.send(UiCommand::AddChannel(Box::new(template_for(kind))));
    }

    /// Re-seed the edit draft from the selected channel's config whenever the
    /// selection changes, so the Configure section edits a fresh working copy.
    fn sync_edit_draft(&mut self, id: ChannelId) {
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

    /// Apply the edit draft to its channel (§13): send a `Reconfigure`. A Running
    /// channel restarts onto it; a Stopped/Faulted one swaps it in for the next
    /// Start/Retry.
    fn apply_edit_draft(&mut self) {
        if let Some((id, config)) = self.edit_draft.clone() {
            self.send(UiCommand::Reconfigure(id, Box::new(config)));
        }
    }

    /// Start a channel, but refuse (and complain) if its config is incomplete — e.g.
    /// a UDP channel with no port would otherwise bind an ephemeral port and silently
    /// "run" (#3, #6). The complaint is folded in as a per-channel error so it shows
    /// inline (the red ⚠ recourse line) — no status bar needed.
    fn try_start(&mut self, id: ChannelId) {
        let incomplete = self
            .state
            .channel(id)
            .map(|v| config_incomplete(&v.config))
            .unwrap_or(false);
        if incomplete {
            self.complain_unconfigured(id);
            return;
        }
        self.send(UiCommand::Start(id));
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

    // ── Panels ──────────────────────────────────────────────────────────────

    fn show_channel_list(&mut self, ui: &mut egui::Ui) {
        // Heading + the "Add" menu (the only place channels are created now), plus
        // bulk Start all / Stop all (#5).
        ui.horizontal(|ui| {
            if ui
                .button("\u{25C0}")
                .on_hover_text("Collapse channel list")
                .clicked()
            {
                self.channels_collapsed = true;
            }
            ui.heading("Channels");
            ui.menu_button("+ Add", |ui| {
                if ui.button("UDP").clicked() {
                    self.add_channel(AddKind::Udp);
                    ui.close();
                }
                if ui.button("TCP").clicked() {
                    self.add_channel(AddKind::Tcp);
                    ui.close();
                }
                if ui.button("Serial").clicked() {
                    self.add_channel(AddKind::Serial);
                    ui.close();
                }
            });
            if ui.button("Start all").clicked() {
                for cid in self.state.channel_ids() {
                    // Route through the same guard: complete channels start; each
                    // unconfigured one gets an inline complaint instead (#3, #6).
                    self.try_start(cid);
                }
            }
            if ui.button("Stop all").clicked() {
                for cid in self.state.channel_ids() {
                    self.send(UiCommand::Stop(cid));
                }
            }
        });
        ui.separator();

        // Snapshot the rows first so the list isn't borrowing `state` while a click
        // mutates `selected`.
        let rows: Vec<ChannelRow> = self
            .state
            .channels()
            .map(|v| ChannelRow {
                id: v.id,
                name: v.name.clone(),
                details: v.details.clone(),
                status: v.status,
                messages: v.messages,
                bytes_per_sec: v.bytes_per_sec,
                info: v.info,
                warnings: v.warnings,
                errors: v.errors,
            })
            .collect();

        if rows.is_empty() {
            ui.label("No channels yet — use “+ Add”.");
            return;
        }

        let base = egui::TextStyle::Body.resolve(ui.style()).size;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in rows {
                let id = row.id;
                let selected = self.selected == Some(id);
                // Each box gets its own id scope so widget ids don't collide between
                // channels (that collision was why the 2nd channel couldn't be
                // selected, #4).
                ui.push_id(id, |ui| {
                    let visuals = ui.visuals();
                    let mut frame = egui::Frame::group(ui.style())
                        .inner_margin(8.0)
                        .corner_radius(egui::CornerRadius::same(6))
                        .stroke(egui::Stroke::new(1.5, BOX_STROKE));
                    if selected {
                        frame.fill = visuals.selection.bg_fill;
                        frame.stroke = egui::Stroke::new(1.5, visuals.selection.stroke.color);
                    }
                    let response = frame
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            // Line 1: status dot + name.
                            let mut line1 = egui::text::LayoutJob::default();
                            line1.append(
                                "\u{25CF}",
                                0.0,
                                egui::TextFormat {
                                    font_id: egui::FontId::proportional(base * 1.4),
                                    color: status_color(row.status),
                                    valign: egui::Align::Center,
                                    ..Default::default()
                                },
                            );
                            line1.append(
                                &format!("  {}", row.name),
                                0.0,
                                egui::TextFormat {
                                    font_id: egui::FontId::proportional(base * 1.1),
                                    color: ui.visuals().text_color(),
                                    valign: egui::Align::Center,
                                    ..Default::default()
                                },
                            );
                            ui.label(line1);
                            // Line 2: connection details.
                            ui.label(egui::RichText::new(&row.details).weak());
                            // Line 3: live stats.
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} msg  ·  {:.0} B/s",
                                    row.messages, row.bytes_per_sec
                                ))
                                .weak(),
                            );
                            // Line 4: per-severity diagnostic counts, color-coded (#8).
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("{} info", row.info))
                                        .weak()
                                        .color(egui::Color32::from_gray(110)),
                                );
                                ui.label(
                                    egui::RichText::new(format!("{} warn", row.warnings))
                                        .color(egui::Color32::from_rgb(150, 100, 0)),
                                );
                                ui.label(
                                    egui::RichText::new(format!("{} err", row.errors))
                                        .color(egui::Color32::from_rgb(170, 30, 30)),
                                );
                            });
                        })
                        .response;
                    // The whole box is display-only and selects on click (#8). It is
                    // interactable as a unit since it holds no inner buttons now.
                    if response.interact(egui::Sense::click()).clicked() {
                        self.selected = Some(id);
                    }
                });
                ui.add_space(6.0);
            }
        });
    }

    fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected else {
            ui.label("No channel selected. Use “+ Add” by the channel list.");
            return;
        };
        self.sync_edit_draft(id);
        let Some((details, status, messages, bps, last_error)) = self.state.channel(id).map(|v| {
            (
                v.details.clone(),
                v.status,
                v.messages,
                v.bytes_per_sec,
                v.last_error.clone(),
            )
        }) else {
            ui.label("That channel is no longer present.");
            return;
        };

        // Top line: status dot + an editable optional name (#6). The name is part of
        // the config draft; Apply commits it (and the list row mirrors it).
        ui.horizontal(|ui| {
            let base = egui::TextStyle::Body.resolve(ui.style()).size;
            ui.label(
                egui::RichText::new("\u{25CF}")
                    .size(base * 1.3)
                    .color(status_color(status)),
            );
            ui.label(bold("Name"));
            // Editing the name renames the channel in place — instant, no restart
            // (the name is a label only, §6). Keep the draft in step so a later
            // Apply of other edits doesn't carry a stale name.
            let mut renamed = None;
            if let Some((_, config)) = &mut self.edit_draft {
                let mut name = config.name.as_str().to_string();
                if ui
                    .add(egui::TextEdit::singleline(&mut name).desired_width(200.0))
                    .changed()
                {
                    config.name = crate::core::ChannelName::new(name.clone());
                    renamed = Some(name);
                }
            }
            if let Some(name) = renamed {
                // Tell the runtime, AND fold the change into the view-model locally.
                // The runtime→UI confirmation (`ChannelRenamed`) is an advisory,
                // lossy push (§99) and can be dropped under load — relying on it
                // left the list row and the re-seeded draft stale (#5). The optimistic
                // local apply makes the rename authoritative on the UI side; the
                // echoed update, if it survives, is idempotent.
                self.send(UiCommand::Rename(
                    id,
                    crate::core::ChannelName::new(name.clone()),
                ));
                self.state.apply(bridge::UiUpdate::ChannelRenamed(id, name));
            }
        });
        ui.horizontal(|ui| {
            ui.label(status_label(status));
            ui.label("·");
            ui.label(egui::RichText::new(&details).weak());
        });
        // Messages + throughput. (Warnings live in the Diagnostics line below — the
        // two were the same per-channel count, so the duplicate here is dropped, #5.)
        ui.label(format!("Messages: {messages}    Throughput: {bps:.0} B/s"));
        // Lifecycle actions, below the stats line (#6); bigger so Stop/Remove stand out.
        ui.horizontal(|ui| {
            let size = egui::vec2(86.0, 30.0);
            match status {
                ChannelStatus::Running => {
                    if ui.add_sized(size, egui::Button::new("Stop")).clicked() {
                        self.send(UiCommand::Stop(id));
                    }
                }
                // Faulted can't go straight to Starting (§8.5): Retry = Stop + Start.
                ChannelStatus::Faulted => {
                    if ui.add_sized(size, egui::Button::new("Retry")).clicked() {
                        self.send(UiCommand::Stop(id));
                        self.try_start(id);
                    }
                }
                _ => {
                    if ui.add_sized(size, egui::Button::new("Start")).clicked() {
                        self.try_start(id);
                    }
                }
            }
            if ui.add_sized(size, egui::Button::new("Remove")).clicked() {
                // Confirm first — removal is destructive and can't be undone (#1).
                self.confirm_remove = Some(id);
            }
        });
        if let Some(err) = &last_error {
            ui.colored_label(egui::Color32::from_rgb(170, 30, 30), format!("⚠ {err}"));
            ui.label(
                "Recourse: change the port below and Apply, free the resource (Stop \
                 the other channel on that port) then Retry, or Remove this channel.",
            );
        }

        // Configure: edit the full interface config on a working copy, then Apply.
        let ports = self.serial_ports.clone();
        // Force the section open for one frame when focus moved to a needy channel
        // (task 1); `None` afterwards so the user can still collapse it.
        let force_open = self.force_config_open.then_some(true);
        let mut apply = false;
        let mut refresh = false;
        if let Some((_, config)) = &mut self.edit_draft {
            egui::CollapsingHeader::new("Configure")
                // Per-channel id so each channel remembers its own open/closed state
                // — closing it on a running channel stays closed on return (#1).
                .id_salt(("configure", id))
                .open(force_open)
                .default_open(true)
                .show(ui, |ui| {
                    refresh = ui
                        .push_id("edit_iface", |ui| edit_interface(ui, id, config, &ports))
                        .inner;
                    apply = ui.button("Apply").clicked();
                });
        }
        self.force_config_open = false;
        if refresh {
            self.refresh_serial_ports();
        }
        if apply {
            self.apply_edit_draft();
        }

        // Live serial control/status lines (§161): green = high, grey = low.
        if let Some(lines) = self.state.channel(id).and_then(|v| v.control_lines) {
            ui.horizontal(|ui| {
                // Outputs are clickable toggles (§161): clicking sends Set{Rts,Dtr};
                // the shown state still comes from the live poll, so it reflects what
                // the port actually did, not just what we asked for.
                ui.label(bold("Out:"));
                if line_toggle(ui, "RTS", lines.rts).clicked() {
                    self.send(UiCommand::SetRts(id, !lines.rts));
                }
                if line_toggle(ui, "DTR", lines.dtr).clicked() {
                    self.send(UiCommand::SetDtr(id, !lines.dtr));
                }
                ui.separator();
                // Inputs are read-only indicators.
                ui.label(bold("In:"));
                line_indicator(ui, "CTS", lines.cts);
                line_indicator(ui, "DSR", lines.dsr);
                line_indicator(ui, "DCD", lines.dcd);
                line_indicator(ui, "RI", lines.ri);
            });
        }

        ui.horizontal(|ui| {
            let base = egui::TextStyle::Body.resolve(ui.style()).size;
            ui.label(bold("View"));
            ui.radio_value(&mut self.msg_mode, DisplayMode::Hex, "Hex");
            ui.radio_value(&mut self.msg_mode, DisplayMode::Rendered, "Rendered");
            ui.radio_value(&mut self.msg_mode, DisplayMode::Raw, "Raw");
            // Fixed-height dividers so the enlarged ␊ below doesn't stretch them.
            vsep(ui);
            // Control-character rendering (§46) — applies to Raw mode. Three styles,
            // matching talker (glyph / token / hex; LF shown as the example). The
            // control-picture glyph is a compact 2-letter design, so it's bumped up to
            // visually match the full-size [LF]/<0A> neighbours.
            ui.add_enabled_ui(self.msg_mode == DisplayMode::Raw, |ui| {
                ui.label(bold("ctrl-chars"));
                ui.radio_value(
                    &mut self.msg_chars,
                    CharacterRendering::Glyph,
                    egui::RichText::new("␊").size(base * 1.6),
                )
                .on_hover_text("Control pictures (␊ ␍ ␉ …)");
                ui.radio_value(&mut self.msg_chars, CharacterRendering::Token, "[LF]")
                    .on_hover_text("Bracketed names ([LF] [CR] [TAB] …)");
                ui.radio_value(&mut self.msg_chars, CharacterRendering::HexEscape, "<0A>")
                    .on_hover_text("Hex escapes (<0A> <0D> <09> …)");
            });
            vsep(ui);
            ui.label(bold("Add"));
            ui.checkbox(&mut self.show_msg_number, "msg #");
            ui.checkbox(&mut self.show_timestamp, "timestamp");
        });
        ui.horizontal(|ui| {
            ui.label(bold("Size"));
            // An editable "combo": type any size into the field, or pick a preset
            // from the ▾ menu (#7 — no separate entry box). The text is the source of
            // truth while editing; a valid parse updates the size (clamped).
            let resp = ui.add(egui::TextEdit::singleline(&mut self.font_text).desired_width(40.0));
            if resp.changed() {
                if let Ok(v) = self.font_text.trim().parse::<f32>() {
                    self.msg_font_size = v.clamp(6.0, 72.0);
                }
            }
            // U+25BC (full triangle), not U+25BE (the "small" one) — the small glyph
            // rendered noticeably tinier than the ComboBox / collapsing arrows.
            ui.menu_button("\u{25BC}", |ui| {
                for &size in MSG_FONT_SIZES {
                    if ui.button(format!("{size:.0}")).clicked() {
                        self.msg_font_size = size;
                        self.font_text = format!("{size:.0}");
                        ui.close();
                    }
                }
            });
            ui.separator();
            // Color scheme as a dropdown. It opens instantly now that the global
            // popup fade is off (see `apply_style`).
            ui.label(bold("Colors"));
            egui::ComboBox::from_id_salt("msg_colors")
                .selected_text(self.msg_colors.label())
                .show_ui(ui, |ui| {
                    for scheme in [
                        ColorScheme::BlackOnWhite,
                        ColorScheme::GreenOnBlack,
                        ColorScheme::AmberOnBlack,
                        ColorScheme::WhiteOnBlack,
                    ] {
                        ui.selectable_value(&mut self.msg_colors, scheme, scheme.label());
                    }
                });
            ui.separator();
            // Monospace face for the message dump.
            ui.label(bold("Mono"));
            egui::ComboBox::from_id_salt("msg_font")
                .selected_text(self.msg_font.label())
                .show_ui(ui, |ui| {
                    for &font in MonoFont::ALL {
                        ui.selectable_value(&mut self.msg_font, font, font.label());
                    }
                });
        });
        let msg_mode = self.msg_mode;
        let msg_chars = self.msg_chars;
        let show_number = self.show_msg_number;
        let show_timestamp = self.show_timestamp;
        let font_size = self.msg_font_size;
        let mono_family = self.msg_font.family();
        let fg = self.msg_colors.fg();
        let bg = self.msg_colors.bg();
        ui.separator();

        // Diagnostics / matches — only meaningful once there's a snapshot. Pull the
        // data into owned locals so the filter checkboxes can mutate `self` without a
        // live `self.state` borrow.
        struct DiagView {
            headline: String,
            headline_color: egui::Color32,
            counts: (usize, usize, usize),
            entries: Vec<(SystemTime, DiagnosticSeverity, String)>,
            matches: Vec<(Option<u64>, String)>,
        }
        let diag_view = self
            .state
            .channel(id)
            .and_then(|v| v.snapshot.as_ref())
            .map(|s| {
                let d = &s.diagnostics;
                let mut entries: Vec<(SystemTime, DiagnosticSeverity, String)> = Vec::new();
                for e in d.events.iter().chain(&d.warnings).chain(&d.errors) {
                    entries.push((e.timestamp, e.severity, e.message.clone()));
                }
                // Chronological now that each entry carries a timestamp (a single
                // timeline across severities, not three separate buckets).
                entries.sort_by_key(|(t, _, _)| *t);
                let (headline, headline_color) = latest_diagnostic(d);
                DiagView {
                    headline,
                    headline_color,
                    counts: (d.events.len(), d.warnings.len(), d.errors.len()),
                    entries,
                    matches: s
                        .matches
                        .iter()
                        .rev()
                        .take(20)
                        .map(|m| {
                            (
                                m.message_number,
                                short_id(&m.rule_id.to_string()).to_string(),
                            )
                        })
                        .collect(),
                }
            });
        if let Some(dv) = diag_view {
            // A real-time, color-coded status line (the channel's headline diagnostic)
            // that opens into a filterable, ms-timestamped log. The header updates
            // every snapshot (5 Hz); errors stay headlined over warnings over info.
            let header =
                egui::RichText::new(format!("Diagnostics — {}", truncate(&dv.headline, 70)))
                    .color(dv.headline_color);
            egui::CollapsingHeader::new(header)
                .id_salt("diagnostics")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (e, w, x) = dv.counts;
                        ui.label(bold("Show"));
                        ui.checkbox(&mut self.show_info, format!("Info ({e})"));
                        ui.checkbox(&mut self.show_warn, format!("Warn ({w})"));
                        ui.checkbox(&mut self.show_error, format!("Error ({x})"));
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("diag_log")
                        .max_height(200.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let mut shown = 0usize;
                            for (t, sev, msg) in dv.entries.iter().rev() {
                                let (enabled, color, level) = match sev {
                                    DiagnosticSeverity::Event => {
                                        (self.show_info, egui::Color32::from_gray(80), "INFO ")
                                    }
                                    DiagnosticSeverity::Warning => (
                                        self.show_warn,
                                        egui::Color32::from_rgb(150, 100, 0),
                                        "WARN ",
                                    ),
                                    DiagnosticSeverity::Error => (
                                        self.show_error,
                                        egui::Color32::from_rgb(170, 30, 30),
                                        "ERROR",
                                    ),
                                };
                                if !enabled {
                                    continue;
                                }
                                let dt: chrono::DateTime<chrono::Local> = (*t).into();
                                shown += 1;
                                ui.colored_label(
                                    color,
                                    format!("{}  {level}  {msg}", dt.format("%H:%M:%S%.3f")),
                                );
                            }
                            if shown == 0 {
                                ui.label(
                                    egui::RichText::new("no diagnostics match the filter").weak(),
                                );
                            }
                        });
                });
            if !dv.matches.is_empty() {
                egui::CollapsingHeader::new(format!("Match firings ({})", dv.matches.len()))
                    .id_salt("matches")
                    .show(ui, |ui| {
                        for (number, rule) in &dv.matches {
                            let on = number
                                .map(|n| format!("#{n}"))
                                .unwrap_or_else(|| "(idle)".to_string());
                            ui.monospace(format!("{on}  rule {rule}"));
                        }
                    });
            }
        }

        ui.separator();
        // Pause/Resume the primary Display View, if one exists.
        let view0 = self
            .state
            .channel(id)
            .and_then(|v| v.snapshot.as_ref())
            .and_then(|s| s.display_views.first())
            .map(|v0| (v0.id, v0.paused));
        if let Some((view_id, is_paused)) = view0 {
            ui.horizontal(|ui| {
                if is_paused {
                    if ui.button("Resume").clicked() {
                        self.send(UiCommand::ResumeDisplay(id, view_id));
                    }
                    ui.label("view paused — reception continues");
                } else if ui.button("Pause").clicked() {
                    self.send(UiCommand::PauseDisplay(id, view_id));
                }
            });
        }

        // The message area is ALWAYS present (#1) — it shows a waiting note before
        // there's data, rather than popping into existence on first Start.
        let has_snapshot = self
            .state
            .channel(id)
            .map(|v| v.snapshot.is_some())
            .unwrap_or(false);
        let count = self
            .state
            .channel(id)
            .and_then(|v| v.snapshot.as_ref())
            .map(|s| {
                s.display_views
                    .first()
                    .map(|v| v.messages.len())
                    .unwrap_or(s.retained.len())
            })
            .unwrap_or(0);
        ui.label(format!("Recent messages ({count}):"));
        egui::Frame::new()
            .fill(bg)
            .inner_margin(4.0)
            .show(ui, |ui| {
                if count == 0 {
                    let note = if has_snapshot {
                        "no messages received yet"
                    } else {
                        "waiting for data — Start the channel"
                    };
                    ui.label(egui::RichText::new(note).weak());
                    return;
                }
                // Render via the domain DisplayView so the GUI matches the runtime's
                // Hex/Rendered/Raw + character-rendering semantics (§42/§46).
                let renderer = DisplayView {
                    mode: msg_mode,
                    encoding: DisplayEncoding::Utf8,
                    character_rendering: msg_chars,
                    wrapping: WrappingMode::NoWrap,
                    wrap_width: None,
                    hex_separator: " ".to_string(),
                    hex_bytes_per_line: 16,
                };
                // Virtualized: only the visible rows are laid out, so the buffer can
                // hold many thousands of messages without stalling the UI (#4). One
                // message per row; long lines extend (use the horizontal scrollbar).
                let row_h = ui.fonts_mut(|f| {
                    f.row_height(&egui::FontId::new(font_size, mono_family.clone()))
                });
                egui::ScrollArea::both()
                    .id_salt("messages")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, count, |ui, range| {
                        let Some(snapshot) =
                            self.state.channel(id).and_then(|v| v.snapshot.as_ref())
                        else {
                            return;
                        };
                        let messages = snapshot
                            .display_views
                            .first()
                            .map(|v| v.messages.as_slice())
                            .unwrap_or(snapshot.retained.as_slice());
                        let end = range.end.min(messages.len());
                        let start = range.start.min(end);
                        for decoded in &messages[start..end] {
                            let mut prefix = String::new();
                            if show_timestamp {
                                let dt: chrono::DateTime<chrono::Local> =
                                    decoded.message.metadata.arrival_timestamp.wall_clock.into();
                                prefix.push_str(&format!("{} ", dt.format("%H:%M:%S%.3f")));
                            }
                            if show_number {
                                prefix.push_str(&format!("#{} ", decoded.message.number));
                            }
                            let kind = decoded
                                .protocol
                                .as_ref()
                                .and_then(|p| p.message_type.clone())
                                .map(|t| format!("[{t}] "))
                                .unwrap_or_default();
                            // Flatten embedded CR/LF so each message stays a single
                            // virtualized row (Raw+Native can otherwise emit newlines).
                            let body = renderer
                                .render_text(&decoded.message.bytes)
                                .replace(['\n', '\r'], " ");
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("{prefix}{kind}{body}"))
                                        .font(egui::FontId::new(font_size, mono_family.clone()))
                                        .color(fg),
                                )
                                .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        }
                    });
            });
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
                    .add(egui::Button::new("Remove").fill(egui::Color32::from_rgb(170, 30, 30)))
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
                        let dot = egui::RichText::new("\u{25CF}")
                            .size(base * 1.5)
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
    }
}

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
fn edit_interface(
    ui: &mut egui::Ui,
    channel_id: ChannelId,
    config: &mut ChannelConfig,
    serial_ports: &[String],
) -> bool {
    let mut refresh = false;

    // NMEA decode belongs with the connection: it's the per-channel decoder (§30),
    // not a display option. Toggling it sets the channel's decoder.
    let mut nmea = matches!(config.decoder, DecoderConfig::Nmea0183 { .. });
    if ui
        .checkbox(&mut nmea, "NMEA decode")
        .on_hover_text("Decode received messages as NMEA 0183 (adds protocol metadata)")
        .changed()
    {
        config.decoder = if nmea {
            DecoderConfig::Nmea0183 {
                validation_mode: NmeaValidationMode::Standard,
            }
        } else {
            DecoderConfig::None
        };
    }

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
fn list_serial_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|list| list.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    ports.sort();
    ports
}

/// A fresh template config for the given interface kind. New UDP channels default
/// to Broadcast (the first/most-common option for this tool).
fn template_for(kind: AddKind) -> ChannelConfig {
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
fn config_incomplete(config: &ChannelConfig) -> bool {
    match &config.interface {
        InterfaceConfig::Udp(u) => u.port == 0,
        InterfaceConfig::TcpListener(t) => t.port == 0,
        InterfaceConfig::Serial(s) => s.port.trim().is_empty(),
    }
}

/// The headline diagnostic for the real-time status line: the most recent error,
/// else the most recent warning, else the most recent event, with its display color.
/// Errors win so a fault stays visible in the collapsed header while troubleshooting.
fn latest_diagnostic(
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
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}\u{2026}")
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

/// A serial control-line indicator (§161): the line name colored green when the
/// line is high (asserted), grey when low, with a hover tooltip.
fn line_indicator(ui: &mut egui::Ui, name: &str, high: bool) {
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
fn line_toggle(ui: &mut egui::Ui, name: &str, high: bool) -> egui::Response {
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
fn status_color(status: ChannelStatus) -> egui::Color32 {
    match status {
        ChannelStatus::Running => egui::Color32::from_rgb(30, 150, 30),
        ChannelStatus::Stopped => egui::Color32::from_gray(120),
        ChannelStatus::Faulted => egui::Color32::from_rgb(190, 40, 40),
        ChannelStatus::Reconnecting => egui::Color32::from_rgb(200, 140, 0),
    }
}

/// Shorten a UUID string to its first segment, enough to disambiguate at a glance.
fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// A vertical divider at the standard control height. Unlike `ui.separator()` (which
/// stretches to the row height), this stays a fixed length, so a row containing an
/// over-tall element — e.g. the enlarged `␊` glyph — doesn't get a taller divider
/// than every other row.
fn vsep(ui: &mut egui::Ui) {
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
