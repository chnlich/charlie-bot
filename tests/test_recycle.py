"""Tests for SessionManager.recycle_history_before and global event_index."""

from __future__ import annotations

import datetime
import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import ndjson
from src.runtime.api import message_utils


@pytest.mark.asyncio
async def test_recycle_archives_old_chat_events_and_advances_offset(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  cutoff, events = conftest.archive_cutoff_events()
  conftest.backdate_task_created_event(mgr, session.id, cutoff - datetime.timedelta(days=1))
  live_path = mgr.events.get_chat_events_path(session.id)
  conftest.append_events(live_path, events)

  result = await mgr.recycle_history_before(session.id, cutoff)

  assert result["events_archived"] == 6
  archive_path = pathlib.Path(result["archive_file"])
  assert archive_path.exists()
  iso = cutoff.isocalendar()
  assert archive_path.name == f"chat_events.{iso.year}-W{iso.week:02d}.jsonl"

  archive_lines = [json.loads(line) for line in archive_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert archive_lines[0]["type"] == ET.TASK_CREATED
  assert [e["content"] for e in archive_lines if e.get("type") == "user"] == [f"e{i}" for i in range(5)]

  live_lines = [json.loads(line) for line in live_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert [e["content"] for e in live_lines] == [f"f{i}" for i in range(3)]

  meta = await mgr.store.get_session(session.id)
  assert meta is not None
  assert meta.archive_offset == 6

  # Subsequent persist_and_broadcast must continue the global numbering.
  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr.events.persist_and_broadcast(
        session.id,
        {
            "type": "user",
            "content": "after",
            "timestamp": (cutoff + datetime.timedelta(days=1)).isoformat()
        },
    )
  msg_payloads = [c.args[1] for c in broadcast_mock.await_args_list if c.args[1].get("type") == "message"]
  assert msg_payloads, "expected at least one message delta"
  # Live has 3 retained tail events + 1 new = 4 lines after six archived rows.
  assert msg_payloads[0]["message"]["event_index"] == 9


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
  live_path = mgr.events.get_chat_events_path(session.id)
  conftest.append_events(live_path, events)

  result = await mgr.recycle_history_before(session.id, cutoff)

  assert result["events_archived"] == 0
  assert result["archive_file"] is None
  live_lines = [json.loads(line) for line in live_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert [e["content"] for e in live_lines if e.get("type") == "user"] == ["future"]
  assert live_lines[0]["type"] == ET.TASK_CREATED

  meta = await mgr.store.get_session(session.id)
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
  with mock.patch("src.infra.ndjson.count_ndjson_lines", side_effect=delete_mid_count):
    got, _has_more = mgr.events.load_chat_events_range(session.id, 7, 9)
  assert got == []


@pytest.mark.asyncio
async def test_live_range_walk_matches_full_build_across_line_shapes(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")
  await conftest.recycle_archive_cutoff_events(mgr, session.id)
  live_path = mgr.events.get_chat_events_path(session.id)
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
  for start, end in [(7, 8), (8, 10), (9, 11), (7, 11), (10, 13), (12, 15), (8, 8)]:
    got, has_more = mgr.events.load_chat_events_range(session.id, start, end)
    rel0, rel1 = start - 6, end - 6
    expected = [c for c in spec[max(0, rel0):max(0, rel1)] if c is not None]
    assert [e["content"] for e in got] == expected, (start, end)
    assert has_more is (start > 0)


@pytest.mark.asyncio
async def test_session_bootstrap_uses_global_event_indices_after_archive(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")
  await conftest.recycle_archive_cutoff_events(mgr, session.id)

  full_view = await message_utils.build_session_bootstrap_data(session.id, mgr.store, mgr.events)
  assert full_view.total_event_count == 9
  assert full_view.has_more is True
  assert [m["event_index"] for m in full_view.messages] == [6, 7, 8]

  tail_view = await message_utils.build_session_bootstrap_data(session.id, mgr.store, mgr.events, message_limit=2)
  assert tail_view.total_event_count == 9
  assert tail_view.has_more is True
  assert full_view.oldest_message_ordinal == 6
  assert tail_view.oldest_message_ordinal == 7
  assert [m["event_index"] for m in tail_view.messages] == [7, 8]
