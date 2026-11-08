#!/usr/bin/env python3
"""Draw the mihoctl menu-bar icon and print it as base64 PNGs for mihoctl.5m.sh.

The glyph is an "M" drawn as a route through three proxy nodes. Each variant is an 18 pt template
image (36 px at 144 dpi, black on transparent) so macOS tints it for light and dark menu bars:
  ok    the glyph
  warn  the glyph with a "!" badge knocked out of the bottom-right corner
  down  the glyph crossed out, with a gap around the slash

Requires Pillow. Run: python3 contrib/swiftbar/icon.py [preview-dir]
With a directory argument it also writes enlarged previews there.
"""

import base64
import io
import sys

from PIL import Image, ImageDraw

PX = 36  # 18 pt @2x
SS = 8  # supersampling factor for anti-aliasing
INK = (0, 0, 0, 255)
CLEAR = (0, 0, 0, 0)


def s(v):
    return round(v * SS)


def glyph(d):
    route = [(6.5, 28), (6.5, 10), (18, 23), (29.5, 10), (29.5, 28)]
    d.line([(s(x), s(y)) for x, y in route], fill=INK, width=s(3.2), joint="curve")
    for x, y in route[::4]:  # round caps on the two open ends
        d.ellipse([s(x - 1.6), s(y - 1.6), s(x + 1.6), s(y + 1.6)], fill=INK)
    for x, y in route[1:4]:  # the three nodes the route passes through
        d.ellipse([s(x - 3.6), s(y - 3.6), s(x + 3.6), s(y + 3.6)], fill=INK)


def warn_badge(d):
    cx, cy = 27.5, 27.5
    d.ellipse([s(cx - 9.6), s(cy - 9.6), s(cx + 9.6), s(cy + 9.6)], fill=CLEAR)  # gap around the badge
    d.ellipse([s(cx - 7.8), s(cy - 7.8), s(cx + 7.8), s(cy + 7.8)], fill=INK)
    d.rounded_rectangle([s(cx - 1.6), s(cy - 5.2), s(cx + 1.6), s(cy + 1.0)], radius=s(1.6), fill=CLEAR)
    d.ellipse([s(cx - 1.8), s(cy + 2.2), s(cx + 1.8), s(cy + 5.8)], fill=CLEAR)


def slash(d):
    a, b = (s(4), s(4)), (s(32), s(32))
    d.line([a, b], fill=CLEAR, width=s(6.4))  # gap so the slash reads against the glyph
    d.line([a, b], fill=INK, width=s(2.6))


def render(*extras):
    img = Image.new("RGBA", (PX * SS, PX * SS), CLEAR)
    d = ImageDraw.Draw(img)
    glyph(d)
    for extra in extras:
        extra(d)
    return img.resize((PX, PX), Image.Resampling.BOX)  # area average: no ringing


def png(img):
    buf = io.BytesIO()
    img.save(buf, "PNG", dpi=(144, 144), optimize=True)
    return buf.getvalue()


VARIANTS = {"OK": render(), "WARN": render(warn_badge), "DOWN": render(slash)}

if __name__ == "__main__":
    for name, img in VARIANTS.items():
        print(f"ICON_{name}='{base64.b64encode(png(img)).decode()}'")
    if len(sys.argv) > 1:
        for name, img in VARIANTS.items():
            preview = Image.new("RGBA", img.size, (236, 236, 236, 255))
            preview.alpha_composite(img)
            preview.resize((PX * 8, PX * 8), Image.Resampling.NEAREST).save(f"{sys.argv[1]}/{name.lower()}.png")
