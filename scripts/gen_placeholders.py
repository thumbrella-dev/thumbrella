#!/usr/bin/env python3
"""Generate per-kind placeholder thumbnail JPEGs (250×200).

Usage
-----
    python3 scripts/gen_placeholders.py --out crates/tier1/assets/placeholders/

Writes one JPEG per media kind plus a generic 'failed' icon:
    image.jpg  video.jpg  audio.jpg  vector.jpg  document.jpg
    geometry.jpg  archive.jpg  text.jpg  binary.jpg  unknown.jpg  failed.jpg

Dependencies (install once):
    pip install cairosvg pillow

Icons: Heroicons v2 outline — MIT licence
    https://github.com/tailwindlabs/heroicons
"""

import argparse
import io
import os
import sys

import cairosvg
from PIL import Image, ImageFilter

# ── Canvas ──────────────────────────────────────────────────────────────────
W, H = 250, 200
DEFAULT_QUALITY = 62

# ── Per-kind spec ────────────────────────────────────────────────────────────
# color    – background hex; all media kinds desaturated ~50% via HSL
#            (keeps colour family legible without competing with the icon).
#            "failed" retains its richer hue — it signals a process error,
#            not a media kind, and warrants visual distinction.
# label    – short uppercase string rendered below the icon
# icon     – Heroicons v2 outline path data  (24 × 24 viewBox, stroke icons)
SPECS = [
    dict(
        name="image",
        color="#546B83",   # steel blue-grey  (was #3C6B9A)
        label="IMAGE",
        icon=(
            "M2.25 15.75l5.159-5.159a2.25 2.25 0 013.182 0l5.159 5.159"
            "m-1.5-1.5l1.409-1.409a2.25 2.25 0 013.182 0l2.909 2.909"
            "m-18 3.75h16.5a1.5 1.5 0 001.5-1.5V6a1.5 1.5 0 00-1.5-1.5"
            "H3.75A1.5 1.5 0 002.25 6v12a1.5 1.5 0 001.5 1.5z"
            "m10.5-11.25h.008v.008h-.008V8.25z"
            "m.375 0a.375.375 0 11-.75 0 .375.375 0 01.75 0z"
        ),
    ),
    dict(
        name="video",
        color="#594A75",   # muted purple  (was #52348A)
        label="VIDEO",
        icon=(
            "M15.75 10.5l4.72-4.72a.75.75 0 011.28.53v11.38"
            "a.75.75 0 01-1.28.53l-4.72-4.72"
            "M4.5 18.75h9a2.25 2.25 0 002.25-2.25v-9"
            "a2.25 2.25 0 00-2.25-2.25h-9"
            "A2.25 2.25 0 002.25 7.5v9a2.25 2.25 0 002.25 2.25z"
        ),
    ),
    dict(
        name="audio",
        color="#2F5959",   # muted teal  (was #1A6E6E)
        label="AUDIO",
        icon=(
            "M9 19V6l12-3v13"
            "M9 19c0 1.105-1.343 2-3 2s-3-.895-3-2 1.343-2 3-2 3 .895 3 2z"
            "m12-3c0 1.105-1.343 2-3 2s-3-.895-3-2 1.343-2 3-2 3 .895 3 2z"
            "M9 10l12-3"
        ),
    ),
    dict(
        name="vector",
        color="#60432B",   # muted amber-brown  (was #7A4010)
        label="VECTOR",
        icon=(
            "M16.862 4.487l1.687-1.688a1.875 1.875 0 112.652 2.652"
            "L6.832 19.82a4.5 4.5 0 01-1.897 1.13l-2.685.8"
            ".8-2.685a4.5 4.5 0 011.13-1.897L16.863 4.487zm0 0L19.5 7.125"
        ),
    ),
    dict(
        name="document",
        color="#395164",   # muted slate  (was #24537A)
        label="DOCUMENT",
        icon=(
            "M19.5 14.25v-2.625a3.375 3.375 0 00-3.375-3.375h-1.5"
            "A1.125 1.125 0 0113.5 7.125v-1.5"
            "a3.375 3.375 0 00-3.375-3.375H8.25"
            "m2.25 0H5.625c-.621 0-1.125.504-1.125 1.125v17.25"
            "c0 .621.504 1.125 1.125 1.125h12.75"
            "c.621 0 1.125-.504 1.125-1.125V11.25a9 9 0 00-9-9z"
        ),
    ),
    dict(
        name="geometry",
        color="#4F4065",   # muted violet  (was #4A2E78)
        label="3D",
        icon=(
            "M21 7.5l-9-5.25L3 7.5m18 0l-9 5.25m9-5.25v9l-9 5.25"
            "M3 7.5l9 5.25M3 7.5v9l9 5.25m0-9v9"
        ),
    ),
    dict(
        name="archive",
        color="#4C3C29",   # muted brown  (was #5E3E18)
        label="ARCHIVE",
        icon=(
            "M20.25 7.5l-.625 10.632a2.25 2.25 0 01-2.247 2.118H6.622"
            "a2.25 2.25 0 01-2.247-2.118L3.75 7.5M10 11.25h4"
            "M3.375 7.5h17.25c.621 0 1.125-.504 1.125-1.125v-1.5"
            "c0-.621-.504-1.125-1.125-1.125H3.375"
            "c-.621 0-1.125.504-1.125 1.125v1.5c0 .621.504 1.125 1.125 1.125z"
        ),
    ),
    dict(
        name="text",
        color="#404D56",   # muted blue-grey  (was #344E62)
        label="TEXT",
        icon="M3.75 6.75h16.5M3.75 12h16.5m-16.5 5.25h16.5",
    ),
    dict(
        name="binary",
        color="#263E35",   # muted dark green  (was #1A4A38)
        label="BINARY",
        icon=(
            "M17.25 6.75L22.5 12l-5.25 5.25"
            "m-10.5 0L1.5 12l5.25-5.25"
            "m7.5-3l-4.5 16.5"
        ),
    ),
    dict(
        name="unknown",
        color="#484853",   # near-neutral grey  (was #424258)
        label="UNKNOWN",
        icon=(
            "M9.879 7.519c1.171-1.025 3.071-1.025 4.242 0"
            " 1.172 1.025 1.172 2.687 0 3.712"
            "-.203.179-.43.326-.67.442"
            "-.745.361-1.45.999-1.45 1.827v.75"
            "M21 12a9 9 0 11-18 0 9 9 0 0118 0z"
            "m-9 5.25h.008v.008H12v-.008z"
        ),
    ),
    dict(
        name="failed",
        color="#3D3D3D",   # neutral dark grey — deliberately dull; not a media kind
        label="",          # no label — the warning triangle is self-explanatory
        fg="#B83838",       # red icon on grey (inverted from the others)
        # exclamation-triangle: conveys process/system failure rather than
        # a media-content category
        icon=(
            "M12 9v3.75"
            "m-9.303 3.376c-.866 1.5.217 3.374 1.948 3.374h14.71"
            "c1.73 0 2.813-1.874 1.948-3.374L13.949 3.378"
            "c-.866-1.5-3.032-1.5-3.898 0L2.697 16.126z"
            "M12 15.75h.007v.008H12v-.008z"
        ),
    ),
]

