//! The shared status-glyph set and its painter (talker ADR-016 / listener
//! ADR-019 — extracted once both apps needed the same symbols, like
//! `repaint`). Both apps mark channel state with the same three symbols so
//! the two read as one product:
//!
//! - `●` running / active recording
//! - `■` stopped / idle
//! - `⚠` faulted / needs attention
//!
//! What each *state* maps to stays app-specific (listener has Reconnecting,
//! talker has running-with-error); only the symbols, their optical sizing,
//! and the fixed-cell painter live here. Colors come from
//! [`crate::palette`] at the call site.

/// Running / active: `●`.
pub const RUNNING: &str = "\u{25CF}";
/// Stopped / idle: `■`.
pub const STOPPED: &str = "\u{25A0}";
/// Faulted / needs attention: `⚠`.
pub const FAULT: &str = "\u{26A0}";

/// A serial control line that is **high** (asserted): `●`.
///
/// Deliberately the same symbol as [`RUNNING`] — an asserted line and a running
/// channel mean the same thing to a reader, and one product should not spell
/// "active" two ways. Paired with [`LINE_LOW`] it is filled against hollow,
/// which is the distinction that survives when the colour does not.
pub const LINE_HIGH: &str = "\u{25CF}";
/// A serial control line that is **low**: `○`.
///
/// Hollow, so high and low differ in *shape* and not only in colour. Green
/// against grey is the classic red-green collision, and a control-line readout
/// whose whole job is telling the two levels apart cannot rest on it.
pub const LINE_LOW: &str = "\u{25CB}";

/// The base size multiplier for status glyphs (relative to body size). The
/// square (`■`) is the reference at this size; the dot and triangle are
/// enlarged by the optical correction in [`glyph_size`] to match the
/// square's apparent size.
const STATUS_GLYPH_SCALE: f32 = 1.5;

/// Per-glyph optical correction: `●`/`■`/`⚠` have different bounding boxes,
/// so at one font size they look different sizes. The square is the
/// reference (1.0); the dot and triangle are enlarged so all three read the
/// same size.
fn optical_scale(glyph: &str) -> f32 {
    match glyph {
        STOPPED => 1.0,  // ■ square — the reference
        RUNNING => 1.34, // ● dot — enlarge up to the square
        FAULT => 1.30,   // ⚠ triangle — enlarge up to the square
        _ => 1.0,
    }
}

/// The body-relative size for a status glyph (base scale × optical
/// correction), so every call site renders the same symbol at the same
/// apparent size.
pub fn glyph_size(glyph: &str) -> f32 {
    STATUS_GLYPH_SCALE * optical_scale(glyph)
}

/// Paint a status `glyph` into a **fixed-size, non-interactive cell**,
/// centered. Painting (rather than adding a sized label) keeps the glyph
/// from driving the row height — a taller glyph otherwise shifts the line
/// beside it. `allocate_space` reserves only layout space with no widget id,
/// so there's no stray hover/focus rectangle. `scale` is the body-relative
/// glyph size (from [`glyph_size`]); the cell is sized to the largest glyph.
pub fn paint_glyph(ui: &mut egui::Ui, glyph: &str, scale: f32, color: egui::Color32) {
    let base = egui::TextStyle::Body.resolve(ui.style()).size;
    let (_id, rect) = ui.allocate_space(egui::vec2(base * 1.5, base));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(base * scale),
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_three_glyphs_read_the_same_apparent_size() {
        // The optical corrections put every glyph's effective size in a
        // narrow band around the square's reference size.
        let square = glyph_size(STOPPED);
        for g in [RUNNING, FAULT] {
            let s = glyph_size(g);
            assert!(s > square && s < square * 1.4, "{g} out of band: {s}");
        }
    }

    #[test]
    fn unknown_glyphs_fall_back_to_the_base_scale() {
        assert_eq!(glyph_size("x"), STATUS_GLYPH_SCALE);
    }
}
