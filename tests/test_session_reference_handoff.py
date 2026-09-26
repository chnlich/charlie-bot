"""Tests for clone/elone history handoff into the child's own chat log."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    assistant_text_event,
    make_home_session,
    make_parent,
    recycle_archive_cutoff_events,
    user_event,
)
from conftest import append_events as _append_events
from conftest import archive_cutoff_events as _archive_cutoff_events

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SessionStatus
from src.core.recap import extract_recap
from src.core.sessions import SessionManager


def _read_events(path: Path) -> list[dict]:
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _parent_side_files(cfg: CharlieBotConfig) -> list[Path]:
  """Every parent_*.jsonl side file under the sessions dir; the child-log contract allows none."""
  return sorted(cfg.sessions_dir.glob("*/data/parent_*.jsonl"))


def _assert_child_log_is_parent_prefix_plus_marker(mgr: SessionManager, parent_id: str, child_id: str,
                                                   end: int) -> list[dict]:
  """The child log holds the parent's events [0, end) then exactly one clone_start marker.

  The parent side drops any in-memory event_index stamp (persist_and_broadcast
  injects one onto cache-served dicts; the persisted lines never carried it).
  """
  child_events = _read_events(mgr.get_chat_events_path(child_id))
  parent_prefix = [
      {
          k: v for k, v in event.items() if k != "event_index"
      } for event in mgr.load_chat_events_range(parent_id, 0, end)[0]
  ]
  assert child_events[:end] == parent_prefix
  assert len(child_events) == end + 1
  marker = child_events[end]
  assert marker["type"] == ET.CLONE_START
  assert marker["parent_session_id"] == parent_id
  return child_events


@pytest.mark.asyncio
async def test_fork_session_copies_parent_prefix_and_clone_marker_into_child_log(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await make_parent(mgr)
  # A third event past the fork point proves the copied prefix truncates there.
  _append_events(mgr.get_chat_events_path(parent), [user_event("e2")])

  child = await mgr.fork_session(parent, event_index=1)

  child_events = _assert_child_log_is_parent_prefix_plus_marker(mgr, parent, child.id, end=2)
  assert [event["content"] for event in child_events[:2]] == ["e0", "e1"]


@pytest.mark.asyncio
async def test_child_log_prefix_from_warm_cache_carries_no_in_memory_event_index(tmp_path: Path) -> None:
  """persist_and_broadcast stamps an in-memory-only event_index onto the cached
  event dicts after the disk write; the copied prefix is the parent's raw bytes,
  so a warm parent's child log must not inherit a stamp the persisted lines
  never carried."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await make_parent(mgr)
  for content in ["w0", "w1"]:
    await mgr.persist_and_broadcast(parent, user_event(content))

  child = await mgr.fork_session(parent, event_index=2)

  child_events = _assert_child_log_is_parent_prefix_plus_marker(mgr, parent, child.id, end=3)
  assert [event["content"] for event in child_events[:3]] == ["e0", "e1", "w0"]
  assert all("event_index" not in event for event in child_events)


@pytest.mark.asyncio
async def test_elone_session_copies_parent_prefix_into_child_log_and_archives_parent(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await make_parent(mgr)

  child = await mgr.elone_session(parent, event_index=0)

  child_events = _assert_child_log_is_parent_prefix_plus_marker(mgr, parent, child.id, end=1)
  assert [event["content"] for event in child_events[:1]] == ["e0"]
  updated_parent = await mgr.get_session(parent)
  assert updated_parent is not None
  assert updated_parent.status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_history_handoff_uses_global_event_index_with_archive_offset(tmp_path: Path) -> None:
  _cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend=OPUS_BACKEND_ID)
  await recycle_archive_cutoff_events(mgr, parent.id)

  child = await mgr.fork_session(parent.id, event_index=6)

  child_events = _assert_child_log_is_parent_prefix_plus_marker(mgr, parent.id, child.id, end=7)
  assert [event["content"] for event in child_events[:7]] == ["e0", "e1", "e2", "e3", "e4", "f0", "f1"]


@pytest.mark.asyncio
async def test_fork_child_live_file_holds_archived_parent_lines_with_zero_offset(tmp_path: Path) -> None:
  """The child's archive_offset starts at 0: parent lines the weekly recycle
  already archived land in the child's live file, and the child's own rotation
  moves them later."""
  _cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend=OPUS_BACKEND_ID)
  await recycle_archive_cutoff_events(mgr, parent.id)

  child = await mgr.fork_session(parent.id)

  child_events = _assert_child_log_is_parent_prefix_plus_marker(mgr, parent.id, child.id, end=8)
  assert [event["content"] for event in child_events[:8]] == ["e0", "e1", "e2", "e3", "e4", "f0", "f1", "f2"]
  assert child.archive_offset == 0


