//! The shared base style: widget visuals for both themes plus the non-visual
//! style tweaks, so the two apps read as one product (talker ADR-016 /
//! listener ADR-019).
//!
//! The light theme is listener's shipped look (grey backdrop, heavier text,
//! raised buttons); the dark theme carries the same treatment over talker's
//! original dark values. Callers install both with [`install_visuals`] and
//! pick the active one via `ctx.set_theme(...)`.

/// Listener's light look: a grey backdrop with darker (heavier) text, visible
/// separators, and buttons that read as raised, interactive objects in every
/// state (not flat labels) — filled face + visible border + a little rounding,
/// brightening on hover and darkening on press. Disabled buttons use
/// `noninteractive` (flat/dim), so the enabled↔disabled distinction survives.
pub fn light_visuals() -> egui::Visuals {
    let mut light = egui::Visuals::light();
    light.override_text_color = Some(egui::Color32::from_gray(20));
    light.panel_fill = egui::Color32::from_gray(220);
    light.window_fill = egui::Color32::from_gray(220);
    // More visible dividers: `ui.separator()` draws with the noninteractive
    // bg_stroke, which defaults to a very faint grey — darken and thicken it.
    light.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.5_f32, egui::Color32::from_gray(120));
    let btn_border = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(150));
    let btn_round = egui::CornerRadius::same(4);
    light.widgets.inactive.weak_bg_fill = egui::Color32::from_gray(236);
    light.widgets.inactive.bg_fill = egui::Color32::from_gray(236);
    light.widgets.inactive.bg_stroke = btn_border;
    light.widgets.inactive.corner_radius = btn_round;
    light.widgets.hovered.weak_bg_fill = egui::Color32::from_gray(248);
    light.widgets.hovered.bg_fill = egui::Color32::from_gray(248);
    light.widgets.hovered.bg_stroke = egui::Stroke::new(1.2_f32, egui::Color32::from_gray(110));
    light.widgets.hovered.corner_radius = btn_round;
    light.widgets.active.weak_bg_fill = egui::Color32::from_gray(214);
    light.widgets.active.bg_fill = egui::Color32::from_gray(214);
    light.widgets.active.bg_stroke = egui::Stroke::new(1.2_f32, egui::Color32::from_gray(90));
    light.widgets.active.corner_radius = btn_round;
    light
}

/// The dark counterpart: talker's original dark values (light text, visible
/// separators) with the same raised-button treatment as the light theme.
pub fn dark_visuals() -> egui::Visuals {
    let fg = egui::Color32::from_gray(230);
    let mut dark = egui::Visuals::dark();
    dark.override_text_color = Some(fg);
    dark.widgets.noninteractive.fg_stroke.color = fg;
    dark.widgets.inactive.fg_stroke.color = fg;
    dark.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.5_f32, egui::Color32::from_gray(100));
    let btn_border = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(105));
    let btn_round = egui::CornerRadius::same(4);
    dark.widgets.inactive.weak_bg_fill = egui::Color32::from_gray(52);
    dark.widgets.inactive.bg_fill = egui::Color32::from_gray(52);
    dark.widgets.inactive.bg_stroke = btn_border;
    dark.widgets.inactive.corner_radius = btn_round;
    dark.widgets.hovered.weak_bg_fill = egui::Color32::from_gray(64);
    dark.widgets.hovered.bg_fill = egui::Color32::from_gray(64);
    dark.widgets.hovered.bg_stroke = egui::Stroke::new(1.2_f32, egui::Color32::from_gray(140));
    dark.widgets.hovered.corner_radius = btn_round;
    dark.widgets.active.weak_bg_fill = egui::Color32::from_gray(40);
    dark.widgets.active.bg_fill = egui::Color32::from_gray(40);
    dark.widgets.active.bg_stroke = egui::Stroke::new(1.2_f32, egui::Color32::from_gray(160));
    dark.widgets.active.corner_radius = btn_round;
    dark
}

/// Install both themes' visuals. The caller picks the active theme with
/// `ctx.set_theme(egui::ThemePreference::...)` — both apps ship a Light/Dark
/// toggle.
pub fn install_visuals(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Light, light_visuals());
    ctx.set_visuals_of(egui::Theme::Dark, dark_visuals());
}

/// The non-visual style tweaks both apps share, applied to every theme:
///
/// - Non-monospace text +0.5pt (the monospace data views keep their size).
/// - Popups/menus open instantly — egui's ~83 ms area fade made dropdowns feel
///   laggy; for a utility UI an instant snap reads as snappier.
/// - Collapsing-section triangles (and checkbox/radio glyphs) ~25% larger, so
///   they're easier to hit and read.
pub fn apply_style_tweaks(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        for font in style.text_styles.values_mut() {
            if font.family != egui::FontFamily::Monospace {
                font.size += 0.5;
            }
        }
        style.animation_time = 0.0;
        style.spacing.icon_width *= 1.25;
        style.spacing.icon_width_inner *= 1.25;
    });
}
