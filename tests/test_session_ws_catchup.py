from __future__ import annotations

import asyncio

import conftest
import pytest

import server


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
      "input_mode": None,
      "event_index": 6,
      "id": "legacy:6",
      "timestamp": "t1",
  }
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


class _QueryParams:

  def __init__(self, cursor: str | None) -> None:
    self.cursor = cursor

  def get(self, key: str) -> str | None:
    assert key == "cursor"
    return self.cursor


class _SessionWebSocket:

  def __init__(self, cursor: str | None) -> None:
    self.query_params = _QueryParams(cursor)
    self.accepted = False
    self.close_code = None
    self.sent: list[dict] = []
    self.receive_calls = 0

  async def accept(self) -> None:
    self.accepted = True

  async def close(self, code: int) -> None:
    self.close_code = code

  async def send_json(self, payload: dict) -> None:
    self.sent.append(payload)

  async def receive_text(self) -> str:
    self.receive_calls += 1
    raise AssertionError("session WebSocket must not wait for a cursor frame")


@pytest.mark.parametrize("raw_cursor", [None, "-1", "abc"])
@pytest.mark.asyncio
async def test_invalid_handshake_cursor_closes_before_subscription(
    monkeypatch: pytest.MonkeyPatch,
    raw_cursor: str | None,
) -> None:
  websocket = _SessionWebSocket(raw_cursor)
  warnings: list[tuple[str, dict[str, object]]] = []
  subscriptions: list[tuple[object, ...]] = []
  replays: list[tuple[object, ...]] = []

  async def check_auth(_websocket: _SessionWebSocket) -> bool:
    return True

  def warn(event: str, **fields: object) -> None:
    warnings.append((event, fields))

  async def subscribe(*args: object) -> None:
    subscriptions.append(args)

  async def replay(*args: object) -> tuple[int, int]:
    replays.append(args)
    return 0, 0

  monkeypatch.setattr(server.auth, "check_ws_auth", check_auth)
  monkeypatch.setattr(server.log, "warning", warn)
  monkeypatch.setattr(server.streaming.streaming_manager, "subscribe", subscribe)
  monkeypatch.setattr(server, "_send_session_catchup", replay)

  await server.session_websocket(websocket, "session-a")

  assert websocket.accepted
  assert websocket.close_code == 4400
  assert websocket.sent == []
  assert websocket.receive_calls == 0
  assert subscriptions == []
  assert replays == []
  assert warnings == [("session_ws_cursor_invalid", {"session_id": "session-a", "raw_cursor": raw_cursor})]


@pytest.mark.asyncio
async def test_handshake_cursor_controls_replay_after_delayed_accept(monkeypatch: pytest.MonkeyPatch) -> None:
  websocket = _SessionWebSocket("9")
  calls: list[tuple[object, ...]] = []
  accept_started = asyncio.Event()
  accept_gate = asyncio.Event()

  async def check_auth(_websocket: _SessionWebSocket) -> bool:
    return True

  async def delayed_accept() -> None:
    accept_started.set()
    await accept_gate.wait()
    websocket.accepted = True

  async def subscribe(channel: str, _websocket: _SessionWebSocket) -> None:
    calls.append(("subscribe", channel))

  async def unsubscribe(channel: str, _websocket: _SessionWebSocket) -> None:
    calls.append(("unsubscribe", channel))

  async def send_catchup(
      _websocket: _SessionWebSocket,
      _events: object,
      session_id: str,
      cursor: int,
      _meta: object,
  ) -> tuple[int, int]:
    calls.append(("replay", session_id, cursor))
    return 0, 9

  async def keepalive(*_args: object, **_kwargs: object) -> None:
    calls.append(("keepalive",))

  class SessionStore:

    async def get_session(self, _session_id: str) -> None:
      return None

  monkeypatch.setattr(server.auth, "check_ws_auth", check_auth)
  monkeypatch.setattr(websocket, "accept", delayed_accept)
  monkeypatch.setattr(server.streaming, "session_channel", lambda session_id: f"session:{session_id}")
  monkeypatch.setattr(server.streaming.streaming_manager, "subscribe", subscribe)
  monkeypatch.setattr(server.streaming.streaming_manager, "unsubscribe", unsubscribe)
  monkeypatch.setattr(server.session_store, "store", lambda: SessionStore())
  monkeypatch.setattr(server.session_events, "events", lambda: object())
  monkeypatch.setattr(server, "_send_session_catchup", send_catchup)
  monkeypatch.setattr(server, "_ws_keepalive", keepalive)

  task = asyncio.create_task(server.session_websocket(websocket, "session-a"))
  await accept_started.wait()
  assert not task.done()
  accept_gate.set()
  await task

  assert websocket.close_code is None
  assert websocket.receive_calls == 0
  assert websocket.sent == [{"type": "catchup_complete"}]
  assert calls[:3] == [
      ("subscribe", "session:session-a"),
      ("subscribe", server.streaming.SIDEBAR_CHANNEL),
      ("replay", "session-a", 9),
  ]
  assert calls[3] == ("keepalive",)
