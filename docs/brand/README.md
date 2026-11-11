# AdsConsole brand

The symbol is two identical pieces that interlock around a diagonal channel; the second piece is the first one rotated 180 degrees. Two sides (your AI agent and your ad accounts) that fit together through AdsConsole.

## Files

| File | Use |
|---|---|
| `horizontal.svg`, `horizontal-reversed.svg` | Symbol + name, the default lockup |
| `horizontal-black.svg`, `horizontal-white.svg` | One-color lockup |
| `stacked.svg`, `stacked-reversed.svg` | Narrow spaces |
| `symbol.svg`, `symbol-reversed.svg` | Symbol alone |
| `symbol-black.svg`, `symbol-white.svg` | One-color symbol |
| `build.py` | Generates all of the above and the app icons in `crates/app/icons/` |

App icons (`crates/app/icons/`):

- `icon.icns`: macOS, built on Apple's icon template (1024 canvas, 824 shape, continuous corners, shadow in the margin). Sizes below 128 px use a larger symbol so it stays legible.
- `icon.ico`, `*.png`: Windows and Linux, full-bleed tile.
- `tray-template.png`: the macOS menu bar icon, a monochrome template image that macOS tints for light and dark menu bars.
- `source-macos.svg`, `source.svg`: the 1024 px masters.

To rebuild after changing the geometry:

```bash
python3 docs/brand/build.py /tmp/adsconsole-brand <path to Outfit[wght].ttf>
cp /tmp/adsconsole-brand/brand/*.svg docs/brand/
cp /tmp/adsconsole-brand/icons/* crates/app/icons/
```

## Construction

Each piece is a slanted band (slope 0.68 horizontal per vertical) with a flat tab on top. Both pieces share the same outer corner radius, with a softer radius where the tab meets the band. The channel between them runs parallel to the bands. `build.py` holds the exact values.

## Color

| Role | Hex |
|---|---|
| Ink | `#15211B` |
| Paper | `#F1EBDF` |

Ink on paper is the default. Use paper on ink for dark backgrounds, and black or white when only one ink is available.

## Type

The name is set in Outfit Medium (weight 500) with slightly tight tracking. Outfit is licensed under the SIL Open Font License. In the SVGs the name is outlined, so no font is needed to use them.

## Rules

- **Clear space:** keep at least the height of one piece's tab (about a quarter of the symbol's height) free on every side.
- **Minimum size:** symbol 16 px wide; horizontal lockup 20 px tall on screen.
- **Do:** use the reversed versions on dark backgrounds and the one-color versions when color isn't available.
- **Don't:** recolor the two pieces differently, change the gap between them, add effects (gradients, glows, outlines), rotate, stretch, or set the name in another font.
