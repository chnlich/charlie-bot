from __future__ import annotations

import pathlib
from unittest import mock

import conftest
import pytest

from src.runtime import sessions


def _broadcast_calls(broadcast_mock: mock.AsyncMock) -> list[dict]:
  return [call.args[1] for call in broadcast_mock.await_args_list]


@pytest.mark.asyncio
async def test_persist_user_event_broadcasts_message_delta_only(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "ts"})

  payloads = _broadcast_calls(broadcast_mock)
  assert len(payloads) == 1
  assert payloads[0]["type"] == "message"
  assert payloads[0]["message"]["role"] == "user"
  assert payloads[0]["message"]["content"] == "hi"


@pytest.mark.asyncio
async def test_persist_handler_result_broadcasts_message_delta_and_raw_event(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr.persist_and_broadcast(
        session.id, {
            "type": "handler_result",
            "task": "Lint",
            "status": "ok",
            "message": "Done",
            "timestamp": "ts",
        })

  payloads = _broadcast_calls(broadcast_mock)
  assert [p["type"] for p in payloads] == ["message", "handler_result"]
  assert payloads[0]["message"]["role"] == "system"
  assert payloads[0]["message"]["content"] == "✓ Lint: Done"


@pytest.mark.asyncio
async def test_lazy_init_aggregator_after_restart_does_not_replay_history(tmp_path: pathlib.Path) -> None:
  """A new SessionManager (simulating restart) must not re-broadcast historical deltas."""
  cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()):
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
  mgr2 = sessions.SessionManager(cfg)
  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr2.persist_and_broadcast(session.id, {
        "type": "user",
        "content": "next",
        "timestamp": "t4",
    })

  payloads = _broadcast_calls(broadcast_mock)
  # Only the new event's delta is broadcast; historical events stay silent.
  assert len(payloads) == 1
  assert payloads[0]["type"] == "message"
  assert payloads[0]["message"]["role"] == "user"
  assert payloads[0]["message"]["content"] == "next"
  assert payloads[0]["message"]["event_index"] == 3
