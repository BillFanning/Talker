//! Raw, Rendered, and Hex rendering plus the four character-rendering modes
//! and wrapping (spec §42–§46), and the configured [`DisplayView`] renderer.

use super::encoding::decode_with_offsets;
use super::{CharacterRendering, DisplayEncoding, DisplayMode, RenderedOutput, WrappingMode};

/// Where an inline annotation string is spliced relative to the byte it targets
/// (§50.2 Mark timestamps). Renderer-local so the pure display layer does not depend
/// on `config`; the pipeline maps `config::MarkPosition` onto this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationPlacement {
    /// Splice immediately before the target byte's rendering.
    Before,
    /// Splice immediately after the target byte's rendering.
    After,
}

/// An inline text annotation to splice into rendered output at a byte offset
/// (§50.2). `offset` is a byte index into the *rendered span's* bytes (0-based).
#[derive(Clone, Debug)]
pub struct RenderAnnotation {
    pub offset: usize,
    pub placement: AnnotationPlacement,
    pub text: String,
}

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

/// Split `annotations` into the strings that fall **before** and **after** the byte
/// at `offset`. Both lists preserve input order (annotations are pre-sorted by
/// offset), so multiple marks at one offset stack in a stable order.
fn splice_at(
    annotations: &[RenderAnnotation],
    offset: usize,
) -> (impl Iterator<Item = &str>, impl Iterator<Item = &str>) {
    let before = annotations
        .iter()
        .filter(move |a| a.offset == offset && a.placement == AnnotationPlacement::Before)
        .map(|a| a.text.as_str());
    let after = annotations
        .iter()
        .filter(move |a| a.offset == offset && a.placement == AnnotationPlacement::After)
        .map(|a| a.text.as_str());
    (before, after)
}

