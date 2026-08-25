"""GET /api/sessions/status and /api/sessions/tui/status are scoped to requested ids.

The sidebar renders a couple of dozen sessions but the session directory holds
hundreds; both handlers must resolve exactly the ids the client asks for and
never enumerate the whole directory.
"""

from pathlib import Path

import pytest
from conftest import build_tui_sessions_cfg
from conftest import make_sessions_client as _build_client

from src.core.models import CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager


def _forbid_list_sessions(monkeypatch: pytest.MonkeyPatch) -> None:
  """Make the full-directory sweep an error so a regression fails loudly."""

  async def explode(*args: object, **kwargs: object) -> list[SessionMetadata]:
    raise AssertionError("status handlers must not enumerate all sessions")

  monkeypatch.setattr(SessionManager, "list_sessions", explode)


@pytest.mark.asyncio
async def test_status_returns_exactly_the_requested_ids(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  wanted = await session_mgr.create_session(CreateSessionRequest(name="Sidebar"))
  other = await session_mgr.create_session(CreateSessionRequest(name="Off screen"))
  _forbid_list_sessions(monkeypatch)

  with _build_client(cfg, session_mgr) as client:
    response = client.get(f"/api/sessions/status?ids={wanted.id}")

  assert response.status_code == 200
  body = response.json()
  assert set(body) == {wanted.id}
  assert other.id not in body
  assert set(body[wanted.id]) == {
      "has_unread",
      "has_running_tasks",
      "thinking_since",
      "has_pending_trigger",
      "pending_trigger_count",
      "next_trigger_at",
      "has_pending_plan_approval",
  }


@pytest.mark.asyncio
async def test_status_omits_unknown_ids_without_error(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  known = await session_mgr.create_session(CreateSessionRequest(name="Known"))
  _forbid_list_sessions(monkeypatch)

  with _build_client(cfg, session_mgr) as client:
    response = client.get(f"/api/sessions/status?ids={known.id},deleted-session-id")

  assert response.status_code == 200
  assert set(response.json()) == {known.id}


@pytest.mark.asyncio
async def test_status_ids_is_required(tmp_path: Path) -> None:
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  await session_mgr.create_session(CreateSessionRequest(name="Sidebar"))

  with _build_client(cfg, session_mgr) as client:
    assert client.get("/api/sessions/status").status_code == 422
    assert client.get("/api/sessions/status?ids=").status_code == 422
    assert client.get("/api/sessions/status?ids=,,").status_code == 422


@pytest.mark.asyncio
async def test_tui_status_returns_only_requested_tui_sessions(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  requested = SessionMetadata(name="TUI on screen", backend="claude-tui")
  off_screen = SessionMetadata(name="TUI off screen", backend="claude-tui")
  await session_mgr.save_metadata(requested)
  await session_mgr.save_metadata(off_screen)

  checked = []

  async def fake_tmux_session_exists(session_id: str) -> bool:
    checked.append(session_id)
    return True

  def fake_claude_jsonl_busy(session_id: str) -> bool:
    return False

  monkeypatch.setattr("src.agents.backends.tui.tmux_session_exists", fake_tmux_session_exists)
  monkeypatch.setattr("src.agents.backends.tui._claude_jsonl_busy", fake_claude_jsonl_busy)
  _forbid_list_sessions(monkeypatch)

  with _build_client(cfg, session_mgr) as client:
    response = client.get(f"/api/sessions/tui/status?ids={requested.id},deleted-session-id")

  assert response.status_code == 200
  assert response.json() == {requested.id: {"running": True, "busy": False}}
  assert checked == [requested.id]


@pytest.mark.asyncio
async def test_tui_status_ids_is_required(tmp_path: Path) -> None:
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  await session_mgr.save_metadata(SessionMetadata(name="TUI", backend="claude-tui"))

  with _build_client(cfg, session_mgr) as client:
    assert client.get("/api/sessions/tui/status").status_code == 422
    assert client.get("/api/sessions/tui/status?ids=").status_code == 422
    assert client.get("/api/sessions/tui/status?ids=%20").status_code == 422
