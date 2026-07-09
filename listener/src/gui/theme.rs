//! The GUI's named color palette and shared button styling. The values now
//! live in the shared `wiredata-ui` palette (ADR-019); this module pins
//! listener's call sites to the **light** palette — the dark one arrives with
//! the theme toggle.
//!
//! The stream-view text colors are deliberately **not** here — those are
//! user-chosen per view via [`super::widgets::ColorScheme`].

use egui::Color32;
use wiredata_ui::palette::LIGHT;

// ── Status / severity ────────────────────────────────────────────────────────

/// Everything that means "fault / error / destructive" — channel fault, recording
/// fault, error diagnostics, the Remove button. One red so "something is wrong"
/// always looks identical.
pub(super) const FAULT_RED: Color32 = LIGHT.fault_red;

/// A running / active channel (and an active recording). A bright blue-green chosen to
/// read distinctly from the red fault for red-green color blindness.
pub(super) const RUNNING_GREEN: Color32 = LIGHT.running_green;

/// A reconnecting channel — a yellower amber, pushed away from the red fault.
pub(super) const RECONNECTING_AMBER: Color32 = LIGHT.reconnecting_amber;

/// A warning (diagnostics, "won't start without a destination" hints).
pub(super) const WARNING_AMBER: Color32 = LIGHT.warning_amber;

/// A stopped/idle status glyph and other "neutral, inactive" accents.
pub(super) const IDLE_GREY: Color32 = LIGHT.idle_grey;

// ── Diagnostic-log text ──────────────────────────────────────────────────────

/// An INFO-severity diagnostic line.
pub(super) const INFO_GREY: Color32 = LIGHT.info_grey;
/// A faded info line in the real-time headline (slightly darker than INFO).
pub(super) const EVENT_GREY: Color32 = LIGHT.event_grey;
/// The per-tab "info" count in the channel list (lighter, secondary).
pub(super) const COUNT_INFO_GREY: Color32 = LIGHT.count_info_grey;

// ── Serial control lines (§161) ──────────────────────────────────────────────

/// A high (asserted) serial control/status line.
pub(super) const LINE_HIGH_GREEN: Color32 = LIGHT.line_high_green;
/// A low serial control/status line.
pub(super) const LINE_LOW_GREY: Color32 = LIGHT.line_low_grey;

// ── Structure ────────────────────────────────────────────────────────────────

/// The channel-card border (talker's connection-card color).
pub(super) const BOX_STROKE: Color32 = LIGHT.box_stroke;
