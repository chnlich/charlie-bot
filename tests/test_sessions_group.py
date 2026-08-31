"""Regression tests for session group change broadcasts and group inheritance."""

import asyncio
import json
from datetime import UTC, datetime
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest
from conftest import BROADCAST_PATCH_TARGET, OPUS_BACKEND_ID
from conftest import make_session_mgr as _make_session_mgr

from src.core import event_types as ET
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager


async def _seed_parent(
    mgr: SessionManager,
    *,
    group: str | None = "Work",
    backend: str = OPUS_BACKEND_ID,
) -> SessionMetadata:
  """Create and persist a parent session with chat events."""
  parent = SessionMetadata(name="Parent", group=group, backend=backend)
  await mgr.save_metadata(parent)
  events_path = mgr.get_chat_events_path(parent.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(json.dumps({"type": "user", "content": "hello"}) + "\n")
  return parent


@pytest.mark.asyncio
async def test_set_group_broadcasts_sidebar_event() -> None:
  session_mgr = SessionManager(SimpleNamespace())
  updated = SessionMetadata(id="session-1", name="Session 1", group="Work")

  with (
      patch.object(session_mgr, "_update_field", new=AsyncMock(return_value=updated)) as mock_update,
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
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
  parent = await _seed_parent(mgr, group="Research", backend=OPUS_BACKEND_ID)

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
  parent = await _seed_parent(mgr, group="Research", backend=OPUS_BACKEND_ID)

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
  await mgr.save_metadata(meta)

  # Populate mgr's metadata cache with a stale pre-group snapshot.
  await mgr.get_session(meta.id)

  concurrent_meta = await concurrent_mgr.get_session(meta.id)
  assert concurrent_meta is not None
  concurrent_meta.group = "Research"
  await concurrent_mgr.save_metadata(concurrent_meta)

  updated_at = datetime(2026, 3, 31, 12, 1, tzinfo=UTC)
  await mgr.update_thinking_state(meta.id, updated_at=updated_at)

  updated = await verify_mgr.get_session(meta.id)
  assert updated is not None
  assert updated.group == "Research"
  assert updated.updated_at == updated_at


@pytest.mark.asyncio
async def test_mark_unread_and_update_thinking_state_do_not_clobber(tmp_path: Path) -> None:
  """Regression: concurrent mark_unread vs update_thinking_state must both persist.

  Without per-session RMW locking, these two coroutines interleave at their
  internal await points and the second save clobbers the first mutator's field.
  """
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="Session 1", has_unread=False)
  await mgr.save_metadata(meta)

  # Force interleaving by injecting a yield point inside save_metadata. The
  # per-session lock must serialize the two RMW sequences so both mutations
  # end up on disk.
  real_save = mgr.save_metadata

  async def yielding_save(m: SessionMetadata) -> None:
    await asyncio.sleep(0)
    await real_save(m)

  updated_at = datetime(2026, 3, 31, 12, 1, tzinfo=UTC)
  with (
      patch.object(mgr, "save_metadata", side_effect=yielding_save),
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await asyncio.gather(
        mgr.mark_unread(meta.id),
        mgr.update_thinking_state(meta.id, updated_at=updated_at),
    )

  # Bypass the metadata cache to check what's actually on disk.
  mgr._invalidate_cache(meta.id)
  final = await mgr.get_session(meta.id)
  assert final is not None
  assert final.has_unread is True
  assert final.updated_at == updated_at
