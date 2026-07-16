//! Theme-aware access to the shared wiredata palette (ADR-019). The chrome
//! colors live in `wiredata-ui` with a `LIGHT` and a `DARK` instance; this
//! module picks the one matching the **active** theme, so every call site
//! recolors live when the user toggles.
//!
//! The active-theme flag is a process-global mirror of egui's own app-global
//! theme preference, written only by the UI thread (startup + the toggle
//! button). It exists so the pure color helpers (`status_color` and friends,
//! unit-tested without a `Ui`) stay argument-free.
//!
//! The stream-view text colors are deliberately **not** here — those are
//! user-chosen per view via [`super::widgets::ColorScheme`], with both light
//! and dark schemes in the picker.

use std::sync::atomic::{AtomicBool, Ordering};

use egui::Color32;
use wiredata_ui::palette::{Palette, DARK, LIGHT};

static DARK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Record the active theme. Call whenever the egui theme preference is set
/// (startup restore and the header's ◐ toggle) so the palette follows it.
pub(super) fn set_dark_active(dark: bool) {
    DARK_ACTIVE.store(dark, Ordering::Relaxed);
}

/// The palette matching the active theme.
pub(super) fn palette() -> &'static Palette {
    if DARK_ACTIVE.load(Ordering::Relaxed) {
        &DARK
    } else {
        &LIGHT
    }
}

// ── Status / severity ────────────────────────────────────────────────────────

/// Everything that means "fault / error / destructive" — channel fault, recording
/// fault, error diagnostics, the Remove button. One red so "something is wrong"
/// always looks identical.
pub(super) fn fault_red() -> Color32 {
    palette().fault_red
}

/// A running / active channel (and an active recording). A bright blue-green chosen to
/// read distinctly from the red fault for red-green color blindness.
pub(super) fn running_green() -> Color32 {
    palette().running_green
}

/// A reconnecting channel — a yellower amber, pushed away from the red fault.
pub(super) fn reconnecting_amber() -> Color32 {
    palette().reconnecting_amber
}

/// A warning (diagnostics, "won't start without a destination" hints).
pub(super) fn warning_amber() -> Color32 {
    palette().warning_amber
}

/// A stopped/idle status glyph and other "neutral, inactive" accents.
pub(super) fn idle_grey() -> Color32 {
    palette().idle_grey
}

// ── Diagnostic-log text ──────────────────────────────────────────────────────

/// An INFO-severity diagnostic line.
pub(super) fn info_grey() -> Color32 {
    palette().info_grey
}

/// A faded info line in the real-time headline (slightly darker than INFO).
pub(super) fn event_grey() -> Color32 {
    palette().event_grey
}

// ── Serial control lines (§161) ──────────────────────────────────────────────

/// A high (asserted) serial control/status line.
pub(super) fn line_high_green() -> Color32 {
    palette().line_high_green
}

/// A low serial control/status line.
pub(super) fn line_low_grey() -> Color32 {
    palette().line_low_grey
}
