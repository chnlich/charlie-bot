"""GET /api/sessions/status and /api/sessions/tui/status are scoped to requested ids.

The sidebar renders a couple of dozen sessions but the session directory holds
hundreds; both handlers must resolve exactly the ids the client asks for and
never enumerate the whole directory.
"""

from pathlib import Path

import pytest
from conftest import build_tui_sessions_cfg
from conftest import make_sessions_client as _build_client

from src.core import sidebar_state, thinking_state
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
async def test_status_derived_map_serves_whole_between_state_bumps(tmp_path: Path,) -> None:
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

  sidebar_state.store_snapshot_entry(
      session.id, {
          sidebar_state.THREAD_RUNNING: False,
          sidebar_state.PENDING_TRIGGER_COUNT: 0,
          sidebar_state.NEXT_TRIGGER_AT: None,
          sidebar_state.HAS_PENDING_PLAN_APPROVAL: False,
      })
  fifth = await session_mgr.resolve_sidebar_state([session], **flags)
  assert fifth is not fourth


@pytest.mark.asyncio
async def test_list_sessions_rows_carry_stamp_and_derived_fields(tmp_path: Path,) -> None:
  """Every listing row leaves the manager stamped and derived, as its own copy.

  The row build folds the thinking stamp and resolve_sidebar_state's derived
  fields into the one model_copy; a future edit that drops either half would
  serve unstamped rows or stale sidebar verdicts silently, and a row that
  aliased the shared cache object would let a caller's mutation corrupt every
  later listing.
  """
  sidebar_state.reset_for_tests()
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Stamped"))
  thinking_state.mark_busy(session.id)
  flags = {
      "include_running_status": True,
      "include_pending_trigger_status": True,
      "include_pending_plan_approval": True,
  }
  rows = await session_mgr.list_sessions(**flags)
  row = next(r for r in rows if r.id == session.id)
  assert row.thinking_since == thinking_state.busy_since(session.id) is not None
  assert row.has_running_tasks is True  # the live busy state's derived verdict
  assert row.has_pending_trigger is False
  assert row.has_pending_plan_approval is False
  row.has_unread = True  # a caller mutation must never reach the shared cache
  assert (await session_mgr.list_sessions(**flags))[0].has_unread is False


@pytest.mark.asyncio
async def test_root_list_changed_round_rerenders_only_moved_rows(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  """A moved row re-dumps itself; every unmoved row reuses its rendered dict.

  The root list's whole-body memo misses whenever any row's overlay state
  moves — the standing shape under active turns — and the changed round then
  re-rendered every row. A row's memo slot keys on the row identity the
  manager's fresh check moves exactly when its content moves, so a stale
  render can only be served for a row whose content provably did not move;
  the served body must still equal a full re-render's bytes.
  """
  sidebar_state.reset_for_tests()
  import src.api.sessions as sessions_api

  sessions_api._sessions_list_whole_body = None
  sessions_api._sessions_list_row_render.clear()
  cfg = build_tui_sessions_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  stay = await session_mgr.create_session(CreateSessionRequest(name="Steady"))
  mover = await session_mgr.create_session(CreateSessionRequest(name="Churning"))
  leaving = await session_mgr.create_session(CreateSessionRequest(name="Departing"))
  counts = {"dump": 0}
  real_dump = SessionMetadata.model_dump

  def counted_dump(self: SessionMetadata, *args: object, **kwargs: object) -> object:
    counts["dump"] += 1
    return real_dump(self, *args, **kwargs)

  monkeypatch.setattr(SessionMetadata, "model_dump", counted_dump)

  with _build_client(cfg, session_mgr) as client:
    full = client.get("/api/sessions/")
    assert full.status_code == 200
    full_dumps = counts["dump"]

    counts["dump"] = 0
    thinking_state.mark_busy(mover.id)
    changed = client.get("/api/sessions/")
    assert changed.status_code == 200
    changed_dumps = counts["dump"]
    # the mover's row re-dumped; the unmoved rows did not
    assert 0 < changed_dumps < full_dumps

    # byte parity: a forced full re-render of the same corpus and states
    sessions_api._sessions_list_whole_body = None
    sessions_api._sessions_list_row_render.clear()
    counts["dump"] = 0
    forced = client.get("/api/sessions/")
    assert forced.status_code == 200
    assert forced.content == changed.content
    assert counts["dump"] == full_dumps

    # a row that left the projection drops its slot with it
    await session_mgr.archive_session(leaving.id)
    thinking_state.clear_busy(mover.id)
    after = client.get("/api/sessions/")
    assert after.status_code == 200
    assert len(sessions_api._sessions_list_row_render) == len(after.json())
    assert all(row["id"] != leaving.id for row in after.json())
