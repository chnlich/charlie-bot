"""The session-read contract: "read" means "the content actually rendered".

GET /api/sessions/{id}/bootstrap and /view are side-effect-free reads — a bare
data fetch (the latency-perf loop's M96 collector, a superseded SPA switch)
must not wipe has_unread. Only the client's post-render
POST /api/sessions/{id}/read flips it, emitting one unread_changed sidebar
broadcast exactly when the flag flips (SessionManager.mark_read semantics).
"""

from pathlib import Path
from unittest.mock import AsyncMock, patch
from uuid import uuid4

import httpx
import pytest
from conftest import BROADCAST_PATCH_TARGET, make_home_session
from fastapi import FastAPI

from src.api import deps
from src.api.deps import get_config, get_config_on_loop, get_session_manager, get_thread_manager
from src.api.sessions import router as sessions_router
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.streaming import SIDEBAR_CHANNEL
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager


def _sessions_app(cfg: CharlieBotConfig, mgr: SessionManager) -> FastAPI:
  """Sessions-router app with the view route's thread dependency pinned to *cfg*.

  AsyncClient+ASGITransport runs the handlers on the test's own loop, so the
  manager's per-session asyncio locks bind to the same loop the seed writes
  used (TestClient's portal loop would rebind them cross-loop).
  """
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: mgr
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  return app


@pytest.mark.asyncio
async def test_bootstrap_and_view_gets_keep_the_unread_flag(tmp_path: Path) -> None:
  """A session with has_unread=True keeps it across both switch-fetch GETs."""
  cfg, mgr, session = await make_home_session(tmp_path, name="read-contract")
  await mgr.mark_unread(session.id)

  async with httpx.AsyncClient(transport=httpx.ASGITransport(app=_sessions_app(cfg, mgr)),
                               base_url="http://test") as client:
    with (
        patch.object(deps, "_trigger_manager", TriggerManager(cfg, mgr)),
        patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
    ):
      for endpoint in ("bootstrap", "view"):
        resp = await client.get(f"/api/sessions/{session.id}/{endpoint}")
        assert resp.status_code == 200, resp.text
      # Neither GET flips the flag, so neither emits the flip broadcast.
      mock_broadcast.assert_not_awaited()

  meta = await mgr.get_session(session.id)
  assert meta.has_unread is True


@pytest.mark.asyncio
async def test_read_post_flips_unread_and_broadcasts_exactly_on_flip(tmp_path: Path) -> None:
  """POST /read clears has_unread with one unread_changed broadcast; a repeat POST is silent."""
  cfg, mgr, session = await make_home_session(tmp_path, name="read-contract")
  await mgr.mark_unread(session.id)

  async with httpx.AsyncClient(transport=httpx.ASGITransport(app=_sessions_app(cfg, mgr)),
                               base_url="http://test") as client:
    with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast:
      resp = await client.post(f"/api/sessions/{session.id}/read")
      assert resp.status_code == 200, resp.text
      assert resp.json() == {"session_id": session.id, "has_unread": False}

      mock_broadcast.assert_awaited_once()
      channel, event = mock_broadcast.await_args.args
      assert channel == SIDEBAR_CHANNEL
      assert event == {"type": ET.UNREAD_CHANGED, "session_id": session.id, "has_unread": False}

      # Second POST: same response shape, but no flip means no further broadcast.
      resp = await client.post(f"/api/sessions/{session.id}/read")
      assert resp.status_code == 200, resp.text
      assert resp.json() == {"session_id": session.id, "has_unread": False}
      mock_broadcast.assert_awaited_once()

  meta = await mgr.get_session(session.id)
  assert meta.has_unread is False


@pytest.mark.asyncio
async def test_read_route_resolves_and_status_poll_still_answers(tmp_path: Path) -> None:
  """FastAPI resolves POST /{uuid}/read and GET /status?ids=... alongside it."""
  cfg, mgr, session = await make_home_session(tmp_path, name="read-contract")
  await mgr.mark_unread(session.id)

  async with httpx.AsyncClient(transport=httpx.ASGITransport(app=_sessions_app(cfg, mgr)),
                               base_url="http://test") as client:
    resp = await client.get("/api/sessions/status", params={"ids": session.id})
    assert resp.status_code == 200, resp.text
    assert resp.json()[session.id]["has_unread"] is True

    with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
      resp = await client.post(f"/api/sessions/{session.id}/read")
      assert resp.status_code == 200, resp.text

    resp = await client.get("/api/sessions/status", params={"ids": session.id})
    assert resp.status_code == 200, resp.text
    assert resp.json()[session.id]["has_unread"] is False

    # require_session keeps its 404 for an unknown id.
    resp = await client.post(f"/api/sessions/{uuid4()}/read")
    assert resp.status_code == 404, resp.text
