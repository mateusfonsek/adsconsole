#!/usr/bin/env python3
"""Build the AdsConsole logo kit: logo SVGs (docs/brand) and app icons (crates/app/icons).

The symbol is two identical pieces, the second rotated 180 degrees. Each piece is a
slanted band with a flat tab; the pieces interlock around a diagonal channel.

Usage (macOS; needs fontTools, rsvg-convert and iconutil):
    python3 docs/brand/build.py <out-dir> <Outfit[wght].ttf>
The font is Outfit (SIL OFL), from github.com/google/fonts/tree/main/ofl/outfit.
Writes <out-dir>/brand and <out-dir>/icons; copy them to docs/brand and crates/app/icons.
"""
import math
import os
import struct
import subprocess
import sys
import tempfile

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

INK = "#15211B"    # deep green-black
CREAM = "#F1EBDF"  # warm paper

# ---------------------------------------------------------------- symbol geometry
# Native units; the symbol's bounding box is WT x HT.
K = 0.68     # slope of the slanted edges (dx per dy)
H = 78       # height of one piece
WB = 50      # band width along its bottom edge
TH = 34      # tab height
W1 = 126     # tab's right edge
WT, HT = 168, 120
R_OUTER, R_INNER = 12, 18


def rounded_polygon(pts, radii):
    """Closed polygon with circular-arc corners (works for convex and reflex corners)."""
    n = len(pts)
    segs = []
    for i in range(n):
        p0, p1, p2 = pts[i - 1], pts[i], pts[(i + 1) % n]
        v1 = (p0[0] - p1[0], p0[1] - p1[1])
        v2 = (p2[0] - p1[0], p2[1] - p1[1])
        l1, l2 = math.hypot(*v1), math.hypot(*v2)
        u1, u2 = (v1[0] / l1, v1[1] / l1), (v2[0] / l2, v2[1] / l2)
        ang = math.acos(max(-1.0, min(1.0, u1[0] * u2[0] + u1[1] * u2[1])))
        t = radii[i] / math.tan(ang / 2)
        a = (p1[0] + u1[0] * t, p1[1] + u1[1] * t)
        b = (p1[0] + u2[0] * t, p1[1] + u2[1] * t)
        sweep = 1 if u1[0] * u2[1] - u1[1] * u2[0] < 0 else 0
        segs.append((a, b, radii[i], sweep))
    d = f"M{segs[0][1][0]:.2f} {segs[0][1][1]:.2f}"
    for i in range(1, n + 1):
        a, b, r, sw = segs[i % n]
        d += f"L{a[0]:.2f} {a[1]:.2f}A{r:.2f} {r:.2f} 0 0 {sw} {b[0]:.2f} {b[1]:.2f}"
    return d + "Z"


def symbol_d():
    piece = [(K * H, 0), (W1, 0), (W1, TH), (WB + K * (H - TH), TH), (WB, H), (0, H)]
    rotated = [(WT - x, HT - y) for x, y in piece]
    radii = [R_OUTER, R_OUTER, R_OUTER, R_INNER, R_OUTER, R_OUTER]
    return rounded_polygon(piece, radii) + " " + rounded_polygon(rotated, radii)


SYMBOL = symbol_d()


def symbol_at(fill, cx, cy, width):
    """The symbol centred at (cx, cy), scaled to `width` px."""
    s = width / WT
    return (f'<path fill="{fill}" transform="translate({cx - WT * s / 2:.3f} {cy - HT * s / 2:.3f}) '
            f'scale({s:.5f})" d="{SYMBOL}"/>')


# ---------------------------------------------------------------- helpers
def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def render(svg_path, png_path, w, h=None):
    subprocess.run(["rsvg-convert", "-w", str(w), "-h", str(h or w), "-o", png_path, svg_path], check=True)


