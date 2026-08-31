"""Static guards for the scrollbar compositor rules.

Custom ``::-webkit-scrollbar`` rules make Chrome move every scroller's
scrolling onto the renderer main thread; the standard properties
(``scrollbar-width`` / ``scrollbar-color``) keep it on the compositor.
These tests pin the two style sources to the standard-property form;
the scrolling mechanism itself is asserted by scripts/web_scroll_probe.py.
"""
from __future__ import annotations

from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
STYLES_CSS = ROOT / 'web' / 'static' / 'css' / 'styles.css'
EVENTS_VIEWER = ROOT / 'web' / 'templates' / 'events_viewer.html'


def test_no_webkit_scrollbar_rules_anywhere() -> None:
  """::-webkit-scrollbar must not appear in styles.css or any template."""
  files = [STYLES_CSS, *sorted((ROOT / 'web' / 'templates').glob('*.html'))]
  offenders = [
      str(f.relative_to(ROOT)) for f in files
      if '::-webkit-scrollbar' in f.read_text(encoding='utf-8')
  ]
  assert offenders == [], (
      f'::-webkit-scrollbar rules put scrolling on the main thread; found in: {offenders}'
  )


@pytest.mark.parametrize('path', [STYLES_CSS, EVENTS_VIEWER], ids=lambda p: str(p.relative_to(ROOT)))
def test_standard_scrollbar_properties_present_once(path: Path) -> None:
  """Each style source carries exactly one standard-property scrollbar rule."""
  text = path.read_text(encoding='utf-8')
  assert text.count('scrollbar-width: thin') == 1, f'{path}: scrollbar-width: thin expected once'
  assert text.count('scrollbar-color: #475569 transparent') == 1, (
      f'{path}: scrollbar-color: #475569 transparent expected once'
  )
