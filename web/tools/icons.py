"""The installed app's icons (public/icons): the Piksel sun (as public/favicon.svg draws it) on the
page's near-black, drawn at 4x and scaled down so the squares' corners are smooth. Needs Pillow.

    python3 tools/icons.py            # from web/
"""
from pathlib import Path

from PIL import Image, ImageDraw

SUN = ['..####..', '.######.', '########', '########', '########', '########', '.######.', '..####..']
BG, ACCENT = (10, 10, 12, 255), (255, 122, 69, 255)
OUT = Path(__file__).resolve().parent.parent / 'public' / 'icons'


def icon(size: int, mark: float, background=BG) -> Image.Image:
    """A square icon with the sun `mark` of its side wide (the 48 field's 43.5 drawn), centred."""
    big = size * 4
    im = Image.new('RGBA', (big, big), background)
    draw = ImageDraw.Draw(im)
    unit = big * mark / 43.5
    left = (big - 43.5 * unit) / 2 - 2 * unit
    for r, row in enumerate(SUN):
        for k, c in enumerate(row):
            if c != '#':
                continue
            x, y = left + (2 + k * 5.5) * unit, left + (2 + r * 5.5) * unit
            draw.rounded_rectangle((x, y, x + 5 * unit, y + 5 * unit), radius=0.8 * unit, fill=ACCENT)
    return im.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    # "any": the sun with a little room; "maskable": inside the 80 % circle a launcher may cut to.
    icon(192, 0.72).save(OUT / 'icon-192.png', optimize=True)
    icon(512, 0.72).save(OUT / 'icon-512.png', optimize=True)
    icon(512, 0.50).save(OUT / 'maskable-512.png', optimize=True)
    icon(180, 0.62).save(OUT / 'apple-touch-icon.png', optimize=True)


if __name__ == '__main__':
    main()
