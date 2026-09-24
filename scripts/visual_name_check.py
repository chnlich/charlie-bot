"""Pixel-level readability check for the session-tree screenshots.

The harness already measures live DOM geometry (name/badge bounding boxes).
This companion check looks at the actual SCREENSHOT pixels: it locates each
tree row's bright name-glyph band inside the sidebar column, measures how much
horizontal space the painted name occupies, verifies the secondary badge band
renders below it, and exports zoomed crops for human review.

Usage: uv run --with pillow python scripts/visual_name_check.py <evidence_dir>
Exits non-zero if any expectation fails. Pure analysis — no browser, no server.
"""
import json
import sys
from pathlib import Path

from PIL import Image

SIDEBAR_W = 320
NAME_LUM = 200      # name text renders near-white (slate-100/200) on the dark sidebar
BADGE_LUM_LO = 90   # badges are dim slate-400/500 text
BADGE_LUM_HI = 195


def lum(px):
    r, g, b = px[0], px[1], px[2]
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def row_bands(img, x0, x1, y0, y1):
    """Horizontal strips of the sidebar that contain bright (name) pixels."""
    px = img.load()
    bands = []
    current = None
    for y in range(y0, y1):
        has_bright = any(lum(px[x, y]) >= NAME_LUM for x in range(x0, x1, 2))
        if has_bright:
            if current is None:
                current = [y, y]
            else:
                current[1] = y
        else:
            # A gap of >=3 dark rows ends a band (glyph gaps inside a line are 1-2px).
            if current is not None and y - current[1] >= 3:
                if current[1] - current[0] >= 6:  # a text line is ~10-14px tall
                    bands.append(tuple(current))
                current = None
    if current is not None and current[1] - current[0] >= 6:
        bands.append(tuple(current))
    return bands


def band_metrics(img, x0, x1, band):
    px = img.load()
    xs = []
    count = 0
    for y in range(band[0], band[1] + 1):
        for x in range(x0, x1):
            if lum(px[x, y]) >= NAME_LUM:
                xs.append(x)
                count += 1
    badge_count = 0
    badge_ys = []
    for y in range(band[1] + 1, min(band[1] + 26, img.height)):
        for x in range(x0, x1):
            pixel_lum = lum(px[x, y])
            if BADGE_LUM_LO <= pixel_lum <= BADGE_LUM_HI:
                badge_count += 1
                badge_ys.append(y)
    return {
        "band_y": list(band),
        "name_extent_px": (max(xs) - min(xs)) if xs else 0,
        "name_left_px": min(xs) if xs else None,
        "name_pixel_count": count,
        "badge_pixels_below": badge_count,
        "badge_band_below": (min(badge_ys), max(badge_ys)) if badge_ys else None,
    }


def main() -> None:
    evidence = Path(sys.argv[1])
    report = {}
    failures = []

    for shot, label, expect_rows in (
        ("s20_desktop_readability.png", "desktop", 6),
        ("s20b_mobile_readability.png", "mobile-390", 4),
    ):
        path = evidence / shot
        if not path.exists():
            failures.append(f"{shot}: missing")
            continue
        img = Image.open(path).convert("RGB")
        scale = img.width / 1440 if label == "desktop" else img.width / 390
        sb_x1 = min(img.width, int(SIDEBAR_W * scale))
        # Skip the sidebar header (search box etc.): the tree list starts below ~120 css px.
        bands = row_bands(img, 8, sb_x1 - 4, int(120 * scale), img.height - 10)
        metrics = [band_metrics(img, 8, sb_x1 - 4, b) for b in bands]
        report[label] = {"shot": shot, "size": list(img.size), "rows": metrics}
        wide = [m for m in metrics if m["name_extent_px"] >= 100 * scale]
        wrapped = [m for m in metrics if m["badge_pixels_below"] > 40]
        print(f"{label}: {len(metrics)} name bands; {len(wide)} with name extent >= {int(100 * scale)}px; "
              f"{len(wrapped)} rows with a badge band below the name")
        for m in metrics[:12]:
            print(f"   y={m['band_y'][0]}-{m['band_y'][1]} name_extent={m['name_extent_px']}px "
                  f"left={m['name_left_px']} glyphs={m['name_pixel_count']} badge_px_below={m['badge_pixels_below']}")
        if len(metrics) < expect_rows:
            failures.append(f"{label}: only {len(metrics)} readable name bands (expected >= {expect_rows})")
        if len(wide) < 3:
            failures.append(f"{label}: fewer than 3 rows keep a >= {int(100 * scale)}px visible name")
        if not wrapped:
            failures.append(f"{label}: no row shows its badge band on a second line")
        # Export 3x zoomed crops of the first bands for human review.
        crop_h = int(46 * scale)
        crop = img.crop((0, int(110 * scale), sb_x1, min(img.height, int(110 * scale) + crop_h * 10)))
        crop = crop.resize((crop.width * 2, crop.height * 2), Image.LANCZOS)
        out = evidence / f"{shot.replace('.png', '')}_sidebar_crop.png"
        crop.save(out)
        report[label]["crop"] = out.name

    # Every scenario screenshot must be a real rendering (non-blank, non-flat).
    for path in sorted(evidence.glob("s*.png")):
        if path.name.endswith("_crop.png"):
            continue
        img = Image.open(path).convert("RGB").resize((200, 120))
        colors = len(set(img.getdata()))
        if colors < 50:
            failures.append(f"{path.name}: looks blank/flat ({colors} colors)")

    (evidence / "visual_name_check.json").write_text(json.dumps(report, indent=2))
    if failures:
        print("VISUAL CHECK FAILED:")
        for f in failures:
            print(" -", f)
        sys.exit(1)
    print("VISUAL CHECK PASSED")


if __name__ == "__main__":
    main()
