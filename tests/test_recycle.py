"""Tests for SessionManager.recycle_scheduled_session and global event_index."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import append_events as _append_events
from conftest import archive_cutoff_events as _archive_cutoff_events

from src.api.message_utils import build_session_bootstrap_data, build_session_view_data
from src.api.sessions import get_session_events_page
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, ThreadMetadata, ThreadStatus
from src.core.sessions import SessionManager


def _write_thread(threads_dir: Path, thread_id: str, status: ThreadStatus, completed_at: datetime | None) -> None:
  thread_dir = threads_dir / thread_id
  thread_dir.mkdir(parents=True, exist_ok=True)
  meta = ThreadMetadata(
      id=thread_id,
      session_id="ignored",
      description=f"thread {thread_id}",
      status=status,
      completed_at=completed_at,
  )
  (thread_dir / "metadata.json").write_text(meta.model_dump_json(indent=2), encoding="utf-8")
  # Add a sentinel file so we can verify rmtree actually removed the dir.
  (thread_dir / "sentinel.txt").write_text("x", encoding="utf-8")


@pytest.mark.asyncio
async def test_recycle_deletes_only_old_terminal_threads(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))
  threads_dir = cfg.sessions_dir / session.id / "threads"

  now = datetime.now(timezone.utc)
  cutoff = now - timedelta(days=7)
  old = cutoff - timedelta(days=1)
  recent = cutoff + timedelta(days=1)

  _write_thread(threads_dir, "old-completed", ThreadStatus.COMPLETED, old)
  _write_thread(threads_dir, "old-failed", ThreadStatus.FAILED, old)
  _write_thread(threads_dir, "old-cancelled", ThreadStatus.CANCELLED, old)
  _write_thread(threads_dir, "recent-completed", ThreadStatus.COMPLETED, recent)
  _write_thread(threads_dir, "running", ThreadStatus.RUNNING, None)
  _write_thread(threads_dir, "idle", ThreadStatus.IDLE, None)
  # Corrupt metadata: should be tolerated (skipped, not deleted).
  bad_dir = threads_dir / "broken"
  bad_dir.mkdir()
  (bad_dir / "metadata.json").write_text("{not valid json", encoding="utf-8")
  (bad_dir / "sentinel.txt").write_text("x", encoding="utf-8")

  result = await mgr.recycle_scheduled_session(session.id, cutoff)

  assert result["threads_deleted"] == 3
  assert not (threads_dir / "old-completed").exists()
  assert not (threads_dir / "old-failed").exists()
  assert not (threads_dir / "old-cancelled").exists()
  assert (threads_dir / "recent-completed").exists()
  assert (threads_dir / "running").exists()
  assert (threads_dir / "idle").exists()
  assert (threads_dir / "broken").exists()


@pytest.mark.asyncio
async def test_recycle_archives_old_chat_events_and_advances_offset(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  cutoff, events = _archive_cutoff_events()
  live_path = mgr.get_chat_events_path(session.id)
  _append_events(live_path, events)

  result = await mgr.recycle_scheduled_session(session.id, cutoff)

  assert result["events_archived"] == 5
  archive_path = Path(result["archive_file"])
  assert archive_path.exists()
  iso = cutoff.isocalendar()
  assert archive_path.name == f"chat_events.{iso.year}-W{iso.week:02d}.jsonl"

  archive_lines = [json.loads(line) for line in archive_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert [e["content"] for e in archive_lines] == [f"e{i}" for i in range(5)]

  live_lines = [json.loads(line) for line in live_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert [e["content"] for e in live_lines] == [f"f{i}" for i in range(3)]

  meta = await mgr.get_session(session.id)
  assert meta is not None
  assert meta.archive_offset == 5

  # Subsequent persist_and_broadcast must continue the global numbering.
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(
        session.id,
        {
            "type": "user",
            "content": "after",
            "timestamp": (cutoff + timedelta(days=1)).isoformat()
        },
    )
  msg_payloads = [c.args[1] for c in mock.await_args_list if c.args[1].get("type") == "message"]
  assert msg_payloads, "expected at least one message delta"
  # Live now has 3 retained tail events + 1 new = 4 lines; global = 5 + 4 - 1 = 8.
  assert msg_payloads[0]["message"]["event_index"] == 8


@pytest.mark.asyncio
async def test_recycle_noop_when_nothing_old(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  cutoff = datetime(2026, 5, 10, 0, 0, 0, tzinfo=timezone.utc)
  events = [
      {
          "type": "user",
          "content": "future",
          "timestamp": (cutoff + timedelta(hours=1)).isoformat()
      },
  ]
  live_path = mgr.get_chat_events_path(session.id)
  _append_events(live_path, events)

  result = await mgr.recycle_scheduled_session(session.id, cutoff)

  assert result["events_archived"] == 0
  assert result["archive_file"] is None
  live_lines = [json.loads(line) for line in live_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert [e["content"] for e in live_lines] == ["future"]

  meta = await mgr.get_session(session.id)
  assert meta is not None
  assert meta.archive_offset == 0


@pytest.mark.asyncio
async def test_load_chat_events_range_spans_archive_and_live(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  cutoff, events = _archive_cutoff_events()
  live_path = mgr.get_chat_events_path(session.id)
  _append_events(live_path, events)
  await mgr.recycle_scheduled_session(session.id, cutoff)

  # Fully in archive
  archive_only, has_more = mgr.load_chat_events_range(session.id, 0, 3)
  assert [e["content"] for e in archive_only] == ["e0", "e1", "e2"]
  assert has_more is False

  # Fully in live (global indices 5..7 -> contents f0..f2)
  live_only, has_more = mgr.load_chat_events_range(session.id, 5, 8)
  assert [e["content"] for e in live_only] == ["f0", "f1", "f2"]
  assert has_more is True

  # Straddles archive/live boundary (global indices 3..7 -> e3,e4,f0,f1)
  mixed, has_more = mgr.load_chat_events_range(session.id, 3, 7)
  assert [e["content"] for e in mixed] == ["e3", "e4", "f0", "f1"]
  assert has_more is True


@pytest.mark.asyncio
async def test_session_view_uses_global_event_indices_after_archive(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  cutoff, events = _archive_cutoff_events()
  _append_events(mgr.get_chat_events_path(session.id), events)
  await mgr.recycle_scheduled_session(session.id, cutoff)

  thread_mgr = AsyncMock()
  thread_mgr.list_threads.return_value = []

  full_view = await build_session_view_data(session.id, mgr, thread_mgr)
  assert full_view.total_event_count == 8
  assert full_view.has_more is True
  assert [m["event_index"] for m in full_view.messages] == [5, 6, 7]

  tail_view = await build_session_view_data(session.id, mgr, thread_mgr, message_limit=2)
  assert tail_view.total_event_count == 8
  assert tail_view.has_more is True
  assert full_view.oldest_message_ordinal == 5
  assert tail_view.oldest_message_ordinal == 6
  assert [m["event_index"] for m in tail_view.messages] == [6, 7]


@pytest.mark.asyncio
async def test_session_bootstrap_uses_tail_without_thread_or_usage_load(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))
  # Turns of (user, separator) so tail(2) returns exactly the last turn.
  events = []
  for i in range(4):
    events.append({"type": ET.USER, "content": f"e{i}", "timestamp": f"2026-05-10T00:0{i}:00Z"})
    events.append({"type": ET.MASTER_DONE, "thinking_seconds": 1, "timestamp": f"2026-05-10T00:0{i}:30Z"})
  _append_events(mgr.get_chat_events_path(session.id), events)

  bootstrap = await build_session_bootstrap_data(session.id, mgr, message_limit=2)

  assert bootstrap.session.id == session.id
  assert bootstrap.total_event_count == 8
  assert bootstrap.has_more is True
  assert bootstrap.oldest_message_ordinal == 6
  assert [m["role"] for m in bootstrap.messages] == ["user", "separator"]
  assert bootstrap.messages[0]["content"] == "e3"
  assert [m["event_index"] for m in bootstrap.messages] == [6, 7]


@pytest.mark.asyncio
async def test_events_page_returns_raw_next_before_for_aggregated_messages(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))
  events = [
      {
          "type": ET.TOOL_USE,
          "id": "tool-0",
          "name": "Read",
          "input": {
              "file_path": "a.txt"
          }
      },
      {
          "type": ET.USER,
          "id": "tool-result-1",
          "message": {
              "content": [{
                  "type": "tool_result",
                  "content": "ok"
              }]
          }
      },
      {
          "type": ET.ASSISTANT,
          "id": "assistant-2",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "done"
              }]
          }
      },
  ]
  _append_events(mgr.get_chat_events_path(session.id), events)

  # before is a message ordinal (exclusive upper bound); limit is a message count.
  # The 3 events aggregate into a single still-unflushed assistant draft. The
  # draft belongs to the streaming-preview surface, not to the bubble list, so
  # the committed-message ordinal domain is empty and the page is empty.
  page = await get_session_events_page(session.id, before=3, limit=3, meta=session, session_mgr=mgr)

  assert page["next_before"] == 0
  assert page["messages"] == []
  assert page["has_more"] is False
