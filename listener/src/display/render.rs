//! Raw, Rendered, and Hex rendering plus the four character-rendering modes
//! and wrapping (spec §42–§46), and the configured [`DisplayView`] renderer.

use crate::core::Message;

use super::encoding::decode;
use super::{
    CharacterRendering, DisplayEncoding, DisplayMode, RenderedOutput, Renderer, WrappingMode,
};

/// ASCII control mnemonics for 0x00..=0x1F (index == code point).
const CONTROL_NAMES: [&str; 32] = [
    "NUL", "SOH", "STX", "ETX", "EOT", "ENQ", "ACK", "BEL", "BS", "TAB", "LF", "VT", "FF", "CR",
    "SO", "SI", "DLE", "DC1", "DC2", "DC3", "DC4", "NAK", "SYN", "ETB", "CAN", "EM", "SUB", "ESC",
    "FS", "GS", "RS", "US",
];

/// Whether a character is "special" — a control character or the space — and so
/// gets replaced by the Token/Glyph/HexEscape renderings (§43, §46). Space is
/// included so it can be made visible (§43).
fn is_special(c: char) -> bool {
    let cp = c as u32;
    cp <= 0x20 || cp == 0x7F
}

/// Render one character under a character-rendering mode (§46).
fn render_char(c: char, mode: CharacterRendering) -> String {
    match mode {
        // Pass the character through unchanged — including control characters,
        // which Raw display does not interpret (§43). Use the other modes to
        // make them visible.
        CharacterRendering::Native => c.to_string(),
        CharacterRendering::Token => match c {
            _ if !is_special(c) => c.to_string(),
            ' ' => "[SP]".to_string(),
            '\u{7F}' => "[DEL]".to_string(),
            c => format!("[{}]", CONTROL_NAMES[c as usize]),
        },
        CharacterRendering::Glyph => match c as u32 {
            // Control Pictures block: U+2400 + code point covers 0x00..=0x20
            // (so space → U+2420 ␠); DEL → U+2421 ␡.
            cp @ 0..=0x20 => char::from_u32(0x2400 + cp).unwrap().to_string(),
            0x7F => '\u{2421}'.to_string(),
            _ => c.to_string(),
        },
        CharacterRendering::HexEscape => {
            if is_special(c) {
                format!("<{:02X}>", c as u32)
            } else {
                c.to_string()
            }
        }
    }
}

/// Render bytes as Hex (§45): each byte as two uppercase hex digits, joined by
/// `separator`. When `bytes_per_line` is `Some`, wrap to that many bytes per
/// line.
fn render_hex(bytes: &[u8], separator: &str, bytes_per_line: Option<usize>) -> String {
    let to_line = |chunk: &[u8]| {
        chunk
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(separator)
    };
    match bytes_per_line.filter(|&n| n > 0) {
        Some(n) => bytes.chunks(n).map(to_line).collect::<Vec<_>>().join("\n"),
        None => to_line(bytes),
    }
}

/// Join per-character cells, soft-wrapping at `width` columns while keeping each
/// cell (which may be a multi-character token) intact (§43 wrapping).
fn wrap_cells(cells: &[String], width: Option<usize>) -> String {
    match width.filter(|&w| w > 0) {
        None => cells.concat(),
        Some(w) => {
            let mut out = String::new();
            let mut line_len = 0usize;
            for cell in cells {
                let len = cell.chars().count();
                if line_len > 0 && line_len + len > w {
                    out.push('\n');
                    line_len = 0;
                }
                out.push_str(cell);
                line_len += len;
            }
            out
        }
    }
}

/// Hard-wrap each existing line of `text` to `width` characters.
fn wrap_lines(text: &str, width: Option<usize>) -> String {
    let Some(w) = width.filter(|&w| w > 0) else {
        return text.to_string();
    };
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let chars: Vec<char> = line.chars().collect();
        for (j, chunk) in chars.chunks(w).enumerate() {
            if j > 0 {
                out.push('\n');
            }
            out.extend(chunk);
        }
    }
    out
}

