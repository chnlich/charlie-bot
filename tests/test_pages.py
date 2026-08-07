from __future__ import annotations

import asyncio
import shutil
import subprocess
import time
from pathlib import Path
from types import SimpleNamespace

import pytest
from starlette.requests import Request

from src.api import pages
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata
from src.core.token_tally import AccountRow, ModelRow, TokenTally


@pytest.fixture(autouse=True)
def _reset_token_usage_single_flight() -> None:
  """Isolate the module-level single-flight holder between route tests."""
  pages._token_usage_task = None
  yield
  pages._token_usage_task = None


class FakeSessionManager:
  async def list_sessions(
      self,
      status=None,
      scheduled=False,
      include_running_status=True,
      include_pending_trigger_status=False,
  ) -> list[object]:
    return []


def _build_request() -> Request:
  scope = {
      "type": "http",
      "method": "GET",
      "path": "/",
      "headers": [],
      "query_string": b"",
      "scheme": "http",
      "server": ("testserver", 80),
      "client": ("127.0.0.1", 12345),
  }
  return Request(scope)


def _inline_scripts(body: str) -> list[str]:
  """Extract inline `<script>...</script>` blocks without a `src` attribute."""
  scripts: list[str] = []
  idx = 0
  while True:
    open_idx = body.find("<script>", idx)
    if open_idx == -1:
      return scripts
    close_idx = body.find("</script>", open_idx)
    assert close_idx != -1, "unterminated <script> block"
    scripts.append(body[open_idx + len("<script>"):close_idx])
    idx = close_idx + len("</script>")


@pytest.mark.asyncio
async def test_token_usage_route_returns_rows(monkeypatch: pytest.MonkeyPatch) -> None:
  tally = TokenTally(
      rows=[
          ModelRow(
              model="claude-opus-5", source="Claude Code", calls=1, in_fresh=10,
              cache_write=5, cache_read=20, output=30, total=65, first="2024-01-01",
              last="2024-01-02",
              accounts=[AccountRow(name="work (default)", calls=1, output=30, total=65)]),
          ModelRow(
              model="gpt-5.2", source="Codex", calls=2, in_fresh=40, cache_write=0,
              cache_read=0, output=15, total=55, first="2024-01-01", last="2024-01-03",
              accounts=[AccountRow(name="work (default)", calls=2, output=15, total=55)]),
      ],
      notes=["Claude Code: 1 unique API responses over 1 config dirs"],
      elapsed_s=0.05,
      scanned_bytes=999,
  )

  def fake_collect() -> TokenTally:
    return tally

  monkeypatch.setattr(pages, "collect_token_usage", fake_collect)

  response = await pages.token_usage_viewer(_build_request())
  assert response.status_code == 200
  body = response.body.decode("utf-8")
  # The per-model table (the accessible equivalent of the charts) is present, carrying the
  # same numbers the JS payload embeds.
  assert 'id="tbl"' in body
  assert "claude-opus-5" in body
  assert "gpt-5.2" in body
  assert "Token usage by model" in body
  assert "0.05" in body
  assert "999" in body


@pytest.mark.asyncio
async def test_token_usage_route_is_single_flight(monkeypatch: pytest.MonkeyPatch) -> None:
  calls = 0

  def fake_collect() -> TokenTally:
    nonlocal calls
    calls += 1
    time.sleep(0.2)  # keep the collection genuinely in flight so both requests share it
    return TokenTally(rows=[], notes=[], elapsed_s=0.2, scanned_bytes=0)

  monkeypatch.setattr(pages, "collect_token_usage", fake_collect)

  request_one = _build_request()
  request_two = _build_request()
  first, second = await asyncio.gather(
      pages.token_usage_viewer(request_one), pages.token_usage_viewer(request_two))
  assert first.status_code == 200
  assert second.status_code == 200
  # Two concurrent requests share one in-flight collection: exactly one scan ran.
  assert calls == 1


@pytest.mark.asyncio
async def test_token_usage_viewer_clears_inflight_task_after_render(
    monkeypatch: pytest.MonkeyPatch) -> None:
  """A finished collection is cleared, so the next request re-scans afresh."""
  calls = 0

  def fake_collect() -> TokenTally:
    nonlocal calls
    calls += 1
    return TokenTally(rows=[], notes=[], elapsed_s=0.01, scanned_bytes=0)

  monkeypatch.setattr(pages, "collect_token_usage", fake_collect)
  await pages.token_usage_viewer(_build_request())
  assert calls == 1
  await pages.token_usage_viewer(_build_request())
  assert calls == 2


