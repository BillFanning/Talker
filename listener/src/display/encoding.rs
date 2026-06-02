//! Decoding received bytes into characters per the selected Display Encoding
//! (spec §47, §80.1).
//!
//! Decoding is lossy and never panics on malformed input (§119): invalid
//! sequences become the Unicode replacement character `U+FFFD` rather than
//! being dropped, so the operator still sees that *something* was there.

use super::DisplayEncoding;

/// Decode `bytes` into a sequence of characters under `encoding`.
pub(crate) fn decode(bytes: &[u8], encoding: DisplayEncoding) -> Vec<char> {
    match encoding {
        // 7-bit ASCII: bytes above 0x7F are not valid ASCII.
        DisplayEncoding::Ascii => bytes
            .iter()
            .map(|&b| if b <= 0x7F { b as char } else { '\u{FFFD}' })
            .collect(),
        // ISO-8859-1 maps every byte 1:1 to code points U+0000..=U+00FF.
        DisplayEncoding::Latin1 => bytes.iter().map(|&b| b as char).collect(),
        DisplayEncoding::Utf8 => String::from_utf8_lossy(bytes).chars().collect(),
        DisplayEncoding::Utf16Le => decode_utf16(bytes, false),
        DisplayEncoding::Utf16Be => decode_utf16(bytes, true),
    }
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Vec<char> {
    let mut chunks = bytes.chunks_exact(2);
    let units = chunks.by_ref().map(|pair| {
        let bytes = [pair[0], pair[1]];
        if big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        }
    });
    let mut out: Vec<char> = char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect();
    // A trailing odd byte cannot form a code unit — surface it as replacement.
    if !chunks.remainder().is_empty() {
        out.push('\u{FFFD}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_marks_high_bytes_as_replacement() {
        assert_eq!(
            decode(&[0x41, 0x80, 0x42], DisplayEncoding::Ascii),
            ['A', '\u{FFFD}', 'B']
        );
    }

    #[test]
    fn latin1_maps_every_byte() {
        // 0xE9 is 'é' in ISO-8859-1.
        assert_eq!(decode(&[0xE9], DisplayEncoding::Latin1), ['é']);
    }

    #[test]
    fn utf8_is_lossy_on_invalid_sequences() {
        // 0xFF is never valid UTF-8.
        assert_eq!(
            decode(&[0x41, 0xFF], DisplayEncoding::Utf8),
            ['A', '\u{FFFD}']
        );
    }

    #[test]
    fn utf16_respects_byte_order_and_odd_tail() {
        // 'A' = U+0041.
        assert_eq!(decode(&[0x41, 0x00], DisplayEncoding::Utf16Le), ['A']);
        assert_eq!(decode(&[0x00, 0x41], DisplayEncoding::Utf16Be), ['A']);
        // Odd trailing byte → replacement, no panic.
        assert_eq!(
            decode(&[0x41, 0x00, 0x42], DisplayEncoding::Utf16Le),
            ['A', '\u{FFFD}']
        );
    }
}