# ── SVG template ─────────────────────────────────────────────────────────────
# Icon group: 24×24 → 108×108 (scale=4.5), centered horizontally at x=125.
# Icon vertical center at y=84 → translate(71, 30).
# Stroke-width compensated for scale: 1.0 × 4.5 = 4.5 px rendered
# (same visual weight as the original 1.5 × 3 = 4.5 px).
# Label at y=158, font-size=17.

_SVG_TMPL = """\
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}">
  <defs>
    <radialGradient id="vg" cx="50%" cy="44%" r="68%">
      <stop offset="0%"   stop-color="white" stop-opacity="0.1"/>
      <stop offset="100%" stop-color="black" stop-opacity="0.2"/>
    </radialGradient>
  </defs>
  <!-- Solid background -->
  <rect width="{w}" height="{h}" fill="{color}"/>
  <!-- Radial vignette overlay -->
  <rect width="{w}" height="{h}" fill="url(#vg)"/>
  <!-- Icon: Heroicons outline, scaled 4.5x from 24px, centred at (125, 84) -->
  <g transform="translate(71, 30) scale(4.5)">
    <path d="{icon}"
      fill="none" stroke="{fg}" stroke-width="1.0"
      stroke-linecap="round" stroke-linejoin="round" stroke-opacity="0.8"/>
  </g>
  <!-- Kind label -->
{text_el}</svg>
"""


def build_svg(color: str, label: str, icon: str, fg: str = "white") -> str:
    text_el = (
        f'  <text x="125" y="168"\n'
        f'    text-anchor="middle"\n'
        f'    fill="{fg}" fill-opacity="0.6"\n'
        f'    font-size="17" font-family="sans-serif"\n'
        f'    font-weight="600" letter-spacing="2.5">{label}</text>\n'
    ) if label else ""
    return _SVG_TMPL.format(w=W, h=H, color=color, icon=icon, fg=fg, text_el=text_el)


def render_jpeg(svg_text: str, quality: int) -> bytes:
    """Render SVG → PNG via Cairo → JPEG via Pillow."""
    png = cairosvg.svg2png(
        bytestring=svg_text.encode(),
        output_width=W,
        output_height=H,
    )
    img = Image.open(io.BytesIO(png)).convert("RGB")
    # Unsharp mask: wide radius creates a visible glow/halo around icon
    # strokes that marries them into the background; high amplitude keeps
    # the effect punchy on these fully-controlled flat-colour images.
    img = img.filter(ImageFilter.UnsharpMask(radius=8, percent=80, threshold=1))
    buf = io.BytesIO()
    img.save(buf, "JPEG", quality=quality, optimize=True, subsampling=0)
    return buf.getvalue()


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Generate per-kind placeholder thumbnail JPEGs."
    )
    ap.add_argument("--out", required=True, help="Output directory")
    ap.add_argument(
        "--quality", type=int, default=DEFAULT_QUALITY, help="JPEG quality (default 82)"
    )
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)

    for spec in SPECS:
        svg = build_svg(spec["color"], spec["label"], spec["icon"], spec.get("fg", "white"))
        data = render_jpeg(svg, args.quality)
        out_path = os.path.join(args.out, f"{spec['name']}.jpg")
        with open(out_path, "wb") as fh:
            fh.write(data)
        print(f"  {out_path}  ({len(data):,} bytes)")

    print(f"\ndone — {len(SPECS)} placeholders written to {args.out}")


if __name__ == "__main__":
    main()
