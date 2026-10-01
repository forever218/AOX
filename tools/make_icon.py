"""生成 AOX 应用图标 assets/aox.ico（多尺寸）。

运行：
    python tools/make_icon.py
"""
import os
from PIL import Image, ImageDraw, ImageFilter, ImageFont

SIZE = 256
BG_TOP = (18, 22, 28)
BG_BOTTOM = (9, 11, 14)
ACCENT = (34, 211, 238)   # 青色 A
BORDER = (42, 51, 64)
RADIUS = 52

HERE = os.path.dirname(os.path.abspath(__file__))
OUT_DIR = os.path.normpath(os.path.join(HERE, "..", "assets"))
OUT = os.path.join(OUT_DIR, "aox.ico")


def rounded_mask(size, radius):
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=255)
    return m


def vertical_gradient(size, top, bottom):
    g = Image.new("RGB", (1, size))
    for y in range(size):
        t = y / max(size - 1, 1)
        g.putpixel((0, y), tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3)))
    return g.resize((size, size))


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


def build():
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    img.paste(vertical_gradient(SIZE, BG_TOP, BG_BOTTOM), (0, 0), rounded_mask(SIZE, RADIUS))

    font = load_font(178)
    text = "A"
    probe = ImageDraw.Draw(img)
    box = probe.textbbox((0, 0), text, font=font)
    x = (SIZE - (box[2] - box[0])) / 2 - box[0]
    y = (SIZE - (box[3] - box[1])) / 2 - box[1] - 6

    # 外发光：让小尺寸（16/32px）下字形轮廓更可辨识
    glow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    ImageDraw.Draw(glow).text((x, y), text, font=font, fill=ACCENT + (120,))
    img.alpha_composite(glow.filter(ImageFilter.GaussianBlur(9)))

    d = ImageDraw.Draw(img)
    d.rounded_rectangle([2, 2, SIZE - 3, SIZE - 3], radius=RADIUS - 2, outline=BORDER + (255,), width=3)
    d.text((x, y), text, font=font, fill=ACCENT + (255,))

    os.makedirs(OUT_DIR, exist_ok=True)
    img.save(OUT, format="ICO",
             sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    print("wrote", OUT)


if __name__ == "__main__":
    build()
