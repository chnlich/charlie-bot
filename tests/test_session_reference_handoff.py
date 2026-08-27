"""Tests for clone/elone parent reference handoff."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import append_events as _append_events
from conftest import archive_cutoff_events as _archive_cutoff_events
from conftest import make_home_session

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SessionStatus
from src.core.recap import extract_recap
from src.core.sessions import SessionManager


def _read_events(path: Path) -> list[dict]:
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _reference_path(session_mgr: SessionManager, session_id: str) -> Path:
  return session_mgr.get_chat_events_path(session_id).parent / "parent_reference.jsonl"


@pytest.mark.asyncio
async def test_fork_session_writes_truncated_reference_and_live_banner(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(
      mgr.get_chat_events_path(parent.id),
      [
          {
              "type": "user",
              "content": "e0"
          },
          {
              "type": "assistant",
              "content": "e1"
          },
          {
              "type": "user",
              "content": "e2"
          },
      ],
  )

  child = await mgr.fork_session(parent.id, event_index=1)

  assert [event["content"] for event in _read_events(_reference_path(mgr, child.id))] == ["e0", "e1"]
  live_events = _read_events(mgr.get_chat_events_path(child.id))
  assert len(live_events) == 1
  assert live_events[0]["type"] == ET.CLONE_START
  assert live_events[0]["parent_session_id"] == parent.id


@pytest.mark.asyncio
async def test_elone_session_writes_reference_and_archives_parent(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(
      mgr.get_chat_events_path(parent.id),
      [
          {
              "type": "user",
              "content": "e0"
          },
          {
              "type": "assistant",
              "content": "e1"
          },
      ],
  )

  child = await mgr.elone_session(parent.id, event_index=0)

  assert [event["content"] for event in _read_events(_reference_path(mgr, child.id))] == ["e0"]
  assert [event["type"] for event in _read_events(mgr.get_chat_events_path(child.id))] == [ET.CLONE_START]
  updated_parent = await mgr.get_session(parent.id)
  assert updated_parent is not None
  assert updated_parent.status == SessionStatus.ARCHIVED
  assert updated_parent.rating == "thumbs_down"


@pytest.mark.asyncio
async def test_reference_handoff_uses_global_event_index_with_archive_offset(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")

  cutoff, events = _archive_cutoff_events()
  _append_events(mgr.get_chat_events_path(parent.id), events)
  await mgr.recycle_scheduled_session(parent.id, cutoff)

  child = await mgr.fork_session(parent.id, event_index=6)

  reference_events = _read_events(_reference_path(mgr, child.id))
  assert [event["content"] for event in reference_events] == ["e0", "e1", "e2", "e3", "e4", "f0", "f1"]
  assert [event["type"] for event in _read_events(mgr.get_chat_events_path(child.id))] == [ET.CLONE_START]


@pytest.mark.asyncio
async def test_reference_handoff_errors_write_no_reference(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)

  with pytest.raises(FileNotFoundError):
    await mgr.fork_session("missing", event_index=0)
  assert list(cfg.sessions_dir.glob("*/data/parent_reference.jsonl")) == []

  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "only"}])

  with pytest.raises(ValueError, match="out of range"):
    await mgr.elone_session(parent.id, event_index=1)

  assert list(cfg.sessions_dir.glob("*/data/parent_reference.jsonl")) == []
  updated_parent = await mgr.get_session(parent.id)
  assert updated_parent is not None
  assert updated_parent.status == SessionStatus.ACTIVE
  assert updated_parent.rating is None


@pytest.mark.asyncio
async def test_reference_bootstraps_are_not_divider_recap_asks(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Child", backend="claude-opus-4.6")
  _append_events(
      mgr.get_chat_events_path(session.id),
      [
          {
              "type": "user",
              "content": "This session continues a prior conversation.\n\nbootstrap",
          },
          {
              "type": "user",
              "content": "You're taking over because the user wasn't satisfied with the previous session. bootstrap",
          },
          {
              "type": "user",
              "content": "real ask",
          },
          {
              "type": "assistant",
              "message": {
                  "content": [{
                      "type": "text",
                      "text": "real answer",
                  }]
              },
          },
      ],
  )

  recap = extract_recap(mgr, session.id)

  assert recap["asks"] == ["real ask"]
  assert recap["last"] == {
      "user": "real ask",
      "assistant": "real answer",
  }
