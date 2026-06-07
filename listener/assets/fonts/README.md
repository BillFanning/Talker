# Bundled fonts

The default `egui` font stack (Hack, Ubuntu-Light, NotoEmoji,
emoji-icon-font) covers Latin and basic emoji and nothing else. The
files in this directory are registered at startup
(`gui/mod.rs::install_*_font*`): `NotoSans-Regular` is promoted to
the **primary** proportional UI face, and the rest are fallbacks so
the wider Unicode range renders with real glyphs in the message
editor, preview, output pane, and logs instead of tofu boxes.

## Layout

| File | Role | Coverage |
|---|---|---|
| `CascadiaMono.ttf` | selectable message-view monospace **and** the control-picture fallback (covers U+2400–2421, `␊` `␍` …) for the UI and the other mono faces — `LICENSE-Cascadia.txt` (OFL-1.1) | Microsoft Cascadia Mono |
| `JetBrainsMono-Regular.ttf` | selectable message-view monospace | JetBrains Mono — `LICENSE-JetBrainsMono.txt` (OFL-1.1) |
| `DejaVuSansMono.ttf` | selectable message-view monospace **and** the wide-coverage fallback for the other mono faces (keeps columns aligned before resorting to proportional Noto) — `LICENSE-DejaVu.txt` (Bitstream Vera; DejaVu changes public domain) | DejaVu Sans Mono |
| `NotoSans-Regular.ttf` | **primary UI font** (Proportional) + last-resort Latin fallback for every mono face | Latin Extended + Greek + Cyrillic + Vietnamese (Noto Sans core) |
| `NotoSans-Bold.ttf` | named family `ui_bold` for genuinely-bold section/field titles (egui `.strong()` only recolors) — `LICENSE-Noto.txt` (OFL-1.1) | Noto Sans Bold (static instance, wght 700) |
| `NotoSansSymbols2-Regular.ttf` | fallback | Math symbols, arrows, geometric shapes, technical / misc symbols |
| `NotoSansThai-Regular.ttf` | fallback | Thai (U+0E00–0E7F) |
| `NotoSansArabic-Regular.ttf` | fallback | Arabic + Supplement + Extended (RTL shaping not done by us; egui renders glyphs only) |
| `NotoSansHebrew-Regular.ttf` | fallback | Hebrew (U+0590–05FF) |
| `NotoSansDevanagari-Regular.ttf` | fallback | Devanagari (Hindi, Sanskrit, Marathi, …) |

## What's intentionally *not* included

- **CJK** (Chinese / Japanese / Korean). A non-CJK build is ~3 MB of
  fonts; adding CJK would push that to 15–25 MB depending on
  coverage. Talker users mostly push NMEA / ASCII / Western text, so
  the binary-size cost wasn't worth the marginal benefit. If you
  need CJK, drop a `NotoSansCJK-Regular.ttc` (or equivalent) in here
  and add a matching entry to `install_fonts`.
- **Indic scripts beyond Devanagari** (Tamil, Bengali, Telugu,
  Kannada, Malayalam, …). Same reasoning — add as needed.
- **SE Asian beyond Thai** (Lao, Khmer, Myanmar). Ditto.
- **Color emoji**. egui's bundled `NotoEmoji` covers monochrome emoji
  glyphs; we don't ship the color variants.

## Sizes

Total bundled font payload is ~4.6 MB (Noto Sans + Bold, the four
selectable monospace faces, and the script/symbol fallbacks). The
release binary grows correspondingly.

## Control pictures (␊ ␍ …)

The full `CascadiaMono.ttf` (a selectable message-view face) covers the
Unicode Control Pictures block (U+2400–U+2421), so it doubles as the
control-picture fallback for the proportional UI and the other mono
faces — no separate subset font is bundled. Cascadia Code / Mono is
licensed under the SIL Open Font License v1.1 (`LICENSE-Cascadia.txt`).

## Noto Sans family

All `NotoSans*-Regular.ttf` files are unhinted, full-coverage TTFs
fetched from <https://github.com/notofonts/notofonts.github.io>
(notofonts.org canonical source). All are licensed under the SIL
Open Font License v1.1; the license text is in `LICENSE-Noto.txt`.

Re-download a script-specific Noto Sans:

```bash
url="https://github.com/notofonts/notofonts.github.io/raw/main/fonts/NotoSans${SCRIPT}/full/ttf/NotoSans${SCRIPT}-Regular.ttf"
curl -sL -o "talker/assets/fonts/NotoSans${SCRIPT}-Regular.ttf" "$url"
```

where `SCRIPT` is `Thai`, `Arabic`, `Hebrew`, `Devanagari`, etc. The
core Noto Sans (Latin / Greek / Cyrillic / Vietnamese) uses an empty
script: `NotoSans/full/ttf/NotoSans-Regular.ttf`.