@pytest.mark.skipif(shutil.which("node") is None, reason="requires node on PATH")
@pytest.mark.asyncio
async def test_token_usage_inline_script_parses(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """The page's inline script must be valid JavaScript.

  A bare `window_str` interpolation (a string containing spaces and a non-ASCII arrow) used to
  be emitted unquoted, breaking script parse and leaving charts and table empty on every load.
  This asserts the script parses under `node --check`, not that a literal string is present.
  """
  # Window string deliberately carries spaces and a `→` so an unquoted interpolation breaks.
  tally = TokenTally(
      rows=[
          ModelRow(
              model="claude-opus-5", source="Claude Code", calls=1, in_fresh=10,
              cache_write=5, cache_read=20, output=30, total=65, first="2026-06-24",
              last="2026-08-07",
              accounts=[AccountRow(name="work (default)", calls=1, output=30, total=65)]),
      ],
      notes=["Claude Code: 1 unique API responses over 1 config dirs"],
      elapsed_s=0.05,
      scanned_bytes=999,
  )

  monkeypatch.setattr(pages, "collect_token_usage", lambda: tally)

  response = await pages.token_usage_viewer(_build_request())
  assert response.status_code == 200
  body = response.body.decode("utf-8")

  assert "2026-06-24 → 2026-08-07" in body

  scripts = _inline_scripts(body)
  assert scripts, "no inline <script> block to parse"

  for idx, script in enumerate(scripts):
    script_file = tmp_path / f"inline-{idx}.js"
    script_file.write_text(script, encoding="utf-8")
    subprocess.run(["node", "--check", str(script_file)], check=True)


@pytest.mark.asyncio
async def test_index_uses_pinned_runtime_git_version(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  git_lookup_calls = 0

  def fail_git_lookup() -> str:
    nonlocal git_lookup_calls
    git_lookup_calls += 1
    raise AssertionError("index() should use the startup git version")

  monkeypatch.setattr(pages, "_RUNTIME_GIT_VERSION", "abc1234 · 03-24")
  monkeypatch.setattr(pages, "_get_git_version", fail_git_lookup)

  response_one = await pages.index(
      request=_build_request(),
      session=None,
      session_mgr=FakeSessionManager(),
      thread_mgr=object(),
      cfg=cfg,
  )
  response_two = await pages.index(
      request=_build_request(),
      session=None,
      session_mgr=FakeSessionManager(),
      thread_mgr=object(),
      cfg=cfg,
  )

  assert response_one.context["version"] == "abc1234 · 03-24"
  assert response_two.context["version"] == "abc1234 · 03-24"
  assert git_lookup_calls == 0


@pytest.mark.asyncio
async def test_index_versions_local_static_assets(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  monkeypatch.setattr(pages, "_RUNTIME_GIT_VERSION", "abc1234 · 03-24")

  response = await pages.index(
      request=_build_request(),
      session=None,
      session_mgr=FakeSessionManager(),
      thread_mgr=object(),
      cfg=cfg,
  )

  body = response.body.decode("utf-8")
  assert response.context["static_asset_version"] == "abc1234-03-24"
  assert 'href="/static/css/styles.css?v=abc1234-03-24"' in body
  assert 'src="/static/js/sidebar.js?v=abc1234-03-24"' in body
  assert 'src="/static/js/app.js?v=abc1234-03-24"' in body


class PendingTriggerSessionManager(FakeSessionManager):
  def __init__(self, session: SessionMetadata):
    self._session = session

  async def list_sessions(
      self,
      status=None,
      scheduled=False,
      include_running_status=True,
      include_pending_trigger_status=False,
  ) -> list[object]:
    return [self._session.model_copy()]

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    if session_id != self._session.id:
      return None
    return self._session.model_copy()


@pytest.mark.asyncio
async def test_index_embeds_initial_sessions_for_client_sidebar_render(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session = SessionMetadata(
      id="session-with-trigger",
      name="Wake later",
      has_pending_trigger=True,
      pending_trigger_count=2,
  )

  async def fake_build_session_bootstrap_data(*args, **kwargs) -> SimpleNamespace:
    return SimpleNamespace(
        session=session,
        messages=[],
        pending_draft=None,
        total_event_count=0,
        oldest_message_ordinal=0,
        has_more=False,
    )

  monkeypatch.setattr(pages, "build_session_bootstrap_data", fake_build_session_bootstrap_data)

  response = await pages.index(
      request=_build_request(),
      session=session.id,
      session_mgr=PendingTriggerSessionManager(session),
      thread_mgr=object(),
      cfg=cfg,
  )

  body = response.body.decode("utf-8")
  assert "const INITIAL_SESSIONS =" in body
  assert '"session-with-trigger"' in body
  assert '"has_pending_trigger": true' in body
  assert '"pending_trigger_count": 2' in body
  assert 'id="pending-trigger-session-with-trigger"' not in body
  assert "Loading sessions..." in body
