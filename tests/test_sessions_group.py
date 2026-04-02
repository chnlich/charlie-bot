"""Regression tests for session group change broadcasts and group inheritance."""

import json
from datetime import datetime, timezone
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


async def _seed_parent(
    mgr: SessionManager,
    *,
    group: str | None = "Work",
    backend: str = "claude-opus-4.6",
) -> SessionMetadata:
  """Create and persist a parent session with chat events."""
  parent = SessionMetadata(name="Parent", group=group, backend=backend)
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
  assert child.backend == parent.backend


@pytest.mark.asyncio
async def test_fork_session_inherits_none_group(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group=None)

  child = await mgr.fork_session(parent.id)

  assert child is not None
  assert child.group is None
  assert child.backend == parent.backend


@pytest.mark.asyncio
async def test_fork_session_accepts_backend_override(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group="Research", backend="claude-opus-4.6")

  child = await mgr.fork_session(parent.id, backend="codex-o3")

  assert child is not None
  assert child.group == "Research"
  assert child.backend == "codex-o3"


@pytest.mark.asyncio
async def test_elone_session_inherits_group(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group="Research")

  child = await mgr.elone_session(parent.id, event_index=0)

  assert child is not None
  assert child.group == "Research"
  assert child.backend == parent.backend


@pytest.mark.asyncio
async def test_elone_session_inherits_none_group(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group=None)

  child = await mgr.elone_session(parent.id, event_index=0)

  assert child is not None
  assert child.group is None
  assert child.backend == parent.backend


@pytest.mark.asyncio
async def test_elone_session_accepts_backend_override(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  parent = await _seed_parent(mgr, group="Research", backend="claude-opus-4.6")

  child = await mgr.elone_session(parent.id, event_index=0, backend="codex-o3")

  assert child is not None
  assert child.group == "Research"
  assert child.backend == "codex-o3"


@pytest.mark.asyncio
async def test_update_thinking_state_preserves_newer_group_from_disk(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  cfg = SimpleNamespace(sessions_dir=mgr._cfg.sessions_dir)
  concurrent_mgr = SessionManager(cfg)
  verify_mgr = SessionManager(cfg)
  meta = SessionMetadata(name="Session 1")
  await mgr._save_metadata(meta)

  # Populate mgr's metadata cache with a stale pre-group snapshot.
  await mgr.get_session(meta.id)

  concurrent_meta = await concurrent_mgr.get_session(meta.id)
  assert concurrent_meta is not None
  concurrent_meta.group = "Research"
  await concurrent_mgr.save_metadata(concurrent_meta)

  thinking_since = datetime(2026, 3, 31, 12, 0, tzinfo=timezone.utc)
  updated_at = datetime(2026, 3, 31, 12, 1, tzinfo=timezone.utc)
  await mgr.update_thinking_state(meta.id, thinking_since, updated_at)

  updated = await verify_mgr.get_session(meta.id)
  assert updated is not None
  assert updated.group == "Research"
  assert updated.thinking_since == thinking_since
  assert updated.updated_at == updated_at


@pytest.mark.asyncio
async def test_update_thinking_state_can_leave_existing_thinking_since_unchanged(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="Session 1", thinking_since=datetime(2026, 3, 31, 12, 0, tzinfo=timezone.utc))
  await mgr._save_metadata(meta)

  updated_at = datetime(2026, 3, 31, 12, 1, tzinfo=timezone.utc)
  await mgr.update_thinking_state(meta.id, updated_at=updated_at)

  updated = await mgr.get_session(meta.id)
  assert updated is not None
  assert updated.thinking_since == datetime(2026, 3, 31, 12, 0, tzinfo=timezone.utc)
  assert updated.updated_at == updated_at
