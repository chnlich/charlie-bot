"""Unit invariants for the master-side restart transport:

- ``unanswered_user_events`` picks exactly the real user events after the last
  MASTER_DONE, excluding per-event ids (never per-session).
- ``SessionManager.persist_master_run`` writes (and clears) the record so a
  restarted process reloading metadata.json sees the identical state.
- The consumer clears master_run only after MASTER_DONE is durable, so a
  crash mid-window replays (duplicate) rather than silently dropping.
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.agents import master_cc
from src.core import init as init_module
from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    CreateSessionRequest,
    MasterRunRecord,
    SessionCallbacks,
    SessionMetadata,
    utc_now,
)
from src.core.sessions import SessionManager


def _cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backend_options=[BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model")],
  )


# --- unanswered_user_events -------------------------------------------------


def _user(content: str, event_id: str) -> dict:
  return {"type": "user", "content": content, "id": event_id}


def test_unanswered_scan_picks_only_events_after_last_master_done() -> None:
  events = [
      _user("answered already", "e1"),
      {"type": "assistant", "id": "a1"},
      {"type": "master_done", "id": "d1"},
      _user("still open", "e2"),
      _user("queued behind it", "e3"),
  ]
  pending = init_module.unanswered_user_events(events, set())
  assert [e["id"] for e in pending] == ["e2", "e3"]


def test_unanswered_scan_excludes_the_recorded_turns_event_only() -> None:
  # Exclusion must be per-event: excluding e2 (the crashed turn's message)
  # must NOT also shield e3, which disappeared with the killed in-memory queue.
  events = [
      _user("running when killed", "e2"),
      _user("queued behind it", "e3"),
  ]
  pending = init_module.unanswered_user_events(events, {"e2"})
  assert [e["id"] for e in pending] == ["e3"]


def test_unanswered_scan_ignores_non_message_user_typed_events() -> None:
  events = [
      {"type": "master_done", "id": "d1"},
      {"type": "user", "content": {"tool_result": True}, "id": "e4"},  # tool bridge, not a message
      _user("real message", "e5"),
  ]
  pending = init_module.unanswered_user_events(events, set())
  assert [e["id"] for e in pending] == ["e5"]


# --- SessionManager.persist_master_run --------------------------------------


@pytest.mark.asyncio
async def test_master_run_record_round_trips_through_metadata_json(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  record = MasterRunRecord(
      pid=123,
      pid_start="456.78",
      started_at=utc_now(),
      raw_log="/x/y/agent.raw.ndjson",
      user_event_id="evt-1",
  )

  await session_mgr.persist_master_run(meta.id, record)

  # Fresh process semantics: parse the file directly, not the manager cache.
  raw = json.loads((cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["master_run"]["pid"] == 123
  assert raw["master_run"]["pid_start"] == "456.78"
  assert raw["master_run"]["raw_log"] == "/x/y/agent.raw.ndjson"
  assert raw["master_run"]["user_event_id"] == "evt-1"
  assert SessionMetadata.model_validate(raw).master_run == record

  await session_mgr.persist_master_run(meta.id, None)
  raw = json.loads((cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["master_run"] is None


@pytest.mark.asyncio
async def test_master_run_persist_does_not_clobber_unrelated_fields(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  await session_mgr.persist_cc_session_id(meta.id, "cc-anchor")

  record = MasterRunRecord(started_at=utc_now(), raw_log="/x/agent.raw.ndjson")
  await session_mgr.persist_master_run(meta.id, record)

  fresh = await session_mgr.get_session(meta.id)
  assert fresh is not None
  assert fresh.cc_session_id == "cc-anchor"
  assert fresh.master_run == record


# --- consumer clears master_run only after MASTER_DONE -----------------------


def _make_callbacks(persist_order: list[str]) -> SessionCallbacks:
  async def persist_and_broadcast(session_id: str, event: dict) -> None:
    if event.get("type") == "master_done":
      persist_order.append("master_done")

  async def persist_master_run(session_id: str, record) -> None:
    if record is None:
      persist_order.append("master_run_cleared")

  return SessionCallbacks(
      persist_and_broadcast=persist_and_broadcast,
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=persist_master_run,
  )


@pytest.mark.asyncio
async def test_consumer_clears_master_run_after_master_done(monkeypatch: pytest.MonkeyPatch) -> None:
  persist_order: list[str] = []
  callbacks = _make_callbacks(persist_order)
  session_meta = SessionMetadata(id="session-clear", name="t")

  async def fake_run_cc(item: master_cc._WorkItem):
    return "cc-1", 0, None, {}

  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)

  item = master_cc._WorkItem(
      cfg=_cfg(Path("/tmp/charliebot-unit")),
      session_meta=session_meta,
      user_content="hi",
      callbacks=callbacks,
      is_voice=False,
      auto_trigger=False,
      backend_option=None,
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )
  master_cc._session_queues.pop(session_meta.id, None)
  master_cc._session_queues[session_meta.id] = asyncio.Queue()
  master_cc._session_queues[session_meta.id].put_nowait(item)
  try:
    with (
        patch.object(master_cc, "_run_cc", side_effect=fake_run_cc),
        patch.object(master_cc.streaming_manager, "broadcast", new=AsyncMock()),
        patch("src.core.sessions.SessionManager", return_value=workers_mock),
    ):
      await asyncio.wait_for(master_cc._session_consumer(session_meta.id), timeout=5)
  finally:
    master_cc._session_queues.pop(session_meta.id, None)
    master_cc._session_consumers.pop(session_meta.id, None)

  assert persist_order == ["master_done", "master_run_cleared"], (
      "the restart identity must outlive the result boundary: clearing it first "
      "would let a kill between the two writes silently drop the user message")


# --- queued_user_event_ids ----------------------------------------------------


@pytest.mark.asyncio
async def test_queued_user_event_ids_covers_running_and_queued_items() -> None:
  gate = asyncio.Event()

  async def blocked_run_cc(item: master_cc._WorkItem):
    await gate.wait()
    return None, 0, None, {}

  callbacks = _make_callbacks([])
  cfg = _cfg(Path("/tmp/charliebot-unit"))
  session_meta = SessionMetadata(id="session-queued", name="t")

  def item(event_id: str) -> master_cc._WorkItem:
    return master_cc._WorkItem(
        cfg=cfg,
        session_meta=session_meta,
        user_content="x",
        callbacks=callbacks,
        is_voice=False,
        auto_trigger=False,
        backend_option=None,
        extra_claude_flags=None,
        should_check_tex=False,
        future=asyncio.get_running_loop().create_future(),
        user_event_id=event_id,
    )

  running = item("evt-running")
  queued = item("evt-queued")
  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)
  try:
    with (
        patch.object(master_cc, "_run_cc", side_effect=blocked_run_cc),
        patch.object(master_cc.streaming_manager, "broadcast", new=AsyncMock()),
        patch("src.core.sessions.SessionManager", return_value=workers_mock),
    ):
      master_cc._enqueue_work_item(session_meta.id, running)
      master_cc._enqueue_work_item(session_meta.id, queued)
      # Let the consumer pick up the first item.
      deadline = time.monotonic() + 2.0
      while session_meta.id not in master_cc._current_items and time.monotonic() < deadline:
        await asyncio.sleep(0.01)
      ids = master_cc.queued_user_event_ids(session_meta.id)
      assert ids == {"evt-running", "evt-queued"}
  finally:
    gate.set()
    await asyncio.gather(*(i.future for i in (running, queued)), return_exceptions=True)
    master_cc._session_queues.pop(session_meta.id, None)
    master_cc._session_consumers.pop(session_meta.id, None)
