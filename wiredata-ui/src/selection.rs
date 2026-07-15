//! Shared selected-channel chrome (talker ADR-016 / listener ADR-019).

use crate::palette::{Palette, DARK, LIGHT};

const IDENTITY_TINT_ALPHA: u8 = 48;
const TAB_BRIDGE_OVERLAP: f32 = 2.0;
const TAB_JOIN_RADIUS: f32 = 6.0;
const TAB_ARC_STEPS: usize = 24;

fn active_palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// Stable channel identity used in both a list row and its detail band.
/// `position` is one-based. A fallback name is not repeated.
pub fn channel_title(position: usize, name: &str) -> String {
    let fallback = format!("Channel {position}");
    if name.is_empty() || name == fallback {
        fallback
    } else {
        format!("{fallback} · {name}")
    }
}

fn channel_card_frame(ui: &egui::Ui, selected: bool) -> egui::Frame {
    let mut frame = egui::Frame::group(ui.style())
        .inner_margin(8.0)
        .corner_radius(egui::CornerRadius::same(6))
        .stroke(egui::Stroke::new(1.5, active_palette(ui).box_stroke));
    if selected {
        // Match the detail page rather than filling with a selection color:
        // the open edge can then read as one continuous surface.
        frame.fill = ui.visuals().panel_fill;
        // The complete selected outline is painted later as one path by
        // `connect_tab_to_page`. Preserve the frame's stroke reservation so
        // selection cannot change row height, but make it fully transparent.
        frame.stroke = egui::Stroke::new(1.5, egui::Color32::TRANSPARENT);
        frame.corner_radius = egui::CornerRadius {
            nw: 6,
            ne: 0,
            sw: 6,
            se: 0,
        };
    }
    frame
}

fn detail_identity_frame(ui: &egui::Ui) -> egui::Frame {
    let selection = ui.visuals().selection;
    let accent = selection.bg_fill;
    let tint = egui::Color32::from_rgba_unmultiplied(
        accent.r(),
        accent.g(),
        accent.b(),
        IDENTITY_TINT_ALPHA,
    );
    let fill = ui.visuals().panel_fill.blend(tint);

    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 12,
            right: 10,
            top: 7,
            bottom: 7,
        })
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, selection.stroke.color))
        .corner_radius(egui::CornerRadius::same(4))
}

#[derive(Clone, Debug, PartialEq)]
struct TabJoinGeometry {
    bridge_fill: egui::Rect,
    divider_cover: egui::Rect,
    outline: Vec<egui::Pos2>,
}

fn append_arc(
    points: &mut Vec<egui::Pos2>,
    center: egui::Pos2,
    radius: f32,
    start_angle: f32,
    end_angle: f32,
    end_point: egui::Pos2,
) {
    for step in 1..=TAB_ARC_STEPS {
        let t = step as f32 / TAB_ARC_STEPS as f32;
        let angle = egui::lerp(start_angle..=end_angle, t);
        points.push(center + radius * egui::vec2(angle.cos(), angle.sin()));
    }
    // Trigonometric endpoints carry tiny floating-point residue. Snap the
    // final point so the following straight segment shares the exact vertex.
    if let Some(last) = points.last_mut() {
        *last = end_point;
    }
}

