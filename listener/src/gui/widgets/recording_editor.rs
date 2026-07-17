//! The Raw and Display recording setup editors (§53–§59): destination, overwrite
//! policy, rotation. Config-driven (applied via a §13 Reconfigure); the live Record
//! toggle (ADR-012) is separate. Both editors share `recording_file_fields`.

use std::path::PathBuf;

use crate::config::ChannelConfig;
use crate::record::{effective_overwrite, FileRotationPolicy, OverwritePolicy};

use wiredata_ui::palette::active as palette;

/// The shared destination / overwrite / rotation controls for a recording (§55–§59),
/// used by both the Raw and Display editors (their config structs carry the same
/// fields; ADR-013).
fn recording_file_fields(
    ui: &mut egui::Ui,
    destination: &mut Option<PathBuf>,
    overwrite_policy: &mut OverwritePolicy,
    file_rotation: &mut FileRotationPolicy,
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
            // The native picker is modal and synchronous, so the egui UI (and the live
            // view) freeze while it's open — the same intentional exception to "the UI
            // thread never blocks" (AGENTS §5) as the profile picker. Acceptable: it's a
            // brief user-driven modal, and *reception* never stops (it runs on the
            // runtime thread); only the on-screen view pauses until the dialog closes.
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
                .color(palette(ui).warning_amber),
        );
    }
    // With rotation on, each period gets a fresh file, so "Refuse" makes no
    // sense — disable it and move a Refuse selection to Append, via the same
    // shared §59 rule the runtime settings builders apply.
    *overwrite_policy = effective_overwrite(*overwrite_policy, *file_rotation);
    ui.horizontal(|ui| {
        ui.label("On exists")
            .on_hover_text("What to do when the destination file already exists.");
        ui.add_enabled_ui(!rotating, |ui| {
            ui.radio_value(overwrite_policy, OverwritePolicy::Refuse, "Refuse")
                .on_hover_text(if rotating {
                    "Not available with rotation — each period starts a fresh file."
                } else {
                    "Don't record — fail rather than touch the existing file."
                });
        });
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
}

/// Edit the channel's **Raw** recording setup (§53): destination, overwrite, rotation.
/// The byte-exact verbatim stream (§53) — a separate pipeline tap from Display
/// (ADR-013). The live Record toggle and the "Record on start" flag (`enabled`, ADR-012)
/// live next to the Record button, not here.
pub(crate) fn edit_raw_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.raw_recording;
    recording_file_fields(
        ui,
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
    );
}

/// Edit the channel's **Display** recording setup (§54): destination, overwrite,
/// rotation — the same fields as Raw (ADR-013), records the rendered view output
/// (`.disp`). The "Record on start" flag (`enabled`) lives on the block's header
/// row, like Raw's. Config-driven: applied via a §13 Reconfigure.
pub(crate) fn edit_display_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.display_recording;
    recording_file_fields(
        ui,
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
    );
}
