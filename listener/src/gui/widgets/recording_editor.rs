//! The Raw and Display recording setup editors (§53–§59): destination, overwrite
//! policy, rotation, timestamps. Config-driven (applied via a §13 Reconfigure); the
//! live Record toggle (ADR-012) is separate. Both editors share `recording_file_fields`.

use std::path::PathBuf;

use crate::config::ChannelConfig;
use crate::record::{FileRotationPolicy, OverwritePolicy};

use super::super::theme;

/// The shared destination / overwrite / rotation controls for a recording (§55–§59),
/// used by both the Raw and Display editors (their config structs carry the same
/// fields; ADR-013). `ext` is the file extension shown in hints (".raw"/".disp").
#[allow(clippy::too_many_arguments)]
fn recording_file_fields(
    ui: &mut egui::Ui,
    ext: &str,
    destination: &mut Option<PathBuf>,
    overwrite_policy: &mut OverwritePolicy,
    file_rotation: &mut FileRotationPolicy,
    // When `Some`, a "Record timestamps" checkbox is shown — Display only. Raw passes
    // `None`: a `.raw` file is the verbatim byte stream, so it has no timestamp option.
    timestamp_enabled: Option<&mut bool>,
    // When `Some`, a "Record at start" checkbox is shown on the same line (Raw uses
    // this; Display has its own enable above).
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
        ui.label("On exists")
            .on_hover_text("What to do when the destination file already exists.");
        ui.radio_value(overwrite_policy, OverwritePolicy::Refuse, "Refuse")
            .on_hover_text("Don't record — fail rather than touch the existing file.");
        ui.radio_value(overwrite_policy, OverwritePolicy::Overwrite, "Overwrite")
            .on_hover_text("Replace the existing file (its current contents are lost).");
        ui.radio_value(overwrite_policy, OverwritePolicy::AppendIfExists, "Append")
            .on_hover_text("Keep the existing file and add new data to the end.");
    });
    ui.horizontal(|ui| {
        ui.label("Rotate").on_hover_text(
            "Start a fresh file each time period instead of one continuously growing \
             file. When on, the destination is a folder and files are named \
             <channel>_<time period>.",
        );
        ui.radio_value(file_rotation, FileRotationPolicy::None, "None")
            .on_hover_text("One file that grows for the whole session.");
        ui.radio_value(file_rotation, FileRotationPolicy::Hourly, "Hourly")
            .on_hover_text(
                "A new file each hour, named <channel>_<date>_<hour> (e.g. \
                 GPS_2026-06-03_08) — keep the channel name filesystem-safe (§59).",
            );
        ui.radio_value(file_rotation, FileRotationPolicy::Daily, "Daily")
            .on_hover_text(
                "A new file each day, named <channel>_<date> (e.g. GPS_2026-06-03) — keep \
                 the channel name filesystem-safe (§59).",
            );
    });
    ui.horizontal(|ui| {
        if let Some(enabled) = record_at_start {
            ui.checkbox(enabled, "Record at start")
                .on_hover_text("Begin recording when the channel starts (§53).");
        }
        if let Some(timestamp_enabled) = timestamp_enabled {
            ui.checkbox(timestamp_enabled, format!("Record timestamps ({ext})"))
                .on_hover_text("Prefix each rendered line with its arrival time (§57).");
        }
    });
}

/// Edit the channel's **Raw** recording setup (§53): destination, overwrite, rotation,
/// and the "record at start" flag. The byte-exact verbatim stream (§53) — a separate
/// pipeline tap from Display (ADR-013). Config-driven: applied via a §13 Reconfigure.
/// The live Record toggle (ADR-012) begins/stops it at runtime without a restart, as
/// long as a destination is set. No timestamp option — a `.raw` file is the bytes
/// exactly as received, so the timestamp flag is forced off.
pub(crate) fn edit_raw_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.raw_recording;
    rec.timestamp_enabled = false; // Raw never timestamps; keep the config honest.
    recording_file_fields(
        ui,
        ".raw",
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
        None,                   // Raw: no "Record timestamps" option (verbatim bytes only).
        Some(&mut rec.enabled), // "Record at start"
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
        Some(&mut rec.timestamp_enabled), // Display can prefix rendered lines with time
        None,                             // Display has its own enable checkbox above
    );
}
