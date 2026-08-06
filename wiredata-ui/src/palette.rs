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

/// A semantic accent softened into a surface fill by blending it into the
/// panel behind it, `alpha` out of 255.
///
/// A surface tinted this way needs no light and dark variants: it is derived
/// from the theme's own panel color, so it lands pale on light and deep on
/// dark by construction. That is why the palette holds accents and not
/// backgrounds — a hardcoded pair of tints is the same decision made twice,
/// and it drifts the moment an accent changes.
pub fn tint(ui: &egui::Ui, accent: Color32, alpha: u8) -> Color32 {
    ui.visuals()
        .panel_fill
        .blend(Color32::from_rgba_unmultiplied(
            accent.r(),
            accent.g(),
            accent.b(),
            alpha,
        ))
}

/// The chrome colors, grouped by meaning rather than by widget.
///
/// # Naming
///
/// Every field names the **role**, never the hue. `fault`, not `fault_red` —
/// because a palette exists precisely so a colour can change, and a name that
/// encodes the value contradicts the thing it is for. This is not hypothetical:
/// `fault` was red until it turned out to be indistinguishable from `warning`
/// for a red-green colour deficiency, and the two serial-line colours are next
/// in line for the same reason. `Color32` in a struct called `Palette` already
/// says these are colours; the field only has to say what for.
pub struct Palette {
    // ── Status / severity ────────────────────────────────────────────────
    /// Everything that means "fault / error / destructive" — channel fault,
    /// recording fault, error diagnostics, the Remove button. One color so
    /// "something is wrong" always looks identical.
    ///
    /// Blue, deliberately. This is the app's most important signal, so it must
    /// survive the most common colour deficiency; blue is discriminable on
    /// every common type, being the axis red-green deficiency leaves intact.
    /// Red was tried first and failed against [`Palette::warning`] — see talker
    /// ADR-049. Do not "restore" it.
    pub fault: Color32,
    /// A running / active channel, and an active recording.
    pub running: Color32,
    /// A reconnecting channel — a yellower amber than [`Palette::warning`].
    pub reconnecting: Color32,
    /// A warning (diagnostics, "won't start" hints).
    pub warning: Color32,
    /// A stopped/idle status glyph and other "neutral, inactive" accents.
    pub idle: Color32,

    // ── Diagnostic-log text ──────────────────────────────────────────────
    /// An INFO-severity diagnostic line.
    pub info: Color32,
    /// A faded info line in a real-time headline.
    pub event: Color32,
    /// A per-tab "info" count (lighter, secondary).
    pub count_info: Color32,

    // ── Serial control lines ─────────────────────────────────────────────
    /// A high (asserted) serial control/status line. Reinforces the filled
    /// [`crate::glyphs::LINE_HIGH`] glyph; it does not carry the level alone.
    pub line_high: Color32,
    /// A low serial control/status line. Reinforces the hollow
    /// [`crate::glyphs::LINE_LOW`] glyph.
    pub line_low: Color32,
}

/// The light-theme palette.
pub const LIGHT: Palette = Palette {
    // Deep enough to carry white text on the Remove button's fill.
    fault: Color32::from_rgb(0, 85, 200),
    running: Color32::from_rgb(0, 200, 140),
    reconnecting: Color32::from_rgb(220, 180, 0),
    warning: Color32::from_rgb(150, 100, 0),
    idle: Color32::from_gray(120),
    info: Color32::from_gray(80),
    event: Color32::from_gray(60),
    count_info: Color32::from_gray(110),
    line_high: Color32::from_rgb(30, 150, 30),
    line_low: Color32::from_gray(150),
};

/// The dark-theme palette. Brighter accents and lighter greys so every value
/// stays readable on a dark backdrop (the light `warning`/`info` would all but
/// vanish). Tune values here as the dark look evolves.
pub const DARK: Palette = Palette {
    // Lightened so it stays legible as small text on the dark panel.
    fault: Color32::from_rgb(95, 165, 255),
    running: Color32::from_rgb(0, 210, 150),
    reconnecting: Color32::from_rgb(230, 195, 60),
    warning: Color32::from_rgb(230, 175, 70),
    idle: Color32::from_gray(150),
    info: Color32::from_gray(180),
    event: Color32::from_gray(200),
    count_info: Color32::from_gray(140),
    line_high: Color32::from_rgb(80, 210, 80),
    line_low: Color32::from_gray(120),
};
