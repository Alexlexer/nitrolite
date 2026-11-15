"""Generate the nitrolite icon.

Draws the same motif as the TUI's cooling panel: a dark disc, a red LED ring and
three swept blades. Rendered at 4x and downsampled so the 16 px tray size stays
legible.

    python tools/make_icon.py

Outputs assets/nitrolite.ico (16-256 px) and assets/nitrolite-256.png.
"""

from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

SS = 4  # supersampling factor
BASE = 256
SIZE = BASE * SS

BG = (14, 15, 20, 255)        # near-black, matches the TUI panels
BLADE = (232, 232, 240, 255)  # off-white blades
BLADE_SHADE = (176, 178, 196, 255)
RING = (255, 59, 59, 255)     # LED red
HUB = (255, 59, 59, 255)
CLEAR = (0, 0, 0, 0)


def build() -> Image.Image:
    n = SIZE
    yy, xx = np.mgrid[0:n, 0:n].astype(np.float32)
    cx = cy = (n - 1) / 2.0

    # Normalised radius (1.0 == disc edge) and angle.
    fx = (xx - cx) / (n / 2.0)
    fy = (yy - cy) / (n / 2.0)
    r = np.sqrt(fx * fx + fy * fy)
    theta = np.arctan2(fy, fx)

    disc = r <= 0.98
    ring = (r > 0.84) & (r <= 0.98)
    hub = r <= 0.16

    # Three blades with a spiralled trailing edge, same formula as the TUI.
    blades = (r > 0.18) & (r <= 0.82) & (np.sin(3.0 * theta + 1.1 * r) > 0.35)

    # Blade shading: lighter at the leading edge.
    shade = np.sin(3.0 * theta + 1.1 * r)

    img = Image.new("RGBA", (n, n), CLEAR)
    px = np.zeros((n, n, 4), dtype=np.uint8)

    px[disc] = BG
    px[blades & (shade > 0.7)] = BLADE
    px[blades & (shade <= 0.7)] = BLADE_SHADE
    px[hub] = HUB
    px[ring] = RING

    # Thin dark outline inside the ring so the blades read as separate from it.
    inner = (r > 0.80) & (r <= 0.84)
    px[inner] = BG

    img = Image.fromarray(px, "RGBA")

    # Soft red glow just outside the disc, like the TUI's LED bleed.
    glow = Image.new("RGBA", (n, n), CLEAR)
    gd = ImageDraw.Draw(glow)
    pad = int(SIZE * 0.02)
    gd.ellipse(
        [pad, pad, SIZE - pad - 1, SIZE - pad - 1],
        outline=(255, 59, 59, 90),
        width=int(SIZE * 0.012),
    )
    img = Image.alpha_composite(img, glow)
    return img


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    assets = root / "assets"
    assets.mkdir(exist_ok=True)

    big = build()
    master = big.resize((BASE, BASE), Image.LANCZOS)
    master.save(assets / "nitrolite-256.png")

    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [master.resize((s, s), Image.LANCZOS) for s in sizes]
    frames[-1].save(
        assets / "nitrolite.ico",
        format="ICO",
        sizes=[(s, s) for s in sizes],
        append_images=frames[:-1],
    )
    print("wrote", assets / "nitrolite.ico", "and", assets / "nitrolite-256.png")


if __name__ == "__main__":
    main()
