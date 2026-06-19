//! Status presentation: lifecycle/recording button decisions, the headline
//! diagnostic, serial control-line indicators, and the shared status-glyph set with
//! its optical sizing. Pure (the button/indicator decisions are unit-tested in the
//! parent's test module); only the line/glyph painters touch egui.

use super::super::state::ChannelStatus;
use super::super::theme;

/// The Start/Apply/Retry button's label and enabled state, from the channel status
/// and whether the edit draft has pending changes (§8.5). Pure decision, unit-tested;
/// the detail pane just renders the result and dispatches on click:
/// - Stopped → "Start Channel", enabled (Stopped→Starting is legal)
/// - Running + pending edits → "Apply & Restart", enabled (a coordinated restart)
/// - Running + no edits → "Start Channel", **disabled** (nothing to do)
/// - Faulted → "Retry Channel", enabled — one `CommitAndStart`; the runtime
///   normalizes Faulted→Stopped before starting (§8.5), no client-side Stop step
/// - Reconnecting → "Start Channel", **disabled** — Stop is the only valid action
///   mid-reconnect.
pub(crate) fn start_button(status: ChannelStatus, config_changed: bool) -> (&'static str, bool) {
    match status {
        ChannelStatus::Stopped => ("Start Channel", true),
        ChannelStatus::Running if config_changed => ("Apply & Restart", true),
        ChannelStatus::Running => ("Start Channel", false),
        ChannelStatus::Faulted => ("Retry Channel", true),
        ChannelStatus::Reconnecting => ("Start Channel", false),
    }
}

/// Whether the Stop button is enabled: a Stop is legal from Running, Faulted, or
/// Reconnecting (it returns any of those to Stopped, §8.5/§10.2), and illegal from
/// Stopped. Pure, unit-tested.
pub(crate) fn stop_enabled(status: ChannelStatus) -> bool {
    matches!(
        status,
        ChannelStatus::Running | ChannelStatus::Faulted | ChannelStatus::Reconnecting
    )
}

/// The recording-state indicator: glyph, color, and label for a channel's raw
/// recording state (§53). Uses the **same symbol set and colors as channel status**
/// ([`status_glyph`] / [`status_color`]) — `■` off (grey), `●` recording (green), `⚠`
/// faulted (red) — so the two read consistently. Pure, unit-tested; the detail pane
/// renders it as a colored label sized via [`recording_glyph_size`].
pub(crate) fn recording_indicator(
    recording: Option<crate::core::RecordingState>,
) -> (&'static str, egui::Color32, &'static str) {
    use crate::core::RecordingState;
    // Same colors as channel status (`status_color`): the active ● is RUNNING_GREEN
    // (like a Running channel), faulted ⚠ is FAULT_RED, off ■ is IDLE_GREY.
    match recording {
        Some(RecordingState::Enabled) => ("\u{25CF}", theme::RUNNING_GREEN, "recording"),
        Some(RecordingState::Faulted) => ("\u{26A0}", theme::FAULT_RED, "faulted"),
        Some(RecordingState::Disabled) | None => ("\u{25A0}", theme::IDLE_GREY, "off"),
    }
}

/// The headline diagnostic for the real-time status line: the most recent error,
/// else the most recent warning, else the most recent event, with its display color.
/// Errors win so a fault stays visible in the collapsed header while troubleshooting.
pub(crate) fn latest_diagnostic(
    diag: &crate::runtime::snapshot::DiagnosticsSnapshot,
) -> (String, egui::Color32) {
    if let Some(d) = diag.errors.last() {
        (d.message.clone(), theme::FAULT_RED)
    } else if let Some(d) = diag.warnings.last() {
        (d.message.clone(), theme::WARNING_AMBER)
    } else if let Some(d) = diag.events.last() {
        (d.message.clone(), theme::EVENT_GREY)
    } else {
        ("no activity yet".to_string(), theme::IDLE_GREY)
    }
}

/// A short status word for the detail pane.
pub(crate) fn status_label(status: ChannelStatus) -> &'static str {
    match status {
        ChannelStatus::Stopped => "stopped",
        ChannelStatus::Running => "running",
        ChannelStatus::Faulted => "faulted",
        ChannelStatus::Reconnecting => "reconnecting",
    }
}

