//! The per-channel outbound-data display pane (spec §5.7).
//!
//! Each channel keeps a capped buffer of recently sent messages and renders
//! them on demand in the chosen view mode. This is GUI-only state and is never
//! saved to a profile.

/// How a channel's outgoing data is shown in its display pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    /// Each byte as two uppercase hex digits.
    Hex,
    /// UTF-8 decoded to text; invalid bytes shown as U+FFFD.
    #[default]
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
    /// Hex escape codes: `<0A>` `<0D>` `<1B>` — the same format listener
    /// renders, so the two apps read identically.
    HexEscapes,
}

/// Maximum number of recent messages kept per channel pane.
const CAPACITY: usize = 200;

/// Memoized virtualization rows for the pane, plus the key they were built for.
struct RowCache {
    /// (generation, mode, style, wrap columns) — every input the rows depend on.
    key: (u64, DisplayMode, ControlStyle, usize),
    rows: Vec<String>,
}

/// One channel's display pane: a capped buffer of recent sent messages plus
/// the chosen view settings.
#[derive(Default)]
pub struct ChannelDisplay {
    /// Recent sent messages, oldest first; capped at [`CAPACITY`].
    buffer: Vec<Vec<u8>>,
    pub mode: DisplayMode,
    pub control_style: ControlStyle,
    /// Bumped on every buffer mutation; keys the row cache below.
    generation: u64,
    /// Memoized pane rows, keyed by (generation, mode, style, wrap columns) so
    /// the pane re-splits only when the buffer, the view settings, or the pane
    /// width change — not on every repaint.
    cache: Option<RowCache>,
}

impl ChannelDisplay {
    /// Record a message that was just sent.
    pub fn push(&mut self, payload: Vec<u8>) {
        self.buffer.push(payload);
        if self.buffer.len() > CAPACITY {
            let excess = self.buffer.len() - CAPACITY;
            self.buffer.drain(..excess);
        }
        self.generation += 1;
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.generation += 1;
    }

    /// The whole pane's content as one flowing string in the current view.
    ///
    /// Messages are joined with a single space in Hex mode (so byte groups
    /// stay readable: "34 0D 0A 34", not "34 0D 0A34") and concatenated
    /// verbatim otherwise — any line breaks the user sees come from the
    /// bytes themselves, never synthesized by the display. This is the content
    /// the pane shows; [`rows`](Self::rows) splits it for virtualized layout.
    fn flow_text(&self) -> String {
        let sep = if self.mode == DisplayMode::Hex {
            " "
        } else {
            ""
        };
        self.buffer
            .iter()
            .map(|msg| render(msg, self.mode, self.control_style))
            .collect::<Vec<_>>()
            .join(sep)
    }

    /// The pane content split into uniform-height virtualization rows,
    /// soft-wrapped to `wrap_cols` monospace columns (memoized).
    ///
    /// Rendering one giant selectable `Label` over the whole buffer re-lays it
    /// out every frame; splitting into fixed-height rows lets the `ScrollArea`
    /// lay out only the visible ones (`show_rows`). Listener's stream view hit
    /// the same wall and moved to this same split + virtualization.
    pub(super) fn rows(&mut self, wrap_cols: usize) -> &[String] {
        let key = (self.generation, self.mode, self.control_style, wrap_cols);
        if !matches!(&self.cache, Some(c) if c.key == key) {
            let rows = split_rows(&self.flow_text(), wrap_cols);
            self.cache = Some(RowCache { key, rows });
        }
        &self.cache.as_ref().expect("cache was just filled").rows
    }
}

/// Split flow text into virtualization rows: a hard break at every real
/// newline, and a soft wrap of any line longer than `wrap_cols` monospace
/// columns, so every row is exactly one visual line (the uniform height
/// `show_rows` needs). Char-based, not byte-based, so a multi-byte UTF-8
/// codepoint is never split. Mirrors listener's `split_stream_rows`.
fn split_rows(text: &str, wrap_cols: usize) -> Vec<String> {
    let cols = wrap_cols.max(8);
    let mut rows: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(cols) {
            rows.push(chunk.iter().collect());
        }
    }
    rows
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
        DisplayMode::Rendered => crate::core::message::decode_utf8_lossy_latin1(bytes),
    }
}

