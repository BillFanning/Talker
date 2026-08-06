//! Shared selected-channel chrome (talker ADR-016 / listener ADR-019).

use crate::palette::active as active_palette;

const TAB_BRIDGE_OVERLAP: f32 = 2.0;
const TAB_JOIN_RADIUS: f32 = 6.0;
const TAB_CLIP_TOLERANCE: f32 = 0.5;
/// Vertical breathing room a channel list leaves around tabs so the selected
/// outline can turn into the page edge without touching adjacent content. This
/// deliberately exceeds the curve radius so pixel rounding cannot clip the
/// first or last card out of connector eligibility.
pub const TAB_JOIN_MARGIN: f32 = TAB_JOIN_RADIUS + 2.0;

/// Width of the channel list's Profile menu, in both apps.
///
/// Pinned rather than content-sized: without it each menu takes the width of
/// whichever recent-file name happens to be longest, so the two apps' menus
/// differ from each other and jump about as the recents list changes.
pub const PROFILE_MENU_WIDTH: f32 = 200.0;
const TAB_ARC_STEPS: usize = 24;

/// Stable channel identity used in full and compact channel-list rows.
/// `position` is one-based. A fallback name is not repeated.
pub fn channel_title(position: usize, name: &str) -> String {
    let fallback = format!("Channel {position}");
    if name.is_empty() || name == fallback {
        fallback
    } else {
        format!("{fallback} · {name}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelRowEmphasis {
    pub name: egui::Color32,
    pub info: egui::Color32,
    pub warning: egui::Color32,
    pub error: egui::Color32,
}

/// Shared selected-row hierarchy for both apps. Historical warning/error
/// counts recede on background tabs; a live fault remains app-owned and keeps
/// its saturated fault line regardless of selection.
pub fn channel_row_emphasis(ui: &egui::Ui, selected: bool) -> ChannelRowEmphasis {
    let palette = active_palette(ui);
    // A background row's counts recede to the theme's own faded text rather
    // than to a palette grey of their own: "receded" is emphasis, which the
    // theme already defines, and the counts name themselves ("3 warn") so the
    // colour is never what tells them apart.
    let receded = ui.visuals().weak_text_color();
    ChannelRowEmphasis {
        name: if selected {
            ui.visuals().text_color()
        } else {
            receded
        },
        info: receded,
        warning: if selected { palette.warning } else { receded },
        error: if selected { palette.fault } else { receded },
    }
}

/// A channel row's per-severity counts line, in info · warn · err order with
/// the row's [`ChannelRowEmphasis`] colors (historical counts recede on
/// background tabs). An optional hover tip applies to all three labels.
pub fn severity_counts_line(
    ui: &mut egui::Ui,
    info: u64,
    warnings: u64,
    errors: u64,
    emphasis: &ChannelRowEmphasis,
    hover: Option<&str>,
) {
    ui.horizontal(|ui| {
        let show = |ui: &mut egui::Ui, text: egui::RichText| {
            let response = ui.label(text);
            if let Some(tip) = hover {
                response.on_hover_text(tip);
            }
        };
        show(
            ui,
            egui::RichText::new(format!("{info} info"))
                .weak()
                .color(emphasis.info),
        );
        show(
            ui,
            egui::RichText::new(format!("{warnings} warn")).color(emphasis.warning),
        );
        show(
            ui,
            egui::RichText::new(format!("{errors} err")).color(emphasis.error),
        );
    });
}

/// A channel row's live-fault line: small, saturated fault red on **every**
/// row (selection never dims an active fault), wrapped so the full text reads
/// on the row itself.
pub fn last_error_line(ui: &mut egui::Ui, error: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(format!("\u{26A0} {error}"))
                .small()
                .color(active_palette(ui).fault),
        )
        .wrap(),
    );
}

/// The receded border for an unselected card: the app's **neutral** divider
/// grey (`noninteractive.bg_stroke`) blended well into the panel, so background
/// cards read as quiet, colorless hairlines that sit below the selected tab's
/// outline. The palette box stroke is blue-tinted and read as *color* on
/// unselected cards, which fought the selection. Width stays 1.5 so selection
/// cannot change a row's height.
fn unselected_card_stroke(ui: &egui::Ui) -> egui::Stroke {
    let neutral = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let faint = ui
        .visuals()
        .panel_fill
        .blend(egui::Color32::from_rgba_unmultiplied(
            neutral.r(),
            neutral.g(),
            neutral.b(),
            110,
        ));
    egui::Stroke::new(1.5_f32, faint)
}

