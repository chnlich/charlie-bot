"""Tests for SessionManager.recycle_scheduled_session and global event_index."""

from __future__ import annotations

import datetime
import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.api import message_utils
from src.core import models, ndjson


def _write_thread(
    threads_dir: pathlib.Path, thread_id: str, status: models.ThreadStatus,
    completed_at: datetime.datetime | None) -> None:
  thread_dir = threads_dir / thread_id
  thread_dir.mkdir(parents=True, exist_ok=True)
  meta = models.ThreadMetadata(
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
async def test_recycle_deletes_only_old_terminal_threads(tmp_path: pathlib.Path) -> None:
  cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")
  threads_dir = cfg.sessions_dir / session.id / "threads"

  now = datetime.datetime.now(datetime.UTC)
  cutoff = now - datetime.timedelta(days=7)
  old = cutoff - datetime.timedelta(days=1)
  recent = cutoff + datetime.timedelta(days=1)

  _write_thread(threads_dir, "old-completed", models.ThreadStatus.COMPLETED, old)
  _write_thread(threads_dir, "old-failed", models.ThreadStatus.FAILED, old)
  _write_thread(threads_dir, "old-cancelled", models.ThreadStatus.CANCELLED, old)
  _write_thread(threads_dir, "recent-completed", models.ThreadStatus.COMPLETED, recent)
  _write_thread(threads_dir, "running", models.ThreadStatus.RUNNING, None)
  _write_thread(threads_dir, "idle", models.ThreadStatus.IDLE, None)
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
async def test_recycle_archives_old_chat_events_and_advances_offset(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  cutoff, events = conftest.archive_cutoff_events()
  live_path = mgr.get_chat_events_path(session.id)
  conftest.append_events(live_path, events)

  result = await mgr.recycle_scheduled_session(session.id, cutoff)

  assert result["events_archived"] == 5
  archive_path = pathlib.Path(result["archive_file"])
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
  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr.persist_and_broadcast(
        session.id,
        {
            "type": "user",
            "content": "after",
            "timestamp": (cutoff + datetime.timedelta(days=1)).isoformat()
        },
    )
  msg_payloads = [c.args[1] for c in broadcast_mock.await_args_list if c.args[1].get("type") == "message"]
  assert msg_payloads, "expected at least one message delta"
  # Live now has 3 retained tail events + 1 new = 4 lines; global = 5 + 4 - 1 = 8.
  assert msg_payloads[0]["message"]["event_index"] == 8


@pytest.mark.asyncio
async def test_recycle_noop_when_nothing_old(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  cutoff = datetime.datetime(2026, 5, 10, 0, 0, 0, tzinfo=datetime.UTC)
  events = [
      {
          "type": "user",
          "content": "future",
          "timestamp": (cutoff + datetime.timedelta(hours=1)).isoformat()
      },
  ]
  live_path = mgr.get_chat_events_path(session.id)
  conftest.append_events(live_path, events)

  result = await mgr.recycle_scheduled_session(session.id, cutoff)

  assert result["events_archived"] == 0
  assert result["archive_file"] is None
  live_lines = [json.loads(line) for line in live_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert [e["content"] for e in live_lines] == ["future"]

  meta = await mgr.get_session(session.id)
  assert meta is not None
  assert meta.archive_offset == 0


@pytest.mark.asyncio
async def test_live_range_walk_delete_race_returns_empty_page(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")
  cutoff, live_path = await conftest.recycle_archive_cutoff_events(mgr, session.id)
  conftest.append_events(
      live_path, [{
          "type": "user",
          "content": "f3",
          "timestamp": (cutoff + datetime.timedelta(days=2)).isoformat()
      }])
  ndjson.count_ndjson_lines(live_path)

  def delete_mid_count(path: pathlib.Path) -> int:
    path.unlink()
    raise FileNotFoundError(2, "No such file or directory")

  # A delete landing inside the walk's count bracket must not escape as an
  # exception: the read returns an empty page.
  with mock.patch("src.core.ndjson.count_ndjson_lines", side_effect=delete_mid_count):
    got, _has_more = mgr.load_chat_events_range(session.id, 6, 8)
  assert got == []


@pytest.mark.asyncio
async def test_live_range_walk_matches_full_build_across_line_shapes(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")
  await conftest.recycle_archive_cutoff_events(mgr, session.id)
  live_path = mgr.get_chat_events_path(session.id)
  # blank and malformed lines consume an index, a CRLF pair terminates one
  # line, and the final line carries no newline.
  live_path.write_bytes(
      b'{"content": "a0"}\n'
      b"\n"
      b"{bad json\n"
      b'{"content": "a1"}\r\n'
      b'{"content": "a2"}\n'
      b'{"content": "a3"}')
  ndjson.count_ndjson_lines(live_path)
  spec: list[str | None] = ["a0", None, None, "a1", "a2", "a3"]

  # The full build covers every index; the walk-built windows must match it.
  for start, end in [(6, 7), (7, 9), (8, 10), (6, 10), (9, 12), (11, 14), (7, 7)]:
    got, has_more = mgr.load_chat_events_range(session.id, start, end)
    rel0, rel1 = start - 5, end - 5
    expected = [c for c in spec[max(0, rel0):max(0, rel1)] if c is not None]
    assert [e["content"] for e in got] == expected, (start, end)
    assert has_more is (start > 0)


@pytest.mark.asyncio
async def test_session_bootstrap_uses_global_event_indices_after_archive(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")
  await conftest.recycle_archive_cutoff_events(mgr, session.id)

  full_view = await message_utils.build_session_bootstrap_data(session.id, mgr)
  assert full_view.total_event_count == 8
  assert full_view.has_more is True
  assert [m["event_index"] for m in full_view.messages] == [5, 6, 7]

  tail_view = await message_utils.build_session_bootstrap_data(session.id, mgr, message_limit=2)
  assert tail_view.total_event_count == 8
  assert tail_view.has_more is True
  assert full_view.oldest_message_ordinal == 5
  assert tail_view.oldest_message_ordinal == 6
  assert [m["event_index"] for m in tail_view.messages] == [6, 7]
