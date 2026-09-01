from __future__ import annotations

import pytest
from conftest import FakeWebSocket

from server import _replay_aggregated_catchup, _send_session_catchup

VOICE_KEY = "is_" + "voice"


class _CountOnlySessionManager:
  def __init__(self, count: int) -> None:
    self.count = count
    self.full_load_called = False

  def get_chat_event_count_sync(self, session_id: str, meta) -> int:
    assert session_id == "s"
    assert meta.archive_offset == 5
    return self.count

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    self.full_load_called = True
    raise AssertionError("current cursor must not trigger full replay")


@pytest.mark.asyncio
async def test_replay_skips_pre_cursor_deltas_and_drops_raw_assistant_user() -> None:
  events = [
      {"type": "user", "content": "hi", "timestamp": "t0"},
      {"type": "assistant", "message": {"content": [{"type": "text", "text": "Hello"}]}, "timestamp": "t1"},
      {"type": "master_done", "thinking_seconds": 2, "timestamp": "t2"},
      {"type": "user", "content": "again", "timestamp": "t3"},
  ]
  ws = FakeWebSocket()
  sent_count = await _replay_aggregated_catchup(ws, events, cursor=2, session_id="s")
  assert sent_count == len(ws.sent)

  types = [p["type"] for p in ws.sent]
  # Pre-cursor events 0 and 1 only update aggregator state. From cursor=2:
  #   event[2] master_done -> message delta (separator) + raw master_done event.
  #   event[3] user -> message delta only (raw user is suppressed).
  # The flush of the pre-cursor assistant draft at event[2] also emits a
  # message delta because the aggregator's buffer was non-empty.
  assert types == ["message", "message", "master_done", "message"]
  # First message is the assistant flush (carried across cursor), then separator.
  assert ws.sent[0]["message"]["role"] == "assistant"
  assert ws.sent[0]["message"]["content"] == "Hello"
  assert ws.sent[1]["message"]["role"] == "separator"
  assert ws.sent[3]["message"]["role"] == "user"
  assert ws.sent[3]["message"]["content"] == "again"


@pytest.mark.asyncio
async def test_replay_drops_raw_scheduled_trigger() -> None:
  events = [
      {"type": "scheduled_trigger", "content": "[Scheduled trigger fired] watch", "timestamp": "t0"},
  ]
  ws = FakeWebSocket()
  sent_count = await _replay_aggregated_catchup(ws, events, cursor=0, session_id="s")

  # The suppression list is shared with persist_and_broadcast, so catchup
  # matches the live wire: scheduled_trigger flows only as a message delta.
  assert sent_count == len(ws.sent) == 1
  assert ws.sent[0]["type"] == "message"
  assert ws.sent[0]["message"]["role"] == "scheduled_trigger"


@pytest.mark.asyncio
async def test_replay_emits_only_latest_stream_when_draft_is_dangling() -> None:
  events = [
      {"type": "assistant", "message": {"content": [{"type": "text", "text": "A"}]}, "timestamp": "t0"},
      {"type": "assistant", "message": {"content": [{"type": "text", "text": "B"}]}, "timestamp": "t1"},
  ]
  ws = FakeWebSocket()
  await _replay_aggregated_catchup(ws, events, cursor=0, session_id="s")

  # Two assistant text events split the buffer:
  #   - first event yields a stream("A")
  #   - second event flushes "A" as a message and yields stream("B")
  # Stream deltas are coalesced; only the *latest* preview is sent before each
  # flush + at the very end.
  types = [p["type"] for p in ws.sent]
  assert types == ["message", "stream"]
  assert ws.sent[0]["message"]["content"] == "A"
  assert ws.sent[1]["message"]["content"] == "B"


@pytest.mark.asyncio
async def test_replay_with_cursor_at_end_sends_nothing() -> None:
  events = [
      {"type": "user", "content": "hi", "timestamp": "t0"},
      {"type": "assistant", "message": {"content": [{"type": "text", "text": "ok"}]}, "timestamp": "t1"},
  ]
  ws = FakeWebSocket()
  sent = await _replay_aggregated_catchup(ws, events, cursor=len(events), session_id="s")
  # Pending draft is shown by SSR via pending_draft; catchup sends nothing.
  assert sent == 0
  assert ws.sent == []


@pytest.mark.asyncio
async def test_replay_uses_global_cursor_after_archive_offset() -> None:
  events = [
      {"type": "user", "content": "old-live", "timestamp": "t0"},
      {"type": "user", "content": "missed", "timestamp": "t1"},
  ]
  ws = FakeWebSocket()

  sent = await _replay_aggregated_catchup(ws, events, cursor=6, session_id="s", event_index_offset=5)

  expected_message = {
      "role": "user",
      "content": "missed",
      "uploaded_files": [],
      "event_index": 6,
      "id": "legacy:6",
      "timestamp": "t1",
  }
  expected_message[VOICE_KEY] = False
  assert sent == 1
  assert ws.sent == [
      {
          "type": "message",
          "message": expected_message,
      }
  ]


@pytest.mark.asyncio
async def test_session_catchup_fast_skips_when_cursor_is_current() -> None:
  ws = FakeWebSocket()
  mgr = _CountOnlySessionManager(count=7)
  meta = type("Meta", (), {"archive_offset": 5})()

  sent, total = await _send_session_catchup(ws, mgr, "s", cursor=7, meta=meta)

  assert sent == 0
  assert total == 7
  assert ws.sent == []
  assert mgr.full_load_called is False
