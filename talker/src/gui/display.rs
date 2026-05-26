//! The per-channel outbound-data display pane (spec §5.7).
//!
//! Each channel keeps a capped buffer of recently sent messages and renders
//! them on demand in the chosen view mode. This is GUI-only state and is never
//! saved to a profile.

/// How a channel's outgoing data is shown in its display pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    /// Each byte as two uppercase hex digits.
    #[default]
    Hex,
    /// UTF-8 decoded to text; invalid bytes shown as U+FFFD.
    Rendered,
    /// Printable ASCII shown as-is; other bytes as control symbols.
    Raw,
}

/// How control bytes are rendered in [`DisplayMode::Raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControlStyle {
    /// Unicode control pictures (U+2400 block): `␊` `␍` `␛`.
    #[default]
    Pictures,
    /// Bracketed abbreviations: `[LF]` `[CR]` `[ESC]`.
    Brackets,
    /// Hex escape codes: `<0x0A>` `<0x0D>` `<0x1B>`.
    HexEscapes,
}

/// Maximum number of recent messages kept per channel pane.
const CAPACITY: usize = 200;

/// One channel's display pane: a capped buffer of recent sent messages plus
/// the chosen view settings.
#[derive(Default)]
pub struct ChannelDisplay {
    /// Recent sent messages, oldest first; capped at [`CAPACITY`].
    buffer: Vec<Vec<u8>>,
    pub mode: DisplayMode,
    pub control_style: ControlStyle,
}