fn tab_join_geometry(panel_rect: egui::Rect, tab_rect: egui::Rect) -> Option<TabJoinGeometry> {
    let top = panel_rect.top().max(tab_rect.top());
    let bottom = panel_rect.bottom().min(tab_rect.bottom());
    if bottom <= top {
        return None;
    }

    let edge = panel_rect.right();
    let radius = TAB_JOIN_RADIUS
        .min((bottom - top) * 0.25)
        .min((panel_rect.right() - tab_rect.right()).max(0.0))
        .min(top - panel_rect.top())
        .min(panel_rect.bottom() - bottom)
        .max(0.0);
    let card_radius = TAB_JOIN_RADIUS
        .min((bottom - top) * 0.5)
        .min(tab_rect.width() * 0.5);

    // One path means one tessellation and no stroke cap or color handoff near
    // the tab. Coordinates use egui's downward Y.
    let mut outline = Vec::with_capacity(4 * TAB_ARC_STEPS + 9);
    outline.push(egui::pos2(edge, panel_rect.top()));
    outline.push(egui::pos2(edge, top - radius));
    append_arc(
        &mut outline,
        egui::pos2(edge - radius, top - radius),
        radius,
        0.0,
        std::f32::consts::FRAC_PI_2,
        egui::pos2(edge - radius, top),
    );
    outline.push(egui::pos2(tab_rect.left() + card_radius, top));
    append_arc(
        &mut outline,
        egui::pos2(tab_rect.left() + card_radius, top + card_radius),
        card_radius,
        -std::f32::consts::FRAC_PI_2,
        -std::f32::consts::PI,
        egui::pos2(tab_rect.left(), top + card_radius),
    );
    outline.push(egui::pos2(tab_rect.left(), bottom - card_radius));
    append_arc(
        &mut outline,
        egui::pos2(tab_rect.left() + card_radius, bottom - card_radius),
        card_radius,
        std::f32::consts::PI,
        std::f32::consts::FRAC_PI_2,
        egui::pos2(tab_rect.left() + card_radius, bottom),
    );
    outline.push(egui::pos2(edge - radius, bottom));
    append_arc(
        &mut outline,
        egui::pos2(edge - radius, bottom + radius),
        radius,
        -std::f32::consts::FRAC_PI_2,
        0.0,
        egui::pos2(edge, bottom + radius),
    );
    outline.push(egui::pos2(edge, panel_rect.bottom()));

    Some(TabJoinGeometry {
        bridge_fill: egui::Rect::from_min_max(
            egui::pos2(tab_rect.right() - TAB_BRIDGE_OVERLAP, top),
            egui::pos2(edge + TAB_BRIDGE_OVERLAP, bottom),
        ),
        divider_cover: egui::Rect::from_min_max(
            egui::pos2(edge - TAB_BRIDGE_OVERLAP, panel_rect.top()),
            egui::pos2(edge + TAB_BRIDGE_OVERLAP, panel_rect.bottom()),
        ),
        outline,
    })
}

fn tab_outline_stroke(ui: &egui::Ui) -> egui::Stroke {
    ui.visuals().widgets.noninteractive.bg_stroke
}

/// Draw one channel-list card. The selected card uses the detail page's fill;
/// [`connect_tab_to_page`] removes its right edge after both panels render.
pub fn channel_card<R>(
    ui: &mut egui::Ui,
    selected: bool,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    channel_card_frame(ui, selected).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add_contents(ui)
    })
}

/// Full-width selected-channel identity band. Callers keep this band outside
/// any page scroll area and provide their app-specific identity/status contents.
pub fn detail_identity<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    detail_identity_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add_contents(ui)
    })
}

/// Open a selected tab into the detail page, like a worksheet tab. Call this
/// after rendering both panels so the page-colored bridge covers the tab's
/// right stroke, the panel gutter, and the separator line. The top and bottom
/// strokes turn through a radius into the page edge instead of overshooting it.
/// The original divider is erased in full and replaced by one page-edge path
/// from panel top to bottom, so there is no stroke or color handoff.
pub fn connect_tab_to_page(
    ui: &egui::Ui,
    channel_panel_rect: egui::Rect,
    selected_tab_rect: Option<egui::Rect>,
) {
    let Some(tab_rect) = selected_tab_rect else {
        return;
    };
    let Some(join) = tab_join_geometry(channel_panel_rect, tab_rect) else {
        return;
    };

    let painter = ui.painter();
    let fill = ui.visuals().panel_fill;
    painter.rect_filled(join.bridge_fill, egui::CornerRadius::ZERO, fill);
    painter.rect_filled(join.divider_cover, egui::CornerRadius::ZERO, fill);
    painter.line(join.outline, tab_outline_stroke(ui));
}

