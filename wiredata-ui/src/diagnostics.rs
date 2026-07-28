//! Shared presentation primitives for compact, decision-oriented diagnostics.
//!
//! The applications decide what each signal means and when it deserves attention;
//! this module owns only the identical egui chrome: a quiet card, status badge,
//! aligned signal rows, and a restrained attention callout.
//!
//! # Shared readout vocabulary
//!
//! A technician may read both applications' panels in one session, so a
//! measurement in the same *state* should be described the same way in both. The
//! domain noun differs — Talker counts deadlines and sends, Listener counts
//! chunks and firings — but the state names and the notation do not:
//!
//! | State | When | Reads like |
//! |---|---|---|
//! | Awaiting first sample | nothing measured yet this run | `awaiting first <noun>` |
//! | Warming up | fewer than 20 samples in the recent window | `warm-up (N) · max X` |
//! | Live | at least 20 samples in the recent window | `p99 ≤ X` |
//! | No recent samples | the run has samples, the recent window does not | `no recent <noun>s · run max X` |
//! | Expired | Talker only — see below | `recent snapshot expired · as of X ago` |
//! | Final | Talker only — the exact-at-stop snapshot | `final snapshot` |
//!
//! Conventions that go with it:
//!
//! - **`≤`, not `<=`.** Bucketed histograms yield an upper bound, never an exact
//!   percentile, and the glyph is what says so.
//! - **Two windows, named.** A rolling window (`~last 10 s` while running,
//!   `~final 10 s` once stopped) sits beside a cumulative `run max`. Any readout
//!   showing both must label which is which.
//! - **20 samples** is the warm-up gate in both applications. Below it, show the
//!   observed maximum rather than a percentile that cannot yet mean anything.
//! - **No invented health thresholds.** Neither application scores latency
//!   against a budget it does not have; escalation comes from evidence that
//!   something is actually wrong (a confirmed drop, a queue at half capacity),
//!   not from a timing number being large.
//!
//! *Expired* and *Final* are Talker-only, and deliberately so: Talker pushes
//! collapsed snapshots that age between emissions, while Listener collapses each
//! window when a request is served and so cannot serve a stale one. That
//! asymmetry follows from the two runtime models and is recorded in talker
//! ADR-043 and listener ADR-035 — it is not a gap to be filled on either side.

use std::hash::Hash;

use egui::{Color32, Response, RichText, Ui, WidgetText};

use crate::{fonts::bold, palette::active};

/// Visual emphasis for a diagnostic signal. Semantics and thresholds remain local
/// to the application that owns the measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SignalTone {
    /// Informational or not yet measured.
    #[default]
    Neutral,
    /// The signal confirms the requested work is proceeding normally.
    Healthy,
    /// The signal needs attention but does not prove a hard failure or loss.
    Warning,
    /// The signal proves a failed operation, impossible demand, or data loss.
    Fault,
}

impl SignalTone {
    fn accent(self, ui: &Ui) -> Color32 {
        let palette = active(ui);
        match self {
            Self::Neutral => palette.idle_grey,
            Self::Healthy => palette.running_green,
            Self::Warning => palette.warning_amber,
            Self::Fault => palette.fault_red,
        }
    }

    fn text_color(self, ui: &Ui) -> Color32 {
        match self {
            // The shared running green is deliberately bright because it was
            // chosen for status glyphs. On the light card it is too pale for
            // small text, so calm states use the theme's foreground color and
            // reserve green for the badge border/fill.
            Self::Neutral | Self::Healthy => ui.visuals().text_color(),
            Self::Warning | Self::Fault => self.accent(ui),
        }
    }
}

fn translucent(accent: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha)
}

fn card_frame(ui: &Ui) -> egui::Frame {
    let visuals = ui.visuals();
    let neutral = visuals.widgets.noninteractive.bg_stroke.color;
    let stroke = visuals.panel_fill.blend(translucent(neutral, 150));
    let fill = visuals
        .panel_fill
        .blend(translucent(visuals.widgets.noninteractive.weak_bg_fill, 90));
    egui::Frame::group(ui.style())
        .fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, stroke))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(11))
}