def squircle(x, y, size, radius_ratio=185.4 / 824):
    """Apple continuous-corner rounded square (Bezier approximation of the system shape)."""
    r = size * radius_ratio
    x0, y0, x1, y1 = x, y, x + size, y + size
    a, b, c, d, e, f, g, h = (v * r for v in (1.52866471, 1.08849323, 0.86840689, 0.63149399,
                                              0.07491100, 0.02418421, 0.37282392, 0.16206644))

    def corner(px, py, sx, sy, swap):
        def P(u, v):
            if swap:
                u, v = v, u
            return f"{px + sx * u:.3f} {py + sy * v:.3f}"
        return f"C{P(b, 0)} {P(c, f)} {P(d, e)}C{P(g, h)} {P(h, g)} {P(e, d)}C{P(f, c)} {P(0, b)} {P(0, a)}"

    out = f"M{x0 + a:.3f} {y0:.3f}L{x1 - a:.3f} {y0:.3f}" + corner(x1, y0, -1, 1, False)
    out += f"L{x1:.3f} {y1 - a:.3f}" + corner(x1, y1, -1, -1, True)
    out += f"L{x0 + a:.3f} {y1:.3f}" + corner(x0, y1, 1, -1, False)
    out += f"L{x0:.3f} {y0 + a:.3f}" + corner(x0, y0, 1, 1, True)
    return out + "Z"


# ---------------------------------------------------------------- app icons
# macOS: Apple's template (1024 canvas, 824 shape at 100, continuous corners, shadow in
# the margin). The symbol takes 58% of the shape; small sizes get a larger symbol.
def mac_icon_svg(size):
    k = size / 1024
    off, shape = 100 * k, 824 * k
    ratio = 0.58 if size >= 128 else 0.66 if size >= 32 else 0.74
    body = ""
    defs = ""
    if size >= 128:
        defs = (f'<filter id="s" x="-20%" y="-20%" width="140%" height="140%">'
                f'<feGaussianBlur in="SourceAlpha" stdDeviation="{10 * k:g}"/><feOffset dy="{10 * k:g}"/>'
                f'<feComponentTransfer><feFuncA type="linear" slope="0.28"/></feComponentTransfer>'
                f'<feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge></filter>')
        body += f'<path d="{squircle(off, off, shape)}" fill="{CREAM}" filter="url(#s)"/>'
    else:
        body += f'<rect x="{off}" y="{off}" width="{shape}" height="{shape}" rx="{shape * 0.225:g}" fill="{CREAM}"/>'
    body += symbol_at(INK, size / 2, size / 2, shape * ratio)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">'
            f'{f"<defs>{defs}</defs>" if defs else ""}{body}</svg>\n')


# Windows / Linux: full-bleed tile, no Apple margin.
def win_icon_svg(size):
    off = size * 0.03
    shape = size - 2 * off
    ratio = 0.6 if size >= 64 else 0.7 if size >= 32 else 0.78
    body = f'<rect x="{off:g}" y="{off:g}" width="{shape:g}" height="{shape:g}" rx="{shape * 0.2:g}" fill="{CREAM}"/>'
    body += symbol_at(INK, size / 2, size / 2, shape * ratio)
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">{body}</svg>\n'


# macOS menu bar: template image (black + alpha); the tray scales it to 18 pt tall.
TRAY_W, TRAY_H = 44, 36


def tray_svg():
    body = symbol_at("#000000", TRAY_W / 2, TRAY_H / 2, TRAY_W - 4)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{TRAY_W}" height="{TRAY_H}" '
            f'viewBox="0 0 {TRAY_W} {TRAY_H}">{body}</svg>\n')


def build_ico(pngs, out):
    """ICO with embedded PNG frames (supported since Windows Vista)."""
    datas = [open(p, "rb").read() for p in pngs]
    header = struct.pack("<HHH", 0, 1, len(datas))
    offset = 6 + 16 * len(datas)
    entries = b""
    for d in datas:
        w, h = struct.unpack(">II", d[16:24])
        entries += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(d), offset)
        offset += len(d)
    with open(out, "wb") as f:
        f.write(header + entries + b"".join(datas))


# ---------------------------------------------------------------- wordmark and lockups
def wordmark(font_path, text, cap_px, tracking=-0.015):
    font = TTFont(font_path)
    if "fvar" in font:
        font = instantiateVariableFont(font, {"wght": 500})
    gs = font.getGlyphSet()
    cmap = font.getBestCmap()
    hmtx = font["hmtx"]
    upm = font["head"].unitsPerEm
    s = cap_px / font["OS/2"].sCapHeight
    x = 0.0
    pen = SVGPathPen(gs)
    for ch in text:
        name = cmap[ord(ch)]
        gs[name].draw(TransformPen(pen, (s, 0, 0, -s, x * s, cap_px)))
        x += hmtx[name][0] + tracking * upm
    return pen.getCommands(), (x - tracking * upm) * s


