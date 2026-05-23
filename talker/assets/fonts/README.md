# Bundled fonts

## CascadiaMono-ControlPictures.ttf

A subset of [Cascadia Mono](https://github.com/microsoft/cascadia-code)
containing only Unicode codepoints **U+2400 through U+2421** — the C0
control-character pictures plus `␠` (U+2420) and `␡` (U+2421).

### Why

None of the fonts that ship with `egui` (Hack, Ubuntu-Light, NotoEmoji,
emoji-icon-font) include any glyphs in the U+2400 Control Pictures block.
The display pane's default control-character rendering style — `Pictures`,
per spec §5.7 — would otherwise show every control byte as a tofu box.

This subset is registered as a fallback font for the `Monospace` and
`Proportional` families in `gui/mod.rs::set_high_contrast_dark_visuals`
(or the font-setup function called near it), so egui falls back to it
when those glyphs are requested.

### Size

Full Cascadia Mono is ~714 KB. Subsetting to the 34 glyphs we need
brings it to ~19 KB, with hinting and OpenType layout tables stripped.

### License

Cascadia Code / Mono is licensed under the SIL Open Font License v1.1.
See `LICENSE-Cascadia.txt`.

### Regeneration

The subset is produced with `fonttools`:

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