fn channel_card_frame(ui: &egui::Ui, selected: bool) -> egui::Frame {
    let mut frame = egui::Frame::group(ui.style())
        .inner_margin(8.0)
        .corner_radius(egui::CornerRadius::same(6))
        .stroke(unselected_card_stroke(ui));
    if selected {
        // Match the detail page rather than filling with a selection color:
        // the open edge can then read as one continuous surface.
        frame.fill = ui.visuals().panel_fill;
        // The complete selected outline is painted later as one path by
        // `connect_tab_to_page`. Preserve the frame's stroke reservation so
        // selection cannot change row height, but make it fully transparent.
        frame.stroke = egui::Stroke::new(1.5_f32, egui::Color32::TRANSPARENT);
        frame.corner_radius = egui::CornerRadius {
            nw: 6,
            ne: 0,
            sw: 6,
            se: 0,
        };
    }
    frame
}

#[derive(Clone, Debug, PartialEq)]
struct TabJoinGeometry {
    bridge_fill: egui::Rect,
    outline: Vec<egui::Pos2>,
}

/// A selected tab and the viewport that is allowed to expose it. The connector
/// falls back to a straight page edge unless the whole detour, including its
/// turn radius, fits inside this clip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectedTab {
    rect: egui::Rect,
    clip_rect: egui::Rect,
}

impl SelectedTab {
    pub fn new(rect: egui::Rect, clip_rect: egui::Rect) -> Self {
        Self { rect, clip_rect }
    }

    fn can_join(self) -> bool {
        self.rect.intersects(self.clip_rect)
            && self.rect.left() >= self.clip_rect.left() - TAB_CLIP_TOLERANCE
            && self.rect.right() <= self.clip_rect.right() + TAB_CLIP_TOLERANCE
            && self.rect.top() - TAB_JOIN_RADIUS >= self.clip_rect.top() - TAB_CLIP_TOLERANCE
            && self.rect.bottom() + TAB_JOIN_RADIUS <= self.clip_rect.bottom() + TAB_CLIP_TOLERANCE
    }
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
        outline,
    })
}

