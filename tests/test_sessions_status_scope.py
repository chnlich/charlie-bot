"""GET /api/sessions/status is scoped to requested ids.

The sidebar renders a couple of dozen sessions but the session directory holds
hundreds; the handler must resolve exactly the ids the client asks for and
never enumerate the whole directory.
"""

import os
from pathlib import Path

import orjson
import pytest
from conftest import build_two_backend_cfg
from conftest import make_sessions_listing_client

from src.infra.models import CreateSessionRequest, SessionMetadata, SessionStatus
from src.runtime import sidebar_state, thinking_state
from src.runtime.task_sessions import TaskTreeManager
from src.runtime.api import sessions as sessions_api
from src.runtime.sessions import SessionManager, _iter_trigger_stats, _listing_row_copy


def _build_client(cfg, session_mgr: SessionManager):
  return make_sessions_listing_client(cfg, session_mgr, TaskTreeManager(cfg, session_mgr))


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
  cfg = build_two_backend_cfg(tmp_path)
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
      "work_state",
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
  cfg = build_two_backend_cfg(tmp_path)
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
async def test_status_body_memo_serves_whole_between_state_bumps(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  """A clean poll serves the last rendered body bytes; any state bump re-renders.

  The body memo is the poll's byte-level freshness boundary on top of the
  fold's: a bump that failed to re-render would serve a stale thinking_since
  for a full memo lifetime, and a memo that never hit would silently return
  the poll to the per-request render it used to pay.
  """
  sidebar_state.reset_for_tests()
  cfg = build_two_backend_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Body memo"))
  renders: list[object] = []

  def count_render(content: object) -> bytes:
    renders.append(content)
    return orjson.dumps(content)

  monkeypatch.setattr(sessions_api, "fast_json_bytes", count_render)

  with _build_client(cfg, session_mgr) as client:
    first = client.get(f"/api/sessions/status?ids={session.id}")
    assert first.status_code == 200
    assert first.json()[session.id]["thinking_since"] is None
    assert len(renders) == 1

    # The first poll's own probe store bumps the generation past its key (the
    # fold's warm-up shape), so the second re-render is the one the next poll
    # serves whole.
    second = client.get(f"/api/sessions/status?ids={session.id}")
    assert second.content == first.content
    assert len(renders) == 2

    third = client.get(f"/api/sessions/status?ids={session.id}")
    assert third.content == first.content
    assert len(renders) == 2  # unchanged generation: the stored body serves whole

    thinking_state.mark_busy(session.id)
    fourth = client.get(f"/api/sessions/status?ids={session.id}")
    assert fourth.json()[session.id]["thinking_since"] is not None
    assert len(renders) == 3  # the bump forces the re-render

    thinking_state.clear_busy(session.id)
    fifth = client.get(f"/api/sessions/status?ids={session.id}")
    assert fifth.json()[session.id]["thinking_since"] is None
    assert len(renders) == 4


@pytest.mark.asyncio
async def test_status_body_memo_serves_no_ghost_row_after_delete(tmp_path: Path,) -> None:
  """A permanent deletion bumps the generation the body memo keys on.

  The memo keys on the requested ids, so a poll still carrying a just-deleted
  id — a second open client, or the deleter's client before its listing
  refresh — must miss and rebuild without the row. A deletion that failed to
  bump would memo-hit at the unchanged generation and serve the deleted
  session's ghost row for a full memo lifetime.
  """
  sidebar_state.reset_for_tests()
  cfg = build_two_backend_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  kept = await session_mgr.create_session(CreateSessionRequest(name="Survivor"))
  gone = await session_mgr.create_session(CreateSessionRequest(name="Deleted"))
  ids = f"{kept.id},{gone.id}"

  with _build_client(cfg, session_mgr) as client:
    first = client.get(f"/api/sessions/status?ids={ids}")
    assert first.status_code == 200
    assert set(first.json()) == {kept.id, gone.id}
    # The first poll's own probe stores bump the generation past its key; the
    # second poll primes the memo at the generation that is current now, so a
    # delete that fails to bump would memo-hit the ghost row below.
    second = client.get(f"/api/sessions/status?ids={ids}")
    assert second.status_code == 200
    assert set(second.json()) == {kept.id, gone.id}

    await session_mgr.delete_session_permanently(gone.id)

    third = client.get(f"/api/sessions/status?ids={ids}")
    assert third.status_code == 200
    assert set(third.json()) == {kept.id}


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
  cfg = build_two_backend_cfg(tmp_path)
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
  assert row.has_running_tasks is False  # no Run is live; thinking_since is the separate busy stamp
  assert row.has_pending_trigger is False
  assert row.has_pending_plan_approval is False
  row.has_unread = True  # a caller mutation must never reach the shared cache
  assert (await session_mgr.list_sessions(**flags))[0].has_unread is False

  # The fast copy bakes in SessionMetadata's model config: the copy carries the
  # extras dict and writes None private state, which holds only while the model
  # keeps unregistered keys as extras and carries no private attrs. The value arm
  # below cannot catch a private-attr change on its own — pydantic keeps empty
  # private attrs out of both model_dump() and __pydantic_fields_set__ — so the
  # two config facts are asserted directly and the config change fails here.
  assert SessionMetadata.model_config.get("extra") == "allow"
  assert not SessionMetadata.__private_attributes__
  update = {
      "thinking_since": thinking_state.busy_since(session.id),
      "run_backend": None,
      "has_unread": True,
      "has_running_tasks": True,
      "status": SessionStatus.ARCHIVED,
  }
  for meta in await session_mgr._load_session_metas():
    meta.unregistered_key = "kept"  # an extra key rides the copy
    fast = _listing_row_copy(meta, update)
    reference = meta.model_copy(update=update)
    assert fast.model_dump() == reference.model_dump()
    assert fast.model_extra == reference.model_extra == meta.model_extra
    assert fast.model_extra is not meta.model_extra
    assert fast.__pydantic_fields_set__ == reference.__pydantic_fields_set__


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
  sessions_api._workspace_list_memos.whole_body = None
  sessions_api._workspace_list_memos.row_render.clear()
  cfg = build_two_backend_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  await session_mgr.create_session(CreateSessionRequest(name="Steady"))
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
    sessions_api._workspace_list_memos.whole_body = None
    sessions_api._workspace_list_memos.row_render.clear()
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
    assert len(sessions_api._workspace_list_memos.row_render) == len(after.json())
    assert all(row["id"] != leaving.id for row in after.json())


class TestProbeWalkDirStateMemo:
  """The probe walk memoizes trigger directory membership while its stat holds."""

  def test_trigger_pairs_refresh_when_the_directory_moves_and_hold_while_it_stands(self, tmp_path: Path) -> None:
    triggers_dir = tmp_path / "sessions" / "sid" / "triggers"
    triggers_dir.mkdir(parents=True)
    (triggers_dir / "a.json").write_text("{}", encoding="utf-8")
    pairs = _iter_trigger_stats(os.fspath(triggers_dir), os.stat(triggers_dir))
    assert [os.path.basename(path) for path, _st in pairs] == ["a.json"]
    # the same directory stat serves the memoized pairs unchanged
    assert _iter_trigger_stats(os.fspath(triggers_dir), os.stat(triggers_dir)) == pairs
    # a rename-published trigger moves the directory: the next walk sees it
    landing = triggers_dir / "b.json.new"
    landing.write_text("{}", encoding="utf-8")
    os.replace(landing, triggers_dir / "b.json")
    refreshed = _iter_trigger_stats(os.fspath(triggers_dir), os.stat(triggers_dir))
    assert [os.path.basename(path) for path, _st in refreshed] == ["a.json", "b.json"]
