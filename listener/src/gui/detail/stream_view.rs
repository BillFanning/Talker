//! The stream viewer: the toolbar (Pause/Resume, View mode, ctrl-chars, font/
//! colors/mono) above a virtualized, soft-wrapped byte view, plus its row cache and
//! scrollbar styling. This is the densest layout in the detail pane, so it lives on
//! its own — see the long comments inside for the layout invariants (row-pitch vs
//! `show_rows`, scrollbar handle visibility, viewer height/stick-to-bottom).

use crate::core::ChannelId;
use crate::display::{
    AnnotationPlacement, CharacterRendering, DisplayEncoding, DisplayMode, DisplayView,
    RenderAnnotation, WrappingMode,
};

use super::super::bridge::UiCommand;
use super::super::fonts::{bold, MonoFont};
use super::super::view_prefs::{
    scroll_buffer_label, ViewPrefs, MAX_SCROLL_BUFFER_BYTES, MIN_SCROLL_BUFFER_BYTES,
    SCROLL_BUFFER_PRESETS_KB,
};
use super::super::widgets::{human_bytes, ColorScheme, MSG_FONT_SIZES};
use super::super::ListenerApp;

impl ListenerApp {
    /// The stream viewer (§41): the toolbar (Pause/Resume, View mode, ctrl-chars,
    /// font/colors/mono) above a virtualized, soft-wrapped byte view. Split out of
    /// `show_detail` — this is the densest layout in the pane (scrollbar styling,
    /// selectable-label visuals, row-pitch/virtualization), so it lives on its own.
    pub(super) fn show_stream_view(&mut self, ui: &mut egui::Ui, id: ChannelId) {
        // Per-channel view settings (mode, ctrl-chars, font, colors) live on the
        // channel's ViewPrefs — each channel renders independently. Edit a clone, then
        // (if it changed) write it back, fold it into the channel's config, and tell the
        // runtime so a profile save captures it (SetViewConfig — no restart, §78).
        let Some(mut prefs) = self.state.channel(id).map(|v| v.view_prefs.clone()) else {
            return;
        };

        let stream_len = self
            .state
            .channel(id)
            .map(|v| v.stream_bytes.len())
            .unwrap_or(0);

        // Header: a "View configuration" dropdown holding the controls, with the
        // Pause/Resume button kept on the title row whether the dropdown is open or
        // closed. A plain CollapsingHeader lays its body *and* the sibling button in
        // document order, so opening it pushed the button down onto the first control
        // row; CollapsingState lets us render the title row (title + button) ourselves
        // and the body separately, so the button stays put.
        let view0 = self
            .state
            .channel(id)
            .and_then(|v| v.snapshot.as_ref())
            .and_then(|s| s.display_views.first())
            .map(|v0| (v0.id, v0.paused));
        let header_id = ui.make_persistent_id(("view_config", id));
        egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            header_id,
            false,
        )
        .show_header(ui, |ui| {
            // The collapse arrow is drawn by show_header; add the title + the
            // Pause/Resume button on the same row.
            ui.label(bold("Configure view"));
            if let Some((view_id, is_paused)) = view0 {
                if is_paused {
                    if ui.button("Resume").clicked() {
                        self.send(UiCommand::ResumeDisplay(id, view_id));
                    }
                    ui.label("view paused — reception continues");
                } else if ui
                    .button("Pause")
                    .on_hover_text(
                        "Freeze the view — reception continues (the channel stays open).",
                    )
                    .clicked()
                {
                    self.send(UiCommand::PauseDisplay(id, view_id));
                }
            }
        })
        .body(|ui| self.show_view_controls(ui, &mut prefs, stream_len));

        // Persist any edit: update this channel's prefs + config and sync the runtime
        // (display + scroll-buffer retention; no restart — see SetViewConfig).
        if let Some(view) = self.state.channel_mut(id) {
            if !view.view_prefs.eq_settings(&prefs) {
                view.view_prefs = prefs.clone();
                prefs.apply_to_config(&mut view.config);
                let display = view.config.display.clone();
                let retention = view.config.retention.clone();
                self.send(UiCommand::SetViewConfig(
                    id,
                    Box::new(display),
                    Box::new(retention),
                ));
            }
        }

        let msg_mode = prefs.mode;
        let msg_chars = prefs.chars;
        let font_size = prefs.font_size;
        let mono_family = prefs.mono.family();
        let fg = prefs.colors.fg();
        let bg = prefs.colors.bg();

