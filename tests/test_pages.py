from pathlib import Path

import pytest
from starlette.requests import Request

from src.api import pages
from src.core.config import CharlieBotConfig


class FakeSessionManager:
  async def list_sessions(
      self,
      status=None,
      scheduled=False,
      include_running_status=True,
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
