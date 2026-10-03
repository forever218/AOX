"""Generate the AOX app icon assets/aox.ico (multi-size, pixel-art style).

Run:
    python tools/make_icon.py

Design notes
------------
Pixel art means *authoring at a low resolution and magnifying with
NEAREST*, never drawing at 256 and blurring down. Everything below happens
on a 32x32 grid; each grid cell becomes an 8x8 block at 256px, so every
edge is a hard, deliberate step.

  - letter   : Arial Bold, rasterized on the grid then thresholded. The
               threshold is what kills the anti-aliased greys -- without it
               you get a smooth letter wearing a pixel costume.
  - italic   : a horizontal shear, not an italic font. Doing it ourselves
               keeps the angle identical at every grid size (an italic font
               hinted at 32px and at 256px are not the same letter).
  - outline  : a 1-cell dark ring. On a transparent background the glyph has
               to survive both a light taskbar and a dark one.
  - shadow   : one cell down-right, black. It is what makes the letter read
               as a solid object rather than a sticker.
  - ground   : fully transparent (the "hollow" background). The old mark
               had a rounded gradient plate, which turned into a grey blob
               the moment Windows drew it on a coloured taskbar.
"""
import io
import os
import struct

from PIL import Image, ImageDraw, ImageFont

# One "pixel" of the artwork, in grid cells.
GRID = 32
# Nearest-neighbour magnification from grid to final size.
SCALE = 8
FINAL = GRID * SCALE          # 256

SHEAR = 0.16                  # italic lean; ~13 degrees
GLYPH = 0.78                  # glyph height as a fraction of the grid
THRESHOLD = 128               # >= this counts as ink

ACCENT = (255, 255, 255)     # the A itself: white
EDGE = (0, 0, 0)             # 1-cell outline, black
SHADOW = (0, 0, 0, 105)      # drop shadow

# The preview is composited on both grounds the app actually ships with.
# The icon itself is fully transparent, but a white letter on a transparent
# background is invisible in every image viewer that uses a white page --
# so the preview earns its keep by supplying the ground.
PREVIEW_BG = (26, 25, 22, 255)     # the dark panel
PAPER_BG = (245, 244, 241, 255)    # the light panel

HERE = os.path.dirname(os.path.abspath(__file__))
OUT_DIR = os.path.normpath(os.path.join(HERE, "..", "assets"))
OUT = os.path.join(OUT_DIR, "aox.ico")

# ICO sizes we ship. 16/24/32 come *down* from the grid with a hard
# threshold (so they stay crisp); everything larger is magnified.
ICO_SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]


def load_font(px):
    win = os.environ.get("WINDIR", r"C:\Windows")
    for name in ("arialbd.ttf", "seguisb.ttf", "segoeuib.ttf", "consolab.ttf"):
        p = os.path.join(win, "Fonts", name)
        if os.path.exists(p):
            try:
                return ImageFont.truetype(p, px)
            except OSError:
                pass
    return ImageFont.load_default()


def letter_mask():
    """Rasterize 'A' on the grid and hard-threshold it."""
    img = Image.new("L", (GRID, GRID), 0)
    font = load_font(int(GRID * GLYPH))
    d = ImageDraw.Draw(img)
    box = d.textbbox((0, 0), "A", font=font)
    w, h = box[2] - box[0], box[3] - box[1]
    x = (GRID - w) / 2 - box[0]
    y = (GRID - h) / 2 - box[1]
    d.text((x, y), "A", font=font, fill=255)
    return img.point(lambda v: 255 if v >= THRESHOLD else 0)


def shear(mask, k):
    """Italic lean: shift each row right the further down it is."""
    out = Image.new("L", mask.size, 0)
    src, dst = mask.load(), out.load()
    w, h = mask.size
    for y in range(h):
        dx = int(round((h - 1 - y) * k))
        for x in range(w):
            if src[x, y]:
                nx = x + dx
                if 0 <= nx < w:
                    dst[nx, y] = 255
    return out


def outline(mask):
    """The ring of empty cells touching the glyph -- a 1-cell border."""
    out = Image.new("L", mask.size, 0)
    src, dst = mask.load(), out.load()
    w, h = mask.size
    for y in range(h):
        for x in range(w):
            if src[x, y]:
                continue
            for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
                nx, ny = x + dx, y + dy
                if 0 <= nx < w and 0 <= ny < h and src[nx, ny]:
                    dst[x, y] = 255
                    break
    return out


