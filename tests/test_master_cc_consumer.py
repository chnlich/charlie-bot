"""Tests for master_cc._session_consumer cc_session_id relay."""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.agents import master_cc
from src.core.models import SessionCallbacks, SessionMetadata


def _make_meta(session_id: str) -> SessionMetadata:
  return SessionMetadata(id=session_id, name="t", cc_session_id=None)


def _make_callbacks() -> SessionCallbacks:
  return SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      clear_thinking_since=AsyncMock(),
  )


def _make_item(session_meta: SessionMetadata, callbacks: SessionCallbacks) -> master_cc._WorkItem:
  loop = asyncio.get_running_loop()
  return master_cc._WorkItem(
      cfg=MagicMock(),
      session_meta=session_meta,
      user_content="hi",
      callbacks=callbacks,
      is_voice=False,
      auto_trigger=False,
      backend_option=None,
      extra_claude_flags=None,
      should_check_tex=False,
      future=loop.create_future(),
  )


@pytest.mark.asyncio
async def test_consumer_relays_cc_session_id_across_metadata_instances() -> None:
  """Two queued _WorkItems with distinct SessionMetadata objects must share cc_session_id.

  Reproduces the fork_session race where the bootstrap turn sets cc_session_id on
  meta_A but a concurrently-loaded meta_B still has cc_session_id=None.
  """
  session_id = "test-session-relay"

  meta_bootstrap = _make_meta(session_id)
  meta_user_message = _make_meta(session_id)  # distinct instance, freshly loaded from disk
  cb = _make_callbacks()
  item_bootstrap = _make_item(meta_bootstrap, cb)
  item_user = _make_item(meta_user_message, cb)

  master_cc._session_queues.pop(session_id, None)
  master_cc._session_queues[session_id] = asyncio.Queue()
  master_cc._session_queues[session_id].put_nowait(item_bootstrap)
  master_cc._session_queues[session_id].put_nowait(item_user)

  observed_cc_session_ids: list = []

  async def fake_run_cc(item: master_cc._WorkItem):
    observed_cc_session_ids.append(item.session_meta.cc_session_id)
    return ("cc-id-from-bootstrap", 0, None, {})

  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)

  try:
    with (
        patch.object(master_cc, "_run_cc", side_effect=fake_run_cc),
        patch.object(master_cc.streaming_manager, "broadcast", new=AsyncMock()),
        patch("src.core.sessions.SessionManager", return_value=workers_mock),
    ):
      await asyncio.wait_for(master_cc._session_consumer(session_id), timeout=5)
  finally:
    master_cc._session_queues.pop(session_id, None)
    master_cc._session_consumers.pop(session_id, None)

  assert observed_cc_session_ids == [None, "cc-id-from-bootstrap"], (
      "second _run_cc must observe cc_session_id relayed from bootstrap meta")
  assert meta_user_message.cc_session_id == "cc-id-from-bootstrap"
  assert item_bootstrap.future.done() and item_bootstrap.future.result() == "cc-id-from-bootstrap"
  assert item_user.future.done() and item_user.future.result() == "cc-id-from-bootstrap"