@pytest.mark.asyncio
async def test_fork_session_full_copy_streams_parent_raw_lines_into_child_log(tmp_path: Path) -> None:
  _cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend=OPUS_BACKEND_ID)

  cutoff, events = _archive_cutoff_events()
  _append_events(mgr.get_chat_events_path(parent.id), events)
  await mgr.recycle_scheduled_session(parent.id, cutoff)

  child = await mgr.fork_session(parent.id)

  child_raw = mgr.get_chat_events_path(child.id).read_bytes()
  expected_prefix = "".join(json.dumps(event) + "\n" for event in events).encode("utf-8")
  # Byte identity of the prefix: the child log's raw lines are the parent's.
  assert child_raw.startswith(expected_prefix)
  marker_lines = child_raw[len(expected_prefix):].decode("utf-8").splitlines()
  assert len(marker_lines) == 1
  marker = json.loads(marker_lines[0])
  assert marker["type"] == ET.CLONE_START
  assert marker["parent_session_id"] == parent.id


@pytest.mark.asyncio
async def test_history_prefix_uses_read_time_archive_split(tmp_path: Path) -> None:
  _cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend=OPUS_BACKEND_ID)

  cutoff, events = _archive_cutoff_events()
  _append_events(mgr.get_chat_events_path(parent.id), events)
  # The count behind a fork's budget is taken before the prefix read; an
  # archive pass racing between the two moves lines from the live file to the
  # archive tail without changing the event sequence.
  end = mgr.get_chat_event_count_sync(parent.id)
  await mgr.recycle_scheduled_session(parent.id, cutoff)

  prefix_path = tmp_path / "history_prefix.jsonl"
  mgr._write_history_prefix_sync(prefix_path, parent.id, end)
  assert prefix_path.read_text(encoding="utf-8") == "".join(json.dumps(event) + "\n" for event in events)


@pytest.mark.asyncio
async def test_fork_session_rejects_corrupt_parent_line(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend=OPUS_BACKEND_ID)
  events_path = mgr.get_chat_events_path(parent.id)
  _append_events(events_path, [user_event("ok")])
  with open(events_path, "a", encoding="utf-8") as f:
    f.write("{truncated\n")

  chat_logs_before = set(cfg.sessions_dir.glob("*/data/chat_events.jsonl"))
  with pytest.raises(ValueError, match="not a serialized event object"):
    await mgr.fork_session(parent.id)

  assert not _parent_side_files(cfg)
  assert set(cfg.sessions_dir.glob("*/data/chat_events.jsonl")) == chat_logs_before