/// A serial control-line indicator (§161): the line name colored green when the
/// line is high (asserted), grey when low, with a hover tooltip.
pub(crate) fn line_indicator(ui: &mut egui::Ui, name: &str, high: bool) {
    let color = if high {
        theme::LINE_HIGH_GREEN
    } else {
        theme::LINE_LOW_GREY
    };
    ui.colored_label(color, name)
        .on_hover_text(if high { "high" } else { "low" });
}

/// A clickable serial output-line toggle (RTS/DTR, §161): a selectable chip,
/// highlighted and green when the line is asserted (high). Returns the click
/// response so the caller can send the matching Set command.
pub(crate) fn line_toggle(ui: &mut egui::Ui, name: &str, high: bool) -> egui::Response {
    let color = if high {
        theme::LINE_HIGH_GREEN
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

/// The color for a status glyph. Running is a bright blue-green and Reconnecting a
/// yellower amber, both chosen to read distinctly from the red fault for red-green
/// color blindness (the distinct glyphs ●/■/⚠ are the primary signal; color reinforces).
/// All values live in [`super::super::theme`].
pub(crate) fn status_color(status: ChannelStatus) -> egui::Color32 {
    match status {
        ChannelStatus::Running => theme::RUNNING_GREEN,
        ChannelStatus::Stopped => theme::IDLE_GREY,
        ChannelStatus::Faulted => theme::FAULT_RED,
        ChannelStatus::Reconnecting => theme::RECONNECTING_AMBER,
    }
}

/// The base size multiplier for status indicator glyphs (relative to body size). The
/// square (`■`) is the reference at this size; the dot and triangle are enlarged by
/// [`glyph_scale`] to match the square's apparent size.
pub(crate) const STATUS_GLYPH_SCALE: f32 = 1.5;

/// Per-glyph optical correction: `●`/`■`/`⚠` have different bounding boxes, so at one
/// font size they look different sizes. The square is the reference (1.0); the dot and
/// triangle are enlarged so all three read the same size. Multiply into
/// [`STATUS_GLYPH_SCALE`].
fn glyph_scale(glyph: &str) -> f32 {
    match glyph {
        "\u{25A0}" => 1.0,  // ■ square — the reference
        "\u{25CF}" => 1.34, // ● dot — enlarge up to the square
        "\u{26A0}" => 1.30, // ⚠ triangle — enlarge up to the square
        _ => 1.0,
    }
}

/// The shared status symbol set + its size, used for BOTH channel-lifecycle and raw-
/// recording state so the two read consistently:
/// - Stopped / recording-off → `■`
/// - Running / recording-on → `●`
/// - Faulted (channel or recording) → `⚠`
/// - Reconnecting → `●`
///
/// Returns the glyph and the body-relative size (base scale × optical correction), so
/// every call site renders the same symbol at the same apparent size. Pair with
/// [`status_color`] (channel) or the recording color from [`recording_indicator`].
pub(crate) fn status_glyph(status: ChannelStatus) -> (&'static str, f32) {
    let glyph = match status {
        ChannelStatus::Stopped => "\u{25A0}", // ■ square
        ChannelStatus::Faulted => "\u{26A0}", // ⚠ triangle
        ChannelStatus::Running | ChannelStatus::Reconnecting => "\u{25CF}", // ● dot
    };
    (glyph, STATUS_GLYPH_SCALE * glyph_scale(glyph))
}

/// The size for a recording-indicator glyph, matching [`status_glyph`]'s optical
/// sizing for the same symbol.
pub(crate) fn recording_glyph_size(glyph: &str) -> f32 {
    STATUS_GLYPH_SCALE * glyph_scale(glyph)
}

/// Paint a status/recording `glyph` into a **fixed-size, non-interactive cell**,
/// centered. Painting (rather than adding a sized label) keeps the glyph from driving
/// the row height — a taller glyph otherwise shifts the line beside it. `allocate_space`
/// reserves only layout space with no widget id, so there's no stray hover/focus
/// rectangle. `scale` is the body-relative glyph size (from [`status_glyph`] /
/// [`recording_glyph_size`]); the cell is sized to the largest glyph.
pub(crate) fn paint_glyph(ui: &mut egui::Ui, glyph: &str, scale: f32, color: egui::Color32) {
    let base = egui::TextStyle::Body.resolve(ui.style()).size;
    let (_id, rect) = ui.allocate_space(egui::vec2(base * 1.5, base));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(base * scale),
        color,
    );
}
