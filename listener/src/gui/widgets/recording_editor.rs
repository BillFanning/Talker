//! The Raw and Display recording setup editors (§53–§59): destination, overwrite
//! policy, rotation, timestamps. Config-driven (applied via a §13 Reconfigure); the
//! live Record toggle (ADR-012) is separate. Both editors share `recording_file_fields`.

use std::path::PathBuf;

use crate::config::ChannelConfig;
use crate::record::{FileRotationPolicy, OverwritePolicy};

use super::super::theme;

/// The shared destination / overwrite / rotation / timestamp controls for a recording
/// (§55–§59), used by both the Raw and Display editors (their config structs carry the
/// same fields; ADR-013). `ext` is the file extension shown in hints (".raw"/".disp").
#[allow(clippy::too_many_arguments)]
fn recording_file_fields(
    ui: &mut egui::Ui,
    ext: &str,
    destination: &mut Option<PathBuf>,
    overwrite_policy: &mut OverwritePolicy,
    file_rotation: &mut FileRotationPolicy,
    timestamp_enabled: &mut bool,
    // When `Some`, a "Record at start" checkbox is shown on the same line, left of the
    // "Record timestamps" checkbox (Raw uses this; Display has its own enable above).
    record_at_start: Option<&mut bool>,
) {
    // A single file when not rotating; a directory of <channel>_<period> files
    // otherwise (§59). `rotating` reflects this frame's start — a one-frame lag when
    // the user flips rotation below is harmless.
    let rotating = *file_rotation != FileRotationPolicy::None;
    ui.horizontal(|ui| {
        ui.label(if rotating { "Folder" } else { "File" });
        let mut path = destination
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if ui
            .add(egui::TextEdit::singleline(&mut path).desired_width(180.0))
            .changed()
        {
            let trimmed = path.trim();
            *destination = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        }
        if ui.button("Browse…").clicked() {
            let picked = if rotating {
                rfd::FileDialog::new().pick_folder()
            } else {
                rfd::FileDialog::new().save_file()
            };
            if let Some(p) = picked {
                *destination = Some(p);
            }
        }
    });
    if destination.is_none() {
        ui.label(
            egui::RichText::new("⚠ set a destination — recording won't start without one")
                .color(theme::WARNING_AMBER),
        );
    }
    ui.horizontal(|ui| {
        ui.label("On exists");
        ui.radio_value(overwrite_policy, OverwritePolicy::Refuse, "Refuse");
        ui.radio_value(overwrite_policy, OverwritePolicy::Overwrite, "Overwrite");
        ui.radio_value(overwrite_policy, OverwritePolicy::AppendIfExists, "Append");
    });
    ui.horizontal(|ui| {
        ui.label("Rotate");
        ui.radio_value(file_rotation, FileRotationPolicy::None, "None");
        ui.radio_value(file_rotation, FileRotationPolicy::Hourly, "Hourly");
        ui.radio_value(file_rotation, FileRotationPolicy::Daily, "Daily")
            .on_hover_text(
                "Rotating files are named <channel>_<period> — keep the channel name \
                 filesystem-safe (§59)",
            );
    });
    ui.horizontal(|ui| {
        if let Some(enabled) = record_at_start {
            ui.checkbox(enabled, "Record at start")
                .on_hover_text("Begin recording when the channel starts (§53)");
        }
        ui.checkbox(timestamp_enabled, format!("Record timestamps ({ext})"))
            .on_hover_text("Sidecar index for Raw; inline for Display (§57)");
    });
}

/// Edit the channel's **Raw** recording setup (§53): destination, overwrite,
/// rotation, timestamps, and the "record at start" flag. The byte-exact verbatim
/// stream (§53) — a separate pipeline tap from Display (ADR-013). Config-driven:
/// applied via a §13 Reconfigure. The live Record toggle (ADR-012) begins/stops it at
/// runtime without a restart, as long as a destination is set.
pub(crate) fn edit_raw_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.raw_recording;
    recording_file_fields(
        ui,
        ".raw",
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
        &mut rec.timestamp_enabled,
        Some(&mut rec.enabled), // "Record at start", shown left of "Record timestamps"
    );
}

/// Edit the channel's **Display** recording setup (§54): records the rendered view
/// output (`.disp`) — a separate pipeline tap from Raw (ADR-013). Config-driven:
/// applied via a §13 Reconfigure.
pub(crate) fn edit_display_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.display_recording;
    ui.checkbox(&mut rec.enabled, "Record display output (.disp)")
        .on_hover_text("Record the rendered view, not the raw bytes (§54)");
    if !rec.enabled {
        return;
    }
    recording_file_fields(
        ui,
        ".disp",
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
        &mut rec.timestamp_enabled,
        None, // Display has its own enable checkbox above
    );
}