fn tab_outline_stroke(ui: &egui::Ui, resizable_panel_id: Option<egui::Id>) -> egui::Stroke {
    // `"__resize"` mirrors egui's INTERNAL id for a SidePanel's resize
    // interaction (egui::containers::panel — not public API). If an egui
    // upgrade renames it, `read_response` returns None and this degrades
    // gracefully to the noninteractive stroke — the connector stays correct
    // but stops brightening on resize hover/drag. Re-check this string on
    // every egui bump (upgrade checklist).
    let resize =
        resizable_panel_id.and_then(|panel_id| ui.ctx().read_response(panel_id.with("__resize")));
    if resize.as_ref().is_some_and(egui::Response::dragged) {
        ui.visuals().widgets.active.fg_stroke
    } else if resize.as_ref().is_some_and(egui::Response::hovered) {
        ui.visuals().widgets.hovered.fg_stroke
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    }
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

/// Open a selected tab into the detail page, like a worksheet tab. Call this
/// after rendering both panels so the page-colored bridge covers the tab's
/// right stroke, the panel gutter, and the separator line. The top and bottom
/// strokes turn through a radius into the page edge instead of overshooting it.
/// The original divider is erased in full and replaced by one page-edge path
/// from panel top to bottom, so there is no stroke or color handoff. A clipped
/// tab gets a straight divider instead of exposing a partial connector.
pub fn connect_tab_to_page(
    ui: &egui::Ui,
    channel_panel_rect: egui::Rect,
    selected_tab: Option<SelectedTab>,
    resizable_panel_id: Option<egui::Id>,
) {
    let painter = ui.painter();
    let fill = ui.visuals().panel_fill;
    let stroke = tab_outline_stroke(ui, resizable_panel_id);
    let divider_cover = egui::Rect::from_min_max(
        egui::pos2(
            channel_panel_rect.right() - TAB_BRIDGE_OVERLAP,
            channel_panel_rect.top(),
        ),
        egui::pos2(
            channel_panel_rect.right() + TAB_BRIDGE_OVERLAP,
            channel_panel_rect.bottom(),
        ),
    );
    painter.rect_filled(divider_cover, egui::CornerRadius::ZERO, fill);

    let join = selected_tab
        .filter(|tab| tab.can_join())
        .and_then(|tab| tab_join_geometry(channel_panel_rect, tab.rect));
    if let Some(join) = join {
        painter.rect_filled(join.bridge_fill, egui::CornerRadius::ZERO, fill);
        painter.line(join.outline, stroke);
    } else {
        painter.vline(
            channel_panel_rect.right(),
            channel_panel_rect.y_range(),
            stroke,
        );
    }
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
    fn selected_card_and_page_edge_each_have_one_owned_stroke() {
        egui::__run_test_ui(|ui| {
            for visuals in [egui::Visuals::light(), egui::Visuals::dark()] {
                *ui.visuals_mut() = visuals;

                let ordinary = channel_card_frame(ui, false);
                assert_eq!(ordinary.fill, egui::Color32::TRANSPARENT);
                // Unselected cards recede to a faint neutral hairline, fainter
                // than (and colorless next to) the selected tab's outline grey,
                // but keep the 1.5 width so selection can't change row height.
                assert_eq!(ordinary.stroke, unselected_card_stroke(ui));
                assert_eq!(ordinary.stroke.width, 1.5);
                assert_ne!(
                    ordinary.stroke.color,
                    ui.visuals().widgets.noninteractive.bg_stroke.color
                );

                let selected = channel_card_frame(ui, true);
                assert_eq!(selected.fill, ui.visuals().panel_fill);
                assert_eq!(selected.stroke.width, 1.5);
                assert_eq!(selected.stroke.color, egui::Color32::TRANSPARENT);
                assert_eq!(selected.corner_radius.ne, 0);
                assert_eq!(selected.corner_radius.se, 0);

                let outline = tab_outline_stroke(ui, None);
                assert_eq!(outline, ui.visuals().widgets.noninteractive.bg_stroke);
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

    #[test]
    fn connector_requires_the_tab_and_turns_to_fit_the_viewport() {
        let viewport = egui::Rect::from_min_max(egui::pos2(0.0, 20.0), egui::pos2(300.0, 380.0));
        let visible = SelectedTab::new(
            egui::Rect::from_min_max(egui::pos2(8.0, 100.0), egui::pos2(292.0, 150.0)),
            viewport,
        );
        assert!(visible.can_join());

        let rounded_to_boundary = SelectedTab::new(
            egui::Rect::from_min_max(
                egui::pos2(8.0, viewport.top() + TAB_JOIN_RADIUS - 0.25),
                egui::pos2(292.0, 90.0),
            ),
            viewport,
        );
        assert!(rounded_to_boundary.can_join());

        let clipped_top = SelectedTab::new(
            egui::Rect::from_min_max(egui::pos2(8.0, 22.0), egui::pos2(292.0, 72.0)),
            viewport,
        );
        assert!(!clipped_top.can_join());

        let clipped_right = SelectedTab::new(
            egui::Rect::from_min_max(egui::pos2(8.0, 100.0), egui::pos2(302.0, 150.0)),
            viewport,
        );
        assert!(!clipped_right.can_join());
    }

    #[test]
    fn first_scroll_card_has_room_for_the_page_edge_turns() {
        egui::__run_test_ui(|ui| {
            ui.set_width(320.0);
            let mut first = None;
            let output = egui::ScrollArea::vertical()
                .max_height(160.0)
                .show(ui, |ui| {
                    ui.add_space(TAB_JOIN_MARGIN);
                    for index in 0..8 {
                        let card = channel_card(ui, index == 0, |ui| {
                            ui.label(format!("Channel {}", index + 1));
                            ui.label("Connection details");
                        });
                        if index == 0 {
                            first = Some(card.response.rect);
                        }
                        ui.add_space(TAB_JOIN_MARGIN);
                    }
                });

            let first = first.expect("the first card was rendered");
            let selected = SelectedTab::new(first, output.inner_rect);
            assert!(
                first.top() - output.inner_rect.top() > TAB_JOIN_RADIUS,
                "first card needs rounding clearance above its turn"
            );
            assert!(
                selected.can_join(),
                "first card {first:?} did not fit viewport {:?}",
                output.inner_rect
            );
        });
    }

    #[test]
    fn row_emphasis_recedes_only_background_identity_and_history() {
        egui::__run_test_ui(|ui| {
            for visuals in [egui::Visuals::light(), egui::Visuals::dark()] {
                *ui.visuals_mut() = visuals;
                let selected = channel_row_emphasis(ui, true);
                let background = channel_row_emphasis(ui, false);

                assert_eq!(selected.name, ui.visuals().text_color());
                assert_eq!(background.name, ui.visuals().weak_text_color());
                assert_eq!(selected.info, background.info);
                assert_ne!(selected.warning, background.warning);
                assert_ne!(selected.error, background.error);
                assert_eq!(background.warning, background.info);
                assert_eq!(background.error, background.info);
            }
        });
    }
}
