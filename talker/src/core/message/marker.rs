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

/// Make marker deletion atomic.
///
/// `prev` is the text before the user's edit, `curr` the text after. If the
/// edit disturbed a complete `‹XX›` marker in `prev` (typed inside it,
/// backspaced from inside or immediately adjacent to it, etc.), strip the
/// remainder of that marker from `curr` so a single keypress removes the
/// whole 4-character unit rather than leaving an orphan `‹` or `›`.
///
/// Only the simple single-region edit case is handled — multi-marker edits
/// (e.g. selection-replace spanning two markers) fall through unchanged and
/// rely on the compile-time error for cleanup. That's acceptable: those
/// cases are rare and the encoder already names the offending characters.
pub fn repair_after_edit(prev: &str, curr: &mut String) {
    if prev == curr.as_str() {
        return;
    }
    // First byte index where prev and curr disagree — the edit point.
    let div = prev
        .bytes()
        .zip(curr.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let delta = curr.len() as i64 - prev.len() as i64;
    for (range, seg) in segments(prev) {
        if !matches!(seg, Segment::Byte(_)) {
            continue;
        }
        // Strictly inside the marker: any edit disturbed its contents.
        // At range.start with delta < 0: a deletion ate the leading '‹'
        // from the front (e.g. cursor just before '‹', Delete pressed).
        // Boundary insertions and trailing-side deletions land at
        // range.start (insert before '‹') or range.end (insert/delete
        // after '›') and must NOT trigger — those edits are outside.
        let strict_inside = range.start < div && div < range.end;
        let delete_at_start = div == range.start && delta < 0;
        if !(strict_inside || delete_at_start) {
            continue;
        }
        let new_end = range.end as i64 + delta;
        if new_end < range.start as i64 {
            return; // pathological — bail rather than mangle further
        }
        let new_end = (new_end as usize).min(curr.len());
        curr.replace_range(range.start..new_end, "");
        return;
    }
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

    // ── repair_after_edit ──────────────────────────────────────────────────────

    /// Simulate an edit: `prev` is the text before, and `applied(prev)`
    /// returns the text the editor produced. Run `repair_after_edit` on the
    /// result and check the final state.
    fn after_edit(prev: &str, applied: impl FnOnce(&str) -> String) -> String {
        let mut curr = applied(prev);
        repair_after_edit(prev, &mut curr);
        curr
    }

    #[test]
    fn delete_inside_marker_removes_whole_marker() {
        // Cursor between '1' and 'B' in 'A‹1B›B', press Delete — egui removes
        // the 'B'. After repair, the whole marker is gone.
        let out = after_edit("A‹1B›B", |s| s.replacen('B', "", 1));
        assert_eq!(out, "AB");
    }

    #[test]
    fn backspace_after_closing_removes_whole_marker() {
        // Cursor right after '›'; backspace removes the '›' (3 UTF-8 bytes).
        let out = after_edit("A‹1B›B", |s| s.replace('\u{203A}', ""));
        assert_eq!(out, "AB");
    }

    #[test]
    fn delete_at_opening_removes_whole_marker() {
        // Cursor right before '‹'; delete removes the '‹'.
        let out = after_edit("A‹1B›B", |s| s.replace('\u{2039}', ""));
        assert_eq!(out, "AB");
    }

    #[test]
    fn typing_inside_marker_removes_whole_marker() {
        // Cursor between '‹' and '1'; type 'X'.
        let out = after_edit("A‹1B›B", |_| "A‹X1B›B".to_string());
        assert_eq!(out, "AB");
    }

    #[test]
    fn edit_outside_marker_is_left_alone() {
        // Prepend a char — the marker is untouched and should stay intact.
        let out = after_edit("A‹1B›B", |s| format!("Z{s}"));
        assert_eq!(out, "ZA‹1B›B");
        // Append after the trailing literal.
        let out = after_edit("A‹1B›B", |s| format!("{s}Z"));
        assert_eq!(out, "A‹1B›BZ");
        // Insert between '›' and the trailing 'B'.
        let out = after_edit("A‹1B›B", |s| s.replace("›B", "›ZB"));
        assert_eq!(out, "A‹1B›ZB");
    }

    #[test]
    fn unchanged_text_is_unchanged() {
        let out = after_edit("hello ‹FF› world", |s| s.to_string());
        assert_eq!(out, "hello ‹FF› world");
    }
}