fn status_badge(ui: &mut Ui, text: WidgetText, tone: SignalTone) -> Response {
    let accent = tone.accent(ui);
    let text_color = tone.text_color(ui);
    let fill = ui.visuals().panel_fill.blend(translucent(accent, 38));
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, translucent(accent, 145)))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text.text())
                    .small()
                    .strong()
                    .color(text_color),
            )
        })
        .inner
}

/// Draw one compact diagnostic card. The caller supplies app-specific rows and any
/// progressive-disclosure control beneath them.
pub fn decision_card<R>(
    ui: &mut Ui,
    title: impl Into<WidgetText>,
    status: impl Into<WidgetText>,
    tone: SignalTone,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    let title = title.into();
    let status = status.into();
    card_frame(ui).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(bold(title.text()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                status_badge(ui, status, tone);
            });
        });
        ui.add_space(7.0);
        add_contents(ui)
    })
}

/// Draw a two-column signal grid inside [`decision_card`]. Call
/// [`signal_row`] from `add_rows` for each decision-level signal.
pub fn signal_grid<R>(
    ui: &mut Ui,
    id_source: impl Hash,
    add_rows: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Grid::new(id_source)
        .num_columns(2)
        .spacing(egui::vec2(14.0, 5.0))
        .show(ui, add_rows)
}

/// Draw one aligned decision signal. Warning and fault tones color only the value;
/// calm values use the normal foreground so labels keep a stable hierarchy, healthy
/// text remains legible in the light theme, and the card does not become a wall of
/// status colors.
pub fn signal_row(
    ui: &mut Ui,
    label: impl Into<WidgetText>,
    value: impl Into<WidgetText>,
    tone: SignalTone,
    tooltip: impl Into<WidgetText>,
) {
    ui.label(RichText::new(label.into().text()).strong());
    let text_color = tone.text_color(ui);
    ui.add(
        egui::Label::new(RichText::new(value.into().text()).color(text_color))
            .wrap()
            .sense(egui::Sense::hover()),
    )
    .on_hover_text(tooltip);
    ui.end_row();
}

/// Draw an exceptional condition beneath the summary rows. Healthy information
/// belongs in the grid; this callout is deliberately reserved for warning/fault
/// conditions so it remains noticeable.
pub fn attention_callout(
    ui: &mut Ui,
    id_source: impl Hash,
    text: impl Into<WidgetText>,
    tone: SignalTone,
    tooltip: impl Into<WidgetText>,
) -> Response {
    debug_assert!(matches!(tone, SignalTone::Warning | SignalTone::Fault));
    let accent = tone.accent(ui);
    let fill = ui.visuals().panel_fill.blend(translucent(accent, 24));
    let response = ui
        .push_id(id_source, |ui| {
            egui::Frame::new()
                .fill(fill)
                .stroke(egui::Stroke::new(1.0_f32, translucent(accent, 115)))
                .corner_radius(egui::CornerRadius::same(5))
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("\u{26A0} {}", text.into().text())).color(accent),
                        )
                        .wrap(),
                    )
                })
                .inner
        })
        .inner;
    response.on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_chrome_is_rounded_and_quiet() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let frame = card_frame(ui);
            assert_eq!(frame.corner_radius, egui::CornerRadius::same(8));
            assert_eq!(frame.inner_margin, egui::Margin::same(11));
            assert_eq!(frame.stroke.width, 1.0);
            assert_ne!(frame.fill, SignalTone::Warning.accent(ui));
        });
    }

    #[test]
    fn every_semantic_tone_has_a_distinct_theme_color() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let colors = [
                    SignalTone::Neutral.accent(ui),
                    SignalTone::Healthy.accent(ui),
                    SignalTone::Warning.accent(ui),
                    SignalTone::Fault.accent(ui),
                ];
                for (index, color) in colors.iter().enumerate() {
                    assert!(!colors[..index].contains(color));
                }
            });
        }
    }

    #[test]
    fn calm_text_uses_the_theme_foreground_in_both_themes() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert_eq!(
                    SignalTone::Neutral.text_color(ui),
                    ui.visuals().text_color()
                );
                assert_eq!(
                    SignalTone::Healthy.text_color(ui),
                    ui.visuals().text_color()
                );
                assert_eq!(
                    SignalTone::Warning.text_color(ui),
                    SignalTone::Warning.accent(ui)
                );
                assert_eq!(
                    SignalTone::Fault.text_color(ui),
                    SignalTone::Fault.accent(ui)
                );
            });
        }
    }
}