/// Render bytes as Hex (§45): each byte as two uppercase hex digits, joined by
/// `separator`. When `bytes_per_line` is `Some`, wrap to that many bytes per
/// line. `annotations` are spliced as extra cells before/after their target byte.
fn render_hex(
    bytes: &[u8],
    separator: &str,
    bytes_per_line: Option<usize>,
    annotations: &[RenderAnnotation],
) -> String {
    // Build the ordered list of cells (each an already-formatted token), inserting
    // annotation strings as their own cells around the target byte's hex pair.
    let mut cells: Vec<String> = Vec::with_capacity(bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        let (before, after) = splice_at(annotations, i);
        cells.extend(before.map(str::to_string));
        cells.push(format!("{b:02X}"));
        cells.extend(after.map(str::to_string));
    }
    // Annotations targeting the one-past-the-end offset (an `After` on the final byte
    // is handled above; a `Before` at len is a trailing mark) attach at the end.
    let (end_before, _) = splice_at(annotations, bytes.len());
    cells.extend(end_before.map(str::to_string));

    match bytes_per_line.filter(|&n| n > 0) {
        Some(n) => cells
            .chunks(n)
            .map(|chunk| chunk.join(separator))
            .collect::<Vec<_>>()
            .join("\n"),
        None => cells.join(separator),
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
fn render_rendered(
    bytes: &[u8],
    encoding: DisplayEncoding,
    annotations: &[RenderAnnotation],
) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    // Annotation strings are inserted verbatim (they don't shift the tab column
    // accounting — a timestamp is presentation, not stream content).
    for (c, offset) in decode_with_offsets(bytes, encoding) {
        let (before, after) = splice_at(annotations, offset);
        for s in before {
            out.push_str(s);
        }
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
            // A space is an ordinary printable character in terminal output — it
            // must pass through. (`is_special` treats 0x20 as special only so Raw
            // mode's Token/Glyph/HexEscape can *make it visible*; that does not
            // apply here.) Guard it before the control-dropping arm below.
            ' ' => {
                out.push(' ');
                col += 1;
            }
            c if is_special(c) => {} // other controls are not printed
            c => {
                out.push(c);
                col += 1;
            }
        }
        for s in after {
            out.push_str(s);
        }
    }
    // A trailing annotation at the one-past-end offset (e.g. an After on the final
    // byte lands above; a Before at len is a trailing mark).
    let (end_before, _) = splice_at(annotations, bytes.len());
    for s in end_before {
        out.push_str(s);
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
    /// Render raw stream bytes to the view's text representation (§41).
    pub fn render_text(&self, bytes: &[u8]) -> String {
        self.render_text_annotated(bytes, &[])
    }

    /// Render raw stream bytes, splicing inline `annotations` (§50.2 Mark
    /// timestamps) before/after their target byte in every mode. `annotations` must
    /// be sorted by `offset`; offsets past the span's end are ignored. Text
    /// insertion only — no byte→coordinate mapping — so it works uniformly across
    /// Raw, Rendered, and Hex.
    pub fn render_text_annotated(&self, bytes: &[u8], annotations: &[RenderAnnotation]) -> String {
        let wrap = matches!(self.wrapping, WrappingMode::Wrap);
        match self.mode {
            DisplayMode::Hex => {
                let bytes_per_line = wrap.then_some(self.hex_bytes_per_line);
                render_hex(bytes, &self.hex_separator, bytes_per_line, annotations)
            }
            DisplayMode::Raw => {
                // One cell per byte's rendered char, with annotation strings spliced
                // in as their own cells so wrapping keeps each intact.
                let mut cells: Vec<String> = Vec::with_capacity(bytes.len());
                for (c, offset) in decode_with_offsets(bytes, self.encoding) {
                    let (before, after) = splice_at(annotations, offset);
                    cells.extend(before.map(str::to_string));
                    cells.push(render_char(c, self.character_rendering));
                    cells.extend(after.map(str::to_string));
                }
                let (end_before, _) = splice_at(annotations, bytes.len());
                cells.extend(end_before.map(str::to_string));
                wrap_cells(&cells, if wrap { self.wrap_width } else { None })
            }
            DisplayMode::Rendered => {
                let text = render_rendered(bytes, self.encoding, annotations);
                if wrap {
                    wrap_lines(&text, self.wrap_width)
                } else {
                    text
                }
            }
        }
    }

    /// Render a span of stream bytes for this view (§41, §141).
    pub fn render_stream(
        &self,
        channel_id: crate::core::ChannelId,
        bytes: &[u8],
    ) -> RenderedOutput {
        self.render_stream_annotated(channel_id, bytes, &[])
    }

    /// Render a span of stream bytes with inline `annotations` spliced in (§50.2).
    pub fn render_stream_annotated(
        &self,
        channel_id: crate::core::ChannelId,
        bytes: &[u8],
        annotations: &[RenderAnnotation],
    ) -> RenderedOutput {
        RenderedOutput {
            channel_id,
            text: self.render_text_annotated(bytes, annotations),
            timestamp: None,
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
    fn rendered_preserves_literal_spaces() {
        // Regression: a space (0x20) is printable in terminal output and must not be
        // dropped as a "special" control (it is visible in Raw/Hex but was vanishing
        // in Rendered).
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        assert_eq!(v.render_text(b"a b  c"), "a b  c");
        assert_eq!(v.render_text(b"$GPGGA, 123, 45"), "$GPGGA, 123, 45");
        // Tab-stop column accounting still tracks spaces: after "ab " (col 3) a tab
        // advances to column 8 → 5 spaces.
        assert_eq!(v.render_text(b"ab \tX"), "ab      X");
    }

    #[test]
    fn render_stream_carries_channel_and_text() {
        use crate::core::ChannelId;
        let cid = ChannelId::new();
        let out = view(DisplayMode::Raw, CharacterRendering::Native).render_stream(cid, b"hi");
        assert_eq!(out.channel_id, cid);
        assert_eq!(out.text, "hi");
    }

    fn ann(offset: usize, placement: AnnotationPlacement, text: &str) -> RenderAnnotation {
        RenderAnnotation {
            offset,
            placement,
            text: text.to_string(),
        }
    }

    #[test]
    fn rendered_splices_timestamp_before_the_match() {
        // The canonical case: `[ts]` immediately before the `$` of a GGA sentence.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [ann(3, AnnotationPlacement::Before, "[17:42:03]")];
        assert_eq!(
            v.render_text_annotated(b"ab\n$GPGGA", &anns),
            "ab\n[17:42:03]$GPGGA"
        );
    }

    #[test]
    fn rendered_splices_timestamp_after_the_match() {
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        // After the byte at offset 2 (the second 'b').
        let anns = [ann(2, AnnotationPlacement::After, "<T>")];
        assert_eq!(v.render_text_annotated(b"abbc", &anns), "abb<T>c");
    }

    #[test]
    fn raw_splices_annotation_as_its_own_cell() {
        let v = view(DisplayMode::Raw, CharacterRendering::HexEscape);
        let anns = [ann(1, AnnotationPlacement::Before, "[T]")];
        // Control bytes still escape; the annotation lands before the byte at 1.
        assert_eq!(v.render_text_annotated(b"A\nB", &anns), "A[T]<0A>B");
    }

    #[test]
    fn raw_annotation_cell_is_kept_intact_when_wrapping() {
        let mut v = view(DisplayMode::Raw, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.wrap_width = Some(3);
        // "[TS]" is one cell (4 cols): it forces a wrap before it rather than splitting.
        let anns = [ann(2, AnnotationPlacement::Before, "[TS]")];
        assert_eq!(v.render_text_annotated(b"ABCDE", &anns), "AB\n[TS]\nCDE");
    }

    #[test]
    fn hex_splices_annotation_as_its_own_cell() {
        let v = view(DisplayMode::Hex, CharacterRendering::Native);
        let anns = [ann(1, AnnotationPlacement::Before, "[T]")];
        assert_eq!(v.render_text_annotated(b"ABC", &anns), "41 [T] 42 43");
    }

    #[test]
    fn hex_wrap_counts_annotation_as_a_cell() {
        let mut v = view(DisplayMode::Hex, CharacterRendering::Native);
        v.wrapping = WrappingMode::Wrap;
        v.hex_bytes_per_line = 2;
        let anns = [ann(1, AnnotationPlacement::Before, "[T]")];
        // Cells: 41, [T], 42, 43 → 2 per line.
        assert_eq!(v.render_text_annotated(b"ABC", &anns), "41 [T]\n42 43");
    }

    #[test]
    fn annotation_at_end_offset_is_a_trailing_mark() {
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [ann(2, AnnotationPlacement::Before, "<end>")];
        assert_eq!(v.render_text_annotated(b"ab", &anns), "ab<end>");
    }

    #[test]
    fn annotation_maps_onto_multibyte_utf8_boundary() {
        // "é" is two UTF-8 bytes (0xC3 0xA9) at offset 1; an annotation before the
        // byte after it (offset 3, 'b') must land after the single 'é' char.
        let v = view(DisplayMode::Rendered, CharacterRendering::Native);
        let anns = [ann(3, AnnotationPlacement::Before, "|")];
        assert_eq!(v.render_text_annotated("aéb".as_bytes(), &anns), "aé|b");
    }
}
