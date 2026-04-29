from __future__ import annotations

from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager


def _broadcast_calls(mock: AsyncMock) -> list[dict]:
  return [call.args[1] for call in mock.await_args_list]


@pytest.mark.asyncio
async def test_persist_user_event_broadcasts_message_delta_only(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "ts"})

  payloads = _broadcast_calls(mock)
  assert len(payloads) == 1
  assert payloads[0]["type"] == "message"
  assert payloads[0]["message"]["role"] == "user"
  assert payloads[0]["message"]["content"] == "hi"


@pytest.mark.asyncio
async def test_persist_assistant_text_broadcasts_stream_then_message_on_master_done(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(session.id, {
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "Working"}]},
        "timestamp": "t1",
    })
    await mgr.persist_and_broadcast(session.id, {
        "type": "master_done",
        "thinking_seconds": 2,
        "timestamp": "t2",
    })

  payloads = _broadcast_calls(mock)
  types = [p["type"] for p in payloads]
  # assistant -> stream delta only (raw is suppressed).
  # master_done -> message delta (commit) + raw master_done (state side-effect).
  assert types == ["stream", "message", "message", "master_done"]
  assert payloads[0]["message"]["content"] == "Working"
  assert payloads[1]["message"]["role"] == "assistant"
  assert payloads[1]["message"]["content"] == "Working"
  assert payloads[2]["message"]["role"] == "separator"


@pytest.mark.asyncio
async def test_persist_handler_result_broadcasts_message_delta_and_raw_event(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(session.id, {
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
async def test_aggregator_state_persists_across_calls(tmp_path: Path) -> None:
  """Tools attached in one event surface in the eventual flush message."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock:
    await mgr.persist_and_broadcast(session.id, {
        "type": "assistant",
        "message": {"content": [
            {"type": "text", "text": "Running"},
            {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
        ]},
        "timestamp": "t1",
    })
    await mgr.persist_and_broadcast(session.id, {
        "type": "user",
        "message": {"content": [{"type": "tool_result", "content": "out", "is_error": False}]},
    })
    await mgr.persist_and_broadcast(session.id, {
        "type": "master_done",
        "thinking_seconds": 1,
        "timestamp": "t3",
    })

  payloads = _broadcast_calls(mock)
  commit_msgs = [p["message"] for p in payloads if p["type"] == "message" and p["message"]["role"] == "assistant"]
  assert len(commit_msgs) == 1
  assert commit_msgs[0]["content"] == "Running"
  assert commit_msgs[0]["tools"] == [{
      "name": "Bash",
      "input": {"command": "ls"},
      "output": "out",
      "is_error": False,
  }]


@pytest.mark.asyncio
async def test_lazy_init_aggregator_after_restart_does_not_replay_history(tmp_path: Path) -> None:
  """A new SessionManager (simulating restart) must not re-broadcast historical deltas."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "t1"})
    await mgr.persist_and_broadcast(session.id, {
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "ok"}]},
        "timestamp": "t2",
    })
    await mgr.persist_and_broadcast(session.id, {
        "type": "master_done",
        "thinking_seconds": 1,
        "timestamp": "t3",
    })

  # Simulate process restart: brand-new SessionManager with same on-disk state.
  mgr2 = SessionManager(cfg)
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock:
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