impl ChannelDisplay {
    /// Record a message that was just sent.
    pub fn push(&mut self, payload: Vec<u8>) {
        self.buffer.push(payload);
        if self.buffer.len() > CAPACITY {
            let excess = self.buffer.len() - CAPACITY;
            self.buffer.drain(..excess);
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Render each buffered message to a display line in the current view.
    pub fn lines(&self) -> impl Iterator<Item = String> + '_ {
        let mode = self.mode;
        let style = self.control_style;
        self.buffer.iter().map(move |msg| render(msg, mode, style))
    }
}

/// Render one message's bytes to a display string.
pub fn render(bytes: &[u8], mode: DisplayMode, control_style: ControlStyle) -> String {
    match mode {
        DisplayMode::Hex => bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" "),
        DisplayMode::Raw => {
            let mut s = String::new();
            for &b in bytes {
                if (0x20..=0x7E).contains(&b) {
                    s.push(b as char);
                } else {
                    s.push_str(&render_control(b, control_style));
                }
            }
            s
        }
        DisplayMode::Rendered => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Render a non-printable byte per `style`. Styles with no symbol for a byte
/// (`Pictures`/`Brackets` outside 0x00–0x1F and 0x7F) fall back to a hex escape.
fn render_control(b: u8, style: ControlStyle) -> String {
    match style {
        ControlStyle::HexEscapes => format!("<0x{b:02X}>"),
        ControlStyle::Pictures => match b {
            0x00..=0x1F => char::from_u32(0x2400 + u32::from(b))
                .map(String::from)
                .unwrap_or_else(|| format!("<0x{b:02X}>")),
            0x7F => "\u{2421}".to_string(),
            _ => format!("<0x{b:02X}>"),
        },
        ControlStyle::Brackets => match b {
            0x00..=0x1F => format!("[{}]", C0_NAMES[b as usize]),
            0x7F => "[DEL]".to_string(),
            _ => format!("<0x{b:02X}>"),
        },
    }
}

/// C0 control-character abbreviations, indexed by byte value 0x00..=0x1F.
const C0_NAMES: [&str; 32] = [
    "NUL", "SOH", "STX", "ETX", "EOT", "ENQ", "ACK", "BEL", "BS", "HT", "LF", "VT", "FF", "CR",
    "SO", "SI", "DLE", "DC1", "DC2", "DC3", "DC4", "NAK", "SYN", "ETB", "CAN", "EM", "SUB", "ESC",
    "FS", "GS", "RS", "US",
];

#[cfg(test)]
mod tests {
    use super::*;

    // ── Hex ───────────────────────────────────────────────────────────────────

    #[test]
    fn hex_renders_uppercase_spaced() {
        assert_eq!(
            render(
                &[0x0D, 0x0A, 0xFF],
                DisplayMode::Hex,
                ControlStyle::Pictures
            ),
            "0D 0A FF"
        );
    }

    #[test]
    fn hex_empty() {
        assert_eq!(render(&[], DisplayMode::Hex, ControlStyle::Pictures), "");
    }

    // ── Raw ───────────────────────────────────────────────────────────────────

    #[test]
    fn raw_shows_printable_as_is() {
        assert_eq!(
            render(b"Hello!", DisplayMode::Raw, ControlStyle::Pictures),
            "Hello!"
        );
    }

    #[test]
    fn raw_control_pictures() {
        // LF -> U+240A, CR -> U+240D, ESC -> U+241B, DEL -> U+2421
        assert_eq!(
            render(
                &[0x0A, 0x0D, 0x1B, 0x7F],
                DisplayMode::Raw,
                ControlStyle::Pictures,
            ),
            "\u{240A}\u{240D}\u{241B}\u{2421}"
        );
    }

    #[test]
    fn raw_control_brackets() {
        assert_eq!(
            render(
                &[0x0A, 0x0D, 0x1B, 0x7F],
                DisplayMode::Raw,
                ControlStyle::Brackets,
            ),
            "[LF][CR][ESC][DEL]"
        );
    }

    #[test]
    fn raw_control_hex_escapes() {
        assert_eq!(
            render(
                &[0x00, 0x1F, 0x7F],
                DisplayMode::Raw,
                ControlStyle::HexEscapes,
            ),
            "<0x00><0x1F><0x7F>"
        );
    }

    #[test]
    fn raw_high_byte_falls_back_to_hex_escape() {
        // 0x80-0xFF have no picture/bracket symbol, so every style shows hex.
        for style in [
            ControlStyle::Pictures,
            ControlStyle::Brackets,
            ControlStyle::HexEscapes,
        ] {
            assert_eq!(render(&[0x80], DisplayMode::Raw, style), "<0x80>");
        }
    }

    #[test]
    fn raw_mixed() {
        assert_eq!(
            render(b"AB\r\n", DisplayMode::Raw, ControlStyle::Brackets),
            "AB[CR][LF]"
        );
    }

    // ── Rendered ──────────────────────────────────────────────────────────────

    #[test]
    fn rendered_decodes_valid_utf8() {
        assert_eq!(
            render(
                "héllo".as_bytes(),
                DisplayMode::Rendered,
                ControlStyle::Pictures
            ),
            "héllo"
        );
    }

    #[test]
    fn rendered_invalid_bytes_become_replacement_char() {
        let out = render(&[0xFF, 0xFE], DisplayMode::Rendered, ControlStyle::Pictures);
        assert!(out.contains('\u{FFFD}'));
    }

    // ── ChannelDisplay buffer ─────────────────────────────────────────────────

    #[test]
    fn buffer_caps_at_capacity() {
        let mut d = ChannelDisplay::default();
        for i in 0..(CAPACITY + 25) {
            d.push(vec![i as u8]);
        }
        assert_eq!(d.lines().count(), CAPACITY);
        // The oldest entries were dropped; the newest is last.
        assert_eq!(
            d.lines().last().unwrap(),
            format!("{:02X}", (CAPACITY + 24) as u8)
        );
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut d = ChannelDisplay::default();
        d.push(vec![0x01]);
        d.clear();
        assert_eq!(d.lines().count(), 0);
    }

    #[test]
    fn lines_use_the_current_mode() {
        let mut d = ChannelDisplay::default();
        d.push(vec![0x41, 0x42]);
        assert_eq!(d.lines().next().unwrap(), "41 42");
        d.mode = DisplayMode::Raw;
        assert_eq!(d.lines().next().unwrap(), "AB");
    }
}
