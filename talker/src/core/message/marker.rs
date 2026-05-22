//! Inline raw-byte markers for text payloads (spec §5.3).
//!
//! In a UTF-8 or ASCII message, a non-printable byte is written as the marker
//! `‹XX›` — U+2039, two hex digits, U+203A — for example `‹1B›` for ESC. This
//! module splits such text into literal runs and marker bytes; the GUI uses
//! the same split to highlight markers as they are typed.

use std::ops::Range;

/// One piece of marker-aware text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// A run of literal text.
    Text,
    /// A raw byte written as a `‹XX›` marker.
    Byte(u8),
}

const OPEN: char = '\u{2039}'; // ‹
const CLOSE: char = '\u{203A}'; // ›

/// Total UTF-8 byte length of a `‹XX›` marker: `‹` + 2 hex digits + `›`.
const MARKER_LEN: usize = 3 + 2 + 3;

/// Split `text` into segments, each tagged with its byte range in `text`.
///
/// `‹XX›` is a raw-byte marker; any `‹` not forming a complete marker is
/// treated as literal text. Returned ranges are non-empty and fall on `char`
/// boundaries, so `&text[range]` is always valid.
pub fn segments(text: &str) -> Vec<(Range<usize>, Segment)> {
    let mut out: Vec<(Range<usize>, Segment)> = Vec::new();
    let mut literal_start = 0;
    let mut i = 0;
    while i < text.len() {
        if let Some(byte) = match_marker(text, i) {
            if i > literal_start {
                out.push((literal_start..i, Segment::Text));
            }
            let end = i + MARKER_LEN;
            out.push((i..end, Segment::Byte(byte)));
            i = end;
            literal_start = i;
        } else {
            // `i` is on a char boundary; advance past one character.
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if literal_start < text.len() {
        out.push((literal_start..text.len(), Segment::Text));
    }
    out
}

/// If a complete `‹XX›` marker starts at byte `i`, return its byte value.
fn match_marker(text: &str, i: usize) -> Option<u8> {
    let after_open = text[i..].strip_prefix(OPEN)?;
    let mut chars = after_open.chars();
    let h1 = chars.next()?;
    let h2 = chars.next()?;
    if !h1.is_ascii_hexdigit() || !h2.is_ascii_hexdigit() {
        return None;
    }
    // h1 and h2 are ASCII, so they occupy exactly two bytes.
    after_open[2..].strip_prefix(CLOSE)?;
    u8::from_str_radix(&after_open[..2], 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect `segments` as `(literal-or-byte)` for easy assertions.
    fn parts(text: &str) -> Vec<Segment> {
        segments(text).into_iter().map(|(_, s)| s).collect()
    }

    #[test]
    fn plain_text_is_one_literal_segment() {
        assert_eq!(parts("hello world"), vec![Segment::Text]);
    }

    #[test]
    fn empty_text_has_no_segments() {
        assert!(segments("").is_empty());
    }

    #[test]
    fn a_marker_between_text() {
        assert_eq!(
            parts("A‹0D›B"),
            vec![Segment::Text, Segment::Byte(0x0D), Segment::Text]
        );
    }

    #[test]
    fn marker_only() {
        assert_eq!(parts("‹1B›"), vec![Segment::Byte(0x1B)]);
    }

    #[test]
    fn adjacent_markers() {
        assert_eq!(
            parts("‹0D›‹0A›"),
            vec![Segment::Byte(0x0D), Segment::Byte(0x0A)]
        );
    }

    #[test]
    fn lowercase_hex_is_accepted() {
        assert_eq!(parts("‹ff›"), vec![Segment::Byte(0xFF)]);
    }

    #[test]
    fn segment_ranges_slice_the_original() {
        let text = "A‹0D›B";
        let segs = segments(text);
        assert_eq!(&text[segs[0].0.clone()], "A");
        assert_eq!(&text[segs[1].0.clone()], "‹0D›");
        assert_eq!(&text[segs[2].0.clone()], "B");
    }

    #[test]
    fn incomplete_marker_is_literal() {
        // No hex digits, one digit, non-hex digits, or no closer.
        for s in ["‹›", "‹1›", "‹XY›", "‹1B", "a ‹ b"] {
            assert_eq!(parts(s), vec![Segment::Text], "{s:?}");
        }
    }

    #[test]
    fn marker_at_end_of_multibyte_text() {
        assert_eq!(parts("café‹1B›"), vec![Segment::Text, Segment::Byte(0x1B)]);
    }
}
