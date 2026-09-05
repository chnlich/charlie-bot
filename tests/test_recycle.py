"""Tests for SessionManager.recycle_scheduled_session and global event_index."""

from __future__ import annotations

import json
import os
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import IO, Any
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
  BROADCAST_PATCH_TARGET,
  make_home_session,
  recycle_archive_cutoff_events,
)
from conftest import append_events as _append_events
from conftest import archive_cutoff_events as _archive_cutoff_events

from src.api.message_utils import build_session_bootstrap_data, build_session_view_data
from src.api.sessions import get_session_events_page
from src.core import event_types as ET
from src.core.models import ThreadMetadata, ThreadStatus


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
  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  threads_dir = cfg.sessions_dir / session.id / "threads"

  now = datetime.now(UTC)
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
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")

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
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock:
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
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")

  cutoff = datetime(2026, 5, 10, 0, 0, 0, tzinfo=UTC)
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
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  await recycle_archive_cutoff_events(mgr, session.id)

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
async def test_archive_range_repeat_reads_reuse_memo(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  await recycle_archive_cutoff_events(mgr, session.id)

  first, _ = mgr.load_chat_events_range(session.id, 0, 3)

  real_open = open
  archive_opens = []

  def counting_open(file, *args, **kwargs):
    if "archives" in str(file):
      archive_opens.append(str(file))
    return real_open(file, *args, **kwargs)

  with patch("builtins.open", counting_open):
    second, _ = mgr.load_chat_events_range(session.id, 0, 3)

  assert [e["content"] for e in second] == [e["content"] for e in first] == ["e0", "e1", "e2"]
  assert archive_opens == []


@pytest.mark.asyncio
async def test_archive_range_reparses_after_archive_append(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, _ = await recycle_archive_cutoff_events(mgr, session.id)

  before, _ = mgr.load_chat_events_range(session.id, 0, 5)
  assert [e["content"] for e in before] == [f"e{i}" for i in range(5)]

  # A same-week recycle with a later cutoff appends f0..f2 to the existing
  # weekly archive; the appended file's changed (mtime, size) must invalidate
  # the memo.
  await mgr.recycle_scheduled_session(session.id, cutoff + timedelta(days=3))

  after, _ = mgr.load_chat_events_range(session.id, 0, 8)
  assert [e["content"] for e in after] == [f"e{i}" for i in range(5)] + [f"f{i}" for i in range(3)]


@pytest.mark.asyncio
async def test_live_range_repeat_reads_reuse_memo(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  await recycle_archive_cutoff_events(mgr, session.id)

  first, _ = mgr.load_chat_events_range(session.id, 5, 8)

  real_open = open
  live_opens = []

  def counting_open(file, *args, **kwargs):
    if str(file).endswith("chat_events.jsonl") and "archives" not in str(file):
      live_opens.append(str(file))
    return real_open(file, *args, **kwargs)

  with patch("builtins.open", counting_open):
    second, _ = mgr.load_chat_events_range(session.id, 5, 8)

  assert [e["content"] for e in second] == [e["content"] for e in first] == ["f0", "f1", "f2"]
  assert live_opens == []


@pytest.mark.asyncio
async def test_live_range_reparses_after_append(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, live_path = await recycle_archive_cutoff_events(mgr, session.id)

  before, _ = mgr.load_chat_events_range(session.id, 5, 8)
  assert [e["content"] for e in before] == ["f0", "f1", "f2"]

  # An appended live file's changed (mtime, size) must invalidate the memo.
  _append_events(live_path, [{"type": "user", "content": "f3", "timestamp": (cutoff + timedelta(days=2)).isoformat()}])

  after, _ = mgr.load_chat_events_range(session.id, 5, 9)
  assert [e["content"] for e in after] == ["f0", "f1", "f2", "f3"]


@pytest.mark.asyncio
async def test_live_range_append_extends_memo_without_full_reparse(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, live_path = await recycle_archive_cutoff_events(mgr, session.id)

  before, _ = mgr.load_chat_events_range(session.id, 5, 8)
  assert [e["content"] for e in before] == ["f0", "f1", "f2"]

  _append_events(live_path, [{"type": "user", "content": "f3", "timestamp": (cutoff + timedelta(days=2)).isoformat()}])
  appended_line = json.dumps(
      {
          "type": "user",
          "content": "f3",
          "timestamp": (cutoff + timedelta(days=2)).isoformat()
      }) + "\n"

  read_bytes: list[int] = []

  class _CountingReader:

    def __init__(self, inner: IO[bytes]) -> None:
      self._inner = inner

    def read(self, *args: Any, **kwargs: Any) -> bytes:
      data: bytes = self._inner.read(*args, **kwargs)
      read_bytes.append(len(data))
      return data

    def seek(self, *args: Any, **kwargs: Any) -> int:
      return self._inner.seek(*args, **kwargs)

    def __enter__(self) -> "_CountingReader":
      self._inner.__enter__()
      return self

    def __exit__(self, *args: Any, **kwargs: Any) -> bool:
      return bool(self._inner.__exit__(*args, **kwargs))

  real_open = open

  def counting_open(file: Any, *args: Any, **kwargs: Any) -> IO[bytes] | _CountingReader:
    handle = real_open(file, *args, **kwargs)
    if str(file) == str(live_path):
      return _CountingReader(handle)
    return handle

  with patch("builtins.open", counting_open):
    after, _ = mgr.load_chat_events_range(session.id, 5, 9)
  # The extension read the appended tail only, not the whole file.
  assert sum(read_bytes) == len(appended_line.encode("utf-8"))

  assert [e["content"] for e in after] == ["f0", "f1", "f2", "f3"]
  # The extended memo serves a further unchanged repeat with zero file bytes.
  read_bytes.clear()
  with patch("builtins.open", counting_open):
    again, _ = mgr.load_chat_events_range(session.id, 5, 9)
  assert read_bytes == []
  assert [e["content"] for e in again] == ["f0", "f1", "f2", "f3"]


@pytest.mark.asyncio
async def test_live_range_rewrite_with_larger_size_reparses_fully(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, live_path = await recycle_archive_cutoff_events(mgr, session.id)

  before, _ = mgr.load_chat_events_range(session.id, 5, 8)
  assert [e["content"] for e in before] == ["f0", "f1", "f2"]

  # An archive-style rewrite publishes a new inode via os.replace; a larger
  # size must never read as append growth, or the stale prefix would glue onto
  # the new tail.
  replacement = [
      {
          "type": "user",
          "content": f"g{i}",
          "timestamp": (cutoff + timedelta(hours=i)).isoformat()
      } for i in range(6)
  ]
  tmp_sibling = live_path.with_name(live_path.name + ".rewrite-tmp")
  tmp_sibling.write_text("".join(json.dumps(event) + "\n" for event in replacement), encoding="utf-8")
  os.replace(tmp_sibling, live_path)

  after, _ = mgr.load_chat_events_range(session.id, 5, 11)
  assert [e["content"] for e in after] == ["g0", "g1", "g2", "g3", "g4", "g5"]


@pytest.mark.asyncio
async def test_live_range_completed_partial_line_reparses_fully(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, live_path = await recycle_archive_cutoff_events(mgr, session.id)

  before, _ = mgr.load_chat_events_range(session.id, 5, 8)
  assert [e["content"] for e in before] == ["f0", "f1", "f2"]

  # A read that raced a mid-flight append covers a trailing partial line; the
  # completed line must surface once a full re-parse lands on the newline.
  partial = json.dumps({"type": "user", "content": "f3", "timestamp": (cutoff + timedelta(days=2)).isoformat()})
  with open(live_path, "a", encoding="utf-8") as f:
    f.write(partial[:len(partial) // 2])
  torn, _ = mgr.load_chat_events_range(session.id, 5, 9)
  assert [e["content"] for e in torn if e is not None] == ["f0", "f1", "f2"]

  with open(live_path, "a", encoding="utf-8") as f:
    f.write(partial[len(partial) // 2:] + "\n")
  after, _ = mgr.load_chat_events_range(session.id, 5, 9)
  assert [e["content"] for e in after] == ["f0", "f1", "f2", "f3"]


@pytest.mark.asyncio
async def test_live_range_counts_physical_lines(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  _, live_path = await recycle_archive_cutoff_events(mgr, session.id)

  # blank/malformed lines consume a range index, mirroring parse_ndjson_range.
  live_path.write_text('{"content": "f0_first"}\n\n{bad json\n{"content": "f1"}\n{"content": "f2"}\n', encoding="utf-8")

  got, _ = mgr.load_chat_events_range(session.id, 5, 7)
  assert [e["content"] for e in got] == ["f0_first"]
  got, _ = mgr.load_chat_events_range(session.id, 5, 10)
  assert [e["content"] for e in got] == ["f0_first", "f1", "f2"]


@pytest.mark.asyncio
async def test_session_view_uses_global_event_indices_after_archive(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  await recycle_archive_cutoff_events(mgr, session.id)

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
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
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
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
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
  resp = await get_session_events_page(session.id, before=3, limit=3, meta=session, session_mgr=mgr)
  page = json.loads(resp.body)

  assert page["next_before"] == 0
  assert page["messages"] == []
  assert page["has_more"] is False