/// Compact collapsed-list tab. Selection uses the page fill and leaves its
/// visible outline to [`connect_tab_to_page`], exactly like a full card.
pub fn mini_tab<'a>(
    ui: &mut egui::Ui,
    selected: bool,
    contents: impl egui::IntoAtoms<'a>,
) -> egui::Response {
    let corner_radius = if selected {
        egui::CornerRadius {
            nw: 6,
            ne: 0,
            sw: 6,
            se: 0,
        }
    } else {
        egui::CornerRadius::same(4)
    };
    let mut button = egui::Button::selectable(selected, contents)
        .min_size(egui::vec2(28.0, 24.0))
        .corner_radius(corner_radius);
    if selected {
        button = button
            .fill(ui.visuals().panel_fill)
            .stroke(egui::Stroke::NONE);
    }
    ui.add(button)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_title_keeps_position_and_avoids_duplicate_fallback() {
        assert_eq!(channel_title(3, ""), "Channel 3");
        assert_eq!(channel_title(3, "Channel 3"), "Channel 3");
        assert_eq!(channel_title(3, "GPS"), "Channel 3 · GPS");
    }

    #[test]
    fn tab_join_curves_into_the_page_edge_without_overshooting() {
        let panel = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(300.0, 400.0));
        let tab = egui::Rect::from_min_max(egui::pos2(8.0, 100.0), egui::pos2(292.0, 150.0));

        let join = tab_join_geometry(panel, tab).expect("tab overlaps its panel");
        assert_eq!(join.bridge_fill.min, egui::pos2(290.0, 100.0));
        assert_eq!(join.bridge_fill.max, egui::pos2(302.0, 150.0));
        assert_eq!(join.divider_cover.min, egui::pos2(298.0, 0.0));
        assert_eq!(join.divider_cover.max, egui::pos2(302.0, 400.0));
        assert_eq!(join.outline.first(), Some(&egui::pos2(300.0, 0.0)));
        assert_eq!(join.outline[1], egui::pos2(300.0, 94.0));
        assert_eq!(join.outline[25], egui::pos2(294.0, 100.0));
        assert_eq!(join.outline[26], egui::pos2(14.0, 100.0));
        assert_eq!(join.outline[50], egui::pos2(8.0, 106.0));
        assert_eq!(join.outline[51], egui::pos2(8.0, 144.0));
        assert_eq!(join.outline[75], egui::pos2(14.0, 150.0));
        assert_eq!(join.outline[76], egui::pos2(294.0, 150.0));
        assert_eq!(join.outline[100], egui::pos2(300.0, 156.0));
        assert_eq!(join.outline.last(), Some(&egui::pos2(300.0, 400.0)));
        assert!(join.outline.iter().all(|point| point.x <= panel.right()));
        assert!(join.outline.windows(2).all(|pair| pair[0] != pair[1]));

        let below = tab.translate(egui::vec2(0.0, 400.0));
        assert_eq!(tab_join_geometry(panel, below), None);
    }

    #[test]
    fn selected_card_page_edge_and_identity_each_have_one_owned_stroke() {
        egui::__run_test_ui(|ui| {
            for visuals in [egui::Visuals::light(), egui::Visuals::dark()] {
                *ui.visuals_mut() = visuals;

                let ordinary = channel_card_frame(ui, false);
                assert_eq!(ordinary.fill, egui::Color32::TRANSPARENT);
                assert_eq!(ordinary.stroke.color, active_palette(ui).box_stroke);

                let selected = channel_card_frame(ui, true);
                assert_eq!(selected.fill, ui.visuals().panel_fill);
                assert_eq!(selected.stroke.width, 1.5);
                assert_eq!(selected.stroke.color, egui::Color32::TRANSPARENT);
                assert_eq!(selected.corner_radius.ne, 0);
                assert_eq!(selected.corner_radius.se, 0);

                let outline = tab_outline_stroke(ui);
                assert_eq!(outline, ui.visuals().widgets.noninteractive.bg_stroke);

                let identity = detail_identity_frame(ui);
                assert_ne!(identity.fill, ui.visuals().panel_fill);
                assert_ne!(identity.fill, ui.visuals().selection.bg_fill);
                assert_eq!(identity.stroke.color, ui.visuals().selection.stroke.color);
            }
        });
    }

    #[test]
    fn selection_does_not_change_channel_card_geometry() {
        egui::__run_test_ui(|ui| {
            ui.set_width(320.0);
            let ordinary = channel_card(ui, false, |ui| ui.label("Channel"));
            let selected = channel_card(ui, true, |ui| ui.label("Channel"));

            assert_eq!(ordinary.response.rect.size(), selected.response.rect.size());

            let ordinary_mini = mini_tab(ui, false, "Channel");
            let selected_mini = mini_tab(ui, true, "Channel");
            assert_eq!(ordinary_mini.rect.size(), selected_mini.rect.size());
        });
    }
}
