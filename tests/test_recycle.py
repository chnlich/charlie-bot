"""Tests for SessionManager.recycle_scheduled_session and global event_index."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, ThreadMetadata, ThreadStatus
from src.core.sessions import SessionManager


def _write_thread(threads_dir: Path, thread_id: str, status: ThreadStatus,
                  completed_at: datetime | None) -> None:
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


def _append_events(path: Path, events: list[dict]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    for ev in events:
      f.write(json.dumps(ev) + "\n")


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

  base = datetime(2026, 5, 10, 0, 0, 0, tzinfo=timezone.utc)
  cutoff = base + timedelta(days=3)
  events = [
      {"type": "user", "content": f"e{i}", "timestamp": (base + timedelta(hours=i)).isoformat()}
      for i in range(5)
  ]
  events += [
      {"type": "user", "content": f"f{i}", "timestamp": (cutoff + timedelta(hours=i)).isoformat()}
      for i in range(3)
  ]
  live_path = mgr.get_chat_events_path(session.id)
  _append_events(live_path, events)

  result = await mgr.recycle_scheduled_session(session.id, cutoff)

  assert result["events_archived"] == 5
  archive_path = Path(result["archive_file"])
  assert archive_path.exists()
  iso = cutoff.isocalendar()
  assert archive_path.name == f"chat_events.{iso.year}-W{iso.week:02d}.jsonl"

  archive_lines = [json.loads(l) for l in archive_path.read_text(encoding="utf-8").splitlines() if l.strip()]
  assert [e["content"] for e in archive_lines] == [f"e{i}" for i in range(5)]

  live_lines = [json.loads(l) for l in live_path.read_text(encoding="utf-8").splitlines() if l.strip()]
  assert [e["content"] for e in live_lines] == [f"f{i}" for i in range(3)]

  meta = await mgr.get_session(session.id)
  assert meta is not None
  assert meta.archive_offset == 5

  # Subsequent persist_and_broadcast must continue the global numbering.
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(
        session.id,
        {"type": "user", "content": "after", "timestamp": (cutoff + timedelta(days=1)).isoformat()},
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
      {"type": "user", "content": "future", "timestamp": (cutoff + timedelta(hours=1)).isoformat()},
  ]
  live_path = mgr.get_chat_events_path(session.id)
  _append_events(live_path, events)

  result = await mgr.recycle_scheduled_session(session.id, cutoff)

  assert result["events_archived"] == 0
  assert result["archive_file"] is None
  live_lines = [json.loads(l) for l in live_path.read_text(encoding="utf-8").splitlines() if l.strip()]
  assert [e["content"] for e in live_lines] == ["future"]

  meta = await mgr.get_session(session.id)
  assert meta is not None
  assert meta.archive_offset == 0


@pytest.mark.asyncio
async def test_load_chat_events_range_spans_archive_and_live(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  base = datetime(2026, 5, 10, 0, 0, 0, tzinfo=timezone.utc)
  cutoff = base + timedelta(days=3)
  events = [
      {"type": "user", "content": f"e{i}", "timestamp": (base + timedelta(hours=i)).isoformat()}
      for i in range(5)
  ]
  events += [
      {"type": "user", "content": f"f{i}", "timestamp": (cutoff + timedelta(hours=i)).isoformat()}
      for i in range(3)
  ]
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
