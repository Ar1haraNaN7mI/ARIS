"""Render original ARIS geometry as vector, Windows and web icons (Pillow)."""
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
POLYGONS = [
    [(48, 10), (8, 88), (25, 88), (56, 27)],
    [(61, 36), (52, 54), (70, 88), (88, 88)],
    [(39, 63), (32, 77), (66, 77), (59, 63)],
    [(28, 88), (63, 88), (67, 96), (24, 96)],
]
SVG = '''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" fill="#168ba8" role="img" aria-labelledby="title desc">
  <title id="title">ARIS</title>
  <desc id="desc">An original angular A, formed by separate paths and a shared crossbar, above an open foundation.</desc>
  <path d="M48 10 8 88h17l31-61Z"/>
  <path d="m61 36-9 18 18 34h18Z"/>
  <path d="m39 63-7 14h34l-7-14Z"/>
  <path opacity=".5" d="M28 88h35l4 8H24Z"/>
</svg>
'''

def render(size=512):
    canvas = Image.new("RGBA", (size, size))
    pen = ImageDraw.Draw(canvas)
    pen.rounded_rectangle((0, 0, size-1, size-1), radius=size*0.22, fill="#eef8fb")
    for index, polygon in enumerate(POLYGONS):
        points = [((x*0.78+11)*size/100, (y*0.78+9)*size/100) for x,y in polygon]
        pen.polygon(points, fill="#168ba8" if index < 3 else "#87c5d4")
    return canvas

if __name__ == "__main__":
    source = render(1024)
    for name in ("aris-desktop.ico", "aris-laterano-desktop.ico"):
        source.save(ROOT / "docs/assets" / name, sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    rgba = source.resize((128, 128), Image.Resampling.LANCZOS).tobytes()
    for name in ("icon.rgba", "icon-laterano.rgba"):
        (ROOT / "crates/aris-desktop/assets" / name).write_bytes(rgba)
    public = ROOT / "web/public"
    public.mkdir(exist_ok=True)
    source.resize((256, 256), Image.Resampling.LANCZOS).save(public / "aris-icon.png")
    for target in (public / "aris.svg", ROOT / "docs/assets/aris-emblem.svg", ROOT / "docs/assets/aris-laterano-emblem.svg"):
        target.write_text(SVG, encoding="utf-8")