@pytest.mark.asyncio
async def test_fork_history_prefix_crosses_the_scan_chunk_boundary(tmp_path: Path) -> None:
  """The mmap copy stream sweeps newline positions in 1 MiB chunks; a corpus
  spanning several chunks with frames crossing each boundary must fork
  byte-identically to the JSON lines it carries."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend=OPUS_BACKEND_ID)
  events_path = mgr.get_chat_events_path(parent.id)
  # One line per ~2 KiB until the corpus clears three chunk boundaries, with a
  # frame straddling each: every chunk holds a partial line at both ends.
  filler = "x" * 2048
  events = [user_event(f"{i:04d}-{filler}") for i in range(1600)]
  _append_events(events_path, events)
  assert events_path.stat().st_size > 3 * (1 << 20)

  child = await mgr.fork_session(parent.id)

  child_raw = mgr.get_chat_events_path(child.id).read_bytes()
  expected_prefix = "".join(json.dumps(event) + "\n" for event in events).encode("utf-8")
  assert child_raw.startswith(expected_prefix)
  marker_lines = child_raw[len(expected_prefix):].decode("utf-8").splitlines()
  assert len(marker_lines) == 1
  assert json.loads(marker_lines[0])["type"] == ET.CLONE_START


@pytest.mark.asyncio
async def test_fork_copies_non_ascii_lines_verbatim_and_undecodable_bytes_raise(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend=OPUS_BACKEND_ID)
  events_path = mgr.get_chat_events_path(parent.id)
  _append_events(events_path, [user_event("ok")])
  # A non-ASCII but valid line rides the decode branch (the isascii() proof
  # answers only for ASCII corpora); the copy keeps its raw bytes.
  non_ascii = json.dumps(user_event("中文"), ensure_ascii=False)
  with open(events_path, "a", encoding="utf-8") as f:
    f.write(non_ascii + "\n")

  child = await mgr.fork_session(parent.id)
  expected_prefix = (json.dumps(user_event("ok")) + "\n" + non_ascii + "\n").encode("utf-8")
  child_raw = mgr.get_chat_events_path(child.id).read_bytes()
  assert child_raw.startswith(expected_prefix)
  marker_lines = child_raw[len(expected_prefix):].decode("utf-8").splitlines()
  assert json.loads(marker_lines[0])["type"] == ET.CLONE_START

  # Undecodable bytes raise at fork time, and the failed fork writes no child
  # chat log.
  other = await mgr.create_session(CreateSessionRequest(name="Other"), backend=OPUS_BACKEND_ID)
  _append_events(mgr.get_chat_events_path(other.id), [user_event("ok")])
  with open(mgr.get_chat_events_path(other.id), "ab") as f:
    f.write(b"\xff\xfe\n")
  chat_logs_before = set(cfg.sessions_dir.glob("*/data/chat_events.jsonl"))

  with pytest.raises(UnicodeDecodeError):
    await mgr.fork_session(other.id)

  assert not _parent_side_files(cfg)
  assert set(cfg.sessions_dir.glob("*/data/chat_events.jsonl")) == chat_logs_before


@pytest.mark.asyncio
async def test_history_handoff_errors_write_no_child_log(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)

  with pytest.raises(FileNotFoundError):
    await mgr.fork_session("missing", event_index=0)
  assert not _parent_side_files(cfg)
  # No session exists yet, so no chat log either.
  assert not list(cfg.sessions_dir.glob("*/data/chat_events.jsonl"))

  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend=OPUS_BACKEND_ID)
  _append_events(mgr.get_chat_events_path(parent.id), [user_event("only")])
  chat_logs_before = set(cfg.sessions_dir.glob("*/data/chat_events.jsonl"))

  with pytest.raises(ValueError, match="out of range"):
    await mgr.elone_session(parent.id, event_index=1)

  assert not _parent_side_files(cfg)
  assert set(cfg.sessions_dir.glob("*/data/chat_events.jsonl")) == chat_logs_before
  updated_parent = await mgr.get_session(parent.id)
  assert updated_parent is not None
  assert updated_parent.status == SessionStatus.ACTIVE


@pytest.mark.asyncio
async def test_clone_of_clone_keeps_the_grandparent_history_in_order(tmp_path: Path) -> None:
  """A clone of a clone must not lose the grandparent's history: the grandchild
  log holds the grandparent's events, the first marker, the child's own events,
  then the second marker, in that order."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  grandparent = await make_parent(mgr)
  _append_events(mgr.get_chat_events_path(grandparent), [user_event("g2"), user_event("g3")])

  child = await mgr.fork_session(grandparent, event_index=1)
  _append_events(mgr.get_chat_events_path(child.id), [user_event("c0"), user_event("c1")])
  mgr._chat_events.clear_cache(child.id)
  grandchild = await mgr.fork_session(child.id)

  grandchild_events = _read_events(mgr.get_chat_events_path(grandchild.id))
  assert [event.get("content") for event in grandchild_events] == ["e0", "e1", None, "c0", "c1", None]
  first_marker, second_marker = grandchild_events[2], grandchild_events[5]
  assert first_marker["type"] == ET.CLONE_START and first_marker["parent_session_id"] == grandparent
  assert second_marker["type"] == ET.CLONE_START and second_marker["parent_session_id"] == child.id


@pytest.mark.asyncio
async def test_has_completed_round_scopes_to_the_childs_own_segment(tmp_path: Path) -> None:
  """The parent's copied master_done events are the parent's rounds, not the
  child's; only a master_done after the child's own clone_start marker counts.
  A log without a marker counts every master_done, as before."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend=OPUS_BACKEND_ID)
  _append_events(mgr.get_chat_events_path(parent.id), [user_event("q"), {"type": ET.MASTER_DONE, "exit_code": 0}])
  assert await mgr.has_completed_round(parent.id) is True

  child = await mgr.fork_session(parent.id)
  assert await mgr.has_completed_round(child.id) is False

  _append_events(mgr.get_chat_events_path(child.id), [user_event("next")])
  mgr._chat_events.clear_cache(child.id)
  assert await mgr.has_completed_round(child.id) is False

  _append_events(mgr.get_chat_events_path(child.id), [{"type": ET.MASTER_DONE, "exit_code": 0}])
  mgr._chat_events.clear_cache(child.id)
  assert await mgr.has_completed_round(child.id) is True


@pytest.mark.asyncio
async def test_history_bootstraps_are_not_divider_recap_asks(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="Child", backend=OPUS_BACKEND_ID)
  _append_events(
      mgr.get_chat_events_path(session.id),
      [
          user_event("This session continues a prior conversation.\n\nbootstrap"),
          user_event("You're taking over because the user wasn't satisfied with the previous session. bootstrap"),
          user_event("real ask"),
          assistant_text_event("real answer"),
      ],
  )

  recap = extract_recap(mgr, session.id)

  assert recap["asks"] == ["real ask"]
  assert recap["last"] == {
      "user": "real ask",
      "assistant": "real answer",
  }
