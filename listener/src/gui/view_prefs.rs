//! Per-channel stream-view presentation preferences (§42, §46, §78).
//!
//! These used to be app-global (one font/mode shared by every channel). They now
//! live per channel — each [`ChannelView`](super::state::ChannelView) owns a
//! `ViewPrefs`, like it already owns its own scrollback and pause state — which is
//! also the seam future per-channel features (highlights, marks) will hang off.
//!
//! They persist via the channel's first [`DisplayViewConfig`]: [`from_config`] seeds
//! a `ViewPrefs` when a channel is added or a profile loads, and [`apply_to_config`]
//! folds edits back so a profile save captures them. The font face and color scheme
//! are presets, so they round-trip as stable tokens (see `MonoFont::name` /
//! `ColorScheme::name`), not free strings.

use crate::config::schema::DisplayViewConfig;
use crate::display::{CharacterRendering, DisplayMode};

use super::fonts::MonoFont;
use super::widgets::ColorScheme;

/// The GUI default font size when a config carries none.
pub(crate) const DEFAULT_FONT_SIZE: f32 = 13.0;

/// Default scroll-buffer cap (§87) when a channel's config carries no retention byte
/// limit: 64 KB. Bounds both the runtime's retained scrollback and the GUI viewer's
/// accumulated copy (they hold the same value — one user concept).
pub(crate) const DEFAULT_SCROLL_BUFFER_BYTES: usize = 64 * 1024;

/// Min/max the scroll-buffer field clamps to: 2 KB … 256 KB.
pub(crate) const MIN_SCROLL_BUFFER_BYTES: usize = 2 * 1024;
pub(crate) const MAX_SCROLL_BUFFER_BYTES: usize = 256 * 1024;

/// The selectable scroll-buffer presets (kB), 2 KB … 256 KB.
pub(crate) const SCROLL_BUFFER_PRESETS_KB: &[usize] = &[2, 8, 16, 32, 64, 128, 256];

/// A label for a scroll-buffer byte size in the dropdown ("N kB").
pub(crate) fn scroll_buffer_label(bytes: usize) -> String {
    format!("{} kB", bytes / 1024)
}

/// One channel's stream-view presentation settings.
#[derive(Clone)]
pub(crate) struct ViewPrefs {
    pub mode: DisplayMode,
    pub chars: CharacterRendering,
    pub font_size: f32,
    /// Editable text backing the font-size combo, so a typed size survives across
    /// frames while it's being entered. Kept in sync with `font_size`.
    pub font_text: String,
    pub mono: MonoFont,
    pub colors: ColorScheme,
    /// Scroll-buffer cap in bytes (§87): how far back the viewer scrolls. Drives the
    /// GUI viewer's accumulation cap live and persists as the channel's
    /// `retention.byte_limit` (the runtime adopts it on the channel's next start).
    /// Set from the presets-only dropdown (no free text), so it's always a known value.
    pub scroll_buffer_bytes: usize,
}

impl Default for ViewPrefs {
    /// Fixed defaults for a new channel (the former app-global defaults).
    fn default() -> Self {
        Self {
            mode: DisplayMode::Rendered,
            chars: CharacterRendering::Glyph,
            font_size: DEFAULT_FONT_SIZE,
            font_text: format!("{DEFAULT_FONT_SIZE:.0}"),
            mono: MonoFont::Cascadia,
            colors: ColorScheme::BlackOnWhite,
            scroll_buffer_bytes: DEFAULT_SCROLL_BUFFER_BYTES,
        }
    }
}

impl ViewPrefs {
    /// Whether two prefs have the same *persisted* settings (ignores the transient
    /// `font_text` edit buffer), so the caller only persists on a real change.
    pub(crate) fn eq_settings(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.chars == other.chars
            && self.font_size == other.font_size
            && self.mono == other.mono
            && self.colors == other.colors
            && self.scroll_buffer_bytes == other.scroll_buffer_bytes
    }

    /// Seed prefs from a channel's first display view (or defaults if it has none).
    /// Mode and ctrl-chars map directly; the font face and color scheme are parsed
    /// from their persisted preset tokens; an absent font size falls back to default.
    pub(crate) fn from_config(config: &crate::config::ChannelConfig) -> Self {
        // Scroll buffer comes from retention (a channel-level field), independent of
        // whether the channel has a display view.
        let scroll_buffer_bytes = config
            .retention
            .byte_limit
            .unwrap_or(DEFAULT_SCROLL_BUFFER_BYTES)
            .clamp(MIN_SCROLL_BUFFER_BYTES, MAX_SCROLL_BUFFER_BYTES);
        let Some(view) = config.display.views.first() else {
            return Self {
                scroll_buffer_bytes,
                ..Self::default()
            };
        };
        let font_size = view.font_size.unwrap_or(DEFAULT_FONT_SIZE);
        Self {
            mode: view.mode,
            chars: view.character_rendering,
            font_size,
            font_text: format!("{font_size:.0}"),
            mono: MonoFont::from_name(view.font.as_deref()),
            colors: ColorScheme::from_name(view.foreground_color.as_deref()),
            scroll_buffer_bytes,
        }
    }

    /// Fold these prefs into a channel's config so a profile save captures them: the
    /// presentation fields onto the first display view, and the scroll buffer onto
    /// `retention.byte_limit`. The display no-ops if the channel has no view.
    pub(crate) fn apply_to_config(&self, config: &mut crate::config::ChannelConfig) {
        if let Some(view) = config.display.views.first_mut() {
            apply_to_view(self, view);
        }
        config.retention.byte_limit = Some(self.scroll_buffer_bytes);
    }
}

/// Write one `ViewPrefs` onto a single [`DisplayViewConfig`].
fn apply_to_view(prefs: &ViewPrefs, view: &mut DisplayViewConfig) {
    view.mode = prefs.mode;
    view.character_rendering = prefs.chars;
    view.font_size = Some(prefs.font_size);
    view.font = Some(prefs.mono.name().to_string());
    view.foreground_color = Some(prefs.colors.name().to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::templates;

    #[test]
    fn view_prefs_round_trip_through_a_channel_config() {
        let mut config = templates::udp_template();
        // Edit every setting away from the defaults.
        let mut prefs = ViewPrefs::from_config(&config);
        prefs.mode = DisplayMode::Hex;
        prefs.chars = CharacterRendering::Token;
        prefs.font_size = 20.0;
        prefs.mono = MonoFont::JetBrains;
        prefs.colors = ColorScheme::GreenOnBlack;
        prefs.scroll_buffer_bytes = 256 * 1024;

        // Fold into the config, then re-seed from it — the settings survive.
        prefs.apply_to_config(&mut config);
        assert_eq!(config.retention.byte_limit, Some(256 * 1024)); // persisted in retention
        let restored = ViewPrefs::from_config(&config);
        assert!(restored.eq_settings(&prefs), "settings should round-trip");
        assert_eq!(restored.font_text, "20"); // edit buffer reflects the size
    }

    #[test]
    fn a_config_without_view_font_size_seeds_the_default() {
        let config = templates::udp_template(); // font_size: None in templates
        let prefs = ViewPrefs::from_config(&config);
        assert_eq!(prefs.font_size, DEFAULT_FONT_SIZE);
    }
}
