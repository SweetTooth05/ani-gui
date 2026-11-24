# ani-gui logo

The ア Play mark: the katakana ア (the "a" of anime), drawn with square-cut strokes on a coral tile, with an amber play
button tucked into its curve. The wordmark "ani-gui" is set in Sora Bold. The line アニグイ underneath it uses one
colour per character.

## Files

| File | Use |
|---|---|
| `svg/ani-gui-symbol.svg` | App icon, avatar, anywhere 48 px and up |
| `svg/ani-gui-symbol-small.svg` | 16–32 px (heavier strokes, play nudged clear of the curve) |
| `svg/ani-gui-symbol-black.svg` / `-white.svg` | One-colour tile with the glyph knocked out (`-small-` versions too) |
| `svg/ani-gui-horizontal.svg` | Main lockup on light backgrounds |
| `svg/ani-gui-horizontal-reversed.svg` | Main lockup on dark backgrounds |
| `svg/ani-gui-compact*.svg` | Lockup without アニグイ, for widths under 320 px (sidebar, headers) |
| `svg/ani-gui-stacked*.svg` | Square-ish spaces (splash, README header) |
| `svg/ani-gui-wordmark*.svg` | Text only, where the tile already appears nearby |
| `*-black.svg` / `*-white.svg` | Single-colour print, embossing, monochrome UIs |
| `icons/hicolor/` | Linux icon theme set, app id `io.github.sweettooth05.AniGui` (16–512 px PNG, scalable SVG, symbolic) |
| `board.png` | Presentation board with mockups |

All text is converted to outlines and all strokes are expanded, so no fonts are needed to open the files.

## Colour

| Name | HEX | RGB | Role |
|---|---|---|---|
| Coral | `#FF5C7A` | 255 92 122 | Tile, primary brand colour |
| Amber | `#FFC24B` | 255 194 75 | Play button, app accent (matches the UI theme) |
| Navy | `#0E1018` | 14 16 24 | Wordmark on light backgrounds, app background |
| Violet | `#8B6CFF` | 139 108 255 | グ in アニグイ only |
| Cyan | `#35D0F0` | 53 208 240 | イ in アニグイ only |
| White | `#FFFFFF` | 255 255 255 | ア glyph, wordmark on dark backgrounds |

Violet and cyan appear only in the アニグイ line, never in the tile.

## Clear space and minimum size

- Clear space on every side equals the tile's corner radius: a quarter of the tile's height.
- Symbol: no smaller than 16 px. Use the `-small` cut at 32 px and below.
- Horizontal lockup with アニグイ: at least 320 px wide (below that the katakana is too small to read). Use the
  compact lockup from 320 px down to 120 px, and the symbol alone below 120 px.

## Don'ts

- Don't recolour the tile, swap the amber and coral, or put the coral tile on a coral or red background.
- Don't set the wordmark in another font, or redraw ア with a typeface glyph.
- Don't stretch, rotate, outline, or add shadows or gradients.
- Don't use アニグイ on its own as the logo. It always sits under the wordmark.

## Fonts

- Wordmark: **Sora** Bold (SIL Open Font License, which allows logo use).
- アニグイ line: **Noto Sans CJK JP** Bold (SIL OFL).
- The ア in the tile is drawn by hand, not taken from a font.

Trademark clearance has not been checked; do a search before any commercial use.
