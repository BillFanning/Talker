//! The per-channel outbound-data display pane (spec §5.7).
//!
//! Each channel keeps a capped buffer of recently sent messages and renders
//! them on demand in the chosen view mode. This is GUI-only state and is never
//! saved to a profile.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    ops::Range,
    time::{Duration, Instant},
};

/// How long the sub-sampling badge stays lit after the last frame the send rate
/// exceeded the sample cadence. The throughput estimator updates only ~once a
/// second, so its value jitters across the cadence threshold; latching past the
/// last above-threshold reading stops the badge flickering (hysteresis).
const SAMPLING_BADGE_HOLD: Duration = Duration::from_secs(2);

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

/// One channel's display pane: a capped buffer of recent sent messages plus
/// the chosen view settings.
pub struct ChannelDisplay {
    /// Recent sent messages, oldest first; capped at [`CAPACITY`].
    buffer: VecDeque<DisplaySample>,
    pub mode: DisplayMode,
    pub control_style: ControlStyle,
    /// Bumped on every buffer mutation; keys the render cache below.
    generation: u64,
    /// Memoized whole-pane render, keyed by (generation, mode, style) so the
    /// pane re-renders only when the buffer or view settings change.
    cache: Option<(u64, DisplayMode, ControlStyle, RenderedOutput)>,
    /// Sub-sampling badge latch: the last time the send rate was above the
    /// sample cadence, and that rate. Smooths the throughput estimator's
    /// jitter so the badge doesn't flicker near the threshold.
    sampling_latch: Option<(Instant, f32)>,
}

struct DisplaySample {
    payload: Vec<u8>,
    /// Sorted positions of `?` bytes created by code-page fallback. Keeping
    /// provenance here prevents literal question marks from being painted as
    /// substitutions later.
    replacement_wire_offsets: Vec<usize>,
}

#[derive(Default)]
struct RenderedOutput {
    text: String,
    /// UTF-8 byte ranges in `text`, suitable for egui `LayoutJob` sections.
    replacement_ranges: Vec<Range<usize>>,
}

impl Default for ChannelDisplay {
    fn default() -> Self {
        Self {
            buffer: VecDeque::with_capacity(CAPACITY),
            mode: DisplayMode::Rendered,
            // Keep this explicit at the channel boundary: a newly added
            // channel always has one ctrl-char radio selected before Raw view
            // is first opened.
            control_style: ControlStyle::Pictures,
            generation: 0,
            cache: None,
            sampling_latch: None,
        }
    }
}

impl ChannelDisplay {
    /// Record a message that was just sent.
    pub fn push(&mut self, payload: Vec<u8>, mut replacement_wire_offsets: Vec<usize>) {
        // Samples originate in compiled-message metadata, but normalize at
        // this UI boundary as a defense against stale or malformed observer
        // data. Only an actual fallback byte can receive the background.
        replacement_wire_offsets.retain(|&offset| payload.get(offset) == Some(&b'?'));
        replacement_wire_offsets.sort_unstable();
        replacement_wire_offsets.dedup();
        self.buffer.push_back(DisplaySample {
            payload,
            replacement_wire_offsets,
        });
        if self.buffer.len() > CAPACITY {
            self.buffer.pop_front();
        }
        self.generation += 1;
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.generation += 1;
    }

    /// Reset state that belongs to one runner lifetime without discarding the
    /// retained Output history or the user's view choices.
    pub(super) fn reset_run_state(&mut self) {
        self.sampling_latch = None;
    }

    /// The whole pane's text in the current view, memoized.
    ///
    /// Messages are joined with a single space in Hex mode (so byte groups
    /// stay readable: "34 0D 0A 34", not "34 0D 0A34") and concatenated
    /// verbatim otherwise — any line breaks the user sees come from the
    /// bytes themselves, never synthesized by the display. One label owns the
    /// full logical flow so selection and copied text preserve those bytes.
    pub(super) fn rendered(&mut self) -> (&str, &[Range<usize>]) {
        let key = (self.generation, self.mode, self.control_style);
        let stale = !matches!(&self.cache, Some((g, m, s, _)) if (*g, *m, *s) == key);
        if stale {
            let mut output = RenderedOutput::default();
            for (index, sample) in self.buffer.iter().enumerate() {
                if index > 0 && self.mode == DisplayMode::Hex {
                    output.text.push(' ');
                }
                append_rendered(
                    &mut output,
                    &sample.payload,
                    &sample.replacement_wire_offsets,
                    self.mode,
                    self.control_style,
                );
            }
            self.cache = Some((key.0, key.1, key.2, output));
        }
        let output = &self.cache.as_ref().expect("cache was just filled").3;
        (&output.text, &output.replacement_ranges)
    }

    /// Whether to show the sub-sampling badge, and at what rate. `active` = the
    /// send rate is currently above the sample cadence. The badge stays lit for
    /// [`SAMPLING_BADGE_HOLD`] past the last `active` frame and reports the rate
    /// from that frame, so the throughput estimator's ~1 Hz jitter across the
    /// threshold can't flicker it (hysteresis).
    pub(super) fn sampling_badge(&mut self, active: bool, rate: f32) -> Option<f32> {
        self.sampling_badge_at(active, rate, Instant::now())
    }

    /// [`sampling_badge`](Self::sampling_badge) with an injectable clock, for tests.
    fn sampling_badge_at(&mut self, active: bool, rate: f32, now: Instant) -> Option<f32> {
        if active {
            self.sampling_latch = Some((now, rate));
        }
        self.sampling_latch
            .filter(|(t, _)| now.saturating_duration_since(*t) < SAMPLING_BADGE_HOLD)
            .map(|(_, r)| r)
    }
}

