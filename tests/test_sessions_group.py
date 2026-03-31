"""Regression tests for session group change broadcasts and group inheritance."""

import json
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest

from src.core import event_types as ET
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager


def _make_session_mgr(tmp_path: Path) -> SessionManager:
  """Create a SessionManager backed by a temporary directory."""
  cfg = SimpleNamespace(sessions_dir=tmp_path / "sessions")
  cfg.sessions_dir.mkdir()
  return SessionManager(cfg)


async def _seed_parent(mgr: SessionManager, *, group: str | None = "Work") -> SessionMetadata:
  """Create and persist a parent session with chat events."""
  parent = SessionMetadata(name="Parent", group=group)
  await mgr._save_metadata(parent)
  events_path = mgr._chat_events_path(parent.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(json.dumps({"type": "user", "content": "hello"}) + "\n")
  return parent


@pytest.mark.asyncio
async def test_set_group_broadcasts_sidebar_event() -> None:
  session_mgr = SessionManager(SimpleNamespace())
  updated = SessionMetadata(id="session-1", name="Session 1", group="Work")

  with (
      patch.object(session_mgr, "_update_field", new=AsyncMock(return_value=updated)) as mock_update,
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
  ):
    result = await session_mgr.set_group("session-1", "Work")

  assert result == updated
  mock_update.assert_awaited_once_with("session-1", "group", "Work", "session_group_set")
  mock_broadcast.assert_awaited_once_with(
      "sidebar",
      {
          "type": ET.SESSION_GROUP_CHANGED,
          "session_id": "session-1",
          "group": "Work",
      },
  )


# ---------------------------------------------------------------------------
# Group inheritance: fork (clone) and elone
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_fork_session_inherits_group(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group="Research")

  child = await mgr.fork_session(parent.id)

  assert child is not None
  assert child.group == "Research"


@pytest.mark.asyncio
async def test_fork_session_inherits_none_group(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group=None)

  child = await mgr.fork_session(parent.id)

  assert child is not None
  assert child.group is None


@pytest.mark.asyncio
async def test_elone_session_inherits_group(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group="Research")

  child = await mgr.elone_session(parent.id, event_index=0)

  assert child is not None
  assert child.group == "Research"


@pytest.mark.asyncio
async def test_elone_session_inherits_none_group(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group=None)

  child = await mgr.elone_session(parent.id, event_index=0)

  assert child is not None
  assert child.group is None
