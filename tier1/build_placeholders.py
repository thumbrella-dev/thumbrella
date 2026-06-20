#!/usr/bin/env python3
"""Generate per-kind placeholder thumbnail JPEGs (250×200).

Usage
-----
    python3 tier1/build_placeholders.py --out tier1/assets/placeholders/

Writes one JPEG per media kind plus a generic 'failed' icon:
    image.jpeg  video.jpeg  audio.jpeg  vector.jpeg  document.jpeg
    geometry.jpeg  archive.jpeg  text.jpeg  binary.jpeg  unknown.jpeg  failed.jpeg

Dependencies (install once):
    pip install cairosvg pillow

Icons: Heroicons v2 outline — MIT licence
    https://github.com/tailwindlabs/heroicons
"""

import argparse
import io
import os
import struct
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
            "M3.375 19.5h17.25m-17.25 0a1.125 1.125 0 01-1.125-1.125"
            "M3.375 19.5h1.5C5.496 19.5 6 18.996 6 18.375m-3.75 0V5.625"
            "m0 12.75v-1.5c0-.621.504-1.125 1.125-1.125"
            "m18.375 2.625V5.625m0 12.75c0 .621-.504 1.125-1.125 1.125"
            "m1.125-1.125v-1.5c0-.621-.504-1.125-1.125-1.125"
            "m0 3.75h-1.5A1.125 1.125 0 0118 18.375"
            "M20.625 4.5H3.375m17.25 0c.621 0 1.125.504 1.125 1.125"
            "M20.625 4.5h-1.5C18.504 4.5 18 5.004 18 5.625"
            "m3.75 0v1.5c0 .621-.504 1.125-1.125 1.125"
            "M3.375 4.5c-.621 0-1.125.504-1.125 1.125"
            "M3.375 4.5h1.5C5.496 4.5 6 5.004 6 5.625m-3.75 0v1.5"
            "c0 .621.504 1.125 1.125 1.125m0 0h1.5m-1.5 0"
            "c-.621 0-1.125.504-1.125 1.125v1.5c0 .621.504 1.125 1.125 1.125"
            "m1.5-3.75C5.496 8.25 6 7.746 6 7.125v-1.5"
            "M4.875 8.25C5.496 8.25 6 8.754 6 9.375v1.5m0-5.25v5.25"
            "m0-5.25C6 5.004 6.504 4.5 7.125 4.5h9.75c.621 0 1.125.504 1.125 1.125"
            "m1.125 2.625h1.5m-1.5 0A1.125 1.125 0 0118 7.125v-1.5"
            "m1.125 2.625c-.621 0-1.125.504-1.125 1.125v1.5"
            "m2.625-2.625c.621 0 1.125.504 1.125 1.125v1.5"
            "c0 .621-.504 1.125-1.125 1.125M18 5.625v5.25M7.125 12h9.75"
            "m-9.75 0A1.125 1.125 0 016 10.875"
            "M7.125 12C6.504 12 6 12.504 6 13.125m0-2.25C6 11.496 5.496 12 4.875 12"
            "M18 10.875c0 .621-.504 1.125-1.125 1.125"
            "M18 10.875c0 .621.504 1.125 1.125 1.125m-2.25 0"
            "c.621 0 1.125.504 1.125 1.125m-12 5.25v-5.25m0 5.25"
            "c0 .621.504 1.125 1.125 1.125h9.75c.621 0 1.125-.504 1.125-1.125"
            "m-12 0v-1.5c0-.621-.504-1.125-1.125-1.125M18 18.375v-5.25"
            "m0 5.25v-1.5c0-.621.504-1.125 1.125-1.125M18 13.125v1.5"
            "c0 .621.504 1.125 1.125 1.125M18 13.125c0-.621.504-1.125 1.125-1.125"
            "M6 13.125v1.5c0 .621-.504 1.125-1.125 1.125"
            "M6 13.125C6 12.504 5.496 12 4.875 12m-1.5 0h1.5m-1.5 0"
            "c-.621 0-1.125.504-1.125 1.125v1.5c0 .621.504 1.125 1.125 1.125"
            "M19.125 12h1.5m0 0c.621 0 1.125.504 1.125 1.125v1.5"
            "c0 .621-.504 1.125-1.125 1.125m-17.25 0h1.5m14.25 0h1.5"
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
            "M3.75 3v11.25A2.25 2.25 0 006 16.5h2.25"
            "M3.75 3h-1.5m1.5 0h16.5m0 0h1.5m-1.5 0v11.25"
            "A2.25 2.25 0 0118 16.5h-2.25m-7.5 0h7.5"
            "m-7.5 0l-1 3m8.5-3l1 3m0 0l.5 1.5m-.5-1.5h-9.5"
            "m0 0l-.5 1.5m.75-9l3-3 2.148 2.148"
            "A12.061 12.061 0 0116.5 7.605"
        ),
    ),
    dict(
        name="document",
        color="#395164",   # muted slate  (was #24537A)
        label="DOCUMENT",
        icon=(
            "M12 7.5h1.5m-1.5 3h1.5m-7.5 3h7.5m-7.5 3h7.5"
            "m3-9h3.375c.621 0 1.125.504 1.125 1.125"
            "V18a2.25 2.25 0 01-2.25 2.25"
            "M16.5 7.5V18a2.25 2.25 0 002.25 2.25"
            "M16.5 7.5V4.875c0-.621-.504-1.125-1.125-1.125"
            "H4.125C3.504 3.75 3 4.254 3 4.875V18"
            "a2.25 2.25 0 002.25 2.25h13.5M6 7.5h3v3H6v-3z"
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
        icon=(
            "M19.5 14.25v-2.625a3.375 3.375 0 00-3.375-3.375h-1.5"
            "A1.125 1.125 0 0113.5 7.125v-1.5"
            "a3.375 3.375 0 00-3.375-3.375H8.25"
            "m0 12.75h7.5m-7.5 3H12"
            "M10.5 2.25H5.625c-.621 0-1.125.504-1.125 1.125v17.25"
            "c0 .621.504 1.125 1.125 1.125h12.75"
            "c.621 0 1.125-.504 1.125-1.125V11.25a9 9 0 00-9-9z"
        ),
    ),
    dict(
        name="binary",
        color="#263E35",   # muted dark green  (was #1A4A38)
        label="BINARY",
        icon=(
            "M20.25 6.375c0 2.278-3.694 4.125-8.25 4.125"
            "S3.75 8.653 3.75 6.375m16.5 0"
            "c0-2.278-3.694-4.125-8.25-4.125S3.75 4.097 3.75 6.375"
            "m16.5 0v11.25c0 2.278-3.694 4.125-8.25 4.125"
            "s-8.25-1.847-8.25-4.125V6.375m16.5 0v3.75"
            "m-16.5-3.75v3.75m16.5 0v3.75"
            "C20.25 16.153 16.556 18 12 18"
            "s-8.25-1.847-8.25-4.125v-3.75m16.5 0"
            "c0 2.278-3.694 4.125-8.25 4.125s-8.25-1.847-8.25-4.125"
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
        label="",          # no label — the error icon is self-explanatory
        fg="#B83838",       # red icon on grey (inverted from the others)
        # x-circle: error/close indicator
        icon=(
            "m9.75 9.75 4.5 4.5m0-4.5-4.5 4.5"
            "M21 12a9 9 0 11-18 0 9 9 0 0118 0z"
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
    """Render SVG → PNG via Cairo → JPEG via Pillow, then inject EXIF comment."""
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
    return inject_exif_comment(buf.getvalue(), "thumbrella.dev")


def inject_exif_comment(jpeg: bytes, description: str) -> bytes:
    """Splice an EXIF APP1 segment with metadata into the JPEG stream
    immediately after SOI (FFD8).

    Tags written (all in IFD0, big-endian):
      Software, Orientation=Normal, XResolution=72, YResolution=72,
      ResolutionUnit=inches.
    """
    sw = b"thumbrella.dev\x00"

    # ── TIFF header (big-endian) ───────────────────────────────────────────
    tiff = bytearray()
    tiff += b"MM"
    tiff += struct.pack(">H", 0x002A)  # magic
    tiff += struct.pack(">I", 8)       # offset to 0th IFD

    # ── IFD0: 4 entries ───────────────────────────────────────────────────
    tiff += struct.pack(">H", 4)

    # Offsets for value data living past the IFD table
    # header(8) + count(2) + 4*12(48) + next_ifd(4) = 62
    data_base = 8 + 2 + 4 * 12 + 4
    sw_off = data_base
    xres_off = sw_off + len(sw)
    yres_off = xres_off + 8  # two LONGs

    # Software          tag=0x0131  type=ASCII(2)
    tiff += struct.pack(">HHII", 0x0131, 2, len(sw), sw_off)
    # XResolution       tag=0x011A  type=RATIONAL(5)  72/1
    tiff += struct.pack(">HHII", 0x011A, 5, 1, xres_off)
    # YResolution       tag=0x011B  type=RATIONAL(5)  72/1
    tiff += struct.pack(">HHII", 0x011B, 5, 1, yres_off)
    # ResolutionUnit    tag=0x0128  type=SHORT(3)  val=2 (inches, inline)
    tiff += struct.pack(">HHII", 0x0128, 3, 1, 2 << 16)

    # Next IFD
    tiff += struct.pack(">I", 0)

    # ── Value data ─────────────────────────────────────────────────────────
    tiff += sw
    tiff += struct.pack(">II", 72, 1)  # XResolution 72/1
    tiff += struct.pack(">II", 72, 1)  # YResolution 72/1

    # ── APP1 wrapper ───────────────────────────────────────────────────────
    payload = 6 + len(tiff)
    app1 = b"\xff\xe1"
    app1 += struct.pack(">H", payload)
    app1 += b"Exif\x00\x00"
    app1 += bytes(tiff)

    # ── Splice ─────────────────────────────────────────────────────────────
    if len(jpeg) < 2 or jpeg[:2] != b"\xff\xd8":
        return jpeg
    return jpeg[:2] + app1 + jpeg[2:]


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
        out_path = os.path.join(args.out, f"{spec['name']}.jpeg")
        with open(out_path, "wb") as fh:
            fh.write(data)
        print(f"  {out_path}  ({len(data):,} bytes)")

    print(f"\ndone — {len(SPECS)} placeholders written to {args.out}")


if __name__ == "__main__":
    main()
