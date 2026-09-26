"""Draws the app icon (assets/app.ico, assets/icon-512.png).

A dark squircle like the overlay capsule, with a live-waveform mark: white bars and a red
recording dot. Drawn at 4x and downsampled for clean edges. Run: python assets/make_icon.py
"""
from PIL import Image, ImageDraw
import os

HERE = os.path.dirname(os.path.abspath(__file__))
TOP, BOTTOM = (0x2B, 0x2B, 0x30), (0x14, 0x14, 0x17)
INK, RED = (0xF5, 0xF5, 0xF7), (0xFF, 0x45, 0x3A)


def draw(size, simple=False):
    s = size * 4
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    # Vertical gradient clipped to a rounded square.
    grad = Image.new("RGBA", (s, s))
    gd = ImageDraw.Draw(grad)
    for y in range(s):
        t = y / (s - 1)
        gd.line([(0, y), (s, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,))
    mask = Image.new("L", (s, s), 0)
    inset = round(s * 0.04)
    ImageDraw.Draw(mask).rounded_rectangle([inset, inset, s - inset, s - inset], radius=round(s * 0.24), fill=255)
    img.paste(grad, (0, 0), mask)
    d = ImageDraw.Draw(img)
    # Soft top highlight edge.
    d.rounded_rectangle([inset, inset, s - inset, s - inset], radius=round(s * 0.24),
                        outline=(255, 255, 255, 34), width=max(4, s // 96))

    # Mark: red dot + waveform bars, centred as a group.
    heights = [0.22, 0.46, 0.34] if simple else [0.20, 0.40, 0.58, 0.36, 0.24]
    bar_w = s * (0.085 if simple else 0.062)
    gap = s * (0.07 if simple else 0.052)
    dot_r = s * (0.075 if simple else 0.062)
    dot_gap = s * (0.09 if simple else 0.07)
    total = 2 * dot_r + dot_gap + len(heights) * bar_w + (len(heights) - 1) * gap
    x = (s - total) / 2
    cy = s / 2
    d.ellipse([x, cy - dot_r, x + 2 * dot_r, cy + dot_r], fill=RED + (255,))
    x += 2 * dot_r + dot_gap
    for h in heights:
        hh = s * h
        d.rounded_rectangle([x, cy - hh / 2, x + bar_w, cy + hh / 2], radius=bar_w / 2, fill=INK + (255,))
        x += bar_w + gap
    return img.resize((size, size), Image.LANCZOS)


if __name__ == "__main__":
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    frames = [draw(n, simple=n <= 24) for n in sizes]
    frames[-1].save(os.path.join(HERE, "app.ico"), sizes=[(n, n) for n in sizes],
                    append_images=frames[:-1])
    draw(512).save(os.path.join(HERE, "icon-512.png"))
    print("ok")
