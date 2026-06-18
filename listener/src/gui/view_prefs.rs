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
    }

    /// Seed prefs from a channel's first display view (or defaults if it has none).
    /// Mode and ctrl-chars map directly; the font face and color scheme are parsed
    /// from their persisted preset tokens; an absent font size falls back to default.
    pub(crate) fn from_config(config: &crate::config::ChannelConfig) -> Self {
        let Some(view) = config.display.views.first() else {
            return Self::default();
        };
        let font_size = view.font_size.unwrap_or(DEFAULT_FONT_SIZE);
        Self {
            mode: view.mode,
            chars: view.character_rendering,
            font_size,
            font_text: format!("{font_size:.0}"),
            mono: MonoFont::from_name(view.font.as_deref()),
            colors: ColorScheme::from_name(view.foreground_color.as_deref()),
        }
    }

    /// Fold these prefs into a channel's first display view so a profile save captures
    /// them. No-op if the channel has no display view.
    pub(crate) fn apply_to_config(&self, config: &mut crate::config::ChannelConfig) {
        if let Some(view) = config.display.views.first_mut() {
            apply_to_view(self, view);
        }
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

        // Fold into the config, then re-seed from it — the settings survive.
        prefs.apply_to_config(&mut config);
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