/// Render one message's bytes to a display string.
#[cfg(test)]
pub fn render(bytes: &[u8], mode: DisplayMode, control_style: ControlStyle) -> String {
    let mut output = RenderedOutput::default();
    append_rendered(&mut output, bytes, &[], mode, control_style);
    output.text
}

/// Append one wire message and map its replacement byte positions into ranges
/// in the rendered UTF-8 string. Replacement boundaries are always standalone
/// ASCII `?` bytes, so splitting Rendered-mode decoding at them cannot divide a
/// valid multi-byte UTF-8 scalar.
fn append_rendered(
    output: &mut RenderedOutput,
    bytes: &[u8],
    replacement_offsets: &[usize],
    mode: DisplayMode,
    control_style: ControlStyle,
) {
    match mode {
        DisplayMode::Hex => {
            let mut replacement_index = 0;
            for (offset, byte) in bytes.iter().enumerate() {
                if offset > 0 {
                    output.text.push(' ');
                }
                let start = output.text.len();
                write!(output.text, "{byte:02X}").expect("writing to String cannot fail");
                if replacement_offsets.get(replacement_index) == Some(&offset) {
                    output.replacement_ranges.push(start..output.text.len());
                    replacement_index += 1;
                }
            }
        }
        DisplayMode::Raw => {
            let mut replacement_index = 0;
            for (offset, &byte) in bytes.iter().enumerate() {
                let start = output.text.len();
                if (0x20..=0x7E).contains(&byte) {
                    output.text.push(byte as char);
                } else {
                    output.text.push_str(&render_control(byte, control_style));
                }
                if replacement_offsets.get(replacement_index) == Some(&offset) {
                    output.replacement_ranges.push(start..output.text.len());
                    replacement_index += 1;
                }
            }
        }
        DisplayMode::Rendered => {
            let mut cursor = 0;
            for &offset in replacement_offsets {
                debug_assert_eq!(bytes.get(offset), Some(&b'?'));
                output
                    .text
                    .push_str(&crate::core::message::decode_utf8_lossy_latin1(
                        &bytes[cursor..offset],
                    ));
                let start = output.text.len();
                output.text.push('?');
                output.replacement_ranges.push(start..output.text.len());
                cursor = offset + 1;
            }
            output
                .text
                .push_str(&crate::core::message::decode_utf8_lossy_latin1(
                    &bytes[cursor..],
                ));
        }
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
            d.push(vec![i as u8], Vec::new());
        }
        let text = d.rendered().0.to_string();
        assert_eq!(text.split(' ').count(), CAPACITY);
        // The oldest entries were dropped; the newest is last.
        assert!(text.ends_with(&format!("{:02X}", (CAPACITY + 24) as u8)));
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut d = ChannelDisplay::default();
        d.push(vec![0x01], Vec::new());
        d.clear();
        assert_eq!(d.rendered().0, "");
    }

    #[test]
    fn rendered_follows_the_current_mode_and_memoizes() {
        let mut d = ChannelDisplay {
            mode: DisplayMode::Hex,
            ..Default::default()
        };
        d.push(vec![0x41, 0x42], Vec::new());
        assert_eq!(d.rendered().0, "41 42");
        // A view change invalidates the cache…
        d.mode = DisplayMode::Raw;
        assert_eq!(d.rendered().0, "AB");
        // …and a new payload does too.
        d.push(vec![0x43], Vec::new());
        assert_eq!(d.rendered().0, "ABC");
    }

    #[test]
    fn new_channel_has_an_explicit_ctrl_character_style() {
        let display = ChannelDisplay::default();
        assert_eq!(display.mode, DisplayMode::Rendered);
        assert_eq!(display.control_style, ControlStyle::Pictures);
    }

    #[test]
    fn output_tracks_only_fallback_question_marks_in_every_view() {
        let mut display = ChannelDisplay::default();
        display.push(b"?A?".to_vec(), vec![2]);

        let (text, ranges) = display.rendered();
        assert_eq!(text, "?A?");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 2..3);

        display.mode = DisplayMode::Raw;
        let (text, ranges) = display.rendered();
        assert_eq!(text, "?A?");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 2..3);

        display.mode = DisplayMode::Hex;
        let (text, ranges) = display.rendered();
        assert_eq!(text, "3F 41 3F");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 6..8);
    }

    #[test]
    fn output_rejects_invalid_replacement_metadata() {
        let mut display = ChannelDisplay::default();
        display.push(b"A?".to_vec(), vec![0, 1, 1, 99]);
        let (text, ranges) = display.rendered();
        assert_eq!(text, "A?");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 1..2);
    }

    #[test]
    fn sampling_badge_latches_past_a_rate_dip() {
        // The throughput estimator jitters across the cadence threshold ~1 Hz;
        // the latch keeps the badge lit (and its rate stable) through a dip so
        // it doesn't flicker, and clears only after the hold with no activity.
        let mut d = ChannelDisplay::default();
        let t0 = Instant::now();
        // Above the cadence → badge shows that rate.
        assert_eq!(d.sampling_badge_at(true, 500.0, t0), Some(500.0));
        // A dip to below-cadence shortly after still shows (latched), keeping
        // the last rate — no flicker.
        let t1 = t0 + Duration::from_millis(500);
        assert_eq!(d.sampling_badge_at(false, 0.0, t1), Some(500.0));
        // Well past the hold with no new activity → badge clears.
        let t2 = t0 + Duration::from_secs(3);
        assert_eq!(d.sampling_badge_at(false, 0.0, t2), None);
        // A fresh above-cadence reading re-latches with the new rate.
        assert_eq!(d.sampling_badge_at(true, 300.0, t2), Some(300.0));
        d.reset_run_state();
        assert_eq!(d.sampling_badge_at(false, 0.0, t2), None);
    }
}
