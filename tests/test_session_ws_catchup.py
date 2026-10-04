from __future__ import annotations

import conftest
import pytest

import server

VOICE_KEY = "is_voice"


def _assistant_event(text: str, ts: str) -> dict:
  return {**conftest.assistant_text_event(text), "timestamp": ts}


def _master_done_event(thinking_seconds: int, ts: str) -> dict:
  return {"type": "master_done", "thinking_seconds": thinking_seconds, "timestamp": ts}


@pytest.mark.asyncio
async def test_replay_skips_pre_cursor_deltas_and_drops_raw_assistant_user() -> None:
  events = [
      conftest.user_event("hi", "t0"),
      _assistant_event("Hello", "t1"),
      _master_done_event(2, "t2"),
      conftest.user_event("again", "t3"),
  ]
  ws = conftest.FakeWebSocket()
  sent_count = await server._replay_aggregated_catchup(ws, events, cursor=2, session_id="s")
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
async def test_replay_with_cursor_at_end_sends_nothing() -> None:
  events = [
      conftest.user_event("hi", "t0"),
      _assistant_event("ok", "t1"),
  ]
  ws = conftest.FakeWebSocket()
  sent = await server._replay_aggregated_catchup(ws, events, cursor=len(events), session_id="s")
  # Pending draft is shown by SSR via pending_draft; catchup sends nothing.
  assert sent == 0
  assert ws.sent == []


@pytest.mark.asyncio
async def test_replay_uses_global_cursor_after_archive_offset() -> None:
  events = [
      conftest.user_event("old-live", "t0"),
      conftest.user_event("missed", "t1"),
  ]
  ws = conftest.FakeWebSocket()

  sent = await server._replay_aggregated_catchup(ws, events, cursor=6, session_id="s", event_index_offset=5)

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
  assert ws.sent == [{
      "type": "message",
      "message": expected_message,
  }]


def _mixed_replay_corpus() -> list[dict]:
  return [
      conftest.user_event("hi", "t0"),
      _assistant_event("A", "t1"),
      _assistant_event("B", "t2"),
      _master_done_event(1, "t3"),
      conftest.scheduled_trigger_event("fire", "t4"),
      conftest.user_event("tail", "t5"),
  ]


def _unsliced_catchup_frames(events: list[dict], cursor: int) -> list[dict]:
  """Parity oracle: the production walk fed the whole corpus in one slice."""
  walk = server._CatchupWalk(cursor)
  walk.feed_slice(events, 0, len(events))
  return walk.finish()


@pytest.mark.asyncio
async def test_replay_stops_at_first_send_failure_with_match_count() -> None:

  class FailingWebSocket(conftest.FakeWebSocket):

    async def send_text(self, text: str) -> None:
      if len(self.sent) == 1:
        raise RuntimeError("closed")
      await super().send_text(text)

  events = _mixed_replay_corpus()
  ws = FailingWebSocket()
  sent = await server._replay_aggregated_catchup(ws, events, cursor=0, session_id="s")
  total = len(_unsliced_catchup_frames(events, 0))
  # 1 frame landed, the second send failed, the remaining total - 2 frames
  # were never attempted.
  assert total >= 2
  assert len(ws.sent) == 1
  assert sent == 1