        let has_snapshot = self
            .state
            .channel(id)
            .map(|v| v.snapshot.is_some())
            .unwrap_or(false);
        // The stream viewer (§41): there is one source — the verbatim byte stream.
        let renderer = DisplayView {
            mode: msg_mode,
            encoding: DisplayEncoding::Utf8,
            character_rendering: msg_chars,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_line: 16,
        };
        // Verbatim received bytes (§17–18, §41): line breaks come only from the
        // data — Rendered honors real CR/LF (§44), Raw shows control pictures, Hex
        // is a byte run. Serial and UDP render identically (no reframing).
        //
        // Performance (§100): the scrollback can reach the ~1 MB cap. The bytes
        // arrive incrementally (StreamDelta) so the driver never re-ships the whole
        // buffer; here we (a) memoize the split rows, re-rendering only when data
        // arrives or the view mode changes — keyed on the stream cursor — and (b)
        // virtualize the layout with `show_rows`, laying out only visible rows. Both
        // matter: a non-virtualized selectable Label over ~1 MB stalled the UI.
        let font = egui::FontId::new(font_size, mono_family.clone());
        // Monospace metrics: row height and the width of one glyph ('0' as a stand-in),
        // so we can convert the available pixel width into a column count for wrapping.
        let (row_h, char_w) =
            ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0').max(1.0)));
        // Width of the (non-floating) vertical scrollbar. Used both to size the bar
        // below and to reserve its space when wrapping, so a full row ends just before
        // the bar instead of under it. One const so the two uses can't drift apart.
        const SCROLLBAR_WIDTH: f32 = 12.0;
        // Pin the viewer to exactly the height left in the pane, so the ScrollArea owns
        // the scrolling (and `stick_to_bottom` keeps the newest bytes pinned to the
        // bottom edge) instead of the content overflowing and scrolling the whole pane —
        // which left the latest data stranded below the window fold ("never reaches the
        // bottom"). The Frame's 4px inner margin top+bottom is subtracted.
        let viewer_height = (ui.available_height() - 8.0).max(0.0);
        egui::Frame::new()
            .fill(bg)
            .inner_margin(4.0)
            .show(ui, |ui| {
                // Pre-wrap each cached row to the columns that fit the viewer's inner
                // width (less the scrollbar), so every row is exactly one visual line —
                // uniform height, which `show_rows` needs to virtualize. Re-wraps only
                // when the column count changes (the cache key includes it).
                let avail_w = (ui.available_width() - SCROLLBAR_WIDTH).max(char_w);
                let wrap_cols = (avail_w / char_w).floor().max(8.0) as usize;
                self.refresh_stream_rows(id, &renderer, wrap_cols);
                let rows: &[String] = self
                    .stream_cache
                    .as_ref()
                    .filter(|c| c.key.channel == id)
                    .map(|c| c.rows.as_slice())
                    .unwrap_or(&[]);
                // The viewer fills its full height whether or not data is flowing — the
                // ScrollArea (`auto_shrink([false,false])`) reserves the space, so the
                // window doesn't pop open when a channel starts. Empty state shows a
                // weak note *inside* the scroll area rather than collapsing the frame.
                // Text selection across the (non-interactive) row labels.
                ui.style_mut().interaction.selectable_labels = true;
                // Give the scrollbar a visible track + handle, distinct from the text
                // background, so the strip on the right edge reads as the scrollbar (not
                // mysterious empty space). Both contrast with `bg`, the handle more
                // strongly (see `scrollbar_colors`). Set before the per-widget overrides.
                let (track, handle) = scrollbar_colors(bg);
                ui.visuals_mut().extreme_bg_color = track;
                // A solid, constant-width scrollbar. egui's default is "floating" — thin
                // until hovered, which read as the track widening on hover; `floating =
                // false` plus a fixed `bar_width` (and zero margins) pins it to a steady
                // strip the width we reserved above.
                {
                    let s = &mut ui.style_mut().spacing.scroll;
                    s.floating = false;
                    s.bar_width = SCROLLBAR_WIDTH;
                    s.bar_inner_margin = 0.0;
                    s.bar_outer_margin = 0.0;
                }
                // Suppress the per-widget hover/active visuals egui paints on selectable
                // labels (they drew a flickering box around the rows near the pointer).
                {
                    let w = &mut ui.visuals_mut().widgets;
                    for s in [
                        &mut w.hovered,
                        &mut w.active,
                        &mut w.inactive,
                        &mut w.noninteractive,
                    ] {
                        s.bg_stroke = egui::Stroke::NONE;
                        s.weak_bg_fill = egui::Color32::TRANSPARENT;
                        s.bg_fill = egui::Color32::TRANSPARENT;
                    }
                }
                // The scroll handle is drawn from the *idle* widget visuals (egui maps an
                // un-hovered scrollbar to `widgets.inactive`), using `bg_fill` when
                // `scroll_style.foreground_color` is false (the default) or `fg_stroke`
                // when true. We just cleared those fills for the labels, which is why the
                // handle vanished. Set `foreground_color = true` and drive the handle via
                // `fg_stroke.color` on every interaction state — explicit and immune to
                // the bg_fill clearing above. Hover/drag brighten so it stays visible on
                // a dark scheme.
                ui.style_mut().spacing.scroll.foreground_color = true;
                {
                    let w = &mut ui.visuals_mut().widgets;
                    w.inactive.fg_stroke.color = handle;
                    w.noninteractive.fg_stroke.color = handle;
                    w.hovered.fg_stroke.color = handle.gamma_multiply(1.2);
                    w.active.fg_stroke.color = handle.gamma_multiply(1.4);
                }
                // Zero the inter-row spacing *here*, before `show_rows`, so the row pitch
                // it uses to size the virtual content (and thus where `stick_to_bottom`
                // scrolls) matches what the rows actually render at. Setting it only
                // inside the closure left `show_rows` reserving `row_h + default_spacing`
                // per row while rows drew at `row_h`, so the computed bottom overshot the
                // real last row and the newest data never came into view.
                ui.spacing_mut().item_spacing.y = 0.0;
                let scroll = egui::ScrollArea::vertical()
                    .id_salt("stream")
                    .stick_to_bottom(true)
                    .max_height(viewer_height)
                    .auto_shrink([false, false]);
                if stream_len == 0 {
                    // No data yet: still occupy the full viewer height, with a note.
                    scroll.show(ui, |ui| {
                        let note = if has_snapshot {
                            "no data received yet"
                        } else {
                            "waiting for data — Start the channel"
                        };
                        ui.label(egui::RichText::new(note).weak());
                    });
                    return;
                }
                // Soft-wrapped AND virtualized: rows are pre-wrapped to `wrap_cols`
                // (above) so each is one uniform-height visual line, which lets
                // `show_rows` lay out only the visible rows. This is what keeps the UI
                // responsive — the earlier "render everything" approaches (N labels or
                // one giant galley) re-laid-out the whole buffer every frame and made
                // resizing/the whole UI sluggish. Rows don't wrap again here (they're
                // already wrapped); they just extend if anything slipped through.
                scroll.show_rows(ui, row_h, rows.len().max(1), |ui, range| {
                    for row in &rows[range] {
                        ui.add(
                            egui::Label::new(egui::RichText::new(row).font(font.clone()).color(fg))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
            });
    }

    /// The view-configuration controls (inside the "View configuration" dropdown):
    /// View mode + ctrl-chars on one row, font size / colors / mono on the next, then
    /// the scroll-buffer size readout. Edits the passed-in [`ViewPrefs`] (a clone of
    /// the selected channel's); the caller persists any change. `stream_len` is the
    /// channel's current scrollback byte count. Takes `&self` — touches no app state
    /// beyond rendering.
    fn show_view_controls(&self, ui: &mut egui::Ui, prefs: &mut ViewPrefs, stream_len: usize) {
        let base = egui::TextStyle::Body.resolve(ui.style()).size;
        ui.horizontal(|ui| {
            ui.label(bold("View"))
                .on_hover_text("How the received bytes are displayed in the viewer.");
            ui.radio_value(&mut prefs.mode, DisplayMode::Hex, "Hex")
                .on_hover_text("Each byte as two-digit hex (e.g. 0A 0D 41).");
            ui.radio_value(&mut prefs.mode, DisplayMode::Rendered, "Rendered")
                .on_hover_text("As text, honoring real CR/LF line breaks (§44).");
            ui.radio_value(&mut prefs.mode, DisplayMode::Raw, "Raw")
                .on_hover_text(
                    "As text, but with control characters shown as pictures/markers \
                     instead of acting on the layout (§46).",
                );
            ui.separator();
            // Control-character rendering (§46) — enabled only in Raw mode. The
            // oversized ␊ glyph makes the row taller than the text, so the whole
            // ctrl-chars cluster is bottom-aligned to sit on the View radios' baseline.
            ui.add_enabled_ui(prefs.mode == DisplayMode::Raw, |ui| {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(bold("ctrl-chars"));
                        ui.radio_value(
                            &mut prefs.chars,
                            CharacterRendering::Glyph,
                            egui::RichText::new("␊").size(base * 1.6),
                        )
                        .on_hover_text("Control pictures (␊ ␍ ␉ …)");
                        ui.radio_value(&mut prefs.chars, CharacterRendering::Token, "[LF]")
                            .on_hover_text("Bracketed names ([LF] [CR] [TAB] …)");
                        ui.radio_value(&mut prefs.chars, CharacterRendering::HexEscape, "<0A>")
                            .on_hover_text("Hex escapes (<0A> <0D> <09> …)");
                    });
                });
            });
        });
        ui.horizontal(|ui| {
            ui.label(bold("Size"));
            // An editable "combo": type any size into the field, or pick a preset from
            // the ▾ menu; the text is the source of truth while editing.
            let resp = ui.add(egui::TextEdit::singleline(&mut prefs.font_text).desired_width(40.0));
            if resp.changed() {
                if let Ok(v) = prefs.font_text.trim().parse::<f32>() {
                    prefs.font_size = v.clamp(6.0, 72.0);
                }
            }
            ui.menu_button("\u{25BC}", |ui| {
                for &size in MSG_FONT_SIZES {
                    if ui.button(format!("{size:.0}")).clicked() {
                        prefs.font_size = size;
                        prefs.font_text = format!("{size:.0}");
                        ui.close();
                    }
                }
            });
            ui.separator();
            ui.label(bold("Colors"));
            egui::ComboBox::from_id_salt("msg_colors")
                .selected_text(prefs.colors.label())
                .show_ui(ui, |ui| {
                    for scheme in [
                        ColorScheme::BlackOnWhite,
                        ColorScheme::GreenOnBlack,
                        ColorScheme::GreenOnBlackDim,
                        ColorScheme::AmberOnBlack,
                        ColorScheme::WhiteOnBlack,
                    ] {
                        ui.selectable_value(&mut prefs.colors, scheme, scheme.label());
                    }
                });
            ui.separator();
            ui.label(bold("Mono"));
            egui::ComboBox::from_id_salt("msg_font")
                .selected_text(prefs.mono.label())
                .show_ui(ui, |ui| {
                    for &font in MonoFont::ALL {
                        ui.selectable_value(&mut prefs.mono, font, font.label());
                    }
                });
        });
        // Scroll-buffer cap (§87): how far back the viewer scrolls, chosen from presets
        // (2 … 256 kB) — no free text, so it's always a known value. The current fill is
        // shown alongside. Persisted via retention (the runtime adopts it next start; the
        // GUI viewer caps live).
        const SCROLL_BUFFER_HINT: &str = "How far back you can scroll in this channel's \
            view. A larger buffer uses more memory and can make scrolling and rendering \
            heavier, so pick the smallest that covers what you need — the full history is \
            kept in the .raw recording regardless.";
        ui.horizontal(|ui| {
            ui.label(bold("Scroll buffer"))
                .on_hover_text(SCROLL_BUFFER_HINT);
            egui::ComboBox::from_id_salt("scroll_buffer")
                .selected_text(scroll_buffer_label(prefs.scroll_buffer_bytes))
                .show_ui(ui, |ui| {
                    for &kb in SCROLL_BUFFER_PRESETS_KB {
                        let bytes =
                            (kb * 1024).clamp(MIN_SCROLL_BUFFER_BYTES, MAX_SCROLL_BUFFER_BYTES);
                        ui.selectable_value(
                            &mut prefs.scroll_buffer_bytes,
                            bytes,
                            scroll_buffer_label(bytes),
                        );
                    }
                })
                .response
                .on_hover_text(SCROLL_BUFFER_HINT);
            ui.separator();
            // The current fill (how much is buffered right now, ≤ the cap).
            ui.label(
                egui::RichText::new(format!("now: {}", human_bytes(stream_len as u64))).weak(),
            )
            .on_hover_text("Bytes currently buffered (≤ the scroll-buffer cap).");
        });
    }

    /// Refresh the memoized stream-view rows for `id` if the accumulated bytes or
    /// the render settings changed. Keyed on the view's stream cursor (advances as
    /// deltas are folded) plus the view mode/character rendering, so we re-render
    /// the scrollback only when something actually changed — not every frame.
    fn refresh_stream_rows(&mut self, id: ChannelId, renderer: &DisplayView, wrap_cols: usize) {
        let Some(view) = self.state.channel_mut(id) else {
            self.stream_cache = None;
            return;
        };
        // Inline Mark timestamps (§50.2): rebase each firing's **view-space** offset
        // (the `StreamDelta` offset space — not `byte_offset`, which counts bytes the
        // paused view skipped) onto the accumulated window (front byte = cursor −
        // buffered len), keep only those in-window, and splice their local timestamp
        // text before/after the matched byte — exactly as the Display Recording does.
        let window_start = view.stream_cursor - view.stream_bytes.len() as u64;
        let annotations: Vec<RenderAnnotation> = view
            .snapshot
            .as_ref()
            .map(|snap| {
                snap.matches
                    .iter()
                    .filter_map(|m| {
                        let mark = m.mark.as_ref()?;
                        let offset = m.view_offset?;
                        let within = offset.checked_sub(window_start)? as usize;
                        (within <= view.stream_bytes.len()).then(|| RenderAnnotation {
                            offset: within,
                            placement: if mark.before {
                                AnnotationPlacement::Before
                            } else {
                                AnnotationPlacement::After
                            },
                            text: mark.text.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let marks_sig = annotations_signature(&annotations);
        let key = super::super::StreamRenderKey {
            channel: id,
            cursor: view.stream_cursor,
            len: view.stream_bytes.len(),
            mode: view.view_prefs.mode,
            chars: view.view_prefs.chars,
            wrap_cols,
            marks_sig,
        };
        if self.stream_cache.as_ref().is_some_and(|c| c.key == key) {
            return; // still valid — reuse the cached rows
        }
        let text = renderer.render_text_annotated(view.stream_contiguous(), &annotations);
        let rows = split_stream_rows(&text, wrap_cols);
        self.stream_cache = Some(super::super::StreamRenderCache { key, rows });
    }
}

/// A cheap order-sensitive hash of the spliced annotations, so the row cache
/// invalidates when the inline Mark timestamps change but the raw bytes don't.
fn annotations_signature(annotations: &[RenderAnnotation]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    annotations.len().hash(&mut h);
    for a in annotations {
        a.offset.hash(&mut h);
        matches!(a.placement, AnnotationPlacement::Before).hash(&mut h);
        a.text.hash(&mut h);
    }
    h.finish()
}

/// Scrollbar (track, handle) colors for a viewer whose text background is `bg`. Both
/// are nudged off `bg` toward its opposite end so the scrollbar reads as a distinct
/// strip against the text background — the track subtly, the handle more strongly —
/// and the choice works for both a light (black-on-white) and dark (white-on-black)
/// scheme. Returns colors, not a mutation, so the caller controls when they apply.
fn scrollbar_colors(bg: egui::Color32) -> (egui::Color32, egui::Color32) {
    // Perceived lightness of the background; pick the contrast direction from it.
    let light = (bg.r() as u32 + bg.g() as u32 + bg.b() as u32) / 3 > 128;
    if light {
        // Light bg: a light-grey track, a mid-grey handle.
        (egui::Color32::from_gray(225), egui::Color32::from_gray(150))
    } else {
        // Dark bg: a clearly-lifted track (so it reads against near-black text bg) and
        // a much brighter handle on top of it (gray-95 track vs gray-200 handle reads
        // clearly even when idle).
        (egui::Color32::from_gray(95), egui::Color32::from_gray(200))
    }
}

/// Split rendered stream text into virtualization rows, soft-wrapping each data line
/// to `wrap_cols` monospace columns so every row is exactly one visual line (uniform
/// height — required by `show_rows`). A data line shorter than `wrap_cols` is one row;
/// a longer one (or a line with no LF, e.g. raw binary / UDP) is wrapped into several.
fn split_stream_rows(text: &str, wrap_cols: usize) -> Vec<String> {
    let cols = wrap_cols.max(8);
    let mut rows: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        // Wrap by character count (monospace, so columns == chars). Char-based, not
        // byte-based, so multi-byte UTF-8 isn't split mid-codepoint.
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(cols) {
            rows.push(chunk.iter().collect());
        }
    }
    rows
}
