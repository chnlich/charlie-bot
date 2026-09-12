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

from src.api.message_utils import SessionBootstrapData, build_session_bootstrap_data, build_session_view_data
from src.api.sessions import _bootstrap_payload, get_session_events_page
from src.core import event_types as ET
from src.core.models import SessionMetadata, ThreadMetadata, ThreadStatus
from src.core.ndjson import count_ndjson_lines
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


async def _walk_rig(tmp_path: Path) -> tuple[SessionManager, SessionMetadata, Path, datetime]:
  """One recycled session named "t" whose live log holds f0..f8: f0..f2 survive
  recycle, f3..f8 carry timestamps past the archive cutoff. Global event index 8
  holds f3, so a range window starting at 9 reads f4 and up."""
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, live_path = await recycle_archive_cutoff_events(mgr, session.id)
  _append_events(
      live_path, [
          {
              "type": "user",
              "content": f"f{i}",
              "timestamp": (cutoff + timedelta(days=2, hours=i)).isoformat()
          } for i in range(3, 9)
      ])
  return mgr, session, live_path, cutoff


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
async def test_archive_range_multi_file_matches_full_concatenation(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, live_path = await recycle_archive_cutoff_events(mgr, session.id)

  # A second recycle at a later ISO week writes a second archive file; the
  # f0..f2 events the first recycle left live are the ones it archives.
  second_cutoff = cutoff + timedelta(days=10)
  await mgr.recycle_scheduled_session(session.id, second_cutoff)
  _append_events(
      live_path,
      [
          {
              "type": "user",
              "content": f"l{i}",
              "timestamp": (second_cutoff + timedelta(hours=i)).isoformat()
          } for i in range(3)
      ],
  )

  archives_dir = live_path.parent / "archives"
  reference: list[str] = []
  for archive in sorted(archives_dir.glob("chat_events.*.jsonl")):
    reference.extend(
        json.loads(line)["content"] for line in archive.read_text(encoding="utf-8").splitlines() if line.strip())
  live_contents = [
      json.loads(line)["content"] for line in live_path.read_text(encoding="utf-8").splitlines() if line.strip()
  ]
  reference += live_contents
  assert len(list(archives_dir.glob("chat_events.*.jsonl"))) == 2, "corpus must span two archive files"

  for start, end in [(0, 3), (0, 8), (3, 8), (4, 10), (5, 8), (6, 100), (8, 11), (9, 9), (0, 0), (11, 12), (20, 30)]:
    events, has_more = mgr.load_chat_events_range(session.id, start, end)
    assert [e["content"] for e in events] == reference[start:end], (start, end)
    assert has_more is (start > 0)


@pytest.mark.asyncio
async def test_archive_files_memo_picks_up_new_archive_file(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, _live_path = await recycle_archive_cutoff_events(mgr, session.id)

  first, _ = mgr.load_chat_events_range(session.id, 0, 5)
  assert [e["content"] for e in first] == [f"e{i}" for i in range(5)]

  # A new weekly archive file (a directory-entry change, not a same-week
  # append) must invalidate the memoized name list on the next read.
  second_cutoff = cutoff + timedelta(days=10)
  await mgr.recycle_scheduled_session(session.id, second_cutoff)

  second, _ = mgr.load_chat_events_range(session.id, 0, 8)
  assert [e["content"] for e in second] == [f"e{i}" for i in range(5)] + [f"f{i}" for i in range(3)]


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


class _CountingByteReader:
  """open() wrapper recording every read()'s byte count into a shared tally."""

  def __init__(self, inner: IO[bytes], reads: list[int]) -> None:
    self._inner = inner
    self._reads = reads

  def read(self, *args: Any, **kwargs: Any) -> bytes:
    data: bytes = self._inner.read(*args, **kwargs)
    self._reads.append(len(data))
    return data

  def seek(self, *args: Any, **kwargs: Any) -> int:
    return self._inner.seek(*args, **kwargs)

  def __enter__(self) -> "_CountingByteReader":
    self._inner.__enter__()
    return self

  def __exit__(self, *args: Any, **kwargs: Any) -> bool:
    return bool(self._inner.__exit__(*args, **kwargs))


def _count_reads_of(live_path: Path, real_open: Any, reads: list[int]) -> Any:
  """A builtins.open patch tallying byte reads of *live_path* into *reads*."""

  def counting_open(file: Any, *args: Any, **kwargs: Any) -> IO[bytes] | _CountingByteReader:
    handle = real_open(file, *args, **kwargs)
    if str(file) == str(live_path):
      return _CountingByteReader(handle, reads)
    return handle

  return counting_open


@pytest.mark.asyncio
async def test_live_range_walk_serves_tail_window_without_full_read(tmp_path: Path) -> None:
  mgr, session, live_path, _cutoff = await _walk_rig(tmp_path)
  file_size = live_path.stat().st_size
  count_ndjson_lines(live_path)  # the bootstrap's tail read warms the count memo first

  real_open = open
  reads: list[int] = []
  with patch("builtins.open", _count_reads_of(live_path, real_open, reads)), \
          patch("src.core.chat_events._WALK_CHUNK_BYTES", 64):
    got, has_more = mgr.load_chat_events_range(session.id, 9, 11)
  # The walk read the window's tail span, not the whole file.
  assert 0 < sum(reads) < file_size
  assert [e["content"] for e in got] == ["f4", "f5"]
  assert has_more is True

  # The walked suffix entry serves the repeat with zero file bytes.
  reads.clear()
  with patch("builtins.open", _count_reads_of(live_path, real_open, reads)):
    again, _ = mgr.load_chat_events_range(session.id, 9, 11)
  assert reads == []
  assert [e["content"] for e in again] == ["f4", "f5"]


@pytest.mark.asyncio
async def test_live_range_backward_extension_serves_scroll_below_walked_window(tmp_path: Path) -> None:
  mgr, session, live_path, _cutoff = await _walk_rig(tmp_path)
  file_size = live_path.stat().st_size
  count_ndjson_lines(live_path)

  first, _ = mgr.load_chat_events_range(session.id, 9, 11)
  assert [e["content"] for e in first] == ["f4", "f5"]

  real_open = open
  reads: list[int] = []
  with patch("builtins.open", _count_reads_of(live_path, real_open, reads)), \
          patch("src.core.chat_events._WALK_CHUNK_BYTES", 64):
    second, _ = mgr.load_chat_events_range(session.id, 7, 9)
  # The backward extension read the page's span, not the whole file.
  assert 0 < sum(reads) < file_size
  assert [e["content"] for e in second] == ["f2", "f3"]

  reads.clear()
  with patch("builtins.open", _count_reads_of(live_path, real_open, reads)):
    repeat, _ = mgr.load_chat_events_range(session.id, 7, 9)
  assert reads == []
  assert [e["content"] for e in repeat] == ["f2", "f3"]


@pytest.mark.asyncio
async def test_live_range_walk_delete_race_returns_empty_page(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  cutoff, live_path = await recycle_archive_cutoff_events(mgr, session.id)
  _append_events(live_path, [{"type": "user", "content": "f3", "timestamp": (cutoff + timedelta(days=2)).isoformat()}])
  count_ndjson_lines(live_path)

  def delete_mid_count(path: Path) -> int:
    path.unlink()
    raise FileNotFoundError(2, "No such file or directory")

  # A delete landing inside the walk's count bracket must not escape as an
  # exception; the old whole-file build's guarded open returned an empty page.
  with patch("src.core.chat_events.count_ndjson_lines", side_effect=delete_mid_count):
    got, _has_more = mgr.load_chat_events_range(session.id, 6, 8)
  assert got == []


@pytest.mark.asyncio
async def test_live_range_walk_matches_full_build_across_line_shapes(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  await recycle_archive_cutoff_events(mgr, session.id)
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
  count_ndjson_lines(live_path)
  spec: list[str | None] = ["a0", None, None, "a1", "a2", "a3"]

  # The full build covers every index; the walk-built windows must match it.
  for start, end in [(6, 7), (7, 9), (8, 10), (6, 10), (9, 12), (11, 14), (7, 7)]:
    got, has_more = mgr.load_chat_events_range(session.id, start, end)
    rel0, rel1 = start - 5, end - 5
    expected = [c for c in spec[max(0, rel0):max(0, rel1)] if c is not None]
    assert [e["content"] for e in got] == expected, (start, end)
    assert has_more is (start > 0)


@pytest.mark.asyncio
async def test_live_range_walk_budget_falls_back_to_full_build(tmp_path: Path) -> None:
  mgr, session, live_path, _cutoff = await _walk_rig(tmp_path)
  file_size = live_path.stat().st_size
  count_ndjson_lines(live_path)

  real_open = open
  reads: list[int] = []
  with patch("builtins.open", _count_reads_of(live_path, real_open, reads)), \
          patch("src.core.chat_events._WALK_BYTE_BUDGET", 128), \
          patch("src.core.chat_events._WALK_CHUNK_BYTES", 64):
    got, _ = mgr.load_chat_events_range(session.id, 9, 11)
  # The over-budget span fell back to the full build, which read the file once.
  assert sum(reads) >= file_size
  assert [e["content"] for e in got] == ["f4", "f5"]


@pytest.mark.asyncio
async def test_live_range_walk_entry_extends_after_append(tmp_path: Path) -> None:
  mgr, session, live_path, cutoff = await _walk_rig(tmp_path)
  count_ndjson_lines(live_path)

  first, _ = mgr.load_chat_events_range(session.id, 9, 11)
  assert [e["content"] for e in first] == ["f4", "f5"]

  stamp = (cutoff + timedelta(days=2, hours=9)).isoformat()
  appended = json.dumps({"type": "user", "content": "f9", "timestamp": stamp}) + "\n"
  _append_events(live_path, [{"type": "user", "content": "f9", "timestamp": stamp}])

  real_open = open
  reads: list[int] = []
  with patch("builtins.open", _count_reads_of(live_path, real_open, reads)):
    after, _ = mgr.load_chat_events_range(session.id, 9, 15)
  # The extension parsed the appended tail only.
  assert reads == [len(appended.encode("utf-8"))]
  assert [e["content"] for e in after] == ["f4", "f5", "f6", "f7", "f8", "f9"]

  reads.clear()
  with patch("builtins.open", _count_reads_of(live_path, real_open, reads)):
    repeat, _ = mgr.load_chat_events_range(session.id, 9, 15)
  assert reads == []
  assert [e["content"] for e in repeat] == ["f4", "f5", "f6", "f7", "f8", "f9"]


@pytest.mark.asyncio
async def test_unarchived_range_serves_warm_events_cache_without_disk_read(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  live_path = mgr.get_chat_events_path(session.id)
  _append_events(
      live_path,
      [{
          "type": "user",
          "content": f"c{i}",
          "timestamp": _archive_cutoff_events()[0].isoformat()
      } for i in range(4)])

  cold, _ = mgr.load_chat_events_range(session.id, 0, 4)
  warm = mgr.load_chat_events_sync(session.id)
  assert len(cold) == 4

  real_open = open
  live_opens = []

  def counting_open(file, *args, **kwargs):
    if str(file) == str(live_path):
      live_opens.append(str(file))
    return real_open(file, *args, **kwargs)

  with patch("builtins.open", counting_open):
    got, has_more = mgr.load_chat_events_range(session.id, 1, 4)

  assert live_opens == [], "a warm events cache must serve the unarchived range without re-reading the file"
  assert [e["content"] for e in got] == [e["content"] for e in warm[1:4]] == ["c1", "c2", "c3"]
  assert has_more is True


@pytest.mark.asyncio
async def test_unarchived_range_cache_slice_sees_funnel_appends(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  ts = _archive_cutoff_events()[0].isoformat()
  mgr.load_chat_events_sync(session.id)  # warm the cache on an empty live file

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    for i in range(3):
      await mgr.save_chat_event(session.id, {"type": "user", "content": f"c{i}", "timestamp": ts})

  got, _ = mgr.load_chat_events_range(session.id, 0, 3)
  assert [e["content"] for e in got] == ["c0", "c1", "c2"]


@pytest.mark.asyncio
async def test_unarchived_range_cache_slice_keeps_parsed_event_domain(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  live_path = mgr.get_chat_events_path(session.id)
  # A malformed line between good ones: the disk range read (physical-line
  # islice) would drop the tail event from a count-sized window; the cache's
  # parsed-event index must not.
  live_path.write_text(
      '{"type": "user", "content": "c0"}\n{bad json\n'
      '{"type": "user", "content": "c1"}\n{"type": "user", "content": "c2"}\n',
      encoding="utf-8")

  warm = mgr.load_chat_events_sync(session.id)
  assert len(warm) == 3
  count = mgr.get_chat_event_count_sync(session.id)
  assert count == 3, "the warm count is the cache's parsed-event count"

  got, _ = mgr.load_chat_events_range(session.id, 0, count)
  assert [e["content"] for e in got] == ["c0", "c1", "c2"]


@pytest.mark.asyncio
async def test_session_view_uses_global_event_indices_after_archive(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  await recycle_archive_cutoff_events(mgr, session.id)

  full_view = await build_session_view_data(session.id, mgr, [])
  assert full_view.total_event_count == 8
  assert full_view.has_more is True
  assert [m["event_index"] for m in full_view.messages] == [5, 6, 7]

  tail_view = await build_session_view_data(session.id, mgr, [], message_limit=2)
  assert tail_view.total_event_count == 8
  assert tail_view.has_more is True
  assert full_view.oldest_message_ordinal == 5
  assert tail_view.oldest_message_ordinal == 6
  assert [m["event_index"] for m in tail_view.messages] == [6, 7]


@pytest.mark.asyncio
async def test_session_view_projection_miss_falls_back_to_tail_events(tmp_path: Path) -> None:
  """A projection miss must still serve the legacy tail-events page.

  get_message_projection re-reads archive_offset from disk and returns None
  when the session is archived between the metadata snapshot the view build
  holds and the threaded projection read; the view build must then take the
  tail-events path instead of serving an unbound payload.
  """
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  events = [{"type": ET.USER, "content": f"e{i}", "timestamp": f"2026-05-10T00:0{i}:00Z"} for i in range(4)]
  _append_events(mgr.get_chat_events_path(session.id), events)

  with patch.object(mgr, "get_message_projection", return_value=None):
    view = await build_session_view_data(session.id, mgr, [], message_limit=2)

  assert [m["content"] for m in view.messages] == ["e2", "e3"]
  assert view.total_event_count == 4
  assert view.has_more is True


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


def projection_messages(mgr: SessionManager, session_id: str) -> list[dict]:
  projection = mgr.get_message_projection(session_id)
  assert projection is not None
  messages, _oldest, _more = projection.tail(40)
  return messages


def _switch_payload_messages(mgr: SessionManager, session: SessionMetadata) -> tuple[list[dict], list[dict]]:
  """The bootstrap's messages and the switch payload's trimmed copy of them."""
  messages = projection_messages(mgr, session.id)
  bootstrap = SessionBootstrapData(
      session=session,
      messages=messages,
      pending_draft=None,
      total_event_count=0,
      oldest_message_ordinal=0,
      has_more=False)
  payload = _bootstrap_payload(bootstrap, mgr._cfg)
  return messages, payload["messages"]


@pytest.mark.asyncio
async def test_bootstrap_payload_trims_tool_previews_over_cap(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  big_output = "o" * (500 + 40000)
  big_command = "git " + "x" * (500 + 40000)
  events = [
      {
          "type": ET.TOOL_USE,
          "id": "tool-0",
          "name": "Read",
          "input": {
              "file_path": "a.txt"
          },
          "timestamp": "2026-05-10T00:00:00Z",
      },
      {
          "type": ET.USER,
          "id": "tool-result-1",
          "message": {
              "content": [{
                  "type": "tool_result",
                  "content": big_output
              }]
          },
          "timestamp": "2026-05-10T00:00:01Z",
      },
      {
          "type": ET.TOOL_USE,
          "id": "tool-1",
          "name": "Bash",
          "input": {
              "command": big_command
          },
          "timestamp": "2026-05-10T00:00:02Z",
      },
      {
          "type": ET.USER,
          "id": "tool-result-2",
          "message": {
              "content": [{
                  "type": "tool_result",
                  "content": "ok"
              }]
          },
          "timestamp": "2026-05-10T00:00:03Z",
      },
      {
          "type": ET.ASSISTANT,
          "id": "assistant-3",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "done"
              }]
          },
          "timestamp": "2026-05-10T00:00:04Z",
      },
      {
          "type": ET.MASTER_DONE,
          "thinking_seconds": 1,
          "timestamp": "2026-05-10T00:00:05Z",
      },
  ]
  _append_events(mgr.get_chat_events_path(session.id), events)

  projection_messages_before = projection_messages(mgr, session.id)
  messages, payload_messages = _switch_payload_messages(mgr, session)
  tools = payload_messages[0]["tools"]

  assert tools[0]["output"] == big_output[:500]
  assert tools[0]["output_truncated"] is True
  assert tools[1]["input"]["command"] == big_command[:500]
  assert tools[1]["input_truncated"] is True

  # The trimmed copies never reach the projection memo: a re-read after the
  # payload build returns dicts identical to the pre-payload read, full
  # projection text included (the M94 render cap's own 20000-char shape).
  assert projection_messages(mgr, session.id) == projection_messages_before


@pytest.mark.asyncio
async def test_bootstrap_payload_leaves_small_tools_untouched(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")
  events = [
      {
          "type": ET.TOOL_USE,
          "id": "tool-0",
          "name": "Read",
          "input": {
              "file_path": "a.txt"
          },
          "timestamp": "2026-05-10T00:00:00Z",
      },
      {
          "type": ET.USER,
          "id": "tool-result-1",
          "message": {
              "content": [{
                  "type": "tool_result",
                  "content": "x" * 499
              }]
          },
          "timestamp": "2026-05-10T00:00:01Z",
      },
      {
          "type": ET.ASSISTANT,
          "id": "assistant-2",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "done"
              }]
          },
          "timestamp": "2026-05-10T00:00:02Z",
      },
      {
          "type": ET.MASTER_DONE,
          "thinking_seconds": 1,
          "timestamp": "2026-05-10T00:00:03Z",
      },
  ]
  _append_events(mgr.get_chat_events_path(session.id), events)

  messages, payload_messages = _switch_payload_messages(mgr, session)

  assert payload_messages[0] is messages[0]
  assert "output_truncated" not in payload_messages[0]["tools"][0]
  assert "input_truncated" not in payload_messages[0]["tools"][0]


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
