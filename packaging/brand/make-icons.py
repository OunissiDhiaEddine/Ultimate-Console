"""Regenerates every logo raster from the two SVG sources in this folder.

    python3 packaging/brand/make-icons.py

Needs Python 3 with Pillow, and Node with Playwright + Chromium (used only to
rasterise the SVGs). Outputs, all committed so builds never render anything:

- packaging/icon.ico            installer, shortcuts and every .exe (build.rs)
- packaging/brand/icon-64.rgba  raw 64x64 RGBA for the settings window icon
- site/assets/logo.svg, favicon.svg, logo.png (og:image), favicon.png
"""

import io
import json
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
BIG = HERE / "logo.svg"
SMALL = HERE / "logo-small.svg"  # thicker strokes, for 32 px and below

RENDER_JS = r"""
const { chromium } = require('playwright');
const fs = require('fs');
(async () => {
  const jobs = JSON.parse(process.argv[2]);
  const b = await chromium.launch();
  for (const [svg, out, size] of jobs) {
    const p = await b.newPage({ viewport: { width: size, height: size } });
    const src = fs.readFileSync(svg, 'utf8').replace('<svg ', `<svg width="${size}" height="${size}" `);
    await p.setContent(`<html><body style="margin:0">${src}</body></html>`);
    await p.screenshot({ path: out, omitBackground: true, clip: { x: 0, y: 0, width: size, height: size } });
    await p.close();
  }
  await b.close();
})();
"""


def render(jobs):
    with tempfile.TemporaryDirectory() as tmp:
        js = Path(tmp) / "render.js"
        js.write_text(RENDER_JS)
        subprocess.run(["node", str(js), json.dumps(jobs)], check=True)


def write_ico(path, pngs):
    """ICO with PNG-compressed frames (fine on Vista and later)."""
    blobs = []
    for png in pngs:
        buf = io.BytesIO()
        Image.open(png).save(buf, "PNG", optimize=True)
        blobs.append((Image.open(png).width, buf.getvalue()))
    out = struct.pack("<HHH", 0, 1, len(blobs))
    offset = 6 + 16 * len(blobs)
    for size, data in blobs:
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    path.write_bytes(out + b"".join(d for _, d in blobs))


def main():
    with tempfile.TemporaryDirectory() as tmp:
        t = Path(tmp)
        sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
        jobs = [[str(SMALL if s <= 32 else BIG), str(t / f"{s}.png"), s] for s in sizes]
        jobs.append([str(BIG), str(t / "512.png"), 512])
        render(jobs)

        write_ico(ROOT / "packaging" / "icon.ico", [t / f"{s}.png" for s in sizes])
        (HERE / "icon-64.rgba").write_bytes(Image.open(t / "64.png").convert("RGBA").tobytes())

        assets = ROOT / "site" / "assets"
        shutil.copy(BIG, assets / "logo.svg")
        shutil.copy(SMALL, assets / "favicon.svg")
        Image.open(t / "512.png").save(assets / "logo.png", optimize=True)
        Image.open(t / "64.png").save(assets / "favicon.png", optimize=True)
    print("icons regenerated", file=sys.stderr)


if __name__ == "__main__":
    main()
