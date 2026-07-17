//! Status presentation: lifecycle/recording button decisions, the headline
//! diagnostic, serial control-line indicators, and the shared status-glyph set with
//! its optical sizing. Pure (the button/indicator decisions are unit-tested in the
//! parent's test module); only the line/glyph painters touch egui.

use super::super::state::ChannelStatus;
use wiredata_ui::palette::Palette;

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
    pal: &Palette,
) -> (&'static str, egui::Color32, &'static str) {
    use crate::core::RecordingState;
    // Same colors as channel status (`status_color`): the active ● is RUNNING_GREEN
    // (like a Running channel), faulted ⚠ is FAULT_RED, off ■ is IDLE_GREY.
    use wiredata_ui::glyphs;
    match recording {
        Some(RecordingState::Enabled) => (glyphs::RUNNING, pal.running_green, "recording"),
        Some(RecordingState::Faulted) => (glyphs::FAULT, pal.fault_red, "faulted"),
        Some(RecordingState::Disabled) | None => (glyphs::STOPPED, pal.idle_grey, "off"),
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
    let pal = wiredata_ui::palette::active(ui);
    let color = if high {
        pal.line_high_green
    } else {
        pal.line_low_grey
    };
    ui.colored_label(color, name)
        .on_hover_text(if high { "high" } else { "low" });
}

/// A clickable serial output-line toggle (RTS/DTR, §161): a selectable chip,
/// highlighted and green when the line is asserted (high). Returns the click
/// response so the caller can send the matching Set command.
pub(crate) fn line_toggle(ui: &mut egui::Ui, name: &str, high: bool) -> egui::Response {
    let color = if high {
        wiredata_ui::palette::active(ui).line_high_green
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
/// Pure — callers pass the active theme's palette (`wiredata_ui::palette::active`),
/// the same pattern as talker's `lifecycle_indicator`.
pub(crate) fn status_color(status: ChannelStatus, pal: &Palette) -> egui::Color32 {
    match status {
        ChannelStatus::Running => pal.running_green,
        ChannelStatus::Stopped => pal.idle_grey,
        ChannelStatus::Faulted => pal.fault_red,
        ChannelStatus::Reconnecting => pal.reconnecting_amber,
    }
}

/// The shared status symbol set + its size (now `wiredata_ui::glyphs` — talker
/// uses the same set), used for BOTH channel-lifecycle and raw-recording state
/// so the two read consistently:
/// - Stopped / recording-off → `■`
/// - Running / recording-on → `●`
/// - Faulted (channel or recording) → `⚠`
/// - Reconnecting → `●`
///
/// Returns the glyph and the body-relative size (base scale × optical correction), so
/// every call site renders the same symbol at the same apparent size. Pair with
/// [`status_color`] (channel) or the recording color from [`recording_indicator`].
pub(crate) fn status_glyph(status: ChannelStatus) -> (&'static str, f32) {
    use wiredata_ui::glyphs;
    let glyph = match status {
        ChannelStatus::Stopped => glyphs::STOPPED,
        ChannelStatus::Faulted => glyphs::FAULT,
        ChannelStatus::Running | ChannelStatus::Reconnecting => glyphs::RUNNING,
    };
    (glyph, glyphs::glyph_size(glyph))
}

/// The size for a recording-indicator glyph, matching [`status_glyph`]'s optical
/// sizing for the same symbol.
pub(crate) use wiredata_ui::glyphs::glyph_size as recording_glyph_size;

/// The fixed-cell, non-interactive glyph painter — see `wiredata_ui::glyphs`.
pub(crate) use wiredata_ui::glyphs::paint_glyph;
