from pathlib import Path
from types import SimpleNamespace

import pytest
from starlette.requests import Request

from src.api import pages
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata


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
