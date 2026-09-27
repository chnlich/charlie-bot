from __future__ import annotations

from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    make_home_session,
)

from src.core.sessions import SessionManager


def _broadcast_calls(mock: AsyncMock) -> list[dict]:
  return [call.args[1] for call in mock.await_args_list]


@pytest.mark.asyncio
async def test_persist_user_event_broadcasts_message_delta_only(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "ts"})

  payloads = _broadcast_calls(mock)
  assert len(payloads) == 1
  assert payloads[0]["type"] == "message"
  assert payloads[0]["message"]["role"] == "user"
  assert payloads[0]["message"]["content"] == "hi"


@pytest.mark.asyncio
async def test_persist_handler_result_broadcasts_message_delta_and_raw_event(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(
        session.id, {
            "type": "handler_result",
            "task": "Lint",
            "status": "ok",
            "message": "Done",
            "timestamp": "ts",
        })

  payloads = _broadcast_calls(mock)
  assert [p["type"] for p in payloads] == ["message", "handler_result"]
  assert payloads[0]["message"]["role"] == "system"
  assert payloads[0]["message"]["content"] == "✓ Lint: Done"


@pytest.mark.asyncio
async def test_lazy_init_aggregator_after_restart_does_not_replay_history(tmp_path: Path) -> None:
  """A new SessionManager (simulating restart) must not re-broadcast historical deltas."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "t1"})
    await mgr.persist_and_broadcast(
        session.id, {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "ok"
                }]
            },
            "timestamp": "t2",
        })
    await mgr.persist_and_broadcast(session.id, {
        "type": "master_done",
        "thinking_seconds": 1,
        "timestamp": "t3",
    })

  # Simulate process restart: brand-new SessionManager with same on-disk state.
  mgr2 = SessionManager(cfg)
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock:
    await mgr2.persist_and_broadcast(session.id, {
        "type": "user",
        "content": "next",
        "timestamp": "t4",
    })

  payloads = _broadcast_calls(mock)
  # Only the new event's delta is broadcast; historical events stay silent.
  assert len(payloads) == 1
  assert payloads[0]["type"] == "message"
  assert payloads[0]["message"]["role"] == "user"
  assert payloads[0]["message"]["content"] == "next"
  assert payloads[0]["message"]["event_index"] == 3
