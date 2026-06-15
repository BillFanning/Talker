//! The GUI's named color palette and shared button styling. One place to change any
//! chrome color, so a restyle is a constant edit, not a hunt through call sites.
//!
//! The stream-view text colors are deliberately **not** here — those are user-chosen
//! per view via [`super::widgets::ColorScheme`].

use egui::Color32;

// ── Status / severity ────────────────────────────────────────────────────────

/// Everything that means "fault / error / destructive" — channel fault, recording
/// fault, error diagnostics, the Remove button. One red so "something is wrong"
/// always looks identical.
pub(super) const FAULT_RED: Color32 = Color32::from_rgb(170, 30, 30);

/// A running / active channel (and an active recording). A bright blue-green chosen to
/// read distinctly from the red fault for red-green color blindness.
pub(super) const RUNNING_GREEN: Color32 = Color32::from_rgb(0, 200, 140);

/// A reconnecting channel — a yellower amber, pushed away from the red fault.
pub(super) const RECONNECTING_AMBER: Color32 = Color32::from_rgb(220, 180, 0);

/// A warning (diagnostics, "won't start without a destination" hints).
pub(super) const WARNING_AMBER: Color32 = Color32::from_rgb(150, 100, 0);

/// A stopped/idle status glyph and other "neutral, inactive" accents.
pub(super) const IDLE_GREY: Color32 = Color32::from_gray(120);

// ── Diagnostic-log text ──────────────────────────────────────────────────────

/// An INFO-severity diagnostic line.
pub(super) const INFO_GREY: Color32 = Color32::from_gray(80);
/// A faded info line in the real-time headline (slightly darker than INFO).
pub(super) const EVENT_GREY: Color32 = Color32::from_gray(60);
/// The per-tab "info" count in the channel list (lighter, secondary).
pub(super) const COUNT_INFO_GREY: Color32 = Color32::from_gray(110);

// ── Serial control lines (§161) ──────────────────────────────────────────────

/// A high (asserted) serial control/status line.
pub(super) const LINE_HIGH_GREEN: Color32 = Color32::from_rgb(30, 150, 30);
/// A low serial control/status line.
pub(super) const LINE_LOW_GREY: Color32 = Color32::from_gray(150);

// ── Structure ────────────────────────────────────────────────────────────────

/// The channel-card border (talker's connection-card color).
pub(super) const BOX_STROKE: Color32 = Color32::from_rgb(140, 160, 200);