/// Render a non-printable byte per `style`. Styles with no symbol for a byte
/// (`Pictures`/`Brackets` outside 0x00–0x1F and 0x7F) fall back to a hex escape.
fn render_control(b: u8, style: ControlStyle) -> String {
    match style {
        ControlStyle::HexEscapes => format!("<{b:02X}>"),
        ControlStyle::Pictures => match b {
            0x00..=0x1F => char::from_u32(0x2400 + u32::from(b))
                .map(String::from)
                .unwrap_or_else(|| format!("<{b:02X}>")),
            0x7F => "\u{2421}".to_string(),
            _ => format!("<{b:02X}>"),
        },
        ControlStyle::Brackets => match b {
            0x00..=0x1F => format!("[{}]", C0_NAMES[b as usize]),
            0x7F => "[DEL]".to_string(),
            _ => format!("<{b:02X}>"),
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
            "<00><1F><7F>"
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
            assert_eq!(render(&[0x80], DisplayMode::Raw, style), "<80>");
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
    fn rendered_invalid_bytes_fall_back_to_latin1() {
        // Best-effort decode: invalid UTF-8 bytes are mapped 1:1 to
        // their Latin-1 codepoint so they have a glyph in every font.
        // 0xFF → U+00FF (ÿ), 0xFE → U+00FE (þ).
        let out = render(&[0xFF, 0xFE], DisplayMode::Rendered, ControlStyle::Pictures);
        assert_eq!(out, "\u{00FF}\u{00FE}");
    }

    #[test]
    fn rendered_mixes_utf8_and_high_bytes() {
        // "A" (1 byte, valid ASCII) + 0xEE (invalid UTF-8 start) +
        // "B" (valid ASCII): the high byte falls back to Latin-1
        // (`î`), surrounding valid text decodes normally.
        let out = render(b"A\xEEB", DisplayMode::Rendered, ControlStyle::Pictures);
        assert_eq!(out, "A\u{00EE}B");
    }

    // ── ChannelDisplay buffer ─────────────────────────────────────────────────

    #[test]
    fn buffer_caps_at_capacity() {
        let mut d = ChannelDisplay {
            mode: DisplayMode::Hex,
            ..Default::default()
        };
        for i in 0..(CAPACITY + 25) {
            d.push(vec![i as u8]);
        }
        let text = d.flow_text();
        assert_eq!(text.split(' ').count(), CAPACITY);
        // The oldest entries were dropped; the newest is last.
        assert!(text.ends_with(&format!("{:02X}", (CAPACITY + 24) as u8)));
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut d = ChannelDisplay::default();
        d.push(vec![0x01]);
        d.clear();
        assert_eq!(d.flow_text(), "");
    }

    #[test]
    fn flow_text_follows_the_current_mode() {
        let mut d = ChannelDisplay {
            mode: DisplayMode::Hex,
            ..Default::default()
        };
        d.push(vec![0x41, 0x42]);
        assert_eq!(d.flow_text(), "41 42");
        // A view change re-renders in the new mode…
        d.mode = DisplayMode::Raw;
        assert_eq!(d.flow_text(), "AB");
        // …and a new payload flows on verbatim (no synthesized break).
        d.push(vec![0x43]);
        assert_eq!(d.flow_text(), "ABC");
    }

    // ── Virtualization rows ───────────────────────────────────────────────────

    #[test]
    fn split_rows_hard_breaks_on_newlines() {
        // Real newlines in the data become row boundaries; short lines are one
        // row each; an empty line is a blank row.
        assert_eq!(
            split_rows("ab\n\ncd", 80),
            vec!["ab".to_string(), String::new(), "cd".to_string()]
        );
    }

    #[test]
    fn split_rows_soft_wraps_long_lines() {
        // A line longer than the column count wraps into uniform rows (the
        // column floor is 8).
        assert_eq!(
            split_rows("abcdefghij", 8),
            vec!["abcdefgh".to_string(), "ij".to_string()]
        );
        // An unbroken run with no newline (e.g. a hex byte stream) wraps too.
        assert_eq!(split_rows(&"x".repeat(20), 8).len(), 3); // 8 + 8 + 4
    }

    #[test]
    fn split_rows_never_splits_a_multibyte_codepoint() {
        // 9 'é's = 18 UTF-8 bytes. Char-based wrapping at 8 columns yields
        // 8 + 1 whole chars; byte-based wrapping at 8 would have cut the 4th 'é'.
        let rows = split_rows(&"é".repeat(9), 8);
        assert_eq!(rows, vec!["é".repeat(8), "é".to_string()]);
        assert!(rows
            .iter()
            .all(|r| std::str::from_utf8(r.as_bytes()).is_ok()));
    }

    #[test]
    fn rows_rejoin_to_the_flow_when_nothing_soft_wraps() {
        // At a width wide enough that no line exceeds it, the rows are exactly
        // the flow's newline-separated lines — the split adds no content. A
        // real '\n' in the data (Rendered mode honors it) is the row boundary.
        let mut d = ChannelDisplay {
            mode: DisplayMode::Rendered,
            ..Default::default()
        };
        d.push(b"AB\n".to_vec());
        d.push(b"CD".to_vec());
        let flow = d.flow_text();
        assert_eq!(flow, "AB\nCD");
        let rows = d.rows(200).to_vec();
        assert_eq!(rows, vec!["AB".to_string(), "CD".to_string()]);
        assert_eq!(rows.join("\n"), flow);
    }

    #[test]
    fn rows_rebuild_on_content_and_width_change() {
        let mut d = ChannelDisplay {
            mode: DisplayMode::Hex,
            ..Default::default()
        };
        d.push(vec![0x41, 0x42, 0x43, 0x44]); // "41 42 43 44" (11 chars)
        assert_eq!(d.rows(80), &["41 42 43 44".to_string()]);
        // A narrow pane (the 8-column floor) re-wraps the same content…
        assert_eq!(d.rows(8), &["41 42 43".to_string(), " 44".to_string()]);
        // …and a new send changes the rows.
        d.push(vec![0x45]);
        assert_eq!(d.rows(80), &["41 42 43 44 45".to_string()]);
    }
}
