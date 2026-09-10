"""Every ``toLocale*`` call in the browser surfaces pins ``'en-US'``.

The UI is single-user English, so locale-undefined formatting renders differently
per browser environment: date strings, thousand separators, and decimal points all
follow the visitor's locale. The pin lives at each call site; this contract keeps
a new call from landing unpinned.
"""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WEB_GLOBS = ("web/templates/**/*.html", "web/static/js/**/*.js")


def test_every_tolocale_call_under_web_pins_en_us() -> None:
  sources = [path for pattern in WEB_GLOBS for path in sorted(ROOT.glob(pattern))]
  assert sources, "no web sources found to check"
  unpinned = []
  for path in sources:
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
      if "toLocale" in line and "'en-US'" not in line:
        unpinned.append(f"{path.relative_to(ROOT)}:{number}: {line.strip()}")
  assert not unpinned, "toLocale call without the 'en-US' pin:\n" + "\n".join(unpinned)
