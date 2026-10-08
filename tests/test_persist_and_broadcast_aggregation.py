from __future__ import annotations

import pathlib
from unittest import mock

import conftest
import pytest


def _broadcast_calls(broadcast_mock: mock.AsyncMock) -> list[dict]:
  return [call.args[1] for call in broadcast_mock.await_args_list]


@pytest.mark.asyncio
async def test_persist_user_event_broadcasts_message_delta_only(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr.events.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "ts"})

  payloads = _broadcast_calls(broadcast_mock)
  assert len(payloads) == 1
  assert payloads[0]["type"] == "message"
  assert payloads[0]["message"]["role"] == "user"
  assert payloads[0]["message"]["content"] == "hi"


@pytest.mark.asyncio
async def test_persist_handler_result_broadcasts_message_delta_and_raw_event(tmp_path: pathlib.Path) -> None:
  _cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr.events.persist_and_broadcast(
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
async def test_lazy_init_aggregator_after_restart_does_not_replay_history(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """New session blocks (simulating restart) must not re-broadcast historical deltas."""
  cfg, mgr, session = await conftest.make_home_session(tmp_path, name="t")
  conftest.bind_session_blocks(monkeypatch, mgr)  # the master_done below runs every contribution's after_turn

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()):
    await mgr.events.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "t1"})
    await mgr.events.persist_and_broadcast(
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
    await mgr.events.persist_and_broadcast(
        session.id, {
            "type": "master_done",
            "thinking_seconds": 1,
            "timestamp": "t3",
        })

  # Simulate process restart: brand-new session blocks with same on-disk state.
  mgr2 = conftest.build_session_blocks(cfg)
  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast_mock:
    await mgr2.events.persist_and_broadcast(session.id, {
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
  assert payloads[0]["message"]["event_index"] == 4
