"""Tests for clone/elone history handoff into the child's own chat log."""

from __future__ import annotations

import json
import pathlib

import conftest
import pytest

from src.infra import config, models
from src.infra import event_types as ET


def _read_events(path: pathlib.Path) -> list[dict]:
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _parent_side_files(cfg: config.CharlieBotConfig) -> list[pathlib.Path]:
  """Every parent_*.jsonl side file under the sessions dir; the child-log contract allows none."""
  return sorted(cfg.sessions_dir.glob("*/data/parent_*.jsonl"))


def _assert_child_log_is_parent_prefix_marker_and_creation(
    mgr: conftest.SessionBlocks, parent_id: str, child_id: str, end: int) -> list[dict]:
  """The child log holds the parent's events [0, end), one clone_start marker, then the task_created fact.

  The parent side drops any in-memory event_index stamp (persist_and_broadcast
  injects one onto cache-served dicts; the persisted lines never carried it).
  """
  child_events = _read_events(mgr.events.get_chat_events_path(child_id))
  parent_prefix = [
      {
          k: v for k, v in event.items() if k != "event_index"
      } for event in mgr.events.load_chat_events_range(parent_id, 0, end)[0]
  ]
  assert child_events[:end] == parent_prefix
  assert len(child_events) == end + 2
  marker = child_events[end]
  assert marker["type"] == ET.CLONE_START
  assert marker["parent_session_id"] == parent_id
  assert child_events[end + 1]["type"] == ET.TASK_CREATED
  assert child_events[end + 1]["source_session_id"] == child_id
  return child_events


@pytest.mark.asyncio
async def test_fork_session_copies_parent_prefix_and_clone_marker_into_child_log(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = conftest.build_session_blocks(cfg)
  parent = await conftest.make_parent(mgr)
  # A third event past the fork point proves the copied prefix truncates there.
  conftest.append_events(mgr.events.get_chat_events_path(parent), [conftest.user_event("e2")])

  child = await mgr.fork.fork_session(parent, event_index=2)

  child_events = _assert_child_log_is_parent_prefix_marker_and_creation(mgr, parent, child.id, end=3)
  assert [event["content"] for event in child_events[1:3]] == ["e0", "e1"]


@pytest.mark.asyncio
async def test_fork_session_rejects_corrupt_parent_line(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = conftest.build_session_blocks(cfg)
  parent = await conftest.create_root_session(
      mgr, models.CreateSessionRequest(name="Parent"), backend=conftest.OPUS_BACKEND_ID)
  events_path = mgr.events.get_chat_events_path(parent.id)
  conftest.append_events(events_path, [conftest.user_event("ok")])
  with open(events_path, "a", encoding="utf-8") as f:
    f.write("{truncated\n")

  chat_logs_before = set(cfg.sessions_dir.glob("*/data/chat_events.jsonl"))
  with pytest.raises(ValueError, match="not a serialized event object"):
    await mgr.fork.fork_session(parent.id)

  assert not _parent_side_files(cfg)
  assert set(cfg.sessions_dir.glob("*/data/chat_events.jsonl")) == chat_logs_before


@pytest.mark.asyncio
async def test_fork_copies_non_ascii_lines_verbatim_and_undecodable_bytes_raise(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = conftest.build_session_blocks(cfg)
  parent = await conftest.create_root_session(
      mgr, models.CreateSessionRequest(name="Parent"), backend=conftest.OPUS_BACKEND_ID)
  events_path = mgr.events.get_chat_events_path(parent.id)
  conftest.append_events(events_path, [conftest.user_event("ok")])
  # A non-ASCII but valid line rides the decode branch (the isascii() proof
  # answers only for ASCII corpora); the copy keeps its raw bytes.
  non_ascii = json.dumps(conftest.user_event("中文"), ensure_ascii=False)
  with open(events_path, "a", encoding="utf-8") as f:
    f.write(non_ascii + "\n")

  child = await mgr.fork.fork_session(parent.id)
  expected_prefix = events_path.read_bytes()
  child_raw = mgr.events.get_chat_events_path(child.id).read_bytes()
  assert child_raw.startswith(expected_prefix)
  marker_lines = child_raw[len(expected_prefix):].decode("utf-8").splitlines()
  assert [json.loads(line)["type"] for line in marker_lines] == [ET.CLONE_START, ET.TASK_CREATED]

  # Undecodable bytes raise at fork time, and the failed fork writes no child
  # chat log.
  other = await conftest.create_root_session(
      mgr, models.CreateSessionRequest(name="Other"), backend=conftest.OPUS_BACKEND_ID)
  conftest.append_events(mgr.events.get_chat_events_path(other.id), [conftest.user_event("ok")])
  with open(mgr.events.get_chat_events_path(other.id), "ab") as f:
    f.write(b"\xff\xfe\n")
  chat_logs_before = set(cfg.sessions_dir.glob("*/data/chat_events.jsonl"))

  with pytest.raises(UnicodeDecodeError):
    await mgr.fork.fork_session(other.id)

  assert not _parent_side_files(cfg)
  assert set(cfg.sessions_dir.glob("*/data/chat_events.jsonl")) == chat_logs_before
