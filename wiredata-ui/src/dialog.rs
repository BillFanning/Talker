//! Shared modal dialogs.
//!
//! A dialog is chrome by the [crate] rule: purely presentational, identical in
//! both applications, and egui-only. What the confirmed action *does* is not —
//! talker queues a deferred mutation, listener sends a runtime command — so
//! these render and report, and the caller acts on the answer.
//!
//! The wording lives here too, not just the layout. The two apps' remove
//! dialogs were already the same shape and had still drifted apart into two
//! different sentences; a shared sentence is the part that stops that
//! recurring.

use egui::WidgetText;

use crate::palette::active;

/// What the user did with a confirmation modal this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    /// Still open — the caller keeps its "dialog is showing" state.
    Pending,
    /// Dismissed without acting: Cancel, the backdrop, or Escape.
    Cancelled,
    /// The destructive button was pressed.
    Confirmed,
}

/// A confirmation modal for a destructive action.
///
/// The confirming button carries the fault colour from the *active* theme, so
/// it reads the same in light and dark; the cancel path is the plain button, the
/// backdrop, or Escape.
pub fn confirm_destructive(
    ctx: &egui::Context,
    id_source: &str,
    heading: &str,
    body: impl Into<WidgetText>,
    confirm_label: &str,
) -> Confirm {
    let mut outcome = Confirm::Pending;
    let response = egui::Modal::new(egui::Id::new(id_source)).show(ctx, |ui| {
        ui.set_width(320.0);
        ui.heading(heading);
        ui.add_space(4.0);
        ui.label(body);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                outcome = Confirm::Cancelled;
            }
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(confirm_label).color(egui::Color32::WHITE),
                    )
                    .fill(active(ui).fault_red),
                )
                .clicked()
            {
                outcome = Confirm::Confirmed;
            }
        });
    });
    // Backdrop click and Escape are cancellations, not confirmations.
    if outcome == Confirm::Pending && response.should_close() {
        outcome = Confirm::Cancelled;
    }
    outcome
}

/// The shared "remove this channel?" confirmation, wording included.
pub fn confirm_remove_channel(ctx: &egui::Context, name: &str) -> Confirm {
    confirm_destructive(
        ctx,
        "remove_confirm",
        "Remove channel?",
        format!(
            "\u{201C}{name}\u{201D} will be stopped and removed.\nUnsaved profile changes to \
             this channel will be lost."
        ),
        "Remove",
    )
}