/// Rendered Display (§44): terminal-style text. LF starts a new line, CR is
/// dropped (so CRLF collapses to a single break), TAB expands to the next
/// 8-column tab stop, and other control characters are not printed. v1 does not
/// emulate cursor movement.
fn render_rendered(bytes: &[u8], encoding: DisplayEncoding) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for c in decode(bytes, encoding) {
        match c {
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\r' => {}
            '\t' => {
                let spaces = 8 - (col % 8);
                for _ in 0..spaces {
                    out.push(' ');
                }
                col += spaces;
            }
            c if is_special(c) => {} // other controls are not printed
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// A configured Display View renderer (§47, §78, §141). Holds the presentation
/// settings that affect rendered output; purely visual options (font, colors)
/// live in the persisted config and do not affect the produced text.
#[derive(Clone, Debug)]
pub struct DisplayView {
    pub mode: DisplayMode,
    pub encoding: DisplayEncoding,
    pub character_rendering: CharacterRendering,
    pub wrapping: WrappingMode,
    /// View width in columns; wrapping is applied only when this is `Some`.
    pub wrap_width: Option<usize>,
    pub hex_separator: String,
    /// Bytes per line for Hex display when wrapping (§45).
    pub hex_bytes_per_line: usize,
}

impl Default for DisplayView {
    fn default() -> Self {
        Self {
            mode: DisplayMode::Raw,
            encoding: DisplayEncoding::Utf8,
            character_rendering: CharacterRendering::Native,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_line: 16,
        }
    }
}

impl DisplayView {
    /// Render raw bytes to the view's text representation. Used by the
    /// [`Renderer`] impl for Messages and directly for Stream data (§41).
    pub fn render_text(&self, bytes: &[u8]) -> String {
        let wrap = matches!(self.wrapping, WrappingMode::Wrap);
        match self.mode {
            DisplayMode::Hex => {
                let bytes_per_line = wrap.then_some(self.hex_bytes_per_line);
                render_hex(bytes, &self.hex_separator, bytes_per_line)
            }
            DisplayMode::Raw => {
                let cells: Vec<String> = decode(bytes, self.encoding)
                    .into_iter()
                    .map(|c| render_char(c, self.character_rendering))
                    .collect();
                wrap_cells(&cells, if wrap { self.wrap_width } else { None })
            }
            DisplayMode::Rendered => {
                let text = render_rendered(bytes, self.encoding);
                if wrap {
                    wrap_lines(&text, self.wrap_width)
                } else {
                    text
                }
            }
        }
    }

    /// Render a Stream-data view: no Message number or timestamp (§18, §41).
    pub fn render_stream(
        &self,
        channel_id: crate::core::ChannelId,
        bytes: &[u8],
    ) -> RenderedOutput {
        RenderedOutput {
            channel_id,
            message_number: None,
            text: self.render_text(bytes),
            timestamp: None,
        }
    }
}

impl Renderer for DisplayView {
    fn render(&self, message: &Message) -> RenderedOutput {
        RenderedOutput {
            channel_id: message.channel_id,
            message_number: Some(message.number),
            text: self.render_text(&message.bytes),
            timestamp: Some(message.metadata.arrival_timestamp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mode: DisplayMode, rendering: CharacterRendering) -> DisplayView {
        DisplayView {
            mode,
            character_rendering: rendering,
            ..DisplayView::default()
        }
    }

    #[test]
    fn hex_is_uppercase_two_digit_space_separated() {
        let v = view(DisplayMode::Hex, CharacterRendering::Native);
        assert_eq!(v.render_text(b"Hi\n"), "48 69 0A");
    }

    #[test]
    fn hex_wraps_at_bytes_per_line() {
        let mut v = view(DisplayMode::Hex, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.hex_bytes_per_line = 2;
        assert_eq!(v.render_text(b"ABCDE"), "41 42\n43 44\n45");
    }

    #[test]
    fn raw_native_passes_control_bytes_through_unchanged() {
        // No interpretation: CR is preserved as a literal carriage return (§43).
        let v = view(DisplayMode::Raw, CharacterRendering::Native);
        assert_eq!(v.render_text(b"A\r\nB"), "A\r\nB");
    }

    #[test]
    fn raw_token_names_controls_and_space() {
        let v = view(DisplayMode::Raw, CharacterRendering::Token);
        assert_eq!(v.render_text(b"A\r\n B"), "A[CR][LF][SP]B");
    }

    #[test]
    fn raw_glyph_uses_control_pictures() {
        let v = view(DisplayMode::Raw, CharacterRendering::Glyph);
        // \t → ␉ (U+2409), \n → ␊ (U+240A), space → ␠ (U+2420).
        assert_eq!(v.render_text(b"\t\n "), "␉␊␠");
    }

    #[test]
    fn raw_hex_escape_escapes_controls_and_space() {
        let v = view(DisplayMode::Raw, CharacterRendering::HexEscape);
        assert_eq!(v.render_text(b"A\r\n B"), "A<0D><0A><20>B");
    }

    #[test]
    fn raw_wraps_by_cells_keeping_tokens_intact() {
        let mut v = view(DisplayMode::Raw, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.wrap_width = Some(3);
        assert_eq!(v.render_text(b"ABCDEFG"), "ABC\nDEF\nG");
    }

    #[test]
    fn rendered_interprets_crlf_and_tabs() {
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        // CR dropped, LF is a newline.
        assert_eq!(v.render_text(b"a\r\nb"), "a\nb");
        // Tab expands to the next 8-column stop.
        assert_eq!(v.render_text(b"a\tb"), "a       b");
        // Other controls are not printed.
        assert_eq!(v.render_text(b"a\x07b"), "ab");
    }

    #[test]
    fn renderer_trait_carries_number_and_channel() {
        use crate::core::{ChannelId, ChunkTime, MessageMetadata, MessageTimestamp};
        use std::sync::Arc;

        let cid = ChannelId::new();
        let msg = Message {
            channel_id: cid,
            number: 7,
            bytes: Arc::from(b"hi".as_slice()),
            metadata: MessageMetadata {
                total_byte_count: 2,
                arrival_timestamp: MessageTimestamp::from(ChunkTime::now()),
                reception_duration: None,
            },
        };
        let out = view(DisplayMode::Raw, CharacterRendering::Native).render(&msg);
        assert_eq!(out.channel_id, cid);
        assert_eq!(out.message_number, Some(7));
        assert_eq!(out.text, "hi");
        assert!(out.timestamp.is_some());
    }
}