def shift(mask, dx, dy):
    out = Image.new("L", mask.size, 0)
    src, dst = mask.load(), out.load()
    w, h = mask.size
    for y in range(h):
        for x in range(w):
            if src[x, y]:
                nx, ny = x + dx, y + dy
                if 0 <= nx < w and 0 <= ny < h:
                    dst[nx, ny] = 255
    return out


def paint(mask, color, canvas=None):
    """Magnify a grid mask to FINAL with NEAREST and composite it in colour."""
    big = mask.resize((FINAL, FINAL), Image.NEAREST)
    layer = Image.new("RGBA", (FINAL, FINAL), color[:3] + (0,))
    # Solid fill, but only where the mask is on.
    alpha = big.point(lambda v: color[3] if v else 0)
    layer.putalpha(alpha)
    if canvas is None:
        return layer
    canvas.alpha_composite(layer)
    return canvas


def shrink(mask, size):
    """Box-downsample then re-threshold: smaller sizes stay hard-edged.

    A plain LANCZOS shrink would mix the outline into the background and the
    16px mark would go soft -- exactly what the pixel look must not do.
    """
    small = mask.resize((size, size), Image.BOX)
    return small.point(lambda v: 255 if v > 0 else 0)


def compose(mask, with_shadow=True):
    """Stack the layers into a 256px RGBA canvas: shadow, outline, letter."""
    canvas = Image.new("RGBA", (FINAL, FINAL), (0, 0, 0, 0))
    if with_shadow:
        # Shadow first, and two cells out: the outline ring is one cell wide
        # and sits in exactly the same place a one-cell offset would.
        # Anything closer than that gets painted over and never shows.
        paint(shift(mask, 2, 2), SHADOW, canvas)
    paint(outline(mask), EDGE + (255,), canvas)
    paint(mask, ACCENT + (255,), canvas)
    return canvas


def build_layers():
    glyph = shear(letter_mask(), SHEAR)
    return compose(glyph), glyph


def ico_bytes(images):
    """Hand-assemble the ICO so every size is exactly the art we intended.

    PIL's ICO writer re-derives each size from the master image with its own
    resampling, which is precisely the control this design needs back.
    """
    head = struct.pack("<HHH", 0, 1, len(images))
    entries, blobs, offset = b"", b"", 6 + 16 * len(images)
    for im in images:
        buf = io.BytesIO()
        im.save(buf, format="PNG")
        data = buf.getvalue()
        w, h = im.size
        entries += struct.pack(
            "<BBBBHHII",
            w if w < 256 else 0,
            h if h < 256 else 0,
            0, 0, 1, 32,
            len(data), offset,
        )
        blobs += data
        offset += len(data)
    return head + entries + blobs


def build():
    canvas, glyph = build_layers()
    frames = [canvas]
    for size in ICO_SIZES:
        s = size[0]
        if s >= GRID:
            frames.append(canvas.resize(size, Image.NEAREST))
        else:
            # Below the grid the outline is no longer optional. A white letter
            # with no border on a light taskbar is not a small icon, it is
            # nothing at all -- so the small frames get the outline too and
            # only skip the shadow, which at one pixel would just be a smear.
            frames.append(compose(shrink(glyph, s), with_shadow=False))
    frames.sort(key=lambda im: im.size[0])

    os.makedirs(OUT_DIR, exist_ok=True)
    with open(OUT, "wb") as f:
        f.write(ico_bytes(frames))

    # A PNG next to the .ico, purely so the artwork can be eyeballed without
    # digging through the binary. It carries backgrounds; the .ico does not.
    # Both grounds are shown because a white letter is the one case where
    # "looks right" depends entirely on what is behind it.
    preview = os.path.join(OUT_DIR, "aox-icon-preview.png")
    strip = Image.new("RGBA", (FINAL * 2 + 8, FINAL), PREVIEW_BG)
    for i, bg in enumerate((PREVIEW_BG, PAPER_BG)):
        tile = Image.new("RGBA", (FINAL, FINAL), bg)
        tile.alpha_composite(canvas)
        strip.alpha_composite(tile, (i * (FINAL + 8), 0))
    strip.save(preview)
    print("wrote", OUT)
    print("wrote", preview)


if __name__ == "__main__":
    build()
