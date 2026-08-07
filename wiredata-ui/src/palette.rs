//! The apps' named color palette — one place to change any chrome color, so a
//! restyle is a constant edit, not a hunt through call sites. Values migrated
//! from `listener/src/gui/theme.rs` (talker ADR-016 / listener ADR-019).
//!
//! Two const instances: [`LIGHT`] (the shipped listener look) and [`DARK`]
//! (several light values are unreadable on a dark backdrop, so each gets an
//! explicit counterpart).
//!
//! Stream/display *content* colors are deliberately **not** here — those are
//! user-chosen per view.

use egui::Color32;

/// The palette matching the `Ui`'s active theme. The single accessor both
/// apps and the shared chrome use, so every call site recolors live when the
/// user toggles themes.
pub fn active(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// A semantic accent softened into a surface fill by blending it into the
/// panel behind it, `alpha` out of 255.
///
/// A surface tinted this way needs no light and dark variants: it is derived
/// from the theme's own panel color, so it lands pale on light and deep on
/// dark by construction. That is why the palette holds accents and not
/// backgrounds — a hardcoded pair of tints is the same decision made twice,
/// and it drifts the moment an accent changes.
pub fn tint(ui: &egui::Ui, accent: Color32, alpha: u8) -> Color32 {
    ui.visuals()
        .panel_fill
        .blend(Color32::from_rgba_unmultiplied(
            accent.r(),
            accent.g(),
            accent.b(),
            alpha,
        ))
}

/// The chrome colors: four states, and only four.
///
/// # Naming
///
/// Every field names the **role**, never the hue. `fault`, not `fault_red` —
/// because a palette exists precisely so a colour can change, and a name that
/// encodes the value contradicts the thing it is for. `Color32` in a struct
/// called `Palette` already says these are colours; the field only has to say
/// what for.
///
/// # Why four
///
/// There were ten. Five were greys separated by *emphasis* rather than meaning,
/// two ambers sat a shade apart, and two greens meant the same thing in
/// different places. None of that was free: every colour must stay
/// distinguishable from every other, so the cost is **pairs**. Ten colours is
/// forty-five pairs to keep apart; four is six. Three of those pairs were
/// checked against a red-green deficiency and two failed, so the pair count was
/// not a theoretical budget — it was the surface a real defect lived on.
///
/// What replaced the removed fields is not a different colour. Emphasis is
/// `Visuals::weak_text_color` and `text_color`, which the theme already owns and
/// which no palette should be re-deciding; and every state that used to lean on
/// a hue of its own now carries a glyph or a word that says the same thing. That
/// substitution is the rule this palette runs on:
///
/// **Colour reinforces a state. It never carries one alone.**
///
/// The readouts that survived the colour-deficiency review were exactly the ones
/// already obeying that, and the ones that failed were the ones that did not.
/// Before adding a fifth colour, check whether a glyph or a word would do it —
/// that is the cheaper thing to add, and the one that works for every reader.
pub struct Palette {
    /// Everything that means "fault / error / destructive" — channel fault,
    /// recording fault, error diagnostics, the Remove button. One color so
    /// "something is wrong" always looks identical.
    ///
    /// Blue, deliberately. This is the app's most important signal, so it must
    /// survive the most common colour deficiency; blue is discriminable on
    /// every common type, being the axis red-green deficiency leaves intact.
    /// Red was tried first and failed against [`Palette::warning`] — see talker
    /// ADR-049. Do not "restore" it.
    pub fault: Color32,
    /// Needs attention, short of a failure: warning diagnostics, "won't start"
    /// hints, a reconnecting channel. Paired with `⚠` or `◐`, never alone.
    pub warning: Color32,
    /// Active: a running channel, a live recording, an asserted serial control
    /// line. Paired with `●`.
    pub running: Color32,
    /// Inactive: a stopped channel, a recording that is off, a low serial
    /// control line, a neutral diagnostic tone. Paired with `■` or `○`.
    pub idle: Color32,
}

#[cfg(test)]
impl Palette {
    /// The accents, as a set.
    ///
    /// Destructured deliberately: a struct pattern must name every field, so
    /// adding a fifth accent makes this fail to **compile** rather than quietly
    /// leave the new colour out of whatever iterates it. Listing `self.fault`
    /// and friends instead would have moved the omission from the test to here,
    /// which is no better for being tidier.
    ///
    /// Test-only. It exists so a property can be checked over the whole set,
    /// and nothing in either application wants the accents as a list — a
    /// production API added for a test loop is API nobody asked for.
    fn accents(&self) -> [Color32; 4] {
        let Palette {
            fault,
            warning,
            running,
            idle,
        } = *self;
        [fault, warning, running, idle]
    }
}

/// The light-theme palette.
pub const LIGHT: Palette = Palette {
    // Deep enough to carry white text on the Remove button's fill.
    fault: Color32::from_rgb(0, 85, 200),
    warning: Color32::from_rgb(150, 100, 0),
    running: Color32::from_rgb(0, 200, 140),
    idle: Color32::from_gray(120),
};

/// The dark-theme palette. Brighter accents and a lighter grey so every value
/// stays readable on a dark backdrop (the light `warning` would all but vanish).
pub const DARK: Palette = Palette {
    // Lightened so it stays legible as small text on the dark panel.
    fault: Color32::from_rgb(95, 165, 255),
    warning: Color32::from_rgb(230, 175, 70),
    running: Color32::from_rgb(0, 210, 150),
    idle: Color32::from_gray(150),
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every accent differs from every other, in both themes.
    ///
    /// Distinctness is what a test can hold. That the *set* stays small is an
    /// argument, not an assertion, and it lives in the type's documentation
    /// where someone adding a field will read it. `accents` is destructured, so
    /// a fifth field breaks the build here rather than slipping past this loop.
    #[test]
    fn no_two_accents_share_a_value() {
        for palette in [&LIGHT, &DARK] {
            let accents = palette.accents();
            for (index, color) in accents.iter().enumerate() {
                assert!(
                    !accents[..index].contains(color),
                    "two accents share a value: {color:?}"
                );
            }
        }
    }
}
