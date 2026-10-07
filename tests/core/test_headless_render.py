"""Headless render tests: warm-pool lifecycle under fakes, plus a local_only real-Chrome drive check."""

import pathlib
import types
from collections.abc import Iterator

import pytest

from src.features.artifacts import artifact_check, headless_render


@pytest.fixture(autouse=True)
def _fresh_renderer_singleton() -> Iterator[None]:
  headless_render._renderer = None
  yield
  if headless_render._renderer is not None:
    headless_render._renderer.close()
  headless_render._renderer = None


def test_render_height_launches_once_and_serves_warm(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  renderer = headless_render._WarmRenderer(tmp_path / "chrome")
  launches = []

  def fake_launch(self) -> None:
    launches.append(1)
    self._proc, self._ws = types.SimpleNamespace(poll=lambda: None), types.SimpleNamespace()

  monkeypatch.setattr(headless_render._WarmRenderer, "_launch", fake_launch)
  monkeypatch.setattr(headless_render._WarmRenderer, "_render_once", lambda self, uri: 800)
  monkeypatch.setattr(headless_render._WarmRenderer, "close", lambda self: None)
  assert renderer.render_height("file:///p.html") == 800
  assert renderer.render_height("file:///p.html") == 800
  assert launches == [1]


@pytest.mark.local_only
def test_warm_renderer_measures_real_page_height(tmp_path: pathlib.Path) -> None:
  from src.infra import config

  chrome = config.load_config().headless_chrome_bin
  if not chrome or not pathlib.Path(chrome).exists():
    pytest.skip("no headless_chrome_bin configured on this host")
  artifact = tmp_path / "page.html"
  artifact.write_text(
      "<html><body>" + "".join(f"<p>line {i}</p>" for i in range(200)) + "</body></html>", encoding="utf-8")
  probe = tmp_path / "probe.html"
  probe.write_text(
      artifact_check._PAGE_PROBE_TEMPLATE.format(
          width=artifact_check._PAGE_PROBE_WIDTH_PX, src=artifact.resolve().as_uri()),
      encoding="utf-8")
  renderer = headless_render._WarmRenderer(pathlib.Path(chrome))
  try:
    first, second = renderer.render_height(probe.as_uri()), renderer.render_height(probe.as_uri())
  finally:
    renderer.close()
  assert first > 0 and first == second, "the warm render must be deterministic for identical bytes"
