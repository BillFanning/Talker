# Bundled fonts

The default `egui` font stack (Hack, Ubuntu-Light, NotoEmoji,
emoji-icon-font) covers Latin and basic emoji and nothing else. Every
file in this directory is a fallback registered at startup
(`gui/mod.rs::install_*_fallback_fonts`) so the rest of Unicode
renders with real glyphs in the message editor, preview, output
pane, and logs instead of tofu boxes.

## Layout

| File | Purpose |
|---|---|
| `CascadiaMono-ControlPictures.ttf` | U+2400–2421 control-character pictures (`␊` `␍` `␛` etc.) for the Raw display mode |
| `NotoSans-Regular.ttf` | Latin Extended + Greek + Cyrillic + Vietnamese (Noto Sans core) |
| `NotoSansSymbols2-Regular.ttf` | Math symbols, arrows, geometric shapes, technical / misc symbols |
| `NotoSansThai-Regular.ttf` | Thai (U+0E00–0E7F) |
| `NotoSansArabic-Regular.ttf` | Arabic + Supplement + Extended (RTL shaping not done by us; egui renders glyphs only) |
| `NotoSansHebrew-Regular.ttf` | Hebrew (U+0590–05FF) |
| `NotoSansDevanagari-Regular.ttf` | Devanagari (Hindi, Sanskrit, Marathi, …) |

## What's intentionally *not* included

- **CJK** (Chinese / Japanese / Korean). A non-CJK build is ~3 MB of
  fonts; adding CJK would push that to 15–25 MB depending on
  coverage. Talker users mostly push NMEA / ASCII / Western text, so
  the binary-size cost wasn't worth the marginal benefit. If you
  need CJK, drop a `NotoSansCJK-Regular.ttc` (or equivalent) in here
  and add a matching entry to `install_unicode_fallback_fonts`.
- **Indic scripts beyond Devanagari** (Tamil, Bengali, Telugu,
  Kannada, Malayalam, …). Same reasoning — add as needed.
- **SE Asian beyond Thai** (Lao, Khmer, Myanmar). Ditto.
- **Color emoji**. egui's bundled `NotoEmoji` covers monochrome emoji
  glyphs; we don't ship the color variants.

## Sizes

Total bundled font payload is ~3 MB. The release binary grows from
~9 MB to ~12 MB.

## Cascadia Mono — Control Pictures subset

A subset of [Cascadia Mono](https://github.com/microsoft/cascadia-code)
containing only Unicode codepoints **U+2400 through U+2421** — the C0
control-character pictures plus `␠` (U+2420) and `␡` (U+2421).

Full Cascadia Mono is ~714 KB. Subsetting to the 34 glyphs we need
brings it to ~19 KB, with hinting and OpenType layout tables stripped.

Cascadia Code / Mono is licensed under the SIL Open Font License
v1.1. See `LICENSE-Cascadia.txt`.

Regenerate with `fonttools`:

```python
from fontTools.subset import Subsetter, Options
from fontTools.ttLib import TTFont

font = TTFont('CascadiaMono.ttf')
opts = Options()
opts.layout_features = []
opts.hinting = False
opts.desubroutinize = True
opts.notdef_outline = False
opts.drop_tables += ['GSUB', 'GPOS', 'GDEF', 'BASE', 'JSTF', 'DSIG', 'MATH', 'kern']
sub = Subsetter(options=opts)
sub.populate(unicodes=list(range(0x2400, 0x2422)))
sub.subset(font)
font.save('CascadiaMono-ControlPictures.ttf')
```

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