def lockups(font_path):
    out = {}
    cap = 0.5 * HT
    d, w = wordmark(font_path, "AdsConsole", cap)
    gap = 0.22 * HT
    pad = 0.08 * HT
    tx = WT + gap
    ty = (HT - cap) / 2
    W = tx + w
    for name, fill in [("horizontal", INK), ("horizontal-reversed", CREAM),
                       ("horizontal-black", "#000000"), ("horizontal-white", "#FFFFFF")]:
        out[name] = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{-pad:g} {-pad:g} {W + 2 * pad:.2f} {HT + 2 * pad:g}">'
                     f'<path fill="{fill}" d="{SYMBOL}"/>'
                     f'<path fill="{fill}" transform="translate({tx:.2f} {ty:.2f})" d="{d}"/></svg>\n')
    # stacked
    cap_s = 0.36 * HT
    ds, ws = wordmark(font_path, "AdsConsole", cap_s)
    SW = max(WT, ws)
    SH = HT + 0.3 * HT + cap_s
    for name, fill in [("stacked", INK), ("stacked-reversed", CREAM)]:
        out[name] = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{-pad:g} {-pad:g} {SW + 2 * pad:.2f} {SH + 2 * pad:.2f}">'
                     f'<path fill="{fill}" transform="translate({(SW - WT) / 2:.2f} 0)" d="{SYMBOL}"/>'
                     f'<path fill="{fill}" transform="translate({(SW - ws) / 2:.2f} {HT + 0.3 * HT:.2f})" d="{ds}"/></svg>\n')
    # symbol alone (square artboard)
    side = WT * 1.16
    vb = f"{WT / 2 - side / 2:.2f} {HT / 2 - side / 2:.2f} {side:.2f} {side:.2f}"
    for name, fill in [("symbol", INK), ("symbol-reversed", CREAM), ("symbol-black", "#000000"),
                       ("symbol-white", "#FFFFFF")]:
        out[name] = f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}"><path fill="{fill}" d="{SYMBOL}"/></svg>\n'
    return out


def main():
    out_dir, font_path = sys.argv[1], sys.argv[2]
    brand = os.path.join(out_dir, "brand")
    icons = os.path.join(out_dir, "icons")
    work = tempfile.mkdtemp(prefix="adsconsole-icons-")
    for name, text in lockups(font_path).items():
        write(os.path.join(brand, f"{name}.svg"), text)

    iconset = os.path.join(work, "icon.iconset")
    os.makedirs(iconset)
    mac = {}
    for s in (16, 32, 64, 128, 256, 512, 1024):
        p = os.path.join(work, f"mac-{s}.svg")
        write(p, mac_icon_svg(s))
        mac[s] = os.path.join(work, f"mac-{s}.png")
        render(p, mac[s], s)
    slots = {"16x16": 16, "16x16@2x": 32, "32x32": 32, "32x32@2x": 64, "128x128": 128,
             "128x128@2x": 256, "256x256": 256, "256x256@2x": 512, "512x512": 512, "512x512@2x": 1024}
    for slot, s in slots.items():
        subprocess.run(["cp", mac[s], os.path.join(iconset, f"icon_{slot}.png")], check=True)
    os.makedirs(icons, exist_ok=True)
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(icons, "icon.icns")], check=True)
    write(os.path.join(icons, "source-macos.svg"), mac_icon_svg(1024))

    win = {}
    for s in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
        p = os.path.join(work, f"win-{s}.svg")
        write(p, win_icon_svg(s))
        win[s] = os.path.join(work, f"win-{s}.png")
        render(p, win[s], s)
    write(os.path.join(icons, "source.svg"), win_icon_svg(1024))
    for name, s in {"32x32.png": 32, "64x64.png": 64, "128x128.png": 128, "128x128@2x.png": 256,
                    "icon.png": 512}.items():
        subprocess.run(["cp", win[s], os.path.join(icons, name)], check=True)
    build_ico([win[s] for s in (16, 24, 32, 48, 64, 256)], os.path.join(icons, "icon.ico"))

    write(os.path.join(icons, "tray-template.svg"), tray_svg())
    render(os.path.join(icons, "tray-template.svg"), os.path.join(icons, "tray-template.png"), TRAY_W, TRAY_H)
    print(f"ok (previews in {work})")


if __name__ == "__main__":
    main()
