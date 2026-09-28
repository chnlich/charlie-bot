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
from src.core import sidebar_state


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
async def test_status_derived_map_serves_whole_between_state_bumps(
    tmp_path: Path,
) -> None:
  """A clean poll serves the last fold's map; any state bump forces a re-derive.

  The memo is the /status poll's freshness boundary: a bump that failed to
  invalidate would serve a stale has_running_tasks for a full memo lifetime,
  and a memo that never hit would silently return the poll to the per-session
  rebuild the fold used to pay.
  """
  sidebar_state.reset_for_tests()
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Memo"))
  flags = {
      "include_running_status": True,
      "include_pending_trigger_status": True,
      "include_pending_plan_approval": True,
  }

  # The probe round's own stores bump the generation past its key, so the
  # first clean re-derive is the one the next poll serves whole.
  await session_mgr.resolve_sidebar_state([session], **flags)
  second = await session_mgr.resolve_sidebar_state([session], **flags)
  third = await session_mgr.resolve_sidebar_state([session], **flags)
  assert third is second  # unchanged generation: the stored map serves whole

  sidebar_state.mark_sidebar_dirty(session.id)
  fourth = await session_mgr.resolve_sidebar_state([session], **flags)
  assert fourth is not second

  sidebar_state.store_snapshot_entry(session.id, {
      sidebar_state.THREAD_RUNNING: False,
      sidebar_state.PENDING_TRIGGER_COUNT: 0,
      sidebar_state.NEXT_TRIGGER_AT: None,
      sidebar_state.HAS_PENDING_PLAN_APPROVAL: False,
  })
  fifth = await session_mgr.resolve_sidebar_state([session], **flags)
  assert fifth is not fourth
