//! The apps' named color palette — one place to change any chrome color, so a
//! restyle is a constant edit, not a hunt through call sites. Values migrated
//! from `listener/src/gui/theme.rs` (talker ADR-016 / listener ADR-019).
//!
//! Two const instances: [`LIGHT`] (the shipped listener look) and [`DARK`]
//! (several light values are unreadable on a dark backdrop, so each gets an
//! explicit counterpart).
//!
//! Stream/display *content* colors are deliberately **not** here — those are
//! user-chosen per view.

use egui::Color32;

/// The palette matching the `Ui`'s active theme. The single accessor both
/// apps and the shared chrome use, so every call site recolors live when the
/// user toggles themes.
pub fn active(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// The chrome colors, grouped by meaning rather than by widget.
pub struct Palette {
    // ── Status / severity ────────────────────────────────────────────────
    /// Everything that means "fault / error / destructive" — channel fault,
    /// recording fault, error diagnostics, the Remove button. One red so
    /// "something is wrong" always looks identical.
    pub fault_red: Color32,
    /// A running / active channel (and an active recording). A bright
    /// blue-green chosen to read distinctly from the fault red for red-green
    /// color blindness.
    pub running_green: Color32,
    /// A reconnecting channel — a yellower amber, pushed away from the red.
    pub reconnecting_amber: Color32,
    /// A warning (diagnostics, "won't start" hints).
    pub warning_amber: Color32,
    /// A stopped/idle status glyph and other "neutral, inactive" accents.
    pub idle_grey: Color32,

    // ── Diagnostic-log text ──────────────────────────────────────────────
    /// An INFO-severity diagnostic line.
    pub info_grey: Color32,
    /// A faded info line in a real-time headline.
    pub event_grey: Color32,
    /// A per-tab "info" count (lighter, secondary).
    pub count_info_grey: Color32,

    // ── Serial control lines ─────────────────────────────────────────────
    /// A high (asserted) serial control/status line.
    pub line_high_green: Color32,
    /// A low serial control/status line.
    pub line_low_grey: Color32,

    // ── Structure ────────────────────────────────────────────────────────
    /// The channel-card border.
    pub box_stroke: Color32,
}

/// The light-theme palette — the values listener shipped with.
pub const LIGHT: Palette = Palette {
    fault_red: Color32::from_rgb(170, 30, 30),
    running_green: Color32::from_rgb(0, 200, 140),
    reconnecting_amber: Color32::from_rgb(220, 180, 0),
    warning_amber: Color32::from_rgb(150, 100, 0),
    idle_grey: Color32::from_gray(120),
    info_grey: Color32::from_gray(80),
    event_grey: Color32::from_gray(60),
    count_info_grey: Color32::from_gray(110),
    line_high_green: Color32::from_rgb(30, 150, 30),
    line_low_grey: Color32::from_gray(150),
    box_stroke: Color32::from_rgb(140, 160, 200),
};

/// The dark-theme palette. Brighter accents and lighter greys so every value
/// stays readable on a dark backdrop (the light `warning_amber`/`info_grey`
/// would all but vanish). Tune values here as the dark look evolves.
pub const DARK: Palette = Palette {
    fault_red: Color32::from_rgb(235, 90, 90),
    running_green: Color32::from_rgb(0, 210, 150),
    reconnecting_amber: Color32::from_rgb(230, 195, 60),
    warning_amber: Color32::from_rgb(230, 175, 70),
    idle_grey: Color32::from_gray(150),
    info_grey: Color32::from_gray(180),
    event_grey: Color32::from_gray(200),
    count_info_grey: Color32::from_gray(140),
    line_high_green: Color32::from_rgb(80, 210, 80),
    line_low_grey: Color32::from_gray(120),
    box_stroke: Color32::from_rgb(110, 130, 175),
};
