"""Headless render tests: warm-pool lifecycle under fakes, plus a local_only real-Chrome drive check."""

from pathlib import Path
from types import SimpleNamespace

import pytest

import src.core.artifact_check as artifact_check
import src.core.headless_render as headless_render


@pytest.fixture(autouse=True)
def _fresh_renderer_singleton():
  headless_render._renderer = None
  yield
  if headless_render._renderer is not None:
    headless_render._renderer.close()
  headless_render._renderer = None


def test_render_height_launches_once_and_serves_warm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  renderer = headless_render._WarmRenderer(tmp_path / "chrome")
  launches = []

  def fake_launch(self):
    launches.append(1)
    self._proc, self._ws = SimpleNamespace(poll=lambda: None), SimpleNamespace()

  monkeypatch.setattr(headless_render._WarmRenderer, "_launch", fake_launch)
  monkeypatch.setattr(headless_render._WarmRenderer, "_render_once", lambda self, uri: 800)
  monkeypatch.setattr(headless_render._WarmRenderer, "close", lambda self: None)
  assert renderer.render_height("file:///p.html") == 800
  assert renderer.render_height("file:///p.html") == 800
  assert launches == [1]


@pytest.mark.local_only
def test_warm_renderer_measures_real_page_height(tmp_path: Path) -> None:
  from src.core.config import load_config

  chrome = load_config().headless_chrome_bin
  if not chrome or not Path(chrome).exists():
    pytest.skip("no headless_chrome_bin configured on this host")
  artifact = tmp_path / "page.html"
  artifact.write_text(
      "<html><body>" + "".join(f"<p>line {i}</p>" for i in range(200)) + "</body></html>", encoding="utf-8")
  probe = tmp_path / "probe.html"
  probe.write_text(
      artifact_check._PAGE_PROBE_TEMPLATE.format(
          width=artifact_check._PAGE_PROBE_WIDTH_PX, src=artifact.resolve().as_uri()),
      encoding="utf-8")
  renderer = headless_render._WarmRenderer(Path(chrome))
  try:
    first, second = renderer.render_height(probe.as_uri()), renderer.render_height(probe.as_uri())
  finally:
    renderer.close()
  assert first > 0 and first == second, "the warm render must be deterministic for identical bytes"
