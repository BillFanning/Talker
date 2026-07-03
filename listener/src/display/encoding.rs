//! Decoding received bytes into characters per the selected Display Encoding
//! (spec §47, §80.1).
//!
//! Decoding is lossy and never panics on malformed input (§119): invalid
//! sequences become the Unicode replacement character `U+FFFD` rather than
//! being dropped, so the operator still sees that *something* was there.

use super::DisplayEncoding;

/// Decode `bytes` into characters, pairing each character with the **byte offset**
/// at which its source bytes begin (§50.2 inline annotations need to map a byte
/// offset onto the decoded character stream). The offsets are non-decreasing and the
/// first is `0`; a character produced from several bytes (UTF-8/UTF-16) reports the
/// offset of its first byte.
pub(crate) fn decode_with_offsets(bytes: &[u8], encoding: DisplayEncoding) -> Vec<(char, usize)> {
    match encoding {
        // One byte → one char: offset == index.
        DisplayEncoding::Ascii => bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| (if b <= 0x7F { b as char } else { '\u{FFFD}' }, i))
            .collect(),
        DisplayEncoding::Latin1 => bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| (b as char, i))
            .collect(),
        // UTF-8: walk the input tracking each char's starting byte offset. Lossy
        // decoding replaces an invalid byte with U+FFFD and advances by one byte, so
        // offsets stay aligned to the input.
        DisplayEncoding::Utf8 => {
            let mut out = Vec::new();
            let mut i = 0usize;
            while i < bytes.len() {
                match std::str::from_utf8(&bytes[i..]) {
                    Ok(s) => {
                        for c in s.chars() {
                            out.push((c, i));
                            i += c.len_utf8();
                        }
                    }
                    Err(e) => {
                        let valid = e.valid_up_to();
                        // Safe: bytes[i..i+valid] is valid UTF-8 by construction.
                        for c in std::str::from_utf8(&bytes[i..i + valid]).unwrap().chars() {
                            out.push((c, i));
                            i += c.len_utf8();
                        }
                        out.push(('\u{FFFD}', i));
                        i += 1;
                    }
                }
            }
            out
        }
        DisplayEncoding::Utf16Le => decode_utf16_with_offsets(bytes, false),
        DisplayEncoding::Utf16Be => decode_utf16_with_offsets(bytes, true),
    }
}

fn decode_utf16_with_offsets(bytes: &[u8], big_endian: bool) -> Vec<(char, usize)> {
    // Decode unit-by-unit so each resulting char can carry the byte offset of its
    // first code unit. A surrogate pair (2 units → 1 char) reports the first unit's
    // offset; an unpaired surrogate or odd trailing byte becomes U+FFFD.
    let mut out = Vec::new();
    let mut i = 0usize;
    let unit_at = |i: usize| {
        let pair = [bytes[i], bytes[i + 1]];
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    };
    while i + 2 <= bytes.len() {
        let u = unit_at(i);
        if (0xD800..=0xDBFF).contains(&u) && i + 4 <= bytes.len() {
            let low = unit_at(i + 2);
            if (0xDC00..=0xDFFF).contains(&low) {
                let cp = 0x10000 + (((u as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                out.push((char::from_u32(cp).unwrap_or('\u{FFFD}'), i));
                i += 4;
                continue;
            }
        }
        out.push((char::from_u32(u as u32).unwrap_or('\u{FFFD}'), i));
        i += 2;
    }
    if i < bytes.len() {
        // A trailing odd byte cannot form a code unit — surface it as replacement.
        out.push(('\u{FFFD}', i));
    }
    out
}

/// Decode `bytes` into a sequence of characters under `encoding` — the character
/// stream without byte offsets. Kept as a test convenience over
/// [`decode_with_offsets`] (which production rendering uses so it can splice inline
/// annotations, §50.2).
#[cfg(test)]
pub(crate) fn decode(bytes: &[u8], encoding: DisplayEncoding) -> Vec<char> {
    decode_with_offsets(bytes, encoding)
        .into_iter()
        .map(|(c, _)| c)
        .collect()
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
    fn offsets_track_multibyte_utf8() {
        // "aéb": 'a'@0, 'é'@1 (2 bytes), 'b'@3.
        assert_eq!(
            decode_with_offsets("aéb".as_bytes(), DisplayEncoding::Utf8),
            [('a', 0), ('é', 1), ('b', 3)]
        );
    }

    #[test]
    fn offsets_are_one_per_byte_for_ascii_and_latin1() {
        assert_eq!(
            decode_with_offsets(b"AB", DisplayEncoding::Ascii),
            [('A', 0), ('B', 1)]
        );
        assert_eq!(
            decode_with_offsets(&[0xE9, 0x41], DisplayEncoding::Latin1),
            [('é', 0), ('A', 1)]
        );
    }

    #[test]
    fn offsets_track_utf16_units_and_pairs() {
        // 'A'@0 (1 unit), then a surrogate pair 😀 (U+1F600) @2 (2 units, 4 bytes).
        let bytes = [0x41, 0x00, 0x3D, 0xD8, 0x00, 0xDE];
        assert_eq!(
            decode_with_offsets(&bytes, DisplayEncoding::Utf16Le),
            [('A', 0), ('😀', 2)]
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
